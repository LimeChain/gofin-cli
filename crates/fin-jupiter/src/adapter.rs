//! Jupiter Swap v2 adapter. Provider output is treated as an untrusted
//! transaction proposal; every instruction is frozen and reviewed before it
//! can reach the signing/journaling path.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{Duration as ChronoDuration, Utc};
use fin_domain::{
    AccountingLocation, Adapter, Amount, AssetId, CandidateExpiryKind, Check, CheckStatus,
    ConnectionMetadata, CostBound, CostTreatment, Definition, EffectConstraint, ExecutionActor,
    FinError, FinancialIntent, FinancialState, OperationKind, OperationRecord, PreparedOperation,
    Reconciliation, ResourceAmount, ResourceId, Result, SnapshotScope, StateSnapshot, Status,
    Submission, SubmissionJournal, Valuation, VerifiedNonDispatchExpiry,
    boundaries::BoundarySpec,
    financial_plans::{FinancialPlanFundingIdentity, PlanPermission},
};
use fin_pricing::UsdPriceSource;
use fin_solana::{
    CLUSTER, NETWORK, SolanaWalletAdapter,
    transaction::{
        FrozenTransaction, NormalizedAccountMeta, NormalizedInstruction, ReceiptOutcome,
        TransactionSender,
    },
};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use solana_pubkey3::Pubkey;
use zeroize::Zeroizing;

use crate::{
    api::{HttpJupiterApi, JupiterApi, JupiterHost, RateBucket, RequestPriority},
    pricing::JupiterUsdPrices,
    swap::{self, DecodedSwapInstruction, MetaOrderResponse, SwapIntent, SwapRoute},
};

const OP_QUOTE: &str = "jupiter.quote";
const OP_SWAP: &str = "jupiter.swap";
const REQUEST_DEADLINE: Duration = Duration::from_secs(20);
const FROZEN_CACHE_TTL: Duration = Duration::from_secs(30);
const FROZEN_CACHE_CAPACITY: usize = 4;
const ORDER_ENGINE_PROGRAM: &str = "61DFfeTKM7trxYcPQCM78bJ794ddZprZpAwAnLiwTpYH";
const OKX_ROUTER_PROGRAM: &str = "proVF4pMXVaYqmy4NjniPh4pqKNfMmsihgd4wdkCX3u";
const DFLOW_PROGRAM: &str = crate::dflow::PROGRAM;
pub(crate) const MEMO_PROGRAM: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

pub struct JupiterAdapter {
    wallets: BTreeMap<String, Arc<SolanaWalletAdapter>>,
    api: Arc<dyn JupiterApi>,
    prices: Arc<dyn UsdPriceSource>,
    revalidated_frozen: OneShotCache<(FrozenTransaction, AuxiliaryEffects)>,
}

struct CachedCandidate<T> {
    key: [u8; 32],
    cached_at: Instant,
    value: T,
}

struct OneShotCache<T> {
    entries: Mutex<VecDeque<CachedCandidate<T>>>,
}

impl<T> Default for OneShotCache<T> {
    fn default() -> Self {
        Self {
            entries: Mutex::new(VecDeque::new()),
        }
    }
}

impl<T> OneShotCache<T> {
    fn insert(&self, key: [u8; 32], value: T) {
        self.insert_at(key, value, Instant::now());
    }

    fn insert_at(&self, key: [u8; 32], value: T, now: Instant) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        entries.retain(|entry| {
            now.saturating_duration_since(entry.cached_at) < FROZEN_CACHE_TTL && entry.key != key
        });
        while entries.len() >= FROZEN_CACHE_CAPACITY {
            entries.pop_front();
        }
        entries.push_back(CachedCandidate {
            key,
            cached_at: now,
            value,
        });
    }

    fn take(&self, key: [u8; 32]) -> Option<T> {
        self.take_at(key, Instant::now())
    }

    fn take_at(&self, key: [u8; 32], now: Instant) -> Option<T> {
        let mut entries = self.entries.lock().ok()?;
        entries.retain(|entry| now.saturating_duration_since(entry.cached_at) < FROZEN_CACHE_TTL);
        let index = entries.iter().position(|entry| entry.key == key)?;
        entries.remove(index).map(|entry| entry.value)
    }
}

impl JupiterAdapter {
    /// Keyless operation is the normal default. A credential is supplied only
    /// from the broker's protected credential store.
    pub fn new(
        wallets: BTreeMap<String, Arc<SolanaWalletAdapter>>,
        api_key: Option<Zeroizing<String>>,
    ) -> Result<Self> {
        let api: Arc<dyn JupiterApi> = HttpJupiterApi::new(api_key)?;
        let rpc = wallets
            .values()
            .next()
            .ok_or_else(|| {
                FinError::validation("Jupiter requires at least one enrolled Solana wallet.")
            })?
            .runtime()
            .rpc()
            .clone();
        let prices: Arc<dyn UsdPriceSource> = Arc::new(JupiterUsdPrices::new(api.clone(), rpc));
        Self::with_price_source(wallets, api, prices)
    }

