//! Jupiter Meta-Aggregator adapter. Financial limits are checked against the
//! order response; Jupiter is trusted to construct the corresponding transaction.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{Duration as ChronoDuration, Utc};
use gofin_domain::{
    AccountingLocation, Adapter, Amount, AssetId, CandidateExpiryKind, Check, CheckStatus,
    ConnectionMetadata, CostBound, CostTreatment, Definition, EffectConstraint, ExecutionActor,
    FinancialIntent, FinancialState, GoFinError, OperationKind, OperationRecord, PreparedOperation,
    Reconciliation, ResourceAmount, ResourceId, Result, SnapshotScope, StateSnapshot, Status,
    Submission, SubmissionJournal, Valuation, VerifiedNonDispatchExpiry,
    boundaries::BoundarySpec,
    financial_plans::{FinancialPlanFundingIdentity, PlanPermission},
};
use gofin_pricing::UsdPriceSource;
use gofin_solana::{
    CLUSTER, NETWORK, SolanaWalletAdapter,
    transaction::{FrozenTransaction, ReceiptOutcome, TransactionSender},
};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::{
    api::{HttpJupiterApi, JupiterApi, JupiterHost, RateBucket, RequestPriority},
    pricing::JupiterUsdPrices,
    swap::{self, MetaOrderResponse, SwapIntent},
};

const OP_QUOTE: &str = "jupiter.quote";
const OP_SWAP: &str = "jupiter.swap";
const REQUEST_DEADLINE: Duration = Duration::from_secs(20);
const FROZEN_CACHE_TTL: Duration = Duration::from_secs(30);
const FROZEN_CACHE_CAPACITY: usize = 4;
pub(crate) const MEMO_PROGRAM: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

pub struct JupiterAdapter {
    wallets: BTreeMap<String, Arc<SolanaWalletAdapter>>,
    api: Arc<dyn JupiterApi>,
    prices: Arc<dyn UsdPriceSource>,
    revalidated_frozen: OneShotCache<FrozenTransaction>,
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
                GoFinError::validation("Jupiter requires at least one enrolled Solana wallet.")
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
                GoFinError::validation("Jupiter requires at least one enrolled Solana wallet.")
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
            return Err(GoFinError::validation(
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

    fn cache_frozen_candidate(&self, prepared: &PreparedOperation, frozen: FrozenTransaction) {
        let Some(key) = frozen_candidate_cache_key(prepared) else {
            return;
        };
        self.revalidated_frozen.insert(key, frozen);
    }

    fn take_cached_frozen_candidate(
        &self,
        prepared: &PreparedOperation,
    ) -> Option<FrozenTransaction> {
        self.revalidated_frozen
            .take(frozen_candidate_cache_key(prepared)?)
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
                "The selected Solana wallet is not enrolled for Jupiter.",
                "Choose an enrolled wallet from connection discovery.",
            )
        })
    }

    fn validate_network(input: &Value) -> Result<()> {
        if input.get("network").and_then(Value::as_str) != Some("solana")
            || input.get("cluster").and_then(Value::as_str) != Some(CLUSTER)
        {
            return Err(GoFinError::validation(
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
            GoFinError::validation(
                "The Jupiter swap input does not match its exact amount, mint and limit schema.",
            )
        })
    }

    async fn fetch_candidate(
        &self,
        intent: &SwapIntent,
        wallet: &SolanaWalletAdapter,
        priority: RequestPriority,
    ) -> Result<Candidate> {
        let response = self.fetch_order(intent, wallet, priority).await?;
        Candidate::from_order(intent, wallet.address(), response)
    }

    async fn fetch_order(
        &self,
        intent: &SwapIntent,
        wallet: &SolanaWalletAdapter,
        priority: RequestPriority,
    ) -> Result<MetaOrderResponse> {
        let path = swap::meta_order_path(intent, wallet.address())?;
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
        serde_json::from_value(raw).map_err(|_| bad_provider_response())
    }

    // Presentation only: bind names to independently verified mint identities and
    // on-chain decimals. A missing or malformed provider response is harmless.
    async fn approval_asset_metadata(&self, assets: &[&AssetId]) -> Vec<Value> {
        let mints: Vec<&str> = assets
            .iter()
            .filter(|asset| asset.reference != "native")
            .map(|asset| asset.reference.as_str())
            .collect();
        if mints.is_empty() {
            return Vec::new();
        }
        let path = format!("/tokens/v2/search?query={}", mints.join(","));
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            self.api.request(
                JupiterHost::Platform,
                Method::GET,
                &path,
                None,
                RateBucket::Main,
                RequestPriority::Normal,
                Duration::from_secs(2),
            ),
        )
        .await;
        match response {
            Ok(Ok(value)) => token_approval_metadata(assets, &value),
            _ => Vec::new(),
        }
    }

    async fn prepare_candidate(
        &self,
        intent: &SwapIntent,
        wallet: &SolanaWalletAdapter,
    ) -> Result<PreparedCandidate> {
        let candidate = self
            .fetch_candidate(intent, wallet, RequestPriority::Financial)
            .await?;
        let frozen = self.freeze_candidate(intent, wallet, &candidate).await?;
        Ok(PreparedCandidate { candidate, frozen })
    }

    async fn freeze_candidate(
        &self,
        intent: &SwapIntent,
        wallet: &SolanaWalletAdapter,
        candidate: &Candidate,
    ) -> Result<FrozenTransaction> {
        // Recheck immutable order metadata when loading a durable preview. No
        // transaction instruction, account, fee or simulation review occurs.
        swap::validate_meta_order(intent, wallet.address(), candidate.order_response.clone())?;
        let signer = wallet.signer().ok_or_else(|| {
            GoFinError::new(
                "SOLANA_SIGNING_UNAVAILABLE",
                "prepare",
                "The selected wallet has no protected signing capability.",
                "Enroll a protected signer for this wallet before preparing a swap.",
            )
        })?;
        let remote = gofin_solana::SolanaRuntime::missing_signers(&candidate.tx_base64)?
            .into_iter()
            .filter(|address| address != signer.address())
            .collect::<Vec<_>>();
        wallet
            .runtime()
            .freeze_provider_transaction_with_expiry(
                &candidate.tx_base64,
                &signer,
                candidate.last_valid_block_height,
                candidate.provider_expire_at.as_deref(),
                intent.maximum_network_fee_lamports,
                candidate.request_id.clone(),
                &remote,
            )
            .await
    }

    async fn quote(&self, input: &Value) -> Result<Value> {
        Self::validate_network(input)?;
        let intent = quote_intent(input)?;
        let wallet = self.wallet(input)?;
        if intent.wallet != input["wallet"].as_str().unwrap_or_default() {
            return Err(GoFinError::validation(
                "The quote wallet must match the selected enrolled wallet.",
            ));
        }
        let response = self
            .fetch_order(&intent, &wallet, RequestPriority::Normal)
            .await?;
        let quote = swap::validate_meta_quote(&intent, wallet.address(), response.clone())?;
        Ok(
            json!({"network":NETWORK,"cluster":CLUSTER,"wallet":wallet.address(),"input_mint":intent.input_mint,"output_mint":intent.output_mint,"input_atoms":intent.amount_atoms,"expected_output_atoms":quote.expected_output_atoms.to_string(),"minimum_output_atoms":quote.minimum_output_atoms.to_string(),"maximum_slippage_bps":intent.maximum_slippage_bps,"slippage_bps":quote.slippage_bps,"router":quote.router,"provider_request_id":quote.request_id,"last_valid_block_height":quote.last_valid_block_height,"provider_expire_at":quote.provider_expire_at,"fees":{"estimated_network_fee_lamports":quote.wallet_network_fee_lamports.to_string(),"network_fee_estimate_status":"provider_quote","maximum_platform_fee_bps":quote.fee_bps,"platform_fee_mint":quote.fee_mint,"account_funding_estimate_lamports":quote.wallet_account_funding_lamports.to_string(),"account_funding_estimate_status":"provider_quote"},"expires_at":bounded_expiry(quote.provider_expire_at.as_deref())?,"transaction_available":response.transaction_available(),"provider_error":response.provider_error(),"next_action":response.quote_next_action()}),
        )
    }
}

