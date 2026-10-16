//! Durable personal policy for ordinary operations. No test-run authority.
use super::*;
use gofin_domain::product_names::MetadataValueExt;

fn budget_error(code: &str, message: &str) -> GoFinError {
    GoFinError::new(
        code,
        "budget",
        message,
        "Inspect policy and pending operations. Reconcile the original submission; a new request identity does not release its allocation.",
    )
}

fn charge(prepared: &PreparedOperation) -> Result<BigUint> {
    // A verified cancellation-only operation owns no fills, movement or fees.
    // The original order keeps its own allocation and unresolved reservation.
    if cancellation_target(&prepared.intent).is_some()
        && prepared.intent.debits.is_empty()
        && prepared.intent.maximum_costs.is_empty()
    {
        return Ok(BigUint::zero());
    }
    let value = prepared.valuation.as_ref().ok_or_else(|| {
        budget_error(
            "VALUATION_UNAVAILABLE",
            "An exact USD valuation is required for durable allocation accounting.",
        )
    })?;
    Ok(value.movement_usd_micros.integer()
        + value.maximum_fee_usd_micros.integer()
        + value.additional_allocation_usd_micros.integer())
}

pub(super) fn record_charge(record: &OperationRecord) -> Result<BigUint> {
    let Some(prepared) = record.preview.as_ref() else {
        return Err(budget_error(
            "VALUATION_UNAVAILABLE",
            "A historical submission has no recorded valuation.",
        ));
    };
    if cost_attention_pending(record) {
        // An externally incurred overrun is never capped or valued at invented
        // zero for future policy checks. Retain every other approved slice and
        // enlarge only the proved cost slice using the frozen exact price map.
        let unknown = || {
            budget_error(
                "ALLOCATION_COST_UNKNOWN",
                "An unresolved proved cost has no trustworthy exact valuation; writes remain blocked until its original evidence and pricing are restored.",
            )
        };
        let evidence = record
            .last_reconciliation_evidence
            .as_ref()
            .ok_or_else(unknown)?;
        let verified: VerifiedEffects =
            from_value(evidence["verified_effects"].clone()).map_err(|_| unknown())?;
        let outcome: Status =
            from_value(evidence["reported_status"].clone()).map_err(|_| unknown())?;
        if !matches!(outcome, Status::Completed | Status::Failed) {
            return Err(unknown());
        }
        if let Err(rejection) = validate_effects_mode(record, &verified, &outcome, false)
            && !matches!(
                rejection.code.as_str(),
                "COST_EXCEEDS_RESERVATION"
                    | "EFFECT_EXCEEDS_RESERVATION"
                    | "COST_EXCEEDS_USD_RESERVATION"
            )
        {
            return Err(unknown());
        }
        let valuation = prepared.valuation.as_ref().ok_or_else(unknown)?;
        let actual_cost = actual_cost_usd(&verified.effects, valuation).map_err(|_| unknown())?;
        let approved_cost = valuation.maximum_fee_usd_micros.integer();
        return Ok(charge(prepared)? - &approved_cost + approved_cost.max(actual_cost));
    }
    // A terminal proof with no financial effects proves that no principal,
    // exposure or fees were consumed. Keep its immutable submission identity,
    // but do not charge an unused approval ceiling as rolling expenditure.
    if record.status == Status::Failed
        && record
            .effects
            .as_ref()
            .is_some_and(|proof| proof.effects.is_empty())
    {
        return Ok(BigUint::zero());
    }
    if record.status.terminal()
        && prepared
            .intent
            .maximum_costs
            .iter()
            .any(|cost| cost.treatment == CostTreatment::VenueCalculatedAdditionalDebit)
    {
        let proof = record.effects.as_ref().ok_or_else(|| {
            budget_error(
                "ALLOCATION_COST_UNKNOWN",
                "A venue-calculated fee has no terminal financial proof.",
            )
        })?;
        // The approval-bound exception authorizes the actual fee, but does not
        // allow rolling policy to keep charging only the smaller initial reserve.
        validate_effects_mode(record, proof, &record.status, false)?;
        let valuation = prepared.valuation.as_ref().ok_or_else(|| {
            budget_error(
                "ALLOCATION_COST_UNKNOWN",
                "A venue-calculated fee has no frozen USD valuation.",
            )
        })?;
        let actual = actual_cost_usd(&proof.effects, valuation)?;
        let reserve = valuation.maximum_fee_usd_micros.integer();
        return Ok(charge(prepared)? - &reserve + reserve.max(actual));
    }
    let bounded = prepared
        .binding
        .product_get(gofin_domain::boundaries::BOUNDARY_METADATA_KEY)?
        .is_some()
        || prepared
            .binding
            .product_get(gofin_domain::financial_plans::PLAN_METADATA_KEY)?
            .is_some();
    if !bounded || !record.status.terminal() {
        return charge(prepared);
    }
    let exact_plan_input = prepared
        .binding
        .product_get(gofin_domain::financial_plans::PLAN_METADATA_KEY)?
        .and_then(|metadata| metadata.get("exact_input"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if exact_plan_input {
        // Generic exact scopes can represent position, debt, collateral, and
        // derivative notional effects that are not cash debits. Keep the
        // approved conservative valuation rather than inferring exposure
        // from one subset of typed effects.
        return charge(prepared);
    }
    let Some(verified) = &record.effects else {
        return Ok(BigUint::zero());
    };
    let valuation = prepared.valuation.as_ref().ok_or_else(|| {
        budget_error(
            "VALUATION_UNAVAILABLE",
            "A bounded attempt has no recorded pricing evidence.",
        )
    })?;
    actual_effect_charge(&verified.effects, &valuation.prices)
}

pub(super) fn actual_effect_charge(
    effects: &[gofin_domain::FinancialEffect],
    prices: &[gofin_domain::AssetPrice],
) -> Result<BigUint> {
    let mut total = BigUint::zero();
    for effect in effects {
        match effect {
            gofin_domain::FinancialEffect::CashDebit { amount, .. } => {
                total += amount_usd(amount, prices)?;
            }
            gofin_domain::FinancialEffect::Cost {
                location,
                amount,
                treatment: CostTreatment::IncludedInDebit,
                ..
            } => {
                // Signed net settlements contain gross losses which later gains
                // may offset. Charge those losses unless another typed gross
                // origin already contains them; rebates/positive net settlement
                // cannot erase consumed expenditure.
                let settlement = matches!(location, AccountingLocation::Cash { resource }
                    if effects.iter().any(|effect| matches!(effect, gofin_domain::FinancialEffect::Settlement { resource:r, asset, .. } if r==resource && asset==&amount.asset)));
                let gross_origin = effects.iter().any(|effect| match (location, effect) {
                    (
                        AccountingLocation::Cash { resource },
                        gofin_domain::FinancialEffect::CashDebit {
                            resource: r,
                            amount: debit,
                        },
                    ) => r == resource && debit.asset == amount.asset,
                    (
                        AccountingLocation::Cash { resource },
                        gofin_domain::FinancialEffect::Transfer { from, debit, .. },
                    ) => from == resource && debit.asset == amount.asset,
                    (
                        AccountingLocation::Cash { resource },
                        gofin_domain::FinancialEffect::Conversion {
                            resource: r, spent, ..
                        },
                    ) => r == resource && spent.asset == amount.asset,
                    (_, gofin_domain::FinancialEffect::OriginMovement { from, debit, .. }) => {
                        from == location && debit.asset == amount.asset
                    }
                    _ => false,
                });
                if settlement && !gross_origin {
                    total += amount_usd(amount, prices)?;
                }
            }
            gofin_domain::FinancialEffect::Cost { amount, .. } => {
                total += amount_usd(amount, prices)?;
            }
            gofin_domain::FinancialEffect::Transfer { debit, .. } => {
                total += amount_usd(debit, prices)?;
            }
            gofin_domain::FinancialEffect::Conversion { spent, .. } => {
                total += amount_usd(spent, prices)?;
            }
            gofin_domain::FinancialEffect::EscrowProgress {
                input_asset,
                wallet_debit,
                ..
            }
            | gofin_domain::FinancialEffect::EscrowPositionProgress {
                input_asset,
                wallet_debit,
                ..
            } if !wallet_debit.is_zero() => {
                total += amount_usd(
                    &gofin_domain::Amount {
                        asset: input_asset.clone(),
                        atoms: wallet_debit.clone(),
                    },
                    prices,
                )?;
            }
            gofin_domain::FinancialEffect::AccountFundingProgress { asset, funded, .. }
                if !funded.is_zero() =>
            {
                // Refundable rent/account funding is still gross allocation
                // even when later returned. Cumulative lifecycle effects carry
                // the original funded amount, so this is charged once.
                total += amount_usd(
                    &gofin_domain::Amount {
                        asset: asset.clone(),
                        atoms: funded.clone(),
                    },
                    prices,
                )?;
            }
            gofin_domain::FinancialEffect::ConversionSequence { execution } => {
                for leg in &execution.legs {
                    total += amount_usd(&leg.spent, prices)?;
                }
                for cost in &execution.costs {
                    if cost.treatment != CostTreatment::IncludedInDebit {
                        total += amount_usd(&cost.amount, prices)?;
                    }
                }
            }
            gofin_domain::FinancialEffect::DebtChange { asset, delta, .. }
                if !delta.is_negative() && !delta.is_zero() =>
            {
                // Borrowed principal increases the protected allocation even
                // though it is a credit to the wallet, not a cash debit.
                total += amount_usd(
                    &gofin_domain::Amount {
                        asset: asset.clone(),
                        atoms: delta.magnitude(),
                    },
                    prices,
                )?;
            }
            _ => {}
        }
    }
    Ok(total)
}

pub(super) fn amount_usd(
    amount: &gofin_domain::Amount,
    prices: &[gofin_domain::AssetPrice],
) -> Result<BigUint> {
    let price = prices
        .iter()
        .find(|price| price.asset == amount.asset)
        .ok_or_else(|| {
            budget_error(
                "VALUATION_UNAVAILABLE",
                "Verified consumption has no matching recorded USD price.",
            )
        })?;
    Ok(amount
        .atoms
        .mul_div_ceil(&price.usd_micros_per_unit, &price.unit_atoms)?
        .integer())
}

pub(super) fn resources(intent: &FinancialIntent) -> BTreeSet<ResourceId> {
    let mut result: BTreeSet<_> = intent.locks.iter().cloned().collect();
    result.extend(intent.debits.iter().map(|v| v.resource.clone()));
    result.extend(intent.maximum_costs.iter().map(|v| match &v.location {
        AccountingLocation::Cash { resource } | AccountingLocation::Position { resource, .. } => {
            resource.clone()
        }
    }));
    result
}

fn amendment_scope(prepared: &PreparedOperation) -> Result<Option<(String, BTreeSet<ResourceId>)>> {
    let Some(binding) = prepared.binding.product_get("__gofin_amendment")? else {
        return Ok(None);
    };
    let Some(object) = binding.as_object() else {
        return Err(budget_error(
            "AMENDMENT_BINDING_INVALID",
            "The prepared amendment binding is malformed.",
        ));
    };
    let parent_id = binding
        .get("parent_operation_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let request_id = binding
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let shared_locks = binding.get("shared_locks").and_then(Value::as_array);
    if object.len() != 5
        || binding.get("version").and_then(Value::as_u64) != Some(1)
        || parent_id.is_none()
        || request_id.is_none()
        || binding.get("parent_submission").is_none_or(Value::is_null)
        || shared_locks.is_none_or(|locks| locks.is_empty())
    {
        return Err(budget_error(
            "AMENDMENT_BINDING_INVALID",
            "The prepared amendment binding is incomplete or unsupported.",
        ));
    }
    let mut locks = BTreeSet::new();
    for value in shared_locks.unwrap() {
        let Some(value) = value.as_str().filter(|value| !value.trim().is_empty()) else {
            return Err(budget_error(
                "AMENDMENT_BINDING_INVALID",
                "The amendment lock scope must contain exact resource identities.",
            ));
        };
        if !locks.insert(ResourceId(value.to_owned())) {
            return Err(budget_error(
                "AMENDMENT_BINDING_INVALID",
                "The amendment lock scope contains duplicate identities.",
            ));
        }
    }
    Ok(Some((parent_id.unwrap().to_owned(), locks)))
}

fn protection_parent_scope(
    prepared: &PreparedOperation,
) -> Result<Option<(String, BTreeSet<ResourceId>)>> {
    let Some(binding) = prepared.binding.product_get("__gofin_protection_parent")? else {
        return Ok(None);
    };
    let Some(object) = binding.as_object() else {
        return Err(budget_error(
            "PROTECTION_BINDING_INVALID",
            "The prepared protection-parent binding is malformed.",
        ));
    };
    let parent_id = binding
        .get("parent_operation_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let parent_request = binding
        .get("parent_request_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let shared_locks = binding.get("shared_locks").and_then(Value::as_array);
    if object.len() != 5
        || binding.get("version").and_then(Value::as_u64) != Some(1)
        || parent_id.is_none()
        || parent_request.is_none()
        || binding.get("parent_submission").is_none_or(Value::is_null)
        || shared_locks.is_none_or(|locks| locks.len() != 1)
    {
        return Err(budget_error(
            "PROTECTION_BINDING_INVALID",
            "The prepared protection-parent binding is incomplete or unsupported.",
        ));
    }
    let mut locks = BTreeSet::new();
    for value in shared_locks.unwrap() {
        let Some(value) = value.as_str().filter(|value| !value.trim().is_empty()) else {
            return Err(budget_error(
                "PROTECTION_BINDING_INVALID",
                "The protection lock must contain one exact resource identity.",
            ));
        };
        if !locks.insert(ResourceId(value.to_owned())) {
            return Err(budget_error(
                "PROTECTION_BINDING_INVALID",
                "The protection lock scope contains duplicate identities.",
            ));
        }
    }
    Ok(Some((parent_id.unwrap().to_owned(), locks)))
}

/// Compute the exclusive scope still owned by an unresolved operation. A
/// finalized open escrow owns its escrow/order identity while the original
/// wallet debit and one-shot transaction costs have already been proved. The
/// exact remaining cash reservations are tracked separately by Store and the
/// rolling allocation charge; treating the whole wallet as an exclusive lock
/// would prevent unrelated actions for the lifetime of a resting order.
fn conflict_resources(record: &OperationRecord) -> Result<BTreeSet<ResourceId>> {
    let Some(prepared) = record.preview.as_ref() else {
        return Err(budget_error(
            "RESOURCE_RESERVED",
            "An unresolved operation has no inspectable resource reservation.",
        ));
    };
    let released_wallet_locks = if record.status.terminal() {
        BTreeSet::new()
    } else {
        super::proved_escrow_wallet_locks(
            prepared,
            record
                .effects
                .as_ref()
                .map(|effects| effects.effects.as_slice())
                .unwrap_or_default(),
        )
    };
    let mut result = prepared
        .intent
        .locks
        .iter()
        .filter(|resource| !released_wallet_locks.contains(*resource))
        .cloned()
        .collect::<BTreeSet<_>>();

    // Cash and position constraints remain exclusive while an amount is still
    // reserved. The open-order wallet's settled principal/transaction fee no
    // longer belongs in that scope; its operation-specific order lock remains.
    let maximum_debits = aggregate_resource_amounts(&prepared.intent.debits)?;
    let actual_debits = record
        .effects
        .as_ref()
        .map(|effects| actual_cash_debits(&effects.effects, false))
        .transpose()?
        .unwrap_or_default();
    for debit in &prepared.intent.debits {
        let key = resource_asset_key(debit)?;
        if maximum_debits.get(&key).cloned().unwrap_or_default()
            > actual_debits.get(&key).cloned().unwrap_or_default()
        {
            result.insert(debit.resource.clone());
        }
    }
    if released_wallet_locks.is_empty() {
        result.extend(
            prepared
                .intent
                .maximum_costs
                .iter()
                .map(|cost| match &cost.location {
                    AccountingLocation::Cash { resource }
                    | AccountingLocation::Position { resource, .. } => resource.clone(),
                }),
        );
    } else {
        result.extend(prepared.intent.maximum_costs.iter().filter_map(|cost| {
            let resource = match &cost.location {
                AccountingLocation::Cash { resource }
                | AccountingLocation::Position { resource, .. } => resource,
            };
            (!released_wallet_locks.contains(resource)).then_some(resource.clone())
        }));
    }
    Ok(result)
}

fn counted(record: &OperationRecord, instant: DateTime<Utc>) -> Result<bool> {
    if !record.status.terminal() {
        return Ok(record.preview.is_some() || record.submission.is_some());
    }
    // Submitted fees and turnover remain charged even for proven failed writes.
    Ok(record.submission.is_some()
        && parse_time(&record.updated_at)? > instant - chrono::Duration::hours(24))
}

fn counted_with_amendment_owner(
    record: &OperationRecord,
    records: &BTreeMap<String, OperationRecord>,
    instant: DateTime<Utc>,
) -> Result<bool> {
    if record.status.terminal()
        && record.submission.is_some()
        && let Some(prepared) = &record.preview
        && let Some(children) = compound_mutations::children(&prepared.intent)?
    {
        for child in children {
            let original = records.get(&child.operation_id).ok_or_else(|| {
                budget_error(
                    "AMENDMENT_OWNER_MISSING",
                    "A compound mutation's original accounting owner is missing.",
                )
            })?;
            if counted(original, instant)? {
                return Ok(true);
            }
        }
    }
    if record.status.terminal()
        && record.submission.is_some()
        && let Some(prepared) = &record.preview
    {
        for group in grouped_amendments::groups(&prepared.intent)? {
            let owner = records.get(&group.operation_id).ok_or_else(|| {
                budget_error(
                    "AMENDMENT_OWNER_MISSING",
                    "The grouped original owner is missing.",
                )
            })?;
            if counted(owner, instant)? {
                return Ok(true);
            }
        }
    }
    if record.status == Status::Completed
        && let Some((original_id, _, _)) = record
            .preview
            .as_ref()
            .and_then(|p| amendments::target(&p.intent))
    {
        let original=records.get(original_id).ok_or_else(||budget_error("AMENDMENT_OWNER_MISSING","A proved amendment's original accounting owner is missing; its allocation cannot expire safely."))?;
        // Positive amendment reserves transfer to the original unresolved order.
        // Preserve the delta beyond24h and through the original final settlement window.
        return counted(original, instant);
    }
    counted(record, instant)
}
fn operation_index(records: Vec<OperationRecord>) -> BTreeMap<String, OperationRecord> {
    records
        .into_iter()
        .map(|record| (record.id.clone(), record))
        .collect()
}

fn unpriced(prepared: &PreparedOperation) -> bool {
    prepared
        .valuation
        .as_ref()
        .is_some_and(|value| value.prices.iter().any(gofin_domain::unpriced::is_estimate))
}

pub(super) fn reject_unpriced_run(prepared: &PreparedOperation) -> Result<()> {
    if unpriced(prepared) {
        return Err(budget_error(
            "UNPRICED_ASSET",
            "Unpriced exchange estimates cannot satisfy a scoped acceptance run's verified USD budget.",
        ));
    }
    Ok(())
}

impl Engine {
    pub fn allocation_unpriced_operations(&self) -> Result<u64> {
        let instant = self.clock.now();
        let mut count = 0;
        let records = operation_index(self.operations()?);
        for record in records.values() {
            if counted_with_amendment_owner(record, &records, instant)? {
                let preview = record.preview.as_ref().ok_or_else(|| budget_error("VALUATION_UNAVAILABLE", "A historical submission has no recorded valuation; preserve and reconcile it."))?;
                if unpriced(preview) {
                    count += 1;
                }
            }
        }
        Ok(count)
    }
    pub fn allocation_used_usd_micros(&self) -> Result<String> {
        let mut total = BigUint::zero();
        let instant = self.clock.now();
        let (owners, owner_operations, transfers, operations) = {
            let store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            let operation_rows = store.list_records().map_err(map_store)?;
            let operations = operation_rows
                .into_iter()
                .map(from_value::<OperationRecord>)
                .collect::<Result<Vec<_>>>()?;
            let owners = store.bridge_allocation_owners().map_err(map_store)?;
            let operation_ids = owners
                .iter()
                .map(|row| row.0.clone())
                .collect::<BTreeSet<_>>();
            let transfers = owners
                .iter()
                .map(|row| store.bridge_transfer(&row.1).map_err(map_store))
                .collect::<Result<Vec<_>>>()?;
            (owners, operation_ids, transfers, operations)
        };
        let operation_index = operation_index(operations);
        for record in operation_index.values() {
            // The row's single allocation charge moved atomically to its bridge
            // transfer owner after source proof; never count both identities.
            if owner_operations.contains(&record.id) {
                continue;
            }
            if counted_with_amendment_owner(record, &operation_index, instant)? {
                let store = self
                    .store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?;
                total += unresolved_consumption::record_charge(&store, record)?;
            }
        }
        for (owner, transfer) in owners.iter().zip(transfers.iter()) {
            let unresolved = transfer.as_ref().is_some_and(|record| {
                !matches!(
                    record.state,
                    gofin_domain::BridgeTransferState::Completed
                        | gofin_domain::BridgeTransferState::Refunded
                        | gofin_domain::BridgeTransferState::Failed
                )
            });
            let within_window = owner.4.as_deref().is_some_and(|timestamp| {
                parse_time(timestamp)
                    .map(|settled| settled > instant - chrono::Duration::hours(24))
                    .unwrap_or(true)
            });
            if unresolved || within_window {
                total += parse_uint(&owner.2)?;
            }
        }
        Ok(total.to_string())
    }

    pub(super) async fn reserve_ordinary_operation(
        &self,
        record: &mut OperationRecord,
        prepared: &PreparedOperation,
        replacement_parent: Option<&str>,
        recovery_parent: Option<&str>,
        cancellation_parent: Option<&str>,
    ) -> Result<()> {
        let instant = self.clock.now();
        let approved_expiry = chrono::DateTime::parse_from_rfc3339(&prepared.expires_at)
            .map_err(|_| GoFinError::validation("The prepared operation expiry is invalid."))?
            .with_timezone(&Utc);
        if approved_expiry <= instant {
            return Err(GoFinError::new(
                "PREVIEW_EXPIRED",
                "preflight",
                "The prepared operation expired while checking available cash.",
                "Prepare a fresh candidate within active plan or boundary authority; otherwise preview and approve the updated operation.",
            ));
        }
        let movement_limit =
            gofin_domain::policy::limit(&self.options.max_movement_and_cost_usd_micros)?;
        let allocation_limit =
            gofin_domain::policy::limit(&self.options.max_run_allocation_usd_micros)?;
        if unpriced(prepared) && !self.options.allow_unpriced_exchange_assets {
            return Err(GoFinError::new(
                "UNPRICED_EXCHANGE_ASSET_DISABLED",
                "budget",
                "This operation includes an unpriced exchange asset. The protected policy exception is disabled.",
                "The human may explicitly enable --allow-unpriced-exchange-assets true through gofin policy set in their own Terminal after reviewing the USD coverage warning. Then obtain a fresh preview and per-write Touch ID approval.",
            ));
        }
        if let Some(valuation) = &prepared.valuation {
            let assets = valuation
                .prices
                .iter()
                .map(|price| price.asset.clone())
                .collect();
            validate_price_rows(&valuation.prices, &assets, instant)?;
        }
        // Zero removes personal ceilings, not the exact valuation/accounting contract.
        validate_cost_bound_contract(&prepared.intent)?;
        validate_cost_prices(prepared, &prepared_cost_assets(&prepared.intent), instant)?;
        let own_charge = charge(prepared)?;
        let valuation = prepared.valuation.as_ref().unwrap();
        validate_price_observation(
            &valuation.observed_at,
            &valuation.source,
            instant,
            "VALUATION_UNAVAILABLE",
        )?;
        if valuation.source.trim().is_empty() {
            return Err(budget_error(
                "VALUATION_UNAVAILABLE",
                "An attributable USD valuation is required.",
            ));
        }
        if movement_limit.is_some_and(|limit| {
            valuation.movement_usd_micros.integer() + valuation.maximum_fee_usd_micros.integer()
                > BigUint::from(limit)
        }) {
            return Err(budget_error(
                "MOVEMENT_LIMIT",
                "Movement and maximum execution costs exceed the personal per-movement limit.",
            ));
        }
        let requested_resources = resources(&prepared.intent);
        let amendment = amendment_scope(prepared)?;
        let protection_parent = protection_parent_scope(prepared)?;
        // Read the complete journal before any network observation. The later
        // atomic Store call compares this exact snapshot, so another writer
        // cannot use a stale balance capture to acquire overlapping cash.
        let (snapshot, existing_reservations) = {
            let store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            (
                store.list_records().map_err(map_store)?,
                store.all_reservations().map_err(map_store)?,
            )
        };
        let linked_original = amendments::target(&prepared.intent).map(|(id, _, _)| id);
        let compound_children = compound_mutations::children(&prepared.intent)?;
        let grouped_owners = grouped_amendments::groups(&prepared.intent)?
            .iter()
            .map(|g| g.operation_id.as_str())
            .collect::<Vec<_>>();

        let mut rows: Vec<_> =
            cash_reservation_amounts(&prepared.intent.debits, &prepared.intent.maximum_costs)?
                .iter()
                .map(|amount| {
                    Ok(ReservationRow {
                        resource: resource_asset_key(amount)?,
                        units: amount.amount.atoms.to_string(),
                        fee_units: "0".into(),
                    })
                })
                .collect::<Result<_>>()?;
        // Aggregate debit and fee entries for the same asset before storing.
        let mut amounts = BTreeMap::<String, BigUint>::new();
        for row in rows.drain(..) {
            *amounts.entry(row.resource).or_default() += parse_uint(&row.units)?;
        }
        rows.extend(amounts.into_iter().map(|(resource, units)| ReservationRow {
            resource,
            units: units.to_string(),
            fee_units: "0".into(),
        }));
        rows.extend(requested_resources.iter().map(|resource| ReservationRow {
            resource: format!("lock:{}", resource.0),
            units: "0".into(),
            fee_units: "0".into(),
        }));
        let cash_capacities = self
            .cash_capacities_for_overlaps(
                &record.id,
                &rows,
                &existing_reservations,
                replacement_parent,
            )
            .await?;
        if approved_expiry <= self.clock.now() {
            return Err(GoFinError::new(
                "PREVIEW_EXPIRED",
                "preflight",
                "The prepared operation expired while checking available cash.",
                "Prepare a fresh candidate within active plan or boundary authority; otherwise preview and approve the updated operation.",
            ));
        }
        record.preview = Some(prepared.clone());
        record.updated_at = now();
        let candidate_value = to_value(record)?;
        let mut store = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?;
        if store.list_records().map_err(map_store)? != snapshot {
            return Err(budget_error(
                "RESOURCE_RESERVED",
                "Operation state changed while checking currently available cash. Refresh the preview and retry.",
            ));
        }
        let mut total = own_charge;
        let protection_parent_id = protection_parent.as_ref().map(|(id, _)| id.as_str());
        let records = operation_index(
            snapshot
                .iter()
                .cloned()
                .map(from_value::<OperationRecord>)
                .collect::<Result<Vec<_>>>()?,
        );
        for other in records.values() {
            if other.id == record.id {
                continue;
            }
            if !other.status.terminal() {
                if other.preview.is_none() {
                    return Err(budget_error(
                        "RESOURCE_RESERVED",
                        "An unresolved operation has no inspectable resource reservation.",
                    ));
                }
                let conflicts: BTreeSet<_> = requested_resources
                    .intersection(&conflict_resources(other)?)
                    .cloned()
                    .collect();
                let cancellation_lock_only = Some(other.id.as_str()) == cancellation_parent
                    && conflicts
                        .iter()
                        .all(|resource| prepared.intent.locks.contains(resource));
                let amendment_lock_only =
                    amendment.as_ref().is_some_and(|(parent_id, shared_locks)| {
                        other.id == *parent_id
                            && !conflicts.is_empty()
                            && conflicts
                                .iter()
                                .all(|resource| shared_locks.contains(resource))
                    });
                let protection_lock_only =
                    protection_parent
                        .as_ref()
                        .is_some_and(|(parent_id, shared_locks)| {
                            other.id == *parent_id
                                && !conflicts.is_empty()
                                && conflicts
                                    .iter()
                                    .all(|resource| shared_locks.contains(resource))
                        });
                let linked_protection_sibling_lock_only = if let Some(parent_id) = amendment
                    .as_ref()
                    .map(|(parent_id, _)| parent_id.as_str())
                    .or(cancellation_parent)
                {
                    let mut allowed = !conflicts.is_empty();
                    for resource in &conflicts {
                        if !store
                            .linked_protection_sibling_overlap_allowed(
                                &candidate_value,
                                parent_id,
                                &other.id,
                                &format!("lock:{}", resource.0),
                            )
                            .map_err(map_store)?
                        {
                            allowed = false;
                            break;
                        }
                    }
                    allowed
                } else {
                    false
                };
                if Some(other.id.as_str()) != replacement_parent
                    && linked_original != Some(other.id.as_str())
                    && !grouped_owners.contains(&other.id.as_str())
                    && !compound_children.is_some_and(|children| {
                        children.iter().any(|child| child.operation_id == other.id)
                    })
                    && !conflicts.is_empty()
                    && !cancellation_lock_only
                    && !amendment_lock_only
                    && !protection_lock_only
                    && !linked_protection_sibling_lock_only
                {
                    return Err(budget_error(
                        "RESOURCE_RESERVED",
                        "An unresolved operation reserves a resource required by this write.",
                    ));
                }
            }
            if counted_with_amendment_owner(other, &records, instant)? {
                total += unresolved_consumption::record_charge(&store, other)?;
            }
        }
        if allocation_limit.is_some_and(|limit| total > BigUint::from(limit)) {
            return Err(budget_error(
                "ALLOCATION_LIMIT",
                "This write exceeds the installation-wide rolling 24-hour allocation, including unresolved submissions.",
            ));
        }
        let persist = if !grouped_owners.is_empty() || compound_children.is_some() {
            store.persist_mutation_batch_reservation(
                &record.id,
                &rows,
                &to_value(record)?,
                &snapshot,
                &record.updated_at,
            )
        } else if let Some(original_id) = linked_original {
            store.persist_amendment_reservation(
                &record.id,
                original_id,
                &rows,
                &to_value(record)?,
                &snapshot,
                &record.updated_at,
            )
        } else if let Some(parent_id) = protection_parent_id {
            store.persist_operation_reservation_with_protection_parent(
                &record.id,
                &rows,
                &to_value(record)?,
                &snapshot,
                &record.updated_at,
                parent_id,
                &cash_capacities,
            )
        } else if amendment.is_some() {
            store.persist_operation_reservation_with_links_and_cash_capacities(
                &record.id,
                &rows,
                &to_value(record)?,
                &snapshot,
                &record.updated_at,
                replacement_parent,
                recovery_parent,
                cancellation_parent,
                amendment.as_ref().map(|(parent_id, _)| parent_id.as_str()),
                &cash_capacities,
            )
        } else if !cash_capacities.is_empty() {
            store.persist_operation_reservation_with_links_and_cash_capacities(
                &record.id,
                &rows,
                &to_value(record)?,
                &snapshot,
                &record.updated_at,
                replacement_parent,
                recovery_parent,
                cancellation_parent,
                None,
                &cash_capacities,
            )
        } else if let Some(parent_id) = recovery_parent {
            store.persist_operation_reservation_with_recovery_parent(
                &record.id,
                &rows,
                &to_value(record)?,
                &snapshot,
                &record.updated_at,
                parent_id,
            )
        } else if let Some(parent_id) = cancellation_parent {
            store.persist_operation_reservation_with_cancellation_parent(
                &record.id,
                &rows,
                &to_value(record)?,
                &snapshot,
                &record.updated_at,
                parent_id,
            )
        } else {
            store.persist_operation_reservation_with_parent(
                &record.id,
                &rows,
                &to_value(record)?,
                &snapshot,
                &record.updated_at,
                replacement_parent,
            )
        };
        persist.map_err(map_store)
    }

    async fn cash_capacities_for_overlaps(
        &self,
        operation_id: &str,
        requested: &[ReservationRow],
        existing: &[(String, ReservationRow)],
        replacement_parent: Option<&str>,
    ) -> Result<Vec<CashReservationCapacity>> {
        let existing_keys = existing
            .iter()
            .filter(|(owner, row)| {
                owner != operation_id
                    && Some(owner.as_str()) != replacement_parent
                    && !row.resource.starts_with("lock:")
                    && row.units == "0"
                    && row.fee_units != "0"
            })
            .map(|(_, row)| row.resource.as_str())
            .collect::<BTreeSet<_>>();
        let keys = requested
            .iter()
            .filter(|row| {
                !row.resource.starts_with("lock:") && existing_keys.contains(row.resource.as_str())
            })
            .map(|row| row.resource.clone())
            .collect::<BTreeSet<_>>();
        if keys.is_empty() {
            return Ok(Vec::new());
        }

        let observers = {
            let adapters = self
                .adapters
                .read()
                .map_err(|_| internal("adapter registry lock poisoned"))?;
            let mut selected = Vec::<Arc<dyn Adapter>>::new();
            for adapter in adapters.values() {
                if selected
                    .iter()
                    .any(|existing| Arc::ptr_eq(existing, adapter))
                {
                    continue;
                }
                if adapter.snapshot_identities().iter().any(|identity| {
                    keys.iter().any(|key| {
                        decode_resource_asset_key(key)
                            .is_ok_and(|(resource, _)| resource == identity.resource)
                    })
                }) {
                    selected.push(Arc::clone(adapter));
                }
            }
            selected
        };
        let mut capacities = Vec::with_capacity(keys.len());
        for key in keys {
            let (resource, asset) = decode_resource_asset_key(&key)?;
            let matches = observers
                .iter()
                .filter(|adapter| {
                    adapter
                        .snapshot_identities()
                        .iter()
                        .any(|identity| identity.resource == resource)
                })
                .collect::<Vec<_>>();
            let wallet_matches = matches
                .iter()
                .filter(|adapter| {
                    adapter.definitions().iter().any(|definition| {
                        definition.name == "wallet.balances" || definition.name == "wallet.token"
                    })
                })
                .collect::<Vec<_>>();
            let selected = if wallet_matches.len() == 1 {
                wallet_matches[0]
            } else if wallet_matches.is_empty() && matches.len() == 1 {
                matches[0]
            } else {
                return Err(budget_error(
                    "CASH_SNAPSHOT_UNAVAILABLE",
                    "A single exact wallet-balance observer is required to check cash already reserved by another operation.",
                ));
            };
            let scope = SnapshotScope {
                resources: vec![resource.clone()],
                networks: Vec::new(),
                assets: vec![asset.clone()],
            };
            let snapshot = tokio::time::timeout(
                PREFLIGHT_TIMEOUT,
                selected.snapshot_scoped(&scope),
            )
            .await
            .map_err(|_| {
                budget_error(
                    "CASH_SNAPSHOT_UNAVAILABLE",
                    "The exact wallet cash snapshot did not finish before the preflight deadline.",
                )
            })?
            .map_err(|_| {
                budget_error(
                    "CASH_SNAPSHOT_UNAVAILABLE",
                    "A fresh scoped cash snapshot is required before overlapping reservations can be approved.",
                )
            })?;
            let observed = chrono::DateTime::parse_from_rfc3339(&snapshot.observed_at)
                .map_err(|_| {
                    budget_error(
                        "CASH_SNAPSHOT_UNAVAILABLE",
                        "The wallet cash snapshot has an invalid observation time.",
                    )
                })?
                .with_timezone(&Utc);
            let now = self.clock.now();
            if !snapshot.complete
                || !snapshot.pending.is_empty()
                || observed > now + chrono::Duration::seconds(10)
                || now.signed_duration_since(observed).num_seconds() > 120
                || !snapshot.coverage.iter().any(|entry| entry == &resource.0)
                || !snapshot.state.in_flight.is_empty()
            {
                return Err(budget_error(
                    "CASH_SNAPSHOT_UNAVAILABLE",
                    "A complete, current wallet cash snapshot is required to check overlapping reservations.",
                ));
            }
            let balance = snapshot
                .state
                .holdings
                .iter()
                .filter(|holding| holding.resource == resource && holding.amount.asset == asset)
                .fold(BigUint::zero(), |sum, holding| {
                    sum + holding.amount.atoms.integer()
                });
            capacities.push(CashReservationCapacity {
                resource: key,
                maximum_total_units: balance.to_string(),
            });
        }
        Ok(capacities)
    }
}

pub(super) fn validate_boundary_candidate_policy(
    prepared: &PreparedOperation,
    operation_id: &str,
    options: &EngineOptions,
    store: &Store,
    instant: DateTime<Utc>,
) -> Result<()> {
    if unpriced(prepared) && !options.allow_unpriced_exchange_assets {
        return Err(budget_error(
            "UNPRICED_EXCHANGE_ASSET_DISABLED",
            "This operation includes an unpriced exchange asset and the protected exception is disabled.",
        ));
    }
    let valuation = prepared.valuation.as_ref().ok_or_else(|| {
        budget_error(
            "VALUATION_UNAVAILABLE",
            "Fresh USD valuation is required for every boundary attempt.",
        )
    })?;
    validate_price_observation(
        &valuation.observed_at,
        &valuation.source,
        instant,
        "VALUATION_UNAVAILABLE",
    )?;
    validate_cost_bound_contract(&prepared.intent)?;
    validate_cost_prices(prepared, &prepared_cost_assets(&prepared.intent), instant)?;
    let movement_and_fee =
        valuation.movement_usd_micros.integer() + valuation.maximum_fee_usd_micros.integer();
    if gofin_domain::policy::limit(&options.max_movement_and_cost_usd_micros)?
        .is_some_and(|limit| movement_and_fee > BigUint::from(limit))
    {
        return Err(budget_error(
            "MOVEMENT_LIMIT",
            "Fresh movement and cost valuation exceeds the current protected per-write limit.",
        ));
    }
    let allocation_limit = gofin_domain::policy::limit(&options.max_run_allocation_usd_micros)?;
    if allocation_limit.is_some() {
        let requested_resources = resources(&prepared.intent);
        let amendment = amendment_scope(prepared)?;
        let protection_parent = protection_parent_scope(prepared)?;
        let cancellation_parent = cancellation_target(&prepared.intent).map(|(id, _)| id);
        let mut total = charge(prepared)?;
        let records = operation_index(
            store
                .list_records()
                .map_err(map_store)?
                .into_iter()
                .map(from_value::<OperationRecord>)
                .collect::<Result<Vec<_>>>()?,
        );
        for other in records.values() {
            if other.id == operation_id {
                continue;
            }
            if !other.status.terminal() {
                if other.preview.is_none() {
                    return Err(budget_error(
                        "RESOURCE_RESERVED",
                        "An unresolved operation has no inspectable resource reservation.",
                    ));
                }
                let previous_resources = conflict_resources(other)?;
                let conflicts = requested_resources
                    .intersection(&previous_resources)
                    .cloned()
                    .collect::<BTreeSet<_>>();
                let cancellation_lock_only = Some(other.id.as_str()) == cancellation_parent
                    && conflicts
                        .iter()
                        .all(|resource| prepared.intent.locks.contains(resource));
                let amendment_lock_only =
                    amendment.as_ref().is_some_and(|(parent_id, shared_locks)| {
                        other.id == *parent_id
                            && !conflicts.is_empty()
                            && conflicts
                                .iter()
                                .all(|resource| shared_locks.contains(resource))
                    });
                let protection_lock_only =
                    protection_parent
                        .as_ref()
                        .is_some_and(|(parent_id, shared_locks)| {
                            other.id == *parent_id
                                && !conflicts.is_empty()
                                && conflicts
                                    .iter()
                                    .all(|resource| shared_locks.contains(resource))
                        });
                if !conflicts.is_empty()
                    && !cancellation_lock_only
                    && !amendment_lock_only
                    && !protection_lock_only
                {
                    return Err(budget_error(
                        "RESOURCE_RESERVED",
                        "An unresolved operation reserves a resource required by this write.",
                    ));
                }
            }
            if counted_with_amendment_owner(other, &records, instant)? {
                total += unresolved_consumption::record_charge(store, other)?;
            }
        }
        if allocation_limit.is_some_and(|limit| total > BigUint::from(limit)) {
            return Err(budget_error(
                "ALLOCATION_LIMIT",
                "Fresh valuation exceeds the current rolling 24-hour allocation.",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gofin_domain::{AccountingLocation, Amount, AssetId, Quantity, ResourceId, SignedQuantity};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn asset() -> AssetId {
        AssetId {
            network: "synthetic".into(),
            reference: "USD".into(),
            decimals: 0,
        }
    }

    fn amount(value: &str) -> Amount {
        Amount {
            asset: asset(),
            atoms: Quantity::new(value).unwrap(),
        }
    }

    #[test]
    fn included_conversion_fee_does_not_double_charge_gross_wallet_debit() {
        let wallet = ResourceId("solana:mainnet-beta:wallet:owner".into());
        let location = AccountingLocation::Position {
            resource: wallet.clone(),
            position: "perps:request:collateral".into(),
            position_kind: gofin_domain::PositionKind::LockedCollateral,
        };
        let input_asset = asset_id("solana:mainnet-beta", "USDC", 6);
        let margin_asset = asset_id("solana:mainnet-beta", "jupiter:margin-usd", 6);
        let price = |asset: AssetId| gofin_domain::AssetPrice {
            asset,
            usd_micros_per_unit: quantity("1000000"),
            unit_atoms: quantity("1000000"),
            observed_at: "2026-01-01T00:00:00Z".into(),
            source: "synthetic exact $1 unit".into(),
        };
        let effects = vec![
            gofin_domain::FinancialEffect::CashDebit {
                resource: wallet.clone(),
                amount: Amount {
                    asset: input_asset.clone(),
                    atoms: quantity("11000000"),
                },
            },
            gofin_domain::FinancialEffect::Cost {
                location: location.clone(),
                amount: Amount {
                    asset: margin_asset.clone(),
                    atoms: quantity("1000"),
                },
                category: "protocol_fee".into(),
                treatment: CostTreatment::IncludedInDebit,
            },
            gofin_domain::FinancialEffect::IncludedCostConversion {
                location,
                source: gofin_domain::IncludedCostSource::Cash { resource: wallet },
                gross_debit: Amount {
                    asset: input_asset.clone(),
                    atoms: quantity("11000000"),
                },
                gross_converted: Amount {
                    asset: margin_asset.clone(),
                    atoms: quantity("11000000"),
                },
                net_position: Amount {
                    asset: margin_asset.clone(),
                    atoms: quantity("10999000"),
                },
                included_cost: Amount {
                    asset: margin_asset.clone(),
                    atoms: quantity("1000"),
                },
                evidence_id: "synthetic-finalized-event-1".into(),
            },
        ];
        assert_eq!(
            actual_effect_charge(&effects, &[price(input_asset), price(margin_asset)]).unwrap(),
            BigUint::from(11_000_000u32),
            "gross source debit already includes the IncludedInDebit fee"
        );

        let wallet = ResourceId("solana:mainnet-beta:wallet:owner".into());
        let escrow = ResourceId("solana:mainnet-beta:request-ata".into());
        let input_asset = asset_id("solana:mainnet-beta", "USDC", 6);
        let margin_asset = asset_id("solana:mainnet-beta", "jupiter:margin-usd", 6);
        let escrow_location = AccountingLocation::Position {
            resource: wallet.clone(),
            position: "perps:request:collateral".into(),
            position_kind: gofin_domain::PositionKind::LockedCollateral,
        };
        let escrow_effects = vec![
            gofin_domain::FinancialEffect::EscrowPositionProgress {
                wallet_resource: wallet.clone(),
                escrow_resource: escrow.clone(),
                input_asset: input_asset.clone(),
                wallet_debit: quantity("2000000"),
                escrow_deposit: quantity("2000000"),
                input_transfer_fee: Quantity::zero(),
                input_filled: quantity("1000000"),
                input_refunded: Quantity::zero(),
                remaining_input: quantity("1000000"),
                position_resource: wallet.clone(),
                position: "perps:request:collateral".into(),
                position_kind: gofin_domain::PositionKind::LockedCollateral,
                position_asset: margin_asset.clone(),
                position_change: quantity("999000"),
                state: gofin_domain::EscrowLifecycleState::Open,
                position_anchor: None,
                conditional_position_change: Quantity::zero(),
                conditional_position_anchor: None,
            },
            gofin_domain::FinancialEffect::Cost {
                location: escrow_location.clone(),
                amount: Amount {
                    asset: margin_asset.clone(),
                    atoms: quantity("1000"),
                },
                category: "protocol_fee".into(),
                treatment: CostTreatment::IncludedInDebit,
            },
            gofin_domain::FinancialEffect::IncludedCostConversion {
                location: escrow_location,
                source: gofin_domain::IncludedCostSource::Escrow {
                    wallet_resource: wallet,
                    escrow_resource: escrow,
                },
                gross_debit: Amount {
                    asset: input_asset.clone(),
                    atoms: quantity("1000000"),
                },
                gross_converted: Amount {
                    asset: margin_asset.clone(),
                    atoms: quantity("1000000"),
                },
                net_position: Amount {
                    asset: margin_asset.clone(),
                    atoms: quantity("999000"),
                },
                included_cost: Amount {
                    asset: margin_asset.clone(),
                    atoms: quantity("1000"),
                },
                evidence_id: "synthetic-finalized-fill-1".into(),
            },
        ];
        assert_eq!(
            actual_effect_charge(&escrow_effects, &[price(input_asset), price(margin_asset)])
                .unwrap(),
            BigUint::from(2_000_000u32),
            "gross escrow deposit is charged once; included conversion cost is not added"
        );
    }

    fn quantity(value: &str) -> Quantity {
        Quantity::new(value).unwrap()
    }

    fn asset_id(network: &str, reference: &str, decimals: u8) -> AssetId {
        AssetId {
            network: network.into(),
            reference: reference.into(),
            decimals,
        }
    }

    struct ScopedCashAdapter {
        names: Vec<&'static str>,
        identity: gofin_domain::ConnectionMetadata,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Adapter for ScopedCashAdapter {
        fn definitions(&self) -> Vec<Definition> {
            self.names
                .iter()
                .map(|name| Definition {
                    name: (*name).into(),
                    description: "Synthetic observer test.".into(),
                    kind: gofin_domain::OperationKind::Read,
                    execution_actor: gofin_domain::ExecutionActor::AgentReadable,
                    input_schema: json!({"type":"object"}),
                    output_schema: json!({"type":"object"}),
                    error_schema: json!({"type":"object"}),
                    examples: vec![],
                    coverage: "synthetic".into(),
                    completion: "synthetic".into(),
                    semantics: json!({}),
                })
                .collect()
        }

        fn snapshot_identities(&self) -> Vec<gofin_domain::ConnectionMetadata> {
            vec![self.identity.clone()]
        }

        async fn snapshot_scoped(
            &self,
            scope: &gofin_domain::SnapshotScope,
        ) -> Result<gofin_domain::StateSnapshot> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let resource = self.identity.resource.clone();
            let balance_asset = scope.assets.first().cloned().unwrap_or_else(asset);
            Ok(gofin_domain::StateSnapshot {
                state: gofin_domain::FinancialState {
                    holdings: vec![ResourceAmount {
                        resource: resource.clone(),
                        amount: Amount {
                            asset: balance_asset,
                            atoms: quantity("100"),
                        },
                    }],
                    ..Default::default()
                },
                observed_at: now(),
                complete: true,
                coverage: vec![resource.0],
                pending: vec![],
            })
        }

        async fn read(&self, _operation: &str, _input: &Value) -> Result<Value> {
            Ok(json!({}))
        }

        async fn prepare(&self, _operation: &str, _input: &Value) -> Result<PreparedOperation> {
            Err(GoFinError::validation("Not used by this test."))
        }

        async fn revalidate(&self, _prepared: &PreparedOperation) -> Result<PreparedOperation> {
            Err(GoFinError::validation("Not used by this test."))
        }

        async fn submit(
            &self,
            _prepared: &PreparedOperation,
            _journal: Arc<dyn SubmissionJournal>,
        ) -> Result<gofin_domain::Submission> {
            Err(GoFinError::validation("Not used by this test."))
        }

        async fn reconcile(&self, _record: &OperationRecord) -> Result<Reconciliation> {
            Err(GoFinError::validation("Not used by this test."))
        }
    }

    struct RejectApproval;

    #[async_trait]
    impl gofin_domain::Authorizer for RejectApproval {
        async fn authorize(
            &self,
            _challenge: &gofin_domain::AuthorizationChallenge,
        ) -> Result<gofin_domain::ApprovalAudit> {
            Err(GoFinError::validation("Not used by this test."))
        }
    }

    #[tokio::test]
    async fn cash_overlap_uses_one_wallet_observer_in_multi_adapter_registry() {
        let wallet_resource = ResourceId("solana:wallet:owner".into());
        let identity = gofin_domain::ConnectionMetadata {
            name: "wallet".into(),
            provider: "synthetic".into(),
            resource: wallet_resource.clone(),
            role: Some("wallet".into()),
            parent: None,
            network: Some("solana".into()),
            address: Some("owner".into()),
            credential_ref: None,
            signing_enabled: false,
            details: json!({}),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let wallet = Arc::new(ScopedCashAdapter {
            names: vec!["wallet.balances", "wallet.token"],
            identity: identity.clone(),
            calls: calls.clone(),
        });
        let perps = Arc::new(ScopedCashAdapter {
            names: vec!["jupiter.perps.position"],
            identity,
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let engine = crate::Engine::new(
            gofin_store::Store::in_memory().unwrap(),
            vec![wallet, perps],
            Arc::new(RejectApproval),
            crate::EngineOptions::default(),
        )
        .unwrap();
        let cash_key = resource_asset_key(&ResourceAmount {
            resource: wallet_resource,
            amount: Amount {
                asset: asset_id("solana", "USDC", 6),
                atoms: quantity("1"),
            },
        })
        .unwrap();
        let capacities = engine
            .cash_capacities_for_overlaps(
                "new-write",
                &[ReservationRow {
                    resource: cash_key.clone(),
                    units: "20".into(),
                    fee_units: "0".into(),
                }],
                &[(
                    "pending-fee".into(),
                    ReservationRow {
                        resource: cash_key.clone(),
                        units: "0".into(),
                        fee_units: "5".into(),
                    },
                )],
                None,
            )
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(capacities.len(), 1);
        assert_eq!(capacities[0].resource, cash_key);
        assert_eq!(capacities[0].maximum_total_units, "100");
    }

    fn prices() -> Vec<gofin_domain::AssetPrice> {
        vec![gofin_domain::AssetPrice {
            asset: asset(),
            usd_micros_per_unit: Quantity::new("1").unwrap(),
            unit_atoms: Quantity::new("1").unwrap(),
            observed_at: "2026-01-01T00:00:00Z".into(),
            source: "synthetic test price".into(),
        }]
    }

    #[test]
    fn protected_allocation_counts_cash_once_and_keeps_borrowed_principal() {
        let cash = ResourceId("synthetic:wallet".into());
        let market = ResourceId("synthetic:market".into());
        let transfer_provenance = gofin_domain::FinancialEffect::OriginMovement {
            from: AccountingLocation::Cash {
                resource: cash.clone(),
            },
            to: AccountingLocation::Position {
                resource: market.clone(),
                position: "supply".into(),
                position_kind: gofin_domain::PositionKind::Supply,
            },
            debit: amount("100"),
            credit: amount("100"),
        };
        let cash_effects = vec![
            gofin_domain::FinancialEffect::CashDebit {
                resource: cash.clone(),
                amount: amount("100"),
            },
            transfer_provenance,
        ];
        assert_eq!(
            actual_effect_charge(&cash_effects, &prices()).unwrap(),
            BigUint::from(100u32)
        );

        let borrowed = vec![
            gofin_domain::FinancialEffect::DebtChange {
                resource: market,
                debt: "variableDebtUSD".into(),
                asset: asset(),
                delta: SignedQuantity::new("75").unwrap(),
                anchor: None,
            },
            gofin_domain::FinancialEffect::UsableCredit {
                to: cash,
                amount: amount("75"),
            },
        ];
        assert_eq!(
            actual_effect_charge(&borrowed, &prices()).unwrap(),
            BigUint::from(75u32)
        );
    }

    #[test]
    fn included_repayment_fee_is_not_charged_twice() {
        let resource = ResourceId("synthetic:wallet".into());
        let effects = vec![
            gofin_domain::FinancialEffect::CashDebit {
                resource: resource.clone(),
                amount: amount("100"),
            },
            gofin_domain::FinancialEffect::Cost {
                location: AccountingLocation::Cash { resource },
                amount: amount("5"),
                category: "included repayment fee".into(),
                treatment: CostTreatment::IncludedInDebit,
            },
        ];
        assert_eq!(
            actual_effect_charge(&effects, &prices()).unwrap(),
            BigUint::from(100u32)
        );
    }

    #[test]
    fn terminal_escrow_and_account_funding_charge_gross_once_despite_refunds() {
        let wallet = ResourceId("synthetic:wallet".into());
        let escrow = ResourceId("synthetic:order".into());
        let order_asset = asset();
        let fee = gofin_domain::FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: wallet.clone(),
            },
            amount: amount("5"),
            category: "synthetic network fee".into(),
            treatment: CostTreatment::AdditionalDebit,
        };
        let effects = vec![
            gofin_domain::FinancialEffect::EscrowProgress {
                wallet_resource: wallet.clone(),
                escrow_resource: escrow,
                input_asset: order_asset.clone(),
                output_asset: asset(),
                wallet_debit: quantity("100"),
                escrow_deposit: quantity("100"),
                input_transfer_fee: quantity("0"),
                input_filled: quantity("100"),
                output_filled: quantity("100"),
                input_refunded: quantity("0"),
                remaining_input: quantity("0"),
                state: gofin_domain::EscrowLifecycleState::Filled,
            },
            gofin_domain::FinancialEffect::AccountFundingProgress {
                resource: wallet,
                position: "account-funding:order".into(),
                account: "order".into(),
                asset: order_asset,
                funded: quantity("20"),
                returned: quantity("20"),
                remaining: quantity("0"),
                state: gofin_domain::AccountFundingState::Closed,
                anchor: None,
            },
            fee,
            // Provenance rows describe the same debit and must not be charged
            // as another gross movement.
            gofin_domain::FinancialEffect::OriginMovement {
                from: AccountingLocation::Cash {
                    resource: ResourceId("synthetic:wallet".into()),
                },
                to: AccountingLocation::Position {
                    resource: ResourceId("synthetic:order".into()),
                    position: "account-funding:order".into(),
                    position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                },
                debit: amount("20"),
                credit: amount("20"),
            },
        ];
        assert_eq!(
            actual_effect_charge(&effects, &prices()).unwrap(),
            BigUint::from(125u32)
        );

        let mut record = super::super::tests::record(vec![], vec![]);
        let prepared = record.preview.as_mut().unwrap();
        prepared.valuation = Some(gofin_domain::Valuation {
            movement_usd_micros: quantity("100"),
            maximum_fee_usd_micros: quantity("5"),
            additional_allocation_usd_micros: quantity("20"),
            observed_at: now(),
            source: "synthetic test price".into(),
            prices: prices(),
        });
        prepared.binding[gofin_domain::boundaries::BOUNDARY_METADATA_KEY] = json!({
            "authorization_id":"synthetic-boundary"
        });
        record.status = Status::Completed;
        let mut verified = gofin_domain::VerifiedEffects {
            evidence_version: 1,
            submission: record.submission.clone().unwrap(),
            effects: effects.clone(),
            evidence: json!({}),
            observed_at: now(),
            proof_digest: String::new(),
        };
        verified.proof_digest = gofin_domain::verified_effect_digest(&verified).unwrap();
        record.effects = Some(verified);
        assert_eq!(record_charge(&record).unwrap(), BigUint::from(125u32));
    }

    #[test]
    fn open_escrow_releases_wallet_conflict_but_keeps_exact_order_lock() {
        let wallet = ResourceId("solana:mainnet-beta:wallet".into());
        let order = ResourceId("solana:mainnet-beta:trigger-order:order".into());
        let input = asset_id("synthetic", "USDC", 6);
        let output = asset_id("synthetic", "WSOL", 9);
        let mut parent = super::super::tests::record(vec![], vec![]);
        parent.id = "open-order".into();
        parent.status = Status::Pending;
        let prepared = parent.preview.as_mut().unwrap();
        prepared.intent.locks = vec![wallet.clone(), order.clone()];
        prepared.intent.debits = vec![ResourceAmount {
            resource: wallet.clone(),
            amount: Amount {
                asset: input.clone(),
                atoms: quantity("100"),
            },
        }];
        prepared.intent.maximum_costs = vec![CostBound {
            location: AccountingLocation::Cash {
                resource: wallet.clone(),
            },
            amount: Amount {
                asset: asset_id("synthetic", "SOL", 9),
                atoms: quantity("10000"),
            },
            treatment: CostTreatment::AdditionalDebit,
        }];
        prepared.intent.expected = vec![EffectConstraint::EscrowLifecycle {
            wallet_resource: wallet.clone(),
            escrow_resource: order.clone(),
            input_asset: input.clone(),
            output_asset: output.clone(),
            maximum_wallet_debit: quantity("100"),
            minimum_output_per_input_numerator: quantity("1"),
            minimum_output_per_input_denominator: quantity("1"),
        }];
        let mut verified = gofin_domain::VerifiedEffects {
            evidence_version: 1,
            submission: parent.submission.clone().unwrap(),
            effects: vec![gofin_domain::FinancialEffect::EscrowProgress {
                wallet_resource: wallet.clone(),
                escrow_resource: order.clone(),
                input_asset: input,
                output_asset: output,
                wallet_debit: quantity("100"),
                escrow_deposit: quantity("100"),
                input_transfer_fee: quantity("0"),
                input_filled: quantity("0"),
                output_filled: quantity("0"),
                input_refunded: quantity("0"),
                remaining_input: quantity("100"),
                state: gofin_domain::EscrowLifecycleState::Open,
            }],
            evidence: json!({}),
            observed_at: now(),
            proof_digest: String::new(),
        };
        verified.proof_digest = gofin_domain::verified_effect_digest(&verified).unwrap();
        parent.effects = Some(verified);

        let effective = conflict_resources(&parent).unwrap();
        assert_eq!(effective, BTreeSet::from([order]));
        let unrelated_wallet_action = FinancialIntent {
            debits: vec![ResourceAmount {
                resource: wallet.clone(),
                amount: Amount {
                    asset: asset_id("synthetic", "SOL", 9),
                    atoms: quantity("100000"),
                },
            }],
            maximum_costs: vec![],
            expected: vec![],
            locks: vec![wallet],
        };
        assert!(
            resources(&unrelated_wallet_action)
                .intersection(&effective)
                .next()
                .is_none()
        );
    }
}