    pub fn with_api(
        wallets: BTreeMap<String, Arc<SolanaWalletAdapter>>,
        api: Arc<dyn JupiterApi>,
    ) -> Result<Self> {
        let rpc = wallets
            .values()
            .next()
            .ok_or_else(|| {
                FinError::validation("Jupiter requires at least one enrolled Solana wallet.")
            })?
            .runtime()
            .rpc()
            .clone();
        let prices: Arc<dyn UsdPriceSource> = Arc::new(JupiterUsdPrices::new(api.clone(), rpc));
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
            return Err(FinError::validation(
                "Jupiter requires at least one enrolled Solana wallet.",
            ));
        }
        Ok(Self {
            wallets,
            api,
            prices,
            revalidated_frozen: OneShotCache::default(),
        })
    }

    fn cache_frozen_candidate(
        &self,
        prepared: &PreparedOperation,
        frozen: FrozenTransaction,
        auxiliary: AuxiliaryEffects,
    ) {
        let Some(key) = frozen_candidate_cache_key(prepared) else {
            return;
        };
        self.revalidated_frozen.insert(key, (frozen, auxiliary));
    }

    fn take_cached_frozen_candidate(
        &self,
        prepared: &PreparedOperation,
    ) -> Option<(FrozenTransaction, AuxiliaryEffects)> {
        self.revalidated_frozen
            .take(frozen_candidate_cache_key(prepared)?)
    }

    fn wallet(&self, input: &Value) -> Result<Arc<SolanaWalletAdapter>> {
        let name = input
            .get("wallet")
            .and_then(Value::as_str)
            .ok_or_else(|| FinError::validation("Select an enrolled Solana wallet."))?;
        self.wallets.get(name).cloned().ok_or_else(|| {
            FinError::new(
                "JUPITER_WALLET_UNKNOWN",
                "connection",
                "The selected Solana wallet is not enrolled for Jupiter.",
                "Choose an enrolled wallet from connection discovery.",
            )
        })
    }

    fn validate_network(input: &Value) -> Result<()> {
        if input.get("network").and_then(Value::as_str) != Some("solana")
            || input.get("cluster").and_then(Value::as_str) != Some(CLUSTER)
        {
            return Err(FinError::validation(
                "Jupiter operations require Solana mainnet-beta.",
            ));
        }
        Ok(())
    }

    fn intent(input: &Value) -> Result<SwapIntent> {
        let mut exact = input.clone();
        if let Some(object) = exact.as_object_mut() {
            object.remove("cluster");
        }
        serde_json::from_value(exact).map_err(|_| {
            FinError::validation(
                "The Jupiter swap input does not match its exact amount, mint and route schema.",
            )
        })
    }

    async fn fetch_candidate(
        &self,
        intent: &SwapIntent,
        wallet: &SolanaWalletAdapter,
        priority: RequestPriority,
    ) -> Result<Candidate> {
        self.fetch_candidate_with_account_limit(intent, wallet, priority, None)
            .await
    }

    async fn fetch_candidate_with_account_limit(
        &self,
        intent: &SwapIntent,
        wallet: &SolanaWalletAdapter,
        priority: RequestPriority,
        maximum_route_accounts: Option<u8>,
    ) -> Result<Candidate> {
        self.fetch_candidate_with_options(
            intent,
            wallet,
            priority,
            maximum_route_accounts,
            &[],
            None,
        )
        .await
    }

    async fn fetch_candidate_with_options(
        &self,
        intent: &SwapIntent,
        wallet: &SolanaWalletAdapter,
        priority: RequestPriority,
        maximum_route_accounts: Option<u8>,
        excluded_routers: &[String],
        meta_slippage_bps: Option<u16>,
    ) -> Result<Candidate> {
        let address = wallet.address();
        match intent.route {
            SwapRoute::MetaAggregator => {
                let path = swap::meta_order_path_with_slippage(
                    intent,
                    address,
                    meta_slippage_bps.unwrap_or(intent.maximum_slippage_bps),
                    excluded_routers,
                )?;
                let raw = self
                    .api
                    .request(
                        JupiterHost::Platform,
                        Method::GET,
                        &path,
                        None,
                        RateBucket::Main,
                        priority,
                        REQUEST_DEADLINE,
                    )
                    .await?;
                let response: MetaOrderResponse =
                    serde_json::from_value(raw).map_err(|_| bad_provider_response())?;
                let order = swap::validate_meta_order(intent, response)?;
                Ok(Candidate {
                    tx_base64: order.transaction_base64,
                    request_id: Some(order.request_id),
                    router: Some(order.router),
                    last_valid_block_height: order.last_valid_block_height,
                    provider_expire_at: order.provider_expire_at,
                    expected_output_atoms: order.expected_output_atoms,
                    instructions: None,
                    lookup_tables: vec![],
                    blockhash: None,
                    actual_slippage_bps: 0,
                    platform_fee_bps: order.fee_bps,
                    platform_fee_mint: order.fee_mint,
                })
            }
            SwapRoute::Router => {
                if meta_slippage_bps.is_some() {
                    return Err(bad_provider_response());
                }
                let mut path = swap::router_build_path(intent, address)?;
                if let Some(limit) = maximum_route_accounts {
                    path.push_str(&format!("&maxAccounts={limit}"));
                }
                let raw = self
                    .api
                    .request(
                        JupiterHost::Platform,
                        Method::GET,
                        &path,
                        None,
                        RateBucket::Main,
                        priority,
                        REQUEST_DEADLINE,
                    )
                    .await?;
                let built: RouterBuildResponse =
                    serde_json::from_value(raw).map_err(|_| bad_provider_response())?;
                if built.input_mint != candidate_mint(&intent.input_mint)?
                    || built.output_mint != candidate_mint(&intent.output_mint)?
                    || built.in_amount != intent.amount_atoms
                {
                    return Err(FinError::new(
                        "JUPITER_BUILD_BINDING_MISMATCH",
                        "prepare",
                        "The Router build response changed the exact swap assets or input amount.",
                        "Request a fresh build for the approved wallet, mints and amount.",
                    ));
                }
                let output = parse_atoms(&built.out_amount)?;
                let threshold = parse_atoms(&built.other_amount_threshold)?;
                let (_, minimum) = swap::validate_intent(intent)?;
                if output < minimum
                    || threshold < minimum
                    || built.slippage_bps > intent.maximum_slippage_bps
                    || built.swap_mode != "ExactIn"
                {
                    return Err(FinError::new(
                        "JUPITER_BUILD_LIMIT_MISMATCH",
                        "prepare",
                        "The Router build does not enforce the approved minimum output and slippage.",
                        "Request a fresh route with the exact approved output and slippage bounds.",
                    ));
                }
                if built.tip_instruction.is_some() || !built.other_instructions.is_empty() {
                    return Err(FinError::new(
                        "JUPITER_BUILD_EXTRA_INSTRUCTION",
                        "prepare",
                        "The Router build contains an additional instruction whose effect is not included in the approved swap intent.",
                        "Request a route without extra or tip instructions, or review a separate explicit fee bound.",
                    ));
                }
                let swap_data = STANDARD
                    .decode(&built.swap_instruction.data)
                    .map_err(|_| bad_provider_response())?;
                let route_fee = if built.swap_instruction.program_id == swap::JUPITER_V6_PROGRAM {
                    swap::decode_swap_instruction(&swap_data)?.platform_fee_bps
                } else {
                    0
                };
                let mut instructions = Vec::new();
                instructions.extend(built.compute_budget_instructions);
                instructions.extend(built.setup_instructions);
                instructions.push(built.swap_instruction);
                if let Some(cleanup) = built.cleanup_instruction {
                    instructions.push(cleanup);
                }
                let normalized = instructions
                    .into_iter()
                    .map(ApiInstruction::normalize)
                    .collect::<Result<Vec<_>>>()?;
                let hash_bytes: Vec<u8> = built.blockhash_with_metadata.blockhash;
                let _informational_fetch_time = built.blockhash_with_metadata.fetched_at;
                if hash_bytes.len() != 32 {
                    return Err(bad_provider_response());
                }
                let blockhash = bs58::encode(hash_bytes).into_string();
                let lookup_tables = built
                    .addresses_by_lookup_table_address
                    .unwrap_or_default()
                    .into_keys()
                    .collect();
                Ok(Candidate {
                    tx_base64: String::new(),
                    request_id: None,
                    router: None,
                    last_valid_block_height: Some(
                        built.blockhash_with_metadata.last_valid_block_height,
                    ),
                    provider_expire_at: None,
                    expected_output_atoms: output,
                    instructions: Some(normalized),
                    lookup_tables,
                    blockhash: Some(blockhash),
                    actual_slippage_bps: built.slippage_bps,
                    platform_fee_bps: route_fee,
                    platform_fee_mint: None,
                })
            }
        }
    }

    /// Router builds can exceed Solana's packet size even with the provider's
    /// default account limit. Meta-Aggregator can also return a route format we
    /// do not review. Both paths may request a bounded replacement only for the
    /// specific eligible failure; all intent, instruction and simulation checks
    /// run again before a candidate becomes reviewable.
    /// See https://developers.jup.ag/docs/swap/advanced/reduce-transaction-size
    /// and https://developers.jup.ag/docs/swap/order-and-execute#routing-impact-matrix.
    async fn prepare_candidate(
        &self,
        intent: &SwapIntent,
        wallet: &SolanaWalletAdapter,
    ) -> Result<PreparedCandidate> {
        if intent.route == SwapRoute::MetaAggregator {
            return self.prepare_meta_candidate(intent, wallet).await;
        }
        let mut limits = [None, Some(48), Some(32)].into_iter().peekable();
        loop {
            let limit = limits.next().expect("finite candidate attempts");
            let candidate = self
                .fetch_candidate_with_account_limit(
                    intent,
                    wallet,
                    RequestPriority::Financial,
                    limit,
                )
                .await?;
            match self
                .freeze_candidate(intent, wallet, &candidate, true)
                .await
            {
                Ok((frozen, auxiliary)) => {
                    return Ok(PreparedCandidate {
                        candidate,
                        frozen,
                        auxiliary,
                        warnings: Vec::new(),
                    });
                }
                Err(error)
                    if intent.route == SwapRoute::Router
                        && limits.peek().is_some()
                        && router_build_is_oversized(&error) => {}
                Err(error) => return Err(error),
            }
        }
    }

    async fn prepare_meta_candidate(
        &self,
        intent: &SwapIntent,
        wallet: &SolanaWalletAdapter,
    ) -> Result<PreparedCandidate> {
        let mut excluded = Vec::<String>::new();
        let mut rejected = Vec::<String>::new();
        let mut floor_retry_used = false;
        loop {
            let candidate = self
                .fetch_candidate_with_options(
                    intent,
                    wallet,
                    RequestPriority::Financial,
                    None,
                    &excluded,
                    None,
                )
                .await?;
            if !rejected.is_empty()
                && !candidate
                    .router
                    .as_deref()
                    .is_some_and(swap::is_meta_router)
            {
                return Err(bad_provider_response());
            }
            if candidate
                .router
                .as_deref()
                .is_some_and(|router| excluded.iter().any(|excluded| excluded == router))
            {
                return Err(meta_exclusion_ignored(candidate.router.as_deref()));
            }
            let result = self
                .freeze_candidate(intent, wallet, &candidate, true)
                .await;
            match result {
                Ok((frozen, auxiliary)) => {
                    let warnings = if rejected.is_empty() {
                        Vec::new()
                    } else {
                        vec![format!(
                            "Jupiter returned a {} transaction or route instruction format that could not be validated; a freshly validated {} route is prepared.",
                            rejected.join(", "),
                            candidate.router.as_deref().unwrap_or("unknown")
                        )]
                    };
                    return Ok(PreparedCandidate {
                        candidate,
                        frozen,
                        auxiliary,
                        warnings,
                    });
                }
                Err(error)
                    if error.code == "JUPITER_MINIMUM_OUTPUT_UNPROTECTED" && !floor_retry_used =>
                {
                    let minimum_output = intent
                        .minimum_output_atoms
                        .parse::<u64>()
                        .map_err(|_| bad_provider_response())?;
                    let tighter_slippage = swap::minimum_protecting_slippage_bps(
                        candidate.expected_output_atoms,
                        minimum_output,
                        intent.maximum_slippage_bps,
                    )?;
                    if tighter_slippage >= intent.maximum_slippage_bps {
                        return Err(error);
                    }
                    floor_retry_used = true;

                    // This is the sole absolute-floor retry for the entire
                    // preparation, even if format fallback later excludes
                    // this engine. All financial bindings continue to use the
                    // original intent in fetch validation and transaction
                    // freezing; only the provider's requested slippage is
                    // tightened.
                    let tighter_candidate = self
                        .fetch_candidate_with_options(
                            intent,
                            wallet,
                            RequestPriority::Financial,
                            None,
                            &excluded,
                            Some(tighter_slippage),
                        )
                        .await?;
                    if !rejected.is_empty()
                        && !tighter_candidate
                            .router
                            .as_deref()
                            .is_some_and(swap::is_meta_router)
                    {
                        return Err(bad_provider_response());
                    }
                    if tighter_candidate
                        .router
                        .as_deref()
                        .is_some_and(|router| excluded.iter().any(|item| item == router))
                    {
                        return Err(meta_exclusion_ignored(tighter_candidate.router.as_deref()));
                    }
                    match self
                        .freeze_candidate(intent, wallet, &tighter_candidate, true)
                        .await
                    {
                        Ok((frozen, auxiliary)) => {
                            let warnings = if rejected.is_empty() {
                                Vec::new()
                            } else {
                                vec![format!(
                                    "Jupiter returned a {} transaction or route instruction format that could not be validated; a freshly validated {} route is prepared.",
                                    rejected.join(", "),
                                    tighter_candidate.router.as_deref().unwrap_or("unknown")
                                )]
                            };
                            return Ok(PreparedCandidate {
                                candidate: tighter_candidate,
                                frozen,
                                auxiliary,
                                warnings,
                            });
                        }
                        Err(mut retry_error) if meta_format_rejection(&retry_error) => {
                            let Some(router) = tighter_candidate
                                .router
                                .as_deref()
                                .filter(|router| swap::is_meta_router(router))
                            else {
                                return Err(retry_error);
                            };
                            if excluded.len() >= swap::META_AGGREGATOR_ROUTERS.len() {
                                add_meta_recovery_details(&mut retry_error, &rejected, &excluded);
                                return Err(retry_error);
                            }
                            excluded.push(router.to_owned());
                            rejected.push(router.to_owned());
                            if excluded.len() == swap::META_AGGREGATOR_ROUTERS.len() {
                                add_meta_recovery_details(&mut retry_error, &rejected, &excluded);
                                return Err(retry_error);
                            }
                        }
                        Err(retry_error) => return Err(retry_error),
                    }
                }
                Err(mut error) if meta_format_rejection(&error) => {
                    let Some(router) = candidate
                        .router
                        .as_deref()
                        .filter(|router| swap::is_meta_router(router))
                    else {
                        return Err(error);
                    };
                    if excluded.len() >= swap::META_AGGREGATOR_ROUTERS.len() {
                        add_meta_recovery_details(&mut error, &rejected, &excluded);
                        return Err(error);
                    }
                    excluded.push(router.to_owned());
                    rejected.push(router.to_owned());
                    if excluded.len() == swap::META_AGGREGATOR_ROUTERS.len() {
                        add_meta_recovery_details(&mut error, &rejected, &excluded);
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn freeze_candidate(
        &self,
        intent: &SwapIntent,
        wallet: &SolanaWalletAdapter,
        candidate: &Candidate,
        simulate: bool,
    ) -> Result<(FrozenTransaction, AuxiliaryEffects)> {
        let signer = wallet.signer().ok_or_else(|| {
            FinError::new(
                "SOLANA_SIGNING_UNAVAILABLE",
                "prepare",
                "The selected Solana wallet has no protected signing capability.",
                "Enroll a protected signer for this wallet before preparing a swap.",
            )
        })?;
        let runtime = wallet.runtime();
        let frozen = if let Some(instructions) = &candidate.instructions {
            runtime
                .freeze_instructions_with_expiry(
                    instructions,
                    &candidate.lookup_tables,
                    candidate
                        .blockhash
                        .as_deref()
                        .ok_or_else(bad_provider_response)?,
                    &signer,
                    candidate.last_valid_block_height,
                    candidate.provider_expire_at.as_deref(),
                    intent.maximum_network_fee_lamports,
                    candidate.request_id.clone(),
                    &[],
                )
                .await?
        } else {
            let signer_address = signer.address().to_owned();
            let missing = fin_solana::SolanaRuntime::missing_signers(&candidate.tx_base64)?;
            // The enrolled wallet slot is expected to be blank before
            // signing. Permit other empty slots provisionally; the full
            // instruction review below binds every foreign signer to the
            // transaction fee payer or the verified JupiterZ maker role.
            let remote = missing
                .into_iter()
                .filter(|address| address != &signer_address)
                .collect::<Vec<_>>();
            runtime
                .freeze_transaction_with_expiry(
                    &candidate.tx_base64,
                    &signer,
                    candidate.last_valid_block_height,
                    candidate.provider_expire_at.as_deref(),
                    intent.maximum_network_fee_lamports,
                    candidate.request_id.clone(),
                    &remote,
                )
                .await?
        };
        let auxiliary = validate_frozen_intent(intent, wallet, &frozen, candidate)
            .await
            .map_err(|error| {
                validation_router_context(
                    validation_stage_if_unreviewed(error, "frozen_instruction_review"),
                    candidate.router.as_deref(),
                )
            })?;
        runtime.validate_fee(&frozen).await?;
        if simulate {
            runtime.simulate(&frozen).await?;
        }
        Ok((frozen, auxiliary))
    }

    async fn quote(&self, input: &Value) -> Result<Value> {
        Self::validate_network(input)?;
        let intent = quote_intent(input)?;
        let wallet = self.wallet(input)?;
        if intent.wallet != input["wallet"].as_str().unwrap_or_default() {
            return Err(FinError::validation(
                "The quote wallet must match the selected enrolled wallet.",
            ));
        }
        let candidate = self
            .fetch_candidate(&intent, &wallet, RequestPriority::Normal)
            .await?;
        Ok(
            json!({"network":NETWORK,"cluster":CLUSTER,"wallet":wallet.address(),"input_mint":intent.input_mint,"output_mint":intent.output_mint,"input_atoms":intent.amount_atoms,"expected_output_atoms":candidate.expected_output_atoms.to_string(),"minimum_output_atoms":intent.minimum_output_atoms,"maximum_slippage_bps":intent.maximum_slippage_bps,"route":intent.route,"provider_request_id":candidate.request_id,"last_valid_block_height":candidate.last_valid_block_height,"provider_expire_at":candidate.provider_expire_at,"fees":{"estimated_network_fee_lamports":null,"network_fee_estimate_status":"unavailable_until_transaction_review","maximum_platform_fee_bps":candidate.platform_fee_bps,"platform_fee_mint":candidate.platform_fee_mint,"account_funding_estimate_lamports":null,"account_funding_estimate_status":"unavailable_until_transaction_review"},"expires_at":bounded_expiry(&candidate)?}),
        )
    }
}

fn router_build_is_oversized(error: &FinError) -> bool {
    error.code == "SOLANA_TRANSACTION_INVALID"
        && error.details.as_deref().is_some_and(|details| {
            details.get("transaction_stage").and_then(Value::as_str) == Some("compile")
                && details.get("reason").and_then(Value::as_str) == Some("wire_size_limit_exceeded")
        })
}

fn quote_intent(input: &Value) -> Result<SwapIntent> {
    let route = match input
        .get("route")
        .and_then(Value::as_str)
        .unwrap_or("meta_aggregator")
    {
        "meta_aggregator" => swap::SwapRoute::MetaAggregator,
        "router" => swap::SwapRoute::Router,
        _ => {
            return Err(FinError::validation(
                "Select meta_aggregator or router as the quote route.",
            ));
        }
    };
    let amount = input
        .get("amount_atoms")
        .and_then(Value::as_str)
        .ok_or_else(|| FinError::validation("Provide an exact atom amount for the quote."))?;
    // The quote is a read-only route inquiry. These internal broad bounds
    // permit the provider quote to be displayed without granting authority.
    let intent = SwapIntent {
        wallet: input
            .get("wallet")
            .and_then(Value::as_str)
            .ok_or_else(|| FinError::validation("Select an enrolled wallet for the quote."))?
            .to_owned(),
        network: "solana".into(),
        input_mint: input
            .get("input_mint")
            .and_then(Value::as_str)
            .ok_or_else(|| FinError::validation("Provide the exact input mint."))?
            .to_owned(),
        output_mint: input
            .get("output_mint")
            .and_then(Value::as_str)
            .ok_or_else(|| FinError::validation("Provide the exact output mint."))?
            .to_owned(),
        amount_atoms: amount.to_owned(),
        minimum_output_atoms: "1".into(),
        maximum_slippage_bps: input
            .get("maximum_slippage_bps")
            .and_then(Value::as_u64)
            .and_then(|v| u16::try_from(v).ok())
            .unwrap_or(100),
        maximum_platform_fee_bps: u16::MAX,
        maximum_network_fee_lamports: u64::MAX,
        maximum_account_funding_lamports: u64::MAX,
        route,
    };
    swap::validate_intent(&intent)?;
    Ok(intent)
}

#[async_trait]
impl Adapter for JupiterAdapter {
    fn definitions(&self) -> Vec<Definition> {
        swap_operation_definitions()
    }

    fn connections(&self) -> Vec<ConnectionMetadata> {
        self.wallets.iter().map(|(name,wallet)| ConnectionMetadata { name:name.clone(),provider:"jupiter".into(),resource:ResourceId(format!("solana:mainnet-beta:{}",wallet.address())),role:Some("swap".into()),parent:None,network:Some(NETWORK.into()),address:Some(wallet.address().into()),credential_ref:None,signing_enabled:wallet.signer().is_some(),details:json!({"cluster":CLUSTER,"api_key_required":false,"routes":["meta_aggregator","router"]}) }).collect()
    }

    fn snapshot_identities(&self) -> Vec<ConnectionMetadata> {
        Vec::new()
    }

    async fn snapshot(&self) -> Result<StateSnapshot> {
        Ok(StateSnapshot { state:FinancialState::default(),observed_at:Utc::now().to_rfc3339(),complete:true,coverage:vec!["Solana wallet cash and token holdings are observed by the enrolled wallet adapter; Jupiter Swap contributes no duplicate balance rows.".into()],pending:vec![] })
    }

    async fn snapshot_scoped(&self, scope: &SnapshotScope) -> Result<StateSnapshot> {
        if scope.is_full()
            || self.wallets.values().any(|w| {
                scope.matches_network(NETWORK)
                    && scope.matches_resource(&ResourceId(format!(
                        "solana:mainnet-beta:{}",
                        w.address()
                    )))
            })
        {
            self.snapshot().await
        } else {
            Ok(StateSnapshot{state:FinancialState::default(),observed_at:Utc::now().to_rfc3339(),complete:true,coverage:vec!["No Jupiter Swap balance identities matched the requested scope; wallet balances are observed by the wallet adapter.".into()],pending:vec![]})
        }
    }

    async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
        if operation != OP_QUOTE {
            return Err(unknown_operation());
        }
        self.quote(input).await
    }

    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        if operation != OP_SWAP {
            return Err(unknown_operation());
        }
        Self::validate_network(input)?;
        let intent = Self::intent(input)?;
        let wallet = self.wallet(input)?;
        if intent.wallet != input["wallet"].as_str().unwrap_or_default() {
            return Err(FinError::validation(
                "The swap wallet must match the selected enrolled wallet.",
            ));
        }
        let prepared_candidate = self.prepare_candidate(&intent, &wallet).await?;
        let candidate = prepared_candidate.candidate;
        let frozen = prepared_candidate.frozen;
        let runtime = wallet.runtime();
        let signer = wallet.signer().ok_or_else(unknown_operation)?;
        let (rfq_native_input_direct, rfq_native_output_direct) =
            rfq_native_endpoint_flags(&frozen, candidate.router.as_deref());
        let (dflow_native_input_direct, dflow_native_output_direct) =
            dflow_native_endpoint_flags(&frozen, candidate.router.as_deref());
        let resource = ResourceId(format!("solana:mainnet-beta:{}", wallet.address()));
        let auxiliary = prepared_candidate.auxiliary.clone();
        let baseline_anchors = auxiliary.baseline_anchors.clone();
        let wallet_account_funding_lamports = auxiliary
            .funding_accounts
            .iter()
            .filter(|funding| funding.payer == wallet.address())
            .try_fold(0u64, |total, funding| {
                total
                    .checked_add(funding.lamports)
                    .ok_or_else(bad_provider_response)
            })?;
        let input_asset = AssetId {
            network: NETWORK.into(),
            reference: normalize_asset(&intent.input_mint),
            decimals: token_decimals(&runtime, &intent.input_mint).await?,
        };
        let output_asset = AssetId {
            network: NETWORK.into(),
            reference: normalize_asset(&intent.output_mint),
            decimals: token_decimals(&runtime, &intent.output_mint).await?,
        };
        let input_token_program = token_program_for(&runtime, &intent.input_mint).await?;
        let output_token_program = token_program_for(&runtime, &intent.output_mint).await?;
        let (input_atoms, minimum_atoms) = swap::validate_intent(&intent)?;
        let intent_effect = EffectConstraint::Conversion {
            resource: resource.clone(),
            input: input_asset.clone(),
            maximum_input: fin_domain::Quantity::from_u64(input_atoms),
            output: output_asset.clone(),
            minimum_output: fin_domain::Quantity::from_u64(minimum_atoms),
        };
        let (mut valuation, fee_bound) = valuation(
            self.prices.clone(),
            input_asset.clone(),
            input_atoms,
            output_asset.clone(),
            candidate.expected_output_atoms,
            intent.maximum_network_fee_lamports,
            wallet.address(),
        )
        .await?;
        let native = AssetId {
            network: NETWORK.into(),
            reference: "native".into(),
            decimals: 9,
        };
        let native_price = valuation
            .prices
            .iter()
            .find(|price| price.asset == native)
            .ok_or_else(bad_provider_response)?;
        valuation.additional_allocation_usd_micros =
            fin_domain::Quantity::from_u64(wallet_account_funding_lamports)
                .mul_div_ceil(&native_price.usd_micros_per_unit, &native_price.unit_atoms)?;
        let mut submission_data = json!({"wallet_address":wallet.address(),"fee_payer":frozen.fee_payer(),"route":intent.route,"request_id":candidate.request_id,"router":candidate.router,"message_sha256":frozen.message_sha256(),"last_valid_block_height":frozen.last_valid_block_height(),"provider_expire_at":frozen.provider_expire_at(),"recent_blockhash":frozen.recent_blockhash(),"transaction_base64":candidate.tx_base64,"instructions":candidate.instructions,"lookup_tables":candidate.lookup_tables,"blockhash":candidate.blockhash,"actual_slippage_bps":candidate.actual_slippage_bps,"platform_fee_bps":candidate.platform_fee_bps,"platform_fee_mint":candidate.platform_fee_mint,"input_atoms":input_atoms.to_string(),"expected_output_atoms":candidate.expected_output_atoms.to_string(),"minimum_output_atoms":minimum_atoms.to_string(),"maximum_network_fee_lamports":intent.maximum_network_fee_lamports.to_string(),"input_token_program":input_token_program,"output_token_program":output_token_program,"account_funding_lamports":auxiliary.account_funding_lamports,"funding_accounts":auxiliary.funding_accounts,"wrapped_native_lamports":auxiliary.wrapped_native_lamports,"closed_wrapped_native_accounts":auxiliary.closed_wrapped_native_accounts,"closed_wrapped_native_refunds":auxiliary.closed_wrapped_native_refunds,"rfq_native_input_direct":rfq_native_input_direct,"rfq_native_output_direct":rfq_native_output_direct,"dflow_native_input_direct":dflow_native_input_direct,"dflow_native_output_direct":dflow_native_output_direct});
        submission_data = bind_baseline_anchors(submission_data, &baseline_anchors)?;
        let _ = signer;
        let prepared = PreparedOperation {
            operation: operation.into(),
            input: input.clone(),
            binding: submission_data,
            summary: format!(
                "Swap {} {} for at least {} {} on Solana mainnet-beta; max network fee {} SOL; platform fee {}%; {}.",
                display_atomic_amount(input_atoms, input_asset.decimals),
                solana_asset_label(&input_asset.reference, input_asset.decimals),
                display_atomic_amount(minimum_atoms, output_asset.decimals),
                solana_asset_label(&output_asset.reference, output_asset.decimals),
                display_atomic_amount(intent.maximum_network_fee_lamports, 9),
                format_basis_points(candidate.platform_fee_bps),
                if auxiliary.account_funding_lamports == wallet_account_funding_lamports {
                    format!(
                        "account funding {} SOL",
                        display_atomic_amount(auxiliary.account_funding_lamports, 9)
                    )
                } else {
                    format!(
                        "account funding {} SOL total, {} SOL from your wallet",
                        display_atomic_amount(auxiliary.account_funding_lamports, 9),
                        display_atomic_amount(wallet_account_funding_lamports, 9)
                    )
                },
            ),
            checks: vec![
                check("Exact mint and amount binding"),
                check("Actual Jupiter instruction and account review"),
                check("Maximum network fee"),
                check("Simulation"),
                check("Blockhash expiry"),
            ],
            warnings: {
                let mut warnings = prepared_candidate.warnings;
                warnings.push("Settlement is reported only after the finalized transaction proves the exact token effects.".into());
                warnings
            },
            observed_at: Utc::now().to_rfc3339(),
            expires_at: bounded_expiry(&candidate)?,
            valuation: Some(valuation),
            intent: FinancialIntent {
                debits: {
                    let mut debits = vec![ResourceAmount {
                        resource: resource.clone(),
                        amount: Amount {
                            asset: input_asset.clone(),
                            atoms: fin_domain::Quantity::from_u64(input_atoms),
                        },
                    }];
                    if wallet_account_funding_lamports > 0 {
                        debits.push(ResourceAmount {
                            resource: resource.clone(),
                            amount: Amount {
                                asset: native.clone(),
                                atoms: fin_domain::Quantity::from_u64(
                                    wallet_account_funding_lamports,
                                ),
                            },
                        });
                    }
                    debits
                },
                maximum_costs: vec![fee_bound],
                expected: {
                    let mut expected = vec![intent_effect];
                    if wallet_account_funding_lamports > 0 {
                        expected.push(EffectConstraint::Debit {
                            resource: resource.clone(),
                            asset: native.clone(),
                            maximum_debit: fin_domain::Quantity::from_u64(
                                wallet_account_funding_lamports,
                            ),
                        });
                    }
                    for funding in auxiliary.funding_accounts.iter().filter(|funding| {
                        funding.payer == wallet.address()
                            && !auxiliary
                                .closed_wrapped_native_accounts
                                .contains(&funding.address)
                    }) {
                        let position = format!("account-funding:{}", funding.address);
                        let amount = fin_domain::Quantity::from_u64(funding.lamports);
                        expected.push(EffectConstraint::PositionOfKind {
                            resource: resource.clone(),
                            position: position.clone(),
                            asset: native.clone(),
                            position_kind: fin_domain::PositionKind::RefundableAccountFunding,
                            direction: fin_domain::Direction::Increase,
                            maximum_change: amount.clone(),
                        });
                        expected.push(EffectConstraint::OriginMovement {
                            from: AccountingLocation::Cash {
                                resource: resource.clone(),
                            },
                            to: AccountingLocation::Position {
                                resource: resource.clone(),
                                position,
                                position_kind: fin_domain::PositionKind::RefundableAccountFunding,
                            },
                            debit_asset: native.clone(),
                            credit_asset: native.clone(),
                            maximum_debit: amount.clone(),
                            minimum_credit: amount,
                        });
                    }
                    for refund in auxiliary
                        .closed_wrapped_native_refunds
                        .iter()
                        .filter(|refund| {
                            !auxiliary
                                .funding_accounts
                                .iter()
                                .any(|created| created.address == refund.address)
                        })
                    {
                        let position = format!("account-funding:{}", refund.address);
                        let amount = fin_domain::Quantity::from_u64(refund.lamports);
                        expected.push(EffectConstraint::PositionOfKind {
                            resource: resource.clone(),
                            position: position.clone(),
                            asset: native.clone(),
                            position_kind: fin_domain::PositionKind::RefundableAccountFunding,
                            direction: fin_domain::Direction::Decrease,
                            maximum_change: amount.clone(),
                        });
                        expected.push(EffectConstraint::OriginMovement {
                            from: AccountingLocation::Position {
                                resource: resource.clone(),
                                position,
                                position_kind: fin_domain::PositionKind::RefundableAccountFunding,
                            },
                            to: AccountingLocation::Cash {
                                resource: resource.clone(),
                            },
                            debit_asset: native.clone(),
                            credit_asset: native.clone(),
                            maximum_debit: amount.clone(),
                            minimum_credit: amount,
                        });
                        expected.push(EffectConstraint::Credit {
                            to: resource.clone(),
                            asset: native.clone(),
                            minimum_credit: fin_domain::Quantity::from_u64(refund.lamports),
                        });
                    }
                    expected
                },
                locks: vec![resource],
            },
            action_deadline: None,
            operation_version: 1,
        };
        self.cache_frozen_candidate(&prepared, frozen, auxiliary);
        Ok(prepared)
    }

    async fn prepare_boundaries(
        &self,
        prepared: &PreparedOperation,
        bounds: &Value,
    ) -> Result<BoundarySpec> {
        if prepared.operation != OP_SWAP {
            return Err(FinError::new(
                "BOUNDARY_AUTHORIZATION_UNSUPPORTED",
                "authorization",
                "Jupiter boundaries currently support exact-input spot swaps.",
                "Use an exact operation approval for this action.",
            ));
        }
        let intent = Self::intent(&prepared.input)?;
        let wallet = self.wallet(&prepared.input)?;
        let input = AssetId {
            network: NETWORK.into(),
            reference: normalize_asset(&intent.input_mint),
            decimals: token_decimals(&wallet.runtime(), &intent.input_mint).await?,
        };
        let output = AssetId {
            network: NETWORK.into(),
            reference: normalize_asset(&intent.output_mint),
            decimals: token_decimals(&wallet.runtime(), &intent.output_mint).await?,
        };
        let spec =
            crate::boundaries::swap_spec(&intent, wallet.address(), &input, &output, bounds)?;
        let actual_output = prepared.binding["expected_output_atoms"]
            .as_str()
            .ok_or_else(bad_provider_response)?
            .parse::<u64>()
            .map_err(|_| bad_provider_response())?;
        let actual_fee = prepared.binding["platform_fee_bps"]
            .as_u64()
            .and_then(|v| u16::try_from(v).ok())
            .ok_or_else(bad_provider_response)?;
        crate::boundaries::validate_swap_candidate(
            &spec,
            &intent,
            wallet.address(),
            &input,
            &output,
            actual_output,
            actual_fee,
            prepared.binding["platform_fee_mint"].as_str(),
        )?;
        let lifetime = chrono::DateTime::parse_from_rfc3339(&prepared.expires_at)
            .map_err(|_| bad_provider_response())?
            - chrono::DateTime::parse_from_rfc3339(&prepared.observed_at)
                .map_err(|_| bad_provider_response())?;
        let bounds: crate::boundaries::SwapBounds =
            serde_json::from_value(spec.parameters.clone()).map_err(|_| bad_provider_response())?;
        if lifetime.num_seconds() <= 0
            || lifetime.num_seconds() > i64::from(bounds.maximum_quote_lifetime_seconds)
        {
            return Err(FinError::validation(
                "The prepared Jupiter candidate expires outside the approved quote lifetime.",
            ));
        }
        Ok(spec)
    }

    async fn prepare_plan_permission(
        &self,
        operation: &str,
        input: &Value,
        bounds: &Value,
    ) -> Result<PlanPermission> {
        if operation != OP_SWAP {
            return Err(FinError::new(
                "PLAN_PERMISSION_UNSUPPORTED",
                "authorization",
                "Jupiter plan permissions currently support exact-input spot swap steps.",
                "Choose a reviewed swap step or use ordinary exact approval.",
            ));
        }
        Self::validate_network(input)?;
        let intent = Self::intent(input)?;
        let wallet = self.wallet(input)?;
        let input_asset = AssetId {
            network: NETWORK.into(),
            reference: normalize_asset(&intent.input_mint),
            decimals: token_decimals(&wallet.runtime(), &intent.input_mint).await?,
        };
        let output_asset = AssetId {
            network: NETWORK.into(),
            reference: normalize_asset(&intent.output_mint),
            decimals: token_decimals(&wallet.runtime(), &intent.output_mint).await?,
        };
        let spec = crate::boundaries::swap_spec(
            &intent,
            wallet.address(),
            &input_asset,
            &output_asset,
            bounds,
        )?;
        let output = intent.minimum_output_atoms.parse::<u64>().map_err(|_| {
            FinError::validation("Plan swap minimum output must be exact integer atoms.")
        })?;
        crate::boundaries::validate_swap_candidate(
            &spec,
            &intent,
            wallet.address(),
            &input_asset,
            &output_asset,
            output,
            0,
            None,
        )?;
        Ok(PlanPermission {
            recovery_parent: None,
            spec,
            possible_credits: vec![FinancialPlanFundingIdentity {
                resource: ResourceId(format!("solana:mainnet-beta:{}", wallet.address())),
                asset: output_asset,
            }],
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

    fn validate_boundary_candidate(
        &self,
        spec: &BoundarySpec,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        if prepared.operation != OP_SWAP {
            return Err(FinError::new(
                "BOUNDARY_AUTHORIZATION_UNSUPPORTED",
                "authorization",
                "The prepared operation is not a supported Jupiter spot swap.",
                "Use an exact operation approval.",
            ));
        }
        let intent = Self::intent(&prepared.input)?;
        let wallet = self.wallet(&prepared.input)?;
        let input = AssetId {
            network: NETWORK.into(),
            reference: normalize_asset(&intent.input_mint),
            decimals: spec.fixed["input_asset"]["decimals"]
                .as_u64()
                .and_then(|v| u8::try_from(v).ok())
                .ok_or_else(bad_provider_response)?,
        };
        let output = AssetId {
            network: NETWORK.into(),
            reference: normalize_asset(&intent.output_mint),
            decimals: spec.fixed["output_asset"]["decimals"]
                .as_u64()
                .and_then(|v| u8::try_from(v).ok())
                .ok_or_else(bad_provider_response)?,
        };
        let bounds: crate::boundaries::SwapBounds =
            serde_json::from_value(spec.parameters.clone()).map_err(|_| bad_provider_response())?;
        let lifetime = chrono::DateTime::parse_from_rfc3339(&prepared.expires_at)
            .map_err(|_| bad_provider_response())?
            - chrono::DateTime::parse_from_rfc3339(&prepared.observed_at)
                .map_err(|_| bad_provider_response())?;
        if lifetime.num_seconds() <= 0
            || lifetime.num_seconds() > i64::from(bounds.maximum_quote_lifetime_seconds)
        {
            return Err(FinError::validation(
                "Jupiter candidate expiry exceeds the approved quote lifetime.",
            ));
        }
        crate::boundaries::validate_swap_candidate(
            spec,
            &intent,
            wallet.address(),
            &input,
            &output,
            prepared.binding["expected_output_atoms"]
                .as_str()
                .ok_or_else(bad_provider_response)?
                .parse::<u64>()
                .map_err(|_| bad_provider_response())?,
            prepared.binding["platform_fee_bps"]
                .as_u64()
                .and_then(|v| u16::try_from(v).ok())
                .ok_or_else(bad_provider_response)?,
            prepared.binding["platform_fee_mint"].as_str(),
        )
    }

    fn preparation_reuse_window(&self, prepared: &PreparedOperation) -> Option<Duration> {
        (prepared.operation == OP_SWAP).then_some(Duration::from_secs(30))
    }

    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        if prepared.operation != OP_SWAP {
            return Err(unknown_operation());
        }
        let intent = Self::intent(&prepared.input)?;
        let wallet = self.wallet(&prepared.input)?;
        let candidate = Candidate::from_binding(&prepared.binding)?;
        let (frozen, auxiliary) = self
            .freeze_candidate(&intent, &wallet, &candidate, true)
            .await?;
        if prepared
            .binding
            .get("message_sha256")
            .and_then(Value::as_str)
            != Some(frozen.message_sha256())
        {
            return Err(FinError::new(
                "JUPITER_MESSAGE_CHANGED",
                "revalidation",
                "The immutable Jupiter transaction no longer matches the approved message.",
                "Keep the operation unresolved and prepare a fresh candidate within active plan or boundary authority; otherwise request new approval.",
            ));
        }
        let height = wallet
            .runtime()
            .rpc()
            .request("getBlockHeight", json!([{"commitment":"confirmed"}]))
            .await?
            .as_u64()
            .ok_or_else(bad_provider_response)?;
        if frozen
            .last_valid_block_height()
            .is_some_and(|last_valid| height > last_valid)
        {
            return Err(FinError::new(
                "SOLANA_BLOCKHASH_EXPIRED",
                "revalidation",
                "The approved Jupiter transaction has expired and was not sent.",
                "Prepare a fresh candidate within active plan or boundary authority; otherwise request new approval. Do not replay the expired transaction.",
            ));
        }
        self.cache_frozen_candidate(prepared, frozen, auxiliary);
        Ok(prepared.clone())
    }

    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        if prepared.operation != OP_SWAP {
            return Err(unknown_operation());
        }
        let intent = Self::intent(&prepared.input)?;
        let wallet = self.wallet(&prepared.input)?;
        let signer = wallet.signer().ok_or_else(unknown_operation)?;
        let candidate = Candidate::from_binding(&prepared.binding)?;
        let (frozen, _) = self
            .freeze_candidate(&intent, &wallet, &candidate, true)
            .await?;
        if prepared.binding["message_sha256"].as_str() != Some(frozen.message_sha256()) {
            return Err(FinError::new(
                "JUPITER_MESSAGE_CHANGED",
                "dispatch",
                "The frozen transaction no longer matches the approved message.",
                "Keep the operation unresolved and prepare a new exact swap for approval.",
            ));
        }
        if let Some(request_id) = candidate.request_id.as_ref() {
            let sender = MetaExecuteSender {
                api: self.api.clone(),
                request_id: request_id.clone(),
                expires_at: prepared.expires_at.clone(),
            };
            wallet
                .runtime()
                .dispatch_with_sender(&frozen, &signer, prepared, journal, &sender)
                .await
        } else {
            wallet
                .runtime()
                .dispatch(&frozen, &signer, prepared, journal)
                .await
        }
    }

    async fn submit_revalidated(
        &self,
        validated: &fin_domain::RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        validated.assert_fresh()?;
        let prepared = validated.prepared();
        if prepared.operation != OP_SWAP {
            return Err(unknown_operation());
        }
        let intent = Self::intent(&prepared.input)?;
        let wallet = self.wallet(&prepared.input)?;
        let signer = wallet.signer().ok_or_else(unknown_operation)?;
        let candidate = Candidate::from_binding(&prepared.binding)?;
        let (frozen, _auxiliary) = match self.take_cached_frozen_candidate(prepared) {
            Some((frozen, auxiliary))
                if cached_frozen_matches(prepared, &candidate, signer.address(), &frozen) =>
            {
                wallet.runtime().validate_fee(&frozen).await?;
                (frozen, auxiliary)
            }
            Some(_) => {
                return Err(FinError::new(
                    "JUPITER_MESSAGE_CHANGED",
                    "dispatch",
                    "The cached frozen transaction no longer matches the revalidated candidate.",
                    "Keep the operation unresolved and prepare a fresh candidate within active authority.",
                ));
            }
            None => {
                self.freeze_candidate(&intent, &wallet, &candidate, false)
                    .await?
            }
        };
        if prepared.binding["message_sha256"].as_str() != Some(frozen.message_sha256()) {
            return Err(FinError::new(
                "JUPITER_MESSAGE_CHANGED",
                "dispatch",
                "The frozen transaction no longer matches the approved message.",
                "Keep the operation unresolved and prepare a new exact swap for approval.",
            ));
        }
        let height = wallet
            .runtime()
            .rpc()
            .request("getBlockHeight", json!([{"commitment":"confirmed"}]))
            .await?
            .as_u64()
            .ok_or_else(bad_provider_response)?;
        if frozen
            .last_valid_block_height()
            .is_some_and(|last_valid| height > last_valid)
        {
            return Err(FinError::new(
                "SOLANA_BLOCKHASH_EXPIRED",
                "dispatch",
                "The approved Jupiter transaction has expired and was not sent.",
                "Prepare a fresh candidate within active plan or boundary authority; otherwise request new approval. Do not replay the expired transaction.",
            ));
        }
        validated.assert_fresh()?;
        if let Some(request_id) = candidate.request_id.as_ref() {
            let sender = MetaExecuteSender {
                api: self.api.clone(),
                request_id: request_id.clone(),
                expires_at: prepared.expires_at.clone(),
            };
            wallet
                .runtime()
                .dispatch_with_sender(&frozen, &signer, prepared, journal, &sender)
                .await
        } else {
            wallet
                .runtime()
                .dispatch(&frozen, &signer, prepared, journal)
                .await
        }
    }

    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
        let Some(submission) = record.submission.as_ref() else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"No durable Jupiter transaction submission exists."}),
                verified: None,
            });
        };
        let wallet = self.wallet(&record.input)?;
        let outcome = wallet
            .runtime()
            .reconcile_submission(submission, record.dispatch_evidence.as_ref())
            .await?;
        match outcome {
            ReceiptOutcome::NotDispatched {
                reason_code,
                reason,
            } => {
                let verified = verify_not_dispatched(submission, &reason_code, &reason)?;
                Ok(Reconciliation {
                    status: Status::Failed,
                    evidence: verified.evidence.clone(),
                    verified: Some(verified),
                })
            }
            ReceiptOutcome::Pending { confirmed, reason } => Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"confirmed":confirmed,"reason":reason,"submission":submission}),
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
                    let verified = verify_failed_cost(
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
                // Effect decoding is intentionally performed against the exact
                // pre/post token balances from the runtime proof; provider
                // success fields never establish completion.
                let verified = verify_swap_effects(
                    record,
                    submission,
                    &signature,
                    slot,
                    fee_lamports,
                    &proof,
                )?;
                Ok(Reconciliation {
                    status: Status::Completed,
                    evidence: json!({"signature":signature,"slot":slot,"fee_lamports":fee_lamports,"message_sha256":message_sha256,"proof":proof}),
                    verified: Some(verified),
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

        // Re-run the runtime's full persisted-identity and signer validation;
        // never infer a no-send outcome from journal flags alone.
        let wallet = self.wallet(&record.input)?;
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
    verified: &fin_domain::VerifiedEffects,
) -> Option<CandidateExpiryKind> {
    let dispatch = record.dispatch_evidence.as_ref()?;
    if dispatch.get("no_network_send").and_then(Value::as_bool) != Some(true)
        || dispatch.get("chain_signature").is_some()
        || dispatch.get("message_sha256").and_then(Value::as_str)
            != submission
                .data
                .get("message_sha256")
                .and_then(Value::as_str)
        || &verified.submission != submission
        || !verified.effects.is_empty()
        || fin_domain::verified_effect_digest(verified).ok()? != verified.proof_digest
        || verified.evidence.get("state").and_then(Value::as_str) != Some("not_dispatched")
    {
        return None;
    }
    let reason_code = dispatch.get("reason_code").and_then(Value::as_str)?;
    if verified.evidence.get("reason_code").and_then(Value::as_str) != Some(reason_code) {
        return None;
    }
    non_dispatch_expiry_kind(reason_code)
}

fn non_dispatch_expiry_kind(reason_code: &str) -> Option<CandidateExpiryKind> {
    match reason_code {
        "SOLANA_BLOCKHASH_EXPIRED" | "SOLANA_TX_EXPIRED" => {
            Some(CandidateExpiryKind::TransactionLifetime)
        }
        // This code is emitted by the journal's local prepared-candidate
        // freshness check. Authorization/plan-expiry failures have their own
        // codes and must never trigger a candidate refresh.
        "PREVIEW_EXPIRED" => Some(CandidateExpiryKind::Quote),
        _ => None,
    }
}

#[cfg(test)]
mod non_dispatch_expiry_tests {
    use super::*;
    use fin_domain::{FinancialEffect, VerifiedEffects};
    use fin_pricing::UsdQuote;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingRpc(AtomicUsize);

    #[async_trait::async_trait]
    impl fin_solana::SolanaRpc for CountingRpc {
        async fn request(&self, _method: &str, _params: Value) -> Result<Value> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(FinError::validation("Unexpected RPC in expiry hook test."))
        }
    }

    struct TestApi;

    #[async_trait::async_trait]
    impl JupiterApi for TestApi {
        async fn request(
            &self,
            _host: JupiterHost,
            _method: Method,
            _path_and_query: &str,
            _body: Option<Value>,
            _bucket: RateBucket,
            _priority: RequestPriority,
            _max_wait: Duration,
        ) -> Result<Value> {
            Err(FinError::validation(
                "Unexpected Jupiter API in expiry hook test.",
            ))
        }
    }

    struct TestPrices;

    #[async_trait::async_trait]
    impl UsdPriceSource for TestPrices {
        async fn quote(&self, _asset: &AssetId) -> Result<UsdQuote> {
            Err(FinError::validation(
                "Unexpected price query in expiry hook test.",
            ))
        }
    }

    fn record_fixture(reason_code: &str) -> (JupiterAdapter, OperationRecord, Arc<CountingRpc>) {
        let wallet_address = "11111111111111111111111111111111";
        let message_hash = "11".repeat(32);
        let signature = bs58::encode([2u8; 64]).into_string();
        let submission = Submission {
            kind: "solana.transaction".into(),
            reference: format!("solana:message:{message_hash}"),
            data: json!({
                "schema_version":1,
                "message_sha256":message_hash,
                "recent_blockhash":wallet_address,
                "last_valid_block_height":null,
                "lifetime_kind":"blockhash_validity",
                "wallet_signature":signature,
                "signer":wallet_address,
                "required_signers":[wallet_address],
                "existing_signatures":[{"signer":wallet_address,"signature":signature}],
                "missing_cosigners":[]
            }),
        };
        let dispatch_evidence = json!({
            "no_network_send":true,
            "reason_code":reason_code,
            "message_sha256":message_hash
        });
        let mut verified = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects: Vec::<FinancialEffect>::new(),
            evidence: json!({
                "state":"not_dispatched",
                "reason_code":reason_code,
                "financial_effects":[]
            }),
            observed_at: Utc::now().to_rfc3339(),
            proof_digest: String::new(),
        };
        verified.proof_digest = fin_domain::verified_effect_digest(&verified).unwrap();
        let record = OperationRecord {
            id: "attempt-1".into(),
            request_id: "request-1".into(),
            fingerprint: "fingerprint".into(),
            fingerprint_version: 1,
            operation: OP_SWAP.into(),
            input: json!({"wallet":"fixture"}),
            status: Status::Failed,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
            preview: None,
            approval: None,
            submission: Some(submission),
            dispatch_evidence: Some(dispatch_evidence),
            position_baseline: None,
            effects: Some(verified),
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let rpc = Arc::new(CountingRpc(AtomicUsize::new(0)));
        let wallet = Arc::new(
            SolanaWalletAdapter::new(
                "fixture",
                wallet_address,
                None,
                rpc.clone(),
                Arc::new(TestPrices),
            )
            .unwrap(),
        );
        let adapter = JupiterAdapter::with_price_source(
            BTreeMap::from([("fixture".into(), wallet)]),
            Arc::new(TestApi),
            Arc::new(TestPrices),
        )
        .unwrap();
        (adapter, record, rpc)
    }

    #[test]
    fn only_verified_candidate_or_transaction_lifetime_expiry_codes_are_retryable() {
        assert_eq!(
            non_dispatch_expiry_kind("SOLANA_BLOCKHASH_EXPIRED"),
            Some(CandidateExpiryKind::TransactionLifetime)
        );
        assert_eq!(
            non_dispatch_expiry_kind("SOLANA_TX_EXPIRED"),
            Some(CandidateExpiryKind::TransactionLifetime)
        );
        assert_eq!(
            non_dispatch_expiry_kind("PREVIEW_EXPIRED"),
            Some(CandidateExpiryKind::Quote)
        );

        for excluded in [
            "JUPITER_RFQ_EXPIRED",
            "SOLANA_PRE_SEND_GUARD_DENIED",
            "PLAN_AUTHORIZATION_INACTIVE",
            "APPROVAL_EXPIRED",
            "SOLANA_SUBMISSION_UNCERTAIN",
            "SOLANA_RPC_UNAVAILABLE",
            "UNKNOWN",
        ] {
            assert_eq!(non_dispatch_expiry_kind(excluded), None, "{excluded}");
        }
    }

    #[tokio::test]
    async fn verified_not_dispatched_lifetime_expiry_returns_bound_attestation() {
        let (adapter, record, rpc) = record_fixture("SOLANA_BLOCKHASH_EXPIRED");
        let proof = adapter
            .verify_non_dispatch_expiry(&record)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(proof.operation_id(), record.id);
        assert_eq!(proof.submission(), record.submission.as_ref().unwrap());
        assert_eq!(proof.kind(), CandidateExpiryKind::TransactionLifetime);
        assert_eq!(
            proof.effects_digest(),
            record.effects.as_ref().unwrap().proof_digest
        );
        assert_eq!(rpc.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn forged_or_uncertain_records_are_rejected_before_rpc() {
        let (adapter, mut record, rpc) = record_fixture("SOLANA_BLOCKHASH_EXPIRED");

        let mut forged_message = record.clone();
        forged_message.dispatch_evidence.as_mut().unwrap()["message_sha256"] = json!("00");
        assert!(
            adapter
                .verify_non_dispatch_expiry(&forged_message)
                .await
                .unwrap()
                .is_none()
        );

        let mut forged_digest = record.clone();
        forged_digest.effects.as_mut().unwrap().proof_digest = "forged".into();
        assert!(
            adapter
                .verify_non_dispatch_expiry(&forged_digest)
                .await
                .unwrap()
                .is_none()
        );

        let mut no_send_marker = record.clone();
        no_send_marker.dispatch_evidence.as_mut().unwrap()["no_network_send"] = json!(false);
        assert!(
            adapter
                .verify_non_dispatch_expiry(&no_send_marker)
                .await
                .unwrap()
                .is_none()
        );

        record.status = Status::Unknown;
        assert!(
            adapter
                .verify_non_dispatch_expiry(&record)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(rpc.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn authority_denial_and_missing_no_send_proof_are_not_refreshable() {
        let (adapter, mut record, rpc) = record_fixture("PLAN_AUTHORIZATION_INACTIVE");
        assert!(
            adapter
                .verify_non_dispatch_expiry(&record)
                .await
                .unwrap()
                .is_none()
        );

        record.dispatch_evidence = None;
        assert!(
            adapter
                .verify_non_dispatch_expiry(&record)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(rpc.0.load(Ordering::SeqCst), 0);
    }
}

#[derive(Clone, Debug)]
struct Candidate {
    tx_base64: String,
    request_id: Option<String>,
    router: Option<String>,
    last_valid_block_height: Option<u64>,
    provider_expire_at: Option<String>,
    expected_output_atoms: u64,
    instructions: Option<Vec<NormalizedInstruction>>,
    lookup_tables: Vec<String>,
    blockhash: Option<String>,
    actual_slippage_bps: u16,
    platform_fee_bps: u16,
    platform_fee_mint: Option<String>,
}

#[derive(Debug)]
struct PreparedCandidate {
    candidate: Candidate,
    frozen: FrozenTransaction,
    auxiliary: AuxiliaryEffects,
    warnings: Vec<String>,
}

const META_FORMAT_REJECTION_CODE: &str = "unsupported_instruction_format";

fn meta_format_rejection(error: &FinError) -> bool {
    match error.code.as_str() {
        "JUPITER_INSTRUCTION_UNSUPPORTED" => error.details.as_deref().is_some_and(|details| {
            details.get("validation_stage").and_then(Value::as_str)
                == Some("jupiter_route_instruction")
        }),
        "JUPITER_DFLOW_FORMAT_UNREVIEWED" => true,
        "JUPITER_INSTRUCTION_UNREVIEWED" => error.details.as_deref().is_some_and(|details| {
            details.get("fallback_reason").and_then(Value::as_str)
                == Some(META_FORMAT_REJECTION_CODE)
        }),
        "SOLANA_TX_VERSION_UNSUPPORTED" => error.details.as_deref().is_some_and(|details| {
            details.get("transaction_stage").and_then(Value::as_str) == Some("decode")
                && details.get("transaction_version").and_then(Value::as_u64) == Some(1)
        }),
        "SOLANA_TRANSACTION_INVALID" => error.details.as_deref().is_some_and(|details| {
            let stage = details
                .get("transaction_stage")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let reason = details
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default();
            matches!(stage, "decode" | "sanitize")
                && matches!(
                    reason,
                    "invalid_base64"
                        | "empty_transaction"
                        | "wire_deserialization_failed"
                        | "encoded_size_limit_exceeded"
                        | "wire_size_limit_exceeded"
                        | "transaction_sanitize_failed"
                )
        }),
        _ => false,
    }
}

fn meta_exclusion_ignored(router: Option<&str>) -> FinError {
    FinError::new(
        "JUPITER_ROUTER_EXCLUSION_IGNORED",
        "prepare",
        "Jupiter returned a route from an engine excluded after an unsupported format.",
        "Request a fresh order and verify that its router matches the applied exclusions.",
    )
    .with_details(json!({"router":router.unwrap_or("unknown")}))
}

fn mark_meta_route_format_failure(mut error: FinError, validation_stage: &'static str) -> FinError {
    let mut details = error
        .details
        .take()
        .and_then(|details| details.as_object().cloned())
        .unwrap_or_default();
    details.insert("validation_stage".into(), json!(validation_stage));
    details.insert("fallback_reason".into(), json!(META_FORMAT_REJECTION_CODE));
    error.details = Some(Box::new(Value::Object(details)));
    error
}

fn add_meta_recovery_details(
    error: &mut FinError,
    attempted_routers: &[String],
    excluded_routers: &[String],
) {
    let mut details = error
        .details
        .take()
        .and_then(|details| details.as_object().cloned())
        .unwrap_or_default();
    details.insert(
        "recovery_attempted_routers".into(),
        json!(attempted_routers),
    );
    details.insert("recovery_excluded_routers".into(), json!(excluded_routers));
    error.details = Some(Box::new(Value::Object(details)));
}

enum SwapRouteInstruction {
    Jupiter(DecodedSwapInstruction),
    Rfq,
    Okx,
    Dflow(crate::dflow::DecodedDflowSwap),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DecodedOkxSwap {
    amount_in: u64,
    expected_out: u64,
    slippage_bps: u16,
    route_count: usize,
}

impl Candidate {
    fn from_binding(binding: &Value) -> Result<Self> {
        Ok(Self {
            tx_base64: binding
                .get("transaction_base64")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            request_id: binding
                .get("request_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            router: binding
                .get("router")
                .and_then(Value::as_str)
                .map(str::to_owned),
            last_valid_block_height: binding
                .get("last_valid_block_height")
                .and_then(Value::as_u64),
            provider_expire_at: binding
                .get("provider_expire_at")
                .and_then(Value::as_str)
                .map(str::to_owned),
            expected_output_atoms: parse_atoms(
                binding
                    .get("expected_output_atoms")
                    .and_then(Value::as_str)
                    .ok_or_else(bad_provider_response)?,
            )?,
            instructions: binding.get("instructions").and_then(|v| {
                if v.is_null() {
                    None
                } else {
                    serde_json::from_value(v.clone()).ok()
                }
            }),
            lookup_tables: serde_json::from_value(
                binding
                    .get("lookup_tables")
                    .cloned()
                    .ok_or_else(bad_provider_response)?,
            )
            .map_err(|_| bad_provider_response())?,
            blockhash: binding
                .get("blockhash")
                .and_then(Value::as_str)
                .map(str::to_owned),
            actual_slippage_bps: binding
                .get("actual_slippage_bps")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u16,
            platform_fee_bps: binding
                .get("platform_fee_bps")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u16,
            platform_fee_mint: binding
                .get("platform_fee_mint")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }
}

fn cached_frozen_matches(
    prepared: &PreparedOperation,
    candidate: &Candidate,
    signer: &str,
    frozen: &FrozenTransaction,
) -> bool {
    let maximum_fee = prepared
        .binding
        .get("maximum_network_fee_lamports")
        .and_then(Value::as_str)
        .and_then(|value| parse_atoms(value).ok());
    let expected_expire_at = prepared
        .binding
        .get("provider_expire_at")
        .and_then(Value::as_str);
    prepared
        .binding
        .get("message_sha256")
        .and_then(Value::as_str)
        == Some(frozen.message_sha256())
        && prepared.binding.get("fee_payer").and_then(Value::as_str) == Some(frozen.fee_payer())
        && candidate.request_id.as_deref() == frozen.provider_request_id()
        && prepared
            .binding
            .get("recent_blockhash")
            .and_then(Value::as_str)
            == Some(frozen.recent_blockhash())
        && prepared
            .binding
            .get("last_valid_block_height")
            .and_then(Value::as_u64)
            == frozen.last_valid_block_height()
        && expected_expire_at == frozen.provider_expire_at()
        && maximum_fee == Some(frozen.maximum_network_fee_lamports())
        && frozen.signer() == signer
}

fn bounded_expiry(candidate: &Candidate) -> Result<String> {
    let preview_deadline = Utc::now() + ChronoDuration::seconds(20);
    if let Some(provider_expire_at) = &candidate.provider_expire_at {
        let provider_deadline = chrono::DateTime::parse_from_rfc3339(provider_expire_at)
            .map_err(|_| bad_provider_response())?
            .to_utc();
        if provider_deadline <= Utc::now() {
            return Err(FinError::new(
                "JUPITER_RFQ_EXPIRED",
                "prepare",
                "The Jupiter market-maker quote has expired.",
                "Request a fresh quote before seeking approval.",
            ));
        }
        return Ok(provider_deadline.min(preview_deadline).to_rfc3339());
    }
    Ok(preview_deadline.to_rfc3339())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RouterBuildResponse {
    input_mint: String,
    output_mint: String,
    in_amount: String,
    out_amount: String,
    other_amount_threshold: String,
    swap_mode: String,
    slippage_bps: u16,
    compute_budget_instructions: Vec<ApiInstruction>,
    setup_instructions: Vec<ApiInstruction>,
    swap_instruction: ApiInstruction,
    cleanup_instruction: Option<ApiInstruction>,
    #[serde(default)]
    other_instructions: Vec<ApiInstruction>,
    tip_instruction: Option<ApiInstruction>,
    addresses_by_lookup_table_address: Option<BTreeMap<String, Vec<String>>>,
    blockhash_with_metadata: BlockhashMetadata,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BlockhashMetadata {
    blockhash: Vec<u8>,
    last_valid_block_height: u64,
    /// Informational only; transaction validity remains bound to the exact
    /// returned blockhash and last-valid block height.
    #[serde(default)]
    fetched_at: Option<BlockhashFetchTime>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BlockhashFetchTime {
    #[serde(rename = "secs_since_epoch")]
    _secs_since_epoch: i64,
    #[serde(rename = "nanos_since_epoch")]
    _nanos_since_epoch: i32,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ApiInstruction {
    program_id: String,
    accounts: Vec<ApiAccount>,
    data: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ApiAccount {
    pubkey: String,
    is_signer: bool,
    is_writable: bool,
}
impl ApiInstruction {
    fn normalize(self) -> Result<NormalizedInstruction> {
        Ok(NormalizedInstruction {
            program_id: fin_solana::parse_address(&self.program_id)?,
            accounts: self
                .accounts
                .into_iter()
                .map(|a| {
                    Ok(NormalizedAccountMeta {
                        address: fin_solana::parse_address(&a.pubkey)?,
                        signer: a.is_signer,
                        writable: a.is_writable,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            data_base64: {
                let bytes = STANDARD
                    .decode(self.data)
                    .map_err(|_| bad_provider_response())?;
                STANDARD.encode(bytes)
            },
        })
    }
}

async fn validate_frozen_intent(
    intent: &SwapIntent,
    wallet: &SolanaWalletAdapter,
    frozen: &FrozenTransaction,
    candidate: &Candidate,
) -> Result<AuxiliaryEffects> {
    let allowed_mints = [
        candidate_mint(&intent.input_mint)?,
        candidate_mint(&intent.output_mint)?,
    ];
    let (rfq_native_input_direct, _) =
        rfq_native_endpoint_flags(frozen, candidate.router.as_deref());
    let dflow_native_input = if candidate.router.as_deref() == Some("dflow") {
        frozen.instructions().iter().find_map(|ix| {
            if ix.program_id != DFLOW_PROGRAM {
                return None;
            }
            STANDARD
                .decode(&ix.data_base64)
                .ok()
                .and_then(|data| crate::dflow::decode_swap(&data).ok())
                .and_then(|decoded| decoded.wrap_sol_lamports)
        })
    } else {
        None
    };
    let expected_wrap = if intent.input_mint == "native"
        && !rfq_native_input_direct
        && dflow_native_input.is_none()
    {
        Some(
            intent
                .amount_atoms
                .parse::<u64>()
                .map_err(|_| unsafe_instruction())?,
        )
    } else {
        None
    };
    let auxiliary = validate_auxiliary_instructions(
        wallet,
        frozen,
        &allowed_mints,
        expected_wrap,
        intent.maximum_account_funding_lamports,
    )
    .await
    .map_err(|error| validation_stage_if_unreviewed(error, "auxiliary_instructions"))?;
    let mut route: Option<(&NormalizedInstruction, SwapRouteInstruction)> = None;
    let mut rfq_maker: Option<String> = None;
    let mut account_funding = 0u64;
    let mut native_transfer: Option<(String, u64)> = None;
    for ix in frozen.instructions() {
        let data = STANDARD
            .decode(&ix.data_base64)
            .map_err(|_| bad_provider_response())?;
        match ix.program_id.as_str() {
            swap::COMPUTE_BUDGET_PROGRAM => validate_compute_budget(&data)
                .map_err(|error| validation_stage_if_unreviewed(error, "compute_budget"))?,
            swap::JUPITER_V6_PROGRAM => {
                if route.is_some() {
                    return Err(unsafe_instruction());
                }
                let decoded = swap::decode_swap_instruction(&data).map_err(|error| {
                    validation_stage_if_unreviewed(error, "jupiter_route_instruction")
                })?;
                swap::validate_order_instruction(intent, &decoded).map_err(|error| {
                    validation_stage_if_unreviewed(error, "jupiter_route_instruction")
                })?;
                if decoded.slippage_bps < candidate.actual_slippage_bps
                    || decoded.slippage_bps > intent.maximum_slippage_bps
                {
                    return Err(unsafe_instruction());
                }
                route = Some((ix, SwapRouteInstruction::Jupiter(decoded)));
            }
            ORDER_ENGINE_PROGRAM => {
                if route.is_some() || candidate.router.as_deref() != Some("jupiterz") {
                    return Err(unsafe_instruction());
                }
                validate_order_engine_fill(intent, candidate, wallet, ix, &data, &auxiliary)
                    .await
                    .map_err(|error| validation_stage_if_unreviewed(error, "rfq_route_accounts"))?;
                rfq_maker = ix.accounts.get(1).map(|account| account.address.clone());
                route = Some((ix, SwapRouteInstruction::Rfq));
            }
            DFLOW_PROGRAM => {
                if route.is_some() || candidate.router.as_deref() != Some("dflow") {
                    return Err(unsafe_instruction());
                }
                let decoded = crate::dflow::decode_swap(&data).map_err(|error| {
                    mark_meta_route_format_failure(error, "dflow_route_instruction")
                })?;
                crate::dflow::require_reviewed_format(&decoded).map_err(|error| {
                    mark_meta_route_format_failure(error, "dflow_route_instruction")
                })?;
                validate_dflow_swap_accounts(wallet, ix, &decoded, intent, &auxiliary)
                    .await
                    .map_err(|error| {
                        validation_stage_if_unreviewed(error, "dflow_route_accounts")
                    })?;
                route = Some((ix, SwapRouteInstruction::Dflow(decoded)));
            }
            OKX_ROUTER_PROGRAM => {
                if route.is_some() || candidate.router.as_deref() != Some("okx") {
                    return Err(unsafe_instruction());
                }
                let decoded = decode_okx_swap(&data, intent, candidate).map_err(|error| {
                    validation_stage_if_unreviewed(error, "okx_route_instruction")
                })?;
                validate_okx_swap_accounts(wallet, ix, &decoded, intent, &auxiliary)
                    .await
                    .map_err(|error| validation_stage_if_unreviewed(error, "okx_route_accounts"))?;
                route = Some((ix, SwapRouteInstruction::Okx));
            }
            swap::ASSOCIATED_TOKEN_PROGRAM => {
                let _ = (ix, &data);
            }
            swap::SYSTEM_PROGRAM => {
                if data.len() == 12
                    && u32::from_le_bytes(data[..4].try_into().map_err(|_| unsafe_instruction())?)
                        == 2
                {
                    let amount = u64::from_le_bytes(
                        data[4..12].try_into().map_err(|_| unsafe_instruction())?,
                    );
                    native_transfer = Some((
                        ix.accounts
                            .get(1)
                            .ok_or_else(unsafe_instruction)?
                            .address
                            .clone(),
                        amount,
                    ));
                }
                account_funding = account_funding
                    .checked_add(validate_system_instruction(
                        intent,
                        wallet.address(),
                        frozen,
                        ix,
                        &data,
                    )?)
                    .ok_or_else(unsafe_instruction)?;
            }
            fin_solana::token::TOKEN_PROGRAM_ID | fin_solana::token::TOKEN_2022_PROGRAM_ID => {
                validate_token_setup(wallet.address(), ix, &data)?
            }
            MEMO_PROGRAM if data.len() > 566 => return Err(unsafe_instruction()),
            MEMO_PROGRAM => {}
            _ => return Err(unsafe_instruction()),
        }
    }
    let (route_ix, decoded) = route.ok_or_else(unsafe_instruction)?;
    account_funding = account_funding
        .checked_add(auxiliary.account_funding_lamports)
        .ok_or_else(unsafe_instruction)?;
    if account_funding > intent.maximum_account_funding_lamports {
        return Err(FinError::new(
            "JUPITER_ACCOUNT_FUNDING_LIMIT_EXCEEDED",
            "prepare",
            "The transaction's token-account setup exceeds the approved SOL funding limit.",
            "Review a new transaction with lower setup costs or an explicit funding bound.",
        ));
    }
    match decoded {
        SwapRouteInstruction::Jupiter(decoded) => {
            if decoded.platform_fee_bps != candidate.platform_fee_bps {
                return Err(unsafe_instruction_at("platform-fee quote binding"));
            }
            validate_route_token_accounts(
                wallet,
                route_ix,
                &decoded,
                intent,
                &auxiliary,
                candidate.platform_fee_mint.as_deref(),
            )
            .await
            .map_err(|error| validation_stage_if_unreviewed(error, "jupiter_route_accounts"))?;
            if let Some((destination, amount)) = native_transfer {
                let source_index = match decoded.variant {
                    swap::SwapInstructionVariant::SharedExactIn
                    | swap::SwapInstructionVariant::SharedExactOut
                    | swap::SwapInstructionVariant::SharedTokenLedgerExactIn => 3,
                    swap::SwapInstructionVariant::ExactInV2
                    | swap::SwapInstructionVariant::ExactOutV2 => 1,
                    swap::SwapInstructionVariant::SharedExactInV2
                    | swap::SwapInstructionVariant::SharedExactOutV2 => 2,
                    _ => 2,
                };
                if intent.input_mint != "native"
                    || route_ix
                        .accounts
                        .get(source_index)
                        .map(|account| account.address.as_str())
                        != Some(destination.as_str())
                    || amount
                        != intent
                            .amount_atoms
                            .parse::<u64>()
                            .map_err(|_| unsafe_instruction())?
                {
                    return Err(unsafe_instruction());
                }
            } else if intent.input_mint == "native" {
                return Err(unsafe_instruction());
            }
        }
        SwapRouteInstruction::Rfq => {}
        SwapRouteInstruction::Okx => {
            if let Some((destination, amount)) = native_transfer {
                if intent.input_mint != "native"
                    || route_ix
                        .accounts
                        .get(1)
                        .map(|account| account.address.as_str())
                        != Some(destination.as_str())
                    || amount
                        != intent
                            .amount_atoms
                            .parse::<u64>()
                            .map_err(|_| unsafe_instruction())?
                {
                    return Err(unsafe_instruction());
                }
            } else if intent.input_mint == "native" {
                return Err(unsafe_instruction());
            }
        }
        SwapRouteInstruction::Dflow(decoded) => {
            validate_dflow_bounds(&decoded, intent, candidate)?;
            let input_atoms = intent
                .amount_atoms
                .parse::<u64>()
                .map_err(|_| unsafe_instruction())?;
            if let Some((destination, amount)) = native_transfer
                && (intent.input_mint != "native"
                    || amount != input_atoms
                    || !route_ix
                        .accounts
                        .iter()
                        .any(|account| account.address == destination))
            {
                return Err(unsafe_instruction());
            }
        }
    }
    // A distinct fee payer is supported only when it is a required signer and
    // the actual wallet remains the swap authority. Runtime persists both.
    if !frozen
        .required_signers()
        .contains(&frozen.fee_payer().to_owned())
    {
        return Err(unsafe_instruction());
    }
    let allowed_signers = [
        wallet.address(),
        frozen.fee_payer(),
        rfq_maker.as_deref().unwrap_or(""),
    ];
    if !frozen
        .required_signers()
        .iter()
        .any(|signer| signer == wallet.address())
        || frozen
            .required_signers()
            .iter()
            .any(|signer| !allowed_signers.contains(&signer.as_str()))
    {
        return Err(unsafe_instruction());
    }
    for signer in frozen.missing_cosigners() {
        if signer != frozen.fee_payer() && rfq_maker.as_deref() != Some(signer.as_str()) {
            return Err(unsafe_instruction());
        }
    }
    Ok(auxiliary)
}

fn validate_dflow_bounds(
    decoded: &crate::dflow::DecodedDflowSwap,
    intent: &SwapIntent,
    candidate: &Candidate,
) -> Result<()> {
    let input_atoms = intent
        .amount_atoms
        .parse::<u64>()
        .map_err(|_| unsafe_instruction())?;
    let output_mint = candidate_mint(&intent.output_mint)?;
    let minimum_after_slippage = u128::from(decoded.quoted_output)
        .checked_mul(u128::from(10_000u16.saturating_sub(decoded.slippage_bps)))
        .ok_or_else(unsafe_instruction)?
        / 10_000;
    if decoded.platform_fee_bps != candidate.platform_fee_bps
        || decoded.platform_fee_bps > intent.maximum_platform_fee_bps
        || (decoded.platform_fee_bps == 0) != candidate.platform_fee_mint.is_none()
        || candidate
            .platform_fee_mint
            .as_deref()
            .is_some_and(|mint| mint != output_mint)
        || decoded.positive_slippage_fee_limit_pct != 0
        || !decoded.unsupported_actions.is_empty()
        || decoded.quoted_output != candidate.expected_output_atoms
        || decoded.slippage_bps > intent.maximum_slippage_bps
        || minimum_after_slippage < u128::from(swap::validate_intent(intent)?.1)
        || decoded
            .swap_input_atoms
            .and_then(|amount| amount.checked_add(decoded.transfer_fee_atoms))
            != Some(input_atoms)
        || (intent.input_mint == "native" && decoded.wrap_sol_lamports != Some(input_atoms))
        || (intent.input_mint != "native" && decoded.wrap_sol_lamports.is_some())
        || ((intent.output_mint == "native") != decoded.output_native)
    {
        return Err(unsafe_instruction());
    }
    Ok(())
}

/// Decode the two optional OrderEngine token-account slots from the pinned
/// fill IDL. The transaction instruction validator independently checks the
/// complete instruction layout and mint identity before this result is bound.
fn rfq_native_endpoint_flags(frozen: &FrozenTransaction, router: Option<&str>) -> (bool, bool) {
    if router != Some("jupiterz") {
        return (false, false);
    }
    frozen
        .instructions()
        .iter()
        .find(|ix| ix.program_id == ORDER_ENGINE_PROGRAM && ix.data_base64.len() >= 12)
        .map(|ix| {
            (
                ix.accounts
                    .get(2)
                    .is_some_and(|meta| meta.address == ORDER_ENGINE_PROGRAM),
                ix.accounts
                    .get(4)
                    .is_some_and(|meta| meta.address == ORDER_ENGINE_PROGRAM),
            )
        })
        .unwrap_or((false, false))
}

fn dflow_native_endpoint_flags(frozen: &FrozenTransaction, router: Option<&str>) -> (bool, bool) {
    if router != Some("dflow") {
        return (false, false);
    }
    frozen
        .instructions()
        .iter()
        .find(|ix| ix.program_id == DFLOW_PROGRAM)
        .and_then(|ix| STANDARD.decode(&ix.data_base64).ok())
        .and_then(|data| crate::dflow::decode_swap(&data).ok())
        .map(|decoded| (decoded.wrap_sol_lamports.is_some(), decoded.output_native))
        .unwrap_or((false, false))
}

fn validate_compute_budget(data: &[u8]) -> Result<()> {
    match data.first().copied() {
        Some(2)
            if data.len() == 5
                && u32::from_le_bytes(data[1..5].try_into().map_err(|_| unsafe_instruction())?)
                    <= 1_400_000 =>
        {
            Ok(())
        }
        Some(3) if data.len() == 9 => Ok(()),
        Some(4)
            if data.len() == 5
                && u32::from_le_bytes(data[1..5].try_into().map_err(|_| unsafe_instruction())?)
                    <= 262_144 =>
        {
            Ok(())
        }
        Some(5)
            if data.len() == 5
                && u32::from_le_bytes(data[1..5].try_into().map_err(|_| unsafe_instruction())?)
                    <= 67_108_864 =>
        {
            Ok(())
        }
        _ => Err(unsafe_instruction()),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AuxiliaryEffects {
    pub account_funding_lamports: u64,
    pub wrapped_native_lamports: u64,
    pub closed_wrapped_native_accounts: Vec<String>,
    pub closed_wrapped_native_refunds: Vec<AccountFundingRequirement>,
    pub created_token_accounts: Vec<CreatedTokenAccount>,
    pub funding_accounts: Vec<AccountFundingRequirement>,
    /// Exact finalized pre-dispatch refundable-rent positions that would
    /// otherwise require a broader position observer snapshot.
    pub baseline_anchors: Vec<fin_domain::PositionAnchor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AccountFundingRequirement {
    pub address: String,
    pub lamports: u64,
    pub account_owner: String,
    pub refund_authority: String,
    pub payer: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CreatedTokenAccount {
    pub address: String,
    pub mint: String,
    pub token_program: String,
}

fn refundable_rent_anchor(
    wallet: &str,
    address: &str,
    lamports: u64,
    observed_at: String,
    observation: Value,
) -> fin_domain::PositionAnchor {
    fin_domain::PositionAnchor {
        resource: ResourceId(format!("solana:mainnet-beta:{wallet}")),
        position: format!("account-funding:{address}"),
        position_kind: fin_domain::PositionKind::RefundableAccountFunding,
        principal: Amount {
            asset: AssetId {
                network: NETWORK.into(),
                reference: "native".into(),
                decimals: 9,
            },
            atoms: fin_domain::Quantity::from_u64(lamports),
        },
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"cluster":CLUSTER,"account":address,"observed_at":observed_at,"observation":observation}),
        observed_at,
    }
}

fn finalized_account_observation(info: &Value) -> Result<(String, u64)> {
    let observed_at = Utc::now().to_rfc3339();
    let slot = info
        .pointer("/context/slot")
        .and_then(Value::as_u64)
        .ok_or_else(|| unsafe_instruction_at("finalized account observation context"))?;
    Ok((observed_at, slot))
}

fn bind_baseline_anchors(
    mut binding: Value,
    anchors: &[fin_domain::PositionAnchor],
) -> Result<Value> {
    if !anchors.is_empty() {
        binding[fin_domain::BASELINE_ANCHORS_KEY] =
            serde_json::to_value(anchors).map_err(|_| bad_provider_response())?;
    }
    Ok(binding)
}

#[cfg(test)]
mod rent_baseline_binding_tests {
    use super::*;

    #[test]
    fn prepared_binding_carries_exact_finalized_rent_baseline() {
        let anchor = refundable_rent_anchor(
            "11111111111111111111111111111111",
            "SyntheticAta1111111111111111111111111111",
            0,
            "2026-10-14T12:00:00Z".into(),
            json!({"observation":"getAccountInfo","commitment":"finalized","account_existed":false,"account_absence_proven":true,"context_slot":42,"account_owner":"Tokenkeg","refund_authority":"11111111111111111111111111111111","payer":"11111111111111111111111111111111","rent_lamports":2_039_280}),
        );
        let binding = bind_baseline_anchors(
            json!({"request_id":"prepared-candidate"}),
            std::slice::from_ref(&anchor),
        )
        .expect("serialize broker-owned baseline");
        let rows: Vec<fin_domain::PositionAnchor> =
            serde_json::from_value(binding[fin_domain::BASELINE_ANCHORS_KEY].clone())
                .expect("prepared binding contains typed anchors");
        assert_eq!(rows, vec![anchor]);
        assert_eq!(binding["request_id"], "prepared-candidate");
        assert_eq!(rows[0].principal.atoms, fin_domain::Quantity::zero());
        assert_eq!(rows[0].observed_at, "2026-10-14T12:00:00Z");
        assert_eq!(rows[0].evidence["observed_at"], rows[0].observed_at);
        assert_eq!(rows[0].evidence["observation"]["commitment"], "finalized");
        assert_eq!(
            rows[0].evidence["observation"]["account_absence_proven"],
            true
        );
    }

    #[test]
    fn rent_observation_rejects_missing_or_null_finalized_context_slot() {
        assert!(finalized_account_observation(&json!({"value":null})).is_err());
        assert!(
            finalized_account_observation(&json!({"context":{"slot":null},"value":null})).is_err()
        );
        assert_eq!(
            finalized_account_observation(&json!({"context":{"slot":42},"value":null}))
                .unwrap()
                .1,
            42
        );
    }
}

/// Validate common transaction setup instructions shared by Jupiter protocols.
/// Protocol adapters validate their own main instruction separately and call
/// this once with every allowed mint and an exact optional native wrap amount.
/// It rejects unknown programs and returns only finalized-RPC-verified rent.
pub(crate) async fn validate_auxiliary_instructions(
    wallet: &SolanaWalletAdapter,
    frozen: &FrozenTransaction,
    allowed_mints: &[String],
    expected_native_wrap_lamports: Option<u64>,
    maximum_account_funding_lamports: u64,
) -> Result<AuxiliaryEffects> {
    validate_auxiliary_instructions_with_primary_program(
        wallet,
        frozen,
        allowed_mints,
        expected_native_wrap_lamports,
        maximum_account_funding_lamports,
        None,
    )
    .await
}

/// Validate transaction setup instructions while leaving one already-decoded
/// protocol instruction to its adapter-specific account-graph validator.
pub(crate) async fn validate_auxiliary_instructions_with_primary_program(
    wallet: &SolanaWalletAdapter,
    frozen: &FrozenTransaction,
    allowed_mints: &[String],
    expected_native_wrap_lamports: Option<u64>,
    maximum_account_funding_lamports: u64,
    primary_program: Option<&str>,
) -> Result<AuxiliaryEffects> {
    validate_auxiliary_instructions_with_authorities(
        wallet,
        frozen,
        allowed_mints,
        expected_native_wrap_lamports,
        maximum_account_funding_lamports,
        primary_program,
        &[wallet.address().to_owned()],
    )
    .await
}

/// Validate common setup instructions while permitting explicitly reviewed
/// token-account authorities such as a protocol request PDA. The account
/// address is still derived from the exact authority, token program and mint;
/// every authority is bound by the caller's protocol instruction decoder.
pub(crate) async fn validate_auxiliary_instructions_with_authorities(
    wallet: &SolanaWalletAdapter,
    frozen: &FrozenTransaction,
    allowed_mints: &[String],
    expected_native_wrap_lamports: Option<u64>,
    maximum_account_funding_lamports: u64,
    primary_program: Option<&str>,
    allowed_ata_authorities: &[String],
) -> Result<AuxiliaryEffects> {
    let mut effects = AuxiliaryEffects::default();
    let payer = frozen.fee_payer();
    let native_ata = associated_address(
        wallet.address(),
        fin_solana::token::TOKEN_PROGRAM_ID,
        swap::WRAPPED_SOL_MINT,
    )?;
    for ix in frozen.instructions() {
        if primary_program.is_some_and(|program| ix.program_id == program) {
            continue;
        }
        let data = STANDARD
            .decode(&ix.data_base64)
            .map_err(|_| unsafe_instruction())?;
        match ix.program_id.as_str() {
            swap::COMPUTE_BUDGET_PROGRAM => validate_compute_budget(&data)?,
            swap::ASSOCIATED_TOKEN_PROGRAM => {
                if data.len() != 1 || data[0] > 1 || ix.accounts.len() != 6 {
                    return Err(unsafe_instruction_at("associated-token instruction layout"));
                }
                let a = &ix.accounts;
                validate_associated_create_accounts(
                    a,
                    payer,
                    wallet.address(),
                    allowed_mints,
                    allowed_ata_authorities,
                )
                .map_err(|error| {
                    validation_stage_if_unreviewed(error, "associated-token account graph")
                })?;
                let account_info = wallet
                    .runtime()
                    .rpc()
                    .request(
                        "getAccountInfo",
                        json!([a[1].address,{"encoding":"base64","commitment":"finalized"}]),
                    )
                    .await?;
                let (account_observed_at, account_context_slot) =
                    finalized_account_observation(&account_info)?;
                let account_value = account_info
                    .get("value")
                    .ok_or_else(|| unsafe_instruction_at("associated-token account response"))?;
                if !account_value.is_null() {
                    let account = account_value;
                    if account.get("owner").and_then(Value::as_str) != Some(a[5].address.as_str()) {
                        return Err(unsafe_instruction_at(
                            "associated-token existing account program",
                        ));
                    }
                    let bytes = fin_solana::token::decode_rpc_account_data(
                        account.get("data").ok_or_else(|| {
                            unsafe_instruction_at("associated-token existing account data")
                        })?,
                    )
                    .map_err(|error| {
                        validation_stage_if_unreviewed(
                            error,
                            "associated-token existing account data",
                        )
                    })?;
                    let parsed = fin_solana::token::parse_token_account(
                        &a[5].address,
                        &a[1].address,
                        &bytes,
                    )
                    .map_err(|error| {
                        validation_stage_if_unreviewed(
                            error,
                            "associated-token existing account layout",
                        )
                    })?;
                    if parsed.token_owner != a[2].address
                        || parsed.mint != a[3].address
                        || parsed.state != "initialized"
                    {
                        return Err(unsafe_instruction_at(
                            "associated-token existing account identity",
                        ));
                    }
                } else {
                    let mint_info = wallet
                        .runtime()
                        .rpc()
                        .request(
                            "getAccountInfo",
                            json!([a[3].address,{"encoding":"base64","commitment":"finalized"}]),
                        )
                        .await?;
                    let mint_account = mint_info
                        .get("value")
                        .filter(|value| !value.is_null())
                        .ok_or_else(|| unsafe_instruction_at("associated-token mint existence"))?;
                    if mint_account.get("owner").and_then(Value::as_str)
                        != Some(a[5].address.as_str())
                    {
                        return Err(unsafe_instruction_at(
                            "associated-token mint program binding",
                        ));
                    }
                    let mint_data = fin_solana::token::decode_rpc_account_data(
                        mint_account.get("data").ok_or_else(|| {
                            unsafe_instruction_at("associated-token mint account data")
                        })?,
                    )
                    .map_err(|error| {
                        validation_stage_if_unreviewed(error, "associated-token mint account data")
                    })?;
                    let account_len =
                        fin_solana::token::required_token_account_len(&a[5].address, &mint_data)
                            .map_err(|error| {
                                validation_stage_if_unreviewed(
                                    error,
                                    "associated-token rent layout",
                                )
                            })?;
                    let rent = wallet
                        .runtime()
                        .rpc()
                        .request(
                            "getMinimumBalanceForRentExemption",
                            json!([account_len,{"commitment":"finalized"}]),
                        )
                        .await?
                        .as_u64()
                        .ok_or_else(|| unsafe_instruction_at("associated-token rent response"))?;
                    effects.account_funding_lamports = effects
                        .account_funding_lamports
                        .checked_add(rent)
                        .ok_or_else(|| unsafe_instruction_at("associated-token rent total"))?;
                    effects.created_token_accounts.push(CreatedTokenAccount {
                        address: a[1].address.clone(),
                        mint: a[3].address.clone(),
                        token_program: a[5].address.clone(),
                    });
                    effects.funding_accounts.push(AccountFundingRequirement {
                        address: a[1].address.clone(),
                        lamports: rent,
                        account_owner: a[5].address.clone(),
                        refund_authority: a[2].address.clone(),
                        payer: a[0].address.clone(),
                    });
                    if a[0].address == wallet.address() {
                        effects.baseline_anchors.push(refundable_rent_anchor(
                            wallet.address(),
                            &a[1].address,
                            0,
                            account_observed_at.clone(),
                            json!({"observation":"getAccountInfo","commitment":"finalized","account_existed":false,"account_absence_proven":true,"context_slot":account_context_slot,"account_owner":a[5].address,"refund_authority":a[2].address,"payer":a[0].address,"rent_lamports":rent}),
                        ));
                    }
                }
            }
            swap::SYSTEM_PROGRAM => {
                if data.len() != 12
                    || u32::from_le_bytes(
                        data[..4]
                            .try_into()
                            .map_err(|_| unsafe_instruction_at("native transfer envelope"))?,
                    ) != 2
                    || ix.accounts.len() != 2
                    || ix.accounts[0].address != wallet.address()
                    || !ix.accounts[0].signer
                {
                    return Err(unsafe_instruction_at("native transfer envelope"));
                }
                let amount = u64::from_le_bytes(
                    data[4..12]
                        .try_into()
                        .map_err(|_| unsafe_instruction_at("native transfer envelope"))?,
                );
                if expected_native_wrap_lamports != Some(amount)
                    || ix.accounts[1].address != native_ata
                {
                    return Err(unsafe_instruction_at("native wrap amount/account binding"));
                }
                effects.wrapped_native_lamports = effects
                    .wrapped_native_lamports
                    .checked_add(amount)
                    .ok_or_else(|| unsafe_instruction_at("native wrap amount total"))?;
            }
            fin_solana::token::TOKEN_PROGRAM_ID | fin_solana::token::TOKEN_2022_PROGRAM_ID => {
                if data == [17] {
                    if ix.accounts.first().map(|a| a.address.as_str()) != Some(native_ata.as_str())
                    {
                        return Err(unsafe_instruction_at("native sync account binding"));
                    }
                } else if data == [9] && ix.accounts.len() == 3 {
                    let account = ix.accounts[0].address.as_str();
                    if account != native_ata
                        || ix.accounts[1].address != wallet.address()
                        || ix.accounts[2].address != wallet.address()
                        || !ix.accounts[2].signer
                    {
                        return Err(unsafe_instruction_at("native close account graph"));
                    }
                    let info = wallet
                        .runtime()
                        .rpc()
                        .request(
                            "getAccountInfo",
                            json!([native_ata,{"encoding":"base64","commitment":"finalized"}]),
                        )
                        .await?;
                    let (account_observed_at, account_context_slot) =
                        finalized_account_observation(&info)?;
                    let info_value = info
                        .get("value")
                        .ok_or_else(|| unsafe_instruction_at("native close account response"))?;
                    let mut closed_rent = 0u64;
                    if !info_value.is_null() {
                        let value = info_value;
                        closed_rent = value
                            .get("lamports")
                            .and_then(Value::as_u64)
                            .ok_or_else(|| unsafe_instruction_at("native close account state"))?;
                        let bytes = fin_solana::token::decode_rpc_account_data(
                            value.get("data").ok_or_else(|| {
                                unsafe_instruction_at("native close account state")
                            })?,
                        )
                        .map_err(|error| {
                            validation_stage_if_unreviewed(error, "native close account state")
                        })?;
                        let parsed = fin_solana::token::parse_token_account(
                            value.get("owner").and_then(Value::as_str).ok_or_else(|| {
                                unsafe_instruction_at("native close account state")
                            })?,
                            &native_ata,
                            &bytes,
                        )
                        .map_err(|error| {
                            validation_stage_if_unreviewed(error, "native close account state")
                        })?;
                        if parsed.token_owner != wallet.address()
                            || parsed.mint != swap::WRAPPED_SOL_MINT
                            || parsed.amount_atoms != 0
                        {
                            return Err(unsafe_instruction_at("native close account state"));
                        }
                        effects.baseline_anchors.push(refundable_rent_anchor(
                            wallet.address(),
                            &native_ata,
                            closed_rent,
                            account_observed_at,
                            json!({"observation":"getAccountInfo","commitment":"finalized","account_existed":true,"account_absence_proven":false,"context_slot":account_context_slot,"account_owner":value.get("owner"),"refund_authority":wallet.address(),"token_owner":parsed.token_owner,"mint":parsed.mint,"amount_atoms":parsed.amount_atoms.to_string(),"lamports":closed_rent}),
                        ));
                    } else if let Some(created) = effects
                        .funding_accounts
                        .iter()
                        .find(|funding| funding.address == native_ata)
                    {
                        closed_rent = created.lamports;
                    }
                    if closed_rent > 0 {
                        effects
                            .closed_wrapped_native_refunds
                            .push(AccountFundingRequirement {
                                address: native_ata.clone(),
                                lamports: closed_rent,
                                account_owner: fin_solana::token::TOKEN_PROGRAM_ID.into(),
                                refund_authority: wallet.address().into(),
                                payer: "pre_existing_or_same_transaction".into(),
                            });
                    }
                    effects
                        .closed_wrapped_native_accounts
                        .push(native_ata.clone());
                } else {
                    return Err(unsafe_instruction_at("unsupported token setup instruction"));
                }
            }
            MEMO_PROGRAM if data.len() > 566 => return Err(unsafe_instruction()),
            MEMO_PROGRAM => {}
            _ => { /* The protocol adapter owns non-auxiliary instructions. */ }
        }
    }
    if effects.account_funding_lamports > maximum_account_funding_lamports {
        return Err(FinError::new(
            "JUPITER_ACCOUNT_FUNDING_LIMIT_EXCEEDED",
            "prepare",
            "Token-account rent exceeds the approved SOL funding cap.",
            "Review the exact rent requirement and approve a bounded funding amount.",
        ));
    }
    if expected_native_wrap_lamports.is_some()
        && effects.wrapped_native_lamports != expected_native_wrap_lamports.unwrap_or_default()
    {
        return Err(unsafe_instruction_at("native wrap amount total"));
    }
    let ephemeral_closed_accounts = effects
        .closed_wrapped_native_accounts
        .iter()
        .filter(|closed| {
            effects
                .funding_accounts
                .iter()
                .any(|funding| funding.address == ***closed)
        })
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    effects.baseline_anchors.retain(|anchor| {
        let account = anchor
            .evidence
            .get("account")
            .and_then(Value::as_str)
            .unwrap_or_default();
        !ephemeral_closed_accounts.contains(account)
    });
    Ok(effects)
}

fn associated_address(owner: &str, token_program: &str, mint: &str) -> Result<String> {
    let owner = solana_pubkey3::Pubkey::try_from(owner).map_err(|_| unsafe_instruction())?;
    let token =
        solana_pubkey3::Pubkey::try_from(token_program).map_err(|_| unsafe_instruction())?;
    let mint = solana_pubkey3::Pubkey::try_from(mint).map_err(|_| unsafe_instruction())?;
    let program = solana_pubkey3::Pubkey::try_from(swap::ASSOCIATED_TOKEN_PROGRAM)
        .map_err(|_| unsafe_instruction())?;
    Ok(solana_pubkey3::Pubkey::find_program_address(
        &[owner.as_ref(), token.as_ref(), mint.as_ref()],
        &program,
    )
    .0
    .to_string())
}

fn validate_associated_create_accounts(
    accounts: &[fin_solana::transaction::NormalizedAccountMeta],
    payer: &str,
    wallet: &str,
    allowed_mints: &[String],
    allowed_authorities: &[String],
) -> Result<()> {
    if accounts.len() != 6 {
        return Err(unsafe_instruction_at("associated-token account count"));
    }
    let [payer_meta, ata, authority, mint, system, token_program] = accounts else {
        return Err(unsafe_instruction_at("associated-token account count"));
    };
    // JupiterZ may use a remote transaction fee payer while the enrolled
    // wallet independently funds an associated-token account it creates.
    // Account funding is attributed to this actual payer below and wallet
    // funding remains subject to the approved cap.
    if payer_meta.address != payer && payer_meta.address != wallet {
        return Err(unsafe_instruction_at("associated-token payer identity"));
    }
    if !payer_meta.signer || !payer_meta.writable {
        return Err(unsafe_instruction_at("associated-token payer privileges"));
    }
    if ata.signer || !ata.writable {
        return Err(unsafe_instruction_at("associated-token address privileges"));
    }
    if !allowed_authorities
        .iter()
        .any(|allowed| allowed == &authority.address)
    {
        return Err(unsafe_instruction_at(
            "associated-token authority membership",
        ));
    }
    if authority.signer && authority.address != wallet {
        return Err(unsafe_instruction_at("associated-token authority signer"));
    }
    if mint.signer {
        return Err(unsafe_instruction_at("associated-token mint privileges"));
    }
    if system.address != swap::SYSTEM_PROGRAM || system.signer || system.writable {
        return Err(unsafe_instruction_at("associated-token system program"));
    }
    if !allowed_mints.iter().any(|allowed| allowed == &mint.address) {
        return Err(unsafe_instruction_at("associated-token mint binding"));
    }
    if token_program.address != fin_solana::token::TOKEN_PROGRAM_ID
        && token_program.address != fin_solana::token::TOKEN_2022_PROGRAM_ID
    {
        return Err(unsafe_instruction_at(
            "associated-token token program identity",
        ));
    }
    if token_program.signer || token_program.writable {
        return Err(unsafe_instruction_at(
            "associated-token token program privileges",
        ));
    }
    if associated_address(&authority.address, &token_program.address, &mint.address)
        .map_err(|error| validation_stage_if_unreviewed(error, "associated-token PDA derivation"))?
        != ata.address
    {
        return Err(unsafe_instruction_at("associated-token PDA binding"));
    }
    Ok(())
}

fn validate_system_instruction(
    intent: &SwapIntent,
    wallet: &str,
    frozen: &FrozenTransaction,
    ix: &NormalizedInstruction,
    data: &[u8],
) -> Result<u64> {
    if data.len() < 4 {
        return Err(unsafe_instruction());
    }
    let tag = u32::from_le_bytes(data[..4].try_into().map_err(|_| unsafe_instruction())?);
    match tag {
        2 if data.len() == 12
            && ix.accounts.len() == 2
            && ix.accounts[0].address == wallet
            && ix.accounts[0].signer =>
        {
            let amount =
                u64::from_le_bytes(data[4..12].try_into().map_err(|_| unsafe_instruction())?);
            if intent.input_mint == "native"
                && amount
                    == intent
                        .amount_atoms
                        .parse::<u64>()
                        .map_err(|_| unsafe_instruction())?
            {
                Ok(0)
            } else {
                Err(unsafe_instruction())
            }
        }
        0 if data.len() == 52
            && ix.accounts.len() == 2
            && ix.accounts[0].address == frozen.fee_payer()
            && ix.accounts[0].signer =>
        {
            let lamports =
                u64::from_le_bytes(data[4..12].try_into().map_err(|_| unsafe_instruction())?);
            let space =
                u64::from_le_bytes(data[12..20].try_into().map_err(|_| unsafe_instruction())?);
            let owner = bs58::encode(&data[20..52]).into_string();
            if space > 4096
                || owner != fin_solana::token::TOKEN_PROGRAM_ID
                    && owner != fin_solana::token::TOKEN_2022_PROGRAM_ID
            {
                return Err(unsafe_instruction());
            }
            Ok(lamports)
        }
        _ => Err(unsafe_instruction()),
    }
}

fn validate_token_setup(wallet: &str, ix: &NormalizedInstruction, data: &[u8]) -> Result<()> {
    if data.len() == 1 && data[0] == 17 {
        return Ok(());
    } // SyncNative
    if data.len() == 1
        && data[0] == 9
        && ix.accounts.len() == 3
        && ix.accounts[1].address == wallet
        && ix.accounts[2].address == wallet
        && ix.accounts[2].signer
    {
        return Ok(());
    }
    Err(unsafe_instruction())
}

fn validate_order_engine_account_privileges(
    accounts: &[NormalizedAccountMeta],
    input_mint: &str,
    output_mint: &str,
) -> Result<()> {
    if accounts.len() < 7 {
        return Err(unsafe_instruction_at("RFQ account count"));
    }
    for (index, account) in accounts[2..6].iter().enumerate() {
        let native_endpoint = if index < 2 {
            input_mint == "native"
        } else {
            output_mint == "native"
        };
        if account.address == ORDER_ENGINE_PROGRAM {
            // Anchor's optional-account sentinel is the invoked program ID.
            // An optional account may be absent only for an endpoint explicitly
            // requested as native; explicit WSOL requests remain SPL swaps.
            // The sentinel itself is readonly/nonsigner.
            if !native_endpoint || account.signer || account.writable {
                return Err(unsafe_instruction_at(
                    "RFQ native optional account privileges",
                ));
            }
        } else if !account.writable || account.signer {
            return Err(unsafe_instruction_at("RFQ token account privileges"));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DecodedOrderEngineFill {
    input_amount: u64,
    output_amount: u64,
    expire_at: i64,
    fee_bps: Option<u16>,
    flags: u8,
    integrator_fee_bps: u16,
}

fn decode_order_engine_fill_data(data: &[u8]) -> Result<DecodedOrderEngineFill> {
    const DISC: [u8; 8] = [168, 96, 183, 163, 92, 10, 40, 160];
    if data.len() != 32 && data.len() != 37 {
        return Err(unsafe_instruction_at_with_details(
            "RFQ instruction data length",
            json!({"data_len_bytes": data.len().min(4096), "data_len_truncated": data.len() > 4096}),
        ));
    }
    if data[..8] != DISC {
        return Err(unsafe_instruction_at("RFQ instruction discriminator"));
    }
    let input_amount = u64::from_le_bytes(
        data[8..16]
            .try_into()
            .map_err(|_| unsafe_instruction_at("RFQ input amount encoding"))?,
    );
    let output_amount = u64::from_le_bytes(
        data[16..24]
            .try_into()
            .map_err(|_| unsafe_instruction_at("RFQ output amount encoding"))?,
    );
    let expire_at = i64::from_le_bytes(
        data[24..32]
            .try_into()
            .map_err(|_| unsafe_instruction_at("RFQ expiry encoding"))?,
    );
    let (fee_bps, flags, integrator_fee_bps) = if data.len() == 37 {
        (
            Some(u16::from_le_bytes([data[32], data[33]])),
            data[34],
            u16::from_le_bytes([data[35], data[36]]),
        )
    } else {
        // The published on-chain IDL's legacy Fill args end after expire_at.
        // Metadata-free fills are only valid when no fee was quoted.
        (None, 0, 0)
    };
    Ok(DecodedOrderEngineFill {
        input_amount,
        output_amount,
        expire_at,
        fee_bps,
        flags,
        integrator_fee_bps,
    })
}

fn validate_order_engine_fill_metadata(
    fill: DecodedOrderEngineFill,
    expected_fee_bps: u16,
) -> Result<()> {
    if fill.fee_bps.unwrap_or_default() != expected_fee_bps {
        return Err(unsafe_instruction_at("RFQ fee binding"));
    }
    // Bit 0 would mark exact-output; bits 2..7 are reserved. Bit 1 only selects
    // the side for an integrator fee, so either value is harmless when that
    // separately encoded fee is zero.
    if fill.flags & 0b0000_0001 != 0 || fill.flags & 0b1111_1100 != 0 {
        return Err(unsafe_instruction_at("RFQ mode or fee permission flags"));
    }
    if fill.integrator_fee_bps != 0 {
        return Err(unsafe_instruction_at("RFQ integrator fee binding"));
    }
    Ok(())
}

async fn validate_order_engine_fill(
    intent: &SwapIntent,
    candidate: &Candidate,
    wallet: &SolanaWalletAdapter,
    ix: &NormalizedInstruction,
    data: &[u8],
    auxiliary: &AuxiliaryEffects,
) -> Result<()> {
    const ORDER_ENGINE_PROGRAM: &str = "61DFfeTKM7trxYcPQCM78bJ794ddZprZpAwAnLiwTpYH";
    if ix.program_id != ORDER_ENGINE_PROGRAM {
        return Err(unsafe_instruction_at("RFQ program identity"));
    }
    let fill = decode_order_engine_fill_data(data)?;
    if ix.accounts.len() != 11 {
        return Err(unsafe_instruction_at_with_details(
            "RFQ account count",
            json!({"account_count": ix.accounts.len().min(256), "account_count_truncated": ix.accounts.len() > 256}),
        ));
    }
    if ix.accounts[0].address != wallet.address() {
        return Err(unsafe_instruction_at("RFQ taker identity"));
    }
    for index in [0usize, 1] {
        let account = &ix.accounts[index];
        if !account.signer || !account.writable {
            return Err(unsafe_instruction_at_with_details(
                "RFQ taker and maker privileges",
                json!({"account_index": index, "signer": account.signer, "writable": account.writable}),
            ));
        }
    }
    if ix.accounts[10].address != swap::SYSTEM_PROGRAM {
        return Err(unsafe_instruction_at("RFQ system program identity"));
    }
    if ix.accounts[10].signer || ix.accounts[10].writable {
        return Err(unsafe_instruction_at_with_details(
            "RFQ system program privileges",
            json!({"account_index": 10, "signer": ix.accounts[10].signer, "writable": ix.accounts[10].writable}),
        ));
    }
    for index in 6..10 {
        let account = &ix.accounts[index];
        if account.signer || account.writable {
            return Err(unsafe_instruction_at_with_details(
                "RFQ fixed account privileges",
                json!({"account_index": index, "signer": account.signer, "writable": account.writable}),
            ));
        }
    }
    validate_order_engine_account_privileges(
        &ix.accounts,
        &intent.input_mint,
        &intent.output_mint,
    )?;
    validate_order_engine_fill_metadata(fill, candidate.platform_fee_bps)?;
    let (_, minimum) = swap::validate_intent(intent)?;
    if fill.input_amount
        != intent
            .amount_atoms
            .parse::<u64>()
            .map_err(|_| unsafe_instruction_at("RFQ input amount binding"))?
    {
        return Err(unsafe_instruction_at("RFQ input amount binding"));
    }
    if fill.output_amount < minimum {
        return Err(unsafe_instruction_at("RFQ minimum output binding"));
    }
    if fill.expire_at <= Utc::now().timestamp()
        || candidate
            .provider_expire_at
            .as_deref()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.timestamp())
            != Some(fill.expire_at)
    {
        return Err(unsafe_instruction_at("RFQ expiry binding"));
    }
    let input_mint = candidate_mint(&intent.input_mint)?;
    let output_mint = candidate_mint(&intent.output_mint)?;
    if candidate
        .platform_fee_mint
        .as_deref()
        .is_some_and(|fee_mint| fee_mint != input_mint && fee_mint != output_mint)
    {
        return Err(unsafe_instruction_at("RFQ fee mint binding"));
    }
    if ix.accounts[6].address != input_mint || ix.accounts[8].address != output_mint {
        return Err(unsafe_instruction_at("RFQ mint binding"));
    }
    let input_program = finalized_mint_program(wallet, &input_mint).await?;
    let output_program = finalized_mint_program(wallet, &output_mint).await?;
    if ix.accounts[7].address != input_program || ix.accounts[9].address != output_program {
        return Err(unsafe_instruction_at("RFQ token program binding"));
    }
    for (index, mint, owner, may_be_new) in [
        (2usize, input_mint.as_str(), wallet.address(), true),
        (
            3usize,
            input_mint.as_str(),
            ix.accounts[1].address.as_str(),
            false,
        ),
        (4usize, output_mint.as_str(), wallet.address(), true),
        (
            5usize,
            output_mint.as_str(),
            ix.accounts[1].address.as_str(),
            false,
        ),
    ] {
        let address = ix.accounts[index].address.as_str();
        if address == ORDER_ENGINE_PROGRAM {
            // Anchor optional-account sentinel is only accepted for native
            // endpoints; all SPL endpoints require an exact token account.
            if mint != swap::WRAPPED_SOL_MINT {
                return Err(unsafe_instruction());
            }
            continue;
        }
        if may_be_new && owner == wallet.address() {
            let program = validate_token_account(
                wallet,
                address,
                mint,
                index == 2,
                mint == swap::WRAPPED_SOL_MINT,
                auxiliary,
            )
            .await
            .map_err(|error| validation_stage_if_unreviewed(error, "RFQ wallet token account"))?;
            let expected_program = if index == 2 {
                input_program.as_str()
            } else {
                output_program.as_str()
            };
            if program != expected_program {
                return Err(unsafe_instruction_at("RFQ wallet token program"));
            }
        } else {
            validate_owned_token_account(
                wallet,
                address,
                mint,
                owner,
                if index == 3 {
                    &input_program
                } else {
                    &output_program
                },
            )
            .await
            .map_err(|error| validation_stage_if_unreviewed(error, "RFQ maker token account"))?;
        }
    }
    Ok(())
}

/// Check DFlow's fixed Anchor account prefix and resolve every writable
/// wallet token account in the dynamic route metas against finalized RPC.
/// The remaining AMM accounts are intentionally dynamic; the pinned DFlow
/// instruction is the authority that interprets those route accounts.
async fn validate_dflow_swap_accounts(
    wallet: &SolanaWalletAdapter,
    ix: &NormalizedInstruction,
    decoded: &crate::dflow::DecodedDflowSwap,
    intent: &SwapIntent,
    auxiliary: &AuxiliaryEffects,
) -> Result<()> {
    let remaining_accounts_start = validate_dflow_header(wallet.address(), ix, decoded)?;
    let token_program = &ix.accounts[0].address;
    let input_mint = candidate_mint(&intent.input_mint)?;
    let output_mint = candidate_mint(&intent.output_mint)?;
    let mut allowed_wallet_input_accounts = BTreeMap::<String, String>::new();
    let mut allowed_wallet_output_accounts = BTreeMap::<String, String>::new();

    if decoded.output_native {
        if intent.output_mint != "native"
            || ix.accounts[4].address != wallet.address()
            || !ix.accounts[4].writable
        {
            return Err(unsafe_instruction());
        }
    } else if let Some(destination_index) = decoded.destination_index {
        if intent.output_mint == "native"
            || destination_index != 4
            || ix.accounts[4].address == wallet.address()
            || !ix.accounts[4].writable
            || !dflow_destination_authority_is_wallet(&ix.accounts[5], wallet.address())
            || ix.accounts[6].address != output_mint
        {
            return Err(unsafe_instruction());
        }
        let program = validate_token_account(
            wallet,
            &ix.accounts[4].address,
            &output_mint,
            false,
            false,
            auxiliary,
        )
        .await?;
        if program != *token_program {
            return Err(unsafe_instruction());
        }
        allowed_wallet_output_accounts.insert(ix.accounts[4].address.clone(), output_mint.clone());
    } else if intent.output_mint == "native" {
        return Err(unsafe_instruction());
    }

    for meta in ix.accounts.iter().skip(remaining_accounts_start) {
        if !meta.writable || meta.address == wallet.address() {
            continue;
        }
        let response = wallet
            .runtime()
            .rpc()
            .request(
                "getAccountInfo",
                json!([meta.address,{"encoding":"base64","commitment":"finalized"}]),
            )
            .await?;
        let Some(account) = response.get("value").filter(|value| !value.is_null()) else {
            if let Some(created) = auxiliary
                .created_token_accounts
                .iter()
                .find(|created| created.address == meta.address)
            {
                if associated_address(wallet.address(), &created.token_program, &created.mint)?
                    != meta.address
                    || (created.mint != input_mint && created.mint != output_mint)
                {
                    return Err(unsafe_instruction());
                }
                let target = if created.mint == input_mint {
                    &mut allowed_wallet_input_accounts
                } else {
                    &mut allowed_wallet_output_accounts
                };
                target.insert(meta.address.clone(), created.mint.clone());
                continue;
            }
            if intent.input_mint == "native"
                && meta.address
                    == associated_address(
                        wallet.address(),
                        fin_solana::token::TOKEN_PROGRAM_ID,
                        swap::WRAPPED_SOL_MINT,
                    )?
            {
                allowed_wallet_input_accounts.insert(meta.address.clone(), input_mint.clone());
                continue;
            }
            // Protocol route accounts may be created through CPI, but a
            // wallet-owned missing account must be established by the exact
            // setup instruction above before it can be treated as a wallet
            // input or output.
            continue;
        };
        let owner_program = account
            .get("owner")
            .and_then(Value::as_str)
            .ok_or_else(unsafe_instruction)?;
        if owner_program != fin_solana::token::TOKEN_PROGRAM_ID
            && owner_program != fin_solana::token::TOKEN_2022_PROGRAM_ID
        {
            continue;
        }
        let bytes = fin_solana::token::decode_rpc_account_data(
            account.get("data").ok_or_else(unsafe_instruction)?,
        )?;
        let parsed = fin_solana::token::parse_token_account(owner_program, &meta.address, &bytes)?;
        if parsed.token_owner != wallet.address() {
            continue;
        }
        if parsed.mint != input_mint && parsed.mint != output_mint {
            return Err(unsafe_instruction());
        }
        if parsed.state != "initialized" {
            return Err(unsafe_instruction());
        }
        validate_token_credit_extensions(&parsed)?;
        let target = if parsed.mint == input_mint {
            &mut allowed_wallet_input_accounts
        } else {
            &mut allowed_wallet_output_accounts
        };
        target.insert(meta.address.clone(), parsed.mint);
    }
    if intent.output_mint != "native" && allowed_wallet_output_accounts.is_empty() {
        return Err(unsafe_instruction());
    }
    if allowed_wallet_input_accounts.is_empty() {
        return Err(unsafe_instruction());
    }
    if decoded.platform_fee_bps > 0 {
        let fee_account = ix.accounts.last().ok_or_else(unsafe_instruction)?;
        if !fee_account.writable
            || fee_account.signer
            || fee_account.address == wallet.address()
            || allowed_wallet_input_accounts.contains_key(&fee_account.address)
            || allowed_wallet_output_accounts.contains_key(&fee_account.address)
        {
            return Err(unsafe_instruction());
        }
        validate_non_wallet_fee_account(wallet, &fee_account.address, &output_mint).await?;
    }
    Ok(())
}

fn dflow_destination_authority_is_wallet(meta: &NormalizedAccountMeta, wallet: &str) -> bool {
    // Message privileges are globally unioned. This account aliases the
    // user_token_authority signer at index 3, so signer=true here is valid
    // only for the same reviewed wallet address.
    meta.address == wallet
}

fn validate_dflow_header(
    wallet_address: &str,
    ix: &NormalizedInstruction,
    decoded: &crate::dflow::DecodedDflowSwap,
) -> Result<usize> {
    if ix.program_id != DFLOW_PROGRAM || ix.accounts.len() < 6 {
        return Err(unsafe_instruction());
    }
    let token_program = &ix.accounts[0].address;
    if token_program != fin_solana::token::TOKEN_PROGRAM_ID
        && token_program != fin_solana::token::TOKEN_2022_PROGRAM_ID
    {
        return Err(unsafe_instruction());
    }
    if ix.accounts[1].address != swap::ASSOCIATED_TOKEN_PROGRAM
        || ix.accounts[2].address != swap::SYSTEM_PROGRAM
        || ix.accounts[3].address != wallet_address
        || !ix.accounts[3].signer
        || ix.accounts[1].signer
        || ix.accounts[2].signer
        || ix
            .accounts
            .iter()
            .any(|meta| meta.signer && meta.address != wallet_address)
    {
        return Err(unsafe_instruction());
    }
    let program = Pubkey::try_from(DFLOW_PROGRAM).map_err(|_| unsafe_instruction())?;
    let event_authority = Pubkey::find_program_address(&[b"__event_authority"], &program)
        .0
        .to_string();
    let (event_index, program_index) = if decoded.output_native {
        (5usize, 6usize)
    } else if decoded.destination_index.is_some() {
        (7usize, 8usize)
    } else {
        (4usize, 5usize)
    };
    if ix.accounts.len() <= program_index
        || ix.accounts[event_index].address != event_authority
        || ix.accounts[event_index].signer
        || ix.accounts[event_index].writable
        || ix.accounts[program_index].address != DFLOW_PROGRAM
        || ix.accounts[program_index].signer
        || ix.accounts[program_index].writable
    {
        return Err(unsafe_instruction());
    }
    Ok(program_index + 1)
}

fn decode_okx_swap(
    data: &[u8],
    intent: &SwapIntent,
    candidate: &Candidate,
) -> Result<DecodedOkxSwap> {
    const SWAP_DISC: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];
    if data.len() < 8 + 8 + 8 + 8 + 2 + 4 || data[..8] != SWAP_DISC {
        return Err(unsafe_instruction());
    }
    let mut cursor = 8;
    let read_u64 = |cursor: &mut usize| -> Result<u64> {
        let end = cursor.checked_add(8).ok_or_else(unsafe_instruction)?;
        let bytes: [u8; 8] = data
            .get(*cursor..end)
            .ok_or_else(unsafe_instruction)?
            .try_into()
            .map_err(|_| unsafe_instruction())?;
        *cursor = end;
        Ok(u64::from_le_bytes(bytes))
    };
    let _order_id = read_u64(&mut cursor)?;
    let amount_in = read_u64(&mut cursor)?;
    let expected_out = read_u64(&mut cursor)?;
    let slippage_end = cursor.checked_add(2).ok_or_else(unsafe_instruction)?;
    let slippage_bps = u16::from_le_bytes(
        data.get(cursor..slippage_end)
            .ok_or_else(unsafe_instruction)?
            .try_into()
            .map_err(|_| unsafe_instruction())?,
    );
    cursor = slippage_end;
    let count_end = cursor.checked_add(4).ok_or_else(unsafe_instruction)?;
    let route_count = u32::from_le_bytes(
        data.get(cursor..count_end)
            .ok_or_else(unsafe_instruction)?
            .try_into()
            .map_err(|_| unsafe_instruction())?,
    ) as usize;
    cursor = count_end;
    if route_count == 0 || route_count > 64 || data.len() != cursor + route_count * 4 {
        return Err(unsafe_instruction());
    }
    let dex_count = 143u8; // Pinned Dex enum variant count in resources/okx-dex-router-mainnet-idl.json.
    let mut total_weight = 0u32;
    for route in data[cursor..].chunks_exact(4) {
        if route[0] >= dex_count {
            return Err(unsafe_instruction());
        }
        total_weight = total_weight
            .checked_add(u32::from(u16::from_le_bytes([route[1], route[2]])))
            .ok_or_else(unsafe_instruction)?;
    }
    let minimum_after_slippage = u128::from(expected_out)
        .checked_mul(u128::from(10_000u16.saturating_sub(slippage_bps)))
        .ok_or_else(unsafe_instruction)?
        / 10_000;
    if total_weight != 10_000
        || amount_in
            != intent
                .amount_atoms
                .parse::<u64>()
                .map_err(|_| unsafe_instruction())?
        || expected_out != candidate.expected_output_atoms
        || slippage_bps != candidate.actual_slippage_bps
        || slippage_bps > intent.maximum_slippage_bps
        || candidate.platform_fee_bps != 0
        || candidate.platform_fee_mint.is_some()
        || minimum_after_slippage < u128::from(swap::validate_intent(intent)?.1)
    {
        return Err(unsafe_instruction());
    }
    Ok(DecodedOkxSwap {
        amount_in,
        expected_out,
        slippage_bps,
        route_count,
    })
}

async fn validate_okx_swap_accounts(
    wallet: &SolanaWalletAdapter,
    ix: &NormalizedInstruction,
    decoded: &DecodedOkxSwap,
    intent: &SwapIntent,
    auxiliary: &AuxiliaryEffects,
) -> Result<()> {
    if ix.program_id != OKX_ROUTER_PROGRAM
        || ix.accounts.len() < 7
        || ix.accounts[0].address != wallet.address()
        || !ix.accounts[0].signer
        || ix.accounts[1].signer
        || !ix.accounts[1].writable
        || ix.accounts[2].signer
        || !ix.accounts[2].writable
        || ix.accounts[3].address != candidate_mint(&intent.input_mint)?
        || ix.accounts[4].address != candidate_mint(&intent.output_mint)?
        || ix.accounts[3].signer
        || ix.accounts[3].writable
        || ix.accounts[4].signer
        || ix.accounts[4].writable
        || ix.accounts[5].signer
        || ix.accounts[5].writable
        || ix.accounts[6].address != OKX_ROUTER_PROGRAM
        || ix.accounts[6].signer
        || ix.accounts[6].writable
        || ix.accounts[7..].iter().any(|meta| meta.signer)
    {
        return Err(unsafe_instruction());
    }
    let program = Pubkey::try_from(OKX_ROUTER_PROGRAM).map_err(|_| unsafe_instruction())?;
    let expected_event = Pubkey::find_program_address(&[b"__event_authority"], &program)
        .0
        .to_string();
    if ix.accounts[5].address != expected_event {
        return Err(unsafe_instruction());
    }
    let input_mint = candidate_mint(&intent.input_mint)?;
    let output_mint = candidate_mint(&intent.output_mint)?;
    let input_program = finalized_mint_program(wallet, &input_mint).await?;
    let output_program = finalized_mint_program(wallet, &output_mint).await?;
    if validate_token_account(
        wallet,
        &ix.accounts[1].address,
        &input_mint,
        true,
        intent.input_mint == "native",
        auxiliary,
    )
    .await?
        != input_program
        || validate_token_account(
            wallet,
            &ix.accounts[2].address,
            &output_mint,
            false,
            false,
            auxiliary,
        )
        .await?
            != output_program
        || decoded.amount_in
            != intent
                .amount_atoms
                .parse::<u64>()
                .map_err(|_| unsafe_instruction())?
        || decoded.expected_out < swap::validate_intent(intent)?.1
        || decoded.route_count == 0
        || decoded.slippage_bps > intent.maximum_slippage_bps
    {
        return Err(unsafe_instruction());
    }
    Ok(())
}

async fn validate_route_token_accounts(
    wallet: &SolanaWalletAdapter,
    ix: &NormalizedInstruction,
    decoded: &DecodedSwapInstruction,
    intent: &SwapIntent,
    auxiliary: &AuxiliaryEffects,
    fee_mint: Option<&str>,
) -> Result<()> {
    let v2 = matches!(
        decoded.variant,
        swap::SwapInstructionVariant::ExactInV2
            | swap::SwapInstructionVariant::SharedExactInV2
            | swap::SwapInstructionVariant::ExactOutV2
            | swap::SwapInstructionVariant::SharedExactOutV2
    );
    if v2 {
        return validate_route_token_accounts_v2(wallet, ix, decoded, intent, auxiliary, fee_mint)
            .await;
    }
    let shared = matches!(
        decoded.variant,
        swap::SwapInstructionVariant::SharedExactIn
            | swap::SwapInstructionVariant::SharedExactOut
            | swap::SwapInstructionVariant::SharedTokenLedgerExactIn
    );
    let token_ledger = matches!(
        decoded.variant,
        swap::SwapInstructionVariant::TokenLedgerExactIn
            | swap::SwapInstructionVariant::SharedTokenLedgerExactIn
    );
    let exact_out = matches!(
        decoded.variant,
        swap::SwapInstructionVariant::ExactOut | swap::SwapInstructionVariant::SharedExactOut
    );
    let (source_index, destination_index) = if shared { (3, 6) } else { (2, 3) };
    let (
        source_mint_index,
        destination_mint_index,
        fee_index,
        token_2022_index,
        event_index,
        program_index,
    ) = if shared {
        (
            Some(7),
            8,
            9,
            Some(10),
            if token_ledger { 12 } else { 11 },
            if token_ledger { 13 } else { 12 },
        )
    } else if exact_out {
        (Some(5), 6, 7, Some(8), 9, 10)
    } else if token_ledger {
        (None, 5, 6, None, 8, 9)
    } else {
        (None, 5, 6, None, 7, 8)
    };
    if ix.accounts.len() <= program_index {
        return Err(unsafe_instruction());
    }
    let account = |index: usize| ix.accounts.get(index).ok_or_else(unsafe_instruction);
    let sentinel = swap::JUPITER_V6_PROGRAM;
    let wallet_index = if shared { 2 } else { 1 };
    if (shared && account(0)?.address != fin_solana::token::TOKEN_PROGRAM_ID)
        || (!shared
            && account(0)?.address != fin_solana::token::TOKEN_PROGRAM_ID
            && account(0)?.address != fin_solana::token::TOKEN_2022_PROGRAM_ID)
    {
        return Err(unsafe_instruction());
    }
    if account(wallet_index)?.address != wallet.address() || !account(wallet_index)?.signer {
        return Err(unsafe_instruction());
    }
    if !account(source_index)?.writable || !account(destination_index)?.writable {
        return Err(unsafe_instruction());
    }
    if has_foreign_route_signer(&ix.accounts, wallet.address()) {
        return Err(unsafe_instruction());
    }
    let expected_event = "D8cy77BBepLMngZx6ZukaTff5hCt1HrWyKk3Hnd9oitf";
    if account(event_index)?.address != expected_event
        || account(event_index)?.signer
        || account(event_index)?.writable
        || account(program_index)?.address != swap::JUPITER_V6_PROGRAM
        || account(program_index)?.signer
        || account(program_index)?.writable
    {
        return Err(unsafe_instruction());
    }
    let output_mint = candidate_mint(&intent.output_mint)?;
    if account(destination_mint_index)?.address != output_mint {
        return Err(unsafe_instruction());
    }
    if let Some(index) = source_mint_index
        && account(index)?.address != candidate_mint(&intent.input_mint)?
    {
        return Err(unsafe_instruction());
    }
    if shared {
        let authority_id = decoded.authority_id.ok_or_else(unsafe_instruction)?;
        if authority_id >= 16 {
            return Err(unsafe_instruction());
        }
        let program =
            Pubkey::try_from(swap::JUPITER_V6_PROGRAM).map_err(|_| unsafe_instruction())?;
        let authority =
            Pubkey::find_program_address(&[&b"authority"[..], &[authority_id]], &program)
                .0
                .to_string();
        if account(1)?.address != authority || account(1)?.signer || account(1)?.writable {
            return Err(unsafe_instruction());
        }
        if !account(4)?.writable || !account(5)?.writable {
            return Err(unsafe_instruction());
        }
    } else if decoded.authority_id.is_some() {
        return Err(unsafe_instruction());
    }
    if token_ledger {
        let ledger_index = if shared { 11 } else { 7 };
        if account(ledger_index)?.signer || account(ledger_index)?.writable {
            return Err(unsafe_instruction());
        }
    }
    if !shared && account(4)?.address != sentinel {
        return Err(unsafe_instruction());
    }
    let fee_address = account(fee_index)?.address.as_str();
    if decoded.platform_fee_bps == 0 {
        if fee_address != sentinel || fee_mint.is_some() {
            return Err(unsafe_instruction());
        }
    } else if fee_address == sentinel || !account(fee_index)?.writable || fee_mint.is_none() {
        return Err(unsafe_instruction());
    }
    if let Some(index) = token_2022_index {
        let token_2022 = account(index)?.address.as_str();
        if token_2022 != sentinel && token_2022 != fin_solana::token::TOKEN_2022_PROGRAM_ID {
            return Err(unsafe_instruction());
        }
    }
    let source = account(source_index)?.address.as_str();
    let destination = account(destination_index)?.address.as_str();
    let source_program = validate_token_account(
        wallet,
        source,
        &candidate_mint(&intent.input_mint)?,
        true,
        intent.input_mint == "native",
        auxiliary,
    )
    .await?;
    let destination_program =
        validate_token_account(wallet, destination, &output_mint, false, false, auxiliary).await?;
    let input_mint_program =
        finalized_mint_program(wallet, &candidate_mint(&intent.input_mint)?).await?;
    let output_mint_program = finalized_mint_program(wallet, &output_mint).await?;
    if source_program != input_mint_program
        || (shared && account(0)?.address != fin_solana::token::TOKEN_PROGRAM_ID)
        || (!shared && account(0)?.address != source_program)
    {
        return Err(unsafe_instruction());
    }
    if destination_program != output_mint_program {
        return Err(unsafe_instruction());
    }
    if shared {
        let Some(index) = token_2022_index else {
            return Err(unsafe_instruction());
        };
        if account(0)?.address != fin_solana::token::TOKEN_PROGRAM_ID
            || account(index)?.address
                != shared_token_2022_slot(&source_program, &destination_program)
        {
            return Err(unsafe_instruction());
        }
    } else if account(0)?.address != source_program || source_program != destination_program {
        return Err(unsafe_instruction());
    }
    if shared {
        let authority = account(1)?.address.as_str();
        let program_source = account(4)?.address.as_str();
        let program_destination = account(5)?.address.as_str();
        validate_program_token_account(
            wallet,
            program_source,
            authority,
            &candidate_mint(&intent.input_mint)?,
        )
        .await?;
        validate_program_token_account(wallet, program_destination, authority, &output_mint)
            .await?;
    }
    if let Some(fee_mint) = fee_mint {
        if !fin_solana::parse_address(fee_mint).is_ok() {
            return Err(unsafe_instruction());
        }
        validate_non_wallet_fee_account(wallet, fee_address, fee_mint).await?;
    }
    Ok(())
}

async fn validate_route_token_accounts_v2(
    wallet: &SolanaWalletAdapter,
    ix: &NormalizedInstruction,
    decoded: &DecodedSwapInstruction,
    intent: &SwapIntent,
    auxiliary: &AuxiliaryEffects,
    fee_mint: Option<&str>,
) -> Result<()> {
    use swap::SwapInstructionVariant as V;
    let shared = matches!(decoded.variant, V::SharedExactInV2 | V::SharedExactOutV2);
    let (
        source,
        destination,
        source_mint,
        destination_mint,
        source_program,
        destination_program,
        event,
        program,
        authority,
    ) = if shared {
        (2, 5, 6, 7, 8, 9, 10, 11, Some(0))
    } else {
        (1, 2, 3, 4, 5, 6, 8, 9, None)
    };
    if ix.accounts.len() <= program {
        return Err(unsafe_instruction_at("required account positions"));
    }
    let account = |index: usize| ix.accounts.get(index).ok_or_else(unsafe_instruction);
    let sentinel = swap::JUPITER_V6_PROGRAM;
    validate_v2_account_privileges(&ix.accounts, wallet.address(), shared)?;
    if account(event)?.address != "D8cy77BBepLMngZx6ZukaTff5hCt1HrWyKk3Hnd9oitf"
        || account(event)?.signer
        || account(event)?.writable
        || account(program)?.address != swap::JUPITER_V6_PROGRAM
        || account(program)?.signer
        || account(program)?.writable
    {
        return Err(unsafe_instruction_at(
            "Jupiter event/program identity or privileges",
        ));
    }
    if account(source_mint)?.address != candidate_mint(&intent.input_mint)?
        || account(destination_mint)?.address != candidate_mint(&intent.output_mint)?
    {
        return Err(unsafe_instruction_at("source/destination mint binding"));
    }
    let source_address = account(source)?.address.as_str();
    let destination_address = account(destination)?.address.as_str();
    let source_token_program = validate_token_account(
        wallet,
        source_address,
        &candidate_mint(&intent.input_mint)?,
        true,
        intent.input_mint == "native",
        auxiliary,
    )
    .await?;
    let destination_token_program = validate_token_account(
        wallet,
        destination_address,
        &candidate_mint(&intent.output_mint)?,
        false,
        false,
        auxiliary,
    )
    .await?;
    let input_mint_program =
        finalized_mint_program(wallet, &candidate_mint(&intent.input_mint)?).await?;
    let output_mint_program =
        finalized_mint_program(wallet, &candidate_mint(&intent.output_mint)?).await?;
    if account(source_program)?.address != source_token_program
        || account(destination_program)?.address != destination_token_program
        || source_token_program != input_mint_program
        || destination_token_program != output_mint_program
    {
        return Err(unsafe_instruction_at(
            "token program versus finalized mint identity",
        ));
    }
    if let Some(authority_index) = authority {
        let authority_id = decoded.authority_id.ok_or_else(unsafe_instruction)?;
        if authority_id >= 16 {
            return Err(unsafe_instruction_at("shared authority id"));
        }
        let program_key = Pubkey::try_from(sentinel).map_err(|_| unsafe_instruction())?;
        let expected =
            Pubkey::find_program_address(&[&b"authority"[..], &[authority_id]], &program_key)
                .0
                .to_string();
        if account(authority_index)?.address != expected
            || account(authority_index)?.signer
            || account(authority_index)?.writable
        {
            return Err(unsafe_instruction_at("shared authority PDA"));
        }
        validate_program_token_account(
            wallet,
            &account(3)?.address,
            &expected,
            &candidate_mint(&intent.input_mint)?,
        )
        .await?;
        validate_program_token_account(
            wallet,
            &account(4)?.address,
            &expected,
            &candidate_mint(&intent.output_mint)?,
        )
        .await?;
    } else if account(0)?.address != wallet.address() {
        return Err(unsafe_instruction_at("transfer authority"));
    }
    // The V2 IDL's fixed account prefix ends at slot 9 for route_v2 and slot
    // 11 for shared_accounts_route_v2. Jupiter places platformFeeAccount at
    // the start of the remaining-account suffix; route/pool metas follow it.
    if !shared && account(7)?.address != sentinel {
        return Err(unsafe_instruction_at("optional destination account slot"));
    }
    let fee_account = validate_v2_platform_fee_meta(
        &ix.accounts,
        shared,
        decoded.platform_fee_bps,
        fee_mint,
        &candidate_mint(&intent.input_mint)?,
        &candidate_mint(&intent.output_mint)?,
    )?;
    if let (Some(fee_account), Some(fee_mint)) = (fee_account, fee_mint) {
        validate_non_wallet_fee_account(wallet, &fee_account.address, fee_mint).await?;
    }
    Ok(())
}

fn validate_v2_account_privileges(
    accounts: &[NormalizedAccountMeta],
    wallet: &str,
    shared: bool,
) -> Result<()> {
    let (authority, source, destination) = if shared { (1, 2, 5) } else { (0, 1, 2) };
    let account = |index: usize| accounts.get(index).ok_or_else(unsafe_instruction);
    if has_foreign_route_signer(accounts, wallet) {
        return Err(unsafe_instruction_at("foreign signer scope"));
    }
    if account(authority)?.address != wallet
        || !account(authority)?.signer
        || !account(source)?.writable
        || !account(destination)?.writable
    {
        return Err(unsafe_instruction_at(
            "wallet authority or token-account privileges",
        ));
    }
    if shared && (!account(3)?.writable || !account(4)?.writable) {
        return Err(unsafe_instruction_at(
            "shared program token-account privileges",
        ));
    }
    Ok(())
}

fn validate_v2_platform_fee_meta<'a>(
    accounts: &'a [NormalizedAccountMeta],
    shared: bool,
    fee_bps: u16,
    fee_mint: Option<&str>,
    input_mint: &str,
    output_mint: &str,
) -> Result<Option<&'a NormalizedAccountMeta>> {
    if fee_bps == 0 {
        if fee_mint.is_some() {
            return Err(unsafe_instruction_at("fee mint without platform fee"));
        }
        return Ok(None);
    }
    let fee_mint = fee_mint.ok_or_else(|| unsafe_instruction_at("missing platform-fee mint"))?;
    if fee_mint != input_mint && fee_mint != output_mint {
        return Err(unsafe_instruction_at(
            "platform-fee mint differs from swap mints",
        ));
    }
    // route_v2 has ten fixed accounts and shared_accounts_route_v2 has twelve.
    // Fee account is the first remaining account, ahead of route pool metas.
    let fee_index = if shared { 12 } else { 10 };
    let fee_account = accounts.get(fee_index).ok_or_else(|| {
        unsafe_instruction_at("platform-fee account is missing from remaining accounts")
    })?;
    if !fee_account.writable
        || fee_account.signer
        || fee_account.address == swap::JUPITER_V6_PROGRAM
    {
        return Err(unsafe_instruction_at("platform-fee account privileges"));
    }
    Ok(Some(fee_account))
}

fn has_foreign_route_signer(accounts: &[NormalizedAccountMeta], wallet: &str) -> bool {
    accounts
        .iter()
        .any(|meta| meta.signer && meta.address != wallet)
}

fn shared_token_2022_slot(source_program: &str, destination_program: &str) -> &'static str {
    if source_program == fin_solana::token::TOKEN_2022_PROGRAM_ID
        || destination_program == fin_solana::token::TOKEN_2022_PROGRAM_ID
    {
        fin_solana::token::TOKEN_2022_PROGRAM_ID
    } else {
        swap::JUPITER_V6_PROGRAM
    }
}

async fn validate_token_account(
    wallet: &SolanaWalletAdapter,
    address: &str,
    mint: &str,
    is_source: bool,
    allow_native_missing: bool,
    auxiliary: &AuxiliaryEffects,
) -> Result<String> {
    let response = wallet
        .runtime()
        .rpc()
        .request(
            "getAccountInfo",
            json!([address,{"encoding":"base64","commitment":"finalized"}]),
        )
        .await?;
    let Some(account) = response.get("value").filter(|v| !v.is_null()) else {
        let created = auxiliary
            .created_token_accounts
            .iter()
            .find(|created| created.address == address && created.mint == mint);
        if let Some(created) = created
            && associated_address(wallet.address(), &created.token_program, mint)? == address
        {
            return Ok(created.token_program.clone());
        }
        if allow_native_missing
            && is_source
            && address
                == associated_address(
                    wallet.address(),
                    fin_solana::token::TOKEN_PROGRAM_ID,
                    swap::WRAPPED_SOL_MINT,
                )?
        {
            return Ok(fin_solana::token::TOKEN_PROGRAM_ID.into());
        }
        return Err(unsafe_instruction());
    };
    let program = account
        .get("owner")
        .and_then(Value::as_str)
        .ok_or_else(unsafe_instruction)?;
    if program != fin_solana::token::TOKEN_PROGRAM_ID
        && program != fin_solana::token::TOKEN_2022_PROGRAM_ID
    {
        return Err(unsafe_instruction());
    }
    let data = fin_solana::token::decode_rpc_account_data(
        account.get("data").ok_or_else(unsafe_instruction)?,
    )?;
    let token = fin_solana::token::parse_token_account(program, address, &data)?;
    if token.token_owner != wallet.address()
        || token.mint != mint
        || token.state != "initialized"
        || token.delegate.is_some() && token.delegated_amount_atoms > 0
    {
        return Err(unsafe_instruction());
    }
    validate_token_credit_extensions(&token)?;
    let _ = is_source;
    Ok(program.to_owned())
}

fn validate_token_credit_extensions(token: &fin_solana::token::TokenAccountInfo) -> Result<()> {
    if token.memo_required
        || token.confidential_transfer_approved == Some(false)
            && token.allow_non_confidential_credits != Some(true)
    {
        return Err(FinError::new(
            "JUPITER_RECIPIENT_TOKEN_EXTENSION_UNSUPPORTED",
            "prepare",
            "A token account in the swap transaction requires a memo or blocks public token credits.",
            "Use a recipient account that accepts public credits without a required memo. Confidential-transfer metadata is allowed when non-confidential credits are enabled.",
        ));
    }
    Ok(())
}

async fn validate_program_token_account(
    wallet: &SolanaWalletAdapter,
    address: &str,
    authority: &str,
    mint: &str,
) -> Result<()> {
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
        .ok_or_else(unsafe_instruction)?;
    let program = account
        .get("owner")
        .and_then(Value::as_str)
        .ok_or_else(unsafe_instruction)?;
    if program != fin_solana::token::TOKEN_PROGRAM_ID
        && program != fin_solana::token::TOKEN_2022_PROGRAM_ID
    {
        return Err(unsafe_instruction());
    }
    let data = fin_solana::token::decode_rpc_account_data(
        account.get("data").ok_or_else(unsafe_instruction)?,
    )?;
    let token = fin_solana::token::parse_token_account(program, address, &data)?;
    if token.token_owner != authority || token.mint != mint || token.state != "initialized" {
        return Err(unsafe_instruction());
    }
    validate_token_credit_extensions(&token)?;
    Ok(())
}

async fn validate_owned_token_account(
    wallet: &SolanaWalletAdapter,
    address: &str,
    mint: &str,
    owner: &str,
    token_program: &str,
) -> Result<()> {
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
        .ok_or_else(unsafe_instruction)?;
    if account.get("owner").and_then(Value::as_str) != Some(token_program) {
        return Err(unsafe_instruction());
    }
    let data = fin_solana::token::decode_rpc_account_data(
        account.get("data").ok_or_else(unsafe_instruction)?,
    )?;
    let token = fin_solana::token::parse_token_account(token_program, address, &data)?;
    if token.token_owner != owner
        || token.mint != mint
        || token.state != "initialized"
        || token.delegate.is_some() && token.delegated_amount_atoms > 0
    {
        return Err(unsafe_instruction());
    }
    Ok(())
}

async fn finalized_mint_program(wallet: &SolanaWalletAdapter, mint: &str) -> Result<String> {
    let response = wallet
        .runtime()
        .rpc()
        .request(
            "getAccountInfo",
            json!([mint,{"encoding":"base64","commitment":"finalized"}]),
        )
        .await?;
    let account = response
        .get("value")
        .filter(|value| !value.is_null())
        .ok_or_else(unsafe_instruction)?;
    let program = account
        .get("owner")
        .and_then(Value::as_str)
        .filter(|program| {
            *program == fin_solana::token::TOKEN_PROGRAM_ID
                || *program == fin_solana::token::TOKEN_2022_PROGRAM_ID
        })
        .ok_or_else(unsafe_instruction)?;
    let bytes = fin_solana::token::decode_rpc_account_data(
        account.get("data").ok_or_else(unsafe_instruction)?,
    )?;
    let epoch = wallet
        .runtime()
        .rpc()
        .request("getEpochInfo", json!([{"commitment":"finalized"}]))
        .await?
        .get("epoch")
        .and_then(Value::as_u64)
        .ok_or_else(unsafe_instruction)?;
    let mint_info = fin_solana::token::parse_mint(program, &bytes, epoch)?;
    if !mint_info.transfer_supported_by_runtime
        || mint_info.paused == Some(true)
        || mint_info.transfer_fee.is_some()
        || mint_info.transfer_hook_program.is_some()
        || mint_info.permanent_delegate.is_some()
    {
        return Err(FinError::new(
            "JUPITER_TOKEN_EXTENSION_UNSUPPORTED",
            "prepare",
            "This swap mint has transfer behavior that the selected Jupiter route cannot bound for source debits, fees, or recipient credits.",
            "Use an ordinary SPL or plain Token-2022 mint. Configured transfer fees, transfer hooks, paused mints, permanent delegates, and other unsupported transfer extensions are not accepted. Confidential-transfer metadata is allowed when the token accounts permit public credits.",
        ));
    }
    Ok(program.to_owned())
}

async fn validate_non_wallet_fee_account(
    wallet: &SolanaWalletAdapter,
    address: &str,
    mint: &str,
) -> Result<()> {
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
        .ok_or_else(unsafe_instruction)?;
    let program = account
        .get("owner")
        .and_then(Value::as_str)
        .ok_or_else(unsafe_instruction)?;
    if program != fin_solana::token::TOKEN_PROGRAM_ID
        && program != fin_solana::token::TOKEN_2022_PROGRAM_ID
    {
        return Err(unsafe_instruction());
    }
    let data = fin_solana::token::decode_rpc_account_data(
        account.get("data").ok_or_else(unsafe_instruction)?,
    )?;
    let token = fin_solana::token::parse_token_account(program, address, &data)?;
    if token.mint != mint || token.token_owner == wallet.address() || token.state != "initialized" {
        return Err(unsafe_instruction());
    }
    validate_token_credit_extensions(&token)?;
    Ok(())
}

fn unsafe_instruction() -> FinError {
    FinError::new(
        "JUPITER_INSTRUCTION_UNREVIEWED",
        "prepare",
        "The provider transaction contains an instruction or account effect outside the reviewed swap envelope.",
        "Request a transaction whose complete instruction set and exact token accounts can be verified.",
    )
}

fn validation_stage_if_unreviewed(mut error: FinError, validation_stage: &'static str) -> FinError {
    if matches!(
        error.code.as_str(),
        "JUPITER_INSTRUCTION_UNREVIEWED" | "JUPITER_INSTRUCTION_UNSUPPORTED"
    ) && error.details.is_none()
    {
        error.details = Some(Box::new(json!({"validation_stage": validation_stage})));
    }
    error
}

fn validation_router_context(mut error: FinError, router: Option<&str>) -> FinError {
    if error.code != "JUPITER_INSTRUCTION_UNREVIEWED" {
        return error;
    }
    let router = match router {
        Some("metis") => "metis",
        Some("jupiterz") => "jupiterz",
        Some("dflow") => "dflow",
        Some("okx") => "okx",
        _ => "unknown",
    };
    let mut details = error
        .details
        .take()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    details.insert("router".into(), json!(router));
    error.details = Some(Box::new(Value::Object(details)));
    error
}

fn unsafe_instruction_at(validation_stage: &'static str) -> FinError {
    FinError::new(
        "JUPITER_INSTRUCTION_UNREVIEWED",
        "prepare",
        format!("The Jupiter route failed {validation_stage} validation."),
        "Request a fresh transaction and review the reported validation stage.",
    )
    .with_details(json!({"validation_stage": validation_stage}))
}

fn unsafe_instruction_at_with_details(validation_stage: &'static str, details: Value) -> FinError {
    let mut details = details.as_object().cloned().unwrap_or_default();
    details.insert("validation_stage".into(), json!(validation_stage));
    unsafe_instruction().with_details(Value::Object(details))
}

struct MetaExecuteSender {
    api: Arc<dyn JupiterApi>,
    request_id: String,
    expires_at: String,
}
impl MetaExecuteSender {
    async fn send_checked(
        &self,
        signed_transaction_base64: &str,
        before_send: &(dyn Fn() -> Result<()> + Send + Sync),
    ) -> Result<String> {
        let max_wait =
            crate::api::remaining_until_deadline(&self.expires_at)?.min(REQUEST_DEADLINE);
        let response = self.api.request_checked(
            JupiterHost::Platform,
            Method::POST,
            "/swap/v2/execute",
            Some(json!({"signedTransaction":signed_transaction_base64,"requestId":self.request_id})),
            RateBucket::Execute,
            RequestPriority::Financial,
            max_wait,
            before_send,
            None,
        ).await?;
        let status = response.get("status").and_then(Value::as_str).unwrap_or("");
        let signature = response.get("signature").and_then(Value::as_str).ok_or_else(|| FinError::new(
            "JUPITER_EXECUTE_UNCERTAIN", "submission",
            "Jupiter did not return a transaction signature after the persisted execute request.",
            "Reconcile the original jupiter request identity; do not submit it again.",
        ))?;
        if status != "Success"
            || bs58::decode(signature)
                .into_vec()
                .map_or(true, |bytes| bytes.len() != 64)
        {
            return Err(FinError::new(
                "JUPITER_EXECUTE_UNCERTAIN",
                "submission",
                "Jupiter did not confirm a valid transaction signature for the persisted execute request.",
                "Reconcile the original jupiter request identity; do not submit it again.",
            ));
        }
        Ok(signature.into())
    }
}
#[async_trait]
impl TransactionSender for MetaExecuteSender {
    fn submission_reference(
        &self,
        frozen: &FrozenTransaction,
        _first_signature: Option<&str>,
    ) -> Result<String> {
        if self.request_id.is_empty()
            || frozen.provider_request_id() != Some(self.request_id.as_str())
        {
            return Err(bad_provider_response());
        }
        Ok(format!("jupiter:{}", self.request_id))
    }
    async fn send_signed(
        &self,
        signed_transaction_base64: &str,
        _frozen: &FrozenTransaction,
    ) -> Result<String> {
        self.send_checked(signed_transaction_base64, &|| Ok(()))
            .await
    }

    async fn send_signed_checked(
        &self,
        signed_transaction_base64: &str,
        _frozen: &FrozenTransaction,
        before_send: &(dyn Fn() -> Result<()> + Send + Sync),
    ) -> Result<String> {
        self.send_checked(signed_transaction_base64, before_send)
            .await
    }

    async fn send_signed_with_guard(
        &self,
        signed_transaction_base64: &str,
        _frozen: &FrozenTransaction,
        guard: &dyn fin_solana::transaction::AsyncDispatchGuard,
    ) -> Result<String> {
        let max_wait =
            crate::api::remaining_until_deadline(&self.expires_at)?.min(REQUEST_DEADLINE);
        let response = self.api.request_checked(
            JupiterHost::Platform,
            Method::POST,
            "/swap/v2/execute",
            Some(json!({"signedTransaction":signed_transaction_base64,"requestId":self.request_id})),
            RateBucket::Execute,
            RequestPriority::Financial,
            max_wait,
            &|| Ok(()),
            Some(guard),
        ).await?;
        let signature = response.get("signature").and_then(Value::as_str).ok_or_else(|| FinError::new(
            "JUPITER_EXECUTE_UNCERTAIN", "submission",
            "Jupiter did not return a transaction signature after the persisted execute request.",
            "Reconcile the original jupiter request identity; do not submit it again.",
        ))?;
        if response.get("status").and_then(Value::as_str) != Some("Success")
            || bs58::decode(signature)
                .into_vec()
                .map_or(true, |bytes| bytes.len() != 64)
        {
            return Err(FinError::new(
                "JUPITER_EXECUTE_UNCERTAIN",
                "submission",
                "Jupiter did not confirm a valid transaction signature for the persisted execute request.",
                "Reconcile the original jupiter request identity; do not submit it again.",
            ));
        }
        Ok(signature.into())
    }
}

fn verify_swap_effects(
    record: &OperationRecord,
    submission: &Submission,
    signature: &str,
    slot: u64,
    fee: u64,
    proof: &fin_solana::transaction::FinalizedReceiptProof,
) -> Result<fin_domain::VerifiedEffects> {
    let intent = JupiterAdapter::intent(&record.input)?;
    let expected_input = record
        .preview
        .as_ref()
        .and_then(|p| p.binding.get("input_atoms"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or(intent.amount_atoms.clone());
    let expected_min = record
        .preview
        .as_ref()
        .and_then(|p| p.binding.get("minimum_output_atoms"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or(intent.minimum_output_atoms.clone());
    let input_mint = candidate_mint(&intent.input_mint)?;
    let output_mint = candidate_mint(&intent.output_mint)?;
    let wallet_address = record
        .preview
        .as_ref()
        .and_then(|p| p.binding.get("wallet_address"))
        .and_then(Value::as_str)
        .ok_or_else(bad_provider_response)?;
    let preview = record.preview.as_ref().ok_or_else(bad_provider_response)?;
    let (input_decimals, output_decimals) = preview
        .intent
        .expected
        .first()
        .and_then(|effect| match effect {
            EffectConstraint::Conversion { input, output, .. } => {
                Some((input.decimals, output.decimals))
            }
            _ => None,
        })
        .ok_or_else(bad_provider_response)?;
    let input_program = preview
        .binding
        .get("input_token_program")
        .and_then(Value::as_str)
        .ok_or_else(bad_provider_response)?;
    let rfq_native_input_direct = preview
        .binding
        .get("rfq_native_input_direct")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let rfq_native_output_direct = preview
        .binding
        .get("rfq_native_output_direct")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let dflow_native_input_direct = preview
        .binding
        .get("dflow_native_input_direct")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let dflow_native_output_direct = preview
        .binding
        .get("dflow_native_output_direct")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if (rfq_native_input_direct || rfq_native_output_direct)
        && preview.binding.get("router").and_then(Value::as_str) != Some("jupiterz")
        || (dflow_native_input_direct || dflow_native_output_direct)
            && preview.binding.get("router").and_then(Value::as_str) != Some("dflow")
    {
        return Err(bad_provider_response());
    }
    let output_program = preview
        .binding
        .get("output_token_program")
        .and_then(Value::as_str)
        .ok_or_else(bad_provider_response)?;
    let input_delta =
        proof.token_delta_verified(wallet_address, &input_mint, input_program, input_decimals)?;
    let output_delta = proof.token_delta_verified(
        wallet_address,
        &output_mint,
        output_program,
        output_decimals,
    )?;
    validate_no_unexpected_wallet_token_changes(
        proof,
        wallet_address,
        &[input_mint.as_str(), output_mint.as_str()],
    )?;
    let expected_input_atoms = expected_input
        .parse::<u128>()
        .map_err(|_| bad_provider_response())?;
    let debit = if intent.input_mint == "native" {
        let wrapped = preview
            .binding
            .get("wrapped_native_lamports")
            .and_then(Value::as_u64)
            .ok_or_else(bad_provider_response)?;
        if (wrapped as u128 != expected_input_atoms
            && !rfq_native_input_direct
            && !dflow_native_input_direct)
            || ((rfq_native_input_direct || dflow_native_input_direct) && wrapped != 0)
            || !input_delta.is_zero()
        {
            return Err(bad_provider_response());
        }
        expected_input_atoms
    } else {
        if !input_delta.is_negative() {
            return Err(bad_provider_response());
        }
        input_delta
            .magnitude()
            .as_str()
            .parse::<u128>()
            .map_err(|_| bad_provider_response())?
    };
    if output_delta.is_negative() {
        return Err(bad_provider_response());
    }
    let mut credit = output_delta
        .as_str()
        .parse::<u128>()
        .map_err(|_| bad_provider_response())?;
    if debit
        != expected_input
            .parse::<u128>()
            .map_err(|_| bad_provider_response())?
    {
        return Err(FinError::new(
            "JUPITER_SETTLEMENT_EFFECT_MISMATCH",
            "reconciliation",
            "The finalized transaction token balances do not prove the approved input debit.",
            "Keep the operation unresolved and inspect the finalized transaction effects.",
        ));
    }
    let input_asset = AssetId {
        network: NETWORK.into(),
        reference: normalize_asset(&intent.input_mint),
        decimals: record
            .preview
            .as_ref()
            .and_then(|p| p.intent.expected.first())
            .and_then(|e| match e {
                EffectConstraint::Conversion { input, .. } => Some(input.decimals),
                _ => None,
            })
            .ok_or_else(bad_provider_response)?,
    };
    let output_asset = AssetId {
        network: NETWORK.into(),
        reference: normalize_asset(&intent.output_mint),
        decimals: record
            .preview
            .as_ref()
            .and_then(|p| p.intent.expected.first())
            .and_then(|e| match e {
                EffectConstraint::Conversion { output, .. } => Some(output.decimals),
                _ => None,
            })
            .ok_or_else(bad_provider_response)?,
    };
    let resource = ResourceId(format!("solana:mainnet-beta:{wallet_address}"));
    let native = AssetId {
        network: NETWORK.into(),
        reference: "native".into(),
        decimals: 9,
    };
    let mut effects = vec![fin_domain::FinancialEffect::Conversion {
        resource: resource.clone(),
        spent: Amount {
            asset: input_asset,
            atoms: fin_domain::Quantity::new(debit.to_string())?,
        },
        received: Amount {
            asset: output_asset,
            atoms: fin_domain::Quantity::new(credit.to_string())?,
        },
    }];
    let funding: Vec<AccountFundingRequirement> = serde_json::from_value(
        record
            .preview
            .as_ref()
            .and_then(|preview| preview.binding.get("funding_accounts"))
            .cloned()
            .ok_or_else(bad_provider_response)?,
    )
    .map_err(|_| bad_provider_response())?;
    let closed_accounts = preview
        .binding
        .get("closed_wrapped_native_accounts")
        .and_then(Value::as_array)
        .ok_or_else(bad_provider_response)?;
    let mut actual_funding = 0u64;
    let mut wallet_rent_gross = 0u64;
    for requirement in &funding {
        if requirement.refund_authority != wallet_address || requirement.lamports == 0 {
            return Err(bad_provider_response());
        }
        if requirement.payer == wallet_address {
            wallet_rent_gross = wallet_rent_gross
                .checked_add(requirement.lamports)
                .ok_or_else(bad_provider_response)?;
        }
        let delta = proof.lamport_delta(&requirement.address)?;
        let delta = delta
            .as_str()
            .parse::<i128>()
            .map_err(|_| bad_provider_response())?;
        let closed_in_transaction = closed_accounts
            .iter()
            .any(|value| value.as_str() == Some(&requirement.address));
        if (closed_in_transaction && delta != 0)
            || (!closed_in_transaction && delta != requirement.lamports as i128)
        {
            return Err(FinError::new(
                "JUPITER_ACCOUNT_FUNDING_MISMATCH",
                "reconciliation",
                "Finalized account balances do not prove the approved refundable account funding.",
                "Keep the operation unresolved and inspect the finalized account creation and close proof.",
            ));
        }
        if closed_in_transaction {
            continue;
        }
        if requirement.payer != wallet_address {
            continue;
        }
        actual_funding = actual_funding
            .checked_add(requirement.lamports)
            .ok_or_else(bad_provider_response)?;
        let principal = fin_domain::Quantity::from_u64(requirement.lamports);
        let position = format!("account-funding:{}", requirement.address);
        let anchor = fin_domain::PositionAnchor {
            resource: resource.clone(),
            position: position.clone(),
            position_kind: fin_domain::PositionKind::RefundableAccountFunding,
            principal: Amount {
                asset: native.clone(),
                atoms: principal.clone(),
            },
            scaled_receipt: None,
            evidence_version: 1,
            evidence: json!({"cluster":CLUSTER,"account":requirement.address,"account_owner":requirement.account_owner,"refund_authority":requirement.refund_authority,"rent_lamports":requirement.lamports,"proof_signature":signature,"proof_message_sha256":proof.message_sha256}),
            observed_at: Utc::now().to_rfc3339(),
        };
        effects.push(fin_domain::FinancialEffect::PositionChange {
            resource: resource.clone(),
            position,
            asset: native.clone(),
            delta: fin_domain::SignedQuantity::positive(&principal),
            scaled_receipt: None,
            anchor: Some(anchor),
        });
        effects.push(fin_domain::FinancialEffect::OriginMovement {
            from: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            to: AccountingLocation::Position {
                resource: resource.clone(),
                position: format!("account-funding:{}", requirement.address),
                position_kind: fin_domain::PositionKind::RefundableAccountFunding,
            },
            debit: Amount {
                asset: native.clone(),
                atoms: principal.clone(),
            },
            credit: Amount {
                asset: native.clone(),
                atoms: principal,
            },
        });
    }
    let expected_funding = record
        .preview
        .as_ref()
        .and_then(|preview| preview.binding.get("account_funding_lamports"))
        .and_then(Value::as_u64)
        .ok_or_else(bad_provider_response)?;
    if actual_funding > expected_funding {
        return Err(bad_provider_response());
    }
    let wallet_rent_net = funding
        .iter()
        .filter(|f| {
            f.payer == wallet_address
                && !closed_accounts
                    .iter()
                    .any(|value| value.as_str() == Some(&f.address))
        })
        .try_fold(0u64, |total, f| {
            total
                .checked_add(f.lamports)
                .ok_or_else(bad_provider_response)
        })?;
    let has_matching_wallet_funding_debit = wallet_rent_gross > 0
        && preview.intent.expected.iter().any(|constraint| {
            matches!(
                constraint,
                EffectConstraint::Debit {
                    resource: expected_resource,
                    asset: expected_asset,
                    maximum_debit,
                } if expected_resource == &resource
                    && expected_asset == &native
                    && maximum_debit == &fin_domain::Quantity::from_u64(wallet_rent_gross)
            )
        });
    // Keep the debit expectation satisfiable for temporary wrapped-SOL
    // accounts that were funded and closed in this same transaction. The
    // finalized account and wallet balance checks establish that net rent is zero;
    // only the immutable, matching preview constraint justifies emitting zero.
    if wallet_rent_net > 0 || (wallet_rent_net == 0 && has_matching_wallet_funding_debit) {
        effects.push(fin_domain::FinancialEffect::CashDebit {
            resource: resource.clone(),
            amount: Amount {
                asset: native.clone(),
                atoms: fin_domain::Quantity::from_u64(wallet_rent_net),
            },
        });
    }
    let refunds: Vec<AccountFundingRequirement> = serde_json::from_value(
        preview
            .binding
            .get("closed_wrapped_native_refunds")
            .cloned()
            .ok_or_else(bad_provider_response)?,
    )
    .map_err(|_| bad_provider_response())?;
    let mut close_refund_total = 0u64;
    for refund in refunds {
        if refund.refund_authority != wallet_address
            || refund.lamports == 0
            || !closed_accounts
                .iter()
                .any(|value| value.as_str() == Some(&refund.address))
        {
            return Err(bad_provider_response());
        }
        let delta = proof
            .lamport_delta(&refund.address)?
            .as_str()
            .parse::<i128>()
            .map_err(|_| bad_provider_response())?;
        let created_here = funding
            .iter()
            .any(|f| f.address == refund.address && f.lamports == refund.lamports);
        if (created_here && delta != 0) || (!created_here && delta != -(refund.lamports as i128)) {
            return Err(bad_provider_response());
        }
        close_refund_total = close_refund_total
            .checked_add(refund.lamports)
            .ok_or_else(bad_provider_response)?;
        if !created_here {
            let amount = fin_domain::Quantity::from_u64(refund.lamports);
            let position = format!("account-funding:{}", refund.address);
            let anchor = fin_domain::PositionAnchor {
                resource: resource.clone(),
                position: position.clone(),
                position_kind: fin_domain::PositionKind::RefundableAccountFunding,
                principal: Amount {
                    asset: native.clone(),
                    // Position anchors represent post-transaction state. A
                    // closed account has no refundable rent position left.
                    atoms: fin_domain::Quantity::zero(),
                },
                scaled_receipt: None,
                evidence_version: 1,
                evidence: json!({"cluster":CLUSTER,"account":refund.address,"account_owner":refund.account_owner,"refund_authority":refund.refund_authority,"returned_rent_lamports":refund.lamports,"proof_signature":signature,"proof_message_sha256":proof.message_sha256}),
                observed_at: Utc::now().to_rfc3339(),
            };
            effects.push(fin_domain::FinancialEffect::PositionChange {
                resource: resource.clone(),
                position: position.clone(),
                asset: native.clone(),
                delta: fin_domain::SignedQuantity::negative(&amount),
                scaled_receipt: None,
                anchor: Some(anchor),
            });
            effects.push(fin_domain::FinancialEffect::UsableCredit {
                to: resource.clone(),
                amount: Amount {
                    asset: native.clone(),
                    atoms: amount.clone(),
                },
            });
            effects.push(fin_domain::FinancialEffect::OriginMovement {
                from: AccountingLocation::Position {
                    resource: resource.clone(),
                    position,
                    position_kind: fin_domain::PositionKind::RefundableAccountFunding,
                },
                to: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                debit: Amount {
                    asset: native.clone(),
                    atoms: amount.clone(),
                },
                credit: Amount {
                    asset: native.clone(),
                    atoms: amount,
                },
            });
        }
    }
    let wallet_delta = proof
        .lamport_delta(wallet_address)?
        .as_str()
        .parse::<i128>()
        .map_err(|_| bad_provider_response())?;
    let wallet_fee =
        if submission.data.get("fee_payer").and_then(Value::as_str) == Some(wallet_address) {
            fee
        } else {
            0
        };
    if intent.output_mint == "native" {
        if closed_accounts.is_empty() && !rfq_native_output_direct && !dflow_native_output_direct {
            return Err(FinError::new(
                "JUPITER_SETTLEMENT_EFFECT_MISMATCH",
                "reconciliation",
                "The finalized native-SOL output did not close its canonical wrapped-SOL account.",
                "Keep the operation unresolved and inspect the finalized transaction effects.",
            ));
        }
        credit = native_output_credit(
            wallet_delta,
            intent.input_mint == "native",
            expected_input_atoms,
            wallet_fee,
            wallet_rent_gross,
            close_refund_total,
        )?;
        match effects.first_mut() {
            Some(fin_domain::FinancialEffect::Conversion { received, .. }) => {
                received.atoms = fin_domain::Quantity::new(credit.to_string())?;
            }
            _ => return Err(bad_provider_response()),
        }
    }
    if credit
        < expected_min
            .parse::<u128>()
            .map_err(|_| bad_provider_response())?
    {
        return Err(FinError::new(
            "JUPITER_SETTLEMENT_EFFECT_MISMATCH",
            "reconciliation",
            "The finalized transaction effects do not prove the approved minimum output credit.",
            "Keep the operation unresolved and inspect the finalized transaction effects.",
        ));
    }
    let expected_wallet_delta = (if intent.output_mint == "native"
        && (!closed_accounts.is_empty() || rfq_native_output_direct || dflow_native_output_direct)
    {
        credit as i128
    } else {
        0
    }) - (if intent.input_mint == "native" {
        expected_input_atoms as i128
    } else {
        0
    }) - wallet_fee as i128
        - wallet_rent_gross as i128
        + close_refund_total as i128;
    if wallet_delta != expected_wallet_delta {
        return Err(FinError::new(
            "JUPITER_NATIVE_BALANCE_MISMATCH",
            "reconciliation",
            "Finalized SOL changes do not conserve the approved swap, fee, account funding and close refunds.",
            "Keep the operation unresolved and inspect the complete finalized account-balance proof.",
        ));
    }
    if submission.data.get("fee_payer").and_then(Value::as_str) == Some(wallet_address) && fee > 0 {
        effects.push(fin_domain::FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: AssetId {
                    network: NETWORK.into(),
                    reference: "native".into(),
                    decimals: 9,
                },
                atoms: fin_domain::Quantity::from_u64(fee),
            },
            category: "solana_network_fee".into(),
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    let evidence = json!({"cluster":CLUSTER,"signature":signature,"slot":slot,"network_fee_lamports":fee,"proof":proof,"provider_request_id":submission.data.get("provider_request_id")});
    let mut verified = fin_domain::VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects,
        evidence,
        observed_at: Utc::now().to_rfc3339(),
        proof_digest: String::new(),
    };
    verified.proof_digest = fin_domain::verified_effect_digest(&verified)?;
    Ok(verified)
}

fn native_output_credit(
    wallet_delta: i128,
    input_is_native: bool,
    input_atoms: u128,
    wallet_fee: u64,
    wallet_rent_gross: u64,
    close_refund_total: u64,
) -> Result<u128> {
    let input = i128::try_from(if input_is_native { input_atoms } else { 0 })
        .map_err(|_| bad_provider_response())?;
    let credit = wallet_delta
        .checked_add(input)
        .and_then(|value| value.checked_add(i128::from(wallet_fee)))
        .and_then(|value| value.checked_add(i128::from(wallet_rent_gross)))
        .and_then(|value| value.checked_sub(i128::from(close_refund_total)))
        .ok_or_else(bad_provider_response)?;
    u128::try_from(credit).map_err(|_| bad_provider_response())
}

fn validate_no_unexpected_wallet_token_changes(
    proof: &fin_solana::transaction::FinalizedReceiptProof,
    wallet: &str,
    allowed_mints: &[&str],
) -> Result<()> {
    let mut identities = BTreeMap::<String, (String, u8)>::new();
    for row in proof
        .pre_token_balances
        .iter()
        .chain(&proof.post_token_balances)
    {
        if row.get("owner").and_then(Value::as_str) != Some(wallet) {
            continue;
        }
        let mint = row
            .get("mint")
            .and_then(Value::as_str)
            .ok_or_else(bad_provider_response)?;
        let program = row
            .get("programId")
            .and_then(Value::as_str)
            .ok_or_else(bad_provider_response)?;
        let decimals = row
            .pointer("/uiTokenAmount/decimals")
            .and_then(Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
            .ok_or_else(bad_provider_response)?;
        if program != fin_solana::token::TOKEN_PROGRAM_ID
            && program != fin_solana::token::TOKEN_2022_PROGRAM_ID
        {
            return Err(bad_provider_response());
        }
        if identities
            .insert(mint.to_owned(), (program.to_owned(), decimals))
            .is_some_and(|previous| previous != (program.to_owned(), decimals))
        {
            return Err(bad_provider_response());
        }
    }
    for (mint, (program, decimals)) in identities {
        if allowed_mints.contains(&mint.as_str()) {
            continue;
        }
        if !proof
            .token_delta_verified(wallet, &mint, &program, decimals)?
            .is_zero()
        {
            return Err(FinError::new(
                "JUPITER_UNEXPECTED_TOKEN_EFFECT",
                "reconciliation",
                "The finalized swap changed an additional wallet token balance outside its approved input and output mints.",
                "Keep the operation unresolved and inspect the complete finalized token-balance proof.",
            ));
        }
    }
    Ok(())
}

pub(crate) async fn token_decimals(
    runtime: &fin_solana::transaction::SolanaRuntime,
    mint: &str,
) -> Result<u8> {
    if mint == "native" {
        return Ok(9);
    }
    let address = fin_solana::parse_address(mint)?;
    let response = runtime
        .rpc()
        .request(
            "getAccountInfo",
            json!([address,{"encoding":"base64","commitment":"finalized"}]),
        )
        .await?;
    let account = response
        .get("value")
        .filter(|v| !v.is_null())
        .ok_or_else(bad_provider_response)?;
    let owner = account
        .get("owner")
        .and_then(Value::as_str)
        .ok_or_else(bad_provider_response)?;
    let epoch = runtime
        .rpc()
        .request("getEpochInfo", json!([{"commitment":"finalized"}]))
        .await?
        .get("epoch")
        .and_then(Value::as_u64)
        .ok_or_else(bad_provider_response)?;
    let data = fin_solana::token::decode_rpc_account_data(
        account.get("data").ok_or_else(bad_provider_response)?,
    )?;
    Ok(fin_solana::token::parse_mint(owner, &data, epoch)?.decimals)
}

pub(crate) async fn token_program_for(
    runtime: &fin_solana::transaction::SolanaRuntime,
    mint: &str,
) -> Result<String> {
    let address = candidate_mint(mint)?;
    let value = runtime
        .rpc()
        .request(
            "getAccountInfo",
            json!([address,{"encoding":"base64","commitment":"finalized"}]),
        )
        .await?;
    let owner = value
        .get("value")
        .filter(|v| !v.is_null())
        .and_then(|v| v.get("owner"))
        .and_then(Value::as_str)
        .ok_or_else(bad_provider_response)?;
    if owner != fin_solana::token::TOKEN_PROGRAM_ID
        && owner != fin_solana::token::TOKEN_2022_PROGRAM_ID
    {
        return Err(bad_provider_response());
    }
    Ok(owner.to_owned())
}

pub(crate) async fn valuation(
    prices: Arc<dyn UsdPriceSource>,
    input: AssetId,
    input_atoms: u64,
    output: AssetId,
    output_atoms: u64,
    fee_lamports: u64,
    wallet: &str,
) -> Result<(Valuation, CostBound)> {
    let native = AssetId {
        network: NETWORK.into(),
        reference: "native".into(),
        decimals: 9,
    };
    let requested = [input.clone(), output.clone(), native.clone()];
    let mut unique = Vec::<AssetId>::new();
    let mut indices = Vec::with_capacity(requested.len());
    for asset in &requested {
        let index = unique
            .iter()
            .position(|known| known == asset)
            .unwrap_or_else(|| {
                unique.push(asset.clone());
                unique.len() - 1
            });
        indices.push(index);
    }
    let unique_quotes = prices.quotes(&unique).await?;
    if unique_quotes.len() != unique.len() {
        return Err(bad_provider_response());
    }
    let quotes = indices
        .into_iter()
        .map(|index| unique_quotes[index].clone())
        .collect::<Vec<_>>();
    let to_micros = |asset: &AssetId,
                     atoms: u64,
                     quote: &fin_pricing::UsdQuote|
     -> Result<fin_domain::Quantity> {
        let unit = fin_domain::Quantity::from_decimal(&quote.usd_per_unit, 36)?;
        let denominator = fin_domain::Quantity::new(format!(
            "1{}",
            "0".repeat(usize::from(asset.decimals) + 30)
        ))?;
        fin_domain::Quantity::from_u64(atoms).mul_div_ceil(&unit, &denominator)
    };
    let movement = to_micros(&input, input_atoms, &quotes[0])?;
    let _output_value = to_micros(&output, output_atoms, &quotes[1])?;
    let fee_value = to_micros(&native, fee_lamports, &quotes[2])?;
    let prices_vec = unique
        .iter()
        .zip(unique_quotes.iter())
        .map(|(asset, quote)| valuation_asset_price(asset, quote))
        .collect::<Result<Vec<_>>>()?;
    let fee = Amount {
        asset: native.clone(),
        atoms: fin_domain::Quantity::from_u64(fee_lamports),
    };
    let cost = CostBound {
        location: AccountingLocation::Cash {
            resource: ResourceId(format!("solana:mainnet-beta:{wallet}")),
        },
        amount: fee,
        treatment: CostTreatment::AdditionalDebit,
    };
    Ok((
        Valuation {
            movement_usd_micros: movement.clone(),
            maximum_fee_usd_micros: fee_value.clone(),
            // The engine charges movement + maximum fee + additional once.
            additional_allocation_usd_micros: fin_domain::Quantity::zero(),
            observed_at: Utc::now().to_rfc3339(),
            source: quotes
                .iter()
                .map(|q| q.source.as_str())
                .collect::<Vec<_>>()
                .join(";"),
            prices: prices_vec,
        },
        cost,
    ))
}

pub(crate) fn valuation_asset_price(
    asset: &AssetId,
    quote: &fin_pricing::UsdQuote,
) -> Result<fin_domain::AssetPrice> {
    Ok(fin_domain::AssetPrice {
        asset: asset.clone(),
        usd_micros_per_unit: fin_domain::Quantity::from_decimal(&quote.usd_per_unit, 36)?,
        // Numerator is USD-per-whole-unit scaled by 10^36. Keep the matching
        // 30-place denominator scale so value = atoms * price * 10^6 / 10^d.
        unit_atoms: fin_domain::Quantity::new(
            "1".to_owned() + &"0".repeat(usize::from(asset.decimals) + 30),
        )?,
        observed_at: quote.updated_at.clone(),
        source: quote.source.clone(),
    })
}

fn swap_operation_definitions() -> Vec<Definition> {
    let schema = swap::definition_input_schema();
    let quote_schema = json!({"type":"object","additionalProperties":false,"required":["wallet","network","cluster","input_mint","output_mint","amount_atoms"],"properties":{"wallet":{"type":"string","minLength":1},"network":{"const":"solana"},"cluster":{"const":"mainnet-beta"},"input_mint":{"type":"string"},"output_mint":{"type":"string"},"amount_atoms":{"type":"string","pattern":"^[0-9]+$"},"route":{"type":"string","enum":["meta_aggregator","router"]},"maximum_slippage_bps":{"type":"integer","minimum":0,"maximum":10000}}});
    let output = swap::definition_output_schema();
    let mut definitions: Vec<_> = [OP_QUOTE, OP_SWAP]
        .into_iter()
        .map(|name| Definition {
            name: name.into(),
            description: if name == OP_QUOTE {
                "Get a current exact-mint Jupiter quote on Solana mainnet-beta.".into()
            } else {
                "Prepare and submit one bounded Jupiter Swap v2 conversion through an enrolled Solana wallet.".into()
            },
            kind: if name == OP_QUOTE { OperationKind::Read } else { OperationKind::Write },
            execution_actor: if name == OP_QUOTE { ExecutionActor::AgentReadable } else { ExecutionActor::AgentInvocable },
            input_schema: if name == OP_QUOTE { quote_schema.clone() } else { schema.clone() },
            output_schema: output.clone(),
            error_schema: json!({"type":"object","required":["code","stage","message","next_action"]}),
            examples: vec![if name == OP_QUOTE {
                json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","input_mint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","output_mint":"2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo","amount_atoms":"1000000","route":"meta_aggregator"})
            } else {
                json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","input_mint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","output_mint":"2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo","amount_atoms":"1000000","minimum_output_atoms":"900000","maximum_slippage_bps":100,"maximum_platform_fee_bps":0,"maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0,"route":"meta_aggregator"})
            }],
            coverage: "Exact input SOL/SPL to SOL/SPL Swap v2 through Meta-Aggregator /order+/execute or Router /build+RPC. DFlow instructions can be decoded for diagnostics, but writes remain unavailable until its action-account graph proves the exact wallet input source and output destination; multi-action and DynamicRoute formats also remain unreviewed.".into(),
            completion: if name == OP_QUOTE { "Provider quote only; it creates no financial submission.".into() } else { "Completed only after finalized chain evidence proves the exact input debit, minimum output credit and bounded network fee.".into() },
            semantics: json!({"network":NETWORK,"cluster":CLUSTER,"mainnet":true,"route_choice":["meta_aggregator","router"],"keyless_default":true,"uncertain_submission":"reconcile_original_identity_no_retry","quote_is_settlement":false,"dflow":{"instruction_idl":"pinned diagnostic decoder","write_status":"fail_closed_unreviewed","required_review":"bind each action to exact wallet source, intermediate and destination accounts; verify total debit and fees","currently_unreviewed":["action_account_graph","multi_action_routes","DynamicRouteV1","nonzero_orchestrator_flags"]}}),
        })
        .collect();
    if let Some(swap) = definitions
        .iter_mut()
        .find(|definition| definition.name == OP_SWAP)
    {
        swap.semantics["plan_allowance_seconds"] =
            json!(fin_domain::plan_timing::ONCHAIN_ALLOWANCE_SECONDS);
    }
    definitions
}

/// Offline operation catalog for the complete Jupiter application crate.
/// Runtime adapters still return only their own definitions to avoid duplicate
/// registration when the broker composes swap, trigger, JLP and perps adapters.
pub fn operation_definitions() -> Vec<Definition> {
    let mut definitions = swap_operation_definitions();
    definitions.extend(crate::jlp::operation_definitions());
    definitions.extend(crate::perps::operation_definitions());
    definitions.extend(crate::trigger::operation_definitions());
    definitions
}

pub(crate) fn verify_failed_cost(
    record: &OperationRecord,
    submission: &Submission,
    signature: &str,
    slot: u64,
    fee: u64,
    proof: &fin_solana::transaction::FinalizedReceiptProof,
) -> Result<fin_domain::VerifiedEffects> {
    let binding = &record
        .preview
        .as_ref()
        .ok_or_else(bad_provider_response)?
        .binding;
    let wallet = binding
        .get("wallet_address")
        .and_then(Value::as_str)
        .ok_or_else(bad_provider_response)?;
    if !proof.failed || proof.fee_lamports != fee {
        return Err(bad_provider_response());
    }
    let wallet_is_fee_payer = proof.account_keys.first().map(String::as_str) == Some(wallet);
    let wallet_lamport_delta = proof.lamport_delta(wallet)?;
    if wallet_is_fee_payer {
        if !wallet_lamport_delta.is_negative()
            || wallet_lamport_delta.magnitude().as_str() != fee.to_string()
        {
            return Err(bad_provider_response());
        }
    } else if !wallet_lamport_delta.is_zero() {
        return Err(bad_provider_response());
    }
    for mint in wallet_token_mints(proof, wallet)? {
        if !proof.token_delta(wallet, &mint)?.is_zero() {
            return Err(FinError::new(
                "JUPITER_FAILED_TRANSACTION_ASSET_MOVEMENT",
                "reconciliation",
                "A failed transaction changed a wallet token balance despite the expected atomic rollback.",
                "Keep the operation unresolved and inspect the finalized transaction account effects.",
            ));
        }
    }
    let resource = ResourceId(format!("solana:mainnet-beta:{wallet}"));
    let asset = AssetId {
        network: NETWORK.into(),
        reference: "native".into(),
        decimals: 9,
    };
    let evidence = json!({"cluster":CLUSTER,"signature":signature,"slot":slot,"network_fee_lamports":fee,"failed_transaction":true,"proof":proof});
    let mut verified = fin_domain::VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects: if wallet_is_fee_payer && fee > 0 {
            vec![fin_domain::FinancialEffect::Cost {
                location: AccountingLocation::Cash { resource },
                amount: Amount {
                    asset,
                    atoms: fin_domain::Quantity::from_u64(fee),
                },
                category: "solana_network_fee".into(),
                treatment: CostTreatment::AdditionalDebit,
            }]
        } else {
            Vec::new()
        },
        evidence,
        observed_at: Utc::now().to_rfc3339(),
        proof_digest: String::new(),
    };
    verified.proof_digest = fin_domain::verified_effect_digest(&verified)?;
    Ok(verified)
}

fn wallet_token_mints(
    proof: &fin_solana::transaction::FinalizedReceiptProof,
    wallet: &str,
) -> Result<std::collections::BTreeSet<String>> {
    let mut mints = std::collections::BTreeSet::new();
    for row in proof
        .pre_token_balances
        .iter()
        .chain(proof.post_token_balances.iter())
    {
        if row.get("owner").and_then(Value::as_str) == Some(wallet) {
            mints.insert(
                row.get("mint")
                    .and_then(Value::as_str)
                    .ok_or_else(bad_provider_response)?
                    .to_owned(),
            );
        }
    }
    Ok(mints)
}

pub(crate) fn verify_not_dispatched(
    submission: &Submission,
    reason_code: &str,
    reason: &str,
) -> Result<fin_domain::VerifiedEffects> {
    let evidence = json!({"state":"not_dispatched","reason_code":reason_code,"reason":reason,"financial_effects":[]});
    let mut verified = fin_domain::VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects: Vec::new(),
        evidence,
        observed_at: Utc::now().to_rfc3339(),
        proof_digest: String::new(),
    };
    verified.proof_digest = fin_domain::verified_effect_digest(&verified)?;
    Ok(verified)
}

fn candidate_mint(mint: &str) -> Result<String> {
    if mint == "native" {
        Ok(swap::WRAPPED_SOL_MINT.into())
    } else {
        fin_solana::parse_address(mint)
    }
}

fn normalize_asset(mint: &str) -> String {
    if mint == "native" {
        "native".into()
    } else {
        mint.into()
    }
}

fn solana_asset_label(mint: &str, decimals: u8) -> &str {
    match (mint, decimals) {
        ("native", 9) => "SOL",
        ("So11111111111111111111111111111111111111112", 9) => "WSOL",
        ("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", 6) => "USDC",
        ("2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo", 6) => "PYUSD",
        ("BPxxfRCXkUVhig4HS1Lh7kZqV6SPJhzfEk4x6fVBjPCy", 9) => "BP",
        ("DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263", 5) => "BONK",
        // Unknown assets remain identifiable by their exact mint. Never trust
        // names returned by a quote service or supplied by the requester.
        _ => mint,
    }
}

fn display_atomic_amount(atoms: u64, decimals: u8) -> String {
    if decimals == 0 {
        return atoms.to_string();
    }
    let scale = 10u64.checked_pow(u32::from(decimals));
    let Some(scale) = scale else {
        return format!("{atoms} atoms");
    };
    let whole = atoms / scale;
    let fraction = atoms % scale;
    if fraction == 0 {
        return whole.to_string();
    }
    let raw = format!("{fraction:0width$}", width = usize::from(decimals));
    format!("{whole}.{}", raw.trim_end_matches('0'))
}

fn format_basis_points(bps: u16) -> String {
    let whole = bps / 100;
    let fraction = bps % 100;
    if fraction == 0 {
        whole.to_string()
    } else if fraction.is_multiple_of(10) {
        format!("{whole}.{}", fraction / 10)
    } else {
        format!("{whole}.{fraction:02}")
    }
}

#[cfg(test)]
mod swap_presentation_tests {
    use super::*;

    #[test]
    fn exact_solana_asset_labels_and_amounts_are_bound_to_mint_and_decimals() {
        assert_eq!(
            format!(
                "{} {} for at least {} {}",
                display_atomic_amount(123_456_789_123, 9),
                solana_asset_label("BPxxfRCXkUVhig4HS1Lh7kZqV6SPJhzfEk4x6fVBjPCy", 9),
                display_atomic_amount(567_890_000, 5),
                solana_asset_label("DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263", 5),
            ),
            "123.456789123 BP for at least 5678.9 BONK"
        );
        assert_eq!(
            solana_asset_label("UnknownExactMint", 0),
            "UnknownExactMint"
        );
        assert_eq!(display_atomic_amount(1234, 0), "1234");
        assert_eq!(
            solana_asset_label("DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263", 9),
            "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263"
        );
        assert_eq!(format_basis_points(10), "0.1");
    }
}

fn frozen_candidate_cache_key(prepared: &PreparedOperation) -> Option<[u8; 32]> {
    let mut binding = prepared.binding.clone();
    if let Some(binding) = binding.as_object_mut() {
        binding.remove(fin_domain::financial_plans::PLAN_METADATA_KEY);
        binding.remove(fin_domain::boundaries::BOUNDARY_METADATA_KEY);
    }
    // Key every transaction-affecting input and the original preview lifetime.
    // Valuation and presentation may be enriched by the engine after revalidation;
    // authority/freshness are independently checked by its handoff and journal.
    let encoded = serde_json::to_vec(&(
        &prepared.operation,
        prepared.operation_version,
        &prepared.input,
        binding,
        &prepared.intent,
        &prepared.observed_at,
        &prepared.expires_at,
    ))
    .ok()?;
    Some(Sha256::digest(encoded).into())
}

fn parse_atoms(value: &str) -> Result<u64> {
    if value.is_empty() || value.len() > 20 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad_provider_response());
    }
    value.parse().map_err(|_| bad_provider_response())
}

fn check(name: &str) -> Check {
    Check {
        name: name.into(),
        status: CheckStatus::Passed,
        detail: "Verified against the frozen provider transaction and mainnet-beta runtime.".into(),
    }
}

fn unknown_operation() -> FinError {
    FinError::new(
        "JUPITER_OPERATION_UNKNOWN",
        "operation",
        "This Jupiter operation is not implemented by the selected adapter.",
        "Inspect Fin discovery for the supported Jupiter operations.",
    )
}

fn bad_provider_response() -> FinError {
    FinError::new(
        "JUPITER_RESPONSE_INVALID",
        "prepare",
        "Jupiter returned incomplete or invalid transaction data.",
        "Request a fresh quote and review its exact transaction before approval.",
    )
}

#[cfg(test)]
mod validation_diagnostic_tests {
    use super::*;

    #[test]
    fn unreviewed_diagnostics_add_only_stage_and_allowlisted_router() {
        let error = validation_router_context(
            validation_stage_if_unreviewed(unsafe_instruction(), "jupiter_route_accounts"),
            Some("metis"),
        );
        assert_eq!(error.code, "JUPITER_INSTRUCTION_UNREVIEWED");
        assert_eq!(
            error.details.as_deref().unwrap(),
            &json!({"validation_stage":"jupiter_route_accounts","router":"metis"})
        );
    }

    #[test]
    fn diagnostics_preserve_specific_stage_and_do_not_modify_other_errors() {
        let specific = unsafe_instruction_at("platform-fee quote binding");
        let specific = validation_router_context(
            validation_stage_if_unreviewed(specific, "fallback"),
            Some("unexpected-provider-value"),
        );
        assert_eq!(
            specific.details.as_deref().unwrap(),
            &json!({"validation_stage":"platform-fee quote binding","router":"unknown"})
        );

        let unrelated = validation_stage_if_unreviewed(bad_provider_response(), "fallback");
        assert_eq!(unrelated.code, "JUPITER_RESPONSE_INVALID");
        assert!(unrelated.details.is_none());
    }

    #[test]
    fn rfq_diagnostic_details_only_include_bounded_shape_and_privileges() {
        let error = unsafe_instruction_at_with_details(
            "RFQ fixed account privileges",
            json!({"account_index": 8, "signer": false, "writable": true}),
        );
        assert_eq!(
            error.details.as_deref().unwrap(),
            &json!({
                "validation_stage":"RFQ fixed account privileges",
                "account_index":8,
                "signer":false,
                "writable":true
            })
        );
        let serialized = serde_json::to_string(error.details.as_deref().unwrap()).unwrap();
        assert!(!serialized.contains("address"));
        assert!(!serialized.contains("instruction_data"));
    }
}

#[cfg(test)]
mod order_engine_privilege_tests {
    use super::*;

    #[test]
    fn native_optional_sentinel_is_readonly_and_spl_accounts_remain_writable() {
        let meta = |address: &str, signer, writable| NormalizedAccountMeta {
            address: address.to_owned(),
            signer,
            writable,
        };
        let mut accounts = vec![meta("account", false, true); 11];
        accounts[2] = meta(ORDER_ENGINE_PROGRAM, false, false);
        accounts[3] = meta("input-maker-account", false, true);
        accounts[4] = meta("output-wallet-account", false, true);
        accounts[5] = meta("output-maker-account", false, true);
        accounts[6] = meta("input-mint", false, false);

        assert!(
            validate_order_engine_account_privileges(&accounts, "native", "output-mint",).is_ok()
        );

        let mut native_output = accounts.clone();
        native_output[2] = meta("input-wallet-account", false, true);
        native_output[4] = meta(ORDER_ENGINE_PROGRAM, false, false);
        assert!(
            validate_order_engine_account_privileges(&native_output, "input-mint", "native",)
                .is_ok()
        );

        // An explicit WSOL SPL request does not authorize the native sentinel.
        assert!(
            validate_order_engine_account_privileges(
                &accounts,
                swap::WRAPPED_SOL_MINT,
                "output-mint",
            )
            .is_err()
        );

        let mut maker_sentinel = accounts.clone();
        maker_sentinel[3] = meta(ORDER_ENGINE_PROGRAM, false, false);
        assert!(
            validate_order_engine_account_privileges(&maker_sentinel, "native", "output-mint",)
                .is_ok()
        );

        let mut writable_sentinel = accounts.clone();
        writable_sentinel[2].writable = true;
        assert!(
            validate_order_engine_account_privileges(&writable_sentinel, "native", "output-mint",)
                .is_err()
        );

        let mut readonly_spl_account = accounts.clone();
        readonly_spl_account[4].writable = false;
        assert!(
            validate_order_engine_account_privileges(
                &readonly_spl_account,
                "native",
                "output-mint",
            )
            .is_err()
        );

        let mut sentinel_signer = accounts;
        sentinel_signer[2].signer = true;
        assert!(
            validate_order_engine_account_privileges(&sentinel_signer, "native", "output-mint",)
                .is_err()
        );
    }
}

#[cfg(test)]
mod order_engine_fill_data_tests {
    use super::*;

    fn fill_data(metadata: Option<(u16, u8, u16)>) -> Vec<u8> {
        let mut data = vec![168, 96, 183, 163, 92, 10, 40, 160];
        data.extend_from_slice(&1_000u64.to_le_bytes());
        data.extend_from_slice(&2_000u64.to_le_bytes());
        data.extend_from_slice(&1_800_000_000i64.to_le_bytes());
        if let Some((fee_bps, flags, integrator_fee_bps)) = metadata {
            data.extend_from_slice(&fee_bps.to_le_bytes());
            data.push(flags);
            data.extend_from_slice(&integrator_fee_bps.to_le_bytes());
        }
        data
    }

    #[test]
    fn fill_decoder_consumes_pinned_legacy_and_current_metadata_formats() {
        let legacy = decode_order_engine_fill_data(&fill_data(None)).unwrap();
        assert_eq!(legacy.input_amount, 1_000);
        assert_eq!(legacy.output_amount, 2_000);
        assert_eq!(legacy.expire_at, 1_800_000_000);
        assert_eq!(legacy.fee_bps, None);
        validate_order_engine_fill_metadata(legacy, 0).unwrap();
        assert!(validate_order_engine_fill_metadata(legacy, 1).is_err());

        let zero_fee = decode_order_engine_fill_data(&fill_data(Some((0, 0, 0)))).unwrap();
        validate_order_engine_fill_metadata(zero_fee, 0).unwrap();
        let at_approved_max = decode_order_engine_fill_data(&fill_data(Some((2, 0, 0)))).unwrap();
        validate_order_engine_fill_metadata(at_approved_max, 2).unwrap();
        assert!(validate_order_engine_fill_metadata(at_approved_max, 1).is_err());

        let output_fee_side = decode_order_engine_fill_data(&fill_data(Some((0, 2, 0)))).unwrap();
        validate_order_engine_fill_metadata(output_fee_side, 0).unwrap();
    }

    #[test]
    fn fill_decoder_rejects_unknown_lengths_and_unapproved_metadata() {
        let mut extra_trailing_byte = fill_data(Some((0, 0, 0)));
        extra_trailing_byte.push(0);
        assert!(decode_order_engine_fill_data(&extra_trailing_byte).is_err());
        assert!(decode_order_engine_fill_data(&fill_data(None)[..31]).is_err());

        for flags in [1, 4, 255] {
            let decoded = decode_order_engine_fill_data(&fill_data(Some((0, flags, 0)))).unwrap();
            assert!(validate_order_engine_fill_metadata(decoded, 0).is_err());
        }
        let integrator_fee = decode_order_engine_fill_data(&fill_data(Some((0, 0, 1)))).unwrap();
        assert!(validate_order_engine_fill_metadata(integrator_fee, 0).is_err());

        let mut wrong_discriminator = fill_data(None);
        wrong_discriminator[0] ^= 1;
        assert!(decode_order_engine_fill_data(&wrong_discriminator).is_err());
    }
}

#[cfg(test)]
mod frozen_candidate_cache_tests {
    use super::*;

    fn prepared() -> PreparedOperation {
        PreparedOperation {
            operation: OP_SWAP.into(),
            input: json!({"wallet":"wallet-a","amount_atoms":"100"}),
            binding: json!({"message_sha256":"message-a","request_id":"request-a","transaction_base64":"unsigned-tx"}),
            summary: "synthetic swap".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: "2026-10-13T10:00:00Z".into(),
            expires_at: "2026-10-13T10:00:20Z".into(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        }
    }

    #[test]
    fn cache_key_binds_candidate_input_intent_and_freshness() {
        let base = prepared();
        let key = frozen_candidate_cache_key(&base).unwrap();

        let mut changed_input = base.clone();
        changed_input.input["amount_atoms"] = json!("101");
        assert_ne!(frozen_candidate_cache_key(&changed_input), Some(key));

        let mut changed_message = base.clone();
        changed_message.binding["message_sha256"] = json!("message-b");
        assert_ne!(frozen_candidate_cache_key(&changed_message), Some(key));

        let mut changed_intent = base.clone();
        changed_intent
            .intent
            .locks
            .push(ResourceId("solana:wallet-a".into()));
        assert_ne!(frozen_candidate_cache_key(&changed_intent), Some(key));

        let mut changed_expiry = base.clone();
        changed_expiry.expires_at = "2026-10-13T10:00:19Z".into();
        assert_ne!(frozen_candidate_cache_key(&changed_expiry), Some(key));

        let mut broker_marked = base;
        broker_marked.binding[fin_domain::financial_plans::PLAN_METADATA_KEY] =
            json!({"plan_id":"plan"});
        broker_marked.binding[fin_domain::boundaries::BOUNDARY_METADATA_KEY] =
            json!({"authorization_id":"boundary"});
        assert_eq!(frozen_candidate_cache_key(&broker_marked), Some(key));

        let mut enriched = broker_marked;
        enriched.valuation = Some(Valuation {
            movement_usd_micros: fin_domain::Quantity::zero(),
            maximum_fee_usd_micros: fin_domain::Quantity::zero(),
            additional_allocation_usd_micros: fin_domain::Quantity::zero(),
            observed_at: "2026-10-13T10:00:01Z".into(),
            source: "engine-enrichment".into(),
            prices: vec![],
        });
        enriched.summary = "presentation enrichment".into();
        assert_eq!(frozen_candidate_cache_key(&enriched), Some(key));
    }

    #[test]
    fn one_shot_cache_is_bounded_expires_monotonically_and_consumes_once() {
        let cache = OneShotCache::default();
        let now = Instant::now();
        let key = |index: u8| [index; 32];
        for index in 0..FROZEN_CACHE_CAPACITY as u8 {
            cache.insert_at(key(index), index, now);
        }
        cache.insert_at(key(4), 4, now);
        assert_eq!(cache.take_at(key(0), now), None);
        assert_eq!(cache.take_at(key(1), now), Some(1));
        assert_eq!(cache.take_at(key(1), now), None);

        cache.insert_at(key(5), 5, now);
        assert_eq!(
            cache.take_at(key(5), now + FROZEN_CACHE_TTL),
            None,
            "an expired cache entry must be discarded"
        );
    }
}

#[cfg(test)]
mod router_build_response_tests {
    use super::*;

    #[test]
    fn meta_recovery_is_limited_to_typed_instruction_format_failures() {
        let unsupported_jupiter = validation_stage_if_unreviewed(
            swap::decode_swap_instruction(&[0; 8]).unwrap_err(),
            "jupiter_route_instruction",
        );
        assert!(meta_format_rejection(&unsupported_jupiter));

        let unsupported_dflow = mark_meta_route_format_failure(
            crate::dflow::require_reviewed_format(&crate::dflow::DecodedDflowSwap {
                instruction: "swap2".into(),
                quoted_output: 100,
                slippage_bps: 10,
                platform_fee_bps: 0,
                positive_slippage_fee_limit_pct: 0,
                action_count: 1,
                swap_input_atoms: Some(100),
                transfer_fee_atoms: 0,
                wrap_sol_lamports: None,
                unsupported_actions: vec!["NewAction".into()],
                output_native: false,
                destination_index: None,
            })
            .unwrap_err(),
            "dflow_route_instruction",
        );
        assert!(meta_format_rejection(&unsupported_dflow));

        assert!(!meta_format_rejection(&unsafe_instruction_at(
            "platform-fee quote binding"
        )));
        assert!(!meta_format_rejection(&unsafe_instruction_at(
            "jupiter_route_instruction"
        )));
        assert!(!meta_format_rejection(&unsafe_instruction_at(
            "associated-token mint binding"
        )));
        assert!(!meta_format_rejection(&unsafe_instruction_at(
            "associated-token PDA binding"
        )));
        assert!(!meta_format_rejection(&FinError::new(
            "JUPITER_ORDER_EMPTY",
            "prepare",
            "empty",
            "retry",
        )));
        assert!(!meta_format_rejection(&FinError::new(
            "JUPITER_INSTRUCTION_UNSUPPORTED",
            "prepare",
            "unsupported",
            "retry",
        )));

        let invalid_base64 = FinError::new(
            "SOLANA_TRANSACTION_INVALID",
            "prepare",
            "malformed",
            "retry",
        )
        .with_details(json!({
            "transaction_stage":"decode",
            "reason":"invalid_base64"
        }));
        assert!(meta_format_rejection(&invalid_base64));
        let unsupported_version = FinError::new(
            "SOLANA_TX_VERSION_UNSUPPORTED",
            "prepare",
            "unsupported",
            "retry",
        )
        .with_details(json!({"transaction_stage":"decode","transaction_version":1}));
        assert!(meta_format_rejection(&unsupported_version));
        let sanitize_error = FinError::new(
            "SOLANA_TRANSACTION_INVALID",
            "prepare",
            "malformed",
            "retry",
        )
        .with_details(json!({
            "transaction_stage":"sanitize",
            "reason":"transaction_sanitize_failed"
        }));
        assert!(meta_format_rejection(&sanitize_error));
        let wire_size_compile_error = FinError::new(
            "SOLANA_TRANSACTION_INVALID",
            "prepare",
            "too large",
            "retry",
        )
        .with_details(json!({
            "transaction_stage":"compile",
            "reason":"wire_size_limit_exceeded"
        }));
        assert!(!meta_format_rejection(&wire_size_compile_error));
        let unrelated_decode_error = FinError::new(
            "SOLANA_TRANSACTION_INVALID",
            "prepare",
            "malformed",
            "retry",
        )
        .with_details(json!({"transaction_stage":"decode","reason":"signature_invalid"}));
        assert!(!meta_format_rejection(&unrelated_decode_error));
    }

    #[test]
    fn meta_recovery_does_not_accept_unknown_engines_or_ignored_exclusions() {
        assert!(swap::is_meta_router("metis"));
        assert!(swap::is_meta_router("jupiterz"));
        assert!(swap::is_meta_router("dflow"));
        assert!(swap::is_meta_router("okx"));
        assert!(!swap::is_meta_router("unknown"));

        let error = meta_exclusion_ignored(Some("metis"));
        assert_eq!(error.code, "JUPITER_ROUTER_EXCLUSION_IGNORED");
        assert_eq!(
            error.details.as_deref().unwrap()["router"],
            Value::String("metis".into())
        );
    }

    struct RouteApi {
        replies: Mutex<VecDeque<Value>>,
        paths: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl JupiterApi for RouteApi {
        async fn request(
            &self,
            _host: JupiterHost,
            _method: Method,
            path: &str,
            _body: Option<Value>,
            _bucket: RateBucket,
            _priority: RequestPriority,
            _max_wait: Duration,
        ) -> Result<Value> {
            self.paths.lock().unwrap().push(path.to_owned());
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(bad_provider_response)
        }
    }

    struct RouteRpc;
    #[async_trait]
    impl fin_solana::SolanaRpc for RouteRpc {
        async fn request(&self, method: &str, _params: Value) -> Result<Value> {
            assert_eq!(
                method, "getSlot",
                "oversized builds must stop before simulation or dispatch"
            );
            Ok(json!(100))
        }
    }

    struct MetaRpc;
    #[async_trait]
    impl fin_solana::SolanaRpc for MetaRpc {
        async fn request(&self, method: &str, _params: Value) -> Result<Value> {
            match method {
                "getBlockHeight" => Ok(json!(100)),
                "isBlockhashValid" => Ok(json!(true)),
                other => panic!("unexpected Meta-Aggregator preparation RPC call: {other}"),
            }
        }
    }

    struct NoPrices;
    #[async_trait]
    impl UsdPriceSource for NoPrices {
        async fn quote(&self, _asset: &AssetId) -> Result<fin_pricing::UsdQuote> {
            panic!("candidate preparation does not value an invalid route")
        }
    }

    fn oversized_build(intent: &SwapIntent) -> Value {
        json!({
            "inputMint":intent.input_mint, "outputMint":intent.output_mint,
            "inAmount":intent.amount_atoms, "outAmount":"100", "otherAmountThreshold":"99",
            "swapMode":"ExactIn", "slippageBps":100,
            "computeBudgetInstructions":[], "setupInstructions":[],
            "swapInstruction":{
                "programId":bs58::encode([99u8;32]).into_string(),
                "accounts":(1..=10).map(|index| json!({
                    "pubkey":bs58::encode([index;32]).into_string(),
                    "isSigner":false,"isWritable":false
                })).collect::<Vec<_>>(),
                "data":STANDARD.encode(vec![0;1024]),
            },
            "cleanupInstruction":null,"otherInstructions":[],"tipInstruction":null,
            "addressesByLookupTableAddress":{},
            "blockhashWithMetadata":{"blockhash":vec![0;32],"lastValidBlockHeight":1000}
        })
    }

    fn route_fixture() -> (SwapIntent, Arc<SolanaWalletAdapter>) {
        // RFC 8032 public test vector, never a funded account.
        let key = hex::decode(concat!(
            "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        ))
        .unwrap();
        let signer =
            Arc::new(fin_solana::SolanaSigner::parse(&bs58::encode(key).into_string()).unwrap());
        let wallet = Arc::new(
            SolanaWalletAdapter::new(
                "synthetic",
                signer.address(),
                Some(signer.clone()),
                Arc::new(RouteRpc),
                Arc::new(NoPrices),
            )
            .unwrap(),
        );
        let intent = SwapIntent {
            wallet: "synthetic".into(),
            network: "solana".into(),
            input_mint: swap::WRAPPED_SOL_MINT.into(),
            output_mint: bs58::encode([60u8; 32]).into_string(),
            amount_atoms: "10".into(),
            minimum_output_atoms: "99".into(),
            maximum_slippage_bps: 100,
            maximum_platform_fee_bps: 0,
            maximum_network_fee_lamports: 5000,
            maximum_account_funding_lamports: 0,
            route: SwapRoute::Router,
        };
        (intent, wallet)
    }

    fn meta_fixture() -> (SwapIntent, Arc<SolanaWalletAdapter>) {
        // RFC 8032 public test vector, never a funded account.
        let key = hex::decode(concat!(
            "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        ))
        .unwrap();
        let signer =
            Arc::new(fin_solana::SolanaSigner::parse(&bs58::encode(key).into_string()).unwrap());
        let wallet = Arc::new(
            SolanaWalletAdapter::new(
                "synthetic",
                signer.address(),
                Some(signer.clone()),
                Arc::new(MetaRpc),
                Arc::new(NoPrices),
            )
            .unwrap(),
        );
        let intent = SwapIntent {
            wallet: "synthetic".into(),
            network: "solana".into(),
            input_mint: swap::WRAPPED_SOL_MINT.into(),
            output_mint: bs58::encode([60u8; 32]).into_string(),
            amount_atoms: "10".into(),
            minimum_output_atoms: "90".into(),
            maximum_slippage_bps: 100,
            maximum_platform_fee_bps: 0,
            maximum_network_fee_lamports: 5000,
            maximum_account_funding_lamports: 0,
            route: SwapRoute::MetaAggregator,
        };
        (intent, wallet)
    }

    fn serialized_route_transaction(wallet: &str, data: &[u8]) -> String {
        fn shortvec(mut value: usize, output: &mut Vec<u8>) {
            loop {
                let mut elem = (value & 0x7f) as u8;
                value >>= 7;
                if value != 0 {
                    elem |= 0x80;
                }
                output.push(elem);
                if value == 0 {
                    break;
                }
            }
        }

        let wallet = bs58::decode(wallet).into_vec().unwrap();
        let program = bs58::decode(swap::JUPITER_V6_PROGRAM).into_vec().unwrap();
        let mut message = vec![1, 0, 0]; // one wallet signer and fee payer
        shortvec(2, &mut message);
        message.extend(wallet);
        message.extend(program);
        message.extend([5; 32]); // fixed synthetic recent blockhash
        shortvec(1, &mut message);
        message.push(1); // program id is account key 1
        shortvec(0, &mut message); // route accounts are irrelevant to decoder failure
        shortvec(data.len(), &mut message);
        message.extend(data);

        let mut transaction = Vec::with_capacity(message.len() + 65);
        shortvec(1, &mut transaction);
        transaction.extend([0; 64]); // empty wallet signature slot
        transaction.extend(message);
        STANDARD.encode(transaction)
    }

    fn unsupported_route_data() -> Vec<u8> {
        vec![0; 8]
    }

    fn exact_in_route_data(input: u64) -> Vec<u8> {
        exact_in_route_data_with_slippage(input, 100)
    }

    fn exact_in_route_data_with_slippage(input: u64, slippage_bps: u16) -> Vec<u8> {
        let mut data = vec![229, 23, 203, 151, 122, 227, 173, 42];
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&[0, 100, 0, 1]); // one Saber route step
        data.extend_from_slice(&input.to_le_bytes());
        data.extend_from_slice(&100u64.to_le_bytes());
        data.extend_from_slice(&slippage_bps.to_le_bytes());
        data.push(0); // no platform fee
        data
    }

    fn meta_order(wallet: &SolanaWalletAdapter, router: &str, data: &[u8]) -> Value {
        json!({
            "transaction": serialized_route_transaction(wallet.address(), data),
            "requestId": format!("request-{router}"),
            "outAmount": "100",
            "router": router,
            "mode": "ultra",
            "lastValidBlockHeight": 1000,
            "expireAt": "2030-01-01T00:00:00Z",
            "feeBps": 0,
            "feeMint": null
        })
    }

    fn malformed_meta_order(wallet: &SolanaWalletAdapter, router: &str) -> Value {
        let mut order = meta_order(wallet, router, &unsupported_route_data());
        order["transaction"] = json!("not_base64!");
        order
    }

    #[tokio::test]
    async fn meta_recovery_exhausts_only_the_four_known_engines() {
        let (intent, wallet) = meta_fixture();
        let replies = ["metis", "jupiterz", "dflow", "okx"]
            .into_iter()
            .map(|router| malformed_meta_order(&wallet, router))
            .collect();
        let api = Arc::new(RouteApi {
            replies: Mutex::new(replies),
            paths: Mutex::new(vec![]),
        });
        let adapter = JupiterAdapter::with_api(
            BTreeMap::from([("synthetic".into(), wallet.clone())]),
            api.clone(),
        )
        .unwrap();

        let error = adapter
            .prepare_candidate(&intent, &wallet)
            .await
            .expect_err("all known engines return unsupported route data");
        assert_eq!(error.code, "SOLANA_TRANSACTION_INVALID");
        let details = error.details.as_deref().unwrap();
        assert_eq!(details["transaction_stage"], "decode");
        assert_eq!(details["reason"], "invalid_base64");
        assert_eq!(
            details["recovery_attempted_routers"],
            json!(["metis", "jupiterz", "dflow", "okx"])
        );
        assert_eq!(api.paths.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn meta_recovery_fails_when_jupiter_ignores_an_engine_exclusion() {
        let (intent, wallet) = meta_fixture();
        let api = Arc::new(RouteApi {
            replies: Mutex::new(VecDeque::from([
                malformed_meta_order(&wallet, "metis"),
                malformed_meta_order(&wallet, "metis"),
            ])),
            paths: Mutex::new(vec![]),
        });
        let adapter = JupiterAdapter::with_api(
            BTreeMap::from([("synthetic".into(), wallet.clone())]),
            api.clone(),
        )
        .unwrap();

        let error = adapter
            .prepare_candidate(&intent, &wallet)
            .await
            .expect_err("excluded engine must not be accepted");
        assert_eq!(error.code, "JUPITER_ROUTER_EXCLUSION_IGNORED");
        let paths = api.paths.lock().unwrap();
        assert_eq!(paths.len(), 2);
        assert!(paths[1].contains("excludeRouters=metis"));
        assert!(paths[1].contains("amount=10&"));
        assert!(paths[1].contains(&format!("taker={}", wallet.address())));
    }

    #[tokio::test]
    async fn meta_replacement_with_wrong_input_binding_stops_before_approval_or_send() {
        let (intent, wallet) = meta_fixture();
        let api = Arc::new(RouteApi {
            replies: Mutex::new(VecDeque::from([
                malformed_meta_order(&wallet, "metis"),
                meta_order(&wallet, "jupiterz", &exact_in_route_data(11)),
            ])),
            paths: Mutex::new(vec![]),
        });
        let adapter = JupiterAdapter::with_api(
            BTreeMap::from([("synthetic".into(), wallet.clone())]),
            api.clone(),
        )
        .unwrap();

        let error = adapter
            .prepare_candidate(&intent, &wallet)
            .await
            .expect_err("replacement must retain the exact approved input");
        assert_eq!(error.code, "JUPITER_INPUT_BINDING_MISMATCH");
        let paths = api.paths.lock().unwrap();
        assert_eq!(
            paths.len(),
            2,
            "invalid replacement must not cause another order"
        );
        assert!(paths[1].contains("amount=10&"));
        assert!(paths[1].contains("excludeRouters=metis"));
    }

    #[tokio::test]
    async fn meta_replacement_quote_below_the_original_minimum_stops_before_review() {
        let (intent, wallet) = meta_fixture();
        let mut below_minimum = meta_order(&wallet, "jupiterz", &exact_in_route_data(10));
        below_minimum["outAmount"] = json!("89");
        let api = Arc::new(RouteApi {
            replies: Mutex::new(VecDeque::from([
                malformed_meta_order(&wallet, "metis"),
                below_minimum,
            ])),
            paths: Mutex::new(vec![]),
        });
        let adapter = JupiterAdapter::with_api(
            BTreeMap::from([("synthetic".into(), wallet.clone())]),
            api.clone(),
        )
        .unwrap();

        let error = adapter
            .prepare_candidate(&intent, &wallet)
            .await
            .expect_err("replacement quote must preserve the approved minimum output");
        assert_eq!(error.code, "JUPITER_MINIMUM_OUTPUT_UNMET");
        let paths = api.paths.lock().unwrap();
        assert_eq!(
            paths.len(),
            2,
            "below-minimum quote must not be re-requested"
        );
        assert!(paths[1].contains("amount=10&"));
        assert!(paths[1].contains("excludeRouters=metis"));
    }

    #[tokio::test]
    async fn meta_order_requests_one_tighter_floor_order_then_keeps_final_account_checks() {
        let (mut intent, wallet) = meta_fixture();
        intent.minimum_output_atoms = "100".into();
        let api = Arc::new(RouteApi {
            replies: Mutex::new(VecDeque::from([
                meta_order(
                    &wallet,
                    "metis",
                    &exact_in_route_data_with_slippage(10, 100),
                ),
                meta_order(&wallet, "metis", &exact_in_route_data_with_slippage(10, 0)),
            ])),
            paths: Mutex::new(vec![]),
        });
        let adapter = JupiterAdapter::with_api(
            BTreeMap::from([("synthetic".into(), wallet.clone())]),
            api.clone(),
        )
        .unwrap();

        let error = adapter
            .prepare_candidate(&intent, &wallet)
            .await
            .expect_err("the synthetic route has no reviewed Jupiter route-account graph");
        assert_eq!(error.code, "JUPITER_INSTRUCTION_UNREVIEWED");
        assert_eq!(
            error.details.as_deref().unwrap()["validation_stage"],
            "jupiter_route_accounts"
        );
        let paths = api.paths.lock().unwrap();
        assert_eq!(paths.len(), 2, "at most one floor retry is allowed");
        assert!(paths[0].contains("amount=10&taker="));
        assert!(paths[0].contains("slippageBps=100"));
        assert!(paths[1].contains("amount=10&taker="));
        assert!(paths[1].contains("slippageBps=0"));
        assert!(!paths[1].contains("excludeRouters="));
    }

    #[tokio::test]
    async fn meta_floor_retry_is_final_instruction_checked_and_never_repeated() {
        let (mut intent, wallet) = meta_fixture();
        intent.minimum_output_atoms = "100".into();
        let api = Arc::new(RouteApi {
            replies: Mutex::new(VecDeque::from([
                meta_order(
                    &wallet,
                    "metis",
                    &exact_in_route_data_with_slippage(10, 100),
                ),
                // The API honored the 0 bps request in the URL only
                // incorrectly; the returned on-chain instruction remains at
                // 100 bps, so final decoded validation must reject it.
                meta_order(
                    &wallet,
                    "metis",
                    &exact_in_route_data_with_slippage(10, 100),
                ),
            ])),
            paths: Mutex::new(vec![]),
        });
        let adapter = JupiterAdapter::with_api(
            BTreeMap::from([("synthetic".into(), wallet.clone())]),
            api.clone(),
        )
        .unwrap();

        let error = adapter
            .prepare_candidate(&intent, &wallet)
            .await
            .expect_err("the returned instruction still violates the absolute output floor");
        assert_eq!(error.code, "JUPITER_MINIMUM_OUTPUT_UNPROTECTED");
        let paths = api.paths.lock().unwrap();
        assert_eq!(paths.len(), 2, "there must be no second floor retry");
        assert!(paths[0].contains("slippageBps=100"));
        assert!(paths[1].contains("slippageBps=0"));
    }

    #[tokio::test]
    async fn meta_floor_retry_budget_is_shared_across_engine_format_fallbacks() {
        let (mut intent, wallet) = meta_fixture();
        intent.minimum_output_atoms = "100".into();
        let api = Arc::new(RouteApi {
            replies: Mutex::new(VecDeque::from([
                malformed_meta_order(&wallet, "metis"),
                meta_order(
                    &wallet,
                    "jupiterz",
                    &exact_in_route_data_with_slippage(10, 100),
                ),
                meta_order(
                    &wallet,
                    "jupiterz",
                    &exact_in_route_data_with_slippage(10, 0),
                ),
            ])),
            paths: Mutex::new(vec![]),
        });
        let adapter = JupiterAdapter::with_api(
            BTreeMap::from([("synthetic".into(), wallet.clone())]),
            api.clone(),
        )
        .unwrap();

        let error = adapter
            .prepare_candidate(&intent, &wallet)
            .await
            .expect_err("the replacement still needs a complete reviewed account graph");
        assert_eq!(error.code, "JUPITER_INSTRUCTION_UNREVIEWED");
        let paths = api.paths.lock().unwrap();
        assert_eq!(paths.len(), 3);
        assert!(paths[0].contains("slippageBps=100"));
        assert!(paths[1].contains("slippageBps=100&excludeRouters=metis"));
        assert!(paths[2].contains("slippageBps=0&excludeRouters=metis"));
        assert_eq!(
            paths
                .iter()
                .filter(|path| path.contains("slippageBps=0"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn oversized_router_builds_request_smaller_routes_with_a_finite_limit() {
        let (intent, wallet) = route_fixture();
        let api = Arc::new(RouteApi {
            replies: Mutex::new(VecDeque::from(vec![oversized_build(&intent); 3])),
            paths: Mutex::new(vec![]),
        });
        let adapter = JupiterAdapter::with_api(
            BTreeMap::from([("synthetic".into(), wallet.clone())]),
            api.clone(),
        )
        .unwrap();
        let error = match adapter.prepare_candidate(&intent, &wallet).await {
            Err(error) => error,
            Ok(_) => panic!("every synthetic build exceeds wire size"),
        };
        assert!(router_build_is_oversized(&error));
        let paths = api.paths.lock().unwrap();
        assert_eq!(paths.len(), 3);
        assert!(!paths[0].contains("maxAccounts="));
        assert!(paths[1].ends_with("&maxAccounts=48"));
        assert!(paths[2].ends_with("&maxAccounts=32"));
        for path in paths.iter() {
            assert!(path.contains("amount=10&"));
            assert!(path.contains("slippageBps=100&"));
        }
    }

    #[tokio::test]
    async fn smaller_router_build_keeps_the_original_minimum_and_does_not_retry_other_errors() {
        let (intent, wallet) = route_fixture();
        let mut low_output = oversized_build(&intent);
        low_output["otherAmountThreshold"] = json!("98");
        let api = Arc::new(RouteApi {
            replies: Mutex::new(VecDeque::from([oversized_build(&intent), low_output])),
            paths: Mutex::new(vec![]),
        });
        let adapter = JupiterAdapter::with_api(
            BTreeMap::from([("synthetic".into(), wallet.clone())]),
            api.clone(),
        )
        .unwrap();
        let error = match adapter.prepare_candidate(&intent, &wallet).await {
            Err(error) => error,
            Ok(_) => panic!("the reduced route violates minimum output"),
        };
        assert_eq!(error.code, "JUPITER_BUILD_LIMIT_MISMATCH");
        assert_eq!(api.paths.lock().unwrap().len(), 2);

        let mut malformed = oversized_build(&intent);
        malformed["swapInstruction"]["data"] = json!("private-not-base64");
        let api = Arc::new(RouteApi {
            replies: Mutex::new(VecDeque::from([malformed])),
            paths: Mutex::new(vec![]),
        });
        let adapter = JupiterAdapter::with_api(
            BTreeMap::from([("synthetic".into(), wallet.clone())]),
            api.clone(),
        )
        .unwrap();
        assert!(adapter.prepare_candidate(&intent, &wallet).await.is_err());
        assert_eq!(api.paths.lock().unwrap().len(), 1);
    }

    #[test]
    fn router_build_accepts_informational_fetched_at_metadata() {
        let response: RouterBuildResponse = serde_json::from_value(json!({
            "inputMint": "So11111111111111111111111111111111111111112",
            "outputMint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "inAmount": "18000000",
            "outAmount": "12345",
            "otherAmountThreshold": "12000",
            "swapMode": "ExactIn",
            "slippageBps": 50,
            "computeBudgetInstructions": [],
            "setupInstructions": [],
            "swapInstruction": {"programId": "11111111111111111111111111111111", "accounts": [], "data": ""},
            "cleanupInstruction": null,
            "otherInstructions": [],
            "tipInstruction": null,
            "addressesByLookupTableAddress": {},
            "blockhashWithMetadata": {
                "blockhash": vec![7u8; 32],
                "lastValidBlockHeight": 1000,
                "fetchedAt": {"secs_since_epoch": 1_700_000_000, "nanos_since_epoch": 123_000_000}
            }
        }))
        .expect("current Swap API V2 blockhash metadata shape should deserialize");

        assert_eq!(response.blockhash_with_metadata.blockhash, vec![7; 32]);
        assert_eq!(
            response.blockhash_with_metadata.last_valid_block_height,
            1000
        );
        assert!(response.blockhash_with_metadata.fetched_at.is_some());
    }

    #[test]
    fn router_build_keeps_fetched_at_optional_and_metadata_strict() {
        let without_fetch_time = serde_json::from_value::<BlockhashMetadata>(json!({
            "blockhash": vec![9u8; 32],
            "lastValidBlockHeight": 1001
        }))
        .unwrap();
        assert!(without_fetch_time.fetched_at.is_none());

        assert!(
            serde_json::from_value::<BlockhashMetadata>(json!({
                "blockhash": vec![9u8; 32],
                "lastValidBlockHeight": 1001,
                "fetchedAt": {"secs_since_epoch": 1, "nanos_since_epoch": 0},
                "unexpected": true
            }))
            .is_err()
        );
    }
}

#[cfg(test)]
mod valuation_tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn swap_exposes_trusted_five_minute_plan_allowance() {
        let definitions = operation_definitions();
        let definition = definitions
            .iter()
            .find(|definition| definition.name == OP_SWAP)
            .unwrap();
        assert_eq!(
            definition.semantics["plan_allowance_seconds"],
            fin_domain::plan_timing::ONCHAIN_ALLOWANCE_SECONDS
        );
        for operation in [
            "jupiter.perps.increase_market",
            "jupiter.perps.limit.create",
            "jupiter.perps.tpsl.create",
            "jupiter.limit.create",
        ] {
            let definition = definitions
                .iter()
                .find(|definition| definition.name == operation)
                .unwrap_or_else(|| panic!("missing operation definition: {operation}"));
            assert_eq!(
                definition.semantics.get("plan_allowance_seconds"),
                None,
                "asynchronous or standing operation {operation} needs an explicit plan duration"
            );
        }
        for operation in ["jupiter.jlp.mint", "jupiter.jlp.burn"] {
            let definition = definitions
                .iter()
                .find(|definition| definition.name == operation)
                .unwrap();
            assert_eq!(
                definition.semantics["plan_allowance_seconds"],
                fin_domain::plan_timing::ONCHAIN_ALLOWANCE_SECONDS
            );
        }
    }

    #[test]
    fn recipient_extension_checks_distinguish_metadata_from_blocked_public_credits() {
        let account = |memo_required, approved, allow_public| fin_solana::token::TokenAccountInfo {
            address: "recipient".into(),
            program_id: fin_solana::token::TOKEN_2022_PROGRAM_ID.into(),
            mint: "mint".into(),
            token_owner: "owner".into(),
            amount_atoms: 0,
            delegate: None,
            delegated_amount_atoms: 0,
            close_authority: None,
            state: "initialized".into(),
            is_native: false,
            rent_exempt_reserve_lamports: None,
            withheld_transfer_fee_atoms: None,
            memo_required,
            confidential_transfer_approved: approved,
            allow_non_confidential_credits: allow_public,
            extensions: vec![],
        };
        assert!(validate_token_credit_extensions(&account(false, Some(true), Some(true))).is_ok());
        assert!(validate_token_credit_extensions(&account(false, None, None)).is_ok());
        assert!(validate_token_credit_extensions(&account(false, Some(false), Some(true))).is_ok());
        assert!(
            validate_token_credit_extensions(&account(false, Some(false), Some(false))).is_err()
        );
        assert!(validate_token_credit_extensions(&account(true, Some(true), Some(true))).is_err());
    }

    #[test]
    fn compiled_route_may_repeat_the_approved_wallet_signer() {
        use solana_instruction::{AccountMeta, Instruction};
        use solana_message::Message;
        use solana_pubkey4::Pubkey;
        use std::str::FromStr;

        let wallet = Pubkey::from_str("11111111111111111111111111111111").unwrap();
        let other = Pubkey::from_str("Vote111111111111111111111111111111111111111").unwrap();
        let program = Pubkey::from_str(swap::JUPITER_V6_PROGRAM).unwrap();
        let instruction = Instruction {
            program_id: program,
            accounts: vec![
                AccountMeta::new_readonly(wallet, true),
                AccountMeta::new(wallet, false),
                AccountMeta::new_readonly(other, false),
                AccountMeta::new_readonly(wallet, true),
            ],
            data: Vec::new(),
        };
        let message = Message::new(&[instruction], Some(&wallet));
        let compiled = &message.instructions[0];
        let normalized = NormalizedInstruction {
            program_id: message.account_keys[usize::from(compiled.program_id_index)].to_string(),
            accounts: compiled
                .accounts
                .iter()
                .map(|index| {
                    let index = usize::from(*index);
                    let required = usize::from(message.header.num_required_signatures);
                    let writable_signed =
                        required - usize::from(message.header.num_readonly_signed_accounts);
                    let writable_unsigned = message.account_keys.len()
                        - usize::from(message.header.num_readonly_unsigned_accounts);
                    NormalizedAccountMeta {
                        address: message.account_keys[index].to_string(),
                        signer: index < required,
                        writable: if index < required {
                            index < writable_signed
                        } else {
                            index < writable_unsigned
                        },
                    }
                })
                .collect(),
            data_base64: String::new(),
        };

        assert!(
            normalized
                .accounts
                .iter()
                .filter(|meta| meta.address == wallet.to_string())
                .count()
                >= 2
        );
        assert!(
            normalized
                .accounts
                .iter()
                .filter(|meta| meta.address == wallet.to_string())
                .all(|meta| meta.signer)
        );
        assert!(!has_foreign_route_signer(
            &normalized.accounts,
            &wallet.to_string()
        ));

        let mut tampered = normalized.accounts.clone();
        tampered.push(NormalizedAccountMeta {
            address: other.to_string(),
            signer: true,
            writable: false,
        });
        assert!(has_foreign_route_signer(&tampered, &wallet.to_string()));
    }

    #[test]
    fn v2_route_account_privileges_and_platform_fee_prefix_are_bound() {
        use fin_solana::transaction::NormalizedAccountMeta as Meta;

        let wallet = "wallet";
        let input = "input-mint";
        let output = "output-mint";
        let fee = "fee-token-account";
        let other = "route-pool-account";
        let meta = |address: &str, signer, writable| Meta {
            address: address.to_owned(),
            signer,
            writable,
        };

        // route_v2 fixed prefix has ten accounts; first remaining account is
        // the fee account, while later route accounts may be readonly.
        let mut route = vec![meta(other, false, false); 12];
        route[0] = meta(wallet, true, true);
        route[1] = meta("source", false, true);
        route[2] = meta("destination", false, true);
        route[10] = meta(fee, false, true);
        assert!(validate_v2_account_privileges(&route, wallet, false).is_ok());
        let fee_meta = validate_v2_platform_fee_meta(&route, false, 2, Some(output), input, output)
            .unwrap()
            .unwrap();
        assert_eq!(fee_meta.address, fee);

        let mut wrong_fee_slot = route.clone();
        wrong_fee_slot[10] = meta("route-pool-account", false, true);
        wrong_fee_slot[11] = meta(fee, false, true);
        assert_eq!(
            validate_v2_platform_fee_meta(&wrong_fee_slot, false, 2, Some(output), input, output)
                .unwrap()
                .unwrap()
                .address,
            "route-pool-account"
        );
        // Account identity/mint is checked against finalized token-account
        // data by validate_non_wallet_fee_account in the async validator.
        assert!(
            validate_v2_platform_fee_meta(&route, false, 2, Some("foreign-mint"), input, output)
                .is_err()
        );
        assert!(validate_v2_platform_fee_meta(&route, false, 2, None, input, output).is_err());

        let mut readonly_fee = route.clone();
        readonly_fee[10].writable = false;
        assert!(
            validate_v2_platform_fee_meta(&readonly_fee, false, 2, Some(output), input, output)
                .is_err()
        );
        let mut fee_signer = route.clone();
        fee_signer[10].signer = true;
        assert!(
            validate_v2_platform_fee_meta(&fee_signer, false, 2, Some(output), input, output)
                .is_err()
        );

        let mut readonly_user_source = route.clone();
        readonly_user_source[1].writable = false;
        assert!(validate_v2_account_privileges(&readonly_user_source, wallet, false).is_err());
        let mut foreign_signer = route.clone();
        foreign_signer[11].signer = true;
        assert!(validate_v2_account_privileges(&foreign_signer, wallet, false).is_err());

        // shared_accounts_route_v2 has twelve fixed accounts; fee is first
        // after those, with both shared token accounts writable.
        let mut shared = vec![meta(other, false, false); 14];
        shared[1] = meta(wallet, true, true);
        shared[2] = meta("source", false, true);
        shared[3] = meta("program-source", false, true);
        shared[4] = meta("program-destination", false, true);
        shared[5] = meta("destination", false, true);
        shared[12] = meta(fee, false, true);
        assert!(validate_v2_account_privileges(&shared, wallet, true).is_ok());
        assert_eq!(
            validate_v2_platform_fee_meta(&shared, true, 2, Some(input), input, output)
                .unwrap()
                .unwrap()
                .address,
            fee
        );
        for index in [3, 4] {
            let mut tampered = shared.clone();
            tampered[index].writable = false;
            assert!(validate_v2_account_privileges(&tampered, wallet, true).is_err());
        }
    }

    #[test]
    fn associated_account_graph_binds_system_program_payer_and_authority() {
        use fin_solana::transaction::NormalizedAccountMeta as Meta;

        let wallet = "Vote111111111111111111111111111111111111111";
        let mint = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
        let token_program = fin_solana::token::TOKEN_PROGRAM_ID;
        let ata = associated_address(wallet, token_program, mint).unwrap();
        let meta = |address: &str, signer, writable| Meta {
            address: address.to_owned(),
            signer,
            writable,
        };
        let valid = vec![
            meta(wallet, true, true),
            meta(&ata, false, true),
            meta(wallet, true, true), // Global transaction privileges include the wallet signer.
            meta(mint, false, false),
            meta(swap::SYSTEM_PROGRAM, false, false),
            meta(token_program, false, false),
        ];
        let mints = vec![mint.to_owned()];
        let authorities = vec![wallet.to_owned()];
        validate_associated_create_accounts(&valid, wallet, wallet, &mints, &authorities).unwrap();

        // A sponsored transaction may have a distinct fee payer while the
        // enrolled wallet funds its own exact ATA. Both must still be a
        // writable signer, and the rest of the ATA account graph is unchanged.
        let sponsor = "SysvarRent111111111111111111111111111111111";
        let mut wallet_funded_ata = valid.clone();
        wallet_funded_ata[0] = meta(wallet, true, true);
        validate_associated_create_accounts(
            &wallet_funded_ata,
            sponsor,
            wallet,
            &mints,
            &authorities,
        )
        .unwrap();

        let mut wrong_system = valid.clone();
        wrong_system[4].address = "SysvarRent111111111111111111111111111111111".into();
        assert!(
            validate_associated_create_accounts(
                &wrong_system,
                wallet,
                wallet,
                &mints,
                &authorities,
            )
            .is_err()
        );

        let mut wrong_payer = valid.clone();
        wrong_payer[0].address = "Stake11111111111111111111111111111111111111".into();
        let payer_error = validate_associated_create_accounts(
            &wrong_payer,
            sponsor,
            wallet,
            &mints,
            &authorities,
        )
        .unwrap_err();
        assert_eq!(
            payer_error.details.as_deref().unwrap(),
            &json!({"validation_stage":"associated-token payer identity"})
        );

        let mut unapproved_authority = valid.clone();
        unapproved_authority[2].address = "SysvarRent111111111111111111111111111111111".into();
        let authority_error = validate_associated_create_accounts(
            &unapproved_authority,
            wallet,
            wallet,
            &mints,
            &authorities,
        )
        .unwrap_err();
        assert_eq!(
            authority_error.details.as_deref().unwrap(),
            &json!({"validation_stage":"associated-token authority membership"})
        );

        let mut extra_signer = valid.clone();
        extra_signer[3].signer = true;
        assert!(
            validate_associated_create_accounts(
                &extra_signer,
                wallet,
                wallet,
                &mints,
                &authorities,
            )
            .is_err()
        );

        let mut extra_account = valid.clone();
        extra_account.push(meta(wallet, false, false));
        assert!(
            validate_associated_create_accounts(
                &extra_account,
                wallet,
                wallet,
                &mints,
                &authorities,
            )
            .is_err()
        );
    }

    struct RecordingPrices {
        requested: Mutex<Vec<AssetId>>,
    }

    #[async_trait::async_trait]
    impl UsdPriceSource for RecordingPrices {
        async fn quote(&self, asset: &AssetId) -> Result<fin_pricing::UsdQuote> {
            Ok(fin_pricing::UsdQuote {
                usd_per_unit: if asset.reference == "native" {
                    "2000"
                } else {
                    "1"
                }
                .into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
                observed_at: "2026-01-01T00:00:00Z".into(),
                source: "synthetic-test".into(),
            })
        }

        async fn quotes(&self, assets: &[AssetId]) -> Result<Vec<fin_pricing::UsdQuote>> {
            *self.requested.lock().unwrap() = assets.to_vec();
            let mut quotes = Vec::with_capacity(assets.len());
            for asset in assets {
                quotes.push(self.quote(asset).await?);
            }
            Ok(quotes)
        }
    }

    #[test]
    fn engine_price_pair_values_one_whole_token_in_micros_without_extra_decimals() {
        let asset = AssetId {
            network: NETWORK.into(),
            reference: "synthetic-mint".into(),
            decimals: 9,
        };
        let quote = fin_pricing::UsdQuote {
            usd_per_unit: "12.34".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            observed_at: "2026-01-01T00:00:00Z".into(),
            source: "synthetic-test".into(),
        };
        let pair = valuation_asset_price(&asset, &quote).unwrap();
        let whole_token_atoms = fin_domain::Quantity::from_u64(1_000_000_000);
        assert_eq!(
            whole_token_atoms
                .mul_div_ceil(&pair.usd_micros_per_unit, &pair.unit_atoms)
                .unwrap()
                .as_str(),
            "12340000"
        );
        assert_eq!(pair.unit_atoms.as_str(), "1".to_owned() + &"0".repeat(39));
    }

    #[tokio::test]
    async fn native_input_output_and_fee_share_one_exact_price_row() {
        let native = AssetId {
            network: NETWORK.into(),
            reference: "native".into(),
            decimals: 9,
        };
        let prices = Arc::new(RecordingPrices {
            requested: Mutex::new(Vec::new()),
        });
        let (valuation, _) = valuation(
            prices.clone(),
            native.clone(),
            1_000_000_000,
            native.clone(),
            900_000_000,
            5_000,
            "synthetic-wallet",
        )
        .await
        .unwrap();
        assert_eq!(*prices.requested.lock().unwrap(), vec![native.clone()]);
        assert_eq!(valuation.prices.len(), 1);
        assert_eq!(valuation.prices[0].asset, native);
        assert_eq!(valuation.movement_usd_micros.as_str(), "2000000000");
        assert_eq!(valuation.maximum_fee_usd_micros.as_str(), "10000");
    }

    #[test]
    fn native_output_credit_includes_fee_and_exact_closed_account_rent() {
        let credit = native_output_credit(22_034_280, false, 0, 5_000, 0, 2_039_280).unwrap();
        assert_eq!(credit, 20_000_000);
        assert_eq!(
            native_output_credit(22_034_279, false, 0, 5_000, 0, 2_039_280).unwrap(),
            19_999_999
        );
        assert_eq!(
            native_output_credit(22_034_280, false, 0, 5_000, 0, 2_039_281).unwrap(),
            19_999_999
        );
        assert!(native_output_credit(-5_000, false, 0, 5_000, 0, 2_039_280).is_err());
    }

    #[test]
    fn finalized_native_output_proof_emits_swap_rent_refund_and_fee_once() {
        let wallet = "11111111111111111111111111111111";
        let usdc = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
        let wsol_ata = "SysvarRent111111111111111111111111111111111";
        let token_program = fin_solana::token::TOKEN_PROGRAM_ID;
        let resource = ResourceId(format!("solana:mainnet-beta:{wallet}"));
        let native = AssetId {
            network: NETWORK.into(),
            reference: "native".into(),
            decimals: 9,
        };
        let input_asset = AssetId {
            network: NETWORK.into(),
            reference: usdc.into(),
            decimals: 6,
        };
        let input = json!({
            "wallet":"wallet", "network":"solana", "cluster":"mainnet-beta",
            "input_mint":usdc, "output_mint":"native", "amount_atoms":"2000000",
            "minimum_output_atoms":"20000000", "maximum_slippage_bps":50,
            "maximum_platform_fee_bps":0, "maximum_network_fee_lamports":5000,
            "maximum_account_funding_lamports":0, "route":"router"
        });
        let refund = AccountFundingRequirement {
            address: wsol_ata.into(),
            lamports: 2_039_280,
            account_owner: token_program.into(),
            refund_authority: wallet.into(),
            payer: wallet.into(),
        };
        let submission = Submission {
            kind: "solana_transaction".into(),
            reference: "solana:message:testhash".into(),
            data: json!({"fee_payer":wallet,"provider_request_id":"native-output-test"}),
        };
        let preview = PreparedOperation {
            operation: OP_SWAP.into(),
            input: input.clone(),
            binding: json!({
                "wallet_address":wallet, "fee_payer":wallet,
                "input_atoms":"2000000", "minimum_output_atoms":"20000000",
                "input_token_program":token_program, "output_token_program":token_program,
                "funding_accounts":[], "account_funding_lamports":0,
                "wrapped_native_lamports":0, "closed_wrapped_native_accounts":[wsol_ata],
                "closed_wrapped_native_refunds":[refund]
            }),
            summary: "synthetic native-output swap".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: "2026-01-01T00:00:00Z".into(),
            expires_at: "2026-01-01T00:05:00Z".into(),
            valuation: None,
            intent: FinancialIntent {
                expected: vec![
                    EffectConstraint::Conversion {
                        resource: resource.clone(),
                        input: input_asset.clone(),
                        maximum_input: fin_domain::Quantity::from_u64(2_000_000),
                        output: native.clone(),
                        minimum_output: fin_domain::Quantity::from_u64(20_000_000),
                    },
                    EffectConstraint::PositionOfKind {
                        resource: resource.clone(),
                        position: format!("account-funding:{wsol_ata}"),
                        asset: native.clone(),
                        position_kind: fin_domain::PositionKind::RefundableAccountFunding,
                        direction: fin_domain::Direction::Decrease,
                        maximum_change: fin_domain::Quantity::from_u64(2_039_280),
                    },
                    EffectConstraint::OriginMovement {
                        from: AccountingLocation::Position {
                            resource: resource.clone(),
                            position: format!("account-funding:{wsol_ata}"),
                            position_kind: fin_domain::PositionKind::RefundableAccountFunding,
                        },
                        to: AccountingLocation::Cash {
                            resource: resource.clone(),
                        },
                        debit_asset: native.clone(),
                        credit_asset: native.clone(),
                        maximum_debit: fin_domain::Quantity::from_u64(2_039_280),
                        minimum_credit: fin_domain::Quantity::from_u64(2_039_280),
                    },
                    EffectConstraint::Credit {
                        to: resource.clone(),
                        asset: native.clone(),
                        minimum_credit: fin_domain::Quantity::from_u64(2_039_280),
                    },
                ],
                ..FinancialIntent::default()
            },
            action_deadline: None,
            operation_version: 1,
        };
        let record = OperationRecord {
            id: "native-output-op".into(),
            request_id: "request".into(),
            fingerprint: "fingerprint".into(),
            fingerprint_version: 1,
            operation: OP_SWAP.into(),
            input,
            status: Status::Submitted,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            preview: Some(preview),
            approval: None,
            submission: Some(submission.clone()),
            dispatch_evidence: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
            position_baseline: None,
        };
        let proof = fin_solana::transaction::FinalizedReceiptProof {
            signature: "synthetic-signature".into(),
            slot: 42,
            fee_lamports: 5_000,
            failed: false,
            message_sha256: "synthetic-message-hash".into(),
            account_keys: vec![
                wallet.into(),
                "AssociatedTokenAccount111111111111111111111".into(),
                wsol_ata.into(),
            ],
            message_instructions: vec![],
            required_signatures: vec!["SYNTHETIC-ENCODED-SIGNATURE".into()],
            pre_balances_lamports: vec![1_000_000_000, 2_039_280, 2_039_280],
            post_balances_lamports: vec![1_022_034_280, 2_039_280, 0],
            pre_token_balances: vec![
                json!({"accountIndex":1,"mint":usdc,"owner":wallet,"programId":token_program,"uiTokenAmount":{"amount":"2000000","decimals":6}}),
                json!({"accountIndex":2,"mint":swap::WRAPPED_SOL_MINT,"owner":wallet,"programId":token_program,"uiTokenAmount":{"amount":"0","decimals":9}}),
            ],
            post_token_balances: vec![
                json!({"accountIndex":1,"mint":usdc,"owner":wallet,"programId":token_program,"uiTokenAmount":{"amount":"0","decimals":6}}),
            ],
            loaded_addresses: json!({"writable":[],"readonly":[]}),
            inner_instructions: vec![],
            log_messages: vec![],
            error: None,
        };
        let verified = verify_swap_effects(
            &record,
            &submission,
            "synthetic-signature",
            42,
            5_000,
            &proof,
        )
        .unwrap();
        assert!(verified.effects.iter().any(|effect| matches!(effect,
            fin_domain::FinancialEffect::Conversion { received, .. } if received.atoms.as_str()=="20000000")));
        assert!(verified.effects.iter().any(|effect| matches!(effect,
            fin_domain::FinancialEffect::UsableCredit { amount, .. } if amount.atoms.as_str()=="2039280")));
        assert!(verified.effects.iter().any(|effect| matches!(effect,
            fin_domain::FinancialEffect::PositionChange { delta, anchor:Some(anchor), .. }
                if delta.is_negative() && delta.magnitude().as_str()=="2039280"
                    && anchor.principal.atoms.is_zero())));
        assert!(verified.effects.iter().any(|effect| matches!(effect,
            fin_domain::FinancialEffect::Cost { amount, .. } if amount.atoms.as_str()=="5000")));
        assert_eq!(
            verified
                .effects
                .iter()
                .filter(|effect| matches!(effect, fin_domain::FinancialEffect::UsableCredit { .. }))
                .count(),
            1
        );
        assert!(!verified.effects.iter().any(|effect| matches!(effect,
            fin_domain::FinancialEffect::CashDebit { amount, .. } if amount.atoms.is_zero())));

        // A temporary WSOL account created and closed in one transaction has
        // gross rent funding but a zero net wallet debit. The immutable preview
        // still includes the approved gross Debit constraint, so emit an exact
        // zero CashDebit after the finalized account and wallet proofs validate.
        let mut transient_record = record.clone();
        let transient_preview = transient_record.preview.as_mut().unwrap();
        transient_preview.binding["funding_accounts"] = json!([refund.clone()]);
        transient_preview.binding["account_funding_lamports"] = json!(refund.lamports);
        transient_preview.intent.debits = vec![ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: native.clone(),
                atoms: fin_domain::Quantity::from_u64(refund.lamports),
            },
        }];
        let conversion = transient_preview.intent.expected[0].clone();
        transient_preview.intent.expected = vec![
            conversion,
            EffectConstraint::Debit {
                resource: resource.clone(),
                asset: native.clone(),
                maximum_debit: fin_domain::Quantity::from_u64(refund.lamports),
            },
        ];
        let mut transient_proof = proof.clone();
        transient_proof.pre_balances_lamports[0] = 1_000_000_000;
        transient_proof.post_balances_lamports[0] = 1_019_995_000;
        transient_proof.pre_balances_lamports[2] = 0;
        transient_proof.post_balances_lamports[2] = 0;
        transient_proof
            .pre_token_balances
            .retain(|row| row["accountIndex"] != 2);
        transient_proof
            .post_token_balances
            .retain(|row| row["accountIndex"] != 2);
        let transient = verify_swap_effects(
            &transient_record,
            &submission,
            "synthetic-signature",
            42,
            5_000,
            &transient_proof,
        )
        .unwrap();
        assert!(transient.effects.iter().any(|effect| matches!(effect,
            fin_domain::FinancialEffect::CashDebit { resource: actual_resource, amount }
                if actual_resource == &resource && amount.asset == native && amount.atoms.is_zero())));
        assert!(transient.effects.iter().any(|effect| matches!(effect,
            fin_domain::FinancialEffect::Cost { amount, .. } if amount.atoms.as_str() == "5000")));

        let mut malformed = transient_proof.clone();
        malformed.post_balances_lamports[2] = 1;
        assert!(
            verify_swap_effects(
                &transient_record,
                &submission,
                "synthetic-signature",
                42,
                5_000,
                &malformed,
            )
            .is_err()
        );

        let mut without_bound = transient_record.clone();
        without_bound
            .preview
            .as_mut()
            .unwrap()
            .intent
            .expected
            .retain(|constraint| !matches!(constraint, EffectConstraint::Debit { .. }));
        let unbound_effects = verify_swap_effects(
            &without_bound,
            &submission,
            "synthetic-signature",
            42,
            5_000,
            &transient_proof,
        )
        .unwrap();
        assert!(
            !unbound_effects.effects.iter().any(|effect| matches!(effect,
            fin_domain::FinancialEffect::CashDebit { amount, .. } if amount.atoms.is_zero()))
        );

        let mut bad_output = proof.clone();
        bad_output.post_balances_lamports[0] -= 1;
        assert!(
            verify_swap_effects(
                &record,
                &submission,
                "synthetic-signature",
                42,
                5_000,
                &bad_output
            )
            .is_err()
        );
        let mut bad_rent = proof.clone();
        bad_rent.post_balances_lamports[2] = 1;
        assert!(
            verify_swap_effects(
                &record,
                &submission,
                "synthetic-signature",
                42,
                5_000,
                &bad_rent
            )
            .is_err()
        );

        // JupiterZ's pinned OrderEngine IDL permits a direct native-SOL
        // output with the optional output token account set to its sentinel;
        // that path has no WSOL close or rent refund.
        let mut direct_record = record.clone();
        direct_record.preview.as_mut().unwrap().binding["router"] = json!("jupiterz");
        direct_record.preview.as_mut().unwrap().binding["rfq_native_output_direct"] = json!(true);
        direct_record.preview.as_mut().unwrap().binding["closed_wrapped_native_accounts"] =
            json!([]);
        direct_record.preview.as_mut().unwrap().binding["closed_wrapped_native_refunds"] =
            json!([]);
        let mut direct_proof = fin_solana::transaction::FinalizedReceiptProof {
            account_keys: vec![
                wallet.into(),
                "AssociatedTokenAccount111111111111111111111".into(),
                wsol_ata.into(),
            ],
            pre_balances_lamports: vec![1_000_000_000, 0, 0],
            post_balances_lamports: vec![1_019_995_000, 0, 0],
            ..proof.clone()
        };
        direct_proof.pre_token_balances = proof.pre_token_balances.clone();
        direct_proof.post_token_balances = proof.post_token_balances.clone();
        let direct = verify_swap_effects(
            &direct_record,
            &submission,
            "synthetic-signature",
            42,
            5_000,
            &direct_proof,
        )
        .unwrap();
        assert!(direct.effects.iter().any(|effect| matches!(effect,
            fin_domain::FinancialEffect::Conversion { received, .. } if received.atoms.as_str()=="20000000")));
        assert!(
            !direct
                .effects
                .iter()
                .any(|effect| matches!(effect, fin_domain::FinancialEffect::UsableCredit { .. }))
        );
        direct_proof.post_balances_lamports[0] -= 1;
        assert!(
            verify_swap_effects(
                &direct_record,
                &submission,
                "synthetic-signature",
                42,
                5_000,
                &direct_proof
            )
            .is_err()
        );

        // The prepared contract explicitly constrains the returned rent as a
        // usable credit in addition to its position/origin accounting.
        assert!(
            record
                .preview
                .unwrap()
                .intent
                .expected
                .iter()
                .any(|expected| matches!(expected,
            EffectConstraint::Credit { to, asset, minimum_credit }
                if to==&resource && asset==&native && minimum_credit.as_str()=="2039280"))
        );
    }

    #[test]
    fn shared_route_includes_token_2022_for_either_mixed_mint_direction() {
        let token = fin_solana::token::TOKEN_PROGRAM_ID;
        let token_2022 = fin_solana::token::TOKEN_2022_PROGRAM_ID;
        assert_eq!(shared_token_2022_slot(token_2022, token), token_2022);
        assert_eq!(shared_token_2022_slot(token, token_2022), token_2022);
        assert_eq!(
            shared_token_2022_slot(token, token),
            swap::JUPITER_V6_PROGRAM
        );
    }

    #[test]
    fn dflow_pinned_route_enforces_quoted_minimum_fee_and_exact_input() {
        let intent = SwapIntent {
            wallet: "wallet".into(),
            network: "solana".into(),
            input_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            output_mint: "So11111111111111111111111111111111111111112".into(),
            amount_atoms: "1000000".into(),
            minimum_output_atoms: "940000".into(),
            maximum_slippage_bps: 100,
            maximum_platform_fee_bps: 20,
            maximum_network_fee_lamports: 10_000,
            maximum_account_funding_lamports: 0,
            route: swap::SwapRoute::MetaAggregator,
        };
        let candidate = Candidate {
            tx_base64: String::new(),
            request_id: Some("synthetic".into()),
            router: Some("dflow".into()),
            last_valid_block_height: Some(10),
            provider_expire_at: None,
            expected_output_atoms: 950_000,
            instructions: None,
            lookup_tables: Vec::new(),
            blockhash: None,
            actual_slippage_bps: 0,
            platform_fee_bps: 0,
            platform_fee_mint: None,
        };
        let decoded = crate::dflow::DecodedDflowSwap {
            instruction: "swap2".into(),
            quoted_output: 950_000,
            slippage_bps: 100,
            platform_fee_bps: 0,
            positive_slippage_fee_limit_pct: 0,
            action_count: 1,
            swap_input_atoms: Some(1_000_000),
            transfer_fee_atoms: 0,
            wrap_sol_lamports: None,
            unsupported_actions: Vec::new(),
            output_native: false,
            destination_index: None,
        };
        assert!(validate_dflow_bounds(&decoded, &intent, &candidate).is_ok());

        let mut tampered = decoded.clone();
        tampered.swap_input_atoms = Some(1_000_001);
        assert!(validate_dflow_bounds(&tampered, &intent, &candidate).is_err());
        tampered = decoded.clone();
        tampered.slippage_bps = 200;
        assert!(validate_dflow_bounds(&tampered, &intent, &candidate).is_err());
        tampered = decoded.clone();
        tampered.positive_slippage_fee_limit_pct = 1;
        assert!(validate_dflow_bounds(&tampered, &intent, &candidate).is_err());
        // A second split leg or an explicit transfer-fee action must be
        // included in the total; a valid first leg cannot hide overspending.
        tampered = decoded.clone();
        tampered.swap_input_atoms = Some(600_000);
        tampered.transfer_fee_atoms = 400_001;
        assert!(validate_dflow_bounds(&tampered, &intent, &candidate).is_err());
        tampered.transfer_fee_atoms = 400_000;
        assert!(validate_dflow_bounds(&tampered, &intent, &candidate).is_ok());
    }

    #[test]
    fn dflow_fixed_account_graph_binds_wallet_event_and_program() {
        let wallet = "11111111111111111111111111111111";
        let program = Pubkey::try_from(DFLOW_PROGRAM).unwrap();
        let event = Pubkey::find_program_address(&[b"__event_authority"], &program)
            .0
            .to_string();
        let decoded = crate::dflow::DecodedDflowSwap {
            instruction: "swap".into(),
            quoted_output: 1,
            slippage_bps: 0,
            platform_fee_bps: 0,
            positive_slippage_fee_limit_pct: 0,
            action_count: 1,
            swap_input_atoms: Some(1),
            transfer_fee_atoms: 0,
            wrap_sol_lamports: None,
            unsupported_actions: Vec::new(),
            output_native: false,
            destination_index: None,
        };
        let mut ix = NormalizedInstruction {
            program_id: DFLOW_PROGRAM.into(),
            accounts: vec![
                NormalizedAccountMeta {
                    address: fin_solana::token::TOKEN_PROGRAM_ID.into(),
                    signer: false,
                    writable: false,
                },
                NormalizedAccountMeta {
                    address: swap::ASSOCIATED_TOKEN_PROGRAM.into(),
                    signer: false,
                    writable: false,
                },
                NormalizedAccountMeta {
                    address: swap::SYSTEM_PROGRAM.into(),
                    signer: false,
                    writable: false,
                },
                NormalizedAccountMeta {
                    address: wallet.into(),
                    signer: true,
                    writable: false,
                },
                NormalizedAccountMeta {
                    address: event,
                    signer: false,
                    writable: false,
                },
                NormalizedAccountMeta {
                    address: DFLOW_PROGRAM.into(),
                    signer: false,
                    writable: false,
                },
            ],
            data_base64: String::new(),
        };
        assert!(matches!(
            validate_dflow_header(wallet, &ix, &decoded),
            Ok(6)
        ));
        ix.accounts[4].address = wallet.into();
        assert!(validate_dflow_header(wallet, &ix, &decoded).is_err());
        ix.accounts[4].address = Pubkey::find_program_address(&[b"__event_authority"], &program)
            .0
            .to_string();
        ix.accounts[3].signer = false;
        assert!(validate_dflow_header(wallet, &ix, &decoded).is_err());
    }

    #[test]
    fn dflow_destination_authority_accepts_only_wallet_privilege_union() {
        let wallet = NormalizedAccountMeta {
            address: "wallet-address".into(),
            signer: true,
            writable: true,
        };
        let foreign_signer = NormalizedAccountMeta {
            address: "other-signer".into(),
            signer: true,
            writable: false,
        };
        assert!(dflow_destination_authority_is_wallet(
            &wallet,
            "wallet-address"
        ));
        assert!(!dflow_destination_authority_is_wallet(
            &foreign_signer,
            "wallet-address"
        ));
    }

    #[test]
    fn okx_swap_args_match_pinned_idl_and_intent_bounds() {
        let intent = SwapIntent {
            wallet: "wallet".into(),
            network: "solana".into(),
            input_mint: "native".into(),
            output_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            amount_atoms: "1000".into(),
            minimum_output_atoms: "900".into(),
            maximum_slippage_bps: 100,
            maximum_platform_fee_bps: 0,
            maximum_network_fee_lamports: 5000,
            maximum_account_funding_lamports: 0,
            route: SwapRoute::MetaAggregator,
        };
        let candidate = Candidate {
            tx_base64: String::new(),
            request_id: Some("synthetic".into()),
            router: Some("okx".into()),
            last_valid_block_height: None,
            provider_expire_at: None,
            expected_output_atoms: 950,
            instructions: None,
            lookup_tables: vec![],
            blockhash: None,
            actual_slippage_bps: 50,
            platform_fee_bps: 0,
            platform_fee_mint: None,
        };
        let mut data = vec![248, 198, 158, 145, 225, 117, 135, 200];
        data.extend_from_slice(&7u64.to_le_bytes()); // order_id
        data.extend_from_slice(&1000u64.to_le_bytes()); // amount_in
        data.extend_from_slice(&950u64.to_le_bytes()); // expect_amount_out
        data.extend_from_slice(&50u16.to_le_bytes()); // slippage
        data.extend_from_slice(&1u32.to_le_bytes()); // one route
        data.push(142); // last pinned Dex enum variant
        data.extend_from_slice(&10_000u16.to_le_bytes());
        data.push(0); // route index
        let decoded = decode_okx_swap(&data, &intent, &candidate).unwrap();
        assert_eq!(decoded.amount_in, 1000);
        assert_eq!(decoded.expected_out, 950);
        assert_eq!(decoded.route_count, 1);

        let mut too_high_minimum = intent.clone();
        too_high_minimum.minimum_output_atoms = "946".into();
        assert!(decode_okx_swap(&data, &too_high_minimum, &candidate).is_err());

        data[16] = 1; // changing the exact input amount
        assert!(decode_okx_swap(&data, &intent, &candidate).is_err());
        data[16] = 0xe8; // restore little-endian 1000 (0x03e8)
        data[17] = 0x03;
        data[18..24].fill(0);
        data[38] = 143; // unknown pinned enum variant
        assert!(decode_okx_swap(&data, &intent, &candidate).is_err());
    }
}