fn quote_intent(input: &Value) -> Result<SwapIntent> {
    if input.get("route").is_some() {
        return Err(GoFinError::validation(
            "Jupiter spot quotes use Meta-Aggregator /order exclusively; route is no longer an input.",
        ));
    }
    let amount = input
        .get("amount_atoms")
        .and_then(Value::as_str)
        .ok_or_else(|| GoFinError::validation("Provide an exact atom amount for the quote."))?;
    // The quote is a read-only route inquiry. These internal broad bounds
    // permit the provider quote to be displayed without granting authority.
    let intent = SwapIntent {
        wallet: input
            .get("wallet")
            .and_then(Value::as_str)
            .ok_or_else(|| GoFinError::validation("Select an enrolled wallet for the quote."))?
            .to_owned(),
        network: "solana".into(),
        input_mint: input
            .get("input_mint")
            .and_then(Value::as_str)
            .ok_or_else(|| GoFinError::validation("Provide the exact input mint."))?
            .to_owned(),
        output_mint: input
            .get("output_mint")
            .and_then(Value::as_str)
            .ok_or_else(|| GoFinError::validation("Provide the exact output mint."))?
            .to_owned(),
        amount_atoms: amount.to_owned(),
        minimum_output_atoms: "1".into(),
        maximum_slippage_bps: input
            .get("maximum_slippage_bps")
            .and_then(Value::as_u64)
            .and_then(|v| u16::try_from(v).ok())
            .unwrap_or(10_000),
        maximum_platform_fee_bps: u16::MAX,
        maximum_network_fee_lamports: u64::MAX,
        maximum_account_funding_lamports: u64::MAX,
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
        self.wallets.iter().map(|(name,wallet)| ConnectionMetadata { name:name.clone(),provider:"jupiter".into(),resource:ResourceId(format!("solana:mainnet-beta:{}",wallet.address())),role:Some("swap".into()),parent:None,network:Some(NETWORK.into()),address:Some(wallet.address().into()),credential_ref:None,signing_enabled:wallet.signer().is_some(),details:json!({"cluster":CLUSTER,"api_key_required":false,"integration":"meta_aggregator_order_execute"}) }).collect()
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
            return Err(GoFinError::validation(
                "The swap wallet must match the selected enrolled wallet.",
            ));
        }
        let prepared_candidate = self.prepare_candidate(&intent, &wallet).await?;
        let candidate = prepared_candidate.candidate;
        let frozen = prepared_candidate.frozen;
        let runtime = wallet.runtime();
        let resource = ResourceId(format!("solana:mainnet-beta:{}", wallet.address()));
        // Reserve the approved rent cap: provider rent is an estimate, while
        // finalized token-account balances determine the actual allocation.
        let wallet_account_funding_lamports = intent.maximum_account_funding_lamports;
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
        let approval_asset_metadata = self
            .approval_asset_metadata(&[&input_asset, &output_asset])
            .await;
        let input_token_program = token_program_for(&runtime, &intent.input_mint).await?;
        let output_token_program = token_program_for(&runtime, &intent.output_mint).await?;
        let (input_atoms, minimum_atoms) = swap::validate_intent(&intent)?;
        let intent_effect = EffectConstraint::Conversion {
            resource: resource.clone(),
            input: input_asset.clone(),
            maximum_input: gofin_domain::Quantity::from_u64(input_atoms),
            output: output_asset.clone(),
            minimum_output: gofin_domain::Quantity::from_u64(minimum_atoms),
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
            gofin_domain::Quantity::from_u64(wallet_account_funding_lamports)
                .mul_div_ceil(&native_price.usd_micros_per_unit, &native_price.unit_atoms)?;
        let submission_data = json!({"provider_trust":true,"order_response":candidate.order_response,"wallet_address":wallet.address(),"fee_payer":frozen.fee_payer(),"request_id":candidate.request_id,"router":candidate.router,"message_sha256":frozen.message_sha256(),"last_valid_block_height":frozen.last_valid_block_height(),"provider_expire_at":frozen.provider_expire_at(),"recent_blockhash":frozen.recent_blockhash(),"transaction_base64":candidate.tx_base64,"actual_slippage_bps":candidate.actual_slippage_bps,"platform_fee_bps":candidate.platform_fee_bps,"platform_fee_mint":candidate.platform_fee_mint,"input_atoms":input_atoms.to_string(),"expected_output_atoms":candidate.expected_output_atoms.to_string(),"minimum_output_atoms":minimum_atoms.to_string(),"maximum_network_fee_lamports":intent.maximum_network_fee_lamports.to_string(),"maximum_account_funding_lamports":intent.maximum_account_funding_lamports,"input_token_program":input_token_program,"output_token_program":output_token_program,"wallet_network_fee_lamports":candidate.wallet_network_fee_lamports,"wallet_account_funding_lamports":candidate.wallet_account_funding_lamports});
        let mut submission_data = submission_data;
        if !approval_asset_metadata.is_empty() {
            submission_data["approval_asset_metadata"] = Value::Array(approval_asset_metadata);
        }
        let prepared = PreparedOperation {
            operation: operation.into(),
            input: input.clone(),
            binding: submission_data,
            summary: format!(
                "Swap {} {} for at least {} {} on Solana mainnet-beta; max network fee {} SOL; Jupiter fee {}%; max account funding {} SOL (Jupiter estimate {} SOL).",
                display_atomic_amount(input_atoms, input_asset.decimals),
                solana_asset_label(&input_asset.reference, input_asset.decimals),
                display_atomic_amount(minimum_atoms, output_asset.decimals),
                solana_asset_label(&output_asset.reference, output_asset.decimals),
                display_atomic_amount(intent.maximum_network_fee_lamports, 9),
                format_basis_points(candidate.platform_fee_bps),
                display_atomic_amount(wallet_account_funding_lamports, 9),
                display_atomic_amount(candidate.wallet_account_funding_lamports, 9),
            ),
            checks: vec![
                check("Jupiter order wallet, mint and exact input binding"),
                check("Jupiter order minimum output and RTSE slippage limits"),
                check("Jupiter quoted network, platform and account funding limits"),
                check("Immutable signing message and expiry"),
            ],
            warnings: vec!["Jupiter constructs the transaction; GoFin checks order limits without inspecting or simulating its instructions.".into(), "Settlement is reported only after finalized chain evidence proves the financial effects.".into()],
            observed_at: Utc::now().to_rfc3339(),
            expires_at: bounded_expiry(candidate.provider_expire_at.as_deref())?,
            valuation: Some(valuation),
            intent: FinancialIntent {
                debits: {
                    let mut debits = vec![ResourceAmount {
                        resource: resource.clone(),
                        amount: Amount {
                            asset: input_asset.clone(),
                            atoms: gofin_domain::Quantity::from_u64(input_atoms),
                        },
                    }];
                    if wallet_account_funding_lamports > 0 {
                        debits.push(ResourceAmount {
                            resource: resource.clone(),
                            amount: Amount {
                                asset: native.clone(),
                                atoms: gofin_domain::Quantity::from_u64(
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
                    expected.push(EffectConstraint::AccountFundingBudget {
                        resource: resource.clone(), asset: native.clone(),
                        maximum_debit: gofin_domain::Quantity::from_u64(wallet_account_funding_lamports),
                    });
                    expected
                },
                locks: vec![resource],
            },
            action_deadline: None,
            operation_version: 1,
        };
        self.cache_frozen_candidate(&prepared, frozen);
        Ok(prepared)
    }

    async fn prepare_boundaries(
        &self,
        prepared: &PreparedOperation,
        bounds: &Value,
    ) -> Result<BoundarySpec> {
        if prepared.operation != OP_SWAP {
            return Err(GoFinError::new(
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
            return Err(GoFinError::validation(
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
            return Err(GoFinError::new(
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
            GoFinError::validation("Plan swap minimum output must be exact integer atoms.")
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
            return Err(GoFinError::new(
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
            return Err(GoFinError::validation(
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
        let frozen = self.freeze_candidate(&intent, &wallet, &candidate).await?;
        if prepared
            .binding
            .get("message_sha256")
            .and_then(Value::as_str)
            != Some(frozen.message_sha256())
        {
            return Err(GoFinError::new(
                "JUPITER_MESSAGE_CHANGED",
                "revalidation",
                "The immutable Jupiter transaction no longer matches the approved message.",
                "Keep the operation unresolved and prepare a fresh candidate within active plan or boundary authority; otherwise request new approval.",
            ));
        }
        self.cache_frozen_candidate(prepared, frozen);
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
        let frozen = self.freeze_candidate(&intent, &wallet, &candidate).await?;
        if prepared.binding["message_sha256"].as_str() != Some(frozen.message_sha256()) {
            return Err(GoFinError::new(
                "JUPITER_MESSAGE_CHANGED",
                "dispatch",
                "The frozen transaction no longer matches the approved message.",
                "Keep the operation unresolved and prepare a new exact swap for approval.",
            ));
        }
        {
            let request_id = candidate
                .request_id
                .as_ref()
                .ok_or_else(bad_provider_response)?;
            let sender = MetaExecuteSender {
                api: self.api.clone(),
                request_id: request_id.clone(),
                expires_at: prepared.expires_at.clone(),
            };
            wallet
                .runtime()
                .dispatch_with_sender(&frozen, &signer, prepared, journal, &sender)
                .await
        }
    }

    async fn submit_revalidated(
        &self,
        validated: &gofin_domain::RevalidatedOperation,
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
        let frozen = match self.take_cached_frozen_candidate(prepared) {
            Some(frozen)
                if cached_frozen_matches(prepared, &candidate, signer.address(), &frozen) =>
            {
                frozen
            }
            Some(_) => {
                return Err(GoFinError::new(
                    "JUPITER_MESSAGE_CHANGED",
                    "dispatch",
                    "The cached frozen transaction no longer matches the revalidated candidate.",
                    "Keep the operation unresolved and prepare a fresh candidate within active authority.",
                ));
            }
            None => self.freeze_candidate(&intent, &wallet, &candidate).await?,
        };
        if prepared.binding["message_sha256"].as_str() != Some(frozen.message_sha256()) {
            return Err(GoFinError::new(
                "JUPITER_MESSAGE_CHANGED",
                "dispatch",
                "The frozen transaction no longer matches the approved message.",
                "Keep the operation unresolved and prepare a new exact swap for approval.",
            ));
        }
        validated.assert_fresh()?;
        {
            let request_id = candidate
                .request_id
                .as_ref()
                .ok_or_else(bad_provider_response)?;
            let sender = MetaExecuteSender {
                api: self.api.clone(),
                request_id: request_id.clone(),
                expires_at: prepared.expires_at.clone(),
            };
            wallet
                .runtime()
                .dispatch_with_sender(&frozen, &signer, prepared, journal, &sender)
                .await
        }
    }

    async fn verified_position_baselines(
        &self,
        record: &OperationRecord,
    ) -> Result<Vec<gofin_domain::PositionAnchor>> {
        if record
            .preview
            .as_ref()
            .and_then(|p| p.binding.get("provider_trust"))
            .and_then(Value::as_bool)
            != Some(true)
        {
            return Ok(Vec::new());
        }
        let reconciliation = self.reconcile(record).await?;
        match reconciliation.verified {
            Some(verified) if reconciliation.status == Status::Completed => {
                crate::spot_settlement::position_baselines(&verified)
            }
            _ => Ok(Vec::new()),
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
    verified: &gofin_domain::VerifiedEffects,
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
        || gofin_domain::verified_effect_digest(verified).ok()? != verified.proof_digest
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
    use gofin_domain::{FinancialEffect, VerifiedEffects};
    use gofin_pricing::UsdQuote;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingRpc(AtomicUsize);

    #[async_trait::async_trait]
    impl gofin_solana::SolanaRpc for CountingRpc {
        async fn request(&self, _method: &str, _params: Value) -> Result<Value> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(GoFinError::validation(
                "Unexpected RPC in expiry hook test.",
            ))
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
            Err(GoFinError::validation(
                "Unexpected Jupiter API in expiry hook test.",
            ))
        }
    }

    struct TestPrices;

    #[async_trait::async_trait]
    impl UsdPriceSource for TestPrices {
        async fn quote(&self, _asset: &AssetId) -> Result<UsdQuote> {
            Err(GoFinError::validation(
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
        verified.proof_digest = gofin_domain::verified_effect_digest(&verified).unwrap();
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
            authorization_source: None,
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
    order_response: MetaOrderResponse,
    wallet_network_fee_lamports: u64,
    wallet_account_funding_lamports: u64,
    actual_slippage_bps: u16,
    platform_fee_bps: u16,
    platform_fee_mint: Option<String>,
}

#[derive(Debug)]
struct PreparedCandidate {
    candidate: Candidate,
    frozen: FrozenTransaction,
}

impl Candidate {
    fn from_order(intent: &SwapIntent, wallet: &str, response: MetaOrderResponse) -> Result<Self> {
        let validated = swap::validate_meta_order(intent, wallet, response.clone())?;
        let order = validated.quote;
        Ok(Self {
            tx_base64: validated.transaction_base64,
            request_id: Some(order.request_id),
            router: Some(order.router),
            last_valid_block_height: order.last_valid_block_height,
            provider_expire_at: order.provider_expire_at,
            expected_output_atoms: order.expected_output_atoms,
            order_response: response,
            wallet_network_fee_lamports: order.wallet_network_fee_lamports,
            wallet_account_funding_lamports: order.wallet_account_funding_lamports,
            actual_slippage_bps: order.slippage_bps,
            platform_fee_bps: order.fee_bps,
            platform_fee_mint: order.fee_mint,
        })
    }

    fn from_binding(binding: &Value) -> Result<Self> {
        if binding.get("provider_trust").and_then(Value::as_bool) != Some(true) {
            return Err(GoFinError::new(
                "JUPITER_PREVIEW_REPREPARE_REQUIRED",
                "prepare",
                "This preview predates Jupiter order-based execution.",
                "Prepare a fresh Meta-Aggregator order; existing submitted transactions remain recoverable.",
            ));
        }
        let order_response: MetaOrderResponse =
            serde_json::from_value(binding["order_response"].clone())
                .map_err(|_| bad_provider_response())?;
        if order_response.transaction.as_deref() != binding["transaction_base64"].as_str()
            || Some(order_response.request_id.as_str()) != binding["request_id"].as_str()
        {
            return Err(bad_provider_response());
        }
        Ok(Self {
            tx_base64: order_response
                .transaction
                .clone()
                .ok_or_else(bad_provider_response)?,
            request_id: Some(order_response.request_id.clone()),
            router: Some(order_response.router.clone()),
            last_valid_block_height: binding["last_valid_block_height"].as_u64(),
            provider_expire_at: binding["provider_expire_at"].as_str().map(str::to_owned),
            expected_output_atoms: parse_atoms(&order_response.out_amount)?,
            actual_slippage_bps: order_response.slippage_bps,
            platform_fee_bps: order_response.fee_bps,
            platform_fee_mint: order_response.fee_mint.clone(),
            wallet_network_fee_lamports: binding["wallet_network_fee_lamports"]
                .as_u64()
                .ok_or_else(bad_provider_response)?,
            wallet_account_funding_lamports: binding["wallet_account_funding_lamports"]
                .as_u64()
                .ok_or_else(bad_provider_response)?,
            order_response,
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

fn bounded_expiry(provider_expire_at: Option<&str>) -> Result<String> {
    let preview_deadline = Utc::now() + ChronoDuration::seconds(20);
    if let Some(provider_expire_at) = provider_expire_at {
        let provider_deadline = chrono::DateTime::parse_from_rfc3339(provider_expire_at)
            .map_err(|_| bad_provider_response())?
            .to_utc();
        if provider_deadline <= Utc::now() {
            return Err(GoFinError::new(
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
    pub baseline_anchors: Vec<gofin_domain::PositionAnchor>,
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
) -> gofin_domain::PositionAnchor {
    gofin_domain::PositionAnchor {
        resource: ResourceId(format!("solana:mainnet-beta:{wallet}")),
        position: format!("account-funding:{address}"),
        position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
        principal: Amount {
            asset: AssetId {
                network: NETWORK.into(),
                reference: "native".into(),
                decimals: 9,
            },
            atoms: gofin_domain::Quantity::from_u64(lamports),
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

#[cfg(test)]
mod rent_baseline_binding_tests {
    use super::*;

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
        gofin_solana::token::TOKEN_PROGRAM_ID,
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
                    let bytes = gofin_solana::token::decode_rpc_account_data(
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
                    let parsed = gofin_solana::token::parse_token_account(
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
                    let mint_data = gofin_solana::token::decode_rpc_account_data(
                        mint_account.get("data").ok_or_else(|| {
                            unsafe_instruction_at("associated-token mint account data")
                        })?,
                    )
                    .map_err(|error| {
                        validation_stage_if_unreviewed(error, "associated-token mint account data")
                    })?;
                    let account_len =
                        gofin_solana::token::required_token_account_len(&a[5].address, &mint_data)
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
            gofin_solana::token::TOKEN_PROGRAM_ID | gofin_solana::token::TOKEN_2022_PROGRAM_ID => {
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
                        let bytes = gofin_solana::token::decode_rpc_account_data(
                            value.get("data").ok_or_else(|| {
                                unsafe_instruction_at("native close account state")
                            })?,
                        )
                        .map_err(|error| {
                            validation_stage_if_unreviewed(error, "native close account state")
                        })?;
                        let parsed = gofin_solana::token::parse_token_account(
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
                                account_owner: gofin_solana::token::TOKEN_PROGRAM_ID.into(),
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
        return Err(GoFinError::new(
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
    accounts: &[gofin_solana::transaction::NormalizedAccountMeta],
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
    if token_program.address != gofin_solana::token::TOKEN_PROGRAM_ID
        && token_program.address != gofin_solana::token::TOKEN_2022_PROGRAM_ID
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

fn unsafe_instruction() -> GoFinError {
    GoFinError::new(
        "JUPITER_INSTRUCTION_UNREVIEWED",
        "prepare",
        "The provider transaction contains an instruction or account effect outside the reviewed swap envelope.",
        "Request a transaction whose complete instruction set and exact token accounts can be verified.",
    )
}

fn validation_stage_if_unreviewed(
    mut error: GoFinError,
    validation_stage: &'static str,
) -> GoFinError {
    if matches!(
        error.code.as_str(),
        "JUPITER_INSTRUCTION_UNREVIEWED" | "JUPITER_INSTRUCTION_UNSUPPORTED"
    ) && error.details.is_none()
    {
        error.details = Some(Box::new(json!({"validation_stage": validation_stage})));
    }
    error
}

fn unsafe_instruction_at(validation_stage: &'static str) -> GoFinError {
    GoFinError::new(
        "JUPITER_INSTRUCTION_UNREVIEWED",
        "prepare",
        format!("The Jupiter route failed {validation_stage} validation."),
        "Request a fresh transaction and review the reported validation stage.",
    )
    .with_details(json!({"validation_stage": validation_stage}))
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
        let signature = response.get("signature").and_then(Value::as_str).ok_or_else(|| GoFinError::new(
            "JUPITER_EXECUTE_UNCERTAIN", "submission",
            "Jupiter did not return a transaction signature after the persisted execute request.",
            "Reconcile the original jupiter request identity; do not submit it again.",
        ))?;
        if status != "Success"
            || bs58::decode(signature)
                .into_vec()
                .map_or(true, |bytes| bytes.len() != 64)
        {
            return Err(GoFinError::new(
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
        guard: &dyn gofin_solana::transaction::AsyncDispatchGuard,
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
        let signature = response.get("signature").and_then(Value::as_str).ok_or_else(|| GoFinError::new(
            "JUPITER_EXECUTE_UNCERTAIN", "submission",
            "Jupiter did not return a transaction signature after the persisted execute request.",
            "Reconcile the original jupiter request identity; do not submit it again.",
        ))?;
        if response.get("status").and_then(Value::as_str) != Some("Success")
            || bs58::decode(signature)
                .into_vec()
                .map_or(true, |bytes| bytes.len() != 64)
        {
            return Err(GoFinError::new(
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
    proof: &gofin_solana::transaction::FinalizedReceiptProof,
) -> Result<gofin_domain::VerifiedEffects> {
    if record
        .preview
        .as_ref()
        .and_then(|p| p.binding.get("provider_trust"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return crate::spot_settlement::verify_swap_effects(
            record, submission, signature, slot, fee, proof,
        );
    }
    let mut legacy_input = record.input.clone();
    if let Some(input) = legacy_input.as_object_mut() {
        input.remove("route");
    }
    let intent = JupiterAdapter::intent(&legacy_input)?;
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
        return Err(GoFinError::new(
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
    let mut effects = vec![gofin_domain::FinancialEffect::Conversion {
        resource: resource.clone(),
        spent: Amount {
            asset: input_asset,
            atoms: gofin_domain::Quantity::new(debit.to_string())?,
        },
        received: Amount {
            asset: output_asset,
            atoms: gofin_domain::Quantity::new(credit.to_string())?,
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
            return Err(GoFinError::new(
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
        let principal = gofin_domain::Quantity::from_u64(requirement.lamports);
        let position = format!("account-funding:{}", requirement.address);
        let anchor = gofin_domain::PositionAnchor {
            resource: resource.clone(),
            position: position.clone(),
            position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
            principal: Amount {
                asset: native.clone(),
                atoms: principal.clone(),
            },
            scaled_receipt: None,
            evidence_version: 1,
            evidence: json!({"cluster":CLUSTER,"account":requirement.address,"account_owner":requirement.account_owner,"refund_authority":requirement.refund_authority,"rent_lamports":requirement.lamports,"proof_signature":signature,"proof_message_sha256":proof.message_sha256}),
            observed_at: Utc::now().to_rfc3339(),
        };
        effects.push(gofin_domain::FinancialEffect::PositionChange {
            resource: resource.clone(),
            position,
            asset: native.clone(),
            delta: gofin_domain::SignedQuantity::positive(&principal),
            scaled_receipt: None,
            anchor: Some(anchor),
        });
        effects.push(gofin_domain::FinancialEffect::OriginMovement {
            from: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            to: AccountingLocation::Position {
                resource: resource.clone(),
                position: format!("account-funding:{}", requirement.address),
                position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
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
                    && maximum_debit == &gofin_domain::Quantity::from_u64(wallet_rent_gross)
            )
        });
    // Keep the debit expectation satisfiable for temporary wrapped-SOL
    // accounts that were funded and closed in this same transaction. The
    // finalized account and wallet balance checks establish that net rent is zero;
    // only the immutable, matching preview constraint justifies emitting zero.
    if wallet_rent_net > 0 || (wallet_rent_net == 0 && has_matching_wallet_funding_debit) {
        effects.push(gofin_domain::FinancialEffect::CashDebit {
            resource: resource.clone(),
            amount: Amount {
                asset: native.clone(),
                atoms: gofin_domain::Quantity::from_u64(wallet_rent_net),
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
            let amount = gofin_domain::Quantity::from_u64(refund.lamports);
            let position = format!("account-funding:{}", refund.address);
            let anchor = gofin_domain::PositionAnchor {
                resource: resource.clone(),
                position: position.clone(),
                position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                principal: Amount {
                    asset: native.clone(),
                    // Position anchors represent post-transaction state. A
                    // closed account has no refundable rent position left.
                    atoms: gofin_domain::Quantity::zero(),
                },
                scaled_receipt: None,
                evidence_version: 1,
                evidence: json!({"cluster":CLUSTER,"account":refund.address,"account_owner":refund.account_owner,"refund_authority":refund.refund_authority,"returned_rent_lamports":refund.lamports,"proof_signature":signature,"proof_message_sha256":proof.message_sha256}),
                observed_at: Utc::now().to_rfc3339(),
            };
            effects.push(gofin_domain::FinancialEffect::PositionChange {
                resource: resource.clone(),
                position: position.clone(),
                asset: native.clone(),
                delta: gofin_domain::SignedQuantity::negative(&amount),
                scaled_receipt: None,
                anchor: Some(anchor),
            });
            effects.push(gofin_domain::FinancialEffect::UsableCredit {
                to: resource.clone(),
                amount: Amount {
                    asset: native.clone(),
                    atoms: amount.clone(),
                },
            });
            effects.push(gofin_domain::FinancialEffect::OriginMovement {
                from: AccountingLocation::Position {
                    resource: resource.clone(),
                    position,
                    position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
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
            return Err(GoFinError::new(
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
            Some(gofin_domain::FinancialEffect::Conversion { received, .. }) => {
                received.atoms = gofin_domain::Quantity::new(credit.to_string())?;
            }
            _ => return Err(bad_provider_response()),
        }
    }
    if credit
        < expected_min
            .parse::<u128>()
            .map_err(|_| bad_provider_response())?
    {
        return Err(GoFinError::new(
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
        return Err(GoFinError::new(
            "JUPITER_NATIVE_BALANCE_MISMATCH",
            "reconciliation",
            "Finalized SOL changes do not conserve the approved swap, fee, account funding and close refunds.",
            "Keep the operation unresolved and inspect the complete finalized account-balance proof.",
        ));
    }
    if submission.data.get("fee_payer").and_then(Value::as_str) == Some(wallet_address) && fee > 0 {
        effects.push(gofin_domain::FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: AssetId {
                    network: NETWORK.into(),
                    reference: "native".into(),
                    decimals: 9,
                },
                atoms: gofin_domain::Quantity::from_u64(fee),
            },
            category: "solana_network_fee".into(),
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    let evidence = json!({"cluster":CLUSTER,"signature":signature,"slot":slot,"network_fee_lamports":fee,"proof":proof,"provider_request_id":submission.data.get("provider_request_id")});
    let mut verified = gofin_domain::VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects,
        evidence,
        observed_at: Utc::now().to_rfc3339(),
        proof_digest: String::new(),
    };
    verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
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
    proof: &gofin_solana::transaction::FinalizedReceiptProof,
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
        if program != gofin_solana::token::TOKEN_PROGRAM_ID
            && program != gofin_solana::token::TOKEN_2022_PROGRAM_ID
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
            return Err(GoFinError::new(
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
    runtime: &gofin_solana::transaction::SolanaRuntime,
    mint: &str,
) -> Result<u8> {
    if mint == "native" {
        return Ok(9);
    }
    let address = gofin_solana::parse_address(mint)?;
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
    let data = gofin_solana::token::decode_rpc_account_data(
        account.get("data").ok_or_else(bad_provider_response)?,
    )?;
    Ok(gofin_solana::token::parse_mint(owner, &data, epoch)?.decimals)
}

pub(crate) async fn token_program_for(
    runtime: &gofin_solana::transaction::SolanaRuntime,
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
    if owner != gofin_solana::token::TOKEN_PROGRAM_ID
        && owner != gofin_solana::token::TOKEN_2022_PROGRAM_ID
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
                     quote: &gofin_pricing::UsdQuote|
     -> Result<gofin_domain::Quantity> {
        let unit = gofin_domain::Quantity::from_decimal(&quote.usd_per_unit, 36)?;
        let denominator = gofin_domain::Quantity::new(format!(
            "1{}",
            "0".repeat(usize::from(asset.decimals) + 30)
        ))?;
        gofin_domain::Quantity::from_u64(atoms).mul_div_ceil(&unit, &denominator)
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
        atoms: gofin_domain::Quantity::from_u64(fee_lamports),
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
            additional_allocation_usd_micros: gofin_domain::Quantity::zero(),
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
    quote: &gofin_pricing::UsdQuote,
) -> Result<gofin_domain::AssetPrice> {
    Ok(gofin_domain::AssetPrice {
        asset: asset.clone(),
        usd_micros_per_unit: gofin_domain::Quantity::from_decimal(&quote.usd_per_unit, 36)?,
        // Numerator is USD-per-whole-unit scaled by 10^36. Keep the matching
        // 30-place denominator scale so value = atoms * price * 10^6 / 10^d.
        unit_atoms: gofin_domain::Quantity::new(
            "1".to_owned() + &"0".repeat(usize::from(asset.decimals) + 30),
        )?,
        observed_at: quote.updated_at.clone(),
        source: quote.source.clone(),
    })
}

fn swap_operation_definitions() -> Vec<Definition> {
    let schema = swap::definition_input_schema();
    let quote_schema = json!({"type":"object","additionalProperties":false,"required":["wallet","network","cluster","input_mint","output_mint","amount_atoms"],"properties":{"wallet":{"type":"string","minLength":1},"network":{"const":"solana"},"cluster":{"const":"mainnet-beta"},"input_mint":{"type":"string","description":"SPL mint or native SOL; explicit WSOL unsupported."},"output_mint":{"type":"string","description":"SPL mint or native SOL; explicit WSOL unsupported."},"amount_atoms":{"type":"string","pattern":"^[0-9]+$"},"maximum_slippage_bps":{"type":"integer","minimum":0,"maximum":10000}}});
    let output = swap::definition_output_schema();
    let mut definitions: Vec<_> = [OP_QUOTE, OP_SWAP]
        .into_iter()
        .map(|name| Definition {
            name: name.into(),
            description: if name == OP_QUOTE {
                "Get current Jupiter pricing even when funds are insufficient, with transaction availability, provider errors and retry guidance.".into()
            } else {
                "Prepare and submit one bounded Jupiter Swap v2 conversion through an enrolled Solana wallet.".into()
            },
            kind: if name == OP_QUOTE { OperationKind::Read } else { OperationKind::Write },
            execution_actor: if name == OP_QUOTE { ExecutionActor::AgentReadable } else { ExecutionActor::AgentInvocable },
            input_schema: if name == OP_QUOTE { quote_schema.clone() } else { schema.clone() },
            output_schema: if name == OP_QUOTE { swap::definition_quote_output_schema() } else { output.clone() },
            error_schema: json!({"type":"object","required":["code","stage","message","next_action"]}),
            examples: vec![if name == OP_QUOTE {
                json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","input_mint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","output_mint":"2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo","amount_atoms":"1000000"})
            } else {
                json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","input_mint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","output_mint":"2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo","amount_atoms":"1000000","minimum_output_atoms":"900000","maximum_slippage_bps":100,"maximum_platform_fee_bps":0,"maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0})
            }],
            coverage: "Exact input SOL/SPL swaps through Jupiter Meta-Aggregator /order and /execute, with RTSE and financial limits checked against the order response. Jupiter is trusted to construct the transaction.".into(),
            completion: if name == OP_QUOTE { "Provider pricing only; transaction_available and provider_error report construction blockers. After required funds arrive, retry for a fresh quote. No financial submission or execution authority is created.".into() } else { "Completed only after finalized chain evidence proves the exact input debit, minimum output credit and bounded network fee.".into() },
            semantics: json!({"network":NETWORK,"cluster":CLUSTER,"mainnet":true,"integration":"meta_aggregator_order_execute","slippage":"jupiter_rtse_checked_against_approved_maximum","transaction_validation":"provider_trusted","keyless_default":true,"uncertain_submission":"reconcile_original_identity_no_retry","quote_is_settlement":false}),
        })
        .collect();
    if let Some(swap) = definitions
        .iter_mut()
        .find(|definition| definition.name == OP_SWAP)
    {
        swap.semantics["plan_allowance_seconds"] =
            json!(gofin_domain::plan_timing::ONCHAIN_ALLOWANCE_SECONDS);
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
    proof: &gofin_solana::transaction::FinalizedReceiptProof,
) -> Result<gofin_domain::VerifiedEffects> {
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
            return Err(GoFinError::new(
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
    let mut verified = gofin_domain::VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects: if wallet_is_fee_payer && fee > 0 {
            vec![gofin_domain::FinancialEffect::Cost {
                location: AccountingLocation::Cash { resource },
                amount: Amount {
                    asset,
                    atoms: gofin_domain::Quantity::from_u64(fee),
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
    verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
    Ok(verified)
}

fn wallet_token_mints(
    proof: &gofin_solana::transaction::FinalizedReceiptProof,
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
) -> Result<gofin_domain::VerifiedEffects> {
    let evidence = json!({"state":"not_dispatched","reason_code":reason_code,"reason":reason,"financial_effects":[]});
    let mut verified = gofin_domain::VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects: Vec::new(),
        evidence,
        observed_at: Utc::now().to_rfc3339(),
        proof_digest: String::new(),
    };
    verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
    Ok(verified)
}

fn candidate_mint(mint: &str) -> Result<String> {
    if mint == "native" {
        Ok(swap::WRAPPED_SOL_MINT.into())
    } else {
        gofin_solana::parse_address(mint)
    }
}

fn normalize_asset(mint: &str) -> String {
    if mint == "native" {
        "native".into()
    } else {
        mint.into()
    }
}

fn token_approval_metadata(assets: &[&AssetId], response: &Value) -> Vec<Value> {
    let Some(tokens) = response.as_array() else {
        return Vec::new();
    };
    assets
        .iter()
        .filter_map(|asset| {
            if asset.reference == "native" {
                return None;
            }
            let mut matches = tokens
                .iter()
                .filter(|token| token["id"].as_str() == Some(&asset.reference));
            let token = matches.next()?;
            // Ambiguous duplicate entries are not trustworthy presentation metadata.
            if matches.next().is_some()
                || token["decimals"].as_u64() != Some(u64::from(asset.decimals))
            {
                return None;
            }
            let symbol = token["symbol"].as_str()?;
            if symbol.is_empty()
                || symbol.len() > 32
                || !symbol
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
            {
                return None;
            }
            Some(json!({"name": symbol, "identifier": asset}))
        })
        .collect()
}

#[cfg(test)]
mod approval_metadata_tests {
    use super::*;

    #[test]
    fn provider_names_require_exact_mint_decimals_and_safe_symbol() {
        let ore = AssetId {
            network: NETWORK.into(),
            reference: "ore-mint".into(),
            decimals: 11,
        };
        let metadata = token_approval_metadata(
            &[&ore],
            &json!([{ "id": "ore-mint", "symbol": "ORE", "decimals": 11 }]),
        );
        assert_eq!(metadata, vec![json!({"name":"ORE", "identifier":ore})]);
        for response in [
            json!([]),
            json!([{ "id":"other", "symbol":"ORE", "decimals":11 }]),
            json!([{ "id":"ore-mint", "symbol":"ORE", "decimals":9 }]),
            json!([{ "id":"ore-mint", "symbol":"bad\nname", "decimals":11 }]),
            json!([{ "id":"ore-mint", "symbol":"ORE", "decimals":11 }, { "id":"ore-mint", "symbol":"FAKE", "decimals":11 }]),
        ] {
            assert!(token_approval_metadata(&[&ore], &response).is_empty());
        }
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
        gofin_domain::product_names::remove(
            binding,
            gofin_domain::financial_plans::PLAN_METADATA_KEY,
        );
        gofin_domain::product_names::remove(
            binding,
            gofin_domain::boundaries::BOUNDARY_METADATA_KEY,
        );
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

fn unknown_operation() -> GoFinError {
    GoFinError::new(
        "JUPITER_OPERATION_UNKNOWN",
        "operation",
        "This Jupiter operation is not implemented by the selected adapter.",
        "Inspect GoFin discovery for the supported Jupiter operations.",
    )
}

fn bad_provider_response() -> GoFinError {
    GoFinError::new(
        "JUPITER_RESPONSE_INVALID",
        "prepare",
        "Jupiter returned incomplete or invalid transaction data.",
        "Request a fresh quote and review its exact transaction before approval.",
    )
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
        broker_marked.binding[gofin_domain::financial_plans::PLAN_METADATA_KEY] =
            json!({"plan_id":"plan"});
        broker_marked.binding[gofin_domain::boundaries::BOUNDARY_METADATA_KEY] =
            json!({"authorization_id":"boundary"});
        assert_eq!(frozen_candidate_cache_key(&broker_marked), Some(key));

        let mut enriched = broker_marked;
        enriched.valuation = Some(Valuation {
            movement_usd_micros: gofin_domain::Quantity::zero(),
            maximum_fee_usd_micros: gofin_domain::Quantity::zero(),
            additional_allocation_usd_micros: gofin_domain::Quantity::zero(),
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
            gofin_domain::plan_timing::ONCHAIN_ALLOWANCE_SECONDS
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
                gofin_domain::plan_timing::ONCHAIN_ALLOWANCE_SECONDS
            );
        }
    }

    #[test]
    fn associated_account_graph_binds_system_program_payer_and_authority() {
        use gofin_solana::transaction::NormalizedAccountMeta as Meta;

        let wallet = "Vote111111111111111111111111111111111111111";
        let mint = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
        let token_program = gofin_solana::token::TOKEN_PROGRAM_ID;
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
        async fn quote(&self, asset: &AssetId) -> Result<gofin_pricing::UsdQuote> {
            Ok(gofin_pricing::UsdQuote {
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

        async fn quotes(&self, assets: &[AssetId]) -> Result<Vec<gofin_pricing::UsdQuote>> {
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
        let quote = gofin_pricing::UsdQuote {
            usd_per_unit: "12.34".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            observed_at: "2026-01-01T00:00:00Z".into(),
            source: "synthetic-test".into(),
        };
        let pair = valuation_asset_price(&asset, &quote).unwrap();
        let whole_token_atoms = gofin_domain::Quantity::from_u64(1_000_000_000);
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
        let token_program = gofin_solana::token::TOKEN_PROGRAM_ID;
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
                        maximum_input: gofin_domain::Quantity::from_u64(2_000_000),
                        output: native.clone(),
                        minimum_output: gofin_domain::Quantity::from_u64(20_000_000),
                    },
                    EffectConstraint::PositionOfKind {
                        resource: resource.clone(),
                        position: format!("account-funding:{wsol_ata}"),
                        asset: native.clone(),
                        position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                        direction: gofin_domain::Direction::Decrease,
                        maximum_change: gofin_domain::Quantity::from_u64(2_039_280),
                    },
                    EffectConstraint::OriginMovement {
                        from: AccountingLocation::Position {
                            resource: resource.clone(),
                            position: format!("account-funding:{wsol_ata}"),
                            position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                        },
                        to: AccountingLocation::Cash {
                            resource: resource.clone(),
                        },
                        debit_asset: native.clone(),
                        credit_asset: native.clone(),
                        maximum_debit: gofin_domain::Quantity::from_u64(2_039_280),
                        minimum_credit: gofin_domain::Quantity::from_u64(2_039_280),
                    },
                    EffectConstraint::Credit {
                        to: resource.clone(),
                        asset: native.clone(),
                        minimum_credit: gofin_domain::Quantity::from_u64(2_039_280),
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
            authorization_source: None,
            submission: Some(submission.clone()),
            dispatch_evidence: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
            position_baseline: None,
        };
        let proof = gofin_solana::transaction::FinalizedReceiptProof {
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
            gofin_domain::FinancialEffect::Conversion { received, .. } if received.atoms.as_str()=="20000000")));
        assert!(verified.effects.iter().any(|effect| matches!(effect,
            gofin_domain::FinancialEffect::UsableCredit { amount, .. } if amount.atoms.as_str()=="2039280")));
        assert!(verified.effects.iter().any(|effect| matches!(effect,
            gofin_domain::FinancialEffect::PositionChange { delta, anchor:Some(anchor), .. }
                if delta.is_negative() && delta.magnitude().as_str()=="2039280"
                    && anchor.principal.atoms.is_zero())));
        assert!(verified.effects.iter().any(|effect| matches!(effect,
            gofin_domain::FinancialEffect::Cost { amount, .. } if amount.atoms.as_str()=="5000")));
        assert_eq!(
            verified
                .effects
                .iter()
                .filter(|effect| matches!(
                    effect,
                    gofin_domain::FinancialEffect::UsableCredit { .. }
                ))
                .count(),
            1
        );
        assert!(!verified.effects.iter().any(|effect| matches!(effect,
            gofin_domain::FinancialEffect::CashDebit { amount, .. } if amount.atoms.is_zero())));

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
                atoms: gofin_domain::Quantity::from_u64(refund.lamports),
            },
        }];
        let conversion = transient_preview.intent.expected[0].clone();
        transient_preview.intent.expected = vec![
            conversion,
            EffectConstraint::Debit {
                resource: resource.clone(),
                asset: native.clone(),
                maximum_debit: gofin_domain::Quantity::from_u64(refund.lamports),
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
            gofin_domain::FinancialEffect::CashDebit { resource: actual_resource, amount }
                if actual_resource == &resource && amount.asset == native && amount.atoms.is_zero())));
        assert!(
            transient.effects.iter().any(|effect| matches!(effect,
            gofin_domain::FinancialEffect::Cost { amount, .. } if amount.atoms.as_str() == "5000"))
        );

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
            gofin_domain::FinancialEffect::CashDebit { amount, .. } if amount.atoms.is_zero()))
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
        let mut direct_proof = gofin_solana::transaction::FinalizedReceiptProof {
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
            gofin_domain::FinancialEffect::Conversion { received, .. } if received.atoms.as_str()=="20000000")));
        assert!(
            !direct
                .effects
                .iter()
                .any(|effect| matches!(effect, gofin_domain::FinancialEffect::UsableCredit { .. }))
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
}

#[cfg(test)]
mod provider_trust_tests {
    use super::*;
    struct MetaRpc;
    #[async_trait]
    impl gofin_solana::SolanaRpc for MetaRpc {
        async fn request(&self, method: &str, _params: Value) -> Result<Value> {
            match method {
                "getBlockHeight" => Ok(json!(100)),
                "isBlockhashValid" => Ok(json!(true)),
                other => panic!(
                    "provider-trusted candidate must not inspect accounts, fees, or simulate: {other}"
                ),
            }
        }
    }
    struct NoPrices;
    #[async_trait]
    impl UsdPriceSource for NoPrices {
        async fn quote(&self, _asset: &AssetId) -> Result<gofin_pricing::UsdQuote> {
            panic!("freeze does not value a candidate")
        }
    }
    struct OrderApi {
        response: Value,
        requests: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl JupiterApi for OrderApi {
        async fn request(
            &self,
            host: JupiterHost,
            method: Method,
            path: &str,
            body: Option<Value>,
            bucket: RateBucket,
            priority: RequestPriority,
            _deadline: Duration,
        ) -> Result<Value> {
            assert_eq!(host, JupiterHost::Platform);
            assert_eq!(method, Method::GET);
            assert!(body.is_none());
            let _ = (bucket, priority);
            assert!(path.starts_with("/swap/v2/order?"));
            assert!(!path.contains("slippageBps"));
            assert!(!path.contains("excludeRouters"));
            self.requests.lock().unwrap().push(path.into());
            Ok(self.response.clone())
        }
    }
    fn meta_fixture() -> (SwapIntent, Arc<SolanaWalletAdapter>) {
        // RFC 8032 public test vector, never a funded account.
        let key = hex::decode(concat!(
            "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        ))
        .unwrap();
        let signer =
            Arc::new(gofin_solana::SolanaSigner::parse(&bs58::encode(key).into_string()).unwrap());
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
            input_mint: "native".into(),
            output_mint: bs58::encode([60u8; 32]).into_string(),
            amount_atoms: "10".into(),
            minimum_output_atoms: "90".into(),
            maximum_slippage_bps: 100,
            maximum_platform_fee_bps: 0,
            maximum_network_fee_lamports: 5000,
            maximum_account_funding_lamports: 0,
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
        let program = bs58::decode(bs58::encode([77u8; 32]).into_string())
            .into_vec()
            .unwrap();
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

    #[tokio::test]
    async fn provider_transaction_with_unknown_program_is_frozen_without_review_or_simulation() {
        let (intent, wallet) = meta_fixture();
        let transaction = serialized_route_transaction(wallet.address(), &[255, 77, 0, 42]);
        let response = json!({"transaction":transaction, "requestId":"synthetic-order", "inputMint":swap::WRAPPED_SOL_MINT, "outputMint":intent.output_mint, "inAmount":intent.amount_atoms, "outAmount":"100", "otherAmountThreshold":"99", "swapMode":"ExactIn", "slippageBps":100, "taker":wallet.address(), "router":"new-provider-engine", "lastValidBlockHeight":123, "feeBps":0, "signatureFeeLamports":5000, "signatureFeePayer":wallet.address(), "prioritizationFeeLamports":0, "prioritizationFeePayer":null, "rentFeeLamports":0, "rentFeePayer":null});
        let api = Arc::new(OrderApi {
            response,
            requests: Mutex::new(Vec::new()),
        });
        let adapter = JupiterAdapter::with_price_source(
            BTreeMap::from([("synthetic".into(), wallet.clone())]),
            api.clone(),
            Arc::new(NoPrices),
        )
        .unwrap();
        let prepared = adapter.prepare_candidate(&intent, &wallet).await.unwrap();
        assert_eq!(prepared.candidate.tx_base64, transaction);
        assert_eq!(
            prepared.candidate.router.as_deref(),
            Some("new-provider-engine")
        );
        assert!(prepared.frozen.instructions().is_empty());
        assert_eq!(
            prepared.frozen.provider_request_id(),
            Some("synthetic-order")
        );
        assert_eq!(api.requests.lock().unwrap().len(), 1);
    }
}
