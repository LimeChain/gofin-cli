//! Operation orchestration and the shared financial effect ledger.
//!
//! Application adapters interpret venue receipts and produce `VerifiedEffects`.
//! The engine checks that those effects satisfy the frozen intent, the source
//! reservations and the durable submission identity before settlement.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use fin_domain::boundaries::{
    self, BoundaryAttemptSummary, BoundaryAuthorization, BoundaryAuthorizationRecord,
    BoundaryAuthorizationRequest, BoundaryAuthorizationStatus, BoundaryPreview, BoundarySpec,
    BoundaryStatus,
};
use fin_domain::financial_plans::{
    FinancialPlanAuthorizationRecord, FinancialPlanAuthorizationStatus, FinancialPlanRevision,
};
use fin_domain::{
    AccountingLocation, Adapter, Amount, ApprovalAudit, AssetPrice, AuthorizationChallenge,
    CheckStatus, CostBound, CostTreatment, Definition, Direction, EffectConstraint, FinError,
    FinancialEffect, FinancialIntent, OperationKind, OperationRecord, OperationRequest, PlanStep,
    PositionAnchor, PositionKind, PreparationContext, PreparedOperation, ProjectedCash,
    ProjectedEffect, ProjectedPosition, ProjectedRequirement, ProjectedState, QuantityInterval,
    Reconciliation, ResourceAmount, ResourceId, Result, ReturnRouteContext, ReturnRouteProof,
    StateSnapshot, Status, Submission, SubmissionJournal, VerifiedEffects, authorization_binding,
    binding_hash, now, request_fingerprint, verified_effect_digest,
};
use fin_store::{RecoveryReport, ReservationRow, Settlement, Store, StoreError, SubmissionRow};
use num_bigint::{BigInt, BigUint, Sign};
use num_traits::Zero;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

mod boundary_authorizations;
#[cfg(test)]
mod boundary_dispatch_tests;
mod cash_reservation_sources;
mod financial_plans;
mod ordinary_policy;
use ordinary_policy::reject_unpriced_run;
mod projected_provenance;
mod projected_shares;
use financial_plans::{permission_for_step, plan_error, validate_candidate_plan_caps};

const DEFAULT_LIMIT_MICROS: u64 = fin_domain::policy::DEFAULT_POLICY_LIMIT_USD_MICROS;
fn legacy_movement_limit() -> String {
    fin_domain::policy::DEFAULT_POLICY_LIMIT_USD_MICROS.to_string()
}
const MAX_OBSERVATION_AGE_SECONDS: i64 = 120;

#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// Maximum single gross movement plus maximum costs under personal policy.
    pub max_movement_and_cost_usd_micros: String,
    /// Installation-wide rolling 24-hour gross allocation, including maximum costs and additional borrowing.
    pub max_run_allocation_usd_micros: String,
    /// Protected human-approved exception for explicit unpriced exchange estimates.
    pub allow_unpriced_exchange_assets: bool,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            max_movement_and_cost_usd_micros: DEFAULT_LIMIT_MICROS.to_string(),
            max_run_allocation_usd_micros: DEFAULT_LIMIT_MICROS.to_string(),
            allow_unpriced_exchange_assets: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunAllocation {
    pub holding: ResourceAmount,
    pub value_usd_micros: String,
    pub observed_at: String,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunPlan {
    pub id: String,
    pub baseline: StateSnapshot,
    pub allocations: Vec<RunAllocation>,
    pub borrowed_proceeds_reserve_usd_micros: String,
    pub return_cost_reserve_usd_micros: String,
    pub planned_operations: Vec<PlanStep>,
    pub unwind: Vec<PlanStep>,
    /// Filled by the trusted broker after fresh adapter preflight; caller content is ignored.
    #[serde(default)]
    pub route_proofs: Vec<ReturnRouteProof>,
    /// Filled by the broker's trusted AssetValuator; caller content is ignored by observer entrypoints.
    #[serde(default)]
    pub asset_prices: Vec<AssetPrice>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Active,
    CleanupOutstanding,
    Restored,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RunReservation {
    debits: Vec<ResourceAmount>,
    maximum_costs: Vec<CostBound>,
    #[serde(default)]
    provenance: Vec<ProvenanceLot>,
    #[serde(default)]
    additional_cost_provenance: Vec<ProvenanceLot>,
    #[serde(default)]
    cash_provenance_version: u32,
    #[serde(default)]
    cash_sources: Option<cash_reservation_sources::CashReservationSources>,
    maximum_fee_usd_micros: String,
    additional_allocation_usd_micros: String,
    binding_hash: String,
    #[serde(default)]
    route_step_index: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RunLedger {
    #[serde(default)]
    purpose: Option<Value>,
    #[serde(default = "legacy_movement_limit")]
    max_movement_usd_micros: String,
    id: String,
    status: RunStatus,
    baseline: StateSnapshot,
    initial_allocation_usd_micros: String,
    borrowed_proceeds_reserve_usd_micros: String,
    return_cost_reserve_usd_micros: String,
    #[serde(default)]
    planned_operations: Vec<PlanStep>,
    #[serde(default)]
    unwind: Vec<PlanStep>,
    #[serde(default)]
    route_proofs: Vec<ReturnRouteProof>,
    #[serde(default)]
    asset_prices: Vec<AssetPrice>,
    #[serde(default)]
    settled_route_steps: Vec<u32>,
    #[serde(default)]
    route_invalidated: bool,
    // Accept legacy snapshots for conservative migration. Legacy route declarations
    // are deliberately discarded and never authorize a scoped write.
    #[serde(default, skip_serializing)]
    #[serde(rename = "unwind_steps")]
    legacy_unwind_steps: Vec<String>,
    #[serde(default, skip_serializing)]
    #[serde(rename = "return_routes_verified")]
    legacy_return_routes_verified: bool,
    allocations: Vec<RunAllocation>,
    available: Vec<ResourceAmount>,
    reservations: BTreeMap<String, RunReservation>,
    costs: Vec<ResourceAmount>,
    spent_cost_usd_micros: String,
    borrowed_allocation_used_usd_micros: String,
    final_state: Option<StateSnapshot>,
    final_observed_at: Option<String>,
    differences: BTreeMap<String, String>,
    #[serde(default)]
    provenance: Vec<ProvenanceLot>,
    #[serde(default)]
    provenance_events: Vec<ProvenanceEvent>,
    #[serde(default)]
    supply_accruals: Vec<ResourceAmount>,
    #[serde(default)]
    position_anchors: Vec<PositionAnchor>,
    #[serde(default)]
    route_revisions: Vec<RouteRevision>,
    #[serde(default)]
    net_conversion_deltas: Vec<RunConversionDelta>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RouteRevision {
    changed_at: String,
    replaced_from_step: u32,
    superseded_steps: Vec<PlanStep>,
    superseded_proofs: Vec<ReturnRouteProof>,
    replacement_steps: Vec<PlanStep>,
    replacement_proofs: Vec<ReturnRouteProof>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProvenanceLot {
    origin: String,
    location: AccountingLocation,
    asset: fin_domain::AssetId,
    atoms: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProvenanceEvent {
    operation_id: String,
    from: AccountingLocation,
    to: AccountingLocation,
    debit: Amount,
    credit: Amount,
    origins: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunResult {
    pub id: String,
    pub status: RunStatus,
    pub differences: BTreeMap<String, String>,
    /// Exact net converted-principal change in each original allocation asset,
    /// separate from receipt-proved fees and supply yield.
    #[serde(default)]
    pub net_conversion_deltas: Vec<RunConversionDelta>,
    pub baseline_observed_at: String,
    pub final_observed_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunConversionDelta {
    pub resource: ResourceId,
    pub asset: fin_domain::AssetId,
    pub atoms: fin_domain::SignedQuantity,
}

pub struct Engine {
    store: Arc<Mutex<Store>>,
    adapters: RwLock<HashMap<String, Arc<dyn Adapter>>>,
    authorizer: Arc<dyn fin_domain::Authorizer>,
    options: EngineOptions,
    cost_origin_price_provider: Option<Arc<dyn TrustedAssetPriceProvider>>,
    write_queue: AsyncMutex<()>,
    instance_id: uuid::Uuid,
}

/// An observer capture bound to the engine that created it. The broker may
/// inspect the immutable snapshot to validate allocations and fetch prices,
/// then must consume this value when asking the engine to begin a run. It is
/// intentionally non-serializable and cannot be constructed or cloned by a
/// caller, so a `RunPlan.baseline` never becomes an authority source.
pub struct ObservedBaseline {
    engine_id: uuid::Uuid,
    snapshot: StateSnapshot,
}

impl ObservedBaseline {
    pub fn snapshot(&self) -> &StateSnapshot {
        &self.snapshot
    }
}

/// Broker-owned source for exact, current prices used to map a proved cost
/// back to the selected allocation asset that funded it.
#[async_trait]
pub trait TrustedAssetPriceProvider: Send + Sync {
    async fn asset_price(&self, asset: &fin_domain::AssetId) -> Result<AssetPrice>;
}

impl Engine {
    pub fn new(
        mut store: Store,
        adapters: Vec<Arc<dyn Adapter>>,
        authorizer: Arc<dyn fin_domain::Authorizer>,
        options: EngineOptions,
    ) -> Result<Self> {
        let mut registry = HashMap::new();
        for adapter in adapters {
            for definition in adapter.definitions() {
                if registry
                    .insert(definition.name.clone(), adapter.clone())
                    .is_some()
                {
                    return Err(FinError::new(
                        "DUPLICATE_OPERATION",
                        "discovery",
                        format!(
                            "Operation '{}' is registered more than once.",
                            definition.name
                        ),
                        "Remove the duplicate operation definition.",
                    ));
                }
            }
        }
        store
            .recover_boundary_authorization_requests(&now())
            .map_err(map_store)?;
        store
            .recover_financial_plan_requests(&now())
            .map_err(map_store)?;
        Ok(Self {
            store: Arc::new(Mutex::new(store)),
            adapters: RwLock::new(registry),
            authorizer,
            options,
            cost_origin_price_provider: None,
            write_queue: AsyncMutex::new(()),
            instance_id: uuid::Uuid::new_v4(),
        })
    }

    /// Attach the trusted provider before the engine is shared with request
    /// handlers. Synthetic engine tests may intentionally omit one.
    pub fn with_cost_origin_price_provider(
        mut self,
        provider: Arc<dyn TrustedAssetPriceProvider>,
    ) -> Self {
        self.cost_origin_price_provider = Some(provider);
        self
    }

    pub fn register(&self, adapter: Arc<dyn Adapter>) -> Result<()> {
        let mut registry = self
            .adapters
            .write()
            .map_err(|_| internal("registry lock poisoned"))?;
        for definition in adapter.definitions() {
            if registry.contains_key(&definition.name) {
                return Err(FinError::new(
                    "DUPLICATE_OPERATION",
                    "discovery",
                    format!(
                        "Operation '{}' is registered more than once.",
                        definition.name
                    ),
                    "Remove the duplicate operation definition.",
                ));
            }
        }
        for definition in adapter.definitions() {
            registry.insert(definition.name, adapter.clone());
        }
        Ok(())
    }

    pub fn definitions(&self) -> Result<Vec<Definition>> {
        let registry = self
            .adapters
            .read()
            .map_err(|_| internal("registry lock poisoned"))?;
        let mut definitions = registry
            .values()
            .flat_map(|adapter| adapter.definitions())
            .collect::<Vec<_>>();
        definitions.sort_by(|a, b| a.name.cmp(&b.name));
        definitions.dedup_by(|a, b| a.name == b.name);
        Ok(definitions)
    }

    pub async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
        let (adapter, kind) = self.resolve(operation)?;
        if kind != OperationKind::Read {
            return Err(FinError::new(
                "WRITE_REQUIRES_APPROVAL",
                "validation",
                "This operation changes financial state.",
                "Prepare and execute it through the approval path.",
            ));
        }
        adapter.read(operation, input).await
    }

    pub async fn preview(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        self.preview_with_context(operation, input, None).await
    }

    pub async fn preview_with_context(
        &self,
        operation: &str,
        input: &Value,
        run_id: Option<&str>,
    ) -> Result<PreparedOperation> {
        let (adapter, kind) = self.resolve(operation)?;
        if kind != OperationKind::Write {
            return Err(FinError::new(
                "READ_OPERATION",
                "validation",
                "This operation is not a financial write.",
                "Use the read or configuration command for this operation.",
            ));
        }
        let context = self.preparation_context(run_id)?;
        let mut preview = adapter
            .prepare_with_context(operation, input, &context)
            .await?;
        let required_origins = self.cost_origin_assets(&preview, run_id)?;
        self.complete_cost_origin_prices(&mut preview, &required_origins)
            .await?;
        validate_prepared(&preview, operation, Utc::now())?;
        Ok(preview)
    }

    fn cost_origin_assets(
        &self,
        prepared: &PreparedOperation,
        run_id: Option<&str>,
    ) -> Result<BTreeSet<fin_domain::AssetId>> {
        let Some(run_id) = run_id else {
            return Ok(BTreeSet::new());
        };
        if prepared.intent.maximum_costs.is_empty() {
            return Ok(BTreeSet::new());
        }
        let run = self.load_run(run_id)?;
        let selected = select_cash_reservation_provenance(&run, &prepared.intent)?;
        let selected_origins = run
            .allocations
            .iter()
            .map(|allocation| {
                resource_asset_key(&allocation.holding).map(|key| format!("allocation:{key}"))
            })
            .collect::<Result<BTreeSet<_>>>()?;
        let mut assets = BTreeSet::new();
        for bound in &prepared.intent.maximum_costs {
            let sources = match (&bound.location, &bound.treatment) {
                (AccountingLocation::Cash { .. }, CostTreatment::AdditionalDebit) => selected
                    .additional_costs
                    .iter()
                    .filter(|lot| lot.location == bound.location && lot.asset == bound.amount.asset)
                    .collect::<Vec<_>>(),
                (AccountingLocation::Cash { .. }, CostTreatment::IncludedInDebit) => selected
                    .principal
                    .iter()
                    .filter(|lot| lot.location == bound.location && lot.asset == bound.amount.asset)
                    .collect::<Vec<_>>(),
                (AccountingLocation::Position { .. }, CostTreatment::IncludedInDebit) => run
                    .provenance
                    .iter()
                    .filter(|lot| {
                        lot.location == bound.location
                            && lot.asset == bound.amount.asset
                            && !lot.origin.starts_with("accrual:")
                            && selected_origins.contains(&lot.origin)
                    })
                    .collect::<Vec<_>>(),
                (AccountingLocation::Cash { .. }, CostTreatment::DeductedFromCredit) => {
                    selected.principal.iter().collect::<Vec<_>>()
                }
                (AccountingLocation::Position { .. }, CostTreatment::DeductedFromCredit) => {
                    return Err(route_proof_error());
                }
                (AccountingLocation::Position { .. }, CostTreatment::AdditionalDebit) => {
                    return Err(FinError::new(
                        "POSITION_ADDITIONAL_COST_UNSUPPORTED",
                        "budget",
                        "An additional cost cannot be charged directly to a position.",
                        "Use a supported cash cost source.",
                    ));
                }
            };
            for lot in sources {
                let Some(key) = allocation_origin_key(&lot.origin) else {
                    continue;
                };
                let (_, origin_asset) = decode_resource_asset_key(key)?;
                if origin_asset != bound.amount.asset {
                    assets.insert(origin_asset);
                }
            }
        }
        Ok(assets)
    }

    async fn complete_cost_origin_prices(
        &self,
        prepared: &mut PreparedOperation,
        required_assets: &BTreeSet<fin_domain::AssetId>,
    ) -> Result<()> {
        if required_assets.is_empty() {
            return Ok(());
        }
        let provider = self
            .cost_origin_price_provider
            .as_ref()
            .ok_or_else(cost_origin_price_unavailable)?;
        {
            let valuation = prepared.valuation.as_mut().ok_or_else(|| {
                FinError::new(
                    "COST_ORIGIN_PRICE_MISSING",
                    "budget",
                    "A selected cost origin requires a trusted exact asset price before approval.",
                    "Refresh the preview with a trusted source-asset price.",
                )
            })?;
            for asset in required_assets {
                let price = provider
                    .asset_price(asset)
                    .await
                    .map_err(|_| cost_origin_price_unavailable())?;
                if price.asset != *asset {
                    return Err(FinError::new(
                        "COST_ORIGIN_PRICE_INVALID",
                        "budget",
                        "The trusted price provider returned a different asset identity.",
                        "Refresh using the exact source asset identity.",
                    ));
                }
                validate_price_rows(
                    std::slice::from_ref(&price),
                    &BTreeSet::from([asset.clone()]),
                    Utc::now(),
                )
                .map_err(|error| {
                    if error.code == "COST_VALUATION_STALE" {
                        FinError::new(
                            "COST_ORIGIN_PRICE_STALE",
                            "budget",
                            "The selected cost origin has no fresh exact price observation.",
                            "Refresh the source-asset price and preview again.",
                        )
                    } else {
                        FinError::new(
                            "COST_ORIGIN_PRICE_INVALID",
                            "budget",
                            "The selected cost origin price is malformed or incomplete.",
                            "Refresh a positive exact price for the selected source asset.",
                        )
                    }
                })?;
                valuation.prices.retain(|row| row.asset != *asset);
                valuation.prices.push(price);
            }
        }
        let mut all_required = required_assets.clone();
        all_required.extend(prepared_cost_assets(&prepared.intent));
        validate_cost_prices(prepared, &all_required, Utc::now())?;
        Ok(())
    }

    /// Capture current complete state through a registered integration observer.
    pub async fn snapshot(&self, operation: &str) -> Result<StateSnapshot> {
        let (adapter, _) = self.resolve(operation)?;
        adapter.snapshot().await
    }

    /// Capture an observer-owned baseline for a trusted broker to inspect
    /// before it values a run. The capture remains bound to this engine and
    /// must be consumed; it is not a serializable or caller-constructible
    /// replacement for the ordinary observer path.
    pub async fn observe_run_baseline(&self, operation: &str) -> Result<ObservedBaseline> {
        Ok(ObservedBaseline {
            engine_id: self.instance_id,
            snapshot: self.snapshot(operation).await?,
        })
    }

    /// Begin a bounded run using only a snapshot obtained by the registered adapter.
    /// The caller-provided `RunPlan.baseline` is replaced by the observer result.
    pub async fn begin_run_from_observer(
        &self,
        operation: &str,
        plan: RunPlan,
        now_instant: DateTime<Utc>,
    ) -> Result<RunResult> {
        self.begin_run_from_observer_with_asset_prices(operation, plan, vec![], now_instant)
            .await
    }

    /// Begin a run with baseline and prices supplied by trusted broker services.
    /// Any corresponding values in the client plan are replaced before route proof.
    pub async fn begin_run_from_observer_with_asset_prices(
        &self,
        operation: &str,
        plan: RunPlan,
        trusted_asset_prices: Vec<AssetPrice>,
        now_instant: DateTime<Utc>,
    ) -> Result<RunResult> {
        let observed = self.observe_run_baseline(operation).await?;
        self.begin_run_from_observed_baseline_with_asset_prices(
            observed,
            plan,
            trusted_asset_prices,
            now_instant,
        )
        .await
    }

    /// Consume a baseline captured by this exact engine instance. This method
    /// exists for the trusted broker, which must inspect the snapshot to
    /// validate selected holdings and value them before asking the engine to
    /// prove a complete route. Untrusted plan data is still overwritten.
    pub async fn begin_run_from_observed_baseline_with_asset_prices(
        &self,
        observed: ObservedBaseline,
        mut plan: RunPlan,
        trusted_asset_prices: Vec<AssetPrice>,
        now_instant: DateTime<Utc>,
    ) -> Result<RunResult> {
        if observed.engine_id != self.instance_id {
            return Err(FinError::new(
                "OBSERVER_CAPTURE_INVALID",
                "budget",
                "The baseline capture was not created by this engine instance.",
                "Capture a fresh baseline through the broker connected to this engine.",
            ));
        }
        plan.baseline = observed.snapshot;
        let validation_now = current_validation_time(now_instant);
        validate_observation(&plan.baseline.observed_at, validation_now, "STALE_BASELINE")?;
        let resources_covered = snapshot_resources_covered(&plan.baseline);
        if !plan.baseline.complete
            || plan.baseline.coverage.is_empty()
            || !plan.baseline.pending.is_empty()
            || !plan.baseline.state.in_flight.is_empty()
            || !resources_covered
        {
            // Pending entries may contain private resource identities. Expose
            // only bounded fixed-format observer codes, never the raw entries.
            let observer_codes = plan
                .baseline
                .pending
                .iter()
                .filter_map(|entry| entry.strip_prefix("observer:"))
                .filter(|code| {
                    !code.is_empty()
                        && code.len() <= 64
                        && code
                            .bytes()
                            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                })
                .take(8)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(",");
            return Err(FinError::new(
                "INCOMPLETE_BASELINE",
                "budget",
                format!(
                    "Authoritative snapshot refused: complete={}, coverage_count={}, pending_count={}, in_flight_count={}, resources_covered={}, observer_codes=[{}].",
                    plan.baseline.complete,
                    plan.baseline.coverage.len(),
                    plan.baseline.pending.len(),
                    plan.baseline.state.in_flight.len(),
                    resources_covered,
                    observer_codes,
                ),
                "Reconcile pending activity and retry a complete broker-captured snapshot before route preparation.",
            ));
        }
        plan.asset_prices = trusted_asset_prices;
        plan.route_proofs = self.preflight_run_plan(&plan, validation_now).await?;
        self.begin_run(plan, current_validation_time(validation_now))
    }

    /// Replace only the unexecuted route suffix using a newly observed state
    /// and broker-supplied prices. The original baseline, allocations and cost
    /// ceiling remain immutable; this method never authorizes or submits a write.
    pub async fn replan_remaining_from_observer(
        &self,
        run_id: &str,
        observer_operation: &str,
        replacement_steps: Vec<PlanStep>,
        trusted_asset_prices: Vec<AssetPrice>,
        now_instant: DateTime<Utc>,
    ) -> Result<RunResult> {
        if replacement_steps.is_empty() {
            return Err(route_proof_error());
        }
        if trusted_asset_prices
            .iter()
            .any(fin_domain::unpriced::is_estimate)
        {
            return Err(FinError::new(
                "UNPRICED_ASSET",
                "budget",
                "A scoped acceptance return plan requires verified USD prices.",
                "Preserve the run and use a fully priced return plan.",
            ));
        }
        let mut run = self.load_run(run_id)?;
        if run.status == RunStatus::Restored || !run.reservations.is_empty() {
            return Err(FinError::new(
                "RUN_PENDING",
                "budget",
                "A route can be revised only when the run has no unresolved operation.",
                "Reconcile every original submission before revising its remaining return path.",
            ));
        }
        let prefix_len = run.settled_route_steps.len();
        if run
            .settled_route_steps
            .iter()
            .enumerate()
            .any(|(index, step)| *step as usize != index)
            || prefix_len > run.route_proofs.len()
        {
            return Err(route_proof_error());
        }
        let current = self.snapshot(observer_operation).await?;
        let snapshot_validation_now = current_validation_time(now_instant);
        validate_observation(
            &current.observed_at,
            snapshot_validation_now,
            "STALE_BASELINE",
        )?;
        if !current.complete
            || current.coverage.is_empty()
            || !current.pending.is_empty()
            || !current.state.in_flight.is_empty()
            || !snapshot_resources_covered(&current)
        {
            return Err(FinError::new(
                "INCOMPLETE_BASELINE",
                "budget",
                "A fresh complete observation with no unresolved operation is required to revise a return route.",
                "Reconcile pending work and capture complete broker-observed state before replanning cleanup.",
            ));
        }
        let mut state = projected_state_with_cash(&current, &run.available)?;
        let mut proofs = run.route_proofs[..prefix_len].to_vec();
        let old_steps = run
            .planned_operations
            .iter()
            .chain(run.unwind.iter())
            .cloned()
            .collect::<Vec<_>>();
        if old_steps.len() != run.route_proofs.len() || prefix_len > old_steps.len() {
            return Err(route_proof_error());
        }
        let superseded_steps = old_steps[prefix_len..].to_vec();
        let superseded_proofs = run.route_proofs[prefix_len..].to_vec();
        let used_cost = parse_uint(&run.spent_cost_usd_micros)?;
        let total_cost = parse_uint(&run.return_cost_reserve_usd_micros)?;
        let mut remaining_cost = if used_cost > total_cost {
            return Err(route_proof_error());
        } else {
            total_cost - used_cost
        };
        let allocations = run
            .allocations
            .iter()
            .map(|allocation| allocation.holding.clone())
            .collect::<Vec<_>>();
        let mut new_proofs = Vec::with_capacity(replacement_steps.len());
        for (offset, step) in replacement_steps.iter().enumerate() {
            let maximum_fee = parse_uint(&step.maximum_fee_usd_micros)?;
            if maximum_fee > remaining_cost {
                return Err(FinError::new(
                    "RETURN_ROUTE_COST_BUDGET",
                    "budget",
                    "The revised remaining route exceeds the original run's unspent cost reserve.",
                    "Keep the revised cleanup route within the original cost ceiling.",
                ));
            }
            let (adapter, kind) = self.resolve(&step.operation)?;
            if kind != OperationKind::Write {
                return Err(route_proof_error());
            }
            let index = prefix_len + offset;
            let context = ReturnRouteContext {
                baseline: run.baseline.clone(),
                projected: state.clone(),
                allocations: allocations.clone(),
                step_index: u32::try_from(index).map_err(|_| route_proof_error())?,
                prior_proofs: proofs.clone(),
                asset_prices: trusted_asset_prices.clone(),
                remaining_cost_budget_usd_micros: fin_domain::Quantity::new(
                    remaining_cost.to_string(),
                )?,
            };
            let mut proof = adapter
                .preflight_return_route(&step.operation, &step.input, &context)
                .await
                .map_err(|error| {
                    route_step_error(error, index, &step.operation, "adapter preflight")
                })?;
            let proof_validation_now = current_validation_time(now_instant);
            validate_route_proof(step, &proof, index, proof_validation_now).map_err(|error| {
                route_step_error(error, index, &step.operation, "proof validation")
            })?;
            validate_route_fee_bound(step, &proof, &trusted_asset_prices, proof_validation_now)
                .map_err(|error| route_step_error(error, index, &step.operation, "cost bound"))?;
            validate_projected_requirements(
                &state,
                &proof,
                &proofs,
                index,
                &run.available,
                prefix_len,
            )
            .map_err(|error| {
                route_step_error(error, index, &step.operation, "projected requirement")
            })?;
            include_costs_in_projected_cash(&mut proof).map_err(|error| {
                route_step_error(error, index, &step.operation, "cost projection")
            })?;
            validate_projected_cash_legs(&state, &proof).map_err(|error| {
                route_step_error(error, index, &step.operation, "cash flow attribution")
            })?;
            state = apply_projected_effects(&state, &proof.effects, index).map_err(|error| {
                route_step_error(error, index, &step.operation, "projected effects")
            })?;
            proofs.push(proof.clone());
            new_proofs.push(proof);
            remaining_cost -= maximum_fee;
        }
        let validation_now = current_validation_time(now_instant);
        validate_observation(&current.observed_at, validation_now, "STALE_BASELINE")?;
        validate_price_rows(
            &trusted_asset_prices,
            &trusted_asset_prices
                .iter()
                .map(|price| price.asset.clone())
                .collect(),
            validation_now,
        )?;
        for (offset, proof) in new_proofs.iter().enumerate() {
            let index = prefix_len + offset;
            validate_route_proof(&replacement_steps[offset], proof, index, validation_now)?;
            validate_route_fee_bound(
                &replacement_steps[offset],
                proof,
                &trusted_asset_prices,
                validation_now,
            )?;
        }
        projected_provenance::replay_seeded_suffix(
            &run.allocations,
            &run.provenance,
            &projected_state_with_cash(&current, &run.available)?,
            &new_proofs,
            &trusted_asset_prices,
            &parse_uint(&run.return_cost_reserve_usd_micros)?,
            prefix_len,
        )?;
        validate_projected_restoration(&run.baseline, &run.allocations, &state)?;
        run.route_revisions.push(RouteRevision {
            changed_at: now_instant.to_rfc3339(),
            replaced_from_step: u32::try_from(prefix_len).map_err(|_| route_proof_error())?,
            superseded_steps,
            superseded_proofs,
            replacement_steps: replacement_steps.clone(),
            replacement_proofs: new_proofs,
        });
        run.planned_operations = old_steps[..prefix_len].to_vec();
        run.unwind = replacement_steps;
        run.route_proofs = proofs;
        run.asset_prices = trusted_asset_prices;
        run.route_invalidated = false;
        self.save_run(&run)?;
        Ok(run_result(&run))
    }

    async fn preflight_run_plan(
        &self,
        plan: &RunPlan,
        now_instant: DateTime<Utc>,
    ) -> Result<Vec<ReturnRouteProof>> {
        let selected_cash = plan
            .allocations
            .iter()
            .map(|allocation| allocation.holding.clone())
            .collect::<Vec<_>>();
        let mut state = projected_state_with_cash(&plan.baseline, &selected_cash)?;
        let mut proofs = Vec::new();
        let steps = plan
            .planned_operations
            .iter()
            .chain(plan.unwind.iter())
            .collect::<Vec<_>>();
        let mut remaining_cost = parse_uint(&plan.return_cost_reserve_usd_micros)?;
        let allocations = plan
            .allocations
            .iter()
            .map(|allocation| allocation.holding.clone())
            .collect::<Vec<_>>();
        for (index, step) in steps.iter().enumerate() {
            let maximum_fee = parse_uint(&step.maximum_fee_usd_micros)?;
            if maximum_fee > remaining_cost {
                return Err(FinError::new(
                    "RETURN_ROUTE_COST_BUDGET",
                    "budget",
                    "The complete planned route exceeds its reserved execution-cost budget.",
                    "Revise the complete forward and return route within the original cost reserve.",
                ));
            }
            let (adapter, kind) = self.resolve(&step.operation)?;
            if kind != OperationKind::Write {
                return Err(FinError::new(
                    "RETURN_ROUTE_NOT_WRITABLE",
                    "budget",
                    "Every planned route step must be a supported financial write.",
                    "Remove unsupported steps and obtain a reviewed proof for every remaining route step.",
                ));
            }
            let context = ReturnRouteContext {
                baseline: plan.baseline.clone(),
                projected: state.clone(),
                allocations: allocations.clone(),
                step_index: u32::try_from(index)
                    .map_err(|_| FinError::validation("Too many planned route steps."))?,
                prior_proofs: proofs.clone(),
                asset_prices: plan.asset_prices.clone(),
                remaining_cost_budget_usd_micros: fin_domain::Quantity::new(
                    remaining_cost.to_string(),
                )?,
            };
            let mut proof = adapter
                .preflight_return_route(&step.operation, &step.input, &context)
                .await
                .map_err(|error| {
                    route_step_error(error, index, &step.operation, "adapter preflight")
                })?;
            let proof_validation_now = current_validation_time(now_instant);
            validate_route_proof(step, &proof, index, proof_validation_now).map_err(|error| {
                route_step_error(error, index, &step.operation, "proof validation")
            })?;
            validate_route_fee_bound(step, &proof, &plan.asset_prices, proof_validation_now)
                .map_err(|error| route_step_error(error, index, &step.operation, "cost bound"))?;
            validate_projected_requirements(&state, &proof, &proofs, index, &selected_cash, 0)
                .map_err(|error| {
                    route_step_error(error, index, &step.operation, "projected requirement")
                })?;
            include_costs_in_projected_cash(&mut proof).map_err(|error| {
                route_step_error(error, index, &step.operation, "cost projection")
            })?;
            validate_projected_cash_legs(&state, &proof).map_err(|error| {
                route_step_error(error, index, &step.operation, "cash flow attribution")
            })?;
            state = apply_projected_effects(&state, &proof.effects, index).map_err(|error| {
                route_step_error(error, index, &step.operation, "projected effects")
            })?;
            proofs.push(proof);
            remaining_cost -= maximum_fee;
        }
        Ok(proofs)
    }

    /// Return only broker-held, previously verified position anchors.
    pub fn preparation_context(&self, run_id: Option<&str>) -> Result<PreparationContext> {
        if let Some(run_id) = run_id {
            let run = self.load_run(run_id)?;
            return Ok(PreparationContext {
                positions: run.position_anchors,
            });
        }
        let mut latest = BTreeMap::<String, PositionAnchor>::new();
        for record in self.operations()? {
            if record.status != Status::Completed {
                continue;
            }
            let Some(verified) = &record.effects else {
                continue;
            };
            if record.submission.as_ref() != Some(&verified.submission)
                || verified.evidence_version == 0
                || verified.proof_digest != verified_effect_digest(verified)?
            {
                continue;
            }
            for effect in &verified.effects {
                let anchor = match effect {
                    FinancialEffect::PositionChange { anchor, .. }
                    | FinancialEffect::DebtChange { anchor, .. } => anchor.as_ref(),
                    _ => None,
                };
                if let Some(anchor) =
                    anchor.filter(|a| a.evidence_version > 0 && !a.evidence.is_null())
                {
                    let key = position_anchor_key(anchor)?;
                    let is_newer = latest.get(&key).is_none_or(|old| {
                        DateTime::parse_from_rfc3339(&anchor.observed_at).ok()
                            > DateTime::parse_from_rfc3339(&old.observed_at).ok()
                    });
                    if is_newer {
                        latest.insert(key, anchor.clone());
                    }
                }
            }
        }
        Ok(PreparationContext {
            positions: latest.into_values().collect(),
        })
    }

    pub async fn execute(&self, request: OperationRequest) -> Result<OperationRecord> {
        let _guard = self.write_queue.lock().await;
        self.execute_serial(request).await
    }

    async fn execute_serial(&self, request: OperationRequest) -> Result<OperationRecord> {
        if request.request_id.trim().is_empty() {
            return Err(FinError::new(
                "REQUEST_ID_REQUIRED",
                "validation",
                "A stable request identity is required.",
                "Use one identity for this logical financial action.",
            ));
        }
        let (adapter, kind) = self.resolve(&request.operation)?;
        if kind != OperationKind::Write {
            return Err(FinError::new(
                "READ_OPERATION",
                "validation",
                "This operation is not a financial write.",
                "Use the read command.",
            ));
        }
        let fingerprint = request_fingerprint(&request)?;
        let timestamp = now();
        let id = Uuid::new_v4().to_string();
        let initial = OperationRecord {
            id: id.clone(),
            request_id: request.request_id.clone(),
            fingerprint: fingerprint.clone(),
            fingerprint_version: 1,
            operation: request.operation.clone(),
            input: request.input.clone(),
            status: Status::AwaitingApproval,
            created_at: timestamp.clone(),
            updated_at: timestamp.clone(),
            preview: None,
            approval: None,
            submission: None,
            dispatch_evidence: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: request.run_id.clone(),
        };
        let initial_json = to_value(&initial)?;
        let created = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .create_request(
                &id,
                &request.request_id,
                &fingerprint,
                &request.operation,
                request.run_id.as_deref(),
                "awaiting_approval",
                &initial_json,
                &timestamp,
            )
            .map_err(map_store)?;
        if !created.created {
            let existing: OperationRecord = from_value(created.record)?;
            return if matches!(
                existing.status,
                Status::Submitted | Status::Pending | Status::Unknown
            ) {
                self.reconcile_serial(existing.id.clone()).await
            } else {
                Ok(existing)
            };
        }

        let mut record = initial;
        #[cfg(not(test))]
        if request.run_id.is_some() {
            record.status = Status::Rejected;
            record.error = Some(FinError::new(
                "RUN_RETIRED",
                "validation",
                "Historical runs no longer authorize new operations.",
                "Execute without --run; inspect or reconcile existing operations by their original identity.",
            ));
            self.update_record(&record)?;
            return Ok(record);
        }
        let result = self
            .execute_new(adapter, &request, &mut record, None, None, None)
            .await;
        match result {
            Ok(record) => Ok(record),
            Err(error) => {
                if !record.status.terminal() {
                    record.status = if record.submission.is_some() {
                        Status::Unknown
                    } else {
                        status_for_error(&error)
                    };
                    record.error = Some(error);
                    record.updated_at = now();
                    if record.status.terminal() {
                        let run = self.release_unsubmitted_reservation(&record)?;
                        self.commit_terminal(record.clone(), json!(null), json!([]), run.as_ref())?;
                    } else {
                        self.update_record(&record)?;
                    }
                }
                Ok(record)
            }
        }
    }

    fn validate_submission_cancellation(
        &self,
        prepared: &PreparedOperation,
        run_id: Option<&str>,
    ) -> Result<bool> {
        let Some((operation_id, submission)) = cancellation_target(&prepared.intent) else {
            if prepared
                .intent
                .expected
                .iter()
                .any(|effect| matches!(effect, EffectConstraint::SubmissionCancellation { .. }))
            {
                return Err(cancellation_error());
            }
            return Ok(false);
        };
        if !prepared.intent.debits.is_empty() || !prepared.intent.maximum_costs.is_empty() {
            return Err(cancellation_error());
        }
        let original = self.load_record(operation_id)?;
        if !matches!(
            original.status,
            Status::Submitted | Status::Pending | Status::Unknown
        ) || original.submission.as_ref() != Some(submission)
            || original.run_id.as_deref() != run_id
            || original
                .preview
                .as_ref()
                .is_none_or(|preview| preview.intent.locks != prepared.intent.locks)
            || original
                .preview
                .as_ref()
                .is_some_and(|preview| cancellation_target(&preview.intent).is_some())
        {
            return Err(cancellation_error());
        }
        if let Some(run_id) = run_id {
            let run = self.load_run(run_id)?;
            if run.status == RunStatus::Restored || !run.reservations.contains_key(operation_id) {
                return Err(cancellation_error());
            }
        }
        Ok(true)
    }

    async fn execute_new(
        &self,
        adapter: Arc<dyn Adapter>,
        request: &OperationRequest,
        record: &mut OperationRecord,
        boundary: Option<BoundaryAuthorizationRecord>,
        plan: Option<FinancialPlanAuthorizationRecord>,
        preprepared: Option<PreparedOperation>,
    ) -> Result<OperationRecord> {
        let context = self.preparation_context(request.run_id.as_deref())?;
        let mut prepared = match preprepared {
            Some(prepared) => prepared,
            None => {
                adapter
                    .prepare_with_context(&request.operation, &request.input, &context)
                    .await?
            }
        };
        let plan_attempt_hash = if let Some(plan) = &plan {
            let linkage = plan
                .attempts
                .iter()
                .find(|attempt| attempt.attempt_id == record.id)
                .ok_or_else(|| {
                    plan_error(
                        "PLAN_ATTEMPT_MISSING",
                        "The durable plan attempt linkage is missing.",
                        "Reconcile the original attempt before retrying.",
                    )
                })?;
            let prepared_hash = binding_hash(
                &fin_domain::financial_plans::financial_plan_attempt_binding(
                    &prepared,
                    &plan.revision.plan_id,
                    &plan.revision.revision_id,
                    &linkage.step_id,
                ),
            )?;
            if prepared_hash != linkage.execution_binding_hash {
                return Err(plan_error(
                    "PLAN_CANDIDATE_CHANGED",
                    "The prepared operation differs from the immutable plan attempt candidate.",
                    "Preview the current candidate and submit a new attempt only after reconciliation.",
                ));
            }
            self.validate_plan_candidate(
                adapter.as_ref(),
                &plan.revision,
                &linkage.step_id,
                &record.input,
                &record.id,
                &prepared,
            )?;
            {
                let store = self
                    .store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?;
                validate_candidate_plan_caps(plan, &linkage.step_id, &prepared, &store)?;
            }
            financial_plans::inject_plan_marker(
                &mut prepared,
                plan,
                &linkage.step_id,
                prepared_hash.clone(),
            )?;
            Some(prepared_hash)
        } else {
            None
        };
        let boundary_attempt_hash = if let Some(boundary) = &boundary {
            adapter.validate_boundary_candidate(&boundary.authorization.spec, &prepared)?;
            validate_candidate_caps(&boundary.authorization, &prepared)?;
            prepared.summary = boundary.authorization.spec.summary.clone();
            let attempt_hash = binding_hash(&boundaries::attempt_binding(&prepared))?;
            inject_boundary_marker(&mut prepared, &boundary.authorization, attempt_hash.clone())?;
            Some(attempt_hash)
        } else {
            None
        };
        let required_cost_origins =
            self.cost_origin_assets(&prepared, request.run_id.as_deref())?;
        self.complete_cost_origin_prices(&mut prepared, &required_cost_origins)
            .await?;
        validate_prepared(&prepared, &request.operation, Utc::now())?;
        record.preview = Some(prepared.clone());
        record.updated_at = now();

        let cancellation =
            self.validate_submission_cancellation(&prepared, request.run_id.as_deref())?;
        let mut run_state = if cancellation {
            None
        } else if let Some(run_id) = &request.run_id {
            Some(self.reserve_run_operation(run_id, record, &prepared)?)
        } else {
            None
        };
        if let Some(run) = &run_state {
            let reservations =
                store_reservations(record.id.as_str(), &run.reservations[&record.id])?;
            self.store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .persist_reservation(
                    &record.id,
                    &reservations,
                    &to_value(record)?,
                    &run.id,
                    &to_value(run)?,
                    &now(),
                )
                .map_err(map_store)?;
        } else if cancellation {
            // Cancellation shares its original unresolved resource; it cannot
            // acquire a conflicting financial reservation for a new movement.
            self.update_record(record)?;
        } else {
            self.reserve_ordinary_operation(record, &prepared)?;
        }

        let approved_binding = authorization_binding(&prepared);
        let approved_binding_hash = binding_hash(&approved_binding)?;
        let (audit, challenge) = if let Some(boundary) = &boundary {
            if boundary.authorization.status != BoundaryAuthorizationStatus::Active
                || parse_time(&boundary.authorization.expires_at)? <= Utc::now()
            {
                return Err(boundary_error(
                    "Boundary authorization expired or was revoked before dispatch.",
                ));
            }
            ensure_audit_binding(&boundary.approval, &approved_binding_hash)?;
            (boundary.approval.clone(), None)
        } else if let Some(plan) = &plan {
            ensure_audit_binding(&plan.approval, &plan.revision.approval_binding_hash)?;
            validate_future_expiry(
                &plan.revision.expires_at,
                Utc::now(),
                "PLAN_AUTHORIZATION_EXPIRED",
            )?;
            (plan.approval.clone(), None)
        } else {
            let challenge_expiry = std::cmp::min(
                parse_time(&prepared.expires_at)?,
                parse_time(&now())? + chrono::Duration::seconds(90),
            );
            let challenge = AuthorizationChallenge {
                purpose: "financial_write".into(),
                operation_id: record.id.clone(),
                binding: approved_binding,
                binding_hash: approved_binding_hash.clone(),
                summary: prepared.summary.clone(),
                nonce: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
                expires_at: challenge_expiry.to_rfc3339(),
            };
            let audit = self.authorizer.authorize(&challenge).await?;
            validate_approval(&audit, &challenge, Utc::now())?;
            (audit, Some(challenge))
        };
        record.approval = Some(audit.clone());
        record.updated_at = now();
        self.update_record(record)?;

        let adapter_candidate = strip_plan_metadata(&prepared);
        let mut refreshed = adapter.revalidate(&adapter_candidate).await?;
        self.complete_cost_origin_prices(&mut refreshed, &required_cost_origins)
            .await?;
        validate_prepared(&refreshed, &request.operation, Utc::now())?;
        if let Some(boundary) = &boundary {
            let marker = refreshed
                .binding
                .as_object_mut()
                .and_then(|object| object.remove(boundaries::BOUNDARY_METADATA_KEY));
            let refreshed_candidate_hash = binding_hash(&boundaries::attempt_binding(&refreshed))?;
            if Some(refreshed_candidate_hash.as_str()) != boundary_attempt_hash.as_deref() {
                return Err(FinError::new(
                    "REQUEST_CHANGED",
                    "approval",
                    "Execution parameters changed after boundary authorization.",
                    "Refresh the candidate and verify it remains within the approved limits.",
                ));
            }
            adapter.validate_boundary_candidate(&boundary.authorization.spec, &refreshed)?;
            validate_candidate_caps(&boundary.authorization, &refreshed)?;
            inject_boundary_marker(
                &mut refreshed,
                &boundary.authorization,
                boundary_attempt_hash.clone().unwrap_or_default(),
            )?;
            let _ = marker;
        }
        if boundary.is_none()
            && plan.is_none()
            && binding_hash(&authorization_binding(&refreshed))? != approved_binding_hash
        {
            return Err(FinError::new(
                "REQUEST_CHANGED",
                "approval",
                "Execution parameters changed after approval.",
                "Refresh the preview and authorize the changed operation.",
            ));
        }
        if let Some(plan) = &plan {
            let linkage = plan
                .attempts
                .iter()
                .find(|attempt| attempt.attempt_id == record.id)
                .ok_or_else(|| {
                    plan_error(
                        "PLAN_ATTEMPT_MISSING",
                        "The durable plan attempt linkage is missing.",
                        "Reconcile the original attempt.",
                    )
                })?;
            let refreshed_hash = binding_hash(
                &fin_domain::financial_plans::financial_plan_attempt_binding(
                    &refreshed,
                    &plan.revision.plan_id,
                    &plan.revision.revision_id,
                    &linkage.step_id,
                ),
            )?;
            if Some(refreshed_hash.as_str()) != plan_attempt_hash.as_deref() {
                return Err(plan_error(
                    "PLAN_CANDIDATE_CHANGED",
                    "The venue candidate changed after it was bound to this plan attempt.",
                    "Reconcile this attempt before proposing another candidate.",
                ));
            }
            self.validate_plan_candidate(
                adapter.as_ref(),
                &plan.revision,
                &linkage.step_id,
                &record.input,
                &record.id,
                &refreshed,
            )?;
            {
                let store = self
                    .store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?;
                validate_candidate_plan_caps(plan, &linkage.step_id, &refreshed, &store)?;
            }
            financial_plans::inject_plan_marker(
                &mut refreshed,
                plan,
                &linkage.step_id,
                plan_attempt_hash.clone().unwrap_or_default(),
            )?;
        }
        if let Some(challenge) = &challenge {
            ensure_approval_fresh(&audit, challenge, Utc::now())?;
        }
        if let Some(boundary) = &boundary {
            let current = self.load_boundary_record(&boundary.authorization.id)?;
            if current.authorization.status != BoundaryAuthorizationStatus::Active
                || parse_time(&current.authorization.expires_at)? <= Utc::now()
                || current.active_attempt.as_deref() != Some(record.id.as_str())
            {
                return Err(boundary_error(
                    "Boundary authorization was revoked, expired, or replaced before dispatch.",
                ));
            }
        }
        if let Some(plan) = &plan {
            let current = self.load_financial_plan(&plan.revision.plan_id)?;
            if current.revision.revision_id != plan.revision.revision_id
                || current.revision.status != FinancialPlanAuthorizationStatus::Active
                || parse_time(&current.revision.expires_at)? <= Utc::now()
                || current.active_attempt_id.as_deref() != Some(record.id.as_str())
                || plan_attempt_hash.as_deref()
                    != plan
                        .attempts
                        .iter()
                        .find(|attempt| attempt.attempt_id == record.id)
                        .map(|attempt| attempt.execution_binding_hash.as_str())
            {
                return Err(plan_error(
                    "PLAN_AUTHORIZATION_INACTIVE",
                    "Plan authority changed or expired before dispatch.",
                    "Reconcile the attempt and obtain fresh authorization if another attempt is needed.",
                ));
            }
        }
        if cancellation {
            self.validate_submission_cancellation(&refreshed, request.run_id.as_deref())?;
        }

        if let (Some(run_id), Some(run)) = (&request.run_id, run_state.as_mut()) {
            self.recheck_run_reservation(run_id, &record.id, &refreshed)?;
            run.reservations.get_mut(&record.id).unwrap().binding_hash =
                approved_binding_hash.clone();
            record.preview = Some(refreshed.clone());
            self.store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .persist_reservation(
                    &record.id,
                    &store_reservations(&record.id, &run.reservations[&record.id])?,
                    &to_value(record)?,
                    run_id,
                    &to_value(run)?,
                    &now(),
                )
                .map_err(map_store)?;
        }
        if request.run_id.is_none() && !cancellation {
            self.reserve_ordinary_operation(record, &refreshed)?;
        }
        record.preview = Some(refreshed.clone());
        let journal = Arc::new(OperationSubmissionJournal {
            operation_id: record.id.clone(),
            expected_binding_hash: plan
                .as_ref()
                .map(|p| p.revision.approval_binding_hash.clone())
                .unwrap_or(approved_binding_hash),
            approval: audit,
            boundary_authorization_id: boundary.as_ref().map(|b| b.authorization.id.clone()),
            boundary_authorization_snapshot: boundary.clone(),
            financial_plan_id: plan.as_ref().map(|p| p.revision.plan_id.clone()),
            financial_plan_snapshot: plan.clone(),
            adapter: Some(adapter.clone()),
            options: self.options.clone(),
            consumed: AtomicBool::new(false),
            store: self.store.clone(),
            record: Mutex::new(record.clone()),
        });
        let adapter_submission_candidate = strip_plan_metadata(&refreshed);
        let submission = adapter
            .submit(&adapter_submission_candidate, journal.clone())
            .await;
        if let Err(error) = submission {
            let durable = journal
                .record
                .lock()
                .map_err(|_| internal("journal record lock poisoned"))?
                .clone();
            *record = durable;
            return Err(error);
        }
        let submission = submission.expect("checked result");
        let persisted = journal
            .record
            .lock()
            .map_err(|_| internal("journal record lock poisoned"))?
            .clone();
        if persisted.submission.as_ref() != Some(&submission) {
            *record = persisted;
            return Err(FinError::new(
                "SUBMISSION_IDENTITY_MISMATCH",
                "submission",
                "The adapter result differs from the identity persisted before dispatch.",
                "Reconcile the original submission identity; do not submit again.",
            ));
        }
        *record = persisted;
        record.status = Status::Pending;
        record.updated_at = now();
        self.update_record(record)?;
        self.reconcile_serial(record.id.clone()).await
    }

    pub async fn status(&self, id: &str, reconcile: bool) -> Result<OperationRecord> {
        let record = self.load_record(id)?;
        if reconcile
            && matches!(
                record.status,
                Status::Submitted | Status::Pending | Status::Unknown
            )
        {
            let _guard = self.write_queue.lock().await;
            self.reconcile_serial(id.to_owned()).await
        } else {
            Ok(record)
        }
    }

    async fn reconcile_serial(&self, id: String) -> Result<OperationRecord> {
        let record = self.load_record(&id)?;
        if !matches!(
            record.status,
            Status::Submitted | Status::Pending | Status::Unknown
        ) {
            return Ok(record);
        }
        let adapter = self
            .adapters
            .read()
            .map_err(|_| internal("registry lock poisoned"))?
            .get(&record.operation)
            .cloned()
            .ok_or_else(|| {
                FinError::new(
                    "ADAPTER_UNAVAILABLE",
                    "reconciliation",
                    "The original operation adapter is unavailable.",
                    "Restore the same adapter version and reconcile; do not resubmit.",
                )
            })?;
        let mut adapter_record = record.clone();
        if let Some(prepared) = &record.preview {
            adapter_record.preview = Some(strip_plan_metadata(prepared));
        }
        let plan_owner = {
            self.store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .financial_plan_attempt_owner(&record.id)
                .map_err(map_store)?
        };
        if let Some((plan_id, revision_id, step_id, _)) = plan_owner {
            let revision_value = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .financial_plan_revision(&plan_id, &revision_id)
                .map_err(map_store)?
                .ok_or_else(|| {
                    plan_error(
                        "PLAN_REVISION_MISSING",
                        "The original plan revision for this attempt is missing.",
                        "Keep the submission unresolved and restore its immutable revision history.",
                    )
                })?;
            let revision: FinancialPlanRevision = from_value(revision_value)?;
            let step = revision
                .spec
                .steps
                .iter()
                .find(|step| step.step_id == step_id)
                .ok_or_else(|| internal("original plan step disappeared"))?;
            if step.operation != record.operation {
                return Err(plan_error(
                    "PLAN_ATTEMPT_BINDING_MISMATCH",
                    "The original plan step no longer matches its operation record.",
                    "Keep the submission unresolved and inspect its immutable plan history.",
                ));
            }
            let permission = financial_plans::permission_for_step(&revision, &step_id)?;
            if !permission.generated_input_fields.is_empty() {
                let execution_input = financial_plans::plan_attempt_input(
                    &revision,
                    &step_id,
                    &record.input,
                    &record.id,
                )?;
                let prepared = record.preview.as_ref().ok_or_else(|| {
                    plan_error(
                        "PLAN_CANDIDATE_MISSING",
                        "The original plan attempt has no durable prepared candidate.",
                        "Keep the submission unresolved and restore its original candidate record.",
                    )
                })?;
                financial_plans::validate_prepared_generated_input(
                    &permission.generated_input_fields,
                    &execution_input,
                    &prepared.input,
                )?;
                adapter_record.input = execution_input;
            }
        }
        let reconciliation = match adapter.reconcile(&adapter_record).await {
            Ok(reconciliation) => reconciliation,
            Err(error) => {
                let mut retained = record;
                if retained.error.is_none() || retained.submission.is_none() {
                    retained.error = Some(error);
                }
                retained.updated_at = now();
                self.update_record(&retained)?;
                return Ok(retained);
            }
        };
        match self.apply_reconciliation(record.clone(), reconciliation.clone()) {
            Ok(record) => Ok(record),
            Err(error) if error.stage == "reconciliation" || error.stage == "budget" => {
                let mut retained = record;
                retained.status = if retained.submission.is_some() {
                    Status::Unknown
                } else {
                    retained.status
                };
                retained.last_reconciliation_evidence = Some(reconciliation.evidence);
                if retained.error.is_none() || retained.submission.is_none() {
                    retained.error = Some(error);
                }
                retained.updated_at = now();
                self.update_record(&retained)?;
                Ok(retained)
            }
            Err(error) => Err(error),
        }
    }

    fn apply_reconciliation(
        &self,
        mut record: OperationRecord,
        reconciliation: Reconciliation,
    ) -> Result<OperationRecord> {
        match reconciliation.status.clone() {
            Status::Pending | Status::Submitted | Status::Unknown => {
                record.status = reconciliation.status;
                record.last_reconciliation_evidence = Some(reconciliation.evidence);
                if record.submission.is_none() {
                    record.error = None;
                }
                record.updated_at = now();
                self.update_record(&record)?;
                Ok(record)
            }
            Status::Completed | Status::Failed => {
                let verified = reconciliation.verified.ok_or_else(|| FinError::new(
                    "RECONCILIATION_PROOF_PENDING",
                    "reconciliation",
                    "Terminal venue status has no verified typed effects.",
                    "Reconcile this original submission until its proof and effects are available.",
                ))?;
                validate_effects(&record, &verified, &reconciliation.status)?;
                if reconciliation.status == Status::Completed {
                    let prior = self.preparation_context(record.run_id.as_deref())?;
                    validate_anchor_transitions(&verified, &prior)?;
                }
                record.status = reconciliation.status;
                record.last_reconciliation_evidence = Some(reconciliation.evidence);
                record.effects = Some(verified.clone());
                record.error = None;
                record.updated_at = now();
                let cancellation = record
                    .preview
                    .as_ref()
                    .is_some_and(|preview| cancellation_target(&preview.intent).is_some());
                let run = if cancellation {
                    None
                } else if let Some(run_id) = &record.run_id {
                    let mut run: RunLedger = self.load_run(run_id)?;
                    self.settle_run(&mut run, &record, &verified)?;
                    Some(run)
                } else {
                    None
                };
                self.commit_terminal(
                    record.clone(),
                    to_value(&verified)?,
                    to_value(&verified.effects)?,
                    run.as_ref(),
                )?;
                Ok(record)
            }
            Status::Cancelled | Status::Rejected | Status::Expired => {
                record.status = reconciliation.status;
                record.last_reconciliation_evidence = Some(reconciliation.evidence.clone());
                record.updated_at = now();
                if record.submission.is_some() {
                    return Err(FinError::new(
                        "INVALID_RECONCILIATION_STATUS",
                        "reconciliation",
                        "A submitted external action cannot become cancelled or rejected through status lookup.",
                        "Reconcile the original submission until its outcome is proven.",
                    ));
                }
                let run = self.release_unsubmitted_reservation(&record)?;
                self.commit_terminal(
                    record.clone(),
                    reconciliation.evidence,
                    json!([]),
                    run.as_ref(),
                )?;
                Ok(record)
            }
            Status::AwaitingApproval => Err(FinError::new(
                "INVALID_RECONCILIATION_STATUS",
                "reconciliation",
                "Adapter cannot move a submitted operation back to approval.",
                "Inspect the original operation and preserve its submission identity.",
            )),
        }
    }

    /// Read the original historical document without rewriting its reservations or proofs.
    pub fn historical_run(&self, id: &str) -> Result<Value> {
        self.store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .get_run(id)
            .map_err(map_store)?
            .ok_or_else(|| FinError::validation("Historical run not found."))
    }

    pub fn operations(&self) -> Result<Vec<OperationRecord>> {
        self.store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .list_records()
            .map_err(map_store)?
            .into_iter()
            .map(from_value)
            .collect()
    }

    pub fn recover_interrupted(&self) -> Result<RecoveryReport> {
        self.store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .recover_interrupted()
            .map_err(map_store)
    }

    pub(crate) fn begin_run(&self, plan: RunPlan, now_instant: DateTime<Utc>) -> Result<RunResult> {
        if plan.id.trim().is_empty()
            || plan.allocations.is_empty()
            || plan.planned_operations.is_empty()
            || plan.unwind.is_empty()
            || plan.route_proofs.len() != plan.planned_operations.len() + plan.unwind.len()
        {
            return Err(FinError::new(
                "UNWIND_NOT_READY",
                "budget",
                "A baseline, selected holdings and complete verified return path are required.",
                "Capture the full initial state and plan every return step before writing.",
            ));
        }
        validate_route_plan(&plan, now_instant)?;
        validate_observation(&plan.baseline.observed_at, now_instant, "STALE_BASELINE")?;
        if !plan.baseline.complete
            || plan.baseline.coverage.is_empty()
            || !plan.baseline.pending.is_empty()
            || !plan.baseline.state.in_flight.is_empty()
            || !snapshot_resources_covered(&plan.baseline)
        {
            return Err(FinError::new(
                "INCOMPLETE_BASELINE",
                "budget",
                "A complete initial snapshot with no unresolved financial operations is required.",
                "Capture every affected resource and reconcile existing work before starting a run.",
            ));
        }
        let mut seen = BTreeSet::new();
        let mut allocation_total = BigUint::zero();
        let mut available = Vec::new();
        for allocation in &plan.allocations {
            validate_observation(&allocation.observed_at, now_instant, "STALE_VALUATION")?;
            if allocation.source.trim().is_empty()
                || !seen.insert(resource_asset_key(&allocation.holding)?)
            {
                return Err(FinError::new(
                    "DUPLICATE_ALLOCATION",
                    "budget",
                    "Run holdings must have unique physical resource and asset identities with a current valuation source.",
                    "Select each source holding once.",
                ));
            }
            if !plan
                .baseline
                .coverage
                .contains(&allocation.holding.resource.0)
            {
                return Err(FinError::new(
                    "INCOMPLETE_BASELINE",
                    "budget",
                    "The baseline observer did not attest coverage of a selected source resource.",
                    "Capture and verify every selected physical resource.",
                ));
            }
            if allocation.holding.amount.atoms.is_zero() {
                return Err(FinError::validation(
                    "A run allocation must contain a positive exact quantity.",
                ));
            }
            let baseline = amount_for(
                &plan.baseline.state.holdings,
                &allocation.holding.resource,
                &allocation.holding.amount.asset,
            )?;
            if baseline.is_none_or(|amount| amount < &allocation.holding.amount.atoms) {
                return Err(FinError::new(
                    "ALLOCATION_NOT_IN_BASELINE",
                    "budget",
                    "Selected allocation exceeds its observed baseline holding.",
                    "Select only exact units present in the captured baseline.",
                ));
            }
            allocation_total += parse_uint(&allocation.value_usd_micros)?;
            available.push(allocation.holding.clone());
        }
        let borrow_reserve = parse_uint(&plan.borrowed_proceeds_reserve_usd_micros)?;
        let cost_reserve = parse_uint(&plan.return_cost_reserve_usd_micros)?;
        if cost_reserve > allocation_total {
            return Err(FinError::new(
                "RETURN_COSTS_UNFUNDED",
                "budget",
                "The complete return-cost earmark exceeds the selected physical holdings.",
                "Fund return costs from the same selected allocation that is capped for this run.",
            ));
        }
        if fin_domain::policy::limit(&self.options.max_run_allocation_usd_micros)?
            .is_some_and(|limit| &allocation_total + &borrow_reserve > BigUint::from(limit))
        {
            return Err(FinError::new(
                "ALLOCATION_LIMIT",
                "budget",
                "Selected physical holdings and separately authorized borrowed proceeds exceed the effective run allocation limit.",
                "Reduce the selected holdings or borrowed-proceeds reserve.",
            ));
        }
        let provenance = plan
            .allocations
            .iter()
            .map(|allocation| ProvenanceLot {
                origin: format!(
                    "allocation:{}",
                    resource_asset_key(&allocation.holding).unwrap_or_default()
                ),
                location: AccountingLocation::Cash {
                    resource: allocation.holding.resource.clone(),
                },
                asset: allocation.holding.amount.asset.clone(),
                atoms: allocation.holding.amount.atoms.to_string(),
            })
            .collect();
        let position_anchors = plan
            .baseline
            .state
            .positions
            .iter()
            .filter_map(|position| position.anchor.clone())
            .collect();
        let ledger = RunLedger {
            purpose: None,
            max_movement_usd_micros: fin_domain::policy::limit(
                &self.options.max_movement_and_cost_usd_micros,
            )?
            .unwrap_or(0)
            .to_string(),
            id: plan.id.clone(),
            status: RunStatus::Active,
            baseline: plan.baseline,
            initial_allocation_usd_micros: allocation_total.to_string(),
            borrowed_proceeds_reserve_usd_micros: borrow_reserve.to_string(),
            return_cost_reserve_usd_micros: cost_reserve.to_string(),
            planned_operations: plan.planned_operations,
            unwind: plan.unwind,
            route_proofs: plan.route_proofs,
            asset_prices: plan.asset_prices,
            settled_route_steps: vec![],
            route_invalidated: false,
            legacy_unwind_steps: vec![],
            legacy_return_routes_verified: false,
            allocations: plan.allocations,
            available,
            reservations: BTreeMap::new(),
            costs: vec![],
            spent_cost_usd_micros: "0".into(),
            borrowed_allocation_used_usd_micros: "0".into(),
            final_state: None,
            final_observed_at: None,
            differences: BTreeMap::new(),
            provenance,
            provenance_events: vec![],
            supply_accruals: vec![],
            position_anchors,
            route_revisions: vec![],
            net_conversion_deltas: vec![],
        };
        self.store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .create_run(&ledger.id, &to_value(&ledger)?, &now())
            .map_err(map_store)?;
        Ok(run_result(&ledger))
    }

    pub fn run(&self, id: &str) -> Result<RunResult> {
        let run: RunLedger = self.load_run(id)?;
        Ok(run_result(&run))
    }

    /// Return the unique exact assets present in this run's original selected
    /// allocations. This intentionally reads the immutable allocation record,
    /// not current available cash, which may be zero or moved during a route.
    /// It exposes no run identity or account mapping and fails for unknown runs.
    pub fn run_allocation_assets(&self, id: &str) -> Result<Vec<fin_domain::AssetId>> {
        let run = self.load_run(id)?;
        let assets = run
            .allocations
            .into_iter()
            .map(|allocation| allocation.holding.amount.asset)
            .collect::<BTreeSet<_>>();
        Ok(assets.into_iter().collect())
    }

    pub fn finish_run(
        &self,
        id: &str,
        final_snapshot: StateSnapshot,
        now_instant: DateTime<Utc>,
    ) -> Result<RunResult> {
        validate_observation(
            &final_snapshot.observed_at,
            now_instant,
            "STALE_FINAL_STATE",
        )?;
        let mut run: RunLedger = self.load_run(id)?;
        let mut differences = BTreeMap::new();
        if !final_snapshot.complete || final_snapshot.coverage.is_empty() {
            differences.insert(
                "snapshot".into(),
                "final state observation is incomplete".into(),
            );
        }
        if !snapshot_resources_covered(&final_snapshot) {
            differences.insert(
                "snapshot_coverage".into(),
                "some reported resources lack observer coverage".into(),
            );
        }
        if !final_snapshot.pending.is_empty() {
            differences.insert(
                "pending".into(),
                format!(
                    "unresolved external state: {}",
                    final_snapshot.pending.join(",")
                ),
            );
        }
        for resource in run_physical_resources(&run) {
            if !final_snapshot.coverage.contains(&resource.0) {
                differences.insert(
                    format!("coverage:{}", resource.0),
                    "physical resource is missing from the final observation".into(),
                );
            }
        }
        if !run.reservations.is_empty() {
            differences.insert("reservations".into(), "unresolved operations remain".into());
        }
        match run_cost_usd_micros(&run.costs, &run.asset_prices) {
            Ok(actual_cost) => {
                if actual_cost > parse_uint(&run.return_cost_reserve_usd_micros)? {
                    differences.insert(
                        "cost:reserve".into(),
                        format!(
                            "verified costs {actual_cost} exceed reserved {} USD micros",
                            run.return_cost_reserve_usd_micros
                        ),
                    );
                }
            }
            Err(_) => {
                differences.insert(
                    "cost:valuation".into(),
                    "verified costs cannot be valued using the run's trusted prices".into(),
                );
            }
        }
        if !final_snapshot.state.in_flight.is_empty() {
            differences.insert(
                "in_flight".into(),
                "financial operations remain pending".into(),
            );
        }
        for operation in self.operations()? {
            if operation.run_id.as_deref() == Some(id)
                && !matches!(
                    operation.status,
                    Status::Completed
                        | Status::Failed
                        | Status::Rejected
                        | Status::Cancelled
                        | Status::Expired
                )
            {
                differences.insert(
                    format!("operation:{}", operation.id),
                    format!("unresolved status {:?}", operation.status),
                );
            }
        }
        compare_position_claims(
            &run.baseline.state.positions,
            &final_snapshot.state.positions,
            &mut differences,
        );
        compare_exact(
            &run.baseline.state.authorities,
            &final_snapshot.state.authorities,
            "authority",
            &mut differences,
        );
        let (expected_cash, conversion_deltas, provenance_issues) = expected_restored_cash(&run)?;
        run.net_conversion_deltas = conversion_deltas;
        for (index, issue) in provenance_issues.into_iter().enumerate() {
            differences.insert(format!("provenance:{index}"), issue);
        }
        let final_map = aggregate_amounts(&final_snapshot.state.holdings)?;
        let provenance_cash = aggregate_provenance_cash(&run.provenance)?;
        let available_cash = aggregate_amounts(&run.available)?;
        for key in provenance_cash
            .keys()
            .chain(available_cash.keys())
            .cloned()
            .collect::<BTreeSet<_>>()
        {
            let provenance_amount = provenance_cash.get(&key).cloned().unwrap_or_default();
            let available_amount = available_cash.get(&key).cloned().unwrap_or_default();
            if provenance_amount != available_amount {
                differences.insert(
                    format!("provenance:available:{key}"),
                    format!(
                        "receipt-derived cash lots {provenance_amount} disagree with available ledger {available_amount}"
                    ),
                );
            }
        }
        let all_keys = expected_cash
            .keys()
            .chain(final_map.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        for key in all_keys {
            let final_amount = final_map.get(&key).cloned().unwrap_or_else(BigUint::zero);
            let expected = expected_cash.get(&key).cloned().unwrap_or_default();
            if final_amount != expected {
                differences.insert(
                    key,
                    format!("expected {expected} from exact origin lots, observed {final_amount}"),
                );
            }
        }
        run.final_state = Some(final_snapshot.clone());
        run.final_observed_at = Some(final_snapshot.observed_at.clone());
        run.differences = differences;
        run.status = if run.differences.is_empty() {
            RunStatus::Restored
        } else {
            RunStatus::CleanupOutstanding
        };
        self.save_run(&run)?;
        Ok(run_result(&run))
    }

    fn resolve(&self, operation: &str) -> Result<(Arc<dyn Adapter>, OperationKind)> {
        let registry = self
            .adapters
            .read()
            .map_err(|_| internal("registry lock poisoned"))?;
        let adapter = registry.get(operation).cloned().ok_or_else(|| {
            FinError::new(
                "UNSUPPORTED_OPERATION",
                "discovery",
                "This operation is not implemented.",
                "Inspect fin capabilities and fin explain.",
            )
        })?;
        let definition = adapter
            .definitions()
            .into_iter()
            .find(|d| d.name == operation)
            .ok_or_else(|| internal("registry definition disappeared"))?;
        Ok((adapter, definition.kind))
    }

    fn load_record(&self, id: &str) -> Result<OperationRecord> {
        let value = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .get_record(id)
            .map_err(map_store)?
            .ok_or_else(|| {
                FinError::new(
                    "OPERATION_NOT_FOUND",
                    "status",
                    "Operation does not exist.",
                    "Inspect available financial operations.",
                )
            })?;
        from_value(value)
    }

    fn load_boundary_record(&self, id: &str) -> Result<BoundaryAuthorizationRecord> {
        let value = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .boundary_authorization(id)
            .map_err(map_store)?
            .ok_or_else(|| {
                FinError::new(
                    "BOUNDARY_AUTHORIZATION_NOT_FOUND",
                    "authorization",
                    "Boundary authorization does not exist.",
                    "Inspect authorized operations and use the original authorization ID.",
                )
            })?;
        from_value(value)
    }

    fn save_boundary_record(
        &self,
        expected: &BoundaryAuthorizationRecord,
        record: &BoundaryAuthorizationRecord,
    ) -> Result<()> {
        self.store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .compare_update_boundary_authorization(
                &record.authorization.id,
                &to_value(expected)?,
                &to_value(record)?,
                &now(),
            )
            .map_err(map_store)
    }

    fn require_active_boundary(&self, id: &str) -> Result<BoundaryAuthorization> {
        let mut record = self.load_boundary_record(id)?;
        if record.authorization.status == BoundaryAuthorizationStatus::Active
            && parse_time(&record.authorization.expires_at)? <= Utc::now()
        {
            let before = record.clone();
            record.authorization.status = BoundaryAuthorizationStatus::Expired;
            self.save_boundary_record(&before, &record)?;
        }
        if record.authorization.status != BoundaryAuthorizationStatus::Active {
            return Err(FinError::new(
                "BOUNDARY_AUTHORIZATION_INACTIVE",
                "authorization",
                "Boundary authorization is expired, revoked, superseded, or exhausted.",
                "Review the authorization status; new or changed boundaries require fresh approval.",
            ));
        }
        if record.active_attempt.is_some() {
            return Err(FinError::new(
                "BOUNDARY_ATTEMPT_UNRESOLVED",
                "authorization",
                "An earlier attempt is unresolved and may still consume this intent.",
                "Reconcile that original attempt before proposing another candidate.",
            ));
        }
        Ok(record.authorization)
    }

    fn update_record(&self, record: &OperationRecord) -> Result<()> {
        self.store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .update_record(&record.id, &to_value(record)?, &record.updated_at)
            .map_err(map_store)
    }

    fn commit_terminal(
        &self,
        record: OperationRecord,
        proof: Value,
        effects: Value,
        run: Option<&RunLedger>,
    ) -> Result<()> {
        let settlement = Settlement {
            operation_id: record.id.clone(),
            terminal_record: to_value(&record)?,
            proof,
            effects,
            run_id: run.map(|r| r.id.clone()),
            run_record: run.map(to_value).transpose()?,
        };
        let boundary_id = record
            .preview
            .as_ref()
            .and_then(|preview| preview.binding.get(boundaries::BOUNDARY_METADATA_KEY))
            .and_then(|metadata| metadata.get("authorization_id"))
            .and_then(Value::as_str);
        if let Some(boundary_id) = boundary_id {
            let mut boundary = self.load_boundary_record(boundary_id)?;
            let expected = boundary.clone();
            if boundary.active_attempt.as_deref() == Some(record.id.as_str()) {
                boundary.active_attempt = None;
                account_boundary_effects(&mut boundary.authorization, record.effects.as_ref())?;
            }
            self.store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .commit_boundary_settlement(
                    &settlement,
                    boundary_id,
                    &to_value(&expected)?,
                    &to_value(&boundary)?,
                    &record.updated_at,
                )
                .map_err(map_store)
        } else if let Some((plan_id, _revision_id, _step_id, _state)) = {
            self.store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .financial_plan_attempt_owner(&record.id)
                .map_err(map_store)?
        } {
            let mut plan = self.load_financial_plan(&plan_id)?;
            let expected = plan.clone();
            let (_, revision_id, step_id, _) = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .financial_plan_attempt_owner(&record.id)
                .map_err(map_store)?
                .ok_or_else(|| {
                    plan_error(
                        "PLAN_ATTEMPT_MISSING",
                        "Settled operation has no durable plan attempt link.",
                        "Preserve the operation and reconcile against its original plan.",
                    )
                })?;
            let linkage = plan
                .attempts
                .iter()
                .find(|attempt| attempt.attempt_id == record.id)
                .ok_or_else(|| {
                    plan_error(
                        "PLAN_ATTEMPT_MISSING",
                        "Settled operation has no plan history entry.",
                        "Preserve the operation and reconcile against its original plan.",
                    )
                })?;
            let prepared = record.preview.as_ref().ok_or_else(|| {
                plan_error(
                    "PLAN_CANDIDATE_MISSING",
                    "Settled plan attempt has no prepared candidate.",
                    "Preserve the operation and reconcile against its original plan.",
                )
            })?;
            if linkage.revision_id != revision_id
                || linkage.step_id != step_id
                || linkage.execution_binding_hash
                    != binding_hash(
                        &fin_domain::financial_plans::financial_plan_attempt_binding(
                            prepared,
                            &plan_id,
                            &revision_id,
                            &step_id,
                        ),
                    )?
            {
                return Err(plan_error(
                    "PLAN_ATTEMPT_BINDING_MISMATCH",
                    "Settlement candidate does not match its durable plan attempt linkage.",
                    "Keep the attempt unresolved and inspect the original execution binding.",
                ));
            }
            if plan.active_attempt_id.as_deref() == Some(record.id.as_str()) {
                plan.active_attempt_id = None;
                plan.usage
                    .reservations
                    .retain(|reservation| reservation.attempt_id != record.id);
                financial_plans::account_financial_plan_effects(&mut plan, &record)?;
            }
            self.store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .commit_financial_plan_settlement(
                    &settlement,
                    &plan_id,
                    &to_value(&expected)?,
                    &to_value(&plan)?,
                    &record.id,
                    "settled",
                    &record.updated_at,
                )
                .map_err(map_store)
        } else {
            self.store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .commit_settlement(&settlement, &record.updated_at)
                .map_err(map_store)
        }
    }

    fn save_run(&self, run: &RunLedger) -> Result<()> {
        self.store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .save_run(&run.id, &to_value(run)?, &now())
            .map_err(map_store)
    }

    fn load_run(&self, id: &str) -> Result<RunLedger> {
        let value = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .get_run(id)
            .map_err(map_store)?
            .ok_or_else(|| {
                FinError::new(
                    "RUN_NOT_FOUND",
                    "budget",
                    "Run does not exist.",
                    "Inspect configured run identities.",
                )
            })?;
        from_value(value)
    }

    fn reserve_run_operation(
        &self,
        run_id: &str,
        record: &OperationRecord,
        prepared: &PreparedOperation,
    ) -> Result<RunLedger> {
        let mut run = self.load_run(run_id)?;
        if run.status == RunStatus::Restored {
            return Err(FinError::new(
                "RUN_REQUIRED",
                "budget",
                "The run is already restored.",
                "Start a new baseline before another write.",
            ));
        }
        if run.route_invalidated {
            return Err(FinError::new(
                "RETURN_ROUTE_INVALIDATED",
                "budget",
                "A prior planned operation failed, so the remaining projected route is no longer valid.",
                "Capture a fresh state and create a new complete return plan before another write.",
            ));
        }
        if !run.reservations.is_empty() {
            return Err(FinError::new(
                "RUN_PENDING",
                "budget",
                "An unresolved operation retains the run allocation.",
                "Reconcile the original submission before another write.",
            ));
        }
        validate_prepared(prepared, &record.operation, Utc::now())?;
        reject_unpriced_run(prepared)?;
        let route_step_index =
            u32::try_from(run.settled_route_steps.len()).map_err(|_| route_proof_error())?;
        let route_step = run
            .route_proofs
            .get(route_step_index as usize)
            .ok_or_else(route_proof_error)?;
        let planned_steps = run
            .planned_operations
            .iter()
            .chain(run.unwind.iter())
            .collect::<Vec<_>>();
        let planned_step = planned_steps
            .get(route_step_index as usize)
            .ok_or_else(route_proof_error)?;
        validate_route_proof_identity(planned_step, route_step, route_step_index as usize)?;
        if record.operation != route_step.operation
            || prepared.input != planned_step.input
            || !intent_within(&prepared.intent, &route_step.intent)?
        {
            return Err(FinError::new(
                "OUTSIDE_RETURN_ROUTE",
                "budget",
                "The fresh operation preview exceeds or differs from the next preflighted route step.",
                "Use the exact planned operation and input, then obtain a new complete route proof if it changed.",
            ));
        }
        let valuation = prepared.valuation.as_ref().ok_or_else(|| {
            FinError::new(
                "VALUATION_UNAVAILABLE",
                "budget",
                "Fresh USD valuations are required for a bounded run.",
                "Refresh asset and fee valuations.",
            )
        })?;
        validate_price_observation(
            &valuation.observed_at,
            &valuation.source,
            Utc::now(),
            "VALUATION_UNAVAILABLE",
        )?;
        let movement = parse_uint(valuation.movement_usd_micros.as_str())?;
        let fee_usd = parse_uint(valuation.maximum_fee_usd_micros.as_str())?;
        if fee_usd > parse_uint(&planned_step.maximum_fee_usd_micros)? {
            return Err(FinError::new(
                "OUTSIDE_RETURN_ROUTE_COST_CAP",
                "budget",
                "The fresh operation cost quote exceeds this planned step's immutable USD cap.",
                "Do not submit; revise the complete route and capture a new fresh proof chain.",
            ));
        }
        let borrowed = parse_uint(valuation.additional_allocation_usd_micros.as_str())?;
        let cost_assets = prepared_cost_assets(&prepared.intent);
        validate_cost_prices(prepared, &cost_assets, Utc::now())?;
        if [
            fin_domain::policy::limit(&self.options.max_movement_and_cost_usd_micros)?,
            fin_domain::policy::limit(&run.max_movement_usd_micros)?,
        ]
        .into_iter()
        .flatten()
        .any(|limit| &movement + &fee_usd > BigUint::from(limit))
        {
            return Err(FinError::new(
                "MOVEMENT_LIMIT",
                "budget",
                "Gross movement and maximum execution costs exceed the configured per-write limit.",
                "Reduce the amount or use a freshly verified lower cost bound.",
            ));
        }
        if movement
            > parse_uint(&run.initial_allocation_usd_micros)?
                + parse_uint(&run.borrowed_proceeds_reserve_usd_micros)?
        {
            return Err(FinError::new(
                "UNALLOCATED_FUNDS",
                "budget",
                "The operation exceeds selected holdings and the separate borrowed-proceeds allocation.",
                "Use only the selected run allocation.",
            ));
        }
        let total_borrow_reserved = run.reservations.values().fold(BigUint::zero(), |sum, row| {
            sum + parse_uint(&row.additional_allocation_usd_micros).unwrap_or_default()
        });
        if total_borrow_reserved + &borrowed
            > parse_uint(&run.borrowed_proceeds_reserve_usd_micros)?
        {
            return Err(FinError::new(
                "BORROW_ALLOCATION",
                "budget",
                "Borrowed proceeds exceed their separate reserve and cannot be netted against debt.",
                "Reduce the borrow amount.",
            ));
        }
        let total_fee_reserved = run.reservations.values().fold(
            parse_uint(&run.spent_cost_usd_micros).unwrap_or_default(),
            |sum, row| sum + parse_uint(&row.maximum_fee_usd_micros).unwrap_or_default(),
        );
        if total_fee_reserved + &fee_usd > parse_uint(&run.return_cost_reserve_usd_micros)? {
            return Err(FinError::new(
                "UNWIND_RESERVE",
                "budget",
                "This operation would consume funds reserved for the complete return.",
                "Revise the run within its original cost reserve.",
            ));
        }
        ensure_intent_scope(&run, &prepared.intent)?;
        validate_cost_bound_contract(&prepared.intent)?;
        let amounts =
            cash_reservation_amounts(&prepared.intent.debits, &prepared.intent.maximum_costs)?;
        let amount_map = aggregate_resource_amounts(&amounts)?;
        for (key, requested) in &amount_map {
            let available = aggregate_resource_amounts(&run.available)?
                .get(key)
                .cloned()
                .unwrap_or_default();
            if requested > &available {
                return Err(FinError::new(
                    "RESERVATION_EXCEEDED",
                    "budget",
                    "The operation would consume more than the selected source and fee inventory.",
                    "Reduce it to the exact allocated holdings.",
                ));
            }
        }
        let selected = select_cash_reservation_provenance(&run, &prepared.intent)?;
        run.available = subtract_amounts(&run.available, &amount_map)?;
        run.provenance = selected.remaining;
        let reservation = RunReservation {
            debits: prepared.intent.debits.clone(),
            maximum_costs: prepared.intent.maximum_costs.clone(),
            provenance: selected.principal,
            additional_cost_provenance: selected.additional_costs,
            cash_provenance_version: 3,
            cash_sources: Some(selected.sources),
            maximum_fee_usd_micros: fee_usd.to_string(),
            additional_allocation_usd_micros: borrowed.to_string(),
            binding_hash: binding_hash(&authorization_binding(prepared))?,
            route_step_index: Some(route_step_index),
        };
        run.reservations.insert(record.id.clone(), reservation);
        Ok(run)
    }

    fn recheck_run_reservation(
        &self,
        run_id: &str,
        operation_id: &str,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        let run = self.load_run(run_id)?;
        let reservation = run
            .reservations
            .get(operation_id)
            .ok_or_else(|| internal("durable run reservation disappeared"))?;
        if reservation.binding_hash != binding_hash(&authorization_binding(prepared))?
            || reservation.debits != prepared.intent.debits
            || reservation.maximum_costs != prepared.intent.maximum_costs
        {
            return Err(FinError::new(
                "REQUEST_CHANGED",
                "approval",
                "Approved execution or reserved source bounds changed during revalidation.",
                "Preview again and obtain fresh approval.",
            ));
        }
        Ok(())
    }

    fn settle_run(
        &self,
        run: &mut RunLedger,
        record: &OperationRecord,
        verified: &VerifiedEffects,
    ) -> Result<()> {
        let reservation = run.reservations.get(&record.id).cloned().ok_or_else(|| {
            FinError::new(
                "RESERVATION_MISSING",
                "reconciliation",
                "No durable run reservation exists for this submitted operation.",
                "Keep the original submission unresolved and inspect the run journal.",
            )
        })?;
        if reservation.cash_provenance_version != 3 || reservation.cash_sources.is_none() {
            return Err(FinError::new(
                "RESERVATION_PROVENANCE_VERSION_UNSUPPORTED",
                "reconciliation",
                "This reservation has no supported ordered principal and fee provenance.",
                "Keep the original submission and reservation unresolved; never replay it as a new write.",
            ));
        }
        if record.status == Status::Failed {
            // A proved failure can spend gas without completing the planned
            // movement. Its original suffix is no longer fully funded until
            // a fresh complete route accounts for that consumed reserve.
            run.route_invalidated = true;
            run.status = RunStatus::CleanupOutstanding;
        }
        if record.status == Status::Completed {
            let step_index = reservation.route_step_index.ok_or_else(route_proof_error)?;
            let proof = run
                .route_proofs
                .get(step_index as usize)
                .ok_or_else(route_proof_error)?;
            if !route_projection_matches_receipt(proof, verified)? {
                // Receipt truth wins over a stale projection: settle the verified
                // effects and retain provenance, then require a fresh bounded
                // cleanup plan before another scoped write.
                run.route_invalidated = true;
                run.status = RunStatus::CleanupOutstanding;
            }
        }
        let actual = actual_debits_and_costs(&verified.effects)?;
        let reserved = aggregate_resource_amounts(&cash_reservation_amounts(
            &reservation.debits,
            &reservation.maximum_costs,
        )?)?;
        for (key, amount) in &actual {
            if amount > reserved.get(key).unwrap_or(&BigUint::zero()) {
                return Err(FinError::new(
                    "EFFECT_EXCEEDS_RESERVATION",
                    "reconciliation",
                    "Verified debit or cost exceeds the durable exact source reservation.",
                    "Preserve the reservation and reconcile the original operation.",
                ));
            }
        }
        let actual_principal = actual_cash_debits(&verified.effects, false)?;
        let intent = &record
            .preview
            .as_ref()
            .ok_or_else(|| internal("settlement lost its prepared intent"))?
            .intent;
        let mut settled_provenance = cash_reservation_sources::settle(
            &reservation,
            &run.provenance,
            intent,
            &actual_principal,
            &actual,
        )?;
        let refunds = subtract_maps(&reserved, &actual);
        run.available = add_amount_map(&run.available, &refunds)?;
        run.provenance = settled_provenance.remaining;
        run.available = apply_credits(run, &run.available, &verified.effects)?;
        let valuation = record
            .preview
            .as_ref()
            .and_then(|prepared| prepared.valuation.as_ref())
            .ok_or_else(|| {
                FinError::new(
                    "VALUATION_MISSING",
                    "reconciliation",
                    "A bounded run lost its durable exact valuation.",
                    "Keep the run unresolved and restore its approved valuation record.",
                )
            })?;
        let mapped_costs = apply_effect_provenance(
            run,
            &record.id,
            &mut settled_provenance.principal,
            &mut settled_provenance.additional_costs,
            &verified.effects,
            valuation,
        )?;
        run.costs = add_amounts(&run.costs, &mapped_costs)?;
        if record
            .preview
            .as_ref()
            .is_some_and(|p| p.valuation.is_some())
        {
            let preview = record.preview.as_ref().expect("checked");
            let valuation = preview.valuation.as_ref().expect("checked");
            let cost_usd = actual_cost_usd(&verified.effects, valuation)?;
            if cost_usd > parse_uint(&reservation.maximum_fee_usd_micros)? {
                return Err(FinError::new(
                    "COST_EXCEEDS_USD_RESERVATION",
                    "reconciliation",
                    "The proved per-asset cost value exceeds the run's reserved USD amount.",
                    "Keep the operation unresolved and verify its exact cost prices and proof.",
                ));
            }
            run.spent_cost_usd_micros =
                (parse_uint(&run.spent_cost_usd_micros)? + cost_usd).to_string();
        }
        if record.status == Status::Completed {
            run.borrowed_allocation_used_usd_micros =
                (parse_uint(&run.borrowed_allocation_used_usd_micros)?
                    + parse_uint(&reservation.additional_allocation_usd_micros)?)
                .to_string();
            let step_index = reservation.route_step_index.ok_or_else(route_proof_error)?;
            if step_index as usize != run.settled_route_steps.len() {
                return Err(route_proof_error());
            }
            run.settled_route_steps.push(step_index);
        }
        run.reservations.remove(&record.id);
        Ok(())
    }

    fn release_unsubmitted_reservation(
        &self,
        record: &OperationRecord,
    ) -> Result<Option<RunLedger>> {
        if record
            .preview
            .as_ref()
            .is_some_and(|preview| cancellation_target(&preview.intent).is_some())
        {
            return Ok(None);
        }
        let Some(run_id) = &record.run_id else {
            return Ok(None);
        };
        let mut run = self.load_run(run_id)?;
        if let Some(reservation) = run.reservations.get(&record.id).cloned() {
            if reservation.cash_provenance_version != 3 || reservation.cash_sources.is_none() {
                return Err(FinError::new(
                    "RESERVATION_PROVENANCE_VERSION_UNSUPPORTED",
                    "reconciliation",
                    "This reservation has no supported ordered principal and fee provenance.",
                    "Keep the original reservation unresolved; never replay it as a new write.",
                ));
            }
            let restored = cash_reservation_sources::release(&reservation, &run.provenance)?;
            let amounts =
                cash_reservation_amounts(&reservation.debits, &reservation.maximum_costs)?;
            run.available = add_amounts(&run.available, &amounts)?;
            run.provenance = restored;
            run.reservations.remove(&record.id);
        }
        Ok(Some(run))
    }
}

struct OperationSubmissionJournal {
    operation_id: String,
    expected_binding_hash: String,
    approval: ApprovalAudit,
    boundary_authorization_id: Option<String>,
    boundary_authorization_snapshot: Option<BoundaryAuthorizationRecord>,
    financial_plan_id: Option<String>,
    financial_plan_snapshot: Option<FinancialPlanAuthorizationRecord>,
    adapter: Option<Arc<dyn Adapter>>,
    options: EngineOptions,
    consumed: AtomicBool,
    store: Arc<Mutex<Store>>,
    record: Mutex<OperationRecord>,
}

impl SubmissionJournal for OperationSubmissionJournal {
    fn assert_authorized(&self) -> Result<()> {
        ensure_audit_binding(&self.approval, &self.expected_binding_hash)?;
        if let (Some(id), Some(snapshot)) = (&self.financial_plan_id, &self.financial_plan_snapshot)
        {
            let current: FinancialPlanAuthorizationRecord = {
                let store = self
                    .store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?;
                from_value(store.financial_plan(id).map_err(map_store)?.ok_or_else(|| plan_error("PLAN_NOT_FOUND", "Plan authority no longer exists.", "Keep the attempt unresolved and reconcile its original submission."))?)?
            };
            if current.revision.revision_id != snapshot.revision.revision_id
                || current.revision.status != FinancialPlanAuthorizationStatus::Active
                || parse_time(&current.revision.expires_at)? <= Utc::now()
                || current.active_attempt_id.as_deref() != Some(self.operation_id.as_str())
            {
                return Err(plan_error(
                    "PLAN_AUTHORIZATION_INACTIVE",
                    "Plan authority is revoked, expired, or no longer owns this attempt.",
                    "Reconcile the original attempt; fresh authorization is required for another attempt.",
                ));
            }
            let operation = self
                .record
                .lock()
                .map_err(|_| internal("journal record lock poisoned"))?
                .clone();
            let prepared = operation.preview.ok_or_else(|| {
                plan_error(
                    "PLAN_CANDIDATE_MISSING",
                    "Plan attempt has no durable prepared candidate.",
                    "Keep the attempt unresolved and inspect its original record.",
                )
            })?;
            let linkage = current
                .attempts
                .iter()
                .find(|attempt| attempt.attempt_id == self.operation_id)
                .ok_or_else(|| {
                    plan_error(
                        "PLAN_ATTEMPT_MISSING",
                        "Plan attempt linkage is missing.",
                        "Keep the attempt unresolved and reconcile its original record.",
                    )
                })?;
            let candidate_hash = binding_hash(
                &fin_domain::financial_plans::financial_plan_attempt_binding(
                    &prepared,
                    id,
                    &current.revision.revision_id,
                    &linkage.step_id,
                ),
            )?;
            if candidate_hash != linkage.execution_binding_hash {
                return Err(plan_error(
                    "PLAN_CANDIDATE_CHANGED",
                    "The concrete candidate changed after it was bound to this attempt.",
                    "Reconcile this attempt before proposing another candidate.",
                ));
            }
            {
                let store = self
                    .store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?;
                ordinary_policy::validate_boundary_candidate_policy(
                    &prepared,
                    &self.operation_id,
                    &self.options,
                    &store,
                )?;
            }
            return Ok(());
        }
        if let Some(id) = &self.boundary_authorization_id {
            let grant = self.active_boundary_grant(id)?;
            let prepared = self
                .record
                .lock()
                .map_err(|_| internal("journal record lock poisoned"))?
                .preview
                .clone()
                .ok_or_else(|| {
                    boundary_error("Boundary attempt has no durable candidate preview.")
                })?;
            self.validate_boundary_candidate(&grant, &prepared)?;
            Ok(())
        } else {
            validate_future_expiry(&self.approval.expires_at, Utc::now(), "APPROVAL_EXPIRED")
        }
    }

    fn assert_candidate(&self, prepared: &PreparedOperation) -> Result<()> {
        if let (Some(id), Some(snapshot)) = (&self.financial_plan_id, &self.financial_plan_snapshot)
        {
            self.assert_authorized()?;
            let raw_input = self
                .record
                .lock()
                .map_err(|_| internal("journal record lock poisoned"))?
                .input
                .clone();
            let current: FinancialPlanAuthorizationRecord = from_value(
                self.store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?
                    .financial_plan(id)
                    .map_err(map_store)?
                    .ok_or_else(|| {
                        plan_error(
                            "PLAN_NOT_FOUND",
                            "Plan authority no longer exists.",
                            "Reconcile the original attempt.",
                        )
                    })?,
            )?;
            let linkage = current
                .attempts
                .iter()
                .find(|attempt| attempt.attempt_id == self.operation_id)
                .ok_or_else(|| {
                    plan_error(
                        "PLAN_ATTEMPT_MISSING",
                        "Plan attempt linkage is missing.",
                        "Reconcile the original attempt.",
                    )
                })?;
            let hash = binding_hash(
                &fin_domain::financial_plans::financial_plan_attempt_binding(
                    prepared,
                    id,
                    &current.revision.revision_id,
                    &linkage.step_id,
                ),
            )?;
            if hash != linkage.execution_binding_hash {
                return Err(plan_error(
                    "PLAN_CANDIDATE_CHANGED",
                    "The adapter revalidation changed the immutable plan candidate.",
                    "Reconcile this attempt before proposing another candidate.",
                ));
            }
            let adapter = self
                .adapter
                .as_ref()
                .ok_or_else(|| internal("plan execution adapter is missing"))?;
            let permission = permission_for_step(&current.revision, &linkage.step_id)?;
            if let Some(exact) = &permission.exact {
                let step = current
                    .revision
                    .spec
                    .steps
                    .iter()
                    .find(|step| step.step_id == linkage.step_id)
                    .ok_or_else(|| internal("plan step disappeared"))?;
                financial_plans::validate_exact_plan_candidate(
                    adapter.as_ref(),
                    &step.operation,
                    &step.input,
                    exact,
                    &raw_input,
                    prepared,
                )?;
            } else {
                let scope = permission
                    .permission
                    .as_ref()
                    .ok_or_else(|| internal("adapter plan permission disappeared"))?;
                adapter.validate_boundary_candidate(&scope.spec, prepared)?;
            }
            financial_plans::validate_generated_plan_candidate(
                adapter.as_ref(),
                &current.revision,
                &linkage.step_id,
                &raw_input,
                &self.operation_id,
                prepared,
            )?;
            let mut durable_candidate = prepared.clone();
            financial_plans::inject_plan_marker(
                &mut durable_candidate,
                &current,
                &linkage.step_id,
                hash.clone(),
            )?;
            let updated_record = {
                let mut record = self
                    .record
                    .lock()
                    .map_err(|_| internal("journal record lock poisoned"))?;
                record.preview = Some(durable_candidate.clone());
                record.updated_at = now();
                record.clone()
            };
            let mut store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            validate_candidate_plan_caps(&current, &linkage.step_id, prepared, &store)?;
            ordinary_policy::validate_boundary_candidate_policy(
                prepared,
                &self.operation_id,
                &self.options,
                &store,
            )?;
            store
                .update_financial_plan_candidate_preview(
                    id,
                    &to_value(snapshot)?,
                    &self.operation_id,
                    &to_value(&updated_record)?,
                    &updated_record.updated_at,
                )
                .map_err(map_store)?;
            return Ok(());
        }
        let Some(grant_id) = self.boundary_authorization_id.as_deref() else {
            return self.assert_authorized();
        };
        ensure_audit_binding(&self.approval, &self.expected_binding_hash)?;
        let grant = self.active_boundary_grant(grant_id)?;
        self.validate_boundary_candidate(&grant, prepared)?;
        let record = {
            let mut current = self
                .record
                .lock()
                .map_err(|_| internal("journal record lock poisoned"))?;
            current.preview = Some(prepared.clone());
            current.updated_at = now();
            current.clone()
        };
        self.store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .update_boundary_candidate_preview(
                grant_id,
                &to_value(&grant)?,
                &self.operation_id,
                &to_value(&record)?,
                &record.updated_at,
            )
            .map_err(map_store)?;
        Ok(())
    }

    fn persist_submission(&self, submission: &Submission) -> Result<()> {
        self.persist_submission_internal(submission)
    }

    fn persist_dispatch_evidence(&self, submission: &Submission, evidence: &Value) -> Result<()> {
        let mut record = self
            .record
            .lock()
            .map_err(|_| internal("journal record lock poisoned"))?;
        if record.id != self.operation_id || record.submission.as_ref() != Some(submission) {
            return Err(FinError::new(
                "DISPATCH_EVIDENCE_SUBMISSION_MISMATCH",
                "reconciliation",
                "Dispatch evidence does not match this operation's durable submission.",
                "Keep the operation unresolved and reconcile its original submission.",
            ));
        }
        match record.dispatch_evidence.as_ref() {
            Some(existing) if existing == evidence => return Ok(()),
            Some(_) => {
                return Err(FinError::new(
                    "DISPATCH_EVIDENCE_CONFLICT",
                    "reconciliation",
                    "Different response evidence is already recorded for this submission.",
                    "Keep the operation unresolved and inspect its existing evidence.",
                ));
            }
            None => {}
        }
        let submission_row = SubmissionRow {
            kind: submission.kind.clone(),
            reference: submission.reference.clone(),
            data: submission.data.clone(),
        };
        self.store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .persist_dispatch_evidence(&self.operation_id, &submission_row, evidence)
            .map_err(map_store)?;
        record.dispatch_evidence = Some(evidence.clone());
        Ok(())
    }
}

impl OperationSubmissionJournal {
    fn active_boundary_grant(&self, id: &str) -> Result<BoundaryAuthorizationRecord> {
        let raw = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .boundary_authorization(id)
            .map_err(map_store)?
            .ok_or_else(|| boundary_error("Boundary authorization no longer exists."))?;
        let record: BoundaryAuthorizationRecord = from_value(raw)?;
        if record.authorization.status != BoundaryAuthorizationStatus::Active
            || parse_time(&record.authorization.expires_at)? <= Utc::now()
            || record.active_attempt.as_deref() != Some(self.operation_id.as_str())
        {
            return Err(boundary_error(
                "Boundary authorization is revoked, expired, or no longer owns this attempt.",
            ));
        }
        Ok(record)
    }

    fn validate_boundary_candidate(
        &self,
        grant: &BoundaryAuthorizationRecord,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        let metadata = prepared
            .binding
            .get(boundaries::BOUNDARY_METADATA_KEY)
            .ok_or_else(|| {
                boundary_error("Prepared candidate lost its boundary authorization marker.")
            })?;
        let expected = metadata
            .get("attempt_binding_hash")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                boundary_error("Prepared candidate has no immutable attempt binding.")
            })?;
        if binding_hash(&boundaries::attempt_binding(prepared))? != expected {
            return Err(FinError::new(
                "REQUEST_CHANGED",
                "approval",
                "Revalidated candidate differs from its immutable attempt binding.",
                "Prepare and review the changed candidate.",
            ));
        }
        validate_candidate_caps(&grant.authorization, prepared)?;
        let store = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?;
        ordinary_policy::validate_boundary_candidate_policy(
            prepared,
            &self.operation_id,
            &self.options,
            &store,
        )
    }

    fn persist_submission_internal(&self, submission: &Submission) -> Result<()> {
        self.assert_authorized()?;
        if self
            .consumed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(FinError::new(
                "APPROVAL_ALREADY_CONSUMED",
                "submission",
                "This one-use approval has already authorized a submission identity.",
                "Reconcile the original operation and obtain fresh approval for any new write.",
            ));
        }
        let mut record = self
            .record
            .lock()
            .map_err(|_| internal("journal record lock poisoned"))?;
        if record.id != self.operation_id || record.submission.is_some() {
            return Err(FinError::new(
                "SUBMISSION_IDENTITY_CHANGED",
                "submission",
                "Submission identity is already fixed for this operation.",
                "Reconcile the existing submission.",
            ));
        }
        let submission_row = SubmissionRow {
            kind: submission.kind.clone(),
            reference: submission.reference.clone(),
            data: submission.data.clone(),
        };
        let mut store = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?;
        if let (Some(id), Some(snapshot)) = (
            &self.boundary_authorization_id,
            &self.boundary_authorization_snapshot,
        ) {
            store
                .persist_boundary_submission(
                    id,
                    &to_value(snapshot)?,
                    &self.operation_id,
                    &submission_row,
                    &now(),
                )
                .map_err(map_store)?;
        } else if let (Some(id), Some(snapshot)) =
            (&self.financial_plan_id, &self.financial_plan_snapshot)
        {
            let mut updated = snapshot.clone();
            if let Some(linkage) = updated
                .attempts
                .iter_mut()
                .find(|attempt| attempt.attempt_id == self.operation_id)
            {
                linkage.submission_id =
                    Some(format!("{}:{}", submission.kind, submission.reference));
            }
            store
                .persist_financial_plan_submission(
                    id,
                    &to_value(snapshot)?,
                    &to_value(&updated)?,
                    &self.operation_id,
                    &snapshot.revision.expires_at,
                    &submission_row,
                    &now(),
                )
                .map_err(map_store)?;
        } else {
            store
                .persist_submission(&self.operation_id, &submission_row, &now())
                .map_err(map_store)?;
        }
        record.submission = Some(submission.clone());
        record.status = Status::Submitted;
        record.updated_at = now();
        Ok(())
    }
}

fn cancellation_target(intent: &FinancialIntent) -> Option<(&str, &Submission)> {
    match intent.expected.as_slice() {
        [
            EffectConstraint::SubmissionCancellation {
                operation_id,
                submission,
            },
        ] => Some((operation_id, submission)),
        _ => None,
    }
}

fn cancellation_error() -> FinError {
    FinError::new(
        "CANCELLATION_TARGET_MISMATCH",
        "validation",
        "Cancellation must target one unresolved durable submission in the same run with the same resource locks.",
        "Inspect the original operation and cancel its exact remaining order without replacing it.",
    )
}

fn validate_effects(
    record: &OperationRecord,
    verified: &VerifiedEffects,
    outcome: &Status,
) -> Result<()> {
    if verified.evidence_version == 0 || verified.evidence.is_null() {
        return Err(FinError::new(
            "INVALID_VERIFIED_EFFECTS",
            "reconciliation",
            "Adapter proof is incomplete or unversioned.",
            "Keep the original reservation and collect versioned receipt evidence.",
        ));
    }
    if record.submission.as_ref() != Some(&verified.submission) {
        return Err(FinError::new(
            "SUBMISSION_IDENTITY_MISMATCH",
            "reconciliation",
            "Receipt proof refers to a different external submission.",
            "Reconcile using the original persisted submission identity.",
        ));
    }
    let prepared = record.preview.as_ref().ok_or_else(|| {
        FinError::new(
            "PREVIEW_MISSING",
            "reconciliation",
            "The durable prepared operation is missing.",
            "Do not settle; restore a compatible operation record.",
        )
    })?;
    let approval = record.approval.as_ref().ok_or_else(|| {
        FinError::new(
            "APPROVAL_AUDIT_MISSING",
            "reconciliation",
            "The durable approval audit is missing.",
            "Keep the operation unresolved and restore a compatible journal.",
        )
    })?;
    let plan_metadata = prepared
        .binding
        .get(fin_domain::financial_plans::PLAN_METADATA_KEY);
    let approval_matches = if let Some(metadata) = plan_metadata {
        let plan_id = metadata.get("plan_id").and_then(Value::as_str);
        let revision_id = metadata.get("revision_id").and_then(Value::as_str);
        let step_id = metadata.get("step_id").and_then(Value::as_str);
        let attempt_hash = metadata.get("attempt_binding_hash").and_then(Value::as_str);
        let approval_hash = metadata
            .get("approval_binding_hash")
            .and_then(Value::as_str);
        match (plan_id, revision_id, step_id, attempt_hash, approval_hash) {
            (
                Some(plan_id),
                Some(revision_id),
                Some(step_id),
                Some(attempt_hash),
                Some(approval_hash),
            ) => {
                approval_hash == approval.binding_hash
                    && attempt_hash
                        == binding_hash(
                            &fin_domain::financial_plans::financial_plan_attempt_binding(
                                prepared,
                                plan_id,
                                revision_id,
                                step_id,
                            ),
                        )?
            }
            _ => false,
        }
    } else {
        binding_hash(&authorization_binding(prepared))? == approval.binding_hash
    };
    if !approval_matches {
        return Err(FinError::new(
            "APPROVAL_PROOF_MISMATCH",
            "reconciliation",
            "Verified effects are not bound to the exact prepared operation and its one-use approval.",
            "Keep the original reservation and reconcile against the durable approval challenge.",
        ));
    }
    if verified.proof_digest != verified_effect_digest(verified)? {
        return Err(FinError::new(
            "PROOF_DIGEST_MISMATCH",
            "reconciliation",
            "Verified effect bundle digest does not match its canonical typed proof.",
            "Keep the original reservation and obtain a complete proof for the original submission.",
        ));
    }
    let confirmed_failure = matches!(outcome, Status::Failed);
    let mut matched = vec![false; prepared.intent.expected.len()];
    for effect in &verified.effects {
        if confirmed_failure && !matches!(effect, FinancialEffect::Cost { .. }) {
            return Err(FinError::new(
                "FAILED_EFFECT_MISMATCH",
                "reconciliation",
                "A confirmed failed operation may prove attributable costs only; it cannot claim its intended financial effect.",
                "Keep the reservation and reconcile the original submission.",
            ));
        }
        validate_effect_anchor(effect, &verified.observed_at)?;
        let mut found = false;
        for (index, constraint) in prepared.intent.expected.iter().enumerate() {
            if constraint_matches(constraint, effect)? {
                if matched[index] {
                    continue;
                }
                matched[index] = true;
                found = true;
                break;
            }
        }
        if !found && !cost_within_intent(effect, &prepared.intent, &verified.effects)? {
            return Err(FinError::new(
                "UNEXPECTED_FINANCIAL_EFFECT",
                "reconciliation",
                "A verified financial effect does not satisfy the approved expectation or cost reserve.",
                "Preserve the operation as unresolved and review its typed proof.",
            ));
        }
    }
    if !confirmed_failure && matched.iter().any(|value| !value) {
        return Err(FinError::new(
            "EXPECTED_EFFECT_MISSING",
            "reconciliation",
            "Verified proof does not establish every approved effect.",
            "Keep the source reservation and reconcile the original operation.",
        ));
    }
    let mut losses = BTreeMap::<String, BigUint>::new();
    for effect in &verified.effects {
        if let FinancialEffect::Settlement {
            resource,
            asset,
            delta,
            ..
        } = effect
            && delta.is_negative()
        {
            *losses
                .entry(cost_key(
                    &AccountingLocation::Cash {
                        resource: resource.clone(),
                    },
                    asset,
                    &CostTreatment::IncludedInDebit,
                )?)
                .or_default() += delta.magnitude().integer();
        }
    }
    let actual_costs = aggregate_costs(&verified.effects)?;
    if losses
        .iter()
        .any(|(key, amount)| actual_costs.get(key).cloned().unwrap_or_default() < *amount)
    {
        return Err(FinError::new(
            "SETTLEMENT_LOSS_COST_MISSING",
            "reconciliation",
            "Realized collateral loss has no matching typed cost.",
            "Preserve the reservation and reconcile the complete loss evidence.",
        ));
    }
    let actual = actual_debits_and_costs(&verified.effects)?;
    let maximums =
        cash_reservation_amounts(&prepared.intent.debits, &prepared.intent.maximum_costs)?;
    let maximums = aggregate_resource_amounts(&maximums)?;
    let actual_cost_map = aggregate_costs(&verified.effects)?;
    let maximum_cost_map = aggregate_cost_bounds(&prepared.intent.maximum_costs)?;
    for (key, amount) in actual_cost_map {
        if amount > maximum_cost_map.get(&key).cloned().unwrap_or_default() {
            return Err(FinError::new(
                "COST_EXCEEDS_RESERVATION",
                "reconciliation",
                "Proved costs exceed the exact asset and resource cost reserve.",
                "Keep the reservation and reconcile the original operation.",
            ));
        }
    }
    for (key, amount) in actual {
        if amount > maximums.get(&key).cloned().unwrap_or_default() {
            return Err(FinError::new(
                "EFFECT_EXCEEDS_RESERVATION",
                "reconciliation",
                "Actual debit or cost exceeds the exact approved maximum.",
                "Keep the reservation and reconcile the original operation.",
            ));
        }
    }
    if record.run_id.is_some() {
        validate_origin_links(&verified.effects)?;
    }
    Ok(())
}

fn constraint_matches(constraint: &EffectConstraint, effect: &FinancialEffect) -> Result<bool> {
    Ok(match (constraint, effect) {
        (
            EffectConstraint::SubmissionCancellation {
                operation_id,
                submission,
            },
            FinancialEffect::SubmissionCancellation {
                operation_id: actual_id,
                submission: actual_submission,
            },
        ) => operation_id == actual_id && submission == actual_submission,
        (
            EffectConstraint::DerivativePosition {
                resource,
                position,
                asset,
                position_kind,
                minimum_delta,
                maximum_delta,
            },
            FinancialEffect::PositionChange {
                resource: r,
                position: p,
                asset: a,
                delta,
                anchor: Some(anchor),
                ..
            },
        ) => {
            position_kind.is_derivative()
                && r == resource
                && p == position
                && a == asset
                && &anchor.position_kind == position_kind
                && minimum_delta.integer() <= delta.integer()
                && delta.integer() <= maximum_delta.integer()
        }
        (
            EffectConstraint::Settlement {
                resource,
                asset,
                category,
                minimum_delta,
                maximum_delta,
            },
            FinancialEffect::Settlement {
                resource: r,
                asset: a,
                category: c,
                delta,
            },
        ) => {
            r == resource
                && a == asset
                && c == category
                && minimum_delta.integer() <= delta.integer()
                && delta.integer() <= maximum_delta.integer()
        }
        (
            EffectConstraint::OriginMovement {
                from,
                to,
                debit_asset,
                credit_asset,
                maximum_debit,
                minimum_credit,
            },
            FinancialEffect::OriginMovement {
                from: actual_from,
                to: actual_to,
                debit,
                credit,
            },
        ) => {
            actual_from == from
                && actual_to == to
                && debit.asset == *debit_asset
                && credit.asset == *credit_asset
                && !debit.atoms.is_zero()
                && !credit.atoms.is_zero()
                && debit.atoms <= *maximum_debit
                && credit.atoms >= *minimum_credit
        }
        (
            EffectConstraint::Accrual {
                resource,
                position,
                asset,
                position_kind,
                maximum_change,
            },
            FinancialEffect::Accrual {
                resource: actual_resource,
                position: actual_position,
                asset: actual_asset,
                position_kind: actual_kind,
                delta,
            },
        ) => {
            actual_resource == resource
                && actual_position == position
                && actual_asset == asset
                && actual_kind == position_kind
                && delta.magnitude() <= *maximum_change
        }
        (
            EffectConstraint::Debit {
                resource,
                asset,
                maximum_debit,
            },
            FinancialEffect::CashDebit {
                resource: actual_resource,
                amount,
            },
        ) => {
            actual_resource == resource && amount.asset == *asset && amount.atoms <= *maximum_debit
        }
        (
            EffectConstraint::Transfer {
                from,
                to,
                debit_asset,
                credit_asset,
                maximum_debit,
                minimum_credit,
            },
            FinancialEffect::Transfer {
                from: actual_from,
                to: actual_to,
                debit,
                credit,
            },
        ) => {
            actual_from == from
                && actual_to == to
                && debit.asset == *debit_asset
                && credit.asset == *credit_asset
                && debit.atoms <= *maximum_debit
                && credit.atoms >= *minimum_credit
        }
        (
            EffectConstraint::Conversion {
                resource,
                input,
                maximum_input,
                output,
                minimum_output,
            },
            FinancialEffect::Conversion {
                resource: actual_resource,
                spent,
                received,
            },
        ) => {
            actual_resource == resource
                && spent.asset == *input
                && received.asset == *output
                && spent.atoms <= *maximum_input
                && received.atoms >= *minimum_output
        }
        (
            EffectConstraint::Position {
                resource,
                position,
                asset,
                direction,
                maximum_change,
            },
            FinancialEffect::PositionChange {
                resource: actual_resource,
                position: actual_position,
                asset: actual_asset,
                delta,
                anchor,
                ..
            },
        ) => {
            anchor
                .as_ref()
                .is_none_or(|anchor| anchor.position_kind == PositionKind::Supply)
                && actual_resource == resource
                && actual_position == position
                && actual_asset == asset
                && matches_direction(delta.is_negative(), delta.is_zero(), direction)
                && delta.magnitude() <= *maximum_change
        }
        (
            EffectConstraint::Debt {
                resource,
                debt,
                asset,
                direction,
                maximum_change,
            },
            FinancialEffect::DebtChange {
                resource: actual_resource,
                debt: actual_debt,
                asset: actual_asset,
                delta,
                ..
            },
        ) => {
            actual_resource == resource
                && actual_debt == debt
                && actual_asset == asset
                && matches_direction(delta.is_negative(), delta.is_zero(), direction)
                && delta.magnitude() <= *maximum_change
        }
        (
            EffectConstraint::Authority {
                resource,
                authority,
                before,
                after,
            },
            FinancialEffect::AuthorityChange {
                resource: actual_resource,
                authority: actual_authority,
                before: actual_before,
                after: actual_after,
            },
        ) => {
            actual_resource == resource
                && actual_authority == authority
                && actual_before == before
                && actual_after == after
        }
        (
            EffectConstraint::AuthorityTransition {
                resource,
                authority,
                before,
                allowed_after,
            },
            FinancialEffect::AuthorityChange {
                resource: actual_resource,
                authority: actual_authority,
                before: actual_before,
                after,
            },
        ) => {
            actual_resource == resource
                && actual_authority == authority
                && actual_before == before
                && !allowed_after.is_empty()
                && allowed_after.iter().collect::<BTreeSet<_>>().len() == allowed_after.len()
                && allowed_after.contains(after)
        }
        (
            EffectConstraint::Credit {
                to,
                asset,
                minimum_credit,
            },
            FinancialEffect::UsableCredit {
                to: actual_to,
                amount,
            },
        ) => actual_to == to && amount.asset == *asset && amount.atoms >= *minimum_credit,
        _ => false,
    })
}

fn validate_origin_links(effects: &[FinancialEffect]) -> Result<()> {
    let mut linked_sources = BTreeMap::<String, BigUint>::new();
    let mut linked_destinations = BTreeMap::<String, BigUint>::new();
    for effect in effects {
        if let FinancialEffect::OriginMovement {
            from,
            to,
            debit,
            credit,
        } = effect
        {
            if [from, to].iter().any(|location| matches!(location, AccountingLocation::Position { position_kind, .. } if position_kind.is_derivative()))
                || from == to || debit.atoms.is_zero() || credit.atoms.is_zero() {
                return Err(FinError::new(
                    "INVALID_ORIGIN_MOVEMENT",
                    "reconciliation",
                    "A provenance link must connect distinct accounting locations with positive exact quantities.",
                    "Keep the operation unresolved and reconcile typed source and destination evidence.",
                ));
            }
            *linked_sources
                .entry(accounting_key(from, &debit.asset)?)
                .or_default() += debit.atoms.integer();
            *linked_destinations
                .entry(accounting_key(to, &credit.asset)?)
                .or_default() += credit.atoms.integer();
        }
    }
    let mut actual_sources = BTreeMap::<String, BigUint>::new();
    let mut actual_destinations = BTreeMap::<String, BigUint>::new();
    for effect in effects {
        match effect {
            FinancialEffect::CashDebit { resource, amount } => {
                add_accounting_leg(
                    &mut actual_sources,
                    &AccountingLocation::Cash {
                        resource: resource.clone(),
                    },
                    &amount.asset,
                    amount.atoms.integer(),
                )?;
            }
            FinancialEffect::UsableCredit { to, amount } => {
                add_accounting_leg(
                    &mut actual_destinations,
                    &AccountingLocation::Cash {
                        resource: to.clone(),
                    },
                    &amount.asset,
                    amount.atoms.integer(),
                )?;
            }
            FinancialEffect::PositionChange {
                resource,
                position,
                asset,
                delta,
                anchor,
                ..
            } => {
                if anchor
                    .as_ref()
                    .is_some_and(|anchor| anchor.position_kind.is_derivative())
                {
                    continue;
                }
                let location = position_location(effects, resource, position, asset)?;
                if delta.is_negative() {
                    add_accounting_leg(
                        &mut actual_sources,
                        &location,
                        asset,
                        delta.magnitude().integer(),
                    )?;
                } else if !delta.is_zero() {
                    add_accounting_leg(
                        &mut actual_destinations,
                        &location,
                        asset,
                        delta.magnitude().integer(),
                    )?;
                }
            }
            FinancialEffect::DebtChange {
                resource,
                debt,
                asset,
                delta,
                ..
            } => {
                let location = debt_location(effects, resource, debt, asset)?;
                if delta.is_negative() {
                    add_accounting_leg(
                        &mut actual_destinations,
                        &location,
                        asset,
                        delta.magnitude().integer(),
                    )?;
                } else if !delta.is_zero() {
                    add_accounting_leg(
                        &mut actual_sources,
                        &location,
                        asset,
                        delta.magnitude().integer(),
                    )?;
                }
            }
            _ => {}
        }
    }
    if linked_sources != actual_sources || linked_destinations != actual_destinations {
        return Err(FinError::new(
            "ORIGIN_LINK_MISMATCH",
            "reconciliation",
            "Provenance links do not exactly cover the proved cash, position and debt legs.",
            "Keep the run reservation and collect matching versioned financial-leg evidence.",
        ));
    }
    Ok(())
}

fn position_location(
    effects: &[FinancialEffect],
    resource: &ResourceId,
    position: &str,
    asset: &fin_domain::AssetId,
) -> Result<AccountingLocation> {
    let mut kinds = BTreeSet::new();
    for effect in effects {
        if let FinancialEffect::OriginMovement {
            from,
            to,
            debit,
            credit,
        } = effect
        {
            for location in [from, to] {
                if let AccountingLocation::Position {
                    resource: actual_resource,
                    position: actual_position,
                    position_kind,
                } = location
                    && actual_resource == resource
                    && actual_position == position
                    && (debit.asset == *asset || credit.asset == *asset)
                {
                    kinds.insert(position_kind.clone());
                }
            }
        }
    }
    if kinds.len() != 1 {
        return Err(FinError::new(
            "ORIGIN_POSITION_KIND_MISSING",
            "reconciliation",
            "A position balance leg has no unique typed origin-location link.",
            "Keep the run unresolved and include the exact position kind in its provenance link.",
        ));
    }
    Ok(AccountingLocation::Position {
        resource: resource.clone(),
        position: position.into(),
        position_kind: kinds.into_iter().next().expect("one kind"),
    })
}

fn debt_location(
    effects: &[FinancialEffect],
    resource: &ResourceId,
    debt: &str,
    asset: &fin_domain::AssetId,
) -> Result<AccountingLocation> {
    position_location(effects, resource, debt, asset)
}

fn accounting_key(location: &AccountingLocation, asset: &fin_domain::AssetId) -> Result<String> {
    serde_json::to_string(&(location, asset))
        .map_err(|_| FinError::validation("Invalid accounting location identity."))
}

fn add_accounting_leg(
    map: &mut BTreeMap<String, BigUint>,
    location: &AccountingLocation,
    asset: &fin_domain::AssetId,
    amount: BigUint,
) -> Result<()> {
    *map.entry(accounting_key(location, asset)?).or_default() += amount;
    Ok(())
}

fn matches_direction(negative: bool, zero: bool, direction: &Direction) -> bool {
    !zero
        && match direction {
            Direction::Increase => !negative,
            Direction::Decrease => negative,
        }
}

fn cost_within_intent(
    effect: &FinancialEffect,
    intent: &FinancialIntent,
    effects: &[FinancialEffect],
) -> Result<bool> {
    let FinancialEffect::Cost {
        location,
        amount,
        treatment,
        ..
    } = effect
    else {
        return Ok(false);
    };
    let key = cost_key(location, &amount.asset, treatment)?;
    let maximums = aggregate_cost_bounds(&intent.maximum_costs)?;
    if amount.atoms.integer() > maximums.get(&key).cloned().unwrap_or_default() {
        return Ok(false);
    }
    match treatment {
        CostTreatment::AdditionalDebit => Ok(matches!(location, AccountingLocation::Cash { .. })),
        CostTreatment::DeductedFromCredit => {
            let AccountingLocation::Cash { resource } = location else {
                return Ok(false);
            };
            let gross = effects.iter().fold(BigUint::zero(), |sum, effect| {
                sum + match effect {
                    FinancialEffect::Conversion {
                        resource: r,
                        received,
                        ..
                    } if r == resource && received.asset == amount.asset => {
                        received.atoms.integer()
                    }
                    FinancialEffect::Transfer { to, credit, .. }
                        if to == resource && credit.asset == amount.asset =>
                    {
                        credit.atoms.integer()
                    }
                    _ => BigUint::zero(),
                }
            });
            let withheld = effects.iter().fold(BigUint::zero(), |sum, effect| {
                sum + match effect {
                    FinancialEffect::Cost {
                        location: l,
                        amount: a,
                        treatment: CostTreatment::DeductedFromCredit,
                        ..
                    } if l == location && a.asset == amount.asset => a.atoms.integer(),
                    _ => BigUint::zero(),
                }
            });
            Ok(withheld <= gross)
        }
        CostTreatment::IncludedInDebit => {
            let total_cost = effects
                .iter()
                .filter_map(|candidate| match candidate {
                    FinancialEffect::Cost {
                        location: actual_location,
                        amount: actual_amount,
                        treatment: CostTreatment::IncludedInDebit,
                        ..
                    } if actual_location == location && actual_amount.asset == amount.asset => {
                        Some(actual_amount.atoms.integer())
                    }
                    _ => None,
                })
                .fold(BigUint::zero(), |sum, value| sum + value);
            let included_capacity = match location {
                AccountingLocation::Cash { resource } => effects
                    .iter()
                    .filter_map(|candidate| match candidate {
                        FinancialEffect::Settlement {
                            resource: r,
                            asset,
                            delta,
                            ..
                        } if r == resource && asset == &amount.asset && delta.is_negative() => {
                            Some(delta.magnitude().integer())
                        }
                        FinancialEffect::CashDebit {
                            resource: actual_resource,
                            amount: actual_amount,
                        } if actual_resource == resource && actual_amount.asset == amount.asset => {
                            Some(actual_amount.atoms.integer())
                        }
                        FinancialEffect::Transfer { from, debit, .. }
                            if from == resource && debit.asset == amount.asset =>
                        {
                            Some(debit.atoms.integer())
                        }
                        FinancialEffect::Conversion {
                            resource: actual_resource,
                            spent,
                            ..
                        } if actual_resource == resource && spent.asset == amount.asset => {
                            Some(spent.atoms.integer())
                        }
                        _ => None,
                    })
                    .fold(BigUint::zero(), |sum, value| sum + value),
                AccountingLocation::Position { .. } => effects
                    .iter()
                    .filter_map(|candidate| match candidate {
                        FinancialEffect::OriginMovement {
                            from,
                            debit,
                            credit,
                            ..
                        } if from == location
                            && debit.asset == amount.asset
                            && credit.asset == amount.asset
                            && debit.atoms >= credit.atoms =>
                        {
                            Some(debit.atoms.integer() - credit.atoms.integer())
                        }
                        _ => None,
                    })
                    .fold(BigUint::zero(), |sum, value| sum + value),
            };
            Ok(total_cost <= included_capacity)
        }
    }
}

fn validate_effect_anchor(effect: &FinancialEffect, proof_observed_at: &str) -> Result<()> {
    let (resource, name, asset, kind, anchor, scaled_receipt) = match effect {
        FinancialEffect::PositionChange {
            resource,
            position,
            asset,
            delta,
            scaled_receipt,
            anchor,
        } if !delta.is_zero() || anchor.is_some() => (
            resource,
            position,
            asset,
            anchor
                .as_ref()
                .map(|a| a.position_kind.clone())
                .unwrap_or(PositionKind::Supply),
            anchor.as_ref(),
            scaled_receipt.as_ref(),
        ),
        FinancialEffect::DebtChange {
            resource,
            debt,
            asset,
            delta,
            anchor,
        } if !delta.is_zero() => (
            resource,
            debt,
            asset,
            PositionKind::VariableDebt,
            anchor.as_ref(),
            None,
        ),
        _ => return Ok(()),
    };
    let anchor = anchor.ok_or_else(|| {
        FinError::new(
            "POSITION_ANCHOR_MISSING",
            "reconciliation",
            "A nonzero position or debt change has no verified post-state anchor.",
            "Keep the reservation and reconcile with the original submission's exact backing state.",
        )
    })?;
    let kind_matches = matches!(
        (&kind, &anchor.position_kind),
        (PositionKind::Supply, PositionKind::Supply)
            | (PositionKind::DerivativeLong, PositionKind::DerivativeLong)
            | (PositionKind::DerivativeShort, PositionKind::DerivativeShort)
            | (
                PositionKind::VariableDebt,
                PositionKind::VariableDebt | PositionKind::StableDebt
            )
    );
    let anchor_time = DateTime::parse_from_rfc3339(&anchor.observed_at).ok();
    let proof_time = DateTime::parse_from_rfc3339(proof_observed_at).ok();
    if anchor.resource != *resource
        || anchor.position != *name
        || !kind_matches
        || anchor.principal.asset != *asset
        || (anchor.position_kind.is_derivative()
            && (scaled_receipt.is_some() || anchor.scaled_receipt.is_some()))
        || anchor.evidence_version == 0
        || anchor.evidence.is_null()
        || anchor_time.is_none()
        || proof_time.is_none()
        || anchor_time > proof_time
        || scaled_receipt.is_some_and(|scaled| anchor.scaled_receipt.as_ref() != Some(scaled))
    {
        return Err(FinError::new(
            "POSITION_ANCHOR_INVALID",
            "reconciliation",
            "The post-state anchor is incomplete or does not identify the changed position and exact asset.",
            "Keep the reservation and obtain a versioned application proof for the exact post-state.",
        ));
    }
    Ok(())
}

/// Check that every proved position/debt post-anchor is the exact transition from
/// the last trusted anchor. Accrual changes the physical claim, but is not itself
/// a cash/provenance leg; the associated principal movement remains fully linked.
fn validate_anchor_transitions(
    verified: &VerifiedEffects,
    prior: &PreparationContext,
) -> Result<()> {
    #[derive(Default)]
    struct Transition {
        accruals: Vec<BigInt>,
        changes: Vec<BigInt>,
        anchor: Option<PositionAnchor>,
    }

    let mut prior_by_key = BTreeMap::<String, PositionAnchor>::new();
    for anchor in &prior.positions {
        let key = position_anchor_key(anchor)?;
        if prior_by_key.insert(key, anchor.clone()).is_some() {
            return Err(FinError::new(
                "AMBIGUOUS_POSITION_ANCHOR",
                "reconciliation",
                "More than one trusted prior anchor describes the same position.",
                "Preserve the reservation and reconcile against one verified position state.",
            ));
        }
    }

    let mut transitions = BTreeMap::<String, Transition>::new();
    for effect in &verified.effects {
        match effect {
            FinancialEffect::Accrual {
                resource,
                position,
                asset,
                position_kind,
                delta,
            } => {
                let key = transition_key(resource, position, position_kind, asset)?;
                transitions
                    .entry(key)
                    .or_default()
                    .accruals
                    .push(delta.integer());
            }
            FinancialEffect::PositionChange {
                resource,
                position,
                asset,
                delta,
                anchor,
                ..
            } => {
                let Some(anchor) = anchor else {
                    if !delta.is_zero() {
                        return Err(ambiguous_anchor());
                    }
                    continue;
                };
                if anchor.resource != *resource
                    || anchor.position != *position
                    || !(anchor.position_kind == PositionKind::Supply
                        || anchor.position_kind.is_derivative())
                    || anchor.principal.asset != *asset
                {
                    return Err(ambiguous_anchor());
                }
                let key = position_anchor_key(anchor)?;
                let transition = transitions.entry(key).or_default();
                transition.changes.push(delta.integer());
                if transition.anchor.replace(anchor.clone()).is_some() {
                    return Err(ambiguous_anchor());
                }
            }
            FinancialEffect::DebtChange {
                resource,
                debt,
                asset,
                delta,
                anchor,
            } => {
                let Some(anchor) = anchor else {
                    if !delta.is_zero() {
                        return Err(ambiguous_anchor());
                    }
                    continue;
                };
                if anchor.resource != *resource
                    || anchor.position != *debt
                    || !matches!(
                        anchor.position_kind,
                        PositionKind::VariableDebt | PositionKind::StableDebt
                    )
                    || anchor.principal.asset != *asset
                {
                    return Err(ambiguous_anchor());
                }
                let key = position_anchor_key(anchor)?;
                let transition = transitions.entry(key).or_default();
                transition.changes.push(delta.integer());
                if transition.anchor.replace(anchor.clone()).is_some() {
                    return Err(ambiguous_anchor());
                }
            }
            _ => {}
        }
    }

    for (key, transition) in transitions {
        if transition.accruals.len() > 1 || transition.changes.len() > 1 {
            return Err(ambiguous_anchor());
        }
        if !transition.accruals.is_empty() && transition.changes.is_empty() {
            return Err(FinError::new(
                "UNPAIRED_POSITION_ACCRUAL",
                "reconciliation",
                "A proved position accrual is not paired with a position or debt transition.",
                "Preserve the reservation until the complete backed transition is reconciled.",
            ));
        }
        let Some(anchor) = transition.anchor else {
            // Zero-delta observations that do not update an anchor carry no state
            // transition. Any non-zero or accrued change must have a post-anchor.
            if transition.accruals.iter().any(|delta| !delta.is_zero())
                || transition.changes.iter().any(|delta| !delta.is_zero())
            {
                return Err(ambiguous_anchor());
            }
            continue;
        };
        if anchor.evidence_version == 0 || anchor.evidence.is_null() {
            return Err(ambiguous_anchor());
        }
        let actual_key = position_anchor_key(&anchor)?;
        if actual_key != key {
            return Err(ambiguous_anchor());
        }
        let old = match prior_by_key.get(&key) {
            Some(previous) => {
                if previous.principal.asset != anchor.principal.asset {
                    return Err(ambiguous_anchor());
                }
                BigInt::parse_bytes(previous.principal.atoms.as_str().as_bytes(), 10)
                    .ok_or_else(ambiguous_anchor)?
            }
            None => {
                // Opening from a proved zero state is safe. A decrease or accrual
                // without its prior state cannot be reconstructed conservatively.
                if transition.accruals.iter().any(|delta| !delta.is_zero())
                    || transition
                        .changes
                        .iter()
                        .any(|delta| delta.sign() == Sign::Minus)
                {
                    return Err(FinError::new(
                        "PRIOR_POSITION_ANCHOR_REQUIRED",
                        "reconciliation",
                        "A decrease or accrual has no previously verified position anchor.",
                        "Keep the reservation and recover the original position baseline.",
                    ));
                }
                BigInt::from(0u8)
            }
        };
        let change = transition
            .accruals
            .into_iter()
            .chain(transition.changes)
            .fold(BigInt::from(0u8), |sum, delta| sum + delta);
        let expected = old + change;
        let actual = BigInt::parse_bytes(anchor.principal.atoms.as_str().as_bytes(), 10)
            .ok_or_else(ambiguous_anchor)?;
        if expected.sign() == Sign::Minus || expected != actual {
            return Err(FinError::new(
                "POSITION_ANCHOR_TRANSITION_MISMATCH",
                "reconciliation",
                "The proved post-anchor does not equal the prior balance plus its typed accrual and principal changes.",
                "Keep the reservation and reconcile the original position evidence.",
            ));
        }
    }
    Ok(())
}

fn transition_key(
    resource: &ResourceId,
    position: &str,
    kind: &PositionKind,
    asset: &fin_domain::AssetId,
) -> Result<String> {
    serde_json::to_string(&(resource, position, kind, asset))
        .map_err(|_| FinError::validation("Invalid position transition identity."))
}

fn route_projection_matches_receipt(
    proof: &ReturnRouteProof,
    verified: &VerifiedEffects,
) -> Result<bool> {
    for effect in &proof.effects {
        let matches = match effect {
            ProjectedEffect::CashDelta {
                resource,
                asset,
                minimum_delta,
                maximum_delta,
            } => {
                let actual = receipt_cash_delta(&verified.effects, resource, asset);
                minimum_delta.integer() <= actual && actual <= maximum_delta.integer()
            }
            ProjectedEffect::PositionDelta {
                resource,
                position,
                asset,
                position_kind,
                minimum_delta,
                maximum_delta,
            } => {
                let actual = receipt_position_delta(
                    &verified.effects,
                    resource,
                    position,
                    asset,
                    position_kind,
                );
                minimum_delta.integer() <= actual && actual <= maximum_delta.integer()
            }
            ProjectedEffect::AuthorityValue {
                resource,
                authority,
                value,
            } => verified.effects.iter().any(|actual| {
                matches!(actual,
                    FinancialEffect::AuthorityChange { resource: r, authority: a, after, .. }
                    if r == resource && a == authority && after == value)
            }),
            ProjectedEffect::CashSweep {
                resource,
                asset,
                maximum_amount,
                ..
            } => {
                receipt_principal_debit(&verified.effects, resource, asset)
                    <= maximum_amount.integer()
            }
            ProjectedEffect::PositionClose {
                resource,
                position,
                asset,
                position_kind,
                maximum_amount,
                ..
            } => {
                let delta = receipt_position_delta(
                    &verified.effects,
                    resource,
                    position,
                    asset,
                    position_kind,
                );
                let proved_closed = verified.effects.iter().any(|effect| match effect {
                    FinancialEffect::PositionChange {
                        anchor: Some(anchor),
                        ..
                    }
                    | FinancialEffect::DebtChange {
                        anchor: Some(anchor),
                        ..
                    } => {
                        anchor.resource == *resource
                            && anchor.position == *position
                            && anchor.position_kind == *position_kind
                            && anchor.principal.asset == *asset
                            && anchor.principal.atoms.is_zero()
                    }
                    _ => false,
                });
                proved_closed
                    && delta.sign() != Sign::Plus
                    && (-delta) <= BigInt::from(maximum_amount.integer())
            }
        };
        if !matches {
            return Ok(false);
        }
    }
    Ok(true)
}

fn receipt_cash_delta(
    effects: &[FinancialEffect],
    resource: &ResourceId,
    asset: &fin_domain::AssetId,
) -> BigInt {
    let mut delta = BigInt::zero();
    for effect in effects {
        match effect {
            FinancialEffect::Settlement {
                resource: r,
                asset: a,
                delta: value,
                ..
            } if r == resource && a == asset => {
                delta += value.integer();
            }
            FinancialEffect::CashDebit {
                resource: r,
                amount,
            } if r == resource && &amount.asset == asset => {
                delta -= BigInt::from(amount.atoms.integer());
            }
            FinancialEffect::Transfer {
                from,
                to,
                debit,
                credit,
            } => {
                if from == resource && &debit.asset == asset {
                    delta -= BigInt::from(debit.atoms.integer());
                }
                if to == resource && &credit.asset == asset {
                    delta += BigInt::from(credit.atoms.integer());
                }
            }
            FinancialEffect::Conversion {
                resource: r,
                spent,
                received,
            } if r == resource => {
                if &spent.asset == asset {
                    delta -= BigInt::from(spent.atoms.integer());
                }
                if &received.asset == asset {
                    delta += BigInt::from(received.atoms.integer());
                }
            }
            FinancialEffect::UsableCredit { to, amount }
                if to == resource && &amount.asset == asset =>
            {
                delta += BigInt::from(amount.atoms.integer());
            }
            FinancialEffect::Cost {
                location: AccountingLocation::Cash { resource: r },
                amount,
                treatment: CostTreatment::AdditionalDebit | CostTreatment::DeductedFromCredit,
                ..
            } if r == resource && &amount.asset == asset => {
                delta -= BigInt::from(amount.atoms.integer());
            }
            _ => {}
        }
    }
    delta
}

fn receipt_principal_debit(
    effects: &[FinancialEffect],
    resource: &ResourceId,
    asset: &fin_domain::AssetId,
) -> BigUint {
    let mut total = BigUint::zero();
    for effect in effects {
        match effect {
            FinancialEffect::CashDebit {
                resource: r,
                amount,
            } if r == resource && &amount.asset == asset => {
                total += amount.atoms.integer();
            }
            FinancialEffect::Transfer { from, debit, .. }
                if from == resource && &debit.asset == asset =>
            {
                total += debit.atoms.integer();
            }
            FinancialEffect::Conversion {
                resource: r, spent, ..
            } if r == resource && &spent.asset == asset => {
                total += spent.atoms.integer();
            }
            _ => {}
        }
    }
    total
}

fn receipt_position_delta(
    effects: &[FinancialEffect],
    resource: &ResourceId,
    position: &str,
    asset: &fin_domain::AssetId,
    kind: &PositionKind,
) -> BigInt {
    let mut total = BigInt::zero();
    for effect in effects {
        match effect {
            FinancialEffect::Accrual {
                resource: r,
                position: p,
                asset: a,
                position_kind,
                delta,
            } if r == resource && p == position && a == asset && position_kind == kind => {
                total += delta.integer();
            }
            FinancialEffect::PositionChange {
                resource: r,
                position: p,
                asset: a,
                delta,
                anchor,
                ..
            } if r == resource
                && p == position
                && a == asset
                && anchor
                    .as_ref()
                    .is_some_and(|anchor| &anchor.position_kind == kind) =>
            {
                total += delta.integer();
            }
            FinancialEffect::DebtChange {
                resource: r,
                debt,
                asset: a,
                delta,
                anchor,
            } if r == resource
                && debt == position
                && a == asset
                && anchor
                    .as_ref()
                    .is_some_and(|anchor| &anchor.position_kind == kind) =>
            {
                total += delta.integer();
            }
            _ => {}
        }
    }
    total
}

fn validate_route_plan(plan: &RunPlan, now_instant: DateTime<Utc>) -> Result<()> {
    if plan
        .asset_prices
        .iter()
        .any(fin_domain::unpriced::is_estimate)
    {
        return Err(FinError::new(
            "UNPRICED_ASSET",
            "budget",
            "A scoped acceptance run requires verified USD prices for every asset.",
            "Capture a fully priced allocation and return plan.",
        ));
    }
    validate_price_rows(
        &plan.asset_prices,
        &plan
            .asset_prices
            .iter()
            .map(|price| price.asset.clone())
            .collect(),
        now_instant,
    )?;
    let steps = plan
        .planned_operations
        .iter()
        .chain(plan.unwind.iter())
        .collect::<Vec<_>>();
    if steps.len() != plan.route_proofs.len() {
        return Err(route_proof_error());
    }
    let reserve = parse_uint(&plan.return_cost_reserve_usd_micros)?;
    let planned_cost = steps.iter().try_fold(BigUint::zero(), |total, step| {
        Ok::<_, FinError>(total + parse_uint(&step.maximum_fee_usd_micros)?)
    })?;
    if planned_cost > reserve {
        return Err(FinError::new(
            "RETURN_ROUTE_COST_BUDGET",
            "budget",
            "The complete route's approved cost ceilings exceed its funded return reserve.",
            "Keep every forward and return cost ceiling within the original selected reserve.",
        ));
    }
    let selected_cash = plan
        .allocations
        .iter()
        .map(|allocation| allocation.holding.clone())
        .collect::<Vec<_>>();
    let mut state = projected_state_with_cash(&plan.baseline, &selected_cash)?;
    for (index, (step, proof)) in steps.iter().zip(&plan.route_proofs).enumerate() {
        validate_route_proof(step, proof, index, now_instant).map_err(|error| {
            route_step_error(error, index, &step.operation, "stored proof validation")
        })?;
        validate_route_fee_bound(step, proof, &plan.asset_prices, now_instant).map_err(
            |error| route_step_error(error, index, &step.operation, "stored cost bound"),
        )?;
        validate_projected_cash_legs(&state, proof).map_err(|error| {
            route_step_error(
                error,
                index,
                &step.operation,
                "stored paired principal legs",
            )
        })?;
        validate_projected_requirements(
            &state,
            proof,
            &plan.route_proofs[..index],
            index,
            &selected_cash,
            0,
        )
        .map_err(|error| {
            route_step_error(
                error,
                index,
                &step.operation,
                "stored projected requirement",
            )
        })?;
        state = apply_projected_effects(&state, &proof.effects, index).map_err(|error| {
            route_step_error(error, index, &step.operation, "stored projected effects")
        })?;
    }
    projected_provenance::validate_projected_origins(
        &plan.baseline,
        &plan.allocations,
        &plan.route_proofs,
        &plan.asset_prices,
        &reserve,
    )
    .map_err(|error| {
        route_step_error(
            error,
            steps.len(),
            "return path",
            "projected source restoration",
        )
    })?;
    validate_projected_restoration(&plan.baseline, &plan.allocations, &state).map_err(|error| {
        route_step_error(
            error,
            steps.len(),
            "return path",
            "final restoration projection",
        )
    })?;
    Ok(())
}

/// Check the non-economic terminal invariants. Selected cash origin quantities
/// and funded economic shortfalls are independently replayed by projected_provenance.
fn validate_projected_restoration(
    baseline: &StateSnapshot,
    allocations: &[RunAllocation],
    projected: &ProjectedState,
) -> Result<()> {
    for position in &baseline.state.positions {
        let final_amount = projected
            .positions
            .iter()
            .find(|row| {
                row.resource == position.resource
                    && row.position == position.position
                    && row.asset == position.amount.asset
                    && row.position_kind == position.kind
            })
            .map(|row| &row.amount);
        if final_amount.is_none_or(|amount| {
            amount.minimum != position.amount.atoms || amount.maximum != position.amount.atoms
        }) {
            return Err(route_restoration_error("budget.restoration.position"));
        }
    }
    let mut position_keys = BTreeSet::new();
    for position in &projected.positions {
        if !position_keys.insert((
            position.resource.clone(),
            position.position.clone(),
            position.asset.clone(),
            position.position_kind.clone(),
        )) {
            return Err(route_restoration_error("budget.restoration.position"));
        }
        let baseline_amount = baseline
            .state
            .positions
            .iter()
            .find(|row| {
                row.resource == position.resource
                    && row.position == position.position
                    && row.amount.asset == position.asset
                    && row.kind == position.position_kind
            })
            .map(|row| row.amount.atoms.integer())
            .unwrap_or_default();
        if baseline_amount != position.amount.minimum.integer()
            || position.amount.minimum != position.amount.maximum
        {
            return Err(route_restoration_error("budget.restoration.position"));
        }
    }
    let mut baseline_authorities = baseline.state.authorities.clone();
    let mut projected_authorities = projected.authorities.clone();
    baseline_authorities
        .sort_by_key(|authority| (authority.resource.clone(), authority.authority.clone()));
    projected_authorities
        .sort_by_key(|authority| (authority.resource.clone(), authority.authority.clone()));
    if baseline_authorities != projected_authorities {
        return Err(route_restoration_error("budget.restoration.authority"));
    }
    let mut selected = BTreeMap::<String, BigUint>::new();
    let baseline_cash = aggregate_resource_amounts(&baseline.state.holdings)?;
    for allocation in allocations {
        let key = resource_asset_key(&allocation.holding)?;
        if selected
            .insert(key.clone(), allocation.holding.amount.atoms.integer())
            .is_some()
            || allocation.holding.amount.atoms.integer()
                > baseline_cash.get(&key).cloned().unwrap_or_default()
        {
            return Err(route_restoration_error("budget.restoration.allocation"));
        }
    }
    let mut cash_keys = BTreeSet::new();
    for cash in &projected.cash {
        let key = resource_asset_key(&ResourceAmount {
            resource: cash.resource.clone(),
            amount: Amount {
                asset: cash.asset.clone(),
                atoms: cash.amount.minimum.clone(),
            },
        })?;
        if !cash_keys.insert(key.clone()) || cash.amount.minimum > cash.amount.maximum {
            return Err(route_restoration_error("budget.restoration.cash_interval"));
        }
        // Only the originally selected custody/asset pairs may hold final run
        // cash. Baseline holdings outside the selection are added back unchanged
        // at final receipt reconciliation; they cannot absorb intermediate funds.
        if !selected.contains_key(&key) && !cash.amount.maximum.is_zero() {
            return Err(route_restoration_error(
                "budget.restoration.unallocated_asset",
            ));
        }
    }
    Ok(())
}

fn value_usd_micros_ceil(
    asset: &fin_domain::AssetId,
    atoms: &BigUint,
    prices: &[AssetPrice],
) -> Result<BigUint> {
    if atoms.is_zero() {
        return Ok(BigUint::zero());
    }
    let price = prices
        .iter()
        .find(|price| price.asset == *asset)
        .ok_or_else(route_proof_error)?;
    let unit_atoms = price.unit_atoms.integer();
    if unit_atoms.is_zero() || price.usd_micros_per_unit.is_zero() {
        return Err(route_proof_error());
    }
    let numerator = atoms * price.usd_micros_per_unit.integer();
    Ok((numerator + &unit_atoms - BigUint::from(1u8)) / unit_atoms)
}

fn route_restoration_error(stage: &'static str) -> FinError {
    FinError::new(
        "RETURN_ROUTE_INCOMPLETE",
        stage,
        "The preflighted route does not return cash, positions, debt and authorities to the captured baseline within its exact cost and accrual bounds.",
        "Revise the complete return path so no unallocated asset, position, debt or allowance remains.",
    )
}

fn validate_route_proof(
    step: &PlanStep,
    proof: &ReturnRouteProof,
    index: usize,
    now_instant: DateTime<Utc>,
) -> Result<()> {
    let observed = DateTime::parse_from_rfc3339(&proof.observed_at).ok();
    let expires = DateTime::parse_from_rfc3339(&proof.expires_at).ok();
    validate_route_proof_identity(step, proof, index)?;
    if step.operation.trim().is_empty()
        || step.description.trim().is_empty()
        || proof.evidence.is_null()
        || observed.is_none()
        || expires.is_none()
        || expires.is_some_and(|value| value.with_timezone(&Utc) <= now_instant)
        || expires.is_some_and(|value| {
            value
                .with_timezone(&Utc)
                .signed_duration_since(now_instant)
                .num_seconds()
                > MAX_OBSERVATION_AGE_SECONDS
        })
        || parse_uint(&step.maximum_fee_usd_micros).is_err()
        || proof.intent.expected.is_empty()
    {
        return Err(route_proof_error());
    }
    validate_observation(&proof.observed_at, now_instant, "RETURN_ROUTE_STALE")?;
    validate_cost_bound_contract(&proof.intent)?;
    Ok(())
}

fn validate_route_proof_identity(
    step: &PlanStep,
    proof: &ReturnRouteProof,
    index: usize,
) -> Result<()> {
    if proof.step_index as usize != index
        || proof.operation != step.operation
        || proof.input_hash != binding_hash(&step.input)?
        || proof.evidence.is_null()
        || proof.intent.expected.is_empty()
    {
        return Err(route_proof_error());
    }
    Ok(())
}

fn validate_route_fee_bound(
    step: &PlanStep,
    proof: &ReturnRouteProof,
    prices: &[AssetPrice],
    instant: DateTime<Utc>,
) -> Result<()> {
    let assets = proof
        .intent
        .maximum_costs
        .iter()
        .map(|bound| bound.amount.asset.clone())
        .chain(
            proof
                .intent
                .expected
                .iter()
                .filter_map(|expected| match expected {
                    EffectConstraint::Accrual {
                        asset,
                        position_kind: PositionKind::Supply,
                        maximum_change,
                        ..
                    } if !maximum_change.is_zero() => Some(asset.clone()),
                    _ => None,
                }),
        )
        .collect::<BTreeSet<_>>();
    validate_price_rows(prices, &assets, instant)?;
    let mut total = BigUint::zero();
    let mut per_asset = BTreeMap::<fin_domain::AssetId, BigUint>::new();
    for bound in &proof.intent.maximum_costs {
        *per_asset.entry(bound.amount.asset.clone()).or_default() += bound.amount.atoms.integer();
    }
    for (asset, atoms) in per_asset {
        let price = prices
            .iter()
            .find(|price| price.asset == asset)
            .ok_or_else(route_proof_error)?;
        let numerator = atoms * price.usd_micros_per_unit.integer();
        let unit_atoms = price.unit_atoms.integer();
        if unit_atoms.is_zero() {
            return Err(route_proof_error());
        }
        total += (numerator + &unit_atoms - BigUint::from(1u8)) / unit_atoms;
    }
    if total > parse_uint(&step.maximum_fee_usd_micros)? {
        return Err(FinError::new(
            "RETURN_ROUTE_COST_BUDGET",
            "budget",
            "The exact per-asset route cost bounds exceed this step's approved USD limit.",
            "Reduce the planned cost ceiling or increase the complete run reserve within policy.",
        ));
    }
    Ok(())
}

fn include_costs_in_projected_cash(proof: &mut ReturnRouteProof) -> Result<()> {
    let mut deltas = BTreeMap::<
        String,
        (
            AccountingLocation,
            fin_domain::AssetId,
            Option<usize>,
            BigInt,
            BigInt,
        ),
    >::new();
    for (index, effect) in proof.effects.iter().enumerate() {
        if let ProjectedEffect::CashDelta {
            resource,
            asset,
            minimum_delta,
            maximum_delta,
        } = effect
        {
            let location = AccountingLocation::Cash {
                resource: resource.clone(),
            };
            let key = accounting_key(&location, asset)?;
            let entry = deltas.entry(key).or_insert_with(|| {
                (
                    location,
                    asset.clone(),
                    Some(index),
                    BigInt::zero(),
                    BigInt::zero(),
                )
            });
            entry.3 += minimum_delta.integer();
            entry.4 += maximum_delta.integer();
        }
    }
    for cost in &proof.intent.maximum_costs {
        if !matches!(
            cost.treatment,
            CostTreatment::AdditionalDebit | CostTreatment::DeductedFromCredit
        ) {
            continue;
        }
        let AccountingLocation::Cash { .. } = cost.location else {
            return Err(route_proof_error());
        };
        let key = accounting_key(&cost.location, &cost.amount.asset)?;
        let entry = deltas.entry(key).or_insert_with(|| {
            (
                cost.location.clone(),
                cost.amount.asset.clone(),
                None,
                BigInt::zero(),
                BigInt::zero(),
            )
        });
        entry.3 -= BigInt::from(cost.amount.atoms.integer());
    }
    let mut effects = Vec::with_capacity(proof.effects.len() + deltas.len());
    let mut emitted = BTreeSet::new();
    for (index, effect) in proof.effects.iter().enumerate() {
        if let ProjectedEffect::CashDelta {
            resource, asset, ..
        } = effect
        {
            let location = AccountingLocation::Cash {
                resource: resource.clone(),
            };
            let key = accounting_key(&location, asset)?;
            if let Some((_, _, Some(first_index), minimum, maximum)) = deltas.get(&key)
                && emitted.insert(key)
                && index == *first_index
            {
                effects.push(ProjectedEffect::CashDelta {
                    resource: resource.clone(),
                    asset: asset.clone(),
                    minimum_delta: fin_domain::SignedQuantity::new(minimum.to_string())?,
                    maximum_delta: fin_domain::SignedQuantity::new(maximum.to_string())?,
                });
            }
            continue;
        }
        effects.push(effect.clone());
    }
    for (key, (location, asset, first_index, minimum, maximum)) in deltas {
        if first_index.is_some() {
            continue;
        }
        let AccountingLocation::Cash { resource } = location else {
            return Err(route_proof_error());
        };
        let _ = key;
        effects.push(ProjectedEffect::CashDelta {
            resource,
            asset,
            minimum_delta: fin_domain::SignedQuantity::new(minimum.to_string())?,
            maximum_delta: fin_domain::SignedQuantity::new(maximum.to_string())?,
        });
    }
    proof.effects = effects;
    Ok(())
}

/// Match every projected principal leg to exactly one typed in-step flow. This
/// prevents a detached cash movement from borrowing the topology of another
/// valid conversion path elsewhere in the route.
fn validate_projected_cash_legs(state: &ProjectedState, proof: &ReturnRouteProof) -> Result<()> {
    type CashKey = (ResourceId, fin_domain::AssetId);
    let mut cash = BTreeMap::<CashKey, (BigInt, BigInt)>::new();
    let mut sweeps = BTreeMap::<CashKey, &fin_domain::Quantity>::new();
    let mut positions = BTreeMap::<
        (ResourceId, String, fin_domain::AssetId, PositionKind),
        Vec<&ProjectedEffect>,
    >::new();
    for effect in &proof.effects {
        match effect {
            ProjectedEffect::CashDelta {
                resource,
                asset,
                minimum_delta,
                maximum_delta,
            } => {
                let key = (resource.clone(), asset.clone());
                if cash
                    .insert(key, (minimum_delta.integer(), maximum_delta.integer()))
                    .is_some()
                    || minimum_delta.integer() > maximum_delta.integer()
                {
                    return Err(route_proof_error());
                }
            }
            ProjectedEffect::PositionDelta {
                resource,
                position,
                asset,
                position_kind,
                ..
            }
            | ProjectedEffect::PositionClose {
                resource,
                position,
                asset,
                position_kind,
                ..
            } => positions
                .entry((
                    resource.clone(),
                    position.clone(),
                    asset.clone(),
                    position_kind.clone(),
                ))
                .or_default()
                .push(effect),
            ProjectedEffect::CashSweep {
                resource,
                asset,
                maximum_amount,
                ..
            } => {
                if sweeps
                    .insert((resource.clone(), asset.clone()), maximum_amount)
                    .is_some()
                {
                    return Err(route_proof_error());
                }
            }
            ProjectedEffect::AuthorityValue { .. } => {}
        }
    }

    let mut additional_costs = BTreeMap::<CashKey, BigUint>::new();
    for bound in &proof.intent.maximum_costs {
        if bound.treatment != CostTreatment::AdditionalDebit {
            continue;
        }
        let AccountingLocation::Cash { resource } = &bound.location else {
            return Err(route_proof_error());
        };
        *additional_costs
            .entry((resource.clone(), bound.amount.asset.clone()))
            .or_default() += bound.amount.atoms.integer();
    }
    for (key, cost) in &additional_costs {
        let funded = state
            .cash
            .iter()
            .find(|row| row.resource == key.0 && row.asset == key.1)
            .map(|row| row.amount.minimum.integer())
            .unwrap_or_default();
        if funded < *cost {
            return Err(route_proof_error());
        }
    }
    let mut credit_costs = additional_costs.clone();
    for bound in &proof.intent.maximum_costs {
        if bound.treatment == CostTreatment::DeductedFromCredit {
            let AccountingLocation::Cash { resource } = &bound.location else {
                return Err(route_proof_error());
            };
            *credit_costs
                .entry((resource.clone(), bound.amount.asset.clone()))
                .or_default() += bound.amount.atoms.integer();
        }
    }
    let cash_legs = ProjectedCashLegs {
        state,
        proof,
        cash: &cash,
        sweeps: &sweeps,
        additional_costs: &additional_costs,
    };

    let mut used_cash = BTreeSet::<CashKey>::new();
    let mut used_positions = BTreeSet::new();
    let mut origin_count = 0usize;
    for constraint in &proof.intent.expected {
        match constraint {
            EffectConstraint::Transfer {
                from,
                to,
                debit_asset,
                credit_asset,
                maximum_debit,
                minimum_credit,
            } => {
                origin_count += 1;
                if from == to && debit_asset == credit_asset {
                    return Err(route_proof_error());
                }
                let debit = (from.clone(), debit_asset.clone());
                let credit = (to.clone(), credit_asset.clone());
                validate_cash_debit(&cash_legs, &debit, maximum_debit, false, true)?;
                validate_cash_credit(state, &cash, &credit_costs, &credit, minimum_credit)?;
                if !used_cash.insert(debit) || !used_cash.insert(credit) {
                    return Err(route_proof_error());
                }
            }
            EffectConstraint::Conversion {
                resource,
                input,
                maximum_input,
                output,
                minimum_output,
            } => {
                origin_count += 1;
                if input == output {
                    return Err(route_proof_error());
                }
                let debit = (resource.clone(), input.clone());
                let credit = (resource.clone(), output.clone());
                validate_cash_debit(&cash_legs, &debit, maximum_input, true, false)?;
                validate_cash_credit(state, &cash, &credit_costs, &credit, minimum_output)?;
                if !used_cash.insert(debit) || !used_cash.insert(credit) {
                    return Err(route_proof_error());
                }
            }
            EffectConstraint::OriginMovement {
                from,
                to,
                debit_asset,
                credit_asset,
                maximum_debit,
                minimum_credit,
            } => {
                origin_count += 1;
                let debit_cash = cash_location_key(from, debit_asset);
                let credit_cash = cash_location_key(to, credit_asset);
                if let Some(key) = &debit_cash {
                    validate_cash_debit(&cash_legs, key, maximum_debit, false, false)?;
                    if !used_cash.insert(key.clone()) {
                        return Err(route_proof_error());
                    }
                }
                if let Some(key) = &credit_cash {
                    validate_cash_credit(state, &cash, &additional_costs, key, minimum_credit)?;
                    if !used_cash.insert(key.clone()) {
                        return Err(route_proof_error());
                    }
                }
                for location in [from, to] {
                    if let AccountingLocation::Position {
                        resource,
                        position,
                        position_kind,
                    } = location
                    {
                        let key = (
                            resource.clone(),
                            position.clone(),
                            if location == from {
                                debit_asset.clone()
                            } else {
                                credit_asset.clone()
                            },
                            position_kind.clone(),
                        );
                        validate_position_leg(
                            proof,
                            state,
                            &positions,
                            &key,
                            minimum_credit,
                            maximum_debit,
                            location == from,
                        )?;
                        if !used_positions.insert(key) {
                            return Err(route_proof_error());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let mut settlements = BTreeMap::<CashKey, (BigInt, BigInt)>::new();
    for constraint in &proof.intent.expected {
        match constraint {
            EffectConstraint::DerivativePosition {
                resource,
                position,
                asset,
                position_kind,
                minimum_delta,
                maximum_delta,
            } => {
                if !position_kind.is_derivative()
                    || minimum_delta.integer() > maximum_delta.integer()
                {
                    return Err(route_proof_error());
                }
                let key = (
                    resource.clone(),
                    position.clone(),
                    asset.clone(),
                    position_kind.clone(),
                );
                let legs = positions.get(&key).ok_or_else(route_proof_error)?;
                if legs.len() != 1 || !used_positions.insert(key) {
                    return Err(route_proof_error());
                }
                match legs[0] {
                    ProjectedEffect::PositionDelta {
                        minimum_delta: lo,
                        maximum_delta: hi,
                        ..
                    } if lo.integer() >= minimum_delta.integer()
                        && hi.integer() <= maximum_delta.integer() => {}
                    ProjectedEffect::PositionClose { maximum_amount, .. }
                        if minimum_delta.integer() <= -BigInt::from(maximum_amount.integer())
                            && !maximum_delta.is_negative() => {}
                    _ => return Err(route_proof_error()),
                }
            }
            EffectConstraint::Settlement {
                resource,
                asset,
                minimum_delta,
                maximum_delta,
                ..
            } => {
                if minimum_delta.integer() > maximum_delta.integer() {
                    return Err(route_proof_error());
                }
                let entry = settlements
                    .entry((resource.clone(), asset.clone()))
                    .or_default();
                entry.0 += minimum_delta.integer();
                entry.1 += maximum_delta.integer();
                if minimum_delta.is_negative() {
                    let cap = aggregate_resource_amounts(&proof.intent.debits)?
                        .get(&resource_asset_key(&ResourceAmount {
                            resource: resource.clone(),
                            amount: Amount {
                                asset: asset.clone(),
                                atoms: minimum_delta.magnitude(),
                            },
                        })?)
                        .cloned()
                        .unwrap_or_default();
                    if cap < minimum_delta.magnitude().integer() {
                        return Err(route_proof_error());
                    }
                }
            }
            _ => {}
        }
    }
    for (key, (minimum, maximum)) in settlements {
        let cost = additional_costs.get(&key).cloned().unwrap_or_default();
        let (lo, hi) = cash.get(&key).ok_or_else(route_proof_error)?;
        if *lo != minimum - BigInt::from(cost) || *hi != maximum || !used_cash.insert(key) {
            return Err(route_proof_error());
        }
    }

    // Debit/Credit often restate an Aave OriginMovement's principal bound. They
    // may refine that same flow, but cannot introduce a second detached leg.
    for constraint in &proof.intent.expected {
        match constraint {
            EffectConstraint::Debit {
                resource,
                asset,
                maximum_debit,
            } => {
                let key = (resource.clone(), asset.clone());
                let matched_origin =
                    proof
                        .intent
                        .expected
                        .iter()
                        .any(|candidate| match candidate {
                            EffectConstraint::OriginMovement {
                                from: AccountingLocation::Cash { resource: from },
                                debit_asset,
                                maximum_debit: origin_maximum,
                                ..
                            } => {
                                from == resource
                                    && debit_asset == asset
                                    && maximum_debit.integer() <= origin_maximum.integer()
                            }
                            _ => false,
                        });
                if !matched_origin || !used_cash.contains(&key) {
                    return Err(route_proof_error());
                }
                let (minimum, _) = cash.get(&key).ok_or_else(route_proof_error)?;
                if minimum.sign() != Sign::Minus {
                    return Err(route_proof_error());
                }
                let cost = additional_costs.get(&key).cloned().unwrap_or_default();
                let gross_principal = minimum.magnitude().clone();
                if gross_principal < cost || gross_principal - cost > maximum_debit.integer() {
                    return Err(route_proof_error());
                }
            }
            EffectConstraint::Credit {
                to,
                asset,
                minimum_credit,
            } => {
                let key = (to.clone(), asset.clone());
                let matched_origin =
                    proof
                        .intent
                        .expected
                        .iter()
                        .any(|candidate| match candidate {
                            EffectConstraint::OriginMovement {
                                to: AccountingLocation::Cash { resource },
                                credit_asset,
                                ..
                            } => resource == to && credit_asset == asset,
                            _ => false,
                        });
                if !matched_origin || !used_cash.contains(&key) {
                    return Err(route_proof_error());
                }
                validate_cash_credit(state, &cash, &additional_costs, &key, minimum_credit)?;
            }
            _ => {}
        }
    }

    if origin_count > 1 {
        return Err(route_proof_error());
    }
    for (key, cost) in &additional_costs {
        if used_cash.contains(key) {
            continue;
        }
        let (minimum, maximum) = cash.get(key).ok_or_else(route_proof_error)?;
        let available = state
            .cash
            .iter()
            .find(|row| row.resource == key.0 && row.asset == key.1)
            .map(|row| row.amount.minimum.integer())
            .unwrap_or_default();
        if minimum != &-BigInt::from(cost.clone()) || !maximum.is_zero() || available < *cost {
            return Err(route_proof_error());
        }
        used_cash.insert(key.clone());
    }
    if cash.keys().any(|key| !used_cash.contains(key))
        || positions.keys().any(|key| !used_positions.contains(key))
        || sweeps.keys().any(|key| !used_cash.contains(key))
    {
        return Err(route_proof_error());
    }
    Ok(())
}

fn cash_location_key(
    location: &AccountingLocation,
    asset: &fin_domain::AssetId,
) -> Option<(ResourceId, fin_domain::AssetId)> {
    match location {
        AccountingLocation::Cash { resource } => Some((resource.clone(), asset.clone())),
        AccountingLocation::Position { .. } => None,
    }
}

struct ProjectedCashLegs<'a> {
    state: &'a ProjectedState,
    proof: &'a ReturnRouteProof,
    cash: &'a BTreeMap<(ResourceId, fin_domain::AssetId), (BigInt, BigInt)>,
    sweeps: &'a BTreeMap<(ResourceId, fin_domain::AssetId), &'a fin_domain::Quantity>,
    additional_costs: &'a BTreeMap<(ResourceId, fin_domain::AssetId), BigUint>,
}

fn validate_cash_debit(
    legs: &ProjectedCashLegs<'_>,
    key: &(ResourceId, fin_domain::AssetId),
    maximum_debit: &fin_domain::Quantity,
    allow_sweep: bool,
    require_exact: bool,
) -> Result<()> {
    let cost = legs.additional_costs.get(key).cloned().unwrap_or_default();
    let available = legs
        .state
        .cash
        .iter()
        .find(|row| row.resource == key.0 && row.asset == key.1)
        .ok_or_else(route_proof_error)?;
    if let Some(sweep_cap) = legs.sweeps.get(key) {
        let projected_principal_max = available.amount.maximum.integer();
        if !allow_sweep
            || legs.cash.contains_key(key)
            || !cost.is_zero()
            || projected_principal_max > sweep_cap.integer()
            || sweep_cap.integer() > maximum_debit.integer()
        {
            return Err(route_proof_error());
        }
        if !legs.proof.intent.debits.iter().any(|debit| {
            debit.resource == key.0
                && debit.amount.asset == key.1
                && debit.amount.atoms.integer() >= projected_principal_max
        }) {
            return Err(route_proof_error());
        }
        if require_exact && projected_principal_max != maximum_debit.integer() {
            return Err(route_proof_error());
        }
        return Ok(());
    }
    let (minimum, maximum) = legs.cash.get(key).ok_or_else(route_proof_error)?;
    if minimum.sign() != Sign::Minus || maximum.sign() == Sign::Plus {
        return Err(route_proof_error());
    }
    let projected = minimum.magnitude().clone();
    if projected < cost {
        return Err(route_proof_error());
    }
    let projected = projected - &cost;
    if projected.is_zero() || projected > maximum_debit.integer() {
        return Err(route_proof_error());
    }
    if require_exact && projected != maximum_debit.integer() {
        return Err(route_proof_error());
    }
    let intent_debit = legs.proof.intent.debits.iter().any(|debit| {
        debit.resource == key.0
            && debit.amount.asset == key.1
            && debit.amount.atoms.integer() >= projected
    });
    if !intent_debit {
        return Err(route_proof_error());
    }
    if available.amount.minimum.integer() < projected + &cost {
        return Err(route_proof_error());
    }
    Ok(())
}

fn validate_cash_credit(
    _state: &ProjectedState,
    cash: &BTreeMap<(ResourceId, fin_domain::AssetId), (BigInt, BigInt)>,
    additional_costs: &BTreeMap<(ResourceId, fin_domain::AssetId), BigUint>,
    key: &(ResourceId, fin_domain::AssetId),
    minimum_credit: &fin_domain::Quantity,
) -> Result<()> {
    let (minimum, maximum) = cash.get(key).ok_or_else(route_proof_error)?;
    let cost = additional_costs.get(key).cloned().unwrap_or_default();
    let gross_minimum = minimum + BigInt::from(cost.clone());
    let gross_maximum = maximum.clone();
    let optional = minimum_credit.is_zero();
    if (gross_minimum.sign() != Sign::Plus && !(optional && gross_minimum.is_zero()))
        || (gross_maximum.sign() != Sign::Plus && !(optional && gross_maximum.is_zero()))
        || gross_minimum > gross_maximum
        || gross_minimum.magnitude() < &minimum_credit.integer()
    {
        return Err(route_proof_error());
    }
    Ok(())
}

fn projected_position_minimum(effect: &ProjectedEffect) -> Option<BigUint> {
    match effect {
        ProjectedEffect::PositionDelta { minimum_delta, .. } if !minimum_delta.is_negative() => {
            Some(minimum_delta.magnitude().integer())
        }
        _ => None,
    }
}

fn validate_position_leg(
    proof: &ReturnRouteProof,
    state: &ProjectedState,
    positions: &BTreeMap<
        (ResourceId, String, fin_domain::AssetId, PositionKind),
        Vec<&ProjectedEffect>,
    >,
    key: &(ResourceId, String, fin_domain::AssetId, PositionKind),
    minimum_credit: &fin_domain::Quantity,
    maximum_debit: &fin_domain::Quantity,
    source_endpoint: bool,
) -> Result<()> {
    let legs = positions.get(key).ok_or_else(route_proof_error)?;
    if legs.len() != 1 {
        return Err(route_proof_error());
    }
    let expected_direction = match (&key.3, source_endpoint) {
        (PositionKind::Supply, true) => Direction::Decrease,
        (PositionKind::Supply, false) => Direction::Increase,
        (PositionKind::StableDebt | PositionKind::VariableDebt, true) => Direction::Increase,
        (PositionKind::StableDebt | PositionKind::VariableDebt, false) => Direction::Decrease,
        (PositionKind::DerivativeLong | PositionKind::DerivativeShort, _) => {
            return Err(route_proof_error());
        }
    };
    let has_typed_position_bound = proof.intent.expected.iter().any(|expected| match expected {
        EffectConstraint::Position {
            resource,
            position,
            asset,
            direction,
            maximum_change,
        } => {
            resource == &key.0
                && position == &key.1
                && asset == &key.2
                && key.3 == PositionKind::Supply
                && *direction == expected_direction
                && match (direction, legs[0]) {
                    (Direction::Increase, ProjectedEffect::PositionDelta { maximum_delta, .. }) => {
                        !maximum_delta.is_negative()
                            && maximum_delta.magnitude().integer() <= maximum_change.integer()
                            && maximum_delta.magnitude().integer() <= maximum_debit.integer()
                            && projected_position_minimum(legs[0])
                                .is_some_and(|minimum| minimum >= minimum_credit.integer())
                    }
                    (
                        Direction::Decrease,
                        ProjectedEffect::PositionClose { maximum_amount, .. },
                    ) => {
                        maximum_amount.integer() <= maximum_change.integer()
                            && maximum_amount.integer() <= maximum_debit.integer()
                            && maximum_amount.integer() >= minimum_credit.integer()
                    }
                    _ => false,
                }
        }
        EffectConstraint::Debt {
            resource,
            debt,
            asset,
            direction,
            maximum_change,
        } => {
            resource == &key.0
                && debt == &key.1
                && asset == &key.2
                && matches!(key.3, PositionKind::StableDebt | PositionKind::VariableDebt)
                && *direction == expected_direction
                && match (direction, legs[0]) {
                    (Direction::Increase, ProjectedEffect::PositionDelta { maximum_delta, .. }) => {
                        !maximum_delta.is_negative()
                            && maximum_delta.magnitude().integer() <= maximum_change.integer()
                            && maximum_delta.magnitude().integer() <= maximum_debit.integer()
                            && projected_position_minimum(legs[0])
                                .is_some_and(|minimum| minimum >= minimum_credit.integer())
                    }
                    (
                        Direction::Decrease,
                        ProjectedEffect::PositionClose { maximum_amount, .. },
                    ) => {
                        maximum_amount.integer() <= maximum_change.integer()
                            && maximum_amount.integer() <= maximum_debit.integer()
                            && maximum_amount.integer() >= minimum_credit.integer()
                    }
                    _ => false,
                }
        }
        _ => false,
    });
    if !has_typed_position_bound {
        return Err(route_proof_error());
    }
    if let Some(existing) = state.positions.iter().find(|row| {
        row.resource == key.0
            && row.position == key.1
            && row.asset == key.2
            && row.position_kind == key.3
    }) {
        if let ProjectedEffect::PositionClose { maximum_amount, .. } = legs[0]
            && (existing.amount.minimum.integer() < minimum_credit.integer()
                || existing.amount.maximum.integer() > maximum_amount.integer())
        {
            return Err(route_proof_error());
        }
    } else if matches!(legs[0], ProjectedEffect::PositionClose { .. }) {
        return Err(route_proof_error());
    }
    Ok(())
}

fn projected_state(snapshot: &StateSnapshot) -> Result<ProjectedState> {
    let mut state = ProjectedState::default();
    for holding in &snapshot.state.holdings {
        state.cash.push(ProjectedCash {
            resource: holding.resource.clone(),
            asset: holding.amount.asset.clone(),
            amount: QuantityInterval {
                minimum: holding.amount.atoms.clone(),
                maximum: holding.amount.atoms.clone(),
            },
        });
    }
    for position in &snapshot.state.positions {
        state.positions.push(ProjectedPosition {
            resource: position.resource.clone(),
            position: position.position.clone(),
            asset: position.amount.asset.clone(),
            position_kind: position.kind.clone(),
            amount: QuantityInterval {
                minimum: position.amount.atoms.clone(),
                maximum: position.amount.atoms.clone(),
            },
        });
    }
    state.authorities = snapshot.state.authorities.clone();
    Ok(state)
}

fn projected_state_with_cash(
    snapshot: &StateSnapshot,
    selected_cash: &[ResourceAmount],
) -> Result<ProjectedState> {
    let mut state = projected_state(snapshot)?;
    state.cash = selected_cash
        .iter()
        .map(|holding| ProjectedCash {
            resource: holding.resource.clone(),
            asset: holding.amount.asset.clone(),
            amount: QuantityInterval {
                minimum: holding.amount.atoms.clone(),
                maximum: holding.amount.atoms.clone(),
            },
        })
        .collect();
    Ok(state)
}

fn run_physical_resources(run: &RunLedger) -> BTreeSet<ResourceId> {
    let mut resources = run
        .baseline
        .state
        .holdings
        .iter()
        .map(|row| row.resource.clone())
        .chain(
            run.baseline
                .state
                .positions
                .iter()
                .map(|row| row.resource.clone()),
        )
        .chain(
            run.baseline
                .state
                .authorities
                .iter()
                .map(|row| row.resource.clone()),
        )
        .chain(
            run.allocations
                .iter()
                .map(|row| row.holding.resource.clone()),
        )
        .collect::<BTreeSet<_>>();
    for proof in &run.route_proofs {
        resources.extend(proof.intent.locks.iter().cloned());
        resources.extend(proof.intent.debits.iter().map(|row| row.resource.clone()));
        for effect in &proof.effects {
            match effect {
                ProjectedEffect::CashDelta { resource, .. }
                | ProjectedEffect::CashSweep { resource, .. }
                | ProjectedEffect::AuthorityValue { resource, .. }
                | ProjectedEffect::PositionDelta { resource, .. }
                | ProjectedEffect::PositionClose { resource, .. } => {
                    resources.insert(resource.clone());
                }
            }
        }
        for requirement in &proof.requires {
            match requirement {
                ProjectedRequirement::CashAtLeast { resource, .. }
                | ProjectedRequirement::PositionAtLeast { resource, .. } => {
                    resources.insert(resource.clone());
                }
            }
        }
    }
    resources
}

fn compare_position_claims(
    before: &[fin_domain::PositionState],
    after: &[fin_domain::PositionState],
    differences: &mut BTreeMap<String, String>,
) {
    let signature = |position: &fin_domain::PositionState| {
        serde_json::to_string(&(
            &position.resource,
            &position.position,
            &position.kind,
            &position.amount,
            &position.scaled_receipt,
        ))
        .unwrap_or_default()
    };
    let before_identities = before
        .iter()
        .map(|position| {
            serde_json::to_string(&(
                &position.resource,
                &position.position,
                &position.kind,
                &position.amount.asset,
            ))
            .unwrap_or_default()
        })
        .collect::<BTreeSet<_>>();
    let after_identities = after
        .iter()
        .map(|position| {
            serde_json::to_string(&(
                &position.resource,
                &position.position,
                &position.kind,
                &position.amount.asset,
            ))
            .unwrap_or_default()
        })
        .collect::<BTreeSet<_>>();
    if before_identities.len() != before.len() || after_identities.len() != after.len() {
        differences.insert(
            "position:duplicate_claim".into(),
            "position claims must have unique resource, position, kind, and asset identities"
                .into(),
        );
    }
    let before = before.iter().map(signature).collect::<BTreeSet<_>>();
    let after = after.iter().map(signature).collect::<BTreeSet<_>>();
    for item in before.difference(&after) {
        differences.insert(
            format!("position:{item}"),
            "financial position or exact scaled claim is missing from final state".into(),
        );
    }
    for item in after.difference(&before) {
        differences.insert(
            format!("position:{item}"),
            "additional financial position or scaled claim is present at finish".into(),
        );
    }
}

fn allocation_origin_key(origin: &str) -> Option<&str> {
    let mut root = origin;
    while let Some(rest) = root.strip_prefix("accrual:") {
        root = rest;
    }
    root.strip_prefix("allocation:")
}

type RestoredCash = (
    BTreeMap<String, BigUint>,
    Vec<RunConversionDelta>,
    Vec<String>,
);

fn expected_restored_cash(run: &RunLedger) -> Result<RestoredCash> {
    let mut expected = aggregate_amounts(&run.baseline.state.holdings)?;
    let mut selected = BTreeMap::<String, BigUint>::new();
    let mut allocation_by_key = BTreeMap::<String, ResourceAmount>::new();
    let mut principal_returned = BTreeMap::<String, BigUint>::new();
    for allocation in &run.allocations {
        let key = resource_asset_key(&allocation.holding)?;
        let amount = allocation.holding.amount.atoms.integer();
        *selected.entry(key.clone()).or_default() += &amount;
        allocation_by_key.insert(key.clone(), allocation.holding.clone());
        principal_returned.entry(key).or_default();
    }
    let mut issues = Vec::new();
    for (key, amount) in &selected {
        let baseline = expected.get(key).cloned().unwrap_or_default();
        if amount > &baseline {
            issues.push(format!(
                "selected allocation exceeds its captured baseline for {key}"
            ));
            expected.insert(key.clone(), BigUint::zero());
        } else {
            expected.insert(key.clone(), baseline - amount);
        }
    }
    for lot in &run.provenance {
        let amount = parse_uint(&lot.atoms)?;
        if amount.is_zero() {
            continue;
        }
        let Some(origin_key) = allocation_origin_key(&lot.origin) else {
            issues.push(format!(
                "unreturned non-allocation origin {} remains",
                lot.origin
            ));
            continue;
        };
        if !selected.contains_key(origin_key) {
            issues.push(format!(
                "origin {} does not belong to a selected run allocation",
                lot.origin
            ));
            continue;
        }
        let (origin_resource, origin_asset) = decode_resource_asset_key(origin_key)?;
        let AccountingLocation::Cash { resource } = &lot.location else {
            issues.push(format!(
                "allocation origin {} remains outside cash",
                lot.origin
            ));
            continue;
        };
        if resource != &origin_resource || lot.asset != origin_asset {
            issues.push(format!(
                "allocation origin {} has not returned to its original resource and asset",
                lot.origin
            ));
            continue;
        }
        let key = resource_asset_key(&ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: lot.asset.clone(),
                atoms: fin_domain::Quantity::zero(),
            },
        })?;
        *expected.entry(key.clone()).or_default() += &amount;
        if !lot.origin.starts_with("accrual:") {
            *principal_returned.entry(origin_key.to_owned()).or_default() += amount;
        }
    }
    let cost_map = aggregate_amounts(&run.costs)?;
    for key in cost_map.keys() {
        if !selected.contains_key(key) {
            issues.push(format!(
                "verified cost origin {key} is outside the selected run allocations"
            ));
        }
    }
    let mut conversion_deltas = Vec::new();
    for (key, allocation) in allocation_by_key {
        let selected_amount = selected.get(&key).cloned().unwrap_or_default();
        let returned = principal_returned.get(&key).cloned().unwrap_or_default();
        let actual_cost = cost_map.get(&key).cloned().unwrap_or_default();
        let difference =
            BigInt::from(returned) + BigInt::from(actual_cost) - BigInt::from(selected_amount);
        conversion_deltas.push(RunConversionDelta {
            resource: allocation.resource,
            asset: allocation.amount.asset,
            atoms: fin_domain::SignedQuantity::new(difference.to_string())?,
        });
    }
    Ok((expected, conversion_deltas, issues))
}

fn run_cost_usd_micros(costs: &[ResourceAmount], prices: &[AssetPrice]) -> Result<BigUint> {
    let mut by_asset = BTreeMap::<fin_domain::AssetId, BigUint>::new();
    for cost in costs {
        *by_asset.entry(cost.amount.asset.clone()).or_default() += cost.amount.atoms.integer();
    }
    let mut total = BigUint::zero();
    for (asset, atoms) in by_asset {
        let price = prices.iter().find(|price| price.asset == asset).ok_or_else(|| {
            FinError::new(
                "COST_VALUATION_MISSING",
                "reconciliation",
                "A proved run cost has no trusted exact asset price.",
                "Keep cleanup outstanding until the cost can be valued from the captured run prices.",
            )
        })?;
        let denominator = price.unit_atoms.integer();
        if denominator.is_zero() || price.usd_micros_per_unit.is_zero() {
            return Err(FinError::validation("Invalid trusted run cost price."));
        }
        total += ceil_div(&(atoms * price.usd_micros_per_unit.integer()), &denominator);
    }
    Ok(total)
}

fn validate_projected_requirements(
    state: &ProjectedState,
    proof: &ReturnRouteProof,
    prior: &[ReturnRouteProof],
    index: usize,
    selected_starting_cash: &[ResourceAmount],
    cash_chain_start: usize,
) -> Result<()> {
    for effect in &proof.effects {
        let source_step = match effect {
            ProjectedEffect::CashSweep { source_step, .. }
            | ProjectedEffect::PositionClose { source_step, .. } => *source_step,
            _ => continue,
        };
        let source_index = usize::try_from(source_step).map_err(|_| route_proof_error())?;
        let source_chain = prior.get(..=source_index).ok_or_else(route_proof_error)?;
        let projected_upper = match effect {
            ProjectedEffect::CashSweep {
                resource, asset, ..
            } => state
                .cash
                .iter()
                .find(|row| row.resource == *resource && row.asset == *asset)
                .map(|row| row.amount.maximum.integer())
                .unwrap_or_default(),
            ProjectedEffect::PositionClose {
                resource,
                position,
                asset,
                position_kind,
                ..
            } => state
                .positions
                .iter()
                .find(|row| {
                    row.resource == *resource
                        && row.position == *position
                        && row.asset == *asset
                        && row.position_kind == *position_kind
                })
                .map(|row| row.amount.maximum.integer())
                .unwrap_or_default(),
            _ => BigUint::zero(),
        };
        if source_index >= index || !selector_source_matches(effect, source_chain, &projected_upper)
        {
            return Err(route_proof_error());
        }
    }
    for (requirement_index, requirement) in proof.requires.iter().enumerate() {
        let (available, requested, source_step) = match requirement {
            ProjectedRequirement::CashAtLeast {
                resource,
                asset,
                amount,
                source_step,
            } => (
                state
                    .cash
                    .iter()
                    .find(|row| row.resource == *resource && row.asset == *asset)
                    .map(|row| row.amount.minimum.integer())
                    .unwrap_or_default(),
                amount.integer(),
                *source_step,
            ),
            ProjectedRequirement::PositionAtLeast {
                resource,
                position,
                asset,
                position_kind,
                amount,
                source_step,
            } => (
                state
                    .positions
                    .iter()
                    .find(|row| {
                        row.resource == *resource
                            && row.position == *position
                            && row.asset == *asset
                            && row.position_kind == *position_kind
                    })
                    .map(|row| row.amount.minimum.integer())
                    .unwrap_or_default(),
                amount.integer(),
                *source_step,
            ),
        };
        if available < requested {
            return Err(FinError::new(
                "PROJECTED_REQUIREMENT_UNFUNDED",
                "budget",
                format!(
                    "Projected requirement {requirement_index} is not funded by selected cash or positions."
                ),
                "Revise the route so every prefix is funded by selected holdings or proved prior credits.",
            ));
        }
        if let Some(source_step) = source_step {
            let source_index = usize::try_from(source_step).map_err(|_| route_proof_error())?;
            if source_index >= index {
                return Err(FinError::new(
                    "PROJECTED_REQUIREMENT_SOURCE_INVALID",
                    "budget",
                    format!(
                        "Projected requirement {requirement_index} has no matching prior source proof."
                    ),
                    "Bind each projected credit requirement to an earlier proof that guarantees that exact source.",
                ));
            }
            let valid_source = match requirement {
                ProjectedRequirement::CashAtLeast {
                    resource,
                    asset,
                    amount,
                    ..
                } => cash_requirement_source_matches(
                    resource,
                    asset,
                    amount.integer(),
                    source_index,
                    prior,
                    selected_starting_cash,
                    cash_chain_start,
                )?,
                ProjectedRequirement::PositionAtLeast { .. } => prior
                    .get(source_index)
                    .is_some_and(|source| requirement_source_matches(requirement, source)),
            };
            if !valid_source {
                return Err(FinError::new(
                    "PROJECTED_REQUIREMENT_SOURCE_INVALID",
                    "budget",
                    format!(
                        "Projected requirement {requirement_index} has no matching selected or prior source proof."
                    ),
                    "Bind the requirement to selected starting cash and sufficient matching earlier credits after prior debits.",
                ));
            }
        }
    }
    Ok(())
}

fn selector_source_matches(
    effect: &ProjectedEffect,
    source_chain: &[ReturnRouteProof],
    projected_upper: &BigUint,
) -> bool {
    let mut source_maximum = BigUint::zero();
    let last_index = source_chain.len().saturating_sub(1);
    let mut has_latest_source = false;
    for (source_index, source) in source_chain.iter().enumerate() {
        for candidate in &source.effects {
            let matching = match (effect, candidate) {
                (
                    ProjectedEffect::CashSweep {
                        resource, asset, ..
                    },
                    ProjectedEffect::CashDelta {
                        resource: actual_resource,
                        asset: actual_asset,
                        minimum_delta,
                        maximum_delta,
                    },
                ) => {
                    actual_resource == resource
                        && actual_asset == asset
                        && !minimum_delta.is_negative()
                        && !maximum_delta.is_negative()
                        && maximum_delta.magnitude().integer() > BigUint::zero()
                }
                (
                    ProjectedEffect::PositionClose {
                        resource,
                        position,
                        asset,
                        position_kind,
                        ..
                    },
                    ProjectedEffect::PositionDelta {
                        resource: actual_resource,
                        position: actual_position,
                        asset: actual_asset,
                        position_kind: actual_kind,
                        minimum_delta,
                        maximum_delta,
                    },
                ) => {
                    actual_resource == resource
                        && actual_position == position
                        && actual_asset == asset
                        && actual_kind == position_kind
                        && !minimum_delta.is_negative()
                        && !maximum_delta.is_negative()
                        && maximum_delta.magnitude().integer() > BigUint::zero()
                }
                _ => false,
            };
            if matching {
                let amount = match candidate {
                    ProjectedEffect::CashDelta { maximum_delta, .. }
                    | ProjectedEffect::PositionDelta { maximum_delta, .. } => {
                        maximum_delta.magnitude().integer()
                    }
                    _ => BigUint::zero(),
                };
                source_maximum += amount;
                if source_index == last_index {
                    has_latest_source = true;
                }
            }
        }
    }
    if !has_latest_source {
        return false;
    }
    source_maximum >= *projected_upper
}

fn requirement_source_matches(
    requirement: &ProjectedRequirement,
    source: &ReturnRouteProof,
) -> bool {
    match requirement {
        ProjectedRequirement::CashAtLeast {
            resource,
            asset,
            amount,
            ..
        } => source.effects.iter().any(|effect| {
            matches!(effect, ProjectedEffect::CashDelta { resource: actual_resource, asset: actual_asset, minimum_delta, maximum_delta }
                if actual_resource == resource && actual_asset == asset
                    && !minimum_delta.is_negative()
                    && maximum_delta.magnitude() >= *amount)
        }),
        ProjectedRequirement::PositionAtLeast {
            resource,
            position,
            asset,
            position_kind,
            amount,
            ..
        } => source.effects.iter().any(|effect| {
            matches!(effect, ProjectedEffect::PositionDelta { resource: actual_resource, position: actual_position, asset: actual_asset, position_kind: actual_kind, minimum_delta, maximum_delta }
                if actual_resource == resource && actual_position == position
                    && actual_asset == asset && actual_kind == position_kind
                    && !minimum_delta.is_negative()
                    && maximum_delta.magnitude() >= *amount)
        }),
    }
}

fn cash_requirement_source_matches(
    resource: &ResourceId,
    asset: &fin_domain::AssetId,
    requested: BigUint,
    source_index: usize,
    prior: &[ReturnRouteProof],
    selected_starting_cash: &[ResourceAmount],
    cash_chain_start: usize,
) -> Result<bool> {
    if source_index >= prior.len() {
        return Ok(false);
    }
    let source_has_credit = prior[source_index].effects.iter().any(|effect| {
        matches!(effect,
            ProjectedEffect::CashDelta {
                resource: actual_resource,
                asset: actual_asset,
                maximum_delta,
                ..
            } if actual_resource == resource
                && actual_asset == asset
                && maximum_delta.integer().sign() == Sign::Plus
        )
    });
    if !source_has_credit {
        return Ok(false);
    }

    // Replanning begins from the receipt-backed current `run.available` cash.
    // Earlier source proofs can still identify the origin of that cash, but
    // their deltas are already reflected in the starting balance and must not
    // be replayed into it a second time.
    if source_index < cash_chain_start {
        let selected_current = selected_starting_cash
            .iter()
            .filter(|holding| holding.resource == *resource && holding.amount.asset == *asset)
            .fold(BigUint::zero(), |sum, holding| {
                sum + holding.amount.atoms.integer()
            });
        return Ok(selected_current >= requested);
    }

    let mut guaranteed = selected_starting_cash
        .iter()
        .filter(|holding| holding.resource == *resource && holding.amount.asset == *asset)
        .fold(BigInt::zero(), |sum, holding| {
            sum + BigInt::from(holding.amount.atoms.integer())
        });
    for route_proof in &prior[cash_chain_start..=source_index] {
        let mut delta = BigInt::zero();
        let mut swept = false;
        for effect in &route_proof.effects {
            match effect {
                ProjectedEffect::CashDelta {
                    resource: actual_resource,
                    asset: actual_asset,
                    minimum_delta,
                    ..
                } if actual_resource == resource && actual_asset == asset => {
                    delta += minimum_delta.integer();
                }
                ProjectedEffect::CashSweep {
                    resource: actual_resource,
                    asset: actual_asset,
                    ..
                } if actual_resource == resource && actual_asset == asset => swept = true,
                _ => {}
            }
        }
        guaranteed += delta;
        if swept {
            guaranteed = BigInt::zero();
        }
        if guaranteed.sign() == Sign::Minus {
            return Ok(false);
        }
    }
    Ok(guaranteed >= BigInt::from(requested))
}

fn apply_projected_effects(
    old: &ProjectedState,
    effects: &[ProjectedEffect],
    step_index: usize,
) -> Result<ProjectedState> {
    let mut state = old.clone();
    let mut closures = Vec::new();
    for effect in effects {
        match effect {
            ProjectedEffect::CashDelta {
                resource,
                asset,
                minimum_delta,
                maximum_delta,
            } => {
                let old = state
                    .cash
                    .iter()
                    .find(|row| row.resource == *resource && row.asset == *asset)
                    .map(|row| row.amount.clone())
                    .unwrap_or(QuantityInterval {
                        minimum: fin_domain::Quantity::new("0")?,
                        maximum: fin_domain::Quantity::new("0")?,
                    });
                let min = BigInt::from(old.minimum.integer()) + minimum_delta.integer();
                let max = BigInt::from(old.maximum.integer()) + maximum_delta.integer();
                if min.sign() == Sign::Minus || max < min {
                    return Err(route_proof_error());
                }
                state
                    .cash
                    .retain(|row| row.resource != *resource || row.asset != *asset);
                state.cash.push(ProjectedCash {
                    resource: resource.clone(),
                    asset: asset.clone(),
                    amount: QuantityInterval {
                        minimum: fin_domain::Quantity::new(min.to_string())?,
                        maximum: fin_domain::Quantity::new(max.to_string())?,
                    },
                });
            }
            ProjectedEffect::PositionDelta {
                resource,
                position,
                asset,
                position_kind,
                minimum_delta,
                maximum_delta,
            } => {
                let old = state
                    .positions
                    .iter()
                    .find(|row| {
                        row.resource == *resource
                            && row.position == *position
                            && row.asset == *asset
                            && row.position_kind == *position_kind
                    })
                    .map(|row| row.amount.clone())
                    .unwrap_or(QuantityInterval {
                        minimum: fin_domain::Quantity::new("0")?,
                        maximum: fin_domain::Quantity::new("0")?,
                    });
                let min = BigInt::from(old.minimum.integer()) + minimum_delta.integer();
                let max = BigInt::from(old.maximum.integer()) + maximum_delta.integer();
                if min.sign() == Sign::Minus || max < min {
                    return Err(route_proof_error());
                }
                state.positions.retain(|row| {
                    row.resource != *resource
                        || row.position != *position
                        || row.asset != *asset
                        || row.position_kind != *position_kind
                });
                state.positions.push(ProjectedPosition {
                    resource: resource.clone(),
                    position: position.clone(),
                    asset: asset.clone(),
                    position_kind: position_kind.clone(),
                    amount: QuantityInterval {
                        minimum: fin_domain::Quantity::new(min.to_string())?,
                        maximum: fin_domain::Quantity::new(max.to_string())?,
                    },
                });
            }
            ProjectedEffect::AuthorityValue {
                resource,
                authority,
                value,
            } => {
                state
                    .authorities
                    .retain(|row| row.resource != *resource || row.authority != *authority);
                state.authorities.push(fin_domain::AuthorityState {
                    resource: resource.clone(),
                    authority: authority.clone(),
                    value: value.clone(),
                });
            }
            ProjectedEffect::CashSweep { .. } | ProjectedEffect::PositionClose { .. } => {
                closures.push(effect)
            }
        }
    }
    for effect in closures {
        match effect {
            ProjectedEffect::CashSweep {
                resource,
                asset,
                maximum_amount,
                source_step,
            } => {
                let current = state
                    .cash
                    .iter()
                    .find(|row| row.resource == *resource && row.asset == *asset)
                    .map(|row| row.amount.maximum.integer())
                    .unwrap_or_default();
                if *source_step as usize >= step_index || current > maximum_amount.integer() {
                    return Err(route_proof_error());
                }
                state
                    .cash
                    .retain(|row| row.resource != *resource || row.asset != *asset);
                state.cash.push(ProjectedCash {
                    resource: resource.clone(),
                    asset: asset.clone(),
                    amount: QuantityInterval {
                        minimum: fin_domain::Quantity::new("0")?,
                        maximum: fin_domain::Quantity::new("0")?,
                    },
                });
            }
            ProjectedEffect::PositionClose {
                resource,
                position,
                asset,
                position_kind,
                maximum_amount,
                source_step,
            } => {
                let current = state
                    .positions
                    .iter()
                    .find(|row| {
                        row.resource == *resource
                            && row.position == *position
                            && row.asset == *asset
                            && row.position_kind == *position_kind
                    })
                    .map(|row| row.amount.maximum.integer())
                    .unwrap_or_default();
                if *source_step as usize >= step_index || current > maximum_amount.integer() {
                    return Err(route_proof_error());
                }
                state.positions.retain(|row| {
                    row.resource != *resource
                        || row.position != *position
                        || row.asset != *asset
                        || row.position_kind != *position_kind
                });
                state.positions.push(ProjectedPosition {
                    resource: resource.clone(),
                    position: position.clone(),
                    asset: asset.clone(),
                    position_kind: position_kind.clone(),
                    amount: QuantityInterval {
                        minimum: fin_domain::Quantity::new("0")?,
                        maximum: fin_domain::Quantity::new("0")?,
                    },
                });
            }
            _ => unreachable!("only closing selectors are collected"),
        }
    }
    Ok(state)
}

fn route_proof_error() -> FinError {
    FinError::new(
        "RETURN_ROUTE_PROOF_INVALID",
        "budget",
        "The bounded route proof is incomplete, stale, or inconsistent with the projected financial state.",
        "Do not start or continue this run until every step has a fresh verified return route.",
    )
}

fn route_step_error(error: FinError, index: usize, operation: &str, phase: &str) -> FinError {
    FinError::new(
        &error.code,
        &error.stage,
        format!(
            "Route step {index} ({operation}) failed during {phase} with {}.",
            error.code
        ),
        &error.next_action,
    )
}

fn intent_within(actual: &FinancialIntent, approved: &FinancialIntent) -> Result<bool> {
    let actual_debits = aggregate_resource_amounts(&actual.debits)?;
    let approved_debits = aggregate_resource_amounts(&approved.debits)?;
    if actual_debits
        .iter()
        .any(|(key, amount)| amount > approved_debits.get(key).unwrap_or(&BigUint::zero()))
    {
        return Ok(false);
    }
    let actual_costs = aggregate_cost_bounds(&actual.maximum_costs)?;
    let approved_costs = aggregate_cost_bounds(&approved.maximum_costs)?;
    if actual_costs
        .iter()
        .any(|(key, amount)| amount > approved_costs.get(key).unwrap_or(&BigUint::zero()))
    {
        return Ok(false);
    }
    let actual_locks = actual.locks.iter().collect::<BTreeSet<_>>();
    let approved_locks = approved.locks.iter().collect::<BTreeSet<_>>();
    if actual_locks != approved_locks {
        return Ok(false);
    }
    if actual.expected.len() != approved.expected.len() {
        return Ok(false);
    }
    let mut matched = vec![false; approved.expected.len()];
    for actual_constraint in &actual.expected {
        let Some(index) =
            approved
                .expected
                .iter()
                .enumerate()
                .find_map(|(index, approved_constraint)| {
                    (!matched[index] && constraint_within(actual_constraint, approved_constraint))
                        .then_some(index)
                })
        else {
            return Ok(false);
        };
        matched[index] = true;
    }
    Ok(matched.into_iter().all(|matched| matched))
}

fn constraint_within(actual: &EffectConstraint, approved: &EffectConstraint) -> bool {
    match (actual, approved) {
        (
            EffectConstraint::SubmissionCancellation {
                operation_id: a,
                submission: sa,
            },
            EffectConstraint::SubmissionCancellation {
                operation_id: b,
                submission: sb,
            },
        ) => a == b && sa == sb,
        (
            EffectConstraint::DerivativePosition {
                resource: ar,
                position: ap,
                asset: aa,
                position_kind: ak,
                minimum_delta: amin,
                maximum_delta: amax,
            },
            EffectConstraint::DerivativePosition {
                resource: br,
                position: bp,
                asset: ba,
                position_kind: bk,
                minimum_delta: bmin,
                maximum_delta: bmax,
            },
        ) => {
            ar == br
                && ap == bp
                && aa == ba
                && ak == bk
                && amin.integer() >= bmin.integer()
                && amax.integer() <= bmax.integer()
        }
        (
            EffectConstraint::Settlement {
                resource: ar,
                asset: aa,
                category: ac,
                minimum_delta: amin,
                maximum_delta: amax,
            },
            EffectConstraint::Settlement {
                resource: br,
                asset: ba,
                category: bc,
                minimum_delta: bmin,
                maximum_delta: bmax,
            },
        ) => {
            ar == br
                && aa == ba
                && ac == bc
                && amin.integer() >= bmin.integer()
                && amax.integer() <= bmax.integer()
        }
        (
            EffectConstraint::OriginMovement {
                from: actual_from,
                to: actual_to,
                debit_asset: actual_debit_asset,
                credit_asset: actual_credit_asset,
                maximum_debit: actual_max_debit,
                minimum_credit: actual_min_credit,
            },
            EffectConstraint::OriginMovement {
                from: approved_from,
                to: approved_to,
                debit_asset: approved_debit_asset,
                credit_asset: approved_credit_asset,
                maximum_debit: approved_max_debit,
                minimum_credit: approved_min_credit,
            },
        ) => {
            actual_from == approved_from
                && actual_to == approved_to
                && actual_debit_asset == approved_debit_asset
                && actual_credit_asset == approved_credit_asset
                && actual_max_debit <= approved_max_debit
                && actual_min_credit >= approved_min_credit
        }
        (
            EffectConstraint::Accrual {
                resource: ar,
                position: ap,
                asset: aa,
                position_kind: ak,
                maximum_change: am,
            },
            EffectConstraint::Accrual {
                resource: br,
                position: bp,
                asset: ba,
                position_kind: bk,
                maximum_change: bm,
            },
        ) => ar == br && ap == bp && aa == ba && ak == bk && am <= bm,
        (
            EffectConstraint::Debit {
                resource: ar,
                asset: aa,
                maximum_debit: am,
            },
            EffectConstraint::Debit {
                resource: br,
                asset: ba,
                maximum_debit: bm,
            },
        ) => ar == br && aa == ba && am <= bm,
        (
            EffectConstraint::Transfer {
                from: af,
                to: at,
                debit_asset: ada,
                credit_asset: aca,
                maximum_debit: amd,
                minimum_credit: amc,
            },
            EffectConstraint::Transfer {
                from: bf,
                to: bt,
                debit_asset: bda,
                credit_asset: bca,
                maximum_debit: bmd,
                minimum_credit: bmc,
            },
        ) => af == bf && at == bt && ada == bda && aca == bca && amd <= bmd && amc >= bmc,
        (
            EffectConstraint::Conversion {
                resource: ar,
                input: ai,
                maximum_input: ami,
                output: ao,
                minimum_output: amo,
            },
            EffectConstraint::Conversion {
                resource: br,
                input: bi,
                maximum_input: bmi,
                output: bo,
                minimum_output: bmo,
            },
        ) => ar == br && ai == bi && ao == bo && ami <= bmi && amo >= bmo,
        (
            EffectConstraint::Position {
                resource: ar,
                position: ap,
                asset: aa,
                direction: ad,
                maximum_change: am,
            },
            EffectConstraint::Position {
                resource: br,
                position: bp,
                asset: ba,
                direction: bd,
                maximum_change: bm,
            },
        ) => ar == br && ap == bp && aa == ba && ad == bd && am <= bm,
        (
            EffectConstraint::Debt {
                resource: ar,
                debt: ap,
                asset: aa,
                direction: ad,
                maximum_change: am,
            },
            EffectConstraint::Debt {
                resource: br,
                debt: bp,
                asset: ba,
                direction: bd,
                maximum_change: bm,
            },
        ) => ar == br && ap == bp && aa == ba && ad == bd && am <= bm,
        (
            EffectConstraint::Authority {
                resource: ar,
                authority: aa,
                after: aafter,
                ..
            },
            EffectConstraint::Authority {
                resource: br,
                authority: ba,
                after: bafter,
                ..
            },
        ) => ar == br && aa == ba && aafter == bafter,
        (
            EffectConstraint::AuthorityTransition {
                resource: ar,
                authority: aa,
                allowed_after: aas,
                ..
            },
            EffectConstraint::AuthorityTransition {
                resource: br,
                authority: ba,
                allowed_after: bas,
                ..
            },
        ) => ar == br && aa == ba && aas.iter().all(|value| bas.contains(value)),
        (
            EffectConstraint::Credit {
                to: at,
                asset: aa,
                minimum_credit: am,
            },
            EffectConstraint::Credit {
                to: bt,
                asset: ba,
                minimum_credit: bm,
            },
        ) => at == bt && aa == ba && am >= bm,
        _ => false,
    }
}

fn ambiguous_anchor() -> FinError {
    FinError::new(
        "AMBIGUOUS_POSITION_ANCHOR",
        "reconciliation",
        "Verified effects do not identify one exact post-state anchor for the position transition.",
        "Keep the reservation and reconcile the original position evidence.",
    )
}

fn actual_debits_and_costs(effects: &[FinancialEffect]) -> Result<BTreeMap<String, BigUint>> {
    actual_cash_debits(effects, true)
}

fn actual_cash_debits(
    effects: &[FinancialEffect],
    include_additional_costs: bool,
) -> Result<BTreeMap<String, BigUint>> {
    let mut amounts = BTreeMap::new();
    for effect in effects {
        match effect {
            FinancialEffect::CashDebit { resource, amount } => {
                add_big_amount(&mut amounts, resource, amount, amount.atoms.integer())?
            }
            FinancialEffect::Settlement {
                resource,
                asset,
                delta,
                ..
            } if delta.is_negative() => {
                let amount = Amount {
                    asset: asset.clone(),
                    atoms: delta.magnitude(),
                };
                add_big_amount(&mut amounts, resource, &amount, amount.atoms.integer())?;
            }
            FinancialEffect::Transfer { from, debit, .. } => {
                add_big_amount(&mut amounts, from, debit, debit.atoms.integer())?
            }
            FinancialEffect::Conversion {
                resource, spent, ..
            } => add_big_amount(&mut amounts, resource, spent, spent.atoms.integer())?,
            FinancialEffect::Cost {
                location: AccountingLocation::Cash { resource },
                amount,
                treatment: CostTreatment::AdditionalDebit,
                ..
            } if include_additional_costs => {
                add_big_amount(&mut amounts, resource, amount, amount.atoms.integer())?
            }
            FinancialEffect::Cost {
                treatment: CostTreatment::IncludedInDebit,
                ..
            } => {}
            _ => {}
        }
    }
    Ok(amounts)
}

fn cost_key(
    location: &AccountingLocation,
    asset: &fin_domain::AssetId,
    treatment: &CostTreatment,
) -> Result<String> {
    serde_json::to_string(&(location, asset, treatment))
        .map_err(|_| FinError::validation("Invalid typed cost identity."))
}

fn aggregate_cost_bounds(bounds: &[CostBound]) -> Result<BTreeMap<String, BigUint>> {
    let mut map = BTreeMap::new();
    for bound in bounds {
        *map.entry(cost_key(
            &bound.location,
            &bound.amount.asset,
            &bound.treatment,
        )?)
        .or_insert_with(BigUint::zero) += bound.amount.atoms.integer();
    }
    Ok(map)
}

fn aggregate_costs(effects: &[FinancialEffect]) -> Result<BTreeMap<String, BigUint>> {
    let mut map = BTreeMap::new();
    for effect in effects {
        if let FinancialEffect::Cost {
            location,
            amount,
            treatment,
            ..
        } = effect
        {
            *map.entry(cost_key(location, &amount.asset, treatment)?)
                .or_insert_with(BigUint::zero) += amount.atoms.integer();
        }
    }
    Ok(map)
}

fn cash_reservation_amounts(
    debits: &[ResourceAmount],
    costs: &[CostBound],
) -> Result<Vec<ResourceAmount>> {
    let mut amounts = debits.to_vec();
    for cost in costs {
        match (&cost.location, &cost.treatment) {
            (AccountingLocation::Cash { resource }, CostTreatment::AdditionalDebit) => {
                amounts.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: cost.amount.clone(),
                });
            }
            (
                AccountingLocation::Cash { .. },
                CostTreatment::IncludedInDebit | CostTreatment::DeductedFromCredit,
            )
            | (AccountingLocation::Position { .. }, CostTreatment::IncludedInDebit) => {}
            (AccountingLocation::Position { .. }, CostTreatment::DeductedFromCredit) => {
                return Err(route_proof_error());
            }
            (AccountingLocation::Position { .. }, CostTreatment::AdditionalDebit) => {
                return Err(FinError::new(
                    "POSITION_ADDITIONAL_COST_UNSUPPORTED",
                    "budget",
                    "An additional debit cannot be reserved against a position without a reviewed generic accounting mechanic.",
                    "Express the bounded loss as IncludedInDebit on its exact position-origin flow.",
                ));
            }
        }
    }
    Ok(amounts)
}

fn validate_cost_bound_contract(intent: &FinancialIntent) -> Result<()> {
    let debit_map = aggregate_resource_amounts(&intent.debits)?;
    let mut costs = BTreeMap::<String, BigUint>::new();
    for bound in &intent.maximum_costs {
        let key = cost_key(&bound.location, &bound.amount.asset, &bound.treatment)?;
        *costs.entry(key).or_default() += bound.amount.atoms.integer();
    }
    for bound in &intent.maximum_costs {
        let amount =
            costs[&cost_key(&bound.location, &bound.amount.asset, &bound.treatment)?].clone();
        match (&bound.location, &bound.treatment) {
            (AccountingLocation::Cash { resource }, CostTreatment::IncludedInDebit) => {
                let debit_key = resource_asset_key(&ResourceAmount {
                    resource: resource.clone(),
                    amount: bound.amount.clone(),
                })?;
                if amount > debit_map.get(&debit_key).cloned().unwrap_or_default() {
                    return Err(FinError::new(
                        "INCLUDED_COST_UNBACKED",
                        "budget",
                        "An included cash cost must be bounded by the approved cash debit for the same source asset.",
                        "Increase the exact debit bound or use an additional-debit cost treatment.",
                    ));
                }
            }
            (AccountingLocation::Position { .. }, CostTreatment::IncludedInDebit) => {
                let capacity = intent
                    .expected
                    .iter()
                    .filter_map(|constraint| match constraint {
                        EffectConstraint::OriginMovement {
                            from,
                            debit_asset,
                            credit_asset,
                            maximum_debit,
                            minimum_credit,
                            ..
                        } if from == &bound.location
                            && debit_asset == &bound.amount.asset
                            && credit_asset == debit_asset
                            && maximum_debit >= minimum_credit =>
                        {
                            Some(maximum_debit.integer() - minimum_credit.integer())
                        }
                        _ => None,
                    })
                    .fold(BigUint::zero(), |sum, cap| sum + cap);
                if amount > capacity {
                    return Err(FinError::new(
                        "POSITION_COST_UNBACKED",
                        "budget",
                        "An included position cost exceeds the approved position-origin debit-to-credit loss bound.",
                        "Provide a finite origin movement whose exact-asset loss covers the cost bound.",
                    ));
                }
            }
            (AccountingLocation::Position { .. }, CostTreatment::AdditionalDebit) => {
                return Err(FinError::new(
                    "POSITION_ADDITIONAL_COST_UNSUPPORTED",
                    "budget",
                    "An additional debit cannot be reserved against a position without a reviewed generic accounting mechanic.",
                    "Use an explicitly linked IncludedInDebit bound for position-origin loss.",
                ));
            }
            (AccountingLocation::Cash { resource }, CostTreatment::DeductedFromCredit) => {
                let has_credit = intent.expected.iter().any(|expected| match expected {
                    EffectConstraint::Conversion {
                        resource: r,
                        output,
                        ..
                    } => r == resource && output == &bound.amount.asset,
                    EffectConstraint::Transfer {
                        to, credit_asset, ..
                    } => to == resource && credit_asset == &bound.amount.asset,
                    _ => false,
                });
                if !has_credit {
                    return Err(route_proof_error());
                }
            }
            (AccountingLocation::Position { .. }, CostTreatment::DeductedFromCredit) => {
                return Err(route_proof_error());
            }
            (AccountingLocation::Cash { .. }, CostTreatment::AdditionalDebit) => {}
        }
    }
    Ok(())
}

fn ensure_intent_scope(run: &RunLedger, intent: &FinancialIntent) -> Result<()> {
    let mut scope = run
        .allocations
        .iter()
        .map(|a| resource_asset_key(&a.holding))
        .collect::<Result<BTreeSet<_>>>()?;
    // A zero-baseline intermediate wallet may be used only when the broker's
    // stored route proof contains a finite, positive incoming cash projection
    // for that exact resource and asset. `intent_within` separately fences the
    // current operation to its ordered proof step.
    for proof in &run.route_proofs {
        for effect in &proof.effects {
            if let ProjectedEffect::CashDelta {
                resource,
                asset,
                maximum_delta,
                ..
            } = effect
                && maximum_delta.integer().sign() == Sign::Plus
                && run.baseline.coverage.contains(&resource.0)
            {
                scope.insert(resource_asset_key(&ResourceAmount {
                    resource: resource.clone(),
                    amount: Amount {
                        asset: asset.clone(),
                        atoms: fin_domain::Quantity::zero(),
                    },
                })?);
            }
        }
    }
    for amount in &intent.debits {
        if !scope.contains(&resource_asset_key(amount)?) {
            return Err(FinError::new(
                "OUT_OF_SCOPE_RESOURCE",
                "budget",
                "A debit or fee source is outside the explicit run allocation.",
                "Select and snapshot every source and cost asset before starting the run.",
            ));
        }
    }
    for bound in &intent.maximum_costs {
        if matches!(bound.treatment, CostTreatment::AdditionalDebit)
            && !matches!(bound.location, AccountingLocation::Cash { .. })
        {
            return Err(FinError::new(
                "POSITION_ADDITIONAL_COST_UNSUPPORTED",
                "budget",
                "An additional debit cost must use an allocated cash source.",
                "Use a bounded cash cost source.",
            ));
        }
        if matches!(bound.treatment, CostTreatment::AdditionalDebit)
            && !scope.contains(&resource_asset_key(&ResourceAmount {
                resource: bound.location.resource().clone(),
                amount: bound.amount.clone(),
            })?)
        {
            return Err(FinError::new(
                "OUT_OF_SCOPE_RESOURCE",
                "budget",
                "A cash fee source is outside the explicit run allocation.",
                "Select and snapshot every cost source before starting the run.",
            ));
        }
        if matches!(bound.treatment, CostTreatment::AdditionalDebit) {
            let required = intent
                .maximum_costs
                .iter()
                .filter(|other| {
                    other.treatment == CostTreatment::AdditionalDebit
                        && other.location == bound.location
                        && other.amount.asset == bound.amount.asset
                })
                .fold(BigUint::zero(), |sum, other| {
                    sum + other.amount.atoms.integer()
                });
            let selected_origins = run
                .allocations
                .iter()
                .map(|allocation| resource_asset_key(&allocation.holding))
                .collect::<Result<BTreeSet<_>>>()?;
            let selected_available = run
                .provenance
                .iter()
                .filter(|lot| {
                    lot.location
                        == AccountingLocation::Cash {
                            resource: bound.location.resource().clone(),
                        }
                        && lot.asset == bound.amount.asset
                        && !lot.origin.starts_with("accrual:")
                        && allocation_origin_key(&lot.origin)
                            .is_some_and(|key| selected_origins.contains(key))
                })
                .try_fold(BigUint::zero(), |sum, lot| {
                    Ok::<BigUint, FinError>(sum + parse_uint(&lot.atoms)?)
                })?;
            if selected_available < required {
                return Err(FinError::new(
                    "RETURN_COST_SOURCE_UNFUNDED",
                    "budget",
                    "An additional cost bound is not funded by selected allocation-origin cash at its exact resource and asset.",
                    "Reduce the cost bound or select enough of its actual fee asset before beginning the run.",
                ));
            }
        }
        if let AccountingLocation::Position { .. } = &bound.location
            && !run
                .provenance
                .iter()
                .any(|lot| lot.location == bound.location && lot.asset == bound.amount.asset)
        {
            return Err(FinError::new(
                "OUT_OF_SCOPE_POSITION",
                "budget",
                "A position cost source has no selected run allocation provenance.",
                "Use only a position created from this run's selected allocation.",
            ));
        }
    }
    if intent
        .locks
        .iter()
        .any(|lock| !run.baseline.coverage.contains(&lock.0))
    {
        return Err(FinError::new(
            "OUT_OF_SCOPE_RESOURCE",
            "budget",
            "A locked account or signer is outside the captured baseline.",
            "Include every affected resource in the run snapshot.",
        ));
    }
    for constraint in &intent.expected {
        if let EffectConstraint::OriginMovement {
            from: from @ AccountingLocation::Position { .. },
            to,
            debit_asset,
            credit_asset,
            maximum_debit,
            minimum_credit,
            ..
        } = constraint
            && !run
                .provenance
                .iter()
                .any(|lot| &lot.location == from && &lot.asset == debit_asset)
            && !matches_scoped_new_borrow(
                intent,
                from,
                to,
                debit_asset,
                credit_asset,
                maximum_debit,
                minimum_credit,
            )
        {
            return Err(FinError::new(
                "OUT_OF_SCOPE_POSITION",
                "budget",
                "An existing position source has no selected run allocation provenance.",
                "Do not withdraw or use baseline collateral unless a selected-position allocation profile is implemented.",
            ));
        }
        if let EffectConstraint::Position {
            resource,
            position,
            asset,
            direction: Direction::Decrease,
            ..
        } = constraint
        {
            let location = AccountingLocation::Position {
                resource: resource.clone(),
                position: position.clone(),
                position_kind: PositionKind::Supply,
            };
            if !run
                .provenance
                .iter()
                .any(|lot| lot.location == location && &lot.asset == asset)
            {
                return Err(FinError::new(
                    "OUT_OF_SCOPE_POSITION",
                    "budget",
                    "A position decrease must consume provenance created from the selected run allocation.",
                    "Do not use baseline collateral unless a selected-position allocation profile is implemented.",
                ));
            }
        }
    }
    Ok(())
}

fn matches_scoped_new_borrow(
    intent: &FinancialIntent,
    from: &AccountingLocation,
    to: &AccountingLocation,
    debit_asset: &fin_domain::AssetId,
    credit_asset: &fin_domain::AssetId,
    maximum_debit: &fin_domain::Quantity,
    minimum_credit: &fin_domain::Quantity,
) -> bool {
    let AccountingLocation::Position {
        resource,
        position,
        position_kind: PositionKind::StableDebt | PositionKind::VariableDebt,
    } = from
    else {
        return false;
    };
    if !matches!(to, AccountingLocation::Cash { .. })
        || debit_asset != credit_asset
        || minimum_credit.integer() > maximum_debit.integer()
    {
        return false;
    }
    let matching_debt_increases = intent
        .expected
        .iter()
        .filter(|expected| {
            matches!(expected,
                EffectConstraint::Debt {
                    resource: debt_resource,
                    debt,
                    asset,
                    direction: Direction::Increase,
                    maximum_change,
                } if debt_resource == resource
                    && debt == position
                    && asset == debit_asset
                    && maximum_debit.integer() <= maximum_change.integer()
            )
        })
        .count();
    matching_debt_increases == 1
}

fn resource_asset_key(amount: &ResourceAmount) -> Result<String> {
    serde_json::to_string(&(amount.resource.0.clone(), amount.amount.asset.clone()))
        .map_err(|_| FinError::validation("Invalid resource and asset identity."))
}

fn aggregate_resource_amounts(items: &[ResourceAmount]) -> Result<BTreeMap<String, BigUint>> {
    let mut map = BTreeMap::new();
    for item in items {
        let key = resource_asset_key(item)?;
        *map.entry(key).or_insert_with(BigUint::zero) += item.amount.atoms.integer();
    }
    Ok(map)
}

fn aggregate_amounts(items: &[ResourceAmount]) -> Result<BTreeMap<String, BigUint>> {
    aggregate_resource_amounts(items)
}

fn aggregate_provenance_cash(lots: &[ProvenanceLot]) -> Result<BTreeMap<String, BigUint>> {
    let mut totals = BTreeMap::<String, BigUint>::new();
    for lot in lots {
        let AccountingLocation::Cash { resource } = &lot.location else {
            continue;
        };
        let key = resource_asset_key(&ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: lot.asset.clone(),
                atoms: fin_domain::Quantity::zero(),
            },
        })?;
        *totals.entry(key).or_default() += parse_uint(&lot.atoms)?;
    }
    Ok(totals)
}

fn add_big_amount(
    map: &mut BTreeMap<String, BigUint>,
    resource: &ResourceId,
    amount: &Amount,
    value: BigUint,
) -> Result<()> {
    let item = ResourceAmount {
        resource: resource.clone(),
        amount: amount.clone(),
    };
    *map.entry(resource_asset_key(&item)?)
        .or_insert_with(BigUint::zero) += value;
    Ok(())
}

fn amount_for<'a>(
    items: &'a [ResourceAmount],
    resource: &ResourceId,
    asset: &fin_domain::AssetId,
) -> Result<Option<&'a fin_domain::Quantity>> {
    for item in items {
        if &item.resource == resource && &item.amount.asset == asset {
            return Ok(Some(&item.amount.atoms));
        }
    }
    Ok(None)
}

fn add_amounts(
    base: &[ResourceAmount],
    additions: &[ResourceAmount],
) -> Result<Vec<ResourceAmount>> {
    add_amount_map(base, &aggregate_resource_amounts(additions)?)
}

fn add_amount_map(
    base: &[ResourceAmount],
    additions: &BTreeMap<String, BigUint>,
) -> Result<Vec<ResourceAmount>> {
    let mut map: BTreeMap<String, (ResourceId, fin_domain::AssetId, BigUint)> = BTreeMap::new();
    for item in base {
        let key = resource_asset_key(item)?;
        let entry = map.entry(key).or_insert_with(|| {
            (
                item.resource.clone(),
                item.amount.asset.clone(),
                BigUint::zero(),
            )
        });
        entry.2 += item.amount.atoms.integer();
    }
    for (key, value) in additions {
        if value.is_zero() {
            continue;
        }
        let (resource, asset): (String, fin_domain::AssetId) = serde_json::from_str(key)
            .map_err(|_| internal("invalid persisted resource allocation"))?;
        map.entry(key.clone())
            .or_insert_with(|| (ResourceId(resource), asset, BigUint::zero()))
            .2 += value;
    }
    map.into_values()
        .map(|(resource, asset, amount)| {
            Ok(ResourceAmount {
                resource,
                amount: Amount {
                    asset,
                    atoms: fin_domain::Quantity::new(amount.to_string())?,
                },
            })
        })
        .collect()
}

fn subtract_amounts(
    base: &[ResourceAmount],
    subtractions: &BTreeMap<String, BigUint>,
) -> Result<Vec<ResourceAmount>> {
    let mut map = aggregate_resource_amounts(base)?;
    for (key, amount) in subtractions {
        let current = map.get(key).cloned().unwrap_or_default();
        if current < *amount {
            return Err(FinError::new(
                "RESERVATION_EXCEEDED",
                "budget",
                "Selected source inventory is insufficient.",
                "Reduce the exact requested amount.",
            ));
        }
        map.insert(key.clone(), current - amount);
    }
    let retained = base
        .iter()
        .filter_map(|item| {
            let key = resource_asset_key(item).ok()?;
            let units = map.get(&key)?.clone();
            Some((item.resource.clone(), item.amount.asset.clone(), units))
        })
        .collect::<Vec<_>>();
    retained
        .into_iter()
        .map(|(resource, asset, atoms)| {
            Ok(ResourceAmount {
                resource,
                amount: Amount {
                    asset,
                    atoms: fin_domain::Quantity::new(atoms.to_string())?,
                },
            })
        })
        .collect()
}

fn subtract_maps(
    total: &BTreeMap<String, BigUint>,
    used: &BTreeMap<String, BigUint>,
) -> BTreeMap<String, BigUint> {
    total
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                value - used.get(key).cloned().unwrap_or_default(),
            )
        })
        .filter(|(_, value)| !value.is_zero())
        .collect()
}

fn apply_credits(
    run: &RunLedger,
    available: &[ResourceAmount],
    effects: &[FinancialEffect],
) -> Result<Vec<ResourceAmount>> {
    let baseline_resources = run
        .baseline
        .state
        .holdings
        .iter()
        .map(|x| x.resource.clone())
        .chain(
            run.baseline
                .state
                .positions
                .iter()
                .map(|x| x.resource.clone()),
        )
        .chain(
            run.baseline
                .state
                .authorities
                .iter()
                .map(|x| x.resource.clone()),
        )
        .chain(run.baseline.coverage.iter().cloned().map(ResourceId))
        .collect::<BTreeSet<_>>();
    let mut credits = Vec::new();
    for effect in effects {
        match effect {
            FinancialEffect::Transfer { to, credit, .. } => {
                if !baseline_resources.contains(to) {
                    return Err(FinError::new(
                        "OUT_OF_SCOPE_CREDIT",
                        "reconciliation",
                        "A verified receipt credits a resource outside the captured run scope.",
                        "Capture and verify the recipient as part of the run before submission.",
                    ));
                }
                credits.push(ResourceAmount {
                    resource: to.clone(),
                    amount: credit.clone(),
                });
            }
            FinancialEffect::UsableCredit { to, amount } => {
                if !baseline_resources.contains(to) {
                    return Err(FinError::new(
                        "OUT_OF_SCOPE_CREDIT",
                        "reconciliation",
                        "Verified usable credit is outside the captured run scope.",
                        "Capture its receiving resource before the write.",
                    ));
                }
                credits.push(ResourceAmount {
                    resource: to.clone(),
                    amount: amount.clone(),
                });
            }
            FinancialEffect::Conversion {
                resource, received, ..
            } => credits.push(ResourceAmount {
                resource: resource.clone(),
                amount: received.clone(),
            }),
            _ => {}
        }
    }
    for effect in effects {
        if let FinancialEffect::Settlement {
            resource,
            asset,
            delta,
            ..
        } = effect
            && !delta.is_negative()
            && !delta.is_zero()
        {
            if !baseline_resources.contains(resource) {
                return Err(route_proof_error());
            }
            credits.push(ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: asset.clone(),
                    atoms: delta.magnitude(),
                },
            });
        }
    }
    let mut net = add_amounts(available, &credits)?;
    for effect in effects {
        if let FinancialEffect::Cost {
            location: AccountingLocation::Cash { resource },
            amount,
            treatment: CostTreatment::DeductedFromCredit,
            ..
        } = effect
        {
            let deductions = aggregate_resource_amounts(&[ResourceAmount {
                resource: resource.clone(),
                amount: amount.clone(),
            }])?;
            net = subtract_amounts(&net, &deductions)?;
        }
    }
    Ok(net)
}

fn apply_effect_provenance(
    run: &mut RunLedger,
    operation_id: &str,
    cash_lots: &mut Vec<ProvenanceLot>,
    additional_cost_lots: &mut Vec<ProvenanceLot>,
    effects: &[FinancialEffect],
    valuation: &fin_domain::Valuation,
) -> Result<Vec<ResourceAmount>> {
    let mut movement_origins = BTreeMap::<String, Vec<ProvenanceLot>>::new();
    let mut credit_cost_origins = BTreeMap::<String, Vec<ProvenanceLot>>::new();
    for effect in effects {
        match effect {
            FinancialEffect::Settlement {
                resource,
                asset,
                delta,
                ..
            } => {
                let location = AccountingLocation::Cash {
                    resource: resource.clone(),
                };
                if delta.is_negative() {
                    let origins = take_provenance_in_order(
                        cash_lots,
                        &location,
                        asset,
                        &delta.magnitude().integer(),
                    )?;
                    movement_origins
                        .entry(accounting_key(&location, asset)?)
                        .or_default()
                        .extend(origins);
                } else if !delta.is_zero() {
                    let parents = run
                        .provenance
                        .iter()
                        .chain(cash_lots.iter())
                        .filter(|lot| {
                            lot.location == location
                                && lot.asset == *asset
                                && lot.origin.starts_with("allocation:")
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    if parents.is_empty() {
                        return Err(FinError::new(
                            "SETTLEMENT_ORIGIN_MISSING",
                            "reconciliation",
                            "Realized collateral credit has no selected margin origin.",
                            "Keep the original operation unresolved and verify selected collateral.",
                        ));
                    }
                    run.provenance.push(ProvenanceLot {
                        origin: format!("accrual:{}", parents[0].origin),
                        location,
                        asset: asset.clone(),
                        atoms: delta.magnitude().to_string(),
                    });
                }
            }
            FinancialEffect::Transfer {
                from,
                to,
                debit,
                credit,
            } => {
                let source = AccountingLocation::Cash {
                    resource: from.clone(),
                };
                let target = AccountingLocation::Cash {
                    resource: to.clone(),
                };
                let origins = take_provenance_in_order(
                    cash_lots,
                    &source,
                    &debit.asset,
                    &debit.atoms.integer(),
                )?;
                let mut moved =
                    project_lots(&origins, &target, &credit.asset, &credit.atoms.integer())?;
                movement_origins
                    .entry(accounting_key(&source, &debit.asset)?)
                    .or_default()
                    .extend(origins.clone());
                let labels = moved
                    .iter()
                    .map(|lot| lot.origin.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                let withheld = effects.iter().fold(BigUint::zero(), |sum, effect| {
                    sum + match effect {
                        FinancialEffect::Cost {
                            location: l,
                            amount,
                            treatment: CostTreatment::DeductedFromCredit,
                            ..
                        } if l == &target && amount.asset == credit.asset => amount.atoms.integer(),
                        _ => BigUint::zero(),
                    }
                });
                let fee_sources =
                    take_provenance_in_order(&mut moved, &target, &credit.asset, &withheld)?;
                credit_cost_origins
                    .entry(accounting_key(&target, &credit.asset)?)
                    .or_default()
                    .extend(fee_sources);
                run.provenance.extend(moved);
                run.provenance_events.push(ProvenanceEvent {
                    operation_id: operation_id.into(),
                    from: source,
                    to: target,
                    debit: debit.clone(),
                    credit: credit.clone(),
                    origins: labels,
                });
            }
            FinancialEffect::Conversion {
                resource,
                spent,
                received,
            } => {
                let location = AccountingLocation::Cash {
                    resource: resource.clone(),
                };
                let origins = take_provenance_in_order(
                    cash_lots,
                    &location,
                    &spent.asset,
                    &spent.atoms.integer(),
                )?;
                let mut moved = project_lots(
                    &origins,
                    &location,
                    &received.asset,
                    &received.atoms.integer(),
                )?;
                movement_origins
                    .entry(accounting_key(&location, &spent.asset)?)
                    .or_default()
                    .extend(origins.clone());
                let labels = moved
                    .iter()
                    .map(|lot| lot.origin.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                let withheld = effects.iter().fold(BigUint::zero(), |sum, effect| {
                    sum + match effect {
                        FinancialEffect::Cost {
                            location: l,
                            amount,
                            treatment: CostTreatment::DeductedFromCredit,
                            ..
                        } if l == &location && amount.asset == received.asset => {
                            amount.atoms.integer()
                        }
                        _ => BigUint::zero(),
                    }
                });
                let fee_sources =
                    take_provenance_in_order(&mut moved, &location, &received.asset, &withheld)?;
                credit_cost_origins
                    .entry(accounting_key(&location, &received.asset)?)
                    .or_default()
                    .extend(fee_sources);
                run.provenance.extend(moved);
                run.provenance_events.push(ProvenanceEvent {
                    operation_id: operation_id.into(),
                    from: location.clone(),
                    to: location,
                    debit: spent.clone(),
                    credit: received.clone(),
                    origins: labels,
                });
            }
            _ => {}
        }
    }
    for effect in effects {
        match effect {
            FinancialEffect::OriginMovement {
                from,
                to,
                debit,
                credit,
            } => {
                let origins = match from {
                    AccountingLocation::Cash { .. } => take_provenance_in_order(
                        cash_lots,
                        from,
                        &debit.asset,
                        &debit.atoms.integer(),
                    )?,
                    AccountingLocation::Position {
                        position_kind: PositionKind::Supply,
                        ..
                    } => take_provenance(
                        &mut run.provenance,
                        from,
                        &debit.asset,
                        &debit.atoms.integer(),
                    )?,
                    AccountingLocation::Position {
                        position_kind: PositionKind::DerivativeLong | PositionKind::DerivativeShort,
                        ..
                    } => return Err(route_proof_error()),
                    AccountingLocation::Position {
                        position_kind: PositionKind::StableDebt | PositionKind::VariableDebt,
                        ..
                    } => vec![ProvenanceLot {
                        origin: format!("borrowed:{operation_id}"),
                        location: from.clone(),
                        asset: debit.asset.clone(),
                        atoms: debit.atoms.to_string(),
                    }],
                };
                let moved = match to {
                    AccountingLocation::Cash { .. } => {
                        project_lots(&origins, to, &credit.asset, &credit.atoms.integer())?
                    }
                    AccountingLocation::Position {
                        position_kind: PositionKind::Supply,
                        ..
                    } => project_lots(&origins, to, &credit.asset, &credit.atoms.integer())?,
                    AccountingLocation::Position {
                        position_kind: PositionKind::StableDebt | PositionKind::VariableDebt,
                        ..
                    } => vec![],
                    AccountingLocation::Position {
                        position_kind: PositionKind::DerivativeLong | PositionKind::DerivativeShort,
                        ..
                    } => return Err(route_proof_error()),
                };
                let source_key = accounting_key(from, &debit.asset)?;
                movement_origins
                    .entry(source_key)
                    .or_default()
                    .extend(origins.clone());
                let labels = origins
                    .iter()
                    .map(|lot| lot.origin.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                run.provenance.extend(moved);
                run.provenance_events.push(ProvenanceEvent {
                    operation_id: operation_id.into(),
                    from: from.clone(),
                    to: to.clone(),
                    debit: debit.clone(),
                    credit: credit.clone(),
                    origins: labels,
                });
            }
            FinancialEffect::Accrual {
                resource,
                position,
                asset,
                position_kind: PositionKind::Supply,
                delta,
            } if !delta.is_negative() && !delta.is_zero() => {
                let location = AccountingLocation::Position {
                    resource: resource.clone(),
                    position: position.clone(),
                    position_kind: PositionKind::Supply,
                };
                let parent_lots = run
                    .provenance
                    .iter()
                    .filter(|lot| {
                        lot.location == location
                            && lot.asset == *asset
                            && !lot.origin.starts_with("accrual:")
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if parent_lots.is_empty() {
                    return Err(FinError::new(
                        "ACCRUAL_ORIGIN_MISSING",
                        "reconciliation",
                        "Supply accrual has no selected principal origin in this run.",
                        "Keep the run unresolved; do not attribute gains to uncaptured baseline collateral.",
                    ));
                }
                let mut gains =
                    project_lots(&parent_lots, &location, asset, &delta.magnitude().integer())?;
                for gain in &mut gains {
                    gain.origin = format!("accrual:{}", gain.origin);
                }
                run.provenance.extend(gains);
                run.supply_accruals.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: Amount {
                        asset: asset.clone(),
                        atoms: delta.magnitude(),
                    },
                });
            }
            FinancialEffect::PositionChange {
                anchor: Some(anchor),
                ..
            }
            | FinancialEffect::DebtChange {
                anchor: Some(anchor),
                ..
            } => {
                if anchor.evidence_version == 0 || anchor.evidence.is_null() {
                    return Err(FinError::new(
                        "POSITION_ANCHOR_INVALID",
                        "reconciliation",
                        "A verified position change includes an incomplete backing anchor.",
                        "Keep the run unresolved and obtain a complete application-verified position anchor.",
                    ));
                }
                let key = position_anchor_key(anchor)?;
                run.position_anchors
                    .retain(|old| position_anchor_key(old).is_ok_and(|k| k != key));
                run.position_anchors.push(anchor.clone());
            }
            _ => {}
        }
    }
    let mut mapped_costs = Vec::new();
    for effect in effects {
        let FinancialEffect::Cost {
            location,
            amount,
            treatment,
            ..
        } = effect
        else {
            continue;
        };
        let sources = match (location, treatment) {
            (AccountingLocation::Cash { .. }, CostTreatment::AdditionalDebit) => {
                take_provenance_in_order(
                    additional_cost_lots,
                    location,
                    &amount.asset,
                    &amount.atoms.integer(),
                )?
            }
            (AccountingLocation::Cash { .. }, CostTreatment::DeductedFromCredit) => {
                let lots = credit_cost_origins
                    .get_mut(&accounting_key(location, &amount.asset)?)
                    .ok_or_else(route_proof_error)?;
                take_provenance_in_order(lots, location, &amount.asset, &amount.atoms.integer())?
            }
            (AccountingLocation::Position { .. }, CostTreatment::DeductedFromCredit) => {
                return Err(route_proof_error());
            }
            (_, CostTreatment::IncludedInDebit) => {
                let key = accounting_key(location, &amount.asset)?;
                let lots = movement_origins.get_mut(&key).ok_or_else(|| {
                    FinError::new(
                        "COST_ORIGIN_MISSING",
                        "reconciliation",
                        "An included cost has no matching typed origin flow.",
                        "Keep the run unresolved and reconcile its exact debit/credit origin link.",
                    )
                })?;
                take_provenance_ordered(
                    lots,
                    location,
                    &amount.asset,
                    &amount.atoms.integer(),
                    false,
                )?
            }
            (AccountingLocation::Position { .. }, CostTreatment::AdditionalDebit) => {
                return Err(FinError::new(
                    "POSITION_ADDITIONAL_COST_UNSUPPORTED",
                    "reconciliation",
                    "An additional cost cannot be charged directly to a position origin.",
                    "Use a bounded IncludedInDebit cost linked to the exact position flow.",
                ));
            }
        };
        for lot in sources {
            if lot.origin.starts_with("accrual:") {
                continue;
            }
            let (resource, origin_asset, allocation_origin) = lot
                .origin
                .strip_prefix("allocation:")
                .and_then(|key| decode_resource_asset_key(key).ok())
                .map(|(resource, origin_asset)| (resource, Some(origin_asset), true))
                .unwrap_or_else(|| (location.resource().clone(), None, false));
            if !allocation_origin && lot.origin.starts_with("accrual:") {
                continue;
            }
            let charged = match origin_asset.as_ref() {
                Some(origin_asset) if *origin_asset != amount.asset => convert_cost_to_origin(
                    &amount.asset,
                    origin_asset,
                    &fin_domain::Quantity::new(&lot.atoms)?,
                    valuation,
                )?,
                _ => fin_domain::Quantity::new(&lot.atoms)?,
            };
            mapped_costs.push(ResourceAmount {
                resource,
                amount: Amount {
                    asset: origin_asset.unwrap_or_else(|| amount.asset.clone()),
                    atoms: charged,
                },
            });
        }
    }
    Ok(mapped_costs)
}

fn take_provenance(
    lots: &mut Vec<ProvenanceLot>,
    location: &AccountingLocation,
    asset: &fin_domain::AssetId,
    requested: &BigUint,
) -> Result<Vec<ProvenanceLot>> {
    take_provenance_ordered(lots, location, asset, requested, false)
}

/// Dry-run the exact reservation selection without modifying the journal.
/// Principal and additional fees retain separate sources through settlement,
/// including when a failed transaction consumes only its reserved gas.
fn select_cash_reservation_provenance(
    run: &RunLedger,
    intent: &FinancialIntent,
) -> Result<cash_reservation_sources::CashProvenanceSelection> {
    cash_reservation_sources::select(run, intent)
}

enum CashPrincipalPreference {
    Recipient {
        source: ResourceId,
        source_asset: fin_domain::AssetId,
        recipient_origin: String,
    },
    BorrowedFirst(bool),
}

impl CashPrincipalPreference {
    fn priority(&self, origin: &str) -> u8 {
        match self {
            Self::Recipient {
                source,
                source_asset,
                recipient_origin,
            } => cash_transfer_priority(origin, source, source_asset, recipient_origin),
            Self::BorrowedFirst(borrowed_first) => {
                u8::from(origin.starts_with("borrowed:") != *borrowed_first)
            }
        }
    }
}

fn cash_origin_is_home(origin: &str, resource: &ResourceId, asset: &fin_domain::AssetId) -> bool {
    allocation_origin_key(origin)
        .and_then(|key| decode_resource_asset_key(key).ok())
        .is_some_and(|(origin_resource, origin_asset)| {
            &origin_resource == resource && &origin_asset == asset
        })
}

fn cash_transfer_priority(
    origin: &str,
    source: &ResourceId,
    source_asset: &fin_domain::AssetId,
    recipient_origin: &str,
) -> u8 {
    if origin.starts_with("borrowed:") {
        3
    } else if allocation_origin_key(origin) == recipient_origin.strip_prefix("allocation:") {
        0
    } else if cash_origin_is_home(origin, source, source_asset) {
        2
    } else {
        1
    }
}

fn cash_principal_preference(
    intent: &FinancialIntent,
    resource: &ResourceId,
    asset: &fin_domain::AssetId,
) -> Result<CashPrincipalPreference> {
    let targets = intent
        .expected
        .iter()
        .filter_map(|constraint| match constraint {
            EffectConstraint::Transfer {
                from,
                to,
                debit_asset,
                credit_asset,
                ..
            } if from == resource && debit_asset == asset => {
                Some((to.clone(), credit_asset.clone()))
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let debt_repayment = intent.expected.iter().any(|constraint| matches!(constraint,
        EffectConstraint::OriginMovement {
            from: AccountingLocation::Cash { resource: from },
            to: AccountingLocation::Position { position_kind: PositionKind::StableDebt | PositionKind::VariableDebt, .. },
            debit_asset, ..
        } if from == resource && debit_asset == asset
    ));
    if targets.len() > 1 || (debt_repayment && !targets.is_empty()) {
        return Err(FinError::new(
            "AMBIGUOUS_CASH_PRINCIPAL",
            "budget",
            "One cash debit cannot use competing principal-origin selection rules.",
            "Split the typed principal movements into separately bounded operations.",
        ));
    }
    if let Some((recipient, credit_asset)) = targets.into_iter().next() {
        let recipient_origin = format!(
            "allocation:{}",
            resource_asset_key(&ResourceAmount {
                resource: recipient,
                amount: Amount {
                    asset: credit_asset,
                    atoms: fin_domain::Quantity::zero(),
                },
            })?
        );
        Ok(CashPrincipalPreference::Recipient {
            source: resource.clone(),
            source_asset: asset.clone(),
            recipient_origin,
        })
    } else {
        Ok(CashPrincipalPreference::BorrowedFirst(debt_repayment))
    }
}

fn take_provenance_ordered(
    lots: &mut Vec<ProvenanceLot>,
    location: &AccountingLocation,
    asset: &fin_domain::AssetId,
    requested: &BigUint,
    borrowed_first: bool,
) -> Result<Vec<ProvenanceLot>> {
    take_provenance_preferring(lots, location, asset, requested, |lot| {
        lot.origin.starts_with("borrowed:") == borrowed_first
    })
}

fn take_provenance_in_order(
    lots: &mut Vec<ProvenanceLot>,
    source: &AccountingLocation,
    debit_asset: &fin_domain::AssetId,
    requested: &BigUint,
) -> Result<Vec<ProvenanceLot>> {
    take_provenance_preferring(lots, source, debit_asset, requested, |_| true)
}

#[cfg(test)]
fn take_provenance_for_transfer(
    lots: &mut Vec<ProvenanceLot>,
    source: &AccountingLocation,
    debit_asset: &fin_domain::AssetId,
    requested: &BigUint,
    recipient: &ResourceId,
    credit_asset: &fin_domain::AssetId,
) -> Result<Vec<ProvenanceLot>> {
    let recipient_origin = format!(
        "allocation:{}",
        resource_asset_key(&ResourceAmount {
            resource: recipient.clone(),
            amount: Amount {
                asset: credit_asset.clone(),
                atoms: fin_domain::Quantity::zero()
            },
        })?
    );
    take_provenance_matching(
        lots,
        source,
        debit_asset,
        requested,
        |lot| {
            cash_transfer_priority(
                &lot.origin,
                source.resource(),
                debit_asset,
                &recipient_origin,
            )
        },
        |_| true,
    )
}

fn take_provenance_preferring(
    lots: &mut Vec<ProvenanceLot>,
    location: &AccountingLocation,
    asset: &fin_domain::AssetId,
    requested: &BigUint,
    prefer: impl Fn(&ProvenanceLot) -> bool,
) -> Result<Vec<ProvenanceLot>> {
    take_provenance_matching(
        lots,
        location,
        asset,
        requested,
        |lot| u8::from(!prefer(lot)),
        |_| true,
    )
}

fn take_provenance_matching(
    lots: &mut Vec<ProvenanceLot>,
    location: &AccountingLocation,
    asset: &fin_domain::AssetId,
    requested: &BigUint,
    priority: impl Fn(&ProvenanceLot) -> u8,
    eligible: impl Fn(&ProvenanceLot) -> bool,
) -> Result<Vec<ProvenanceLot>> {
    let mut remaining = requested.clone();
    let mut taken = Vec::new();
    let mut ordered = lots
        .iter()
        .enumerate()
        .filter(|(_, lot)| &lot.location == location && &lot.asset == asset && eligible(lot))
        .map(|(index, lot)| (priority(lot), index))
        .collect::<Vec<_>>();
    ordered.sort_unstable();
    for (_, index) in ordered {
        let lot = &mut lots[index];
        if remaining.is_zero() {
            break;
        }
        if &lot.location != location || &lot.asset != asset || !eligible(lot) {
            continue;
        }
        let available = parse_uint(&lot.atoms)?;
        if available.is_zero() {
            continue;
        }
        let amount = available.clone().min(remaining.clone());
        lot.atoms = (available - &amount).to_string();
        remaining -= &amount;
        taken.push(ProvenanceLot {
            origin: lot.origin.clone(),
            location: location.clone(),
            asset: asset.clone(),
            atoms: amount.to_string(),
        });
    }
    lots.retain(|lot| lot.atoms != "0");
    if !remaining.is_zero() {
        return Err(FinError::new(
            "PROVENANCE_UNAVAILABLE",
            "reconciliation",
            "The proved financial flow is not backed by its recorded source allocation or position origin.",
            "Keep the run unresolved and reconcile its source provenance.",
        ));
    }
    Ok(taken)
}

fn project_lots(
    origins: &[ProvenanceLot],
    target: &AccountingLocation,
    asset: &fin_domain::AssetId,
    output_atoms: &BigUint,
) -> Result<Vec<ProvenanceLot>> {
    let total = origins.iter().try_fold(BigUint::zero(), |sum, lot| {
        Ok::<BigUint, FinError>(sum + parse_uint(&lot.atoms)?)
    })?;
    if total.is_zero() || output_atoms.is_zero() {
        return Ok(vec![]);
    }
    if origins.iter().all(|origin| origin.asset == *asset) && output_atoms < &total {
        // A same-asset haircut is charged against principal lots before accrued
        // income lots. This keeps the proved gain amount intact while the paired
        // IncludedInDebit cost is attributed to the exact principal origin.
        let mut loss = &total - output_atoms;
        let mut shares = origins
            .iter()
            .map(|origin| parse_uint(&origin.atoms))
            .collect::<Result<Vec<_>>>()?;
        for accrued in [false, true] {
            for (index, origin) in origins.iter().enumerate() {
                if origin.origin.starts_with("accrual:") != accrued || loss.is_zero() {
                    continue;
                }
                let removed = shares[index].clone().min(loss.clone());
                shares[index] -= &removed;
                loss -= removed;
            }
        }
        if !loss.is_zero() {
            return Err(internal(
                "same-asset provenance loss exceeds its source lots",
            ));
        }
        return Ok(origins
            .iter()
            .zip(shares)
            .filter(|(_, share)| !share.is_zero())
            .map(|(origin, share)| ProvenanceLot {
                origin: origin.origin.clone(),
                location: target.clone(),
                asset: asset.clone(),
                atoms: share.to_string(),
            })
            .collect());
    }
    let mut output_left = output_atoms.clone();
    let mut projected = Vec::new();
    for (index, origin) in origins.iter().enumerate() {
        let input = parse_uint(&origin.atoms)?;
        let share = if index + 1 == origins.len() {
            output_left.clone()
        } else {
            (&input * output_atoms) / &total
        };
        output_left -= &share;
        if !share.is_zero() {
            projected.push(ProvenanceLot {
                origin: origin.origin.clone(),
                location: target.clone(),
                asset: asset.clone(),
                atoms: share.to_string(),
            });
        }
    }
    Ok(projected)
}

fn decode_resource_asset_key(key: &str) -> Result<(ResourceId, fin_domain::AssetId)> {
    serde_json::from_str::<(String, fin_domain::AssetId)>(key)
        .map(|(resource, asset)| (ResourceId(resource), asset))
        .map_err(|_| internal("invalid persisted resource and asset key"))
}

fn position_anchor_key(anchor: &PositionAnchor) -> Result<String> {
    serde_json::to_string(&(
        anchor.resource.clone(),
        anchor.position.clone(),
        anchor.position_kind.clone(),
        anchor.principal.asset.clone(),
    ))
    .map_err(|_| FinError::validation("Invalid position anchor identity."))
}

fn store_reservations(
    operation_id: &str,
    reservation: &RunReservation,
) -> Result<Vec<ReservationRow>> {
    let mut map = BTreeMap::<String, (BigUint, BigUint)>::new();
    for item in &reservation.debits {
        let key = resource_asset_key(item)?;
        map.entry(key).or_default().0 += item.amount.atoms.integer();
    }
    for item in &reservation.maximum_costs {
        let key = cost_key(&item.location, &item.amount.asset, &item.treatment)?;
        map.entry(key).or_default().1 += item.amount.atoms.integer();
    }
    Ok(map
        .into_iter()
        .map(|(resource, (units, fee))| {
            let _ = operation_id;
            ReservationRow {
                resource,
                units: units.to_string(),
                fee_units: fee.to_string(),
            }
        })
        .collect())
}

fn parse_uint(value: &str) -> Result<BigUint> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(FinError::new(
            "INVALID_QUANTITY",
            "validation",
            "Financial quantities must be exact unsigned integer strings.",
            "Use decimal strings in asset atomic units.",
        ));
    }
    BigUint::parse_bytes(value.as_bytes(), 10)
        .ok_or_else(|| FinError::validation("Invalid exact quantity."))
}

fn validate_cost_prices(
    prepared: &PreparedOperation,
    assets: &BTreeSet<fin_domain::AssetId>,
    instant: DateTime<Utc>,
) -> Result<()> {
    if assets.is_empty() {
        return Ok(());
    }
    let valuation = prepared.valuation.as_ref().ok_or_else(|| {
        FinError::new(
            "COST_VALUATION_MISSING",
            "budget",
            "An exact per-asset valuation is required for every reserved cost asset.",
            "Refresh the cost reserve with a current, attributable price for each fee asset.",
        )
    })?;
    validate_price_rows(&valuation.prices, assets, instant)
}

fn prepared_cost_assets(intent: &FinancialIntent) -> BTreeSet<fin_domain::AssetId> {
    intent
        .maximum_costs
        .iter()
        .map(|bound| bound.amount.asset.clone())
        .collect()
}

fn validate_price_rows(
    prices: &[fin_domain::AssetPrice],
    assets: &BTreeSet<fin_domain::AssetId>,
    instant: DateTime<Utc>,
) -> Result<()> {
    for asset in assets {
        let matching = prices
            .iter()
            .filter(|price| price.asset == *asset)
            .collect::<Vec<_>>();
        if matching.len() != 1 {
            return Err(FinError::new(
                "COST_VALUATION_MISSING",
                "budget",
                "Each asset used for cost or return valuation must have one unambiguous exact price observation.",
                "Provide one fresh price per exact financial asset identity.",
            ));
        }
        let price = matching[0];
        validate_price_observation(
            &price.observed_at,
            &price.source,
            instant,
            "COST_VALUATION_STALE",
        )?;
        if price.source.trim().is_empty()
            || price.unit_atoms.is_zero()
            || (price.usd_micros_per_unit.is_zero() && !fin_domain::unpriced::is_estimate(price))
        {
            return Err(FinError::new(
                "COST_VALUATION_INVALID",
                "budget",
                "Cost valuation requires positive exact units and a positive named price or an explicitly marked unpriced exchange estimate.",
                "Refresh with a complete per-asset valuation observation.",
            ));
        }
    }
    Ok(())
}

/// Use the latest timestamp among the adapter valuation and the exact rows used
/// for this settlement calculation. These rows were already checked against
/// wall-clock time before approval; settlement must remain reproducible during
/// delayed reconciliation and must not fetch or reprice after submission.
fn frozen_valuation_observation(
    valuation: &fin_domain::Valuation,
    assets: &BTreeSet<fin_domain::AssetId>,
) -> Result<DateTime<Utc>> {
    let mut latest = parse_time(&valuation.observed_at)?;
    for price in valuation
        .prices
        .iter()
        .filter(|price| assets.contains(&price.asset))
    {
        let observed_at = parse_time(&price.observed_at)?;
        if observed_at > latest {
            latest = observed_at;
        }
    }
    Ok(latest)
}

fn actual_cost_usd(
    effects: &[FinancialEffect],
    valuation: &fin_domain::Valuation,
) -> Result<BigUint> {
    let mut costs = BTreeMap::<fin_domain::AssetId, BigUint>::new();
    for effect in effects {
        if let FinancialEffect::Cost { amount, .. } = effect {
            *costs.entry(amount.asset.clone()).or_default() += amount.atoms.integer();
        }
    }
    let assets = costs.keys().cloned().collect::<BTreeSet<_>>();
    if !assets.is_empty() {
        let observation = frozen_valuation_observation(valuation, &assets)?;
        validate_price_rows(&valuation.prices, &assets, observation)?;
    }
    let mut total = BigUint::zero();
    for (asset, atoms) in costs {
        let price = valuation
            .prices
            .iter()
            .find(|row| row.asset == asset)
            .ok_or_else(|| {
                FinError::new(
                    "COST_VALUATION_MISSING",
                    "reconciliation",
                    "A proved cost has no trusted exact asset price.",
                    "Keep the reservation until an attributable price observation is available.",
                )
            })?;
        let numerator = atoms * price.usd_micros_per_unit.integer();
        total += ceil_div(&numerator, &price.unit_atoms.integer());
    }
    Ok(total)
}

fn convert_cost_to_origin(
    cost_asset: &fin_domain::AssetId,
    origin_asset: &fin_domain::AssetId,
    cost_atoms: &fin_domain::Quantity,
    valuation: &fin_domain::Valuation,
) -> Result<fin_domain::Quantity> {
    let assets = BTreeSet::from([cost_asset.clone(), origin_asset.clone()]);
    let observation = frozen_valuation_observation(valuation, &assets)?;
    validate_price_rows(&valuation.prices, &assets, observation)?;
    let price = |asset: &fin_domain::AssetId| {
        valuation.prices.iter().find(|row| row.asset == *asset).ok_or_else(|| {
            FinError::new(
                "COST_ORIGIN_PRICE_MISSING",
                "reconciliation",
                "A cost cannot be attributed to its original allocation without both exact asset prices.",
                "Keep the run unresolved until its approved exact price map is complete.",
            )
        })
    };
    let cost_price = price(cost_asset)?;
    let origin_price = price(origin_asset)?;
    if cost_price.usd_micros_per_unit.is_zero() || origin_price.usd_micros_per_unit.is_zero() {
        return Err(FinError::new(
            "COST_ORIGIN_PRICE_MISSING",
            "reconciliation",
            "A scoped run cost cannot be converted through an unpriced estimate.",
            "Keep the run unresolved and preserve the approved price map.",
        ));
    }
    let cost_usd = ceil_div(
        &(cost_atoms.integer() * cost_price.usd_micros_per_unit.integer()),
        &cost_price.unit_atoms.integer(),
    );
    fin_domain::Quantity::new(
        ceil_div(
            &(cost_usd * origin_price.unit_atoms.integer()),
            &origin_price.usd_micros_per_unit.integer(),
        )
        .to_string(),
    )
}

fn ceil_div(numerator: &BigUint, denominator: &BigUint) -> BigUint {
    if numerator.is_zero() {
        return BigUint::zero();
    }
    (numerator + denominator - BigUint::from(1u8)) / denominator
}

fn validate_trading_bounds(intent: &FinancialIntent) -> Result<()> {
    let debits = aggregate_resource_amounts(&intent.debits)?;
    let mut losses = BTreeMap::<String, BigUint>::new();
    for expected in &intent.expected {
        match expected {
            EffectConstraint::DerivativePosition {
                position_kind,
                minimum_delta,
                maximum_delta,
                ..
            } => {
                if !position_kind.is_derivative()
                    || minimum_delta.integer() > maximum_delta.integer()
                {
                    return Err(FinError::validation("Invalid bounded derivative exposure."));
                }
            }
            EffectConstraint::Settlement {
                resource,
                asset,
                minimum_delta,
                maximum_delta,
                ..
            } => {
                if minimum_delta.integer() > maximum_delta.integer() {
                    return Err(FinError::validation("Invalid realized collateral bounds."));
                }
                if minimum_delta.is_negative() {
                    let key = resource_asset_key(&ResourceAmount {
                        resource: resource.clone(),
                        amount: Amount {
                            asset: asset.clone(),
                            atoms: minimum_delta.magnitude(),
                        },
                    })?;
                    *losses.entry(key).or_default() += minimum_delta.magnitude().integer();
                }
            }
            _ => {}
        }
    }
    for (key, loss) in losses {
        let (resource, asset) = decode_resource_asset_key(&key)?;
        let costs = intent
            .maximum_costs
            .iter()
            .filter(|bound| {
                bound.location
                    == AccountingLocation::Cash {
                        resource: resource.clone(),
                    }
                    && bound.amount.asset == asset
                    && bound.treatment == CostTreatment::IncludedInDebit
            })
            .fold(BigUint::zero(), |sum, bound| {
                sum + bound.amount.atoms.integer()
            });
        if loss > debits.get(&key).cloned().unwrap_or_default() || loss > costs {
            return Err(FinError::new(
                "SETTLEMENT_LOSS_UNRESERVED",
                "budget",
                "The maximum realized loss needs matching collateral and included cost reserves.",
                "Reserve the complete realized loss and return costs from selected holdings.",
            ));
        }
    }
    Ok(())
}

fn validate_prepared(
    prepared: &PreparedOperation,
    operation: &str,
    instant: DateTime<Utc>,
) -> Result<()> {
    fin_domain::value_bounds::validate(prepared)?;
    validate_trading_bounds(&prepared.intent)?;

    if prepared.operation != operation {
        return Err(FinError::new(
            "OPERATION_MISMATCH",
            "preview",
            "Adapter prepared a different operation.",
            "Use the matching operation definition.",
        ));
    }
    if prepared
        .checks
        .iter()
        .any(|check| check.status == CheckStatus::Failed)
    {
        return Err(FinError::new(
            "PREVIEW_FAILED",
            "preview",
            "A required preparation check failed.",
            "Inspect the failed checks and revise the request.",
        ));
    }
    validate_future_expiry(&prepared.expires_at, instant, "PREVIEW_EXPIRED")
}

fn validate_approval(
    audit: &ApprovalAudit,
    challenge: &AuthorizationChallenge,
    instant: DateTime<Utc>,
) -> Result<()> {
    ensure_audit_binding(audit, &challenge.binding_hash)?;
    if audit.challenge_nonce != challenge.nonce {
        return Err(FinError::new(
            "INVALID_APPROVAL",
            "approval",
            "Approval does not match the broker-generated challenge.",
            "Request fresh approval for this exact operation.",
        ));
    }
    let approved = parse_time(&audit.approved_at)?;
    let expiry = parse_time(&audit.expires_at)?;
    let challenge_expiry = parse_time(&challenge.expires_at)?;
    if approved > instant + chrono::Duration::seconds(5)
        || expiry <= instant
        || approved >= expiry
        || expiry > challenge_expiry
    {
        return Err(FinError::new(
            "INVALID_APPROVAL",
            "approval",
            "Approval is expired, future-dated, or outlives its prepared operation.",
            "Request fresh approval for the current operation.",
        ));
    }
    Ok(())
}

fn ensure_audit_binding(audit: &ApprovalAudit, expected_hash: &str) -> Result<()> {
    if audit.binding_hash != expected_hash {
        return Err(FinError::new(
            "INVALID_APPROVAL",
            "approval",
            "Approval binding differs from the exact prepared operation.",
            "Request fresh approval for the current operation.",
        ));
    }
    Ok(())
}

fn ensure_approval_fresh(
    audit: &ApprovalAudit,
    challenge: &AuthorizationChallenge,
    instant: DateTime<Utc>,
) -> Result<()> {
    ensure_audit_binding(audit, &challenge.binding_hash)?;
    validate_future_expiry(&audit.expires_at, instant, "APPROVAL_EXPIRED")
}

fn validate_future_expiry(value: &str, instant: DateTime<Utc>, code: &str) -> Result<()> {
    if parse_time(value)? <= instant {
        return Err(FinError::new(
            code,
            "approval",
            "The prepared operation or approval expired before submission.",
            "Refresh the preview and obtain fresh approval.",
        ));
    }
    Ok(())
}

fn validate_price_observation(
    value: &str,
    source: &str,
    instant: DateTime<Utc>,
    code: &str,
) -> Result<()> {
    if fin_domain::price_freshness::is_fresh(value, source, instant) {
        Ok(())
    } else {
        Err(FinError::new(
            code,
            "valuation",
            "Price evidence has expired or contains invalid leg timestamps.",
            "Obtain fresh prices for the specific expired leg.",
        ))
    }
}

fn validate_observation(value: &str, instant: DateTime<Utc>, code: &str) -> Result<()> {
    let at = parse_time(value)?;
    if at > instant + chrono::Duration::seconds(5)
        || instant.signed_duration_since(at).num_seconds() > MAX_OBSERVATION_AGE_SECONDS
    {
        return Err(FinError::new(
            code,
            "budget",
            "The baseline or price observation is stale or future-dated.",
            "Capture a fresh complete state and valuation.",
        ));
    }
    Ok(())
}

fn current_validation_time(requested: DateTime<Utc>) -> DateTime<Utc> {
    std::cmp::max(requested, Utc::now())
}

fn parse_time(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|_| {
            FinError::new(
                "INVALID_TIMESTAMP",
                "validation",
                "A required ISO 8601 timestamp is invalid.",
                "Provide a timezone-qualified timestamp.",
            )
        })
}

fn status_for_error(error: &FinError) -> Status {
    let code = error.code.to_ascii_uppercase();
    if code.contains("CANCEL") {
        Status::Cancelled
    } else if code.contains("DECLIN") || code.contains("REJECT") {
        Status::Rejected
    } else if code.contains("EXPIRED") {
        Status::Expired
    } else {
        Status::Failed
    }
}

fn compare_exact<T: Serialize + PartialEq>(
    before: &[T],
    after: &[T],
    category: &str,
    differences: &mut BTreeMap<String, String>,
) {
    let before = before
        .iter()
        .map(|v| serde_json::to_string(v).unwrap_or_default())
        .collect::<BTreeSet<_>>();
    let after = after
        .iter()
        .map(|v| serde_json::to_string(v).unwrap_or_default())
        .collect::<BTreeSet<_>>();
    for item in before.difference(&after) {
        differences.insert(
            format!("{category}:{item}"),
            "missing from final state".into(),
        );
    }
    for item in after.difference(&before) {
        differences.insert(
            format!("{category}:{item}"),
            "additional final state".into(),
        );
    }
}

fn snapshot_resources_covered(snapshot: &StateSnapshot) -> bool {
    let coverage = snapshot.coverage.iter().collect::<BTreeSet<_>>();
    snapshot
        .state
        .holdings
        .iter()
        .map(|item| &item.resource.0)
        .chain(snapshot.state.positions.iter().map(|item| &item.resource.0))
        .chain(
            snapshot
                .state
                .authorities
                .iter()
                .map(|item| &item.resource.0),
        )
        .all(|resource| coverage.contains(resource))
}

fn run_result(run: &RunLedger) -> RunResult {
    RunResult {
        id: run.id.clone(),
        status: run.status.clone(),
        differences: run.differences.clone(),
        net_conversion_deltas: run.net_conversion_deltas.clone(),
        baseline_observed_at: run.baseline.observed_at.clone(),
        final_observed_at: run.final_observed_at.clone(),
    }
}

fn to_value<T: Serialize>(value: &T) -> Result<Value> {
    serde_json::to_value(value).map_err(|_| {
        FinError::new(
            "SERIALIZATION_ERROR",
            "persistence",
            "Operation state could not be encoded for durable storage.",
            "Preserve the current request and inspect the local journal.",
        )
    })
}

fn boundary_error(message: &str) -> FinError {
    FinError::new(
        "BOUNDARY_AUTHORIZATION_INVALID",
        "authorization",
        message,
        "Review the operation boundaries and request fresh approval if the intended scope changed.",
    )
}

fn format_boundary_usage(debits: &[ResourceAmount], costs: &[CostBound]) -> String {
    let debit_text = if debits.is_empty() {
        "no debit amounts".to_owned()
    } else {
        debits
            .iter()
            .map(|item| {
                format!(
                    "{} {}",
                    item.amount.atoms.to_decimal(item.amount.asset.decimals),
                    item.amount.asset.reference,
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let cost_text = if costs.is_empty() {
        "no fee amounts".to_owned()
    } else {
        costs
            .iter()
            .map(|item| {
                format!(
                    "{} {} fees",
                    item.amount.atoms.to_decimal(item.amount.asset.decimals),
                    item.amount.asset.reference,
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!("{debit_text}; {cost_text}")
}

fn validate_boundary_request(request: &BoundaryAuthorizationRequest) -> Result<()> {
    if request.request_id.trim().is_empty() || request.operation.trim().is_empty() {
        return Err(boundary_error(
            "A request identity and operation are required.",
        ));
    }
    if request.valid_for_seconds == 0 {
        return Err(boundary_error(
            "Authorization duration must be a positive number of seconds.",
        ));
    }
    if chrono::Duration::try_seconds(
        i64::try_from(request.valid_for_seconds)
            .map_err(|_| boundary_error("Authorization duration is too large."))?,
    )
    .is_none()
    {
        return Err(boundary_error("Authorization duration is too large."));
    }
    Ok(())
}

fn validate_boundary_spec(prepared: &PreparedOperation, spec: &BoundarySpec) -> Result<()> {
    if spec.summary.trim().is_empty() || spec.fixed.is_null() || spec.parameters.is_null() {
        return Err(boundary_error(
            "The adapter returned incomplete boundary scope.",
        ));
    }
    if spec.maximum_debits.is_empty() && spec.maximum_costs.is_empty() {
        return Err(boundary_error(
            "The adapter returned no enforceable debit or cost limits.",
        ));
    }
    if spec.maximum_debits.iter().any(|limit| {
        !prepared.intent.debits.iter().any(|debit| {
            debit.resource == limit.resource && debit.amount.asset == limit.amount.asset
        })
    }) {
        return Err(boundary_error(
            "A debit boundary does not identify a resource and asset in the prepared intent.",
        ));
    }
    Ok(())
}

fn validate_candidate_caps(
    authorization: &BoundaryAuthorization,
    prepared: &PreparedOperation,
) -> Result<()> {
    for debit in &prepared.intent.debits {
        let Some(cap) = authorization
            .remaining_debits
            .iter()
            .find(|cap| cap.resource == debit.resource && cap.amount.asset == debit.amount.asset)
        else {
            return Err(boundaries::violation_error(
                "Candidate consumes a resource or asset outside the remaining authorization.",
                boundaries::BoundaryViolation {
                    constraint: "resource_asset_scope".into(),
                    approved_bound: json!(authorization.remaining_debits),
                    candidate_value: json!(debit),
                    evidence_time: Some(prepared.observed_at.clone()),
                    reauthorization_required: true,
                    reconciliation_required: false,
                },
            ));
        };
        if debit.amount.atoms > cap.amount.atoms {
            return Err(boundaries::violation_error(
                "Candidate debit exceeds the remaining approved amount.",
                boundaries::BoundaryViolation {
                    constraint: "maximum_debit".into(),
                    approved_bound: json!(cap),
                    candidate_value: json!(debit),
                    evidence_time: Some(prepared.observed_at.clone()),
                    reauthorization_required: true,
                    reconciliation_required: false,
                },
            ));
        }
    }
    for cost in &prepared.intent.maximum_costs {
        let Some(cap) = authorization.remaining_costs.iter().find(|cap| {
            cap.location == cost.location
                && cap.amount.asset == cost.amount.asset
                && cap.treatment == cost.treatment
        }) else {
            return Err(boundaries::violation_error(
                "Candidate includes a fee outside the remaining authorization.",
                boundaries::BoundaryViolation {
                    constraint: "fee_scope".into(),
                    approved_bound: json!(authorization.remaining_costs),
                    candidate_value: json!(cost),
                    evidence_time: Some(prepared.observed_at.clone()),
                    reauthorization_required: true,
                    reconciliation_required: false,
                },
            ));
        };
        if cost.amount.atoms > cap.amount.atoms {
            return Err(boundaries::violation_error(
                "Candidate fee exceeds the remaining approved amount.",
                boundaries::BoundaryViolation {
                    constraint: "maximum_fee".into(),
                    approved_bound: json!(cap),
                    candidate_value: json!(cost),
                    evidence_time: Some(prepared.observed_at.clone()),
                    reauthorization_required: true,
                    reconciliation_required: false,
                },
            ));
        }
    }
    Ok(())
}

fn remaining_debits(
    maximum: &[ResourceAmount],
    spent: Option<&Vec<ResourceAmount>>,
) -> Result<Vec<ResourceAmount>> {
    let spent = spent.cloned().unwrap_or_default();
    maximum
        .iter()
        .map(|limit| {
            let used = spent
                .iter()
                .filter(|item| {
                    item.resource == limit.resource && item.amount.asset == limit.amount.asset
                })
                .try_fold(fin_domain::Quantity::zero(), |sum, item| {
                    sum.checked_add(&item.amount.atoms)
                })?;
            let atoms = limit.amount.atoms.checked_sub(&used).map_err(|_| {
                boundary_error("Replacement boundaries cannot restore spent debit authority.")
            })?;
            Ok(ResourceAmount {
                resource: limit.resource.clone(),
                amount: Amount {
                    asset: limit.amount.asset.clone(),
                    atoms,
                },
            })
        })
        .collect()
}

fn remaining_costs(
    maximum: &[CostBound],
    spent: Option<&Vec<CostBound>>,
) -> Result<Vec<CostBound>> {
    let spent = spent.cloned().unwrap_or_default();
    maximum
        .iter()
        .map(|limit| {
            let used = spent
                .iter()
                .filter(|item| {
                    item.location == limit.location
                        && item.amount.asset == limit.amount.asset
                        && item.treatment == limit.treatment
                })
                .try_fold(fin_domain::Quantity::zero(), |sum, item| {
                    sum.checked_add(&item.amount.atoms)
                })?;
            let atoms = limit.amount.atoms.checked_sub(&used).map_err(|_| {
                boundary_error("Replacement boundaries cannot restore spent fee authority.")
            })?;
            Ok(CostBound {
                location: limit.location.clone(),
                amount: Amount {
                    asset: limit.amount.asset.clone(),
                    atoms,
                },
                treatment: limit.treatment.clone(),
            })
        })
        .collect()
}

fn inject_boundary_marker(
    prepared: &mut PreparedOperation,
    authorization: &BoundaryAuthorization,
    attempt_binding_hash: String,
) -> Result<()> {
    let grant_binding = boundaries::authorization_binding(
        &authorization.operation,
        &authorization.intent_id,
        &authorization.spec,
        authorization.valid_for_seconds,
    );
    let mut object = prepared
        .binding
        .as_object()
        .cloned()
        .ok_or_else(|| boundary_error("Prepared execution binding must be an object."))?;
    object.remove(fin_domain::value_bounds::KEY);
    object.insert(
        boundaries::BOUNDARY_METADATA_KEY.to_owned(),
        json!({
            "version": 1,
            "authorization_id": authorization.id,
            "intent_id": authorization.intent_id,
            "expires_at": authorization.expires_at,
            "fixed": authorization.spec.fixed,
            "parameters": authorization.spec.parameters,
            "maximum_debits": authorization.spec.maximum_debits,
            "maximum_costs": authorization.spec.maximum_costs,
            "grant_binding": grant_binding,
            "attempt_binding_hash": attempt_binding_hash,
        }),
    );
    prepared.binding = Value::Object(object);
    Ok(())
}

/// Broker-owned journal markers must never cross the adapter API boundary.
/// They are restored on the durable engine preview after validation.
fn strip_plan_metadata(prepared: &PreparedOperation) -> PreparedOperation {
    let mut candidate = prepared.clone();
    if let Some(binding) = candidate.binding.as_object_mut() {
        binding.remove(fin_domain::financial_plans::PLAN_METADATA_KEY);
    }
    candidate
}

fn account_boundary_effects(
    authorization: &mut BoundaryAuthorization,
    effects: Option<&VerifiedEffects>,
) -> Result<()> {
    let Some(effects) = effects else {
        return Ok(());
    };
    let mut debit_consumption: Vec<ResourceAmount> = Vec::new();
    let mut cost_consumption: Vec<CostBound> = Vec::new();
    for effect in &effects.effects {
        match effect {
            FinancialEffect::CashDebit { resource, amount } => {
                add_resource_amount(&mut debit_consumption, resource, amount)?
            }
            FinancialEffect::Transfer { from, debit, .. } => {
                add_resource_amount(&mut debit_consumption, from, debit)?
            }
            FinancialEffect::Conversion {
                resource, spent, ..
            } => add_resource_amount(&mut debit_consumption, resource, spent)?,
            FinancialEffect::OriginMovement { from, debit, .. } => {
                add_resource_amount(&mut debit_consumption, from.resource(), debit)?
            }
            FinancialEffect::Cost {
                location,
                amount,
                treatment,
                ..
            } => {
                let incoming = CostBound {
                    location: location.clone(),
                    amount: amount.clone(),
                    treatment: treatment.clone(),
                };
                if let Some(existing) = cost_consumption.iter_mut().find(|item| {
                    item.location == incoming.location
                        && item.amount.asset == incoming.amount.asset
                        && item.treatment == incoming.treatment
                }) {
                    existing.amount.atoms =
                        existing.amount.atoms.checked_add(&incoming.amount.atoms)?;
                } else {
                    cost_consumption.push(incoming);
                }
            }
            _ => {}
        }
    }
    for consumed in debit_consumption {
        if let Some(current) = authorization.spent_debits.iter_mut().find(|item| {
            item.resource == consumed.resource && item.amount.asset == consumed.amount.asset
        }) {
            current.amount.atoms = current.amount.atoms.checked_add(&consumed.amount.atoms)?;
        } else {
            authorization.spent_debits.push(consumed);
        }
    }
    for consumed in cost_consumption {
        if let Some(current) = authorization.spent_costs.iter_mut().find(|item| {
            item.location == consumed.location
                && item.amount.asset == consumed.amount.asset
                && item.treatment == consumed.treatment
        }) {
            current.amount.atoms = current.amount.atoms.checked_add(&consumed.amount.atoms)?;
        } else {
            authorization.spent_costs.push(consumed);
        }
    }
    authorization.remaining_debits = remaining_debits(
        &authorization.spec.maximum_debits,
        Some(&authorization.spent_debits),
    )?;
    authorization.remaining_costs = remaining_costs(
        &authorization.spec.maximum_costs,
        Some(&authorization.spent_costs),
    )?;
    if authorization.status == BoundaryAuthorizationStatus::Active
        && !authorization.remaining_debits.is_empty()
        && authorization
            .remaining_debits
            .iter()
            .all(|limit| limit.amount.atoms.is_zero())
    {
        // Unused fee allowance cannot authorize more of a completed spend.
        authorization.status = BoundaryAuthorizationStatus::Exhausted;
    }
    Ok(())
}

fn add_resource_amount(
    rows: &mut Vec<ResourceAmount>,
    resource: &ResourceId,
    amount: &Amount,
) -> Result<()> {
    if let Some(existing) = rows
        .iter_mut()
        .find(|item| &item.resource == resource && item.amount.asset == amount.asset)
    {
        existing.amount.atoms = existing.amount.atoms.checked_add(&amount.atoms)?;
    } else {
        rows.push(ResourceAmount {
            resource: resource.clone(),
            amount: amount.clone(),
        });
    }
    Ok(())
}

fn from_value<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|_| {
        FinError::new(
            "JOURNAL_VERSION_UNSUPPORTED",
            "persistence",
            "Durable operation state has an unknown or incompatible version.",
            "Use a compatible Fin version to reconcile the original submission.",
        )
    })
}

fn map_store(error: StoreError) -> FinError {
    match error {
        StoreError::RequestConflict => FinError::new(
            "REQUEST_ID_CONFLICT",
            "validation",
            "This request identity already names different parameters.",
            "Inspect the original record and choose a fresh ID only for a deliberate new action.",
        ),
        StoreError::SubmissionConflict => FinError::new(
            "SUBMISSION_IDENTITY_CHANGED",
            "submission",
            "The durable external submission identity cannot be changed.",
            "Reconcile the original submission.",
        ),
        StoreError::ReservationConflict => FinError::new(
            "RESERVATION_CHANGED",
            "budget",
            "The exact resource reservation cannot be changed after persistence.",
            "Reconcile or release it only with verified evidence.",
        ),
        StoreError::OperationNotFound => FinError::new(
            "OPERATION_NOT_FOUND",
            "persistence",
            "Operation record does not exist.",
            "Inspect available operation identities.",
        ),
        StoreError::AlreadySettled => FinError::new(
            "OPERATION_SETTLED",
            "persistence",
            "Operation is already terminal.",
            "Inspect the existing outcome instead of resubmitting.",
        ),
        StoreError::ConcurrentReservation => FinError::new(
            "RESOURCE_RESERVED",
            "budget",
            "Operation state changed or another write holds a required resource reservation.",
            "Reconcile the original submission before starting another write.",
        ),
        StoreError::ActiveRunExists => FinError::new(
            "ACTIVE_RUN_EXISTS",
            "budget",
            "A non-restored run already owns the run allocation.",
            "Finish or restore the existing run before beginning another.",
        ),
        other => FinError::new(
            "STORE_ERROR",
            "persistence",
            other.to_string(),
            "Preserve the original request and retry status inspection.",
        ),
    }
}

fn internal(message: &str) -> FinError {
    FinError::new(
        "ENGINE_INTERNAL",
        "engine",
        message,
        "Preserve the operation identity and inspect the durable record.",
    )
}

fn cost_origin_price_unavailable() -> FinError {
    FinError::new(
        "COST_ORIGIN_PRICE_UNAVAILABLE",
        "budget",
        "A selected cost origin has no trusted exact asset price provider.",
        "Configure the protected valuation provider and preview again.",
    )
}

#[cfg(test)]
mod projected_flow_tests;

#[cfg(test)]
mod observer_baseline_tests;

#[cfg(test)]
mod cost_origin_price_tests;

#[cfg(test)]
mod frozen_valuation_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use fin_domain::{
        Amount, AssetId, Authorizer, Definition, EffectConstraint, ExecutionActor, FinancialState,
        PositionAnchor, Quantity, SignedQuantity, StateSnapshot,
    };
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn resource(value: &str) -> ResourceId {
        ResourceId(value.to_owned())
    }

    fn asset(network: &str, reference: &str, decimals: u8) -> AssetId {
        AssetId {
            network: network.into(),
            reference: reference.into(),
            decimals,
        }
    }

    fn quantity(value: &str) -> Quantity {
        Quantity::new(value).unwrap()
    }

    fn record(expected: Vec<EffectConstraint>, maximum_costs: Vec<CostBound>) -> OperationRecord {
        let binding = json!({"operation":"synthetic.transfer","amount":"1"});
        let prepared = PreparedOperation {
            operation: "synthetic.transfer".into(),
            input: json!({"amount":"1"}),
            binding,
            summary: "synthetic transfer".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: "2026-01-01T00:00:00Z".into(),
            expires_at: "2026-01-01T00:01:00Z".into(),
            valuation: None,
            intent: FinancialIntent {
                debits: vec![ResourceAmount {
                    resource: resource("binance-account"),
                    amount: Amount {
                        asset: asset("binance", "ETH", 18),
                        atoms: quantity("1000000000000000000"),
                    },
                }],
                maximum_costs,
                expected,
                locks: vec![],
            },
            operation_version: 1,
        };
        let audit = ApprovalAudit {
            mechanism: "synthetic-test-authorizer".into(),
            binding_hash: binding_hash(&authorization_binding(&prepared)).unwrap(),
            challenge_nonce: "test-nonce-001".into(),
            approved_at: "2026-01-01T00:00:00Z".into(),
            expires_at: "2026-01-01T00:01:00Z".into(),
        };
        OperationRecord {
            id: "synthetic-op-1".into(),
            request_id: "synthetic-request-1".into(),
            fingerprint: "synthetic-fingerprint".into(),
            fingerprint_version: 1,
            operation: "synthetic.transfer".into(),
            input: json!({"amount":"1"}),
            status: Status::Pending,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            preview: Some(prepared),
            approval: Some(audit),
            submission: Some(Submission {
                kind: "synthetic_tx".into(),
                reference: "synthetic-tx-001".into(),
                data: json!({"nonce":"synthetic-17"}),
            }),
            dispatch_evidence: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        }
    }

    #[test]
    fn dispatch_receipt_persists_after_approval_expiry_without_changing_submission_state() {
        let mut operation = record(vec![], vec![]);
        let mut legacy_json = to_value(&operation).unwrap();
        legacy_json
            .as_object_mut()
            .unwrap()
            .remove("dispatch_evidence");
        let legacy: OperationRecord = from_value(legacy_json).unwrap();
        assert_eq!(legacy.dispatch_evidence, None);
        let submission = operation.submission.clone().unwrap();
        let mut store = Store::in_memory().unwrap();
        store
            .create_request(
                &operation.id,
                &operation.request_id,
                &operation.fingerprint,
                &operation.operation,
                None,
                "pending",
                &to_value(&operation).unwrap(),
                &operation.created_at,
            )
            .unwrap();
        store
            .persist_submission(
                &operation.id,
                &SubmissionRow {
                    kind: submission.kind.clone(),
                    reference: submission.reference.clone(),
                    data: submission.data.clone(),
                },
                &operation.updated_at,
            )
            .unwrap();
        operation.status = Status::Submitted;

        let journal = OperationSubmissionJournal {
            operation_id: operation.id.clone(),
            expected_binding_hash: operation.approval.as_ref().unwrap().binding_hash.clone(),
            approval: operation.approval.clone().unwrap(),
            boundary_authorization_id: None,
            boundary_authorization_snapshot: None,
            financial_plan_id: None,
            financial_plan_snapshot: None,
            adapter: None,
            options: EngineOptions::default(),
            consumed: AtomicBool::new(true),
            store: Arc::new(Mutex::new(store)),
            record: Mutex::new(operation.clone()),
        };
        let evidence = json!({"txn_id":"SYNTHETIC-txn-42"});
        journal
            .persist_dispatch_evidence(&submission, &evidence)
            .unwrap();
        let persisted = journal.record.lock().unwrap().clone();
        assert_eq!(persisted.dispatch_evidence, Some(evidence.clone()));
        assert_eq!(persisted.submission, Some(submission.clone()));
        assert_eq!(persisted.status, Status::Submitted);
        assert_eq!(
            journal
                .store
                .lock()
                .unwrap()
                .get_record(&operation.id)
                .unwrap()
                .unwrap()["dispatch_evidence"],
            evidence
        );
    }

    fn proof(record: &OperationRecord, effects: Vec<FinancialEffect>) -> VerifiedEffects {
        let mut verified = VerifiedEffects {
            evidence_version: 1,
            submission: record.submission.clone().unwrap(),
            effects,
            evidence: json!({"fixture":"synthetic receipt evidence"}),
            observed_at: "2026-01-01T00:00:30Z".into(),
            proof_digest: String::new(),
        };
        verified.proof_digest = verified_effect_digest(&verified).unwrap();
        verified
    }

    fn synthetic_route_plan() -> (Vec<PlanStep>, Vec<PlanStep>, Vec<ReturnRouteProof>) {
        let forward = PlanStep {
            operation: "synthetic.transfer".into(),
            input: json!({"amount":"1"}),
            maximum_fee_usd_micros: "0".into(),
            description: "synthetic forward transfer".into(),
        };
        let unwind = PlanStep {
            operation: "synthetic.return".into(),
            input: json!({"amount":"1"}),
            maximum_fee_usd_micros: "0".into(),
            description: "synthetic return transfer".into(),
        };
        let forward_intent = record(vec![transfer_constraint()], vec![])
            .preview
            .unwrap()
            .intent;
        let binance = resource("binance-account");
        let ethereum = resource("ethereum-wallet");
        let binance_eth = asset("binance", "ETH", 18);
        let ethereum_eth = asset("ethereum:1", "native", 18);
        let one = quantity("1000000000000000000");
        let reverse_intent = FinancialIntent {
            debits: vec![ResourceAmount {
                resource: ethereum.clone(),
                amount: Amount {
                    asset: ethereum_eth.clone(),
                    atoms: one.clone(),
                },
            }],
            expected: vec![EffectConstraint::Transfer {
                from: ethereum.clone(),
                to: binance.clone(),
                debit_asset: ethereum_eth.clone(),
                credit_asset: binance_eth.clone(),
                maximum_debit: one.clone(),
                minimum_credit: one.clone(),
            }],
            ..Default::default()
        };
        let now_time = Utc::now();
        let proof =
            |index: u32, step: &PlanStep, intent: FinancialIntent, effects| ReturnRouteProof {
                step_index: index,
                operation: step.operation.clone(),
                input_hash: binding_hash(&step.input).unwrap(),
                observed_at: now_time.to_rfc3339(),
                expires_at: (now_time + chrono::Duration::minutes(2)).to_rfc3339(),
                intent,
                effects,
                requires: vec![],
                evidence: json!({"fixture":"synthetic route proof"}),
            };
        let proofs = vec![
            proof(
                0,
                &forward,
                forward_intent,
                vec![
                    ProjectedEffect::CashDelta {
                        resource: binance.clone(),
                        asset: binance_eth.clone(),
                        minimum_delta: SignedQuantity::negative(&one),
                        maximum_delta: SignedQuantity::negative(&one),
                    },
                    ProjectedEffect::CashDelta {
                        resource: ethereum.clone(),
                        asset: ethereum_eth.clone(),
                        minimum_delta: SignedQuantity::positive(&one),
                        maximum_delta: SignedQuantity::positive(&one),
                    },
                ],
            ),
            proof(
                1,
                &unwind,
                reverse_intent,
                vec![
                    ProjectedEffect::CashDelta {
                        resource: ethereum,
                        asset: ethereum_eth,
                        minimum_delta: SignedQuantity::negative(&one),
                        maximum_delta: SignedQuantity::negative(&one),
                    },
                    ProjectedEffect::CashDelta {
                        resource: binance,
                        asset: binance_eth,
                        minimum_delta: SignedQuantity::positive(&one),
                        maximum_delta: SignedQuantity::positive(&one),
                    },
                ],
            ),
        ];
        (vec![forward], vec![unwind], proofs)
    }

    fn synthetic_run_plan(id: &str, baseline: &StateSnapshot, value_usd: &str) -> RunPlan {
        let (planned_operations, unwind, route_proofs) = synthetic_route_plan();
        let holding = baseline.state.holdings[0].clone();
        RunPlan {
            id: id.into(),
            baseline: baseline.clone(),
            allocations: vec![RunAllocation {
                holding,
                value_usd_micros: value_usd.into(),
                observed_at: baseline.observed_at.clone(),
                source: "synthetic fixed-price fixture".into(),
            }],
            borrowed_proceeds_reserve_usd_micros: "0".into(),
            return_cost_reserve_usd_micros: "0".into(),
            planned_operations,
            unwind,
            route_proofs,
            asset_prices: vec![],
        }
    }

    fn transfer_constraint() -> EffectConstraint {
        EffectConstraint::Transfer {
            from: resource("binance-account"),
            to: resource("ethereum-wallet"),
            debit_asset: asset("binance", "ETH", 18),
            credit_asset: asset("ethereum:1", "native", 18),
            maximum_debit: quantity("1000000000000000000"),
            minimum_credit: quantity("990000000000000000"),
        }
    }

    fn transfer_effect() -> FinancialEffect {
        FinancialEffect::Transfer {
            from: resource("binance-account"),
            to: resource("ethereum-wallet"),
            debit: Amount {
                asset: asset("binance", "ETH", 18),
                atoms: quantity("1000000000000000000"),
            },
            credit: Amount {
                asset: asset("ethereum:1", "native", 18),
                atoms: quantity("995000000000000000"),
            },
        }
    }

    struct SyntheticAdapter {
        submissions: AtomicUsize,
        response: Mutex<Reconciliation>,
        include_valuation: bool,
    }

    impl SyntheticAdapter {
        fn new() -> Self {
            Self::new_with_valuation(true)
        }

        fn new_with_valuation(include_valuation: bool) -> Self {
            Self {
                submissions: AtomicUsize::new(0),
                response: Mutex::new(Reconciliation {
                    status: Status::Pending,
                    evidence: json!({"fixture":"synthetic pending"}),
                    verified: None,
                }),
                include_valuation,
            }
        }

        fn write_definition() -> Definition {
            Definition {
                name: "synthetic.transfer".into(),
                description: "Synthetic local transfer for engine tests.".into(),
                kind: OperationKind::Write,
                execution_actor: ExecutionActor::HumanInteractive,
                input_schema: json!({"type":"object"}),
                output_schema: json!({"type":"object"}),
                error_schema: json!({"type":"object"}),
                examples: vec![],
                coverage: "synthetic fixture".into(),
                completion: "verified synthetic receipt".into(),
                semantics: json!({}),
            }
        }

        fn completed_response(&self) {
            let mut verified = VerifiedEffects {
                evidence_version: 1,
                submission: Submission {
                    kind: "synthetic_tx".into(),
                    reference: "synthetic-tx-001".into(),
                    data: json!({"nonce":"synthetic-17"}),
                },
                effects: vec![transfer_effect()],
                evidence: json!({"fixture":"synthetic receipt"}),
                observed_at: now(),
                proof_digest: String::new(),
            };
            verified.proof_digest = verified_effect_digest(&verified).unwrap();
            *self.response.lock().unwrap() = Reconciliation {
                status: Status::Completed,
                evidence: verified.evidence.clone(),
                verified: Some(verified),
            };
        }
    }

    #[async_trait]
    impl Adapter for SyntheticAdapter {
        fn definitions(&self) -> Vec<Definition> {
            vec![Self::write_definition()]
        }

        async fn read(&self, _operation: &str, _input: &Value) -> Result<Value> {
            Ok(json!({}))
        }

        async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
            let at = chrono::Utc::now();
            Ok(PreparedOperation {
                operation: operation.into(),
                input: input.clone(),
                binding: json!({"source":"binance-account","target":"ethereum-wallet","amount":"1"}),
                summary: "synthetic transfer".into(),
                checks: vec![],
                warnings: vec![],
                observed_at: at.to_rfc3339(),
                expires_at: (at + chrono::Duration::minutes(2)).to_rfc3339(),
                valuation: self.include_valuation.then(|| fin_domain::Valuation {
                    movement_usd_micros: quantity(
                        input["movementMicros"].as_str().unwrap_or("1000000"),
                    ),
                    maximum_fee_usd_micros: quantity(input["feeMicros"].as_str().unwrap_or("0")),
                    additional_allocation_usd_micros: quantity(
                        input["borrowMicros"].as_str().unwrap_or("0"),
                    ),
                    observed_at: at.to_rfc3339(),
                    source: "synthetic fixed-price fixture".into(),
                    prices: vec![],
                }),
                intent: FinancialIntent {
                    debits: vec![ResourceAmount {
                        resource: resource("binance-account"),
                        amount: Amount {
                            asset: asset("binance", "ETH", 18),
                            atoms: quantity("1000000000000000000"),
                        },
                    }],
                    maximum_costs: vec![],
                    expected: vec![transfer_constraint()],
                    locks: vec![],
                },
                operation_version: 1,
            })
        }

        async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
            Ok(prepared.clone())
        }

        async fn submit(
            &self,
            _prepared: &PreparedOperation,
            journal: Arc<dyn SubmissionJournal>,
        ) -> Result<Submission> {
            let submission = Submission {
                kind: "synthetic_tx".into(),
                reference: "synthetic-tx-001".into(),
                data: json!({"nonce":"synthetic-17"}),
            };
            journal.assert_authorized()?;
            journal.persist_submission(&submission)?;
            self.submissions.fetch_add(1, Ordering::SeqCst);
            Ok(submission)
        }

        async fn reconcile(&self, _record: &OperationRecord) -> Result<Reconciliation> {
            Ok(self.response.lock().unwrap().clone())
        }
    }

    struct PersistThenErrorAdapter {
        inner: SyntheticAdapter,
        reconciliations: Mutex<VecDeque<Result<Reconciliation>>>,
        reconciliation_calls: AtomicUsize,
    }

    impl PersistThenErrorAdapter {
        fn new() -> Self {
            Self {
                inner: SyntheticAdapter::new(),
                reconciliations: Mutex::new(VecDeque::new()),
                reconciliation_calls: AtomicUsize::new(0),
            }
        }

        fn push_reconciliation(&self, result: Result<Reconciliation>) {
            self.reconciliations.lock().unwrap().push_back(result);
        }
    }

    #[async_trait]
    impl Adapter for PersistThenErrorAdapter {
        fn definitions(&self) -> Vec<Definition> {
            self.inner.definitions()
        }

        async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
            self.inner.read(operation, input).await
        }

        async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
            self.inner.prepare(operation, input).await
        }

        async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
            self.inner.revalidate(prepared).await
        }

        async fn submit(
            &self,
            prepared: &PreparedOperation,
            journal: Arc<dyn SubmissionJournal>,
        ) -> Result<Submission> {
            self.inner.submit(prepared, journal).await?;
            Err(FinError::new(
                "VENUE_REQUEST_FAILED",
                "venue",
                "Synthetic adapter reports an uncertain dispatch failure.",
                "Reconcile the persisted submission identity.",
            ))
        }

        async fn reconcile(&self, _record: &OperationRecord) -> Result<Reconciliation> {
            self.reconciliation_calls.fetch_add(1, Ordering::SeqCst);
            self.reconciliations
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    Err(FinError::new(
                        "SYNTHETIC_RECONCILIATION_EMPTY",
                        "reconciliation",
                        "No synthetic reconciliation outcome was queued.",
                        "Queue a synthetic result in the test.",
                    ))
                })
        }
    }

    struct SyntheticAuthorizer;

    #[async_trait]
    impl Authorizer for SyntheticAuthorizer {
        async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
            Ok(ApprovalAudit {
                mechanism: "synthetic-test-approval".into(),
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: now(),
                expires_at: challenge.expires_at.clone(),
            })
        }
    }

    #[derive(Default)]
    struct ChallengeValidatingAuthorizer {
        challenges: Mutex<Vec<AuthorizationChallenge>>,
    }

    #[async_trait]
    impl Authorizer for ChallengeValidatingAuthorizer {
        async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
            fin_authorization::validate_challenge(challenge, &now())?;
            self.challenges.lock().unwrap().push(challenge.clone());
            Ok(ApprovalAudit {
                mechanism: "synthetic-native-contract-validation".into(),
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: now(),
                expires_at: challenge.expires_at.clone(),
            })
        }
    }

    #[tokio::test]
    async fn persisted_submit_error_survives_proofless_reconciliation_until_terminal_proof() {
        let adapter = Arc::new(PersistThenErrorAdapter::new());
        let authorizer = Arc::new(ChallengeValidatingAuthorizer::default());
        let engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![adapter.clone()],
            authorizer.clone(),
            EngineOptions::default(),
        )
        .unwrap();
        let request = OperationRequest {
            request_id: "synthetic-persisted-submit-error".into(),
            operation: "synthetic.transfer".into(),
            input: json!({"amount":"1"}),
            run_id: None,
        };

        let initial = engine.execute(request.clone()).await.unwrap();
        assert_eq!(initial.status, Status::Unknown);
        assert_eq!(initial.error.as_ref().unwrap().code, "VENUE_REQUEST_FAILED");
        let original_submission = initial.submission.clone().unwrap();
        assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
        assert_eq!(authorizer.challenges.lock().unwrap().len(), 1);

        for (status, evidence) in [
            (Status::Pending, json!({"phase":"pending"})),
            (Status::Unknown, json!({"phase":"unknown"})),
        ] {
            adapter.push_reconciliation(Ok(Reconciliation {
                status: status.clone(),
                evidence: evidence.clone(),
                verified: None,
            }));
            let reconciled = engine.status(&initial.id, true).await.unwrap();
            assert_eq!(reconciled.status, status);
            assert_eq!(
                reconciled.error.as_ref().unwrap().code,
                "VENUE_REQUEST_FAILED"
            );
            assert_eq!(reconciled.submission.as_ref(), Some(&original_submission));
            assert_eq!(reconciled.last_reconciliation_evidence, Some(evidence));
        }

        adapter.push_reconciliation(Err(FinError::new(
            "SYNTHETIC_READ_TIMEOUT",
            "reconciliation",
            "Synthetic status read timed out.",
            "Retry read-only reconciliation.",
        )));
        let after_reconcile_error = engine.status(&initial.id, true).await.unwrap();
        assert_eq!(after_reconcile_error.status, Status::Unknown);
        assert_eq!(
            after_reconcile_error.error.as_ref().unwrap().code,
            "VENUE_REQUEST_FAILED"
        );
        assert_eq!(
            after_reconcile_error.submission.as_ref(),
            Some(&original_submission)
        );

        adapter.push_reconciliation(Ok(Reconciliation {
            status: Status::Completed,
            evidence: json!({"phase":"missing-proof"}),
            verified: None,
        }));
        let after_missing_proof = engine.status(&initial.id, true).await.unwrap();
        assert_eq!(after_missing_proof.status, Status::Unknown);
        assert_eq!(
            after_missing_proof.error.as_ref().unwrap().code,
            "VENUE_REQUEST_FAILED"
        );
        assert_eq!(
            after_missing_proof.last_reconciliation_evidence,
            Some(json!({"phase":"missing-proof"}))
        );

        adapter.inner.completed_response();
        let mut invalid_terminal = adapter.inner.response.lock().unwrap().clone();
        invalid_terminal.evidence = json!({"phase":"invalid-proof"});
        invalid_terminal.verified.as_mut().unwrap().proof_digest =
            "synthetic-invalid-proof-digest".into();
        adapter.push_reconciliation(Ok(invalid_terminal));
        let after_invalid_proof = engine.status(&initial.id, true).await.unwrap();
        assert_eq!(after_invalid_proof.status, Status::Unknown);
        assert_eq!(
            after_invalid_proof.error.as_ref().unwrap().code,
            "VENUE_REQUEST_FAILED"
        );
        assert_eq!(
            after_invalid_proof.last_reconciliation_evidence,
            Some(json!({"phase":"invalid-proof"}))
        );
        assert_eq!(
            after_invalid_proof.submission.as_ref(),
            Some(&original_submission)
        );

        let valid_terminal = adapter.inner.response.lock().unwrap().clone();
        adapter.push_reconciliation(Ok(valid_terminal));
        let settled = engine.status(&initial.id, true).await.unwrap();
        assert_eq!(settled.status, Status::Completed);
        assert!(settled.error.is_none());
        assert_eq!(settled.submission.as_ref(), Some(&original_submission));
        assert!(settled.effects.is_some());
        assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
        assert_eq!(adapter.reconciliation_calls.load(Ordering::SeqCst), 6);
        assert_eq!(authorizer.challenges.lock().unwrap().len(), 1);

        let duplicate = engine.execute(request).await.unwrap();
        assert_eq!(duplicate.id, initial.id);
        assert_eq!(duplicate.submission, Some(original_submission));
        assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
        assert_eq!(authorizer.challenges.lock().unwrap().len(), 1);
    }

    struct ChallengeAdapter {
        inner: SyntheticAdapter,
        preview_lifetime_seconds: i64,
        change_binding_on_revalidation: bool,
    }

    #[async_trait]
    impl Adapter for ChallengeAdapter {
        fn definitions(&self) -> Vec<Definition> {
            self.inner.definitions()
        }

        async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
            self.inner.read(operation, input).await
        }

        async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
            let mut prepared = self.inner.prepare(operation, input).await?;
            prepared.expires_at = (parse_time(&prepared.observed_at)?
                + chrono::Duration::seconds(self.preview_lifetime_seconds))
            .to_rfc3339();
            Ok(prepared)
        }

        async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
            let mut refreshed = prepared.clone();
            refreshed.observed_at = now();
            if self.change_binding_on_revalidation {
                refreshed.intent.debits[0].amount.atoms = quantity("2000000000000000000");
            }
            Ok(refreshed)
        }

        async fn submit(
            &self,
            prepared: &PreparedOperation,
            journal: Arc<dyn SubmissionJournal>,
        ) -> Result<Submission> {
            self.inner.submit(prepared, journal).await
        }

        async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
            self.inner.reconcile(record).await
        }
    }

    #[tokio::test]
    async fn financial_challenge_matches_real_native_contract_for_short_and_ethereum_previews() {
        let authorizer = Arc::new(ChallengeValidatingAuthorizer::default());
        for preview_lifetime_seconds in [30, 120] {
            let adapter = Arc::new(ChallengeAdapter {
                inner: SyntheticAdapter::new(),
                preview_lifetime_seconds,
                change_binding_on_revalidation: false,
            });
            let engine = Engine::new(
                Store::in_memory().unwrap(),
                vec![adapter.clone()],
                authorizer.clone(),
                EngineOptions::default(),
            )
            .unwrap();
            let request = OperationRequest {
                request_id: format!("synthetic-native-challenge-{preview_lifetime_seconds}"),
                operation: "synthetic.transfer".into(),
                input: json!({"amount":"1"}),
                run_id: None,
            };
            let record = engine.execute(request.clone()).await.unwrap();
            assert_eq!(record.status, Status::Pending, "{:?}", record.error);
            let approval = record.approval.as_ref().unwrap();
            let challenge = authorizer
                .challenges
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .clone();
            assert_eq!(challenge.purpose, "financial_write");
            assert_eq!(challenge.nonce.len(), 64);
            assert!(
                challenge
                    .nonce
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            );
            assert_eq!(approval.challenge_nonce, challenge.nonce);
            let prepared = record.preview.as_ref().unwrap();
            assert_eq!(challenge.binding, authorization_binding(prepared));
            let expiry = parse_time(&challenge.expires_at).unwrap();
            let preview_expiry = parse_time(&prepared.expires_at).unwrap();
            assert!(expiry <= preview_expiry);
            if preview_lifetime_seconds == 30 {
                assert_eq!(expiry, preview_expiry);
            } else {
                assert!(preview_expiry - expiry >= chrono::Duration::seconds(29));
                assert!(
                    expiry - parse_time(&approval.approved_at).unwrap()
                        <= chrono::Duration::seconds(90)
                );
            }
            let repeated = engine.execute(request).await.unwrap();
            assert_eq!(repeated.id, record.id);
            assert_eq!(repeated.submission, record.submission);
            assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
        }
        let challenges = authorizer.challenges.lock().unwrap();
        assert_eq!(
            challenges.len(),
            2,
            "duplicate requests must not request approval"
        );
        assert_ne!(challenges[0].nonce, challenges[1].nonce);
    }

    #[tokio::test]
    async fn financial_challenge_cannot_authorize_material_changes_on_revalidation() {
        let adapter = Arc::new(ChallengeAdapter {
            inner: SyntheticAdapter::new(),
            preview_lifetime_seconds: 120,
            change_binding_on_revalidation: true,
        });
        let authorizer = Arc::new(ChallengeValidatingAuthorizer::default());
        let engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![adapter.clone()],
            authorizer.clone(),
            EngineOptions::default(),
        )
        .unwrap();
        let record = engine
            .execute(OperationRequest {
                request_id: "synthetic-native-challenge-changed".into(),
                operation: "synthetic.transfer".into(),
                input: json!({"amount":"1"}),
                run_id: None,
            })
            .await
            .unwrap();
        assert_eq!(record.error.as_ref().unwrap().code, "REQUEST_CHANGED");
        assert!(record.submission.is_none());
        assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 0);
        assert_eq!(authorizer.challenges.lock().unwrap().len(), 1);
    }
    fn synthetic_engine(path: &std::path::Path, adapter: Arc<dyn Adapter>) -> Engine {
        Engine::new(
            Store::open(path).unwrap(),
            vec![adapter],
            Arc::new(SyntheticAuthorizer),
            EngineOptions::default(),
        )
        .unwrap()
    }

    fn empty_run_for_provenance(baseline: StateSnapshot) -> RunLedger {
        RunLedger {
            purpose: None,
            max_movement_usd_micros: legacy_movement_limit(),
            id: "synthetic-provenance".into(),
            status: RunStatus::Active,
            baseline,
            initial_allocation_usd_micros: "1000000".into(),
            borrowed_proceeds_reserve_usd_micros: "0".into(),
            return_cost_reserve_usd_micros: "1000".into(),
            planned_operations: vec![],
            unwind: vec![],
            route_proofs: vec![],
            asset_prices: vec![],
            settled_route_steps: vec![],
            route_invalidated: false,
            legacy_unwind_steps: vec![],
            legacy_return_routes_verified: false,
            allocations: vec![],
            available: vec![],
            reservations: BTreeMap::new(),
            costs: vec![],
            spent_cost_usd_micros: "0".into(),
            borrowed_allocation_used_usd_micros: "0".into(),
            final_state: None,
            final_observed_at: None,
            differences: BTreeMap::new(),
            provenance: vec![],
            provenance_events: vec![],
            supply_accruals: vec![],
            position_anchors: vec![],
            route_revisions: vec![],
            net_conversion_deltas: vec![],
        }
    }

    #[test]
    fn restoration_rejects_costs_mapped_outside_selected_allocations() {
        let selected_resource = resource("selected-wallet");
        let unrelated_resource = resource("unselected-wallet");
        let eth = asset("ethereum:1", "native", 18);
        let selected_holding = ResourceAmount {
            resource: selected_resource.clone(),
            amount: Amount {
                asset: eth.clone(),
                atoms: quantity("1000000000000000000"),
            },
        };
        let baseline = StateSnapshot {
            state: FinancialState {
                holdings: vec![selected_holding.clone()],
                positions: vec![],
                authorities: vec![],
                in_flight: vec![],
            },
            observed_at: "2026-01-01T00:00:00Z".into(),
            complete: true,
            coverage: vec![selected_resource.0.clone()],
            pending: vec![],
        };
        let mut run = empty_run_for_provenance(baseline);
        run.allocations.push(RunAllocation {
            holding: selected_holding,
            value_usd_micros: "1000000".into(),
            observed_at: "2026-01-01T00:00:00Z".into(),
            source: "synthetic selected origin".into(),
        });
        run.costs.push(ResourceAmount {
            resource: unrelated_resource,
            amount: Amount {
                asset: eth,
                atoms: quantity("1"),
            },
        });

        let (_, _, issues) = expected_restored_cash(&run).unwrap();
        assert!(issues.iter().any(|issue| {
            issue.contains("verified cost origin")
                && issue.contains("outside the selected run allocations")
        }));
    }

    #[test]
    fn failed_reconciliation_can_prove_empty_venue_rejection() {
        let record = record(vec![transfer_constraint()], vec![]);
        let verified = proof(&record, vec![]);
        assert_eq!(
            validate_effects(&record, &verified, &Status::Failed),
            Ok(())
        );
    }

    #[test]
    fn failed_reconciliation_can_prove_cost_without_claiming_success_effect() {
        let fee_asset = asset("ethereum:1", "native", 18);
        let fee_resource = resource("ethereum-wallet");
        let record = record(
            vec![transfer_constraint()],
            vec![CostBound {
                location: AccountingLocation::Cash {
                    resource: fee_resource.clone(),
                },
                amount: Amount {
                    asset: fee_asset.clone(),
                    atoms: quantity("500"),
                },
                treatment: CostTreatment::AdditionalDebit,
            }],
        );
        let verified = proof(
            &record,
            vec![FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: fee_resource,
                },
                amount: Amount {
                    asset: fee_asset,
                    atoms: quantity("300"),
                },
                category: "network_fee".into(),
                treatment: CostTreatment::AdditionalDebit,
            }],
        );
        assert_eq!(
            validate_effects(&record, &verified, &Status::Failed),
            Ok(())
        );
    }

    #[test]
    fn failed_reconciliation_rejects_claimed_success_transfer() {
        let record = record(vec![transfer_constraint()], vec![]);
        let verified = proof(&record, vec![transfer_effect()]);
        assert_eq!(
            validate_effects(&record, &verified, &Status::Failed)
                .unwrap_err()
                .code,
            "FAILED_EFFECT_MISMATCH"
        );
    }

    #[test]
    fn successful_custody_transfer_matches_distinct_debit_and_credit_assets() {
        let record = record(vec![transfer_constraint()], vec![]);
        let verified = proof(&record, vec![transfer_effect()]);
        assert_eq!(
            validate_effects(&record, &verified, &Status::Completed),
            Ok(())
        );
    }

    #[test]
    fn corrupted_proof_digest_is_not_terminal_evidence() {
        let record = record(vec![transfer_constraint()], vec![]);
        let mut verified = proof(&record, vec![transfer_effect()]);
        verified.proof_digest = "00".repeat(32);
        assert_eq!(
            validate_effects(&record, &verified, &Status::Completed)
                .unwrap_err()
                .code,
            "PROOF_DIGEST_MISMATCH"
        );
    }

    #[test]
    fn mismatched_approval_binding_cannot_settle_valid_receipt_effects() {
        let mut record = record(vec![transfer_constraint()], vec![]);
        record.approval.as_mut().unwrap().binding_hash = "00".repeat(32);
        let verified = proof(&record, vec![transfer_effect()]);
        assert_eq!(
            validate_effects(&record, &verified, &Status::Completed)
                .unwrap_err()
                .code,
            "APPROVAL_PROOF_MISMATCH"
        );
    }

    #[test]
    fn included_position_loss_uses_origin_link_without_cash_debit() {
        let position_resource = resource("ethereum-position");
        let cash_resource = resource("ethereum-wallet");
        let token = asset("ethereum:1", "USDC", 6);
        let position = AccountingLocation::Position {
            resource: position_resource.clone(),
            position: "aave:USDC:supply".into(),
            position_kind: PositionKind::Supply,
        };
        let cash = AccountingLocation::Cash {
            resource: cash_resource.clone(),
        };
        let intent = vec![
            EffectConstraint::OriginMovement {
                from: position.clone(),
                to: cash.clone(),
                debit_asset: token.clone(),
                credit_asset: token.clone(),
                maximum_debit: quantity("100000000"),
                minimum_credit: quantity("99500000"),
            },
            EffectConstraint::Position {
                resource: position_resource.clone(),
                position: "aave:USDC:supply".into(),
                asset: token.clone(),
                direction: Direction::Decrease,
                maximum_change: quantity("100000000"),
            },
            EffectConstraint::Credit {
                to: cash_resource.clone(),
                asset: token.clone(),
                minimum_credit: quantity("99500000"),
            },
        ];
        let mut record = record(
            intent,
            vec![CostBound {
                location: position.clone(),
                amount: Amount {
                    asset: token.clone(),
                    atoms: quantity("500000"),
                },
                treatment: CostTreatment::IncludedInDebit,
            }],
        );
        record.run_id = Some("synthetic-position-run".into());
        validate_cost_bound_contract(&record.preview.as_ref().unwrap().intent).unwrap();
        let anchor = PositionAnchor {
            resource: position_resource.clone(),
            position: "aave:USDC:supply".into(),
            position_kind: PositionKind::Supply,
            principal: Amount {
                asset: token.clone(),
                atoms: quantity("0"),
            },
            scaled_receipt: Some(quantity("0")),
            evidence_version: 1,
            evidence: json!({"fixture":"synthetic scaled receipt"}),
            observed_at: "2026-01-01T00:00:30Z".into(),
        };
        let verified = proof(
            &record,
            vec![
                FinancialEffect::OriginMovement {
                    from: position.clone(),
                    to: cash.clone(),
                    debit: Amount {
                        asset: token.clone(),
                        atoms: quantity("100000000"),
                    },
                    credit: Amount {
                        asset: token.clone(),
                        atoms: quantity("99500000"),
                    },
                },
                FinancialEffect::PositionChange {
                    resource: position_resource,
                    position: "aave:USDC:supply".into(),
                    asset: token.clone(),
                    delta: SignedQuantity::negative(&quantity("100000000")),
                    scaled_receipt: Some(quantity("0")),
                    anchor: Some(anchor),
                },
                FinancialEffect::UsableCredit {
                    to: cash_resource,
                    amount: Amount {
                        asset: token.clone(),
                        atoms: quantity("99500000"),
                    },
                },
                FinancialEffect::Cost {
                    location: position,
                    amount: Amount {
                        asset: token,
                        atoms: quantity("500000"),
                    },
                    category: "position_rounding_loss".into(),
                    treatment: CostTreatment::IncludedInDebit,
                },
            ],
        );
        let mut missing_anchor_effects = verified.effects.clone();
        if let FinancialEffect::PositionChange { anchor, .. } = &mut missing_anchor_effects[1] {
            *anchor = None;
        }
        let missing_anchor = proof(&record, missing_anchor_effects);
        assert_eq!(
            validate_effects(&record, &missing_anchor, &Status::Completed)
                .unwrap_err()
                .code,
            "POSITION_ANCHOR_MISSING"
        );
        assert_eq!(
            validate_effects(&record, &verified, &Status::Completed),
            Ok(())
        );
        assert!(
            actual_debits_and_costs(&verified.effects)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn cost_usd_uses_per_asset_exact_price_and_ceiling() {
        let asset = asset("ethereum:1", "synthetic-token", 6);
        let valuation = fin_domain::Valuation {
            movement_usd_micros: quantity("1000000"),
            maximum_fee_usd_micros: quantity("100"),
            additional_allocation_usd_micros: quantity("0"),
            observed_at: "2026-01-01T00:00:00Z".into(),
            source: "synthetic valuation".into(),
            prices: vec![fin_domain::AssetPrice {
                asset: asset.clone(),
                usd_micros_per_unit: quantity("2500000"),
                unit_atoms: quantity("1000000"),
                observed_at: "2026-01-01T00:00:00Z".into(),
                source: "synthetic price".into(),
            }],
        };
        let effect = FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: resource("synthetic-account"),
            },
            amount: Amount {
                asset,
                atoms: quantity("41"),
            },
            category: "synthetic_fee".into(),
            treatment: CostTreatment::AdditionalDebit,
        };
        assert_eq!(
            actual_cost_usd(&[effect], &valuation).unwrap(),
            BigUint::from(103u32)
        );
    }

    #[test]
    fn fresh_cost_valuation_requires_cost_assets_not_unrelated_allocation_assets() {
        let selected_asset = asset("binance", "ETH", 36);
        let native_fee_asset = asset("ethereum:1", "native", 18);
        let mut intent = record(vec![], vec![]).preview.unwrap().intent;
        intent.maximum_costs = vec![CostBound {
            location: AccountingLocation::Cash {
                resource: resource("selected-wallet"),
            },
            amount: Amount {
                asset: native_fee_asset.clone(),
                atoms: quantity("10"),
            },
            treatment: CostTreatment::AdditionalDebit,
        }];

        let required = prepared_cost_assets(&intent);
        assert_eq!(required, BTreeSet::from([native_fee_asset]));
        assert!(!required.contains(&selected_asset));
    }

    #[test]
    fn mixed_eth_and_usdc_costs_use_their_own_atomic_scales_and_prices() {
        let eth = asset("ethereum:1", "native", 18);
        let usdc = asset(
            "ethereum:1",
            "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
            6,
        );
        let valuation = fin_domain::Valuation {
            movement_usd_micros: quantity("100000000"),
            maximum_fee_usd_micros: quantity("3000000"),
            additional_allocation_usd_micros: quantity("0"),
            observed_at: "2026-01-01T00:00:00Z".into(),
            source: "synthetic mixed asset price fixture".into(),
            prices: vec![
                fin_domain::AssetPrice {
                    asset: eth.clone(),
                    usd_micros_per_unit: quantity("2000000000"),
                    unit_atoms: quantity("1000000000000000000"),
                    observed_at: "2026-01-01T00:00:00Z".into(),
                    source: "synthetic ETH/USD".into(),
                },
                fin_domain::AssetPrice {
                    asset: usdc.clone(),
                    usd_micros_per_unit: quantity("1000000"),
                    unit_atoms: quantity("1000000"),
                    observed_at: "2026-01-01T00:00:00Z".into(),
                    source: "synthetic USDC/USD".into(),
                },
            ],
        };
        let effects = vec![
            FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: resource("synthetic-wallet"),
                },
                amount: Amount {
                    asset: eth,
                    atoms: quantity("1000000000000000"),
                },
                category: "synthetic_gas".into(),
                treatment: CostTreatment::AdditionalDebit,
            },
            FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: resource("synthetic-custody"),
                },
                amount: Amount {
                    asset: usdc,
                    atoms: quantity("250000"),
                },
                category: "synthetic_venue_fee".into(),
                treatment: CostTreatment::AdditionalDebit,
            },
        ];
        assert_eq!(
            actual_cost_usd(&effects, &valuation).unwrap(),
            BigUint::from(2_250_000u32)
        );
    }

    #[test]
    fn cash_included_cost_is_reserved_and_consumed_only_once() {
        let resource = resource("synthetic-cash");
        let asset = asset("synthetic-network", "USD", 6);
        let debit = ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: asset.clone(),
                atoms: quantity("1000000"),
            },
        };
        let included = CostBound {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: asset.clone(),
                atoms: quantity("10000"),
            },
            treatment: CostTreatment::IncludedInDebit,
        };
        let intent = FinancialIntent {
            debits: vec![debit.clone()],
            maximum_costs: vec![included.clone()],
            expected: vec![],
            locks: vec![],
        };
        validate_cost_bound_contract(&intent).unwrap();
        let reserved = aggregate_resource_amounts(
            &cash_reservation_amounts(&intent.debits, &intent.maximum_costs).unwrap(),
        )
        .unwrap();
        assert_eq!(
            reserved.values().next().unwrap(),
            &BigUint::from(1_000_000u32)
        );
        let effects = vec![
            FinancialEffect::CashDebit {
                resource: resource.clone(),
                amount: Amount {
                    asset: asset.clone(),
                    atoms: quantity("1000000"),
                },
            },
            FinancialEffect::Cost {
                location: included.location,
                amount: included.amount,
                category: "included fee".into(),
                treatment: CostTreatment::IncludedInDebit,
            },
        ];
        assert_eq!(
            actual_debits_and_costs(&effects)
                .unwrap()
                .values()
                .next()
                .unwrap(),
            &BigUint::from(1_000_000u32)
        );
    }

    #[test]
    fn position_withdrawal_returns_principal_origin_less_included_loss() {
        let cash_resource = resource("allocation-account");
        let position_resource = resource("aave-position");
        let token = asset("ethereum:1", "USDC", 6);
        let cash = AccountingLocation::Cash {
            resource: cash_resource.clone(),
        };
        let position = AccountingLocation::Position {
            resource: position_resource.clone(),
            position: "aave:USDC:supply".into(),
            position_kind: PositionKind::Supply,
        };
        let principal = Amount {
            asset: token.clone(),
            atoms: quantity("100000000"),
        };
        let source_holding = ResourceAmount {
            resource: cash_resource.clone(),
            amount: principal.clone(),
        };
        let snapshot = StateSnapshot {
            state: FinancialState {
                holdings: vec![source_holding.clone()],
                ..Default::default()
            },
            observed_at: now(),
            complete: true,
            coverage: vec![cash_resource.0.clone(), position_resource.0.clone()],
            pending: vec![],
        };
        let mut run = empty_run_for_provenance(snapshot);
        let origin = format!(
            "allocation:{}",
            resource_asset_key(&source_holding).unwrap()
        );
        let anchor = PositionAnchor {
            resource: position_resource.clone(),
            position: "aave:USDC:supply".into(),
            position_kind: PositionKind::Supply,
            principal: principal.clone(),
            scaled_receipt: Some(quantity("100000000")),
            evidence_version: 1,
            evidence: json!({"fixture":"synthetic scaled receipt"}),
            observed_at: now(),
        };
        let first_used = vec![ProvenanceLot {
            origin: origin.clone(),
            location: cash.clone(),
            asset: token.clone(),
            atoms: "100000000".into(),
        }];
        let first = vec![
            FinancialEffect::OriginMovement {
                from: cash.clone(),
                to: position.clone(),
                debit: principal.clone(),
                credit: principal.clone(),
            },
            FinancialEffect::PositionChange {
                resource: position_resource.clone(),
                position: "aave:USDC:supply".into(),
                asset: token.clone(),
                delta: SignedQuantity::positive(&principal.atoms),
                scaled_receipt: Some(quantity("100000000")),
                anchor: Some(anchor.clone()),
            },
        ];
        apply_effect_provenance(
            &mut run,
            "synthetic-supply",
            &mut first_used.clone(),
            &mut vec![],
            &first,
            &fin_domain::Valuation {
                movement_usd_micros: quantity("1000000"),
                maximum_fee_usd_micros: quantity("0"),
                additional_allocation_usd_micros: quantity("0"),
                observed_at: now(),
                source: "synthetic".into(),
                prices: vec![],
            },
        )
        .unwrap();
        let withdraw = vec![
            FinancialEffect::OriginMovement {
                from: position.clone(),
                to: cash.clone(),
                debit: principal.clone(),
                credit: Amount {
                    asset: token.clone(),
                    atoms: quantity("99500000"),
                },
            },
            FinancialEffect::PositionChange {
                resource: position_resource.clone(),
                position: "aave:USDC:supply".into(),
                asset: token.clone(),
                delta: SignedQuantity::negative(&principal.atoms),
                scaled_receipt: Some(quantity("0")),
                anchor: Some(anchor),
            },
            FinancialEffect::UsableCredit {
                to: cash_resource.clone(),
                amount: Amount {
                    asset: token.clone(),
                    atoms: quantity("99500000"),
                },
            },
            FinancialEffect::Cost {
                location: position,
                amount: Amount {
                    asset: token.clone(),
                    atoms: quantity("500000"),
                },
                category: "withdrawal_rounding".into(),
                treatment: CostTreatment::IncludedInDebit,
            },
        ];
        let costs = apply_effect_provenance(
            &mut run,
            "synthetic-withdraw",
            &mut vec![],
            &mut vec![],
            &withdraw,
            &fin_domain::Valuation {
                movement_usd_micros: quantity("1000000"),
                maximum_fee_usd_micros: quantity("0"),
                additional_allocation_usd_micros: quantity("0"),
                observed_at: now(),
                source: "synthetic".into(),
                prices: vec![],
            },
        )
        .unwrap();
        run.costs = add_amounts(&run.costs, &costs).unwrap();
        assert_eq!(
            run.costs,
            vec![ResourceAmount {
                resource: cash_resource.clone(),
                amount: Amount {
                    asset: token.clone(),
                    atoms: quantity("500000")
                },
            }]
        );
        let restored = run
            .provenance
            .iter()
            .find(|lot| lot.location == cash && lot.asset == token)
            .unwrap();
        assert_eq!(restored.origin, origin);
        assert_eq!(restored.atoms, "99500000");
    }

    #[tokio::test]
    async fn restart_reconciles_original_submission_without_resubmitting() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("engine.sqlite3");
        let adapter = Arc::new(SyntheticAdapter::new());
        let request = OperationRequest {
            request_id: "synthetic-replay-01".into(),
            operation: "synthetic.transfer".into(),
            input: json!({"amount":"1"}),
            run_id: None,
        };
        let first = synthetic_engine(&path, adapter.clone());
        let submitted = first.execute(request.clone()).await.unwrap();
        assert_eq!(submitted.status, Status::Pending);
        assert_eq!(adapter.submissions.load(Ordering::SeqCst), 1);
        first.recover_interrupted().unwrap();
        drop(first);

        adapter.completed_response();
        let restarted = synthetic_engine(&path, adapter.clone());
        let completed = restarted.execute(request.clone()).await.unwrap();
        assert_eq!(completed.id, submitted.id);
        assert_eq!(completed.status, Status::Completed);
        assert_eq!(completed.submission, submitted.submission);
        assert_eq!(adapter.submissions.load(Ordering::SeqCst), 1);

        let conflicting = OperationRequest {
            input: json!({"amount":"2"}),
            ..request
        };
        assert_eq!(
            restarted.execute(conflicting).await.unwrap_err().code,
            "REQUEST_ID_CONFLICT"
        );
        assert_eq!(adapter.submissions.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn run_restore_requires_complete_matching_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let adapter: Arc<dyn Adapter> = Arc::new(SyntheticAdapter::new());
        let engine = synthetic_engine(&directory.path().join("run.sqlite3"), adapter);
        let instant = chrono::Utc::now();
        let observed_at = instant.to_rfc3339();
        let baseline = StateSnapshot {
            state: FinancialState {
                holdings: vec![ResourceAmount {
                    resource: resource("binance-account"),
                    amount: Amount {
                        asset: asset("binance", "ETH", 18),
                        atoms: quantity("1000000000000000000"),
                    },
                }],
                ..Default::default()
            },
            observed_at: observed_at.clone(),
            complete: true,
            coverage: vec!["binance-account".into(), "ethereum-wallet".into()],
            pending: vec![],
        };
        engine
            .begin_run(
                synthetic_run_plan("synthetic-run-restore", &baseline, "1000000"),
                instant,
            )
            .unwrap();
        let incomplete = StateSnapshot {
            complete: false,
            coverage: vec![],
            ..baseline.clone()
        };
        let pending = engine
            .finish_run("synthetic-run-restore", incomplete, instant)
            .unwrap();
        assert_eq!(pending.status, RunStatus::CleanupOutstanding);
        assert!(!pending.differences.is_empty());
        let restored = engine
            .finish_run("synthetic-run-restore", baseline.clone(), instant)
            .unwrap();
        assert_eq!(
            restored.status,
            RunStatus::Restored,
            "{:?}",
            restored.differences
        );
        assert!(restored.differences.is_empty());

        engine
            .begin_run(
                synthetic_run_plan("synthetic-run-excess-cost", &baseline, "1000000"),
                instant,
            )
            .unwrap();
        let mut over_costed: RunLedger = engine.load_run("synthetic-run-excess-cost").unwrap();
        over_costed.costs = vec![ResourceAmount {
            resource: baseline.state.holdings[0].resource.clone(),
            amount: Amount {
                asset: baseline.state.holdings[0].amount.asset.clone(),
                atoms: quantity("1000000000000000001"),
            },
        }];
        engine.save_run(&over_costed).unwrap();
        let over_spent = engine
            .finish_run("synthetic-run-excess-cost", baseline, instant)
            .unwrap();
        assert_eq!(over_spent.status, RunStatus::CleanupOutstanding);
        assert!(
            over_spent
                .differences
                .keys()
                .any(|key| key.starts_with("cost:"))
        );
    }

    #[test]
    fn final_projection_cannot_absorb_intermediate_cash_into_unselected_holdings() {
        let wallet = resource("selected-wallet");
        let native = asset("synthetic:1", "native", 18);
        let token = asset("synthetic:1", "token", 6);
        let holding = |asset, atoms| ResourceAmount {
            resource: wallet.clone(),
            amount: Amount {
                asset,
                atoms: quantity(atoms),
            },
        };
        let baseline = StateSnapshot {
            state: FinancialState {
                holdings: vec![holding(native.clone(), "100"), holding(token.clone(), "50")],
                ..Default::default()
            },
            observed_at: now(),
            complete: true,
            coverage: vec![wallet.0.clone()],
            pending: vec![],
        };
        let allocation = RunAllocation {
            holding: holding(native.clone(), "20"),
            value_usd_micros: "20".into(),
            observed_at: now(),
            source: "synthetic".into(),
        };
        let row = |asset, atoms| ProjectedCash {
            resource: wallet.clone(),
            asset,
            amount: QuantityInterval {
                minimum: quantity(atoms),
                maximum: quantity(atoms),
            },
        };
        let mut final_state = ProjectedState {
            cash: vec![row(native, "19"), row(token, "0")],
            ..Default::default()
        };
        assert!(
            validate_projected_restoration(
                &baseline,
                std::slice::from_ref(&allocation),
                &final_state
            )
            .is_ok()
        );
        final_state.cash[1].amount.maximum = quantity("1");
        assert_eq!(
            validate_projected_restoration(&baseline, &[allocation], &final_state)
                .unwrap_err()
                .stage,
            "budget.restoration.unallocated_asset"
        );
    }

    #[test]
    fn final_projection_requires_exact_position_and_authority_restoration() {
        let wallet = resource("selected-wallet");
        let token = asset("synthetic:1", "token", 6);
        let baseline = StateSnapshot {
            state: FinancialState {
                positions: vec![fin_domain::PositionState {
                    resource: wallet.clone(),
                    position: "position".into(),
                    kind: PositionKind::Supply,
                    amount: Amount {
                        asset: token.clone(),
                        atoms: quantity("0"),
                    },
                    scaled_receipt: None,
                    anchor: None,
                }],
                authorities: vec![fin_domain::AuthorityState {
                    resource: wallet,
                    authority: "spender".into(),
                    value: "0".into(),
                }],
                ..Default::default()
            },
            observed_at: now(),
            complete: true,
            coverage: vec!["selected-wallet".into()],
            pending: vec![],
        };
        let mut final_state = projected_state(&baseline).unwrap();
        assert!(validate_projected_restoration(&baseline, &[], &final_state).is_ok());
        final_state.positions[0].amount.maximum = quantity("1");
        assert_eq!(
            validate_projected_restoration(&baseline, &[], &final_state)
                .unwrap_err()
                .stage,
            "budget.restoration.position"
        );
        final_state.positions[0].amount.maximum = quantity("0");
        final_state.authorities[0].value = "1".into();
        assert_eq!(
            validate_projected_restoration(&baseline, &[], &final_state)
                .unwrap_err()
                .stage,
            "budget.restoration.authority"
        );
    }

    #[test]
    fn final_projection_rejects_duplicate_allocations_and_cash_intervals() {
        let wallet = resource("selected-wallet");
        let token = asset("synthetic:1", "token", 6);
        let holding = ResourceAmount {
            resource: wallet,
            amount: Amount {
                asset: token,
                atoms: quantity("20"),
            },
        };
        let baseline = StateSnapshot {
            state: FinancialState {
                holdings: vec![holding.clone()],
                ..Default::default()
            },
            observed_at: now(),
            complete: true,
            coverage: vec!["selected-wallet".into()],
            pending: vec![],
        };
        let allocation = RunAllocation {
            holding,
            value_usd_micros: "20".into(),
            observed_at: now(),
            source: "synthetic".into(),
        };
        let mut final_state = projected_state(&baseline).unwrap();
        assert_eq!(
            validate_projected_restoration(
                &baseline,
                &[allocation.clone(), allocation.clone()],
                &final_state
            )
            .unwrap_err()
            .stage,
            "budget.restoration.allocation"
        );
        final_state.cash.push(final_state.cash[0].clone());
        assert_eq!(
            validate_projected_restoration(&baseline, &[allocation], &final_state)
                .unwrap_err()
                .stage,
            "budget.restoration.cash_interval"
        );
    }

    #[test]
    fn stored_route_cannot_reuse_one_cost_reserve_for_multiple_steps() {
        let baseline = StateSnapshot {
            state: FinancialState {
                holdings: vec![ResourceAmount {
                    resource: resource("binance-account"),
                    amount: Amount {
                        asset: asset("binance", "ETH", 18),
                        atoms: quantity("1000000000000000000"),
                    },
                }],
                ..Default::default()
            },
            observed_at: now(),
            complete: true,
            coverage: vec!["binance-account".into(), "ethereum-wallet".into()],
            pending: vec![],
        };
        let mut plan = synthetic_run_plan("synthetic-cost-reuse", &baseline, "1000000");
        plan.return_cost_reserve_usd_micros = "3".into();
        plan.planned_operations[0].maximum_fee_usd_micros = "2".into();
        plan.unwind[0].maximum_fee_usd_micros = "2".into();
        let directory = tempfile::tempdir().unwrap();
        let engine = synthetic_engine(
            &directory.path().join("cost-reuse.sqlite3"),
            Arc::new(SyntheticAdapter::new()),
        );
        assert_eq!(
            engine.begin_run(plan, Utc::now()).unwrap_err().code,
            "RETURN_ROUTE_COST_BUDGET"
        );
        assert!(engine.operations().unwrap().is_empty());
    }

    #[test]
    fn route_return_prices_must_be_fresh_and_unambiguous_even_without_fee_assets() {
        let instant = Utc::now();
        let baseline = StateSnapshot {
            state: FinancialState {
                holdings: vec![ResourceAmount {
                    resource: resource("binance-account"),
                    amount: Amount {
                        asset: asset("binance", "ETH", 18),
                        atoms: quantity("1000000000000000000"),
                    },
                }],
                ..Default::default()
            },
            observed_at: instant.to_rfc3339(),
            complete: true,
            coverage: vec!["binance-account".into(), "ethereum-wallet".into()],
            pending: vec![],
        };
        let mut plan = synthetic_run_plan("synthetic-return-price", &baseline, "1000000");
        plan.asset_prices = vec![AssetPrice {
            asset: baseline.state.holdings[0].amount.asset.clone(),
            unit_atoms: quantity("1000000000000000000"),
            usd_micros_per_unit: quantity("1000000"),
            observed_at: (instant - chrono::Duration::seconds(121)).to_rfc3339(),
            source: "synthetic oracle".into(),
        }];
        assert_eq!(
            validate_route_plan(&plan, instant).unwrap_err().code,
            "COST_VALUATION_STALE"
        );
        plan.asset_prices[0].observed_at = instant.to_rfc3339();
        plan.asset_prices.push(plan.asset_prices[0].clone());
        assert_eq!(
            validate_route_plan(&plan, instant).unwrap_err().code,
            "COST_VALUATION_MISSING"
        );
    }

    #[test]
    fn run_cost_earmark_uses_selected_pool_once_and_requires_source_funding() {
        let instant = Utc::now();
        let baseline = StateSnapshot {
            state: FinancialState {
                holdings: vec![ResourceAmount {
                    resource: resource("binance-account"),
                    amount: Amount {
                        asset: asset("binance", "ETH", 18),
                        atoms: quantity("1000000000000000000"),
                    },
                }],
                ..Default::default()
            },
            observed_at: instant.to_rfc3339(),
            complete: true,
            coverage: vec!["binance-account".into()],
            pending: vec![],
        };

        let accepted_dir = tempfile::tempdir().unwrap();
        let accepted = synthetic_engine(
            &accepted_dir.path().join("cost-pool-accepted.sqlite3"),
            Arc::new(SyntheticAdapter::new()),
        );
        let mut selected_90 =
            synthetic_run_plan("selected-90-borrow-4-cost-10", &baseline, "90000000");
        selected_90.borrowed_proceeds_reserve_usd_micros = "4000000".into();
        selected_90.return_cost_reserve_usd_micros = "10000000".into();
        assert!(accepted.begin_run(selected_90, instant).is_ok());

        let over_cap_dir = tempfile::tempdir().unwrap();
        let over_cap = synthetic_engine(
            &over_cap_dir.path().join("cost-pool-over-cap.sqlite3"),
            Arc::new(SyntheticAdapter::new()),
        );
        let mut selected_97 = synthetic_run_plan("selected-97-borrow-4", &baseline, "97000000");
        selected_97.borrowed_proceeds_reserve_usd_micros = "4000000".into();
        selected_97.return_cost_reserve_usd_micros = "10000000".into();
        assert_eq!(
            over_cap.begin_run(selected_97, instant).unwrap_err().code,
            "ALLOCATION_LIMIT"
        );

        let unfunded_dir = tempfile::tempdir().unwrap();
        let unfunded = synthetic_engine(
            &unfunded_dir.path().join("cost-pool-unfunded.sqlite3"),
            Arc::new(SyntheticAdapter::new()),
        );
        let mut earmark_too_large =
            synthetic_run_plan("selected-90-cost-91", &baseline, "90000000");
        earmark_too_large.return_cost_reserve_usd_micros = "91000000".into();
        assert_eq!(
            unfunded
                .begin_run(earmark_too_large, instant)
                .unwrap_err()
                .code,
            "RETURN_COSTS_UNFUNDED"
        );
    }

    #[test]
    fn route_prefix_additional_costs_reduce_only_selected_projected_cash() {
        let wallet = resource("selected-wallet");
        let native = asset("ethereum:1", "native", 18);
        let proof_for_cost = |fee_atoms: &str| ReturnRouteProof {
            step_index: 0,
            operation: "synthetic.swap".into(),
            input_hash: "synthetic-input-hash".into(),
            observed_at: Utc::now().to_rfc3339(),
            expires_at: (Utc::now() + chrono::Duration::minutes(1)).to_rfc3339(),
            intent: FinancialIntent {
                debits: vec![],
                maximum_costs: vec![CostBound {
                    location: AccountingLocation::Cash {
                        resource: wallet.clone(),
                    },
                    amount: Amount {
                        asset: native.clone(),
                        atoms: quantity(fee_atoms),
                    },
                    treatment: CostTreatment::AdditionalDebit,
                }],
                expected: vec![transfer_constraint()],
                locks: vec![],
            },
            effects: vec![],
            requires: vec![],
            evidence: json!({"fixture":"synthetic route fee"}),
        };
        let state_for_cash = |atoms: &str| ProjectedState {
            cash: vec![ProjectedCash {
                resource: wallet.clone(),
                asset: native.clone(),
                amount: QuantityInterval {
                    minimum: quantity(atoms),
                    maximum: quantity(atoms),
                },
            }],
            ..Default::default()
        };

        let mut affordable = proof_for_cost("6");
        include_costs_in_projected_cash(&mut affordable).unwrap();
        let projected =
            apply_projected_effects(&state_for_cash("10"), &affordable.effects, 0).unwrap();
        assert_eq!(projected.cash[0].amount.minimum.as_str(), "4");
        assert_eq!(projected.cash[0].amount.maximum.as_str(), "10");

        let mut unfunded = proof_for_cost("6");
        include_costs_in_projected_cash(&mut unfunded).unwrap();
        assert!(apply_projected_effects(&state_for_cash("5"), &unfunded.effects, 0).is_err());
    }

    #[test]
    fn conversion_round_trip_restoration_tracks_exact_origin_loss_and_gain() {
        let wallet = resource("conversion-wallet");
        let eth = asset("ethereum:1", "ETH", 18);
        let usdc = asset("ethereum:1", "USDC", 6);
        let eth_holding = ResourceAmount {
            resource: wallet.clone(),
            amount: Amount {
                asset: eth.clone(),
                atoms: quantity("1000000000000000000"),
            },
        };
        let baseline = StateSnapshot {
            state: fin_domain::FinancialState {
                holdings: vec![eth_holding.clone()],
                ..Default::default()
            },
            observed_at: Utc::now().to_rfc3339(),
            complete: true,
            coverage: vec![wallet.0.clone()],
            pending: vec![],
        };
        let valuation = fin_domain::Valuation {
            movement_usd_micros: quantity("0"),
            maximum_fee_usd_micros: quantity("0"),
            additional_allocation_usd_micros: quantity("0"),
            observed_at: baseline.observed_at.clone(),
            source: "synthetic conversion fixture".into(),
            prices: vec![],
        };

        for (returned_atoms, expected_delta) in [
            ("990000000000000000", "-10000000000000000"),
            ("1010000000000000000", "10000000000000000"),
        ] {
            let mut run = empty_run_for_provenance(baseline.clone());
            run.allocations.push(RunAllocation {
                holding: eth_holding.clone(),
                value_usd_micros: "100000000".into(),
                observed_at: run.baseline.observed_at.clone(),
                source: "synthetic selected allocation".into(),
            });
            run.provenance.push(ProvenanceLot {
                origin: format!("allocation:{}", resource_asset_key(&eth_holding).unwrap()),
                location: AccountingLocation::Cash {
                    resource: wallet.clone(),
                },
                asset: eth.clone(),
                atoms: "1000000000000000000".into(),
            });
            let location = AccountingLocation::Cash {
                resource: wallet.clone(),
            };
            let mut forward_lots = take_provenance(
                &mut run.provenance,
                &location,
                &eth,
                &BigUint::from(1_000_000_000_000_000_000u64),
            )
            .unwrap();
            apply_effect_provenance(
                &mut run,
                "synthetic-forward-swap",
                &mut forward_lots,
                &mut vec![],
                &[FinancialEffect::Conversion {
                    resource: wallet.clone(),
                    spent: Amount {
                        asset: eth.clone(),
                        atoms: quantity("1000000000000000000"),
                    },
                    received: Amount {
                        asset: usdc.clone(),
                        atoms: quantity("200000000"),
                    },
                }],
                &valuation,
            )
            .unwrap();
            let mut reverse_lots = take_provenance(
                &mut run.provenance,
                &location,
                &usdc,
                &BigUint::from(200_000_000u64),
            )
            .unwrap();
            apply_effect_provenance(
                &mut run,
                "synthetic-return-swap",
                &mut reverse_lots,
                &mut vec![],
                &[FinancialEffect::Conversion {
                    resource: wallet.clone(),
                    spent: Amount {
                        asset: usdc.clone(),
                        atoms: quantity("200000000"),
                    },
                    received: Amount {
                        asset: eth.clone(),
                        atoms: quantity(returned_atoms),
                    },
                }],
                &valuation,
            )
            .unwrap();

            let (expected, deltas, issues) = expected_restored_cash(&run).unwrap();
            assert!(issues.is_empty(), "{issues:?}");
            let returned = parse_uint(returned_atoms).unwrap();
            assert_eq!(
                expected.get(&resource_asset_key(&eth_holding).unwrap()),
                Some(&returned)
            );
            assert_eq!(deltas[0].atoms.as_str(), expected_delta);
        }
    }

    #[test]
    fn custody_return_selects_exact_matching_selected_origin_before_fifo() {
        let source = AccountingLocation::Cash {
            resource: resource("intermediate-main"),
        };
        let child_a = resource("binance-child-a");
        let child_b = resource("binance-child-b");
        let asset = asset("binance", "USDT", 6);
        let allocation_origin = |account: &ResourceId| {
            let holding = ResourceAmount {
                resource: account.clone(),
                amount: Amount {
                    asset: asset.clone(),
                    atoms: quantity("5000000"),
                },
            };
            format!("allocation:{}", resource_asset_key(&holding).unwrap())
        };
        let mut lots = vec![
            ProvenanceLot {
                origin: allocation_origin(&child_a),
                location: source.clone(),
                asset: asset.clone(),
                atoms: "5000000".into(),
            },
            ProvenanceLot {
                origin: allocation_origin(&child_b),
                location: source.clone(),
                asset: asset.clone(),
                atoms: "5000000".into(),
            },
        ];
        let selected = take_provenance_for_transfer(
            &mut lots,
            &source,
            &asset,
            &BigUint::from(5_000_000u64),
            &child_b,
            &asset,
        )
        .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].origin, allocation_origin(&child_b));
        assert_eq!(lots.len(), 1);
        assert_eq!(lots[0].origin, allocation_origin(&child_a));
    }

    #[tokio::test]
    async fn run_keeps_origin_through_cross_custody_transfer_and_requires_return() {
        let directory = tempfile::tempdir().unwrap();
        let adapter = Arc::new(SyntheticAdapter::new_with_valuation(true));
        let engine = synthetic_engine(
            &directory.path().join("origin-run.sqlite3"),
            adapter.clone(),
        );
        let instant = chrono::Utc::now();
        let observed_at = instant.to_rfc3339();
        let source = resource("binance-account");
        let target = resource("ethereum-wallet");
        let source_amount = ResourceAmount {
            resource: source.clone(),
            amount: Amount {
                asset: asset("binance", "ETH", 18),
                atoms: quantity("1000000000000000000"),
            },
        };
        let baseline = StateSnapshot {
            state: FinancialState {
                holdings: vec![source_amount.clone()],
                ..Default::default()
            },
            observed_at: observed_at.clone(),
            complete: true,
            coverage: vec![source.0.clone(), target.0.clone()],
            pending: vec![],
        };
        engine
            .begin_run(
                synthetic_run_plan("synthetic-origin-run", &baseline, "1000000"),
                instant,
            )
            .unwrap();
        adapter.completed_response();
        let record = engine
            .execute(OperationRequest {
                request_id: "synthetic-origin-transfer".into(),
                operation: "synthetic.transfer".into(),
                input: json!({"amount":"1"}),
                run_id: Some("synthetic-origin-run".into()),
            })
            .await
            .unwrap();
        assert_eq!(record.status, Status::Completed);
        let run: RunLedger = engine.load_run("synthetic-origin-run").unwrap();
        assert_eq!(run.provenance_events.len(), 1);
        assert_eq!(
            run.provenance_events[0].from,
            AccountingLocation::Cash {
                resource: source.clone()
            }
        );
        assert_eq!(
            run.provenance_events[0].to,
            AccountingLocation::Cash {
                resource: target.clone()
            }
        );
        assert_eq!(run.provenance_events[0].origins.len(), 1);
        assert!(run.provenance_events[0].origins[0].starts_with("allocation:"));
        let final_state = StateSnapshot {
            state: FinancialState {
                holdings: vec![ResourceAmount {
                    resource: target,
                    amount: Amount {
                        asset: asset("ethereum:1", "native", 18),
                        atoms: quantity("995000000000000000"),
                    },
                }],
                ..Default::default()
            },
            observed_at: now(),
            complete: true,
            coverage: baseline.coverage.clone(),
            pending: vec![],
        };
        assert_eq!(
            engine
                .finish_run("synthetic-origin-run", final_state, chrono::Utc::now())
                .unwrap()
                .status,
            RunStatus::CleanupOutstanding
        );
    }

    fn position_anchor(
        resource: &str,
        position: &str,
        kind: PositionKind,
        atoms: &str,
    ) -> PositionAnchor {
        PositionAnchor {
            resource: ResourceId(resource.to_owned()),
            position: position.into(),
            position_kind: kind,
            principal: Amount {
                asset: asset("ethereum:1", "USDC", 6),
                atoms: quantity(atoms),
            },
            scaled_receipt: None,
            evidence_version: 1,
            evidence: json!({"fixture":"synthetic-scaled-position-proof"}),
            observed_at: "2026-01-01T00:00:25Z".into(),
        }
    }

    #[test]
    fn lending_anchor_transition_includes_accrual_without_erasing_principal_links() {
        let record = record(vec![], vec![]);
        let cash = resource("wallet");
        let pool = resource("aave-market");
        let usdc = asset("ethereum:1", "USDC", 6);
        let supply = AccountingLocation::Position {
            resource: pool.clone(),
            position: "aUSDC".into(),
            position_kind: PositionKind::Supply,
        };
        let variable_debt = AccountingLocation::Position {
            resource: pool.clone(),
            position: "variableDebtUSDC".into(),
            position_kind: PositionKind::VariableDebt,
        };
        let cases = [
            (
                PositionKind::Supply,
                "aUSDC",
                "100",
                "201",
                "2",
                "99",
                FinancialEffect::CashDebit {
                    resource: cash.clone(),
                    amount: Amount {
                        asset: usdc.clone(),
                        atoms: quantity("100"),
                    },
                },
                FinancialEffect::PositionChange {
                    resource: pool.clone(),
                    position: "aUSDC".into(),
                    asset: usdc.clone(),
                    delta: SignedQuantity::new("99").unwrap(),
                    scaled_receipt: None,
                    anchor: Some(position_anchor(
                        "aave-market",
                        "aUSDC",
                        PositionKind::Supply,
                        "201",
                    )),
                },
                AccountingLocation::Cash {
                    resource: cash.clone(),
                },
                supply.clone(),
                "100",
                "99",
            ),
            (
                PositionKind::VariableDebt,
                "variableDebtUSDC",
                "100",
                "153",
                "3",
                "50",
                FinancialEffect::DebtChange {
                    resource: pool.clone(),
                    debt: "variableDebtUSDC".into(),
                    asset: usdc.clone(),
                    delta: SignedQuantity::new("50").unwrap(),
                    anchor: Some(position_anchor(
                        "aave-market",
                        "variableDebtUSDC",
                        PositionKind::VariableDebt,
                        "153",
                    )),
                },
                FinancialEffect::UsableCredit {
                    to: cash.clone(),
                    amount: Amount {
                        asset: usdc.clone(),
                        atoms: quantity("50"),
                    },
                },
                variable_debt.clone(),
                AccountingLocation::Cash {
                    resource: cash.clone(),
                },
                "50",
                "50",
            ),
            (
                PositionKind::VariableDebt,
                "variableDebtUSDC",
                "153",
                "100",
                "4",
                "-57",
                FinancialEffect::DebtChange {
                    resource: pool.clone(),
                    debt: "variableDebtUSDC".into(),
                    asset: usdc.clone(),
                    delta: SignedQuantity::new("-57").unwrap(),
                    anchor: Some(position_anchor(
                        "aave-market",
                        "variableDebtUSDC",
                        PositionKind::VariableDebt,
                        "100",
                    )),
                },
                FinancialEffect::CashDebit {
                    resource: cash.clone(),
                    amount: Amount {
                        asset: usdc.clone(),
                        atoms: quantity("61"),
                    },
                },
                AccountingLocation::Cash {
                    resource: cash.clone(),
                },
                variable_debt,
                "61",
                "57",
            ),
        ];

        for (
            kind,
            identity,
            old,
            post,
            accrual,
            change,
            principal_leg,
            opposite_leg,
            from,
            to,
            debit,
            credit,
        ) in cases
        {
            let anchor = position_anchor("aave-market", identity, kind.clone(), old);
            let (resource_name, position_name) = if identity == "aUSDC" {
                ("aave-market", "aUSDC")
            } else {
                ("aave-market", identity)
            };
            let accrual = FinancialEffect::Accrual {
                resource: resource(resource_name),
                position: position_name.into(),
                asset: usdc.clone(),
                position_kind: kind,
                delta: SignedQuantity::new(accrual).unwrap(),
            };
            let movement = FinancialEffect::OriginMovement {
                from: from.clone(),
                to: to.clone(),
                debit: Amount {
                    asset: usdc.clone(),
                    atoms: quantity(debit),
                },
                credit: Amount {
                    asset: usdc.clone(),
                    atoms: quantity(credit),
                },
            };
            let effects = vec![accrual, principal_leg, opposite_leg, movement];
            let verified = proof(&record, effects);
            validate_anchor_transitions(
                &verified,
                &PreparationContext {
                    positions: vec![anchor],
                },
            )
            .unwrap();
            validate_origin_links(&verified.effects).unwrap();

            let mut invalid = verified.clone();
            for effect in &mut invalid.effects {
                match effect {
                    FinancialEffect::PositionChange {
                        anchor: Some(anchor),
                        ..
                    }
                    | FinancialEffect::DebtChange {
                        anchor: Some(anchor),
                        ..
                    } => {
                        anchor.principal.atoms =
                            quantity(&post.parse::<u64>().unwrap().saturating_add(1).to_string());
                    }
                    _ => {}
                }
            }
            assert!(
                validate_anchor_transitions(
                    &invalid,
                    &PreparationContext {
                        positions: vec![position_anchor(
                            "aave-market",
                            identity,
                            if identity == "aUSDC" {
                                PositionKind::Supply
                            } else {
                                PositionKind::VariableDebt
                            },
                            old
                        )]
                    },
                )
                .is_err()
            );
            let _ = change;
        }
    }

    #[test]
    fn planned_route_inputs_and_financial_bounds_are_exactly_fenced() {
        let (forward, unwind, proofs) = synthetic_route_plan();
        let mut plan = RunPlan {
            id: "synthetic-route-fence".into(),
            baseline: StateSnapshot {
                state: FinancialState {
                    holdings: vec![ResourceAmount {
                        resource: resource("binance-account"),
                        amount: Amount {
                            asset: asset("binance", "ETH", 18),
                            atoms: quantity("1000000000000000000"),
                        },
                    }],
                    ..Default::default()
                },
                observed_at: now(),
                complete: true,
                coverage: vec!["synthetic".into()],
                pending: vec![],
            },
            allocations: vec![RunAllocation {
                holding: ResourceAmount {
                    resource: resource("binance-account"),
                    amount: Amount {
                        asset: asset("binance", "ETH", 18),
                        atoms: quantity("1000000000000000000"),
                    },
                },
                value_usd_micros: "1000000".into(),
                observed_at: now(),
                source: "synthetic fixture".into(),
            }],
            borrowed_proceeds_reserve_usd_micros: "0".into(),
            return_cost_reserve_usd_micros: "0".into(),
            planned_operations: forward,
            unwind,
            route_proofs: proofs,
            asset_prices: vec![],
        };
        validate_route_plan(&plan, Utc::now()).unwrap();
        plan.route_proofs[0].input_hash =
            binding_hash(&json!({"amount":"attacker-change"})).unwrap();
        assert_eq!(
            validate_route_plan(&plan, Utc::now()).unwrap_err().code,
            "RETURN_ROUTE_PROOF_INVALID"
        );

        let approved = record(vec![transfer_constraint()], vec![])
            .preview
            .unwrap()
            .intent;
        let mut tighter = approved.clone();
        tighter.debits[0].amount.atoms = quantity("500000000000000000");
        assert!(intent_within(&tighter, &approved).unwrap());
        let mut overdraw = approved.clone();
        overdraw.debits[0].amount.atoms = quantity("1000000000000000001");
        assert!(!intent_within(&overdraw, &approved).unwrap());
        let mut changed_effect = approved.clone();
        changed_effect.expected.clear();
        assert!(!intent_within(&changed_effect, &approved).unwrap());

        let projected = ProjectedState::default();
        assert!(
            apply_projected_effects(
                &projected,
                &[ProjectedEffect::CashDelta {
                    resource: resource("synthetic"),
                    asset: asset("synthetic", "USD", 6),
                    minimum_delta: SignedQuantity::new("-1").unwrap(),
                    maximum_delta: SignedQuantity::new("-1").unwrap(),
                }],
                0,
            )
            .is_err()
        );

        let mut source = synthetic_route_plan().2[0].clone();
        source.effects = vec![ProjectedEffect::CashDelta {
            resource: resource("ethereum-wallet"),
            asset: asset("ethereum:1", "USDC", 6),
            minimum_delta: SignedQuantity::new("10000000").unwrap(),
            maximum_delta: SignedQuantity::new("20000000").unwrap(),
        }];
        let projected =
            apply_projected_effects(&ProjectedState::default(), &source.effects, 0).unwrap();
        let mut cleanup = synthetic_route_plan().2[1].clone();
        cleanup.requires.push(ProjectedRequirement::CashAtLeast {
            resource: resource("ethereum-wallet"),
            asset: asset("ethereum:1", "USDC", 6),
            amount: quantity("10000000"),
            source_step: Some(0),
        });
        cleanup.effects.push(ProjectedEffect::CashSweep {
            resource: resource("ethereum-wallet"),
            asset: asset("ethereum:1", "USDC", 6),
            maximum_amount: quantity("10000000"),
            source_step: 0,
        });
        validate_projected_requirements(&projected, &cleanup, &[source.clone()], 1, &[], 0)
            .unwrap();

        let mut second_source = source.clone();
        second_source.effects = vec![ProjectedEffect::CashDelta {
            resource: resource("ethereum-wallet"),
            asset: asset("ethereum:1", "USDC", 6),
            minimum_delta: SignedQuantity::new("10000000").unwrap(),
            maximum_delta: SignedQuantity::new("20000000").unwrap(),
        }];
        let after_second = apply_projected_effects(&projected, &second_source.effects, 1).unwrap();
        let mut aggregate_cleanup = cleanup.clone();
        aggregate_cleanup.requires[0] = ProjectedRequirement::CashAtLeast {
            resource: resource("ethereum-wallet"),
            asset: asset("ethereum:1", "USDC", 6),
            amount: quantity("20000000"),
            source_step: Some(1),
        };
        aggregate_cleanup.effects[0] = ProjectedEffect::CashSweep {
            resource: resource("ethereum-wallet"),
            asset: asset("ethereum:1", "USDC", 6),
            maximum_amount: quantity("40000000"),
            source_step: 1,
        };
        aggregate_cleanup.effects = vec![aggregate_cleanup.effects[0].clone()];
        validate_projected_requirements(
            &after_second,
            &aggregate_cleanup,
            &[source.clone(), second_source],
            2,
            &[],
            0,
        )
        .unwrap();

        let mut wrong_source = cleanup;
        wrong_source.requires[0] = ProjectedRequirement::CashAtLeast {
            resource: resource("another-wallet"),
            asset: asset("ethereum:1", "USDC", 6),
            amount: quantity("10000000"),
            source_step: Some(0),
        };
        assert!(
            validate_projected_requirements(&projected, &wrong_source, &[source], 1, &[], 0)
                .is_err()
        );
    }

    #[test]
    fn route_proof_validation_uses_post_await_clock_without_relaxing_freshness() {
        let (forward, _, mut proofs) = synthetic_route_plan();
        let request_started = Utc::now() - chrono::Duration::seconds(20);
        let after_request = current_validation_time(request_started);
        assert!(after_request > request_started);
        proofs[0].observed_at = (request_started + chrono::Duration::seconds(1)).to_rfc3339();
        assert!(validate_route_proof(&forward[0], &proofs[0], 0, after_request).is_ok());

        let stale = after_request - chrono::Duration::seconds(MAX_OBSERVATION_AGE_SECONDS + 1);
        proofs[0].observed_at = stale.to_rfc3339();
        assert_eq!(
            validate_route_proof(&forward[0], &proofs[0], 0, after_request)
                .unwrap_err()
                .code,
            "RETURN_ROUTE_STALE"
        );

        proofs[0].observed_at = (after_request + chrono::Duration::seconds(10)).to_rfc3339();
        assert_eq!(
            validate_route_proof(&forward[0], &proofs[0], 0, after_request)
                .unwrap_err()
                .code,
            "RETURN_ROUTE_STALE"
        );
    }

    #[test]
    fn cash_requirement_source_aggregates_selected_cash_and_prior_net_credits() {
        let wallet = resource("selected-wallet");
        let token = asset("ethereum:1", "USDC", 6);
        let selected = ResourceAmount {
            resource: wallet.clone(),
            amount: Amount {
                asset: token.clone(),
                atoms: quantity("5"),
            },
        };
        let cash_delta = |delta: &str| ProjectedEffect::CashDelta {
            resource: wallet.clone(),
            asset: token.clone(),
            minimum_delta: SignedQuantity::new(delta).unwrap(),
            maximum_delta: SignedQuantity::new(delta).unwrap(),
        };
        let mut prior = synthetic_route_plan().2;
        prior[0].effects = vec![cash_delta("4")];
        prior[1].effects = vec![cash_delta("3"), cash_delta("-2")];
        let starting = ProjectedState {
            cash: vec![ProjectedCash {
                resource: wallet.clone(),
                asset: token.clone(),
                amount: QuantityInterval {
                    minimum: quantity("5"),
                    maximum: quantity("5"),
                },
            }],
            ..ProjectedState::default()
        };
        let after_first = apply_projected_effects(&starting, &prior[0].effects, 0).unwrap();
        let after_second = apply_projected_effects(&after_first, &prior[1].effects, 1).unwrap();
        let mut cleanup = prior[1].clone();
        cleanup.requires = vec![ProjectedRequirement::CashAtLeast {
            resource: wallet.clone(),
            asset: token.clone(),
            amount: quantity("10"),
            source_step: Some(1),
        }];
        cleanup.effects.clear();
        validate_projected_requirements(
            &after_second,
            &cleanup,
            &prior,
            2,
            std::slice::from_ref(&selected),
            0,
        )
        .unwrap();
        assert!(
            !cash_requirement_source_matches(
                &wallet,
                &token,
                BigUint::from(11u8),
                1,
                &prior,
                std::slice::from_ref(&selected),
                0,
            )
            .unwrap()
        );

        let unrelated = resource("unselected-wallet");
        prior[0].effects.push(ProjectedEffect::CashDelta {
            resource: unrelated,
            asset: token.clone(),
            minimum_delta: SignedQuantity::new("1000").unwrap(),
            maximum_delta: SignedQuantity::new("1000").unwrap(),
        });
        assert!(
            !cash_requirement_source_matches(
                &wallet,
                &token,
                BigUint::from(10u8),
                1,
                &prior,
                &[],
                0,
            )
            .unwrap()
        );

        prior[1].effects = vec![
            ProjectedEffect::CashDelta {
                resource: wallet.clone(),
                asset: token.clone(),
                minimum_delta: SignedQuantity::new("0").unwrap(),
                maximum_delta: SignedQuantity::new("3").unwrap(),
            },
            cash_delta("-2"),
        ];
        let uncertain_source_state =
            apply_projected_effects(&after_first, &prior[1].effects, 1).unwrap();
        cleanup.requires[0] = ProjectedRequirement::CashAtLeast {
            resource: wallet.clone(),
            asset: token.clone(),
            amount: quantity("7"),
            source_step: Some(1),
        };
        assert!(
            validate_projected_requirements(
                &uncertain_source_state,
                &cleanup,
                &prior,
                2,
                std::slice::from_ref(&selected),
                0,
            )
            .is_ok()
        );
        assert!(
            !cash_requirement_source_matches(
                &wallet,
                &token,
                BigUint::from(8u8),
                1,
                &prior,
                std::slice::from_ref(&selected),
                0,
            )
            .unwrap()
        );
    }

    #[test]
    fn replan_can_anchor_current_cash_to_settled_source_without_double_counting_it() {
        let wallet = resource("selected-wallet");
        let token = asset("ethereum:1", "USDC", 6);
        let settled_source = ProjectedEffect::CashDelta {
            resource: wallet.clone(),
            asset: token.clone(),
            minimum_delta: SignedQuantity::new("5").unwrap(),
            maximum_delta: SignedQuantity::new("10").unwrap(),
        };
        let mut prior = synthetic_route_plan().2;
        prior[0].effects = vec![settled_source];
        let current_available = [ResourceAmount {
            resource: wallet.clone(),
            amount: Amount {
                asset: token.clone(),
                atoms: quantity("7"),
            },
        }];

        assert!(
            cash_requirement_source_matches(
                &wallet,
                &token,
                BigUint::from(7u8),
                0,
                &prior,
                &current_available,
                1,
            )
            .unwrap()
        );
        assert!(
            !cash_requirement_source_matches(
                &wallet,
                &token,
                BigUint::from(8u8),
                0,
                &prior,
                &current_available,
                1,
            )
            .unwrap()
        );
        assert!(
            !cash_requirement_source_matches(
                &wallet,
                &asset("ethereum:1", "WETH", 18),
                BigUint::from(1u8),
                0,
                &prior,
                &current_available,
                1,
            )
            .unwrap()
        );
        assert!(
            !cash_requirement_source_matches(
                &wallet,
                &token,
                BigUint::from(1u8),
                9,
                &prior,
                &current_available,
                1,
            )
            .unwrap()
        );
    }

    #[test]
    fn route_projection_mismatch_is_detectable_from_truthful_credit_effects() {
        let recipient = resource("ethereum-wallet");
        let token = asset("ethereum:1", "USDC", 6);
        let proof = ReturnRouteProof {
            step_index: 0,
            operation: "synthetic.transfer".into(),
            input_hash: "unused-in-this-unit-test".into(),
            observed_at: now(),
            expires_at: (Utc::now() + chrono::Duration::minutes(2)).to_rfc3339(),
            intent: FinancialIntent::default(),
            effects: vec![ProjectedEffect::CashDelta {
                resource: recipient.clone(),
                asset: token.clone(),
                minimum_delta: SignedQuantity::positive(&quantity("1")),
                maximum_delta: SignedQuantity::positive(&quantity("100")),
            }],
            requires: vec![],
            evidence: json!({"fixture":"finite conditional output"}),
        };
        let receipt = |atoms: &str| VerifiedEffects {
            evidence_version: 1,
            submission: Submission {
                kind: "synthetic".into(),
                reference: format!("receipt-{atoms}"),
                data: json!({}),
            },
            effects: vec![FinancialEffect::UsableCredit {
                to: recipient.clone(),
                amount: Amount {
                    asset: token.clone(),
                    atoms: quantity(atoms),
                },
            }],
            evidence: json!({"fixture":"verified receipt"}),
            observed_at: now(),
            proof_digest: "synthetic-test-only".into(),
        };
        assert!(route_projection_matches_receipt(&proof, &receipt("100")).unwrap());
        assert!(!route_projection_matches_receipt(&proof, &receipt("101")).unwrap());
    }

    #[test]
    fn repayment_below_reserved_ceiling_refunds_selected_cash_not_borrowed_proceeds() {
        let directory = tempfile::tempdir().unwrap();
        let engine = synthetic_engine(
            &directory.path().join("repayment-refund.sqlite3"),
            Arc::new(SyntheticAdapter::new_with_valuation(true)),
        );
        let wallet = resource("synthetic-wallet");
        let market = resource("synthetic-market");
        let usdc = asset("ethereum:1", "USDC", 6);
        let cash = AccountingLocation::Cash {
            resource: wallet.clone(),
        };
        let debt = AccountingLocation::Position {
            resource: market.clone(),
            position: "synthetic-debt".into(),
            position_kind: PositionKind::VariableDebt,
        };
        let holding = ResourceAmount {
            resource: wallet.clone(),
            amount: Amount {
                asset: usdc.clone(),
                atoms: quantity("60000000"),
            },
        };
        let selected_origin = format!("allocation:{}", resource_asset_key(&holding).unwrap());
        let mut run = empty_run_for_provenance(StateSnapshot {
            state: FinancialState {
                holdings: vec![holding.clone()],
                ..Default::default()
            },
            observed_at: now(),
            complete: true,
            coverage: vec![wallet.0.clone(), market.0.clone()],
            pending: vec![],
        });
        run.allocations.push(RunAllocation {
            holding,
            value_usd_micros: "60000000".into(),
            observed_at: now(),
            source: "synthetic allocation".into(),
        });
        run.available = vec![ResourceAmount {
            resource: wallet.clone(),
            amount: Amount {
                asset: usdc.clone(),
                atoms: quantity("58000000"),
            },
        }];
        run.provenance.push(ProvenanceLot {
            origin: selected_origin.clone(),
            location: cash.clone(),
            asset: usdc.clone(),
            atoms: "58000000".into(),
        });
        let debit = ResourceAmount {
            resource: wallet.clone(),
            amount: Amount {
                asset: usdc.clone(),
                atoms: quantity("42000000"),
            },
        };
        let cost = CostBound {
            location: cash.clone(),
            amount: Amount {
                asset: usdc.clone(),
                atoms: quantity("1000000"),
            },
            treatment: CostTreatment::IncludedInDebit,
        };
        let mut operation = record(
            vec![EffectConstraint::OriginMovement {
                from: cash.clone(),
                to: debt.clone(),
                debit_asset: usdc.clone(),
                credit_asset: usdc.clone(),
                maximum_debit: quantity("42000000"),
                minimum_credit: quantity("40000000"),
            }],
            vec![cost.clone()],
        );
        operation.status = Status::Completed;
        operation.preview.as_mut().unwrap().valuation = Some(fin_domain::Valuation {
            movement_usd_micros: quantity("42000000"),
            maximum_fee_usd_micros: quantity("1000000"),
            additional_allocation_usd_micros: quantity("0"),
            observed_at: now(),
            source: "synthetic price".into(),
            prices: vec![AssetPrice {
                asset: usdc.clone(),
                usd_micros_per_unit: quantity("1000000"),
                unit_atoms: quantity("1000000"),
                observed_at: now(),
                source: "synthetic price".into(),
            }],
        });
        run.route_proofs.push(ReturnRouteProof {
            step_index: 0,
            operation: "synthetic.repay".into(),
            input_hash: "synthetic".into(),
            observed_at: now(),
            expires_at: (Utc::now() + chrono::Duration::minutes(1)).to_rfc3339(),
            intent: operation.preview.as_ref().unwrap().intent.clone(),
            effects: vec![ProjectedEffect::CashDelta {
                resource: wallet.clone(),
                asset: usdc.clone(),
                minimum_delta: SignedQuantity::new("-42000000").unwrap(),
                maximum_delta: SignedQuantity::new("-40000000").unwrap(),
            }],
            requires: vec![],
            evidence: json!({"fixture":"synthetic bounded repayment"}),
        });
        run.reservations.insert(
            operation.id.clone(),
            RunReservation {
                debits: vec![debit],
                maximum_costs: vec![cost],
                additional_cost_provenance: vec![],
                cash_provenance_version: 3,
                cash_sources: Some(cash_reservation_sources::CashReservationSources {
                    before: vec![
                        ProvenanceLot {
                            origin: selected_origin.clone(),
                            location: cash.clone(),
                            asset: usdc.clone(),
                            atoms: "60000000".into(),
                        },
                        ProvenanceLot {
                            origin: "borrowed:synthetic-borrow".into(),
                            location: cash.clone(),
                            asset: usdc.clone(),
                            atoms: "40000000".into(),
                        },
                    ],
                    principal_indices: vec![1, 0],
                    additional_cost_indices: vec![],
                }),
                provenance: vec![
                    ProvenanceLot {
                        origin: "borrowed:synthetic-borrow".into(),
                        location: cash.clone(),
                        asset: usdc.clone(),
                        atoms: "40000000".into(),
                    },
                    ProvenanceLot {
                        origin: selected_origin.clone(),
                        location: cash.clone(),
                        asset: usdc.clone(),
                        atoms: "2000000".into(),
                    },
                ],
                maximum_fee_usd_micros: "1000000".into(),
                additional_allocation_usd_micros: "0".into(),
                binding_hash: "synthetic".into(),
                route_step_index: Some(0),
            },
        );
        let paid = Amount {
            asset: usdc.clone(),
            atoms: quantity("41000000"),
        };
        let verified = proof(
            &operation,
            vec![
                FinancialEffect::CashDebit {
                    resource: wallet.clone(),
                    amount: paid.clone(),
                },
                FinancialEffect::DebtChange {
                    resource: market,
                    debt: "synthetic-debt".into(),
                    asset: usdc.clone(),
                    delta: SignedQuantity::new("-41000000").unwrap(),
                    anchor: None,
                },
                FinancialEffect::OriginMovement {
                    from: cash.clone(),
                    to: debt,
                    debit: paid.clone(),
                    credit: paid,
                },
                FinancialEffect::Cost {
                    location: cash,
                    amount: Amount {
                        asset: usdc,
                        atoms: quantity("1000000"),
                    },
                    category: "synthetic accrued interest".into(),
                    treatment: CostTreatment::IncludedInDebit,
                },
            ],
        );
        validate_origin_links(&verified.effects).unwrap();
        engine.settle_run(&mut run, &operation, &verified).unwrap();
        assert!(run.reservations.is_empty());
        assert!(!run.route_invalidated);
        assert_eq!(run.available[0].amount.atoms.as_str(), "59000000");
        assert!(
            run.provenance
                .iter()
                .all(|lot| lot.origin == selected_origin)
        );
        assert_eq!(
            run.provenance
                .iter()
                .map(|lot| parse_uint(&lot.atoms).unwrap())
                .sum::<BigUint>(),
            BigUint::from(59_000_000u64)
        );
        assert_eq!(run.costs[0].amount.atoms.as_str(), "1000000");
        assert_eq!(run.spent_cost_usd_micros, "1000000");
    }

    fn custody_return_fixture(path: &std::path::Path) -> (Engine, RunLedger, OperationRecord) {
        let engine = synthetic_engine(path, Arc::new(SyntheticAdapter::new_with_valuation(true)));
        let a = resource("synthetic-custody-a");
        let b = resource("synthetic-custody-b");
        let token = asset("synthetic:1", "TOKEN", 0);
        let holding = |resource: &ResourceId, atoms: &str| ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: token.clone(),
                atoms: quantity(atoms),
            },
        };
        let before = vec![holding(&a, "100"), holding(&b, "10")];
        let baseline = StateSnapshot {
            state: FinancialState {
                holdings: before.clone(),
                ..Default::default()
            },
            observed_at: now(),
            complete: true,
            coverage: vec![a.0.clone(), b.0.clone()],
            pending: vec![],
        };
        let mut run = empty_run_for_provenance(baseline);
        run.initial_allocation_usd_micros = "110000".into();
        run.return_cost_reserve_usd_micros = "2000".into();
        run.asset_prices = vec![AssetPrice {
            asset: token.clone(),
            unit_atoms: quantity("1"),
            usd_micros_per_unit: quantity("1000"),
            observed_at: now(),
            source: "synthetic unit price".into(),
        }];
        run.allocations = before
            .iter()
            .map(|holding| RunAllocation {
                holding: holding.clone(),
                value_usd_micros: (holding.amount.atoms.integer() * BigUint::from(1000u16))
                    .to_string(),
                observed_at: now(),
                source: "synthetic selected custody".into(),
            })
            .collect();
        // Seed a previously verified outward transfer. At B, its original ten
        // units precede A's hundred units, so a FIFO reservation would be wrong.
        run.available = vec![holding(&a, "0"), holding(&b, "110")];
        run.provenance = [(&before[1], "10"), (&before[0], "100")]
            .into_iter()
            .map(|(origin, atoms)| ProvenanceLot {
                origin: format!("allocation:{}", resource_asset_key(origin).unwrap()),
                location: AccountingLocation::Cash {
                    resource: b.clone(),
                },
                asset: token.clone(),
                atoms: atoms.into(),
            })
            .collect();
        let fee = CostBound {
            location: AccountingLocation::Cash {
                resource: b.clone(),
            },
            amount: Amount {
                asset: token.clone(),
                atoms: quantity("2"),
            },
            treatment: CostTreatment::AdditionalDebit,
        };
        let mut operation = record(
            vec![EffectConstraint::Transfer {
                from: b.clone(),
                to: a.clone(),
                debit_asset: token.clone(),
                credit_asset: token.clone(),
                maximum_debit: quantity("100"),
                minimum_credit: quantity("100"),
            }],
            vec![fee],
        );
        operation.run_id = Some(run.id.clone());
        operation.status = Status::Completed;
        let prepared = operation.preview.as_mut().unwrap();
        prepared.observed_at = now();
        prepared.expires_at = (Utc::now() + chrono::Duration::seconds(60)).to_rfc3339();
        prepared.intent.debits = vec![holding(&b, "100")];
        prepared.intent.locks = vec![a.clone(), b.clone()];
        prepared.valuation = Some(fin_domain::Valuation {
            movement_usd_micros: quantity("100000"),
            maximum_fee_usd_micros: quantity("2000"),
            additional_allocation_usd_micros: quantity("0"),
            observed_at: now(),
            source: "synthetic exact valuation".into(),
            prices: run.asset_prices.clone(),
        });
        run.planned_operations = vec![PlanStep {
            operation: operation.operation.clone(),
            input: operation.input.clone(),
            maximum_fee_usd_micros: "2000".into(),
            description: "Return the selected principal to its original custody".into(),
        }];
        run.route_proofs = vec![ReturnRouteProof {
            step_index: 0,
            operation: operation.operation.clone(),
            input_hash: binding_hash(&operation.input).unwrap(),
            observed_at: now(),
            expires_at: prepared.expires_at.clone(),
            intent: prepared.intent.clone(),
            effects: vec![
                ProjectedEffect::CashDelta {
                    resource: b,
                    asset: token.clone(),
                    minimum_delta: SignedQuantity::new("-102").unwrap(),
                    maximum_delta: SignedQuantity::new("-100").unwrap(),
                },
                ProjectedEffect::CashDelta {
                    resource: a,
                    asset: token,
                    minimum_delta: SignedQuantity::new("100").unwrap(),
                    maximum_delta: SignedQuantity::new("100").unwrap(),
                },
            ],
            requires: vec![],
            evidence: json!({"fixture":"synthetic bounded custody return"}),
        }];
        engine.save_run(&run).unwrap();
        (engine, run, operation)
    }

    fn custody_return_effects(
        operation: &OperationRecord,
        principal: bool,
    ) -> Vec<FinancialEffect> {
        let prepared = operation.preview.as_ref().unwrap();
        let debit = &prepared.intent.debits[0];
        let EffectConstraint::Transfer { to, .. } = &prepared.intent.expected[0] else {
            panic!("synthetic transfer fixture")
        };
        let mut effects = vec![];
        if principal {
            effects.push(FinancialEffect::Transfer {
                from: debit.resource.clone(),
                to: to.clone(),
                debit: debit.amount.clone(),
                credit: debit.amount.clone(),
            });
        }
        effects.push(FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: debit.resource.clone(),
            },
            amount: Amount {
                asset: debit.amount.asset.clone(),
                atoms: quantity("1"),
            },
            category: "synthetic network fee".into(),
            treatment: CostTreatment::AdditionalDebit,
        });
        effects
    }

    #[test]
    fn custody_return_reserves_home_principal_and_separate_fee_then_restores() {
        let directory = tempfile::tempdir().unwrap();
        let (engine, original, operation) =
            custody_return_fixture(&directory.path().join("custody-return.sqlite3"));
        let prepared = operation.preview.as_ref().unwrap();
        let mut run = engine
            .reserve_run_operation(&original.id, &operation, prepared)
            .unwrap();
        let reservation = &run.reservations[&operation.id];
        let origin_a = format!(
            "allocation:{}",
            resource_asset_key(&original.allocations[0].holding).unwrap()
        );
        let origin_b = format!(
            "allocation:{}",
            resource_asset_key(&original.allocations[1].holding).unwrap()
        );
        assert_eq!(reservation.provenance.len(), 1);
        assert_eq!(reservation.provenance[0].origin, origin_a);
        assert_eq!(reservation.provenance[0].atoms, "100");
        assert_eq!(reservation.additional_cost_provenance.len(), 1);
        assert_eq!(reservation.additional_cost_provenance[0].origin, origin_b);
        assert_eq!(reservation.additional_cost_provenance[0].atoms, "2");
        let verified = proof(&operation, custody_return_effects(&operation, true));
        validate_origin_links(&verified.effects).unwrap();
        engine.settle_run(&mut run, &operation, &verified).unwrap();
        assert!(run.reservations.is_empty());
        assert!(!run.route_invalidated);
        assert_eq!(
            run.costs,
            vec![ResourceAmount {
                resource: original.allocations[1].holding.resource.clone(),
                amount: Amount {
                    asset: prepared.intent.debits[0].amount.asset.clone(),
                    atoms: quantity("1")
                },
            }]
        );
        assert_eq!(run.spent_cost_usd_micros, "1000");
        assert!(run.provenance.iter().all(|lot| {
            allocation_origin_key(&lot.origin)
                .and_then(|key| decode_resource_asset_key(key).ok())
                .is_some_and(|(home, asset)| {
                    lot.location == AccountingLocation::Cash { resource: home }
                        && lot.asset == asset
                })
        }));
        engine.save_run(&run).unwrap();
        let final_snapshot = StateSnapshot {
            state: FinancialState {
                holdings: run.available.clone(),
                ..Default::default()
            },
            observed_at: now(),
            complete: true,
            coverage: original.baseline.coverage,
            pending: vec![],
        };
        let result = engine
            .finish_run(&run.id, final_snapshot, Utc::now())
            .unwrap();
        assert_eq!(
            result.status,
            RunStatus::Restored,
            "{:?}",
            result.differences
        );
    }

    #[test]
    fn custody_return_fee_only_failure_refunds_principal_and_requires_funded_replan() {
        let directory = tempfile::tempdir().unwrap();
        let (engine, original, mut operation) =
            custody_return_fixture(&directory.path().join("custody-revert.sqlite3"));
        let mut run = engine
            .reserve_run_operation(
                &original.id,
                &operation,
                operation.preview.as_ref().unwrap(),
            )
            .unwrap();
        operation.status = Status::Failed;
        let identity = operation.submission.clone();
        engine
            .settle_run(
                &mut run,
                &operation,
                &proof(&operation, custody_return_effects(&operation, false)),
            )
            .unwrap();
        assert_eq!(operation.submission, identity);
        assert!(run.reservations.is_empty());
        assert!(run.route_invalidated);
        assert_eq!(run.status, RunStatus::CleanupOutstanding);
        assert!(run.settled_route_steps.is_empty());
        let mut expected = original.provenance;
        expected[0].atoms = "9".into();
        assert_eq!(
            run.provenance, expected,
            "only the reserved B fee is consumed; A principal and source order survive"
        );
        assert_eq!(run.spent_cost_usd_micros, "1000");
        engine.save_run(&run).unwrap();
        assert_eq!(
            engine
                .reserve_run_operation(&run.id, &operation, operation.preview.as_ref().unwrap())
                .unwrap_err()
                .code,
            "RETURN_ROUTE_INVALIDATED"
        );
        let final_snapshot = StateSnapshot {
            state: FinancialState {
                holdings: run.available.clone(),
                ..Default::default()
            },
            observed_at: now(),
            complete: true,
            coverage: original.baseline.coverage,
            pending: vec![],
        };
        assert_eq!(
            engine
                .finish_run(&run.id, final_snapshot, Utc::now())
                .unwrap()
                .status,
            RunStatus::CleanupOutstanding
        );
    }

    #[test]
    fn custody_return_human_rejection_restores_exact_pre_reservation_order() {
        let directory = tempfile::tempdir().unwrap();
        let (engine, original, mut operation) =
            custody_return_fixture(&directory.path().join("custody-rejection.sqlite3"));
        let reserved = engine
            .reserve_run_operation(
                &original.id,
                &operation,
                operation.preview.as_ref().unwrap(),
            )
            .unwrap();
        engine.save_run(&reserved).unwrap();
        operation.status = Status::Rejected;
        operation.submission = None;
        let released = engine
            .release_unsubmitted_reservation(&operation)
            .unwrap()
            .unwrap();
        assert_eq!(released.provenance, original.provenance);
        assert_eq!(
            aggregate_resource_amounts(&released.available).unwrap(),
            aggregate_resource_amounts(&original.available).unwrap()
        );
        assert!(released.reservations.is_empty());
        assert_eq!(released.spent_cost_usd_micros, "0");
        assert!(!released.route_invalidated);
        engine.save_run(&released).unwrap();
        let retried = engine
            .reserve_run_operation(
                &released.id,
                &operation,
                operation.preview.as_ref().unwrap(),
            )
            .unwrap();
        assert_eq!(
            retried.reservations[&operation.id],
            reserved.reservations[&operation.id]
        );
    }

    #[test]
    fn legacy_cash_reservation_remains_unresolved_without_replay() {
        let directory = tempfile::tempdir().unwrap();
        let (engine, original, operation) =
            custody_return_fixture(&directory.path().join("custody-legacy.sqlite3"));
        let mut run = engine
            .reserve_run_operation(
                &original.id,
                &operation,
                operation.preview.as_ref().unwrap(),
            )
            .unwrap();
        let reservation = run.reservations.get_mut(&operation.id).unwrap();
        reservation.cash_provenance_version = 0;
        reservation.cash_sources = None;
        engine.save_run(&run).unwrap();
        let before = to_value(&run).unwrap();
        let identity = operation.submission.clone();
        let error = engine
            .settle_run(
                &mut run,
                &operation,
                &proof(&operation, custody_return_effects(&operation, true)),
            )
            .unwrap_err();
        assert_eq!(error.code, "RESERVATION_PROVENANCE_VERSION_UNSUPPORTED");
        assert_eq!(to_value(&run).unwrap(), before);
        assert_eq!(
            to_value(&engine.load_run(&run.id).unwrap()).unwrap(),
            before
        );
        assert_eq!(operation.submission, identity);
    }

    #[test]
    fn lending_round_trip_preserves_selected_principal_yield_and_included_costs() {
        let directory = tempfile::tempdir().unwrap();
        let engine = synthetic_engine(
            &directory.path().join("lending-roundtrip.sqlite3"),
            Arc::new(SyntheticAdapter::new_with_valuation(true)),
        );
        let wallet = resource("ethereum-wallet");
        let market = resource("aave-market");
        let usdc = asset("ethereum:1", "USDC", 6);
        let cash = AccountingLocation::Cash {
            resource: wallet.clone(),
        };
        let supply = AccountingLocation::Position {
            resource: market.clone(),
            position: "aUSDC".into(),
            position_kind: PositionKind::Supply,
        };
        let debt = AccountingLocation::Position {
            resource: market.clone(),
            position: "variableDebtUSDC".into(),
            position_kind: PositionKind::VariableDebt,
        };
        let holding = ResourceAmount {
            resource: wallet.clone(),
            amount: Amount {
                asset: usdc.clone(),
                atoms: quantity("60000000"),
            },
        };
        let instant = Utc::now();
        let baseline = StateSnapshot {
            state: fin_domain::FinancialState {
                holdings: vec![holding.clone()],
                ..Default::default()
            },
            observed_at: instant.to_rfc3339(),
            complete: true,
            coverage: vec![wallet.0.clone(), market.0.clone()],
            pending: vec![],
        };
        let mut run = empty_run_for_provenance(baseline.clone());
        run.id = "synthetic-lending-roundtrip".into();
        run.initial_allocation_usd_micros = "60000000".into();
        run.borrowed_proceeds_reserve_usd_micros = "40000000".into();
        run.return_cost_reserve_usd_micros = "1100000".into();
        run.available = vec![holding.clone()];
        run.allocations.push(RunAllocation {
            holding: holding.clone(),
            value_usd_micros: "60000000".into(),
            observed_at: instant.to_rfc3339(),
            source: "synthetic selected allocation".into(),
        });
        run.provenance.push(ProvenanceLot {
            origin: format!("allocation:{}", resource_asset_key(&holding).unwrap()),
            location: cash.clone(),
            asset: usdc.clone(),
            atoms: "60000000".into(),
        });

        let valuation = fin_domain::Valuation {
            movement_usd_micros: quantity("100000000"),
            maximum_fee_usd_micros: quantity("1100000"),
            additional_allocation_usd_micros: quantity("40000000"),
            observed_at: instant.to_rfc3339(),
            source: "synthetic test fixture".into(),
            prices: vec![fin_domain::AssetPrice {
                asset: usdc.clone(),
                usd_micros_per_unit: quantity("1000000"),
                unit_atoms: quantity("1000000"),
                observed_at: instant.to_rfc3339(),
                source: "synthetic test fixture".into(),
            }],
        };
        run.asset_prices = valuation.prices.clone();
        let position_zero = position_anchor("aave-market", "aUSDC", PositionKind::Supply, "0");
        let supply_anchor =
            position_anchor("aave-market", "aUSDC", PositionKind::Supply, "61200000");
        let debt_zero = position_anchor(
            "aave-market",
            "variableDebtUSDC",
            PositionKind::VariableDebt,
            "0",
        );
        let debt_borrowed = position_anchor(
            "aave-market",
            "variableDebtUSDC",
            PositionKind::VariableDebt,
            "40000000",
        );
        let debt_accrued = position_anchor(
            "aave-market",
            "variableDebtUSDC",
            PositionKind::VariableDebt,
            "41000000",
        );
        let mut cash_lots = take_provenance(
            &mut run.provenance,
            &cash,
            &usdc,
            &BigUint::from(60_000_000u64),
        )
        .unwrap();
        let supply_effects = vec![
            FinancialEffect::CashDebit {
                resource: wallet.clone(),
                amount: Amount {
                    asset: usdc.clone(),
                    atoms: quantity("60000000"),
                },
            },
            FinancialEffect::OriginMovement {
                from: cash.clone(),
                to: supply.clone(),
                debit: Amount {
                    asset: usdc.clone(),
                    atoms: quantity("60000000"),
                },
                credit: Amount {
                    asset: usdc.clone(),
                    atoms: quantity("60000000"),
                },
            },
            FinancialEffect::Accrual {
                resource: market.clone(),
                position: "aUSDC".into(),
                asset: usdc.clone(),
                position_kind: PositionKind::Supply,
                delta: SignedQuantity::new("1200000").unwrap(),
            },
            FinancialEffect::PositionChange {
                resource: market.clone(),
                position: "aUSDC".into(),
                asset: usdc.clone(),
                delta: SignedQuantity::new("60000000").unwrap(),
                scaled_receipt: None,
                anchor: Some(supply_anchor.clone()),
            },
        ];
        validate_origin_links(&supply_effects).unwrap();
        validate_anchor_transitions(
            &proof(&record(vec![], vec![]), supply_effects.clone()),
            &PreparationContext {
                positions: vec![position_zero],
            },
        )
        .unwrap();
        apply_effect_provenance(
            &mut run,
            "synthetic-supply",
            &mut cash_lots,
            &mut vec![],
            &supply_effects,
            &valuation,
        )
        .unwrap();
        assert!(cash_lots.is_empty());

        let borrow_effects = vec![
            FinancialEffect::DebtChange {
                resource: market.clone(),
                debt: "variableDebtUSDC".into(),
                asset: usdc.clone(),
                delta: SignedQuantity::new("40000000").unwrap(),
                anchor: Some(debt_borrowed.clone()),
            },
            FinancialEffect::UsableCredit {
                to: wallet.clone(),
                amount: Amount {
                    asset: usdc.clone(),
                    atoms: quantity("40000000"),
                },
            },
            FinancialEffect::OriginMovement {
                from: debt.clone(),
                to: cash.clone(),
                debit: Amount {
                    asset: usdc.clone(),
                    atoms: quantity("40000000"),
                },
                credit: Amount {
                    asset: usdc.clone(),
                    atoms: quantity("40000000"),
                },
            },
        ];
        validate_origin_links(&borrow_effects).unwrap();
        validate_anchor_transitions(
            &proof(&record(vec![], vec![]), borrow_effects.clone()),
            &PreparationContext {
                positions: vec![debt_zero],
            },
        )
        .unwrap();
        apply_effect_provenance(
            &mut run,
            "synthetic-borrow",
            &mut vec![],
            &mut vec![],
            &borrow_effects,
            &valuation,
        )
        .unwrap();

        let debt_interest = vec![
            FinancialEffect::Accrual {
                resource: market.clone(),
                position: "variableDebtUSDC".into(),
                asset: usdc.clone(),
                position_kind: PositionKind::VariableDebt,
                delta: SignedQuantity::new("1000000").unwrap(),
            },
            FinancialEffect::DebtChange {
                resource: market.clone(),
                debt: "variableDebtUSDC".into(),
                asset: usdc.clone(),
                delta: SignedQuantity::new("0").unwrap(),
                anchor: Some(debt_accrued.clone()),
            },
        ];
        validate_anchor_transitions(
            &proof(&record(vec![], vec![]), debt_interest.clone()),
            &PreparationContext {
                positions: vec![debt_borrowed],
            },
        )
        .unwrap();
        apply_effect_provenance(
            &mut run,
            "synthetic-debt-interest",
            &mut vec![],
            &mut vec![],
            &debt_interest,
            &valuation,
        )
        .unwrap();

        let withdrawal_effects = vec![
            FinancialEffect::PositionChange {
                resource: market.clone(),
                position: "aUSDC".into(),
                asset: usdc.clone(),
                delta: SignedQuantity::new("-61200000").unwrap(),
                scaled_receipt: None,
                anchor: Some(position_anchor(
                    "aave-market",
                    "aUSDC",
                    PositionKind::Supply,
                    "0",
                )),
            },
            FinancialEffect::UsableCredit {
                to: wallet.clone(),
                amount: Amount {
                    asset: usdc.clone(),
                    atoms: quantity("61190000"),
                },
            },
            FinancialEffect::OriginMovement {
                from: supply.clone(),
                to: cash.clone(),
                debit: Amount {
                    asset: usdc.clone(),
                    atoms: quantity("61200000"),
                },
                credit: Amount {
                    asset: usdc.clone(),
                    atoms: quantity("61190000"),
                },
            },
            FinancialEffect::Cost {
                location: supply.clone(),
                amount: Amount {
                    asset: usdc.clone(),
                    atoms: quantity("10000"),
                },
                category: "synthetic withdraw rounding loss".into(),
                treatment: CostTreatment::IncludedInDebit,
            },
        ];
        validate_origin_links(&withdrawal_effects).unwrap();
        validate_anchor_transitions(
            &proof(&record(vec![], vec![]), withdrawal_effects.clone()),
            &PreparationContext {
                positions: vec![supply_anchor],
            },
        )
        .unwrap();
        let mapped = apply_effect_provenance(
            &mut run,
            "synthetic-withdraw",
            &mut vec![],
            &mut vec![],
            &withdrawal_effects,
            &valuation,
        )
        .unwrap();
        run.costs = add_amounts(&run.costs, &mapped).unwrap();

        cash_lots = take_provenance_ordered(
            &mut run.provenance,
            &cash,
            &usdc,
            &BigUint::from(41_000_000u64),
            true,
        )
        .unwrap();
        let repayment_effects = vec![
            FinancialEffect::DebtChange {
                resource: market.clone(),
                debt: "variableDebtUSDC".into(),
                asset: usdc.clone(),
                delta: SignedQuantity::new("-41000000").unwrap(),
                anchor: Some(position_anchor(
                    "aave-market",
                    "variableDebtUSDC",
                    PositionKind::VariableDebt,
                    "0",
                )),
            },
            FinancialEffect::CashDebit {
                resource: wallet.clone(),
                amount: Amount {
                    asset: usdc.clone(),
                    atoms: quantity("41000000"),
                },
            },
            FinancialEffect::OriginMovement {
                from: cash.clone(),
                to: debt.clone(),
                debit: Amount {
                    asset: usdc.clone(),
                    atoms: quantity("41000000"),
                },
                credit: Amount {
                    asset: usdc.clone(),
                    atoms: quantity("41000000"),
                },
            },
            FinancialEffect::Cost {
                location: cash.clone(),
                amount: Amount {
                    asset: usdc.clone(),
                    atoms: quantity("1000000"),
                },
                category: "synthetic accrued interest paid".into(),
                treatment: CostTreatment::IncludedInDebit,
            },
        ];
        validate_origin_links(&repayment_effects).unwrap();
        validate_anchor_transitions(
            &proof(&record(vec![], vec![]), repayment_effects.clone()),
            &PreparationContext {
                positions: vec![debt_accrued],
            },
        )
        .unwrap();
        let mapped = apply_effect_provenance(
            &mut run,
            "synthetic-repay",
            &mut cash_lots,
            &mut vec![],
            &repayment_effects,
            &valuation,
        )
        .unwrap();
        run.costs = add_amounts(&run.costs, &mapped).unwrap();
        assert!(cash_lots.is_empty());
        assert!(
            run.provenance
                .iter()
                .all(|lot| !lot.origin.starts_with("borrowed:")),
            "full repayment must consume borrowed proceeds before older selected cash"
        );

        run.available = vec![ResourceAmount {
            resource: wallet.clone(),
            amount: Amount {
                asset: usdc.clone(),
                atoms: quantity("60190000"),
            },
        }];
        engine.save_run(&run).unwrap();
        let final_snapshot = StateSnapshot {
            state: fin_domain::FinancialState {
                holdings: run.available.clone(),
                ..Default::default()
            },
            observed_at: now(),
            complete: true,
            coverage: baseline.coverage.clone(),
            pending: vec![],
        };
        let result = engine
            .finish_run(&run.id, final_snapshot, Utc::now())
            .unwrap();
        assert_eq!(
            result.status,
            RunStatus::Restored,
            "{:?}",
            result.differences
        );
        assert_eq!(run.costs[0].amount.atoms.as_str(), "1010000");
        assert_eq!(
            run.provenance
                .iter()
                .filter(|lot| lot.origin.starts_with("accrual:"))
                .map(|lot| lot.atoms.parse::<u64>().unwrap())
                .sum::<u64>(),
            1_200_000
        );
    }

    #[test]
    fn aave_borrow_scope_allows_only_a_matching_typed_debt_increase_source() {
        let wallet = resource("synthetic-wallet");
        let market = resource("synthetic-market");
        let usdc = asset("ethereum:1", "USDC", 6);
        let debt = AccountingLocation::Position {
            resource: market.clone(),
            position: "variableDebtUSDC".into(),
            position_kind: PositionKind::VariableDebt,
        };
        let cash = AccountingLocation::Cash {
            resource: wallet.clone(),
        };
        let baseline = StateSnapshot {
            state: FinancialState::default(),
            observed_at: now(),
            complete: true,
            coverage: vec![wallet.0.clone(), market.0.clone()],
            pending: vec![],
        };
        let run = empty_run_for_provenance(baseline);
        let movement = EffectConstraint::OriginMovement {
            from: debt.clone(),
            to: cash.clone(),
            debit_asset: usdc.clone(),
            credit_asset: usdc.clone(),
            maximum_debit: quantity("10000000"),
            minimum_credit: quantity("9900000"),
        };
        let debt_increase = EffectConstraint::Debt {
            resource: market.clone(),
            debt: "variableDebtUSDC".into(),
            asset: usdc.clone(),
            direction: Direction::Increase,
            maximum_change: quantity("10000000"),
        };
        let intent = FinancialIntent {
            expected: vec![movement.clone(), debt_increase.clone()],
            ..Default::default()
        };
        // This matches the Aave borrow preview: debt-position origin movement
        // is proved by the newly-created debt, not by a prior position lot.
        ensure_intent_scope(&run, &intent).unwrap();

        for malformed in [
            EffectConstraint::Debt {
                resource: market.clone(),
                debt: "otherDebt".into(),
                asset: usdc.clone(),
                direction: Direction::Increase,
                maximum_change: quantity("10000000"),
            },
            EffectConstraint::Debt {
                resource: market.clone(),
                debt: "variableDebtUSDC".into(),
                asset: asset("ethereum:1", "WETH", 18),
                direction: Direction::Increase,
                maximum_change: quantity("10000000"),
            },
            EffectConstraint::Debt {
                resource: market.clone(),
                debt: "variableDebtUSDC".into(),
                asset: usdc.clone(),
                direction: Direction::Decrease,
                maximum_change: quantity("10000000"),
            },
            EffectConstraint::Debt {
                resource: market.clone(),
                debt: "variableDebtUSDC".into(),
                asset: usdc.clone(),
                direction: Direction::Increase,
                maximum_change: quantity("9999999"),
            },
        ] {
            let invalid = FinancialIntent {
                expected: vec![movement.clone(), malformed],
                ..Default::default()
            };
            assert!(ensure_intent_scope(&run, &invalid).is_err());
        }
        let fake_cash_mint = FinancialIntent {
            expected: vec![
                EffectConstraint::OriginMovement {
                    from: debt.clone(),
                    to: cash,
                    debit_asset: usdc.clone(),
                    credit_asset: usdc,
                    maximum_debit: quantity("10000000"),
                    minimum_credit: quantity("10000001"),
                },
                debt_increase,
            ],
            ..Default::default()
        };
        assert!(ensure_intent_scope(&run, &fake_cash_mint).is_err());
        let fake_cross_asset_borrow = FinancialIntent {
            expected: vec![
                EffectConstraint::OriginMovement {
                    from: debt.clone(),
                    to: AccountingLocation::Cash {
                        resource: wallet.clone(),
                    },
                    debit_asset: asset("ethereum:1", "USDC", 6),
                    credit_asset: asset("ethereum:1", "WETH", 18),
                    maximum_debit: quantity("10000000"),
                    minimum_credit: quantity("10000000"),
                },
                EffectConstraint::Debt {
                    resource: market.clone(),
                    debt: "variableDebtUSDC".into(),
                    asset: asset("ethereum:1", "USDC", 6),
                    direction: Direction::Increase,
                    maximum_change: quantity("10000000"),
                },
            ],
            ..Default::default()
        };
        assert!(ensure_intent_scope(&run, &fake_cross_asset_borrow).is_err());

        let supply_origin = AccountingLocation::Position {
            resource: market,
            position: "aUSDC".into(),
            position_kind: PositionKind::Supply,
        };
        let unselected_withdrawal = FinancialIntent {
            expected: vec![EffectConstraint::OriginMovement {
                from: supply_origin,
                to: AccountingLocation::Cash { resource: wallet },
                debit_asset: asset("ethereum:1", "USDC", 6),
                credit_asset: asset("ethereum:1", "USDC", 6),
                maximum_debit: quantity("10000000"),
                minimum_credit: quantity("10000000"),
            }],
            ..Default::default()
        };
        assert!(ensure_intent_scope(&run, &unselected_withdrawal).is_err());
    }

    #[test]
    fn run_allocation_assets_uses_durable_original_assets_after_cash_moves() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("allocation-assets.sqlite")).unwrap();
        let engine = Engine::new(
            store,
            vec![],
            Arc::new(SyntheticAuthorizer),
            EngineOptions::default(),
        )
        .unwrap();
        let baseline = StateSnapshot {
            state: FinancialState::default(),
            observed_at: Utc::now().to_rfc3339(),
            complete: true,
            coverage: vec!["wallet-a".into(), "wallet-b".into()],
            pending: vec![],
        };
        let mut run = empty_run_for_provenance(baseline);
        run.id = "synthetic-origin-assets".into();
        let eth = asset("binance", "ETH", 18);
        let usdc = asset("binance", "USDC", 8);
        run.allocations = vec![
            RunAllocation {
                holding: ResourceAmount {
                    resource: resource("wallet-a"),
                    amount: Amount {
                        asset: eth.clone(),
                        atoms: quantity("100"),
                    },
                },
                value_usd_micros: "1000".into(),
                observed_at: Utc::now().to_rfc3339(),
                source: "synthetic fixture".into(),
            },
            RunAllocation {
                holding: ResourceAmount {
                    resource: resource("wallet-b"),
                    amount: Amount {
                        asset: eth.clone(),
                        atoms: quantity("50"),
                    },
                },
                value_usd_micros: "500".into(),
                observed_at: Utc::now().to_rfc3339(),
                source: "synthetic fixture".into(),
            },
            RunAllocation {
                holding: ResourceAmount {
                    resource: resource("wallet-a"),
                    amount: Amount {
                        asset: usdc.clone(),
                        atoms: quantity("250"),
                    },
                },
                value_usd_micros: "250".into(),
                observed_at: Utc::now().to_rfc3339(),
                source: "synthetic fixture".into(),
            },
        ];
        // Current local cash is empty after movement; pricing still needs the
        // exact assets captured in the immutable original allocation.
        run.available.clear();
        run.provenance.clear();
        engine
            .store
            .lock()
            .unwrap()
            .create_run(&run.id, &to_value(&run).unwrap(), &now())
            .unwrap();

        assert_eq!(
            engine.run_allocation_assets(&run.id).unwrap(),
            vec![eth, usdc]
        );
        assert_eq!(
            engine
                .run_allocation_assets("missing-synthetic-run")
                .unwrap_err()
                .code,
            "RUN_NOT_FOUND"
        );
    }
    fn trading_rebind(record: &mut OperationRecord) {
        record.approval.as_mut().unwrap().binding_hash =
            binding_hash(&authorization_binding(record.preview.as_ref().unwrap())).unwrap();
    }

    #[test]
    fn derivative_exposure_requires_own_bound_and_exact_anchor_and_is_never_custody() {
        let a = asset("synthetic", "BASE", 0);
        let r = resource("synthetic-margin");
        let mut anchor = position_anchor(
            "synthetic-margin",
            "BASE:LONG",
            PositionKind::DerivativeLong,
            "3",
        );
        anchor.principal.asset = a.clone();
        let effect = FinancialEffect::PositionChange {
            resource: r.clone(),
            position: "BASE:LONG".into(),
            asset: a.clone(),
            delta: SignedQuantity::new("3").unwrap(),
            scaled_receipt: None,
            anchor: Some(anchor.clone()),
        };
        let bound = EffectConstraint::DerivativePosition {
            resource: r.clone(),
            position: "BASE:LONG".into(),
            asset: a.clone(),
            position_kind: PositionKind::DerivativeLong,
            minimum_delta: SignedQuantity::new("0").unwrap(),
            maximum_delta: SignedQuantity::new("3").unwrap(),
        };
        let rec = record(vec![bound.clone()], vec![]);
        let verified = proof(&rec, vec![effect.clone()]);
        validate_effects(&rec, &verified, &Status::Completed).unwrap();
        validate_anchor_transitions(&verified, &PreparationContext::default()).unwrap();
        validate_origin_links(&verified.effects).unwrap();
        let supply = EffectConstraint::Position {
            resource: r.clone(),
            position: "BASE:LONG".into(),
            asset: a.clone(),
            direction: Direction::Increase,
            maximum_change: quantity("3"),
        };
        assert!(!constraint_matches(&supply, &effect).unwrap());
        let mut wrong = effect.clone();
        if let FinancialEffect::PositionChange {
            anchor: Some(anchor),
            ..
        } = &mut wrong
        {
            anchor.principal.atoms = quantity("4");
        }
        assert_eq!(
            validate_anchor_transitions(&proof(&rec, vec![wrong]), &PreparationContext::default())
                .unwrap_err()
                .code,
            "POSITION_ANCHOR_TRANSITION_MISMATCH"
        );
        let mut short = effect.clone();
        if let FinancialEffect::PositionChange {
            anchor: Some(anchor),
            ..
        } = &mut short
        {
            anchor.position_kind = PositionKind::DerivativeShort;
        }
        assert!(!constraint_matches(&bound, &short).unwrap());
        let origin = FinancialEffect::OriginMovement {
            from: AccountingLocation::Cash {
                resource: r.clone(),
            },
            to: AccountingLocation::Position {
                resource: r,
                position: "BASE:LONG".into(),
                position_kind: PositionKind::DerivativeLong,
            },
            debit: Amount {
                asset: a.clone(),
                atoms: quantity("3"),
            },
            credit: Amount {
                asset: a,
                atoms: quantity("3"),
            },
        };
        assert_eq!(
            validate_origin_links(&[origin, effect]).unwrap_err().code,
            "INVALID_ORIGIN_MOVEMENT"
        );
    }

    #[test]
    fn realized_collateral_loss_is_reserved_once_bounded_and_must_be_costed() {
        let r = resource("binance-account");
        let a = asset("binance", "ETH", 18);
        let bound = EffectConstraint::Settlement {
            resource: r.clone(),
            asset: a.clone(),
            category: fin_domain::SettlementCategory::RealizedPnl,
            minimum_delta: SignedQuantity::new("-3").unwrap(),
            maximum_delta: SignedQuantity::new("5").unwrap(),
        };
        let cost = CostBound {
            location: AccountingLocation::Cash {
                resource: r.clone(),
            },
            amount: Amount {
                asset: a.clone(),
                atoms: quantity("3"),
            },
            treatment: CostTreatment::IncludedInDebit,
        };
        let mut rec = record(vec![bound], vec![cost]);
        rec.preview.as_mut().unwrap().intent.debits[0].amount.atoms = quantity("3");
        trading_rebind(&mut rec);
        validate_trading_bounds(&rec.preview.as_ref().unwrap().intent).unwrap();
        let loss = FinancialEffect::Settlement {
            resource: r.clone(),
            asset: a.clone(),
            category: fin_domain::SettlementCategory::RealizedPnl,
            delta: SignedQuantity::new("-2").unwrap(),
        };
        assert_eq!(
            validate_effects(&rec, &proof(&rec, vec![loss.clone()]), &Status::Completed)
                .unwrap_err()
                .code,
            "SETTLEMENT_LOSS_COST_MISSING"
        );
        let cost = FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: r.clone(),
            },
            amount: Amount {
                asset: a.clone(),
                atoms: quantity("2"),
            },
            category: "realized_loss".into(),
            treatment: CostTreatment::IncludedInDebit,
        };
        let effects = vec![loss, cost];
        validate_effects(&rec, &proof(&rec, effects.clone()), &Status::Completed).unwrap();
        assert_eq!(
            actual_debits_and_costs(&effects)
                .unwrap()
                .values()
                .next()
                .unwrap(),
            &BigUint::from(2u8)
        );
        let gain = FinancialEffect::Settlement {
            resource: r,
            asset: a,
            category: fin_domain::SettlementCategory::RealizedPnl,
            delta: SignedQuantity::new("6").unwrap(),
        };
        assert_eq!(
            validate_effects(&rec, &proof(&rec, vec![gain]), &Status::Completed)
                .unwrap_err()
                .code,
            "UNEXPECTED_FINANCIAL_EFFECT"
        );
        rec.preview.as_mut().unwrap().intent.maximum_costs.clear();
        assert_eq!(
            validate_trading_bounds(&rec.preview.unwrap().intent)
                .unwrap_err()
                .code,
            "SETTLEMENT_LOSS_UNRESERVED"
        );
    }

    #[test]
    fn withheld_output_fee_reduces_only_receipt_credit_and_its_origin_once() {
        let r = resource("synthetic-spot");
        let input = asset("synthetic", "INPUT", 0);
        let output = asset("synthetic", "OUTPUT", 0);
        let location = AccountingLocation::Cash {
            resource: r.clone(),
        };
        let baseline = StateSnapshot {
            state: FinancialState {
                holdings: vec![ResourceAmount {
                    resource: r.clone(),
                    amount: Amount {
                        asset: input.clone(),
                        atoms: quantity("100"),
                    },
                }],
                ..Default::default()
            },
            observed_at: now(),
            complete: true,
            coverage: vec![r.0.clone()],
            pending: vec![],
        };
        let mut run = empty_run_for_provenance(baseline);
        let key = resource_asset_key(&ResourceAmount {
            resource: r.clone(),
            amount: Amount {
                asset: input.clone(),
                atoms: quantity("100"),
            },
        })
        .unwrap();
        let mut principal = vec![ProvenanceLot {
            origin: format!("allocation:{key}"),
            location: location.clone(),
            asset: input.clone(),
            atoms: "100".into(),
        }];
        run.provenance.push(ProvenanceLot {
            origin: "untouched-existing-output".into(),
            location: location.clone(),
            asset: output.clone(),
            atoms: "50".into(),
        });
        let conversion = FinancialEffect::Conversion {
            resource: r.clone(),
            spent: Amount {
                asset: input.clone(),
                atoms: quantity("100"),
            },
            received: Amount {
                asset: output.clone(),
                atoms: quantity("10"),
            },
        };
        let fee = FinancialEffect::Cost {
            location: location.clone(),
            amount: Amount {
                asset: output.clone(),
                atoms: quantity("1"),
            },
            category: "commission".into(),
            treatment: CostTreatment::DeductedFromCredit,
        };
        let effects = vec![conversion, fee];
        let valuation = fin_domain::Valuation {
            movement_usd_micros: quantity("100000000"),
            maximum_fee_usd_micros: quantity("10000000"),
            additional_allocation_usd_micros: quantity("0"),
            observed_at: now(),
            source: "synthetic".into(),
            prices: vec![
                AssetPrice {
                    asset: input.clone(),
                    usd_micros_per_unit: quantity("1000000"),
                    unit_atoms: quantity("1"),
                    observed_at: now(),
                    source: "synthetic".into(),
                },
                AssetPrice {
                    asset: output.clone(),
                    usd_micros_per_unit: quantity("10000000"),
                    unit_atoms: quantity("1"),
                    observed_at: now(),
                    source: "synthetic".into(),
                },
            ],
        };
        let mapped = apply_effect_provenance(
            &mut run,
            "synthetic-conversion",
            &mut principal,
            &mut vec![],
            &effects,
            &valuation,
        )
        .unwrap();
        assert_eq!(run.provenance[0].atoms, "50");
        assert_eq!(run.provenance[1].atoms, "9");
        assert_eq!(mapped[0].amount.asset, input);
        assert_eq!(mapped[0].amount.atoms, quantity("10"));
        let available = apply_credits(&run, &[], &effects).unwrap();
        assert_eq!(available[0].amount.atoms, quantity("9"));
        assert_eq!(receipt_cash_delta(&effects, &r, &output), BigInt::from(9u8));
    }

    #[test]
    fn realized_gain_keeps_original_allocation_cap_and_requires_selected_collateral_origin() {
        let r = resource("synthetic-margin");
        let a = asset("synthetic", "USD", 0);
        let location = AccountingLocation::Cash {
            resource: r.clone(),
        };
        let baseline = StateSnapshot {
            state: FinancialState::default(),
            observed_at: now(),
            complete: true,
            coverage: vec![r.0.clone()],
            pending: vec![],
        };
        let mut run = empty_run_for_provenance(baseline);
        let initial = run.initial_allocation_usd_micros.clone();
        let effect = FinancialEffect::Settlement {
            resource: r.clone(),
            asset: a.clone(),
            category: fin_domain::SettlementCategory::RealizedPnl,
            delta: SignedQuantity::new("200").unwrap(),
        };
        let valuation = fin_domain::Valuation {
            movement_usd_micros: quantity("0"),
            maximum_fee_usd_micros: quantity("0"),
            additional_allocation_usd_micros: quantity("0"),
            observed_at: now(),
            source: "synthetic".into(),
            prices: vec![],
        };
        assert_eq!(
            apply_effect_provenance(
                &mut run,
                "gain",
                &mut vec![],
                &mut vec![],
                std::slice::from_ref(&effect),
                &valuation
            )
            .unwrap_err()
            .code,
            "SETTLEMENT_ORIGIN_MISSING"
        );
        let key = resource_asset_key(&ResourceAmount {
            resource: r,
            amount: Amount {
                asset: a.clone(),
                atoms: quantity("10"),
            },
        })
        .unwrap();
        run.provenance.push(ProvenanceLot {
            origin: format!("allocation:{key}"),
            location,
            asset: a,
            atoms: "10".into(),
        });
        apply_effect_provenance(
            &mut run,
            "gain",
            &mut vec![],
            &mut vec![],
            &[effect],
            &valuation,
        )
        .unwrap();
        assert_eq!(run.initial_allocation_usd_micros, initial);
        assert!(run.provenance[1].origin.starts_with("accrual:allocation:"));
        assert_eq!(run.provenance[1].atoms, "200");
    }

    #[tokio::test]
    async fn cancellation_targets_exact_unresolved_identity_and_never_releases_original() {
        let dir = tempfile::tempdir().unwrap();
        let engine = synthetic_engine(
            &dir.path().join("cancel.sqlite"),
            Arc::new(SyntheticAdapter::new()),
        );
        let original = engine
            .execute(OperationRequest {
                request_id: "synthetic-original".into(),
                operation: "synthetic.transfer".into(),
                input: json!({"amount":"1"}),
                run_id: None,
            })
            .await
            .unwrap();
        assert_eq!(original.status, Status::Pending);
        let mut cancellation = original.preview.clone().unwrap();
        cancellation.intent.debits.clear();
        cancellation.intent.maximum_costs.clear();
        cancellation.intent.expected = vec![EffectConstraint::SubmissionCancellation {
            operation_id: original.id.clone(),
            submission: original.submission.clone().unwrap(),
        }];
        assert!(
            engine
                .validate_submission_cancellation(&cancellation, None)
                .unwrap()
        );
        assert!(
            engine
                .validate_submission_cancellation(&cancellation, Some("different-run"))
                .is_err()
        );
        cancellation
            .intent
            .debits
            .push(original.preview.as_ref().unwrap().intent.debits[0].clone());
        assert!(
            engine
                .validate_submission_cancellation(&cancellation, None)
                .is_err()
        );
        cancellation.intent.debits.clear();
        if let EffectConstraint::SubmissionCancellation { submission, .. } =
            &mut cancellation.intent.expected[0]
        {
            submission.reference = "another-order".into();
        }
        assert!(
            engine
                .validate_submission_cancellation(&cancellation, None)
                .is_err()
        );
        assert_eq!(
            engine.status(&original.id, false).await.unwrap().status,
            Status::Pending
        );
    }
    #[test]
    fn derivative_return_projection_tracks_exposure_and_funded_realized_settlement() {
        let r = resource("synthetic-margin");
        let collateral = asset("synthetic", "USD", 0);
        let base = asset("synthetic", "BASE", 0);
        let snapshot = StateSnapshot {
            state: FinancialState {
                holdings: vec![ResourceAmount {
                    resource: r.clone(),
                    amount: Amount {
                        asset: collateral.clone(),
                        atoms: quantity("100"),
                    },
                }],
                ..Default::default()
            },
            observed_at: now(),
            complete: true,
            coverage: vec![r.0.clone()],
            pending: vec![],
        };
        let allocation = RunAllocation {
            holding: snapshot.state.holdings[0].clone(),
            value_usd_micros: "100000000".into(),
            observed_at: now(),
            source: "synthetic".into(),
        };
        let make = |index: u32, opening: bool| {
            let delta = |value: &str| SignedQuantity::new(value).unwrap();
            let lo = if opening { "0" } else { "-3" };
            let hi = if opening { "3" } else { "0" };
            ReturnRouteProof {
                step_index: index,
                operation: "synthetic.derivative".into(),
                input_hash: "synthetic".into(),
                observed_at: now(),
                expires_at: (Utc::now() + chrono::Duration::seconds(60)).to_rfc3339(),
                intent: FinancialIntent {
                    debits: vec![ResourceAmount {
                        resource: r.clone(),
                        amount: Amount {
                            asset: collateral.clone(),
                            atoms: quantity("5"),
                        },
                    }],
                    maximum_costs: vec![CostBound {
                        location: AccountingLocation::Cash {
                            resource: r.clone(),
                        },
                        amount: Amount {
                            asset: collateral.clone(),
                            atoms: quantity("5"),
                        },
                        treatment: CostTreatment::IncludedInDebit,
                    }],
                    expected: vec![
                        EffectConstraint::DerivativePosition {
                            resource: r.clone(),
                            position: "BASE:LONG".into(),
                            asset: base.clone(),
                            position_kind: PositionKind::DerivativeLong,
                            minimum_delta: delta(lo),
                            maximum_delta: delta(hi),
                        },
                        EffectConstraint::Settlement {
                            resource: r.clone(),
                            asset: collateral.clone(),
                            category: fin_domain::SettlementCategory::RealizedPnl,
                            minimum_delta: delta("-5"),
                            maximum_delta: delta("7"),
                        },
                    ],
                    locks: vec![r.clone()],
                },
                effects: vec![
                    if opening {
                        ProjectedEffect::PositionDelta {
                            resource: r.clone(),
                            position: "BASE:LONG".into(),
                            asset: base.clone(),
                            position_kind: PositionKind::DerivativeLong,
                            minimum_delta: delta("0"),
                            maximum_delta: delta("3"),
                        }
                    } else {
                        ProjectedEffect::PositionClose {
                            resource: r.clone(),
                            position: "BASE:LONG".into(),
                            asset: base.clone(),
                            position_kind: PositionKind::DerivativeLong,
                            maximum_amount: quantity("3"),
                            source_step: 0,
                        }
                    },
                    ProjectedEffect::CashDelta {
                        resource: r.clone(),
                        asset: collateral.clone(),
                        minimum_delta: delta("-5"),
                        maximum_delta: delta("7"),
                    },
                ],
                requires: vec![],
                evidence: json!({"synthetic":true}),
            }
        };
        let opening = make(0, true);
        let close = make(1, false);
        let state = projected_state(&snapshot).unwrap();
        validate_projected_cash_legs(&state, &opening).unwrap();
        let state = apply_projected_effects(&state, &opening.effects, 0).unwrap();
        validate_projected_cash_legs(&state, &close).unwrap();
        let final_state = apply_projected_effects(&state, &close.effects, 1).unwrap();
        validate_projected_restoration(&snapshot, std::slice::from_ref(&allocation), &final_state)
            .unwrap();
        let price = AssetPrice {
            asset: collateral,
            usd_micros_per_unit: quantity("1000000"),
            unit_atoms: quantity("1"),
            observed_at: now(),
            source: "synthetic".into(),
        };
        projected_provenance::validate_projected_origins(
            &snapshot,
            &[allocation],
            &[opening.clone(), close],
            &[price],
            &BigUint::from(10_000_000u64),
        )
        .unwrap();
        let mut forged = opening;
        if let ProjectedEffect::CashDelta { maximum_delta, .. } = &mut forged.effects[1] {
            *maximum_delta = SignedQuantity::new("8").unwrap();
        }
        assert!(
            validate_projected_cash_legs(&projected_state(&snapshot).unwrap(), &forged).is_err()
        );
    }
    #[test]
    fn zero_derivative_open_and_close_are_proved_observations_without_custody() {
        let a = asset("synthetic", "BASE", 0);
        let mut anchor = position_anchor(
            "synthetic-margin",
            "BASE:LONG",
            PositionKind::DerivativeLong,
            "0",
        );
        anchor.principal.asset = a.clone();
        let effect = FinancialEffect::PositionChange {
            resource: resource("synthetic-margin"),
            position: "BASE:LONG".into(),
            asset: a,
            delta: SignedQuantity::new("0").unwrap(),
            scaled_receipt: None,
            anchor: Some(anchor.clone()),
        };
        let rec = record(vec![], vec![]);
        let verified = proof(&rec, vec![effect]);
        validate_effect_anchor(&verified.effects[0], &verified.observed_at).unwrap();
        validate_anchor_transitions(&verified, &PreparationContext::default()).unwrap();
        validate_anchor_transitions(
            &verified,
            &PreparationContext {
                positions: vec![anchor],
            },
        )
        .unwrap();
        validate_origin_links(&verified.effects).unwrap();
    }

    #[tokio::test]
    async fn cancellation_sibling_settlement_leaves_pending_run_reservation_and_route_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let engine = synthetic_engine(
            &dir.path().join("cancel-scoped.sqlite"),
            Arc::new(SyntheticAdapter::new()),
        );
        let mut original = engine
            .execute(OperationRequest {
                request_id: "synthetic-scoped-original".into(),
                operation: "synthetic.transfer".into(),
                input: json!({"amount":"1"}),
                run_id: None,
            })
            .await
            .unwrap();
        let baseline = StateSnapshot {
            state: FinancialState::default(),
            observed_at: now(),
            complete: true,
            coverage: vec![],
            pending: vec![],
        };
        let mut run = empty_run_for_provenance(baseline);
        original.run_id = Some(run.id.clone());
        engine.update_record(&original).unwrap();
        run.reservations.insert(
            original.id.clone(),
            RunReservation {
                debits: original.preview.as_ref().unwrap().intent.debits.clone(),
                maximum_costs: vec![],
                provenance: vec![],
                additional_cost_provenance: vec![],
                cash_provenance_version: 0,
                cash_sources: None,
                maximum_fee_usd_micros: "0".into(),
                additional_allocation_usd_micros: "0".into(),
                binding_hash: original.approval.as_ref().unwrap().binding_hash.clone(),
                route_step_index: Some(0),
            },
        );
        engine
            .store
            .lock()
            .unwrap()
            .create_run(&run.id, &to_value(&run).unwrap(), &now())
            .unwrap();
        let before = to_value(&run).unwrap();
        let target = EffectConstraint::SubmissionCancellation {
            operation_id: original.id.clone(),
            submission: original.submission.clone().unwrap(),
        };
        let mut sibling = record(vec![target], vec![]);
        sibling.id = "synthetic-scoped-cancel".into();
        sibling.request_id = "synthetic-scoped-cancel-request".into();
        sibling.run_id = original.run_id.clone();
        sibling.preview.as_mut().unwrap().intent.debits.clear();
        sibling.preview.as_mut().unwrap().intent.locks =
            original.preview.as_ref().unwrap().intent.locks.clone();
        trading_rebind(&mut sibling);
        assert!(
            engine
                .validate_submission_cancellation(
                    sibling.preview.as_ref().unwrap(),
                    sibling.run_id.as_deref()
                )
                .unwrap()
        );
        engine
            .store
            .lock()
            .unwrap()
            .create_request(
                &sibling.id,
                &sibling.request_id,
                &sibling.fingerprint,
                &sibling.operation,
                sibling.run_id.as_deref(),
                "pending",
                &to_value(&sibling).unwrap(),
                &now(),
            )
            .unwrap();
        let verified = proof(
            &sibling,
            vec![FinancialEffect::SubmissionCancellation {
                operation_id: original.id.clone(),
                submission: original.submission.clone().unwrap(),
            }],
        );
        let completed = engine
            .apply_reconciliation(
                sibling,
                Reconciliation {
                    status: Status::Completed,
                    evidence: json!({"synthetic":"cancelled remainder"}),
                    verified: Some(verified),
                },
            )
            .unwrap();
        assert_eq!(completed.status, Status::Completed);
        assert_eq!(
            to_value(&engine.load_run(&run.id).unwrap()).unwrap(),
            before
        );
        assert_eq!(
            engine.status(&original.id, false).await.unwrap().status,
            Status::Pending
        );
    }
    #[test]
    fn projected_position_close_requires_proved_zero_exposure_after_partial_or_no_fill() {
        let collateral = asset("ethereum:1", "USDC", 6);
        let record = record(vec![], vec![]);
        let route = ReturnRouteProof {
            step_index: 1,
            operation: "synthetic.derivative.close".into(),
            input_hash: "synthetic".into(),
            observed_at: now(),
            expires_at: now(),
            intent: FinancialIntent::default(),
            effects: vec![ProjectedEffect::PositionClose {
                resource: resource("synthetic-margin"),
                position: "BASE:LONG".into(),
                asset: collateral.clone(),
                position_kind: PositionKind::DerivativeLong,
                maximum_amount: quantity("3"),
                source_step: 0,
            }],
            requires: vec![],
            evidence: json!({"synthetic":true}),
        };
        for (delta, post, matches) in [
            ("-1", "2", false),
            ("0", "3", false),
            ("-3", "0", true),
            ("0", "0", true),
        ] {
            let anchor = position_anchor(
                "synthetic-margin",
                "BASE:LONG",
                PositionKind::DerivativeLong,
                post,
            );
            let receipt = proof(
                &record,
                vec![FinancialEffect::PositionChange {
                    resource: resource("synthetic-margin"),
                    position: "BASE:LONG".into(),
                    asset: collateral.clone(),
                    delta: SignedQuantity::new(delta).unwrap(),
                    scaled_receipt: None,
                    anchor: Some(anchor),
                }],
            );
            assert_eq!(
                route_projection_matches_receipt(&route, &receipt).unwrap(),
                matches,
                "delta {delta}, remaining {post}"
            );
        }
    }
    fn ordinary_request(identity: &str) -> OperationRequest {
        OperationRequest {
            request_id: identity.into(),
            operation: "synthetic.transfer".into(),
            input: json!({"amount":"1"}),
            run_id: None,
        }
    }

    #[tokio::test]
    async fn ordinary_completed_holdings_need_no_run_or_restoration_and_keep_gross_allocation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ordinary.sqlite3");
        let adapter = Arc::new(SyntheticAdapter::new());
        adapter.completed_response();
        let mut engine = synthetic_engine(&path, adapter.clone());
        engine.options.max_run_allocation_usd_micros = "1500000".into();
        // An old unrestored verification document is archival, never an execution gate.
        let history = json!({"id":"legacy","status":"cleanup_outstanding","purpose":"verification","reservations":{},"baseline":{"fixture":"preserve"}});
        engine
            .store
            .lock()
            .unwrap()
            .create_run("legacy", &history, &now())
            .unwrap();
        let completed = engine
            .execute(ordinary_request("ordinary-first"))
            .await
            .unwrap();
        assert_eq!(completed.status, Status::Completed);
        assert!(completed.run_id.is_none());
        assert_eq!(engine.allocation_used_usd_micros().unwrap(), "1000000");
        assert_eq!(engine.historical_run("legacy").unwrap(), history);
        drop(engine);
        let mut restarted = synthetic_engine(&path, adapter.clone());
        restarted.options.max_run_allocation_usd_micros = "1500000".into();
        let denied = restarted
            .execute(ordinary_request("ordinary-second"))
            .await
            .unwrap();
        assert_eq!(denied.error.unwrap().code, "ALLOCATION_LIMIT");
        assert_eq!(adapter.submissions.load(Ordering::SeqCst), 1);
        let replay = restarted
            .execute(ordinary_request("ordinary-first"))
            .await
            .unwrap();
        assert_eq!(replay.id, completed.id);
        assert_eq!(replay.submission, completed.submission);
        assert_eq!(replay.effects, completed.effects);
        // Only terminal submitted allocation ages out, at least24h after settlement.
        let mut aged = completed;
        aged.updated_at = (Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
        restarted.update_record(&aged).unwrap();
        assert_eq!(restarted.allocation_used_usd_micros().unwrap(), "0");
    }

    #[tokio::test]
    async fn ordinary_pending_reservation_outlives_window_and_zero_does_not_unlock_it() {
        let directory = tempfile::tempdir().unwrap();
        let adapter = Arc::new(SyntheticAdapter::new());
        let mut engine =
            synthetic_engine(&directory.path().join("pending.sqlite3"), adapter.clone());
        engine.options.max_movement_and_cost_usd_micros = "0".into();
        engine.options.max_run_allocation_usd_micros = "0".into();
        let mut pending = engine
            .execute(ordinary_request("pending-first"))
            .await
            .unwrap();
        assert_eq!(pending.status, Status::Pending);
        pending.created_at = (Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
        pending.updated_at = pending.created_at.clone();
        engine.update_record(&pending).unwrap();
        assert_eq!(engine.allocation_used_usd_micros().unwrap(), "1000000");
        engine.recover_interrupted().unwrap();
        let blocked = engine
            .execute(ordinary_request("pending-second"))
            .await
            .unwrap();
        assert_eq!(blocked.error.unwrap().code, "RESOURCE_RESERVED");
        assert_eq!(adapter.submissions.load(Ordering::SeqCst), 1);
        assert_eq!(
            engine
                .operations()
                .unwrap()
                .into_iter()
                .find(|row| row.id == pending.id)
                .unwrap()
                .submission,
            pending.submission
        );
    }

    #[tokio::test]
    async fn ordinary_finite_movement_and_zero_policy_are_enforced_before_approval() {
        for (limit, expected) in [
            ("999999", Some("MOVEMENT_LIMIT")),
            ("1000000", None),
            ("0", None),
        ] {
            let adapter = Arc::new(SyntheticAdapter::new());
            let auth = Arc::new(ChallengeValidatingAuthorizer::default());
            let engine = Engine::new(
                Store::in_memory().unwrap(),
                vec![adapter.clone()],
                auth.clone(),
                EngineOptions {
                    max_movement_and_cost_usd_micros: limit.into(),
                    max_run_allocation_usd_micros: "0".into(),
                    allow_unpriced_exchange_assets: false,
                },
            )
            .unwrap();
            let result = engine
                .execute(ordinary_request("finite-zero"))
                .await
                .unwrap();
            assert_eq!(result.error.as_ref().map(|e| e.code.as_str()), expected);
            assert_eq!(
                auth.challenges.lock().unwrap().len(),
                usize::from(expected.is_none())
            );
            assert_eq!(
                adapter.submissions.load(Ordering::SeqCst),
                usize::from(expected.is_none())
            );
        }
    }

    struct RefusingApproval(bool);
    #[async_trait]
    impl Authorizer for RefusingApproval {
        async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
            if self.0 {
                return Err(FinError::new(
                    "APPROVAL_DENIED",
                    "approval",
                    "Synthetic denied approval",
                    "Retry manually",
                ));
            }
            Ok(ApprovalAudit {
                mechanism: "synthetic-expired".into(),
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: (Utc::now() - chrono::Duration::seconds(120)).to_rfc3339(),
                expires_at: (Utc::now() - chrono::Duration::seconds(30)).to_rfc3339(),
            })
        }
    }
    #[tokio::test]
    async fn ordinary_denied_or_expired_approval_releases_only_unsubmitted_allocation() {
        for denied in [true, false] {
            let adapter = Arc::new(SyntheticAdapter::new());
            let engine = Engine::new(
                Store::in_memory().unwrap(),
                vec![adapter.clone()],
                Arc::new(RefusingApproval(denied)),
                EngineOptions::default(),
            )
            .unwrap();
            let first = engine
                .execute(ordinary_request("refused-first"))
                .await
                .unwrap();
            assert!(first.status.terminal());
            assert!(first.submission.is_none());
            assert_eq!(engine.allocation_used_usd_micros().unwrap(), "0");
            let second = engine
                .execute(ordinary_request("refused-second"))
                .await
                .unwrap();
            assert_ne!(second.error.as_ref().unwrap().code, "RESOURCE_RESERVED");
            assert_eq!(adapter.submissions.load(Ordering::SeqCst), 0);
        }
    }
    #[tokio::test]
    async fn ordinary_policy_counts_costs_and_borrowing_with_no_hidden_verification_ceiling() {
        for (movement, allocation, expected) in [
            ("154999999", "0", Some("MOVEMENT_LIMIT")),
            ("155000000", "164999999", Some("ALLOCATION_LIMIT")),
            ("155000000", "165000000", None),
            ("0", "0", None),
        ] {
            let adapter = Arc::new(SyntheticAdapter::new());
            let engine = Engine::new(
                Store::in_memory().unwrap(),
                vec![adapter.clone()],
                Arc::new(SyntheticAuthorizer),
                EngineOptions {
                    max_movement_and_cost_usd_micros: movement.into(),
                    max_run_allocation_usd_micros: allocation.into(),
                    allow_unpriced_exchange_assets: false,
                },
            )
            .unwrap();
            let mut request = ordinary_request("above-former-test-budget");
            request.input = json!({"movementMicros":"150000000","feeMicros":"5000000","borrowMicros":"10000000"});
            let result = engine.execute(request).await.unwrap();
            assert_eq!(result.error.as_ref().map(|e| e.code.as_str()), expected);
            assert_eq!(
                adapter.submissions.load(Ordering::SeqCst),
                usize::from(expected.is_none())
            );
            assert_eq!(
                engine.allocation_used_usd_micros().unwrap(),
                if expected.is_none() { "165000000" } else { "0" }
            );
        }
    }
    struct UnpricedAdapter {
        inner: SyntheticAdapter,
        malformed: Option<&'static str>,
    }
    #[async_trait]
    impl Adapter for UnpricedAdapter {
        fn definitions(&self) -> Vec<Definition> {
            self.inner.definitions()
        }
        async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
            self.inner.read(operation, input).await
        }
        async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
            let mut prepared = self.inner.prepare(operation, input).await?;
            let exact_asset = asset("binance", "ETH", 18);
            let mut row = AssetPrice {
                asset: exact_asset.clone(),
                unit_atoms: quantity("1000000000000000000"),
                usd_micros_per_unit: Quantity::zero(),
                source: fin_domain::unpriced::source(&exact_asset),
                observed_at: now(),
            };
            match self.malformed {
                Some("forged") => row.source = "SYNTHETIC zero quote".into(),
                Some("onchain") => {
                    row.asset.network = "ethereum:1".into();
                    row.source = fin_domain::unpriced::source(&row.asset);
                }
                Some("stale") => row.observed_at = "2000-01-01T00:00:00Z".into(),
                _ => {}
            }
            let value = prepared.valuation.as_mut().unwrap();
            value.movement_usd_micros = quantity(input["pricedMicros"].as_str().unwrap_or("0"));
            value.prices = vec![row];
            prepared.warnings.push(fin_domain::unpriced::WARNING.into());
            Ok(prepared)
        }
        async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
            Ok(prepared.clone())
        }
        async fn submit(
            &self,
            prepared: &PreparedOperation,
            journal: Arc<dyn SubmissionJournal>,
        ) -> Result<Submission> {
            self.inner.submit(prepared, journal).await
        }
        async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
            self.inner.reconcile(record).await
        }
    }

    #[tokio::test]
    async fn unpriced_ordinary_exception_is_protected_and_keeps_priced_caps_and_approval() {
        for (enabled, priced, movement, allocation, expected) in [
            (
                false,
                "0",
                "0",
                "0",
                Some("UNPRICED_EXCHANGE_ASSET_DISABLED"),
            ),
            (true, "0", "1000000", "1000000", None),
            (true, "1000001", "1000000", "0", Some("MOVEMENT_LIMIT")),
            (true, "1000001", "0", "1000000", Some("ALLOCATION_LIMIT")),
        ] {
            let adapter = Arc::new(UnpricedAdapter {
                inner: SyntheticAdapter::new(),
                malformed: None,
            });
            let auth = Arc::new(ChallengeValidatingAuthorizer::default());
            let engine = Engine::new(
                Store::in_memory().unwrap(),
                vec![adapter.clone()],
                auth.clone(),
                EngineOptions {
                    allow_unpriced_exchange_assets: enabled,
                    max_movement_and_cost_usd_micros: movement.into(),
                    max_run_allocation_usd_micros: allocation.into(),
                },
            )
            .unwrap();
            let mut request = ordinary_request("SYNTHETIC-unpriced");
            request.input["pricedMicros"] = json!(priced);
            let result = engine.execute(request.clone()).await.unwrap();
            assert_eq!(result.error.as_ref().map(|e| e.code.as_str()), expected);
            assert_eq!(
                auth.challenges.lock().unwrap().len(),
                usize::from(expected.is_none())
            );
            assert_eq!(
                adapter.inner.submissions.load(Ordering::SeqCst),
                usize::from(expected.is_none())
            );
            if expected.is_none() {
                assert_eq!(engine.allocation_unpriced_operations().unwrap(), 1);
                assert_eq!(engine.execute(request).await.unwrap().id, result.id);
                assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
            }
        }
    }

    #[tokio::test]
    async fn unpriced_forged_onchain_and_stale_zero_rows_are_rejected_before_approval() {
        for malformed in ["forged", "onchain", "stale"] {
            let adapter = Arc::new(UnpricedAdapter {
                inner: SyntheticAdapter::new(),
                malformed: Some(malformed),
            });
            let auth = Arc::new(ChallengeValidatingAuthorizer::default());
            let engine = Engine::new(
                Store::in_memory().unwrap(),
                vec![adapter.clone()],
                auth.clone(),
                EngineOptions {
                    allow_unpriced_exchange_assets: true,
                    ..Default::default()
                },
            )
            .unwrap();
            let result = engine
                .execute(ordinary_request("SYNTHETIC-bad-zero"))
                .await
                .unwrap();
            assert!(result.error.is_some(), "{malformed}");
            assert!(auth.challenges.lock().unwrap().is_empty());
            assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn retained_unpriced_submission_reconciles_after_exception_is_disabled() {
        let adapter = Arc::new(UnpricedAdapter {
            inner: SyntheticAdapter::new(),
            malformed: None,
        });
        let mut engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![adapter.clone()],
            Arc::new(SyntheticAuthorizer),
            EngineOptions {
                allow_unpriced_exchange_assets: true,
                ..Default::default()
            },
        )
        .unwrap();
        let original = engine
            .execute(ordinary_request("SYNTHETIC-retained-unpriced"))
            .await
            .unwrap();
        assert_eq!(original.status, Status::Pending);
        engine.options.allow_unpriced_exchange_assets = false;
        adapter.inner.completed_response();
        let settled = engine.status(&original.id, true).await.unwrap();
        assert_eq!(settled.status, Status::Completed);
        assert_eq!(settled.submission, original.submission);
        assert_eq!(settled.preview, original.preview);
        assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
        assert_eq!(engine.allocation_used_usd_micros().unwrap(), "0");
        assert_eq!(engine.allocation_unpriced_operations().unwrap(), 1);
    }

    #[tokio::test]
    async fn unpriced_costs_settle_as_estimates_but_cannot_define_acceptance_runs() {
        let exact_asset = asset("binance", "SYNTHETIC", 8);
        let row = AssetPrice {
            asset: exact_asset.clone(),
            unit_atoms: quantity("100000000"),
            usd_micros_per_unit: Quantity::zero(),
            observed_at: now(),
            source: fin_domain::unpriced::source(&exact_asset),
        };
        let valuation = fin_domain::Valuation {
            movement_usd_micros: Quantity::zero(),
            maximum_fee_usd_micros: Quantity::zero(),
            additional_allocation_usd_micros: Quantity::zero(),
            observed_at: now(),
            source: "SYNTHETIC estimate".into(),
            prices: vec![row],
        };
        let costs = vec![FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: resource("SYNTHETIC-exchange"),
            },
            amount: Amount {
                asset: exact_asset,
                atoms: quantity("100"),
            },
            category: "SYNTHETIC trading fee".into(),
            treatment: fin_domain::CostTreatment::AdditionalDebit,
        }];
        assert_eq!(
            actual_cost_usd(&costs, &valuation).unwrap(),
            BigUint::zero()
        );
        let mut prepared = SyntheticAdapter::new()
            .prepare("synthetic.transfer", &json!({}))
            .await
            .unwrap();
        prepared.valuation = Some(valuation);
        assert_eq!(
            reject_unpriced_run(&prepared).unwrap_err().code,
            "UNPRICED_ASSET"
        );
    }
}
