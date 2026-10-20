//! A fresh human decision can accept uncertainty. It cannot become venue proof.
use super::financial_plans::add_cost_bounds;
use super::*;
use gofin_domain::operation_recovery::{
    OperationResolutionInput, OperationResolutionOutcome, RecoveryReference,
};

fn unavailable(message: &str) -> GoFinError {
    GoFinError::new(
        "OPERATION_RESOLUTION_UNAVAILABLE",
        "recovery",
        message,
        "Inspect operation recovery. Preserve the original submission; do not replay it.",
    )
}

fn receipt_identities(intent: &FinancialIntent) -> BTreeSet<(ResourceId, gofin_domain::AssetId)> {
    intent
        .expected
        .iter()
        .filter_map(|constraint| match constraint {
            EffectConstraint::Credit { to, asset, .. } => Some((to.clone(), asset.clone())),
            EffectConstraint::Transfer {
                to, credit_asset, ..
            } => Some((to.clone(), credit_asset.clone())),
            EffectConstraint::Conversion {
                resource, output, ..
            } => Some((resource.clone(), output.clone())),
            _ => None,
        })
        .collect()
}

fn cash_only(record: &OperationRecord) -> bool {
    record.run_id.is_none()
        && record.preview.as_ref().is_some_and(|prepared| {
            prepared.intent.expected.iter().all(|constraint| {
                matches!(
                    constraint,
                    EffectConstraint::Debit { .. }
                        | EffectConstraint::Credit { .. }
                        | EffectConstraint::Transfer { .. }
                        | EffectConstraint::Conversion { .. }
                )
            }) && prepared
                .intent
                .maximum_costs
                .iter()
                .all(|cost| matches!(cost.location, AccountingLocation::Cash { .. }))
        })
}

fn validate_cash_attestation(
    record: &OperationRecord,
    input: &OperationResolutionInput,
) -> Result<()> {
    if input.reason.trim().is_empty()
        || input.reason.len() > 1024
        || input.reason.chars().any(char::is_control)
    {
        return Err(unavailable(
            "A nonempty plain-text reason of at most 1024 bytes is required.",
        ));
    }
    let intent = &record
        .preview
        .as_ref()
        .ok_or_else(|| unavailable("Original intent missing."))?
        .intent;
    if !cash_only(record) {
        if !input.actual_debits.is_empty()
            || !input.actual_costs.is_empty()
            || !input.reported_receipts.is_empty()
        {
            return Err(unavailable(
                "Non-cash operations support outcome-only acceptance: omit all cash claims. Position, debt, allowance and escrow holds remain until owned recovery verifies them.",
            ));
        }
        return Ok(());
    }
    let mut receipts = receipt_identities(intent);
    if input.outcome == OperationResolutionOutcome::Failed {
        receipts.extend(
            intent
                .debits
                .iter()
                .map(|row| (row.resource.clone(), row.amount.asset.clone())),
        );
    }
    for (rows, identities) in [
        (
            &input.actual_debits,
            intent
                .debits
                .iter()
                .map(|row| (row.resource.clone(), row.amount.asset.clone()))
                .collect(),
        ),
        (&input.reported_receipts, receipts),
    ] {
        let mut seen = BTreeSet::new();
        for row in rows {
            let key = (row.resource.clone(), row.amount.asset.clone());
            if !identities.contains(&key) || !seen.insert(key) {
                return Err(unavailable(
                    "Reported cash rows must be unique and use exact original resource and asset identities.",
                ));
            }
        }
    }
    let mut seen = BTreeSet::new();
    for cost in &input.actual_costs {
        let key = serde_json::to_string(&(&cost.location, &cost.amount.asset, &cost.treatment))
            .map_err(|_| internal("cost identity"))?;
        if !seen.insert(key)
            || !intent.maximum_costs.iter().any(|bound| {
                bound.location == cost.location
                    && bound.amount.asset == cost.amount.asset
                    && bound.treatment == cost.treatment
            })
        {
            return Err(unavailable(
                "Reported costs must be unique and preserve original fee asset, origin and treatment.",
            ));
        }
    }
    Ok(())
}

fn extra_allocation(
    prepared: &PreparedOperation,
    debits: &[ResourceAmount],
    costs: &[CostBound],
) -> Result<BigUint> {
    let intent = &prepared.intent;
    let mut additional = BigUint::zero();
    let prices = prepared
        .valuation
        .as_ref()
        .map(|value| value.prices.as_slice())
        .unwrap_or(&[]);
    for actual in debits {
        let cap = intent
            .debits
            .iter()
            .find(|cap| cap.resource == actual.resource && cap.amount.asset == actual.amount.asset)
            .map(|cap| cap.amount.atoms.integer())
            .unwrap_or_default();
        if actual.amount.atoms.integer() > cap {
            additional += ordinary_policy::amount_usd(
                &Amount {
                    asset: actual.amount.asset.clone(),
                    atoms: gofin_domain::Quantity::new(
                        (actual.amount.atoms.integer() - cap).to_string(),
                    )?,
                },
                prices,
            )?;
        }
    }
    for actual in costs {
        let cap = intent
            .maximum_costs
            .iter()
            .find(|cap| {
                cap.location == actual.location
                    && cap.amount.asset == actual.amount.asset
                    && cap.treatment == actual.treatment
            })
            .map(|cap| cap.amount.atoms.integer())
            .unwrap_or_default();
        if actual.amount.atoms.integer() > cap {
            additional += ordinary_policy::amount_usd(
                &Amount {
                    asset: actual.amount.asset.clone(),
                    atoms: gofin_domain::Quantity::new(
                        (actual.amount.atoms.integer() - cap).to_string(),
                    )?,
                },
                prices,
            )?;
        }
    }
    Ok(additional)
}

pub(super) fn human_charged_totals(
    resolution: &Value,
) -> Result<(Vec<ResourceAmount>, Vec<CostBound>)> {
    let mut debits: Vec<ResourceAmount> =
        from_value(resolution["review"]["impact"]["charged_debits"].clone())?;
    let mut costs: Vec<CostBound> =
        from_value(resolution["review"]["impact"]["charged_costs"].clone())?;
    for observation in resolution["external_confirmation"]["observations"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let Some(rows) = observation.get("charged_debits") {
            debits = max_amounts(&debits, &from_value::<Vec<ResourceAmount>>(rows.clone())?)?;
        }
        if let Some(rows) = observation.get("charged_costs") {
            costs = max_costs(&costs, &from_value::<Vec<CostBound>>(rows.clone())?)?;
        }
    }
    Ok((debits, costs))
}

impl Engine {
    pub fn operation_view(&self, record: &OperationRecord) -> Result<Value> {
        let mut value = to_value(record)?;
        if let Some(resolution) = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .operation_resolution(&record.id)
            .map_err(map_store)?
        {
            value["effective_outcome"] =
                json!({"provenance":"human_attested","outcome":resolution["outcome"]});
            value["manual_resolution"] = resolution;
        }
        Ok(value)
    }

    pub fn operation_recovery(&self, operation_id: &str) -> Result<Value> {
        let record = self.load_record(operation_id)?;
        let operation = self.operation_view(&record)?;
        let manual = operation.get("manual_resolution");
        let store = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?;
        let plan_context=store.financial_plan_attempt_owner(operation_id).map_err(map_store)?
            .map(|(id,revision,step,_)|->Result<Value>{
                let raw=store.financial_plan(&id).map_err(map_store)?.ok_or_else(||unavailable("Linked plan missing."))?;
                let plan:FinancialPlanAuthorizationRecord=from_value(raw)?;
                Ok(json!({"plan_id":id,"revision_id":plan.revision.revision_id,"original_revision_id":revision,
                    "step_id":step,"expires_at":plan.revision.expires_at,"status":plan.revision.status,
                    "active_attempt_id":plan.active_attempt_id,
                    "authority_active":plan.revision.status==gofin_domain::financial_plans::FinancialPlanAuthorizationStatus::Active
                        &&parse_time(&plan.revision.expires_at)?>self.clock.now(),
                    "completed_prefix_preserved":true}))
            }).transpose()?;
        let remaining_reservations = store.all_effective_reservations().map_err(map_store)?
            .into_iter().filter(|(id, _)| id == operation_id)
            .map(|(_, row)| json!({"resource":row.resource,"units":row.units,"fee_units":row.fee_units}))
            .collect::<Vec<_>>();
        drop(store);
        let mut notes = vec![];
        let mut actions = vec![];
        let mut blockers = vec![];
        let unresolved = matches!(
            record.status,
            Status::Unknown | Status::Pending | Status::Submitted
        );
        if record.submission.is_some() && unresolved {
            actions.push(json!({"operation":"status","operation_id":operation_id,"read_only":true,"requires":"Reconcile this original immutable submission; never resubmit it."}));
            actions.push(json!({"operation":"operations.recover","operation_id":operation_id,"read_only":true,"requires":"A supported exact receipt reference; the adapter verifies its original-identity correlation."}));
        }
        if let Some(manual) = manual {
            if manual["external_confirmation"]["status"] == "contradicts" {
                blockers.push(json!({"code":"HUMAN_RESOLUTION_CONTRADICTED","message":"Later evidence contradicts the human report. Remaining attested funding is quarantined; consumed history and charges remain."}));
            } else {
                notes.push(json!({"code":"HUMAN_RESOLUTION_RECORDED","message":"A human disposition already exists; further reconciliation can append original evidence without replay or double accounting."}));
            }
            if !remaining_reservations.is_empty() {
                blockers.push(json!({"code":"NON_CASH_HOLDS_RETAINED","message":"Outcome acceptance did not release position, nonce, order or protocol holds. Use the adapter's original-identity recovery or cancellation path.","reservations":remaining_reservations}));
            }
        } else if unresolved && record.submission.is_some() && record.status != Status::Submitted {
            blockers.push(json!({"code":"ORIGINAL_OUTCOME_UNRESOLVED","message":"The original action may still execute; balance changes alone do not establish its outcome."}));
            let cash_only = cash_only(&record);
            actions.push(json!({"operation":"operations.resolve-preview","operation_id":operation_id,"authorization":"fresh_touch_id",
                "mode":if cash_only{"cash_attestation"}else{"outcome_only_with_retained_non_cash_holds"},
                "requires":"Exact human-reported outcome and acceptance of uncertainty; no external proof is invented."}));
        }
        if let Some(error) = &record.error {
            blockers
                .push(json!({"code":error.code,"message":error.message,"details":error.details}));
            if record.submission.is_none() {
                let waiting = matches!(
                    error.code.as_str(),
                    "RPC_RATE_LIMITED"
                        | "RPC_UNAVAILABLE"
                        | "RPC_HISTORICAL_STATE_UNAVAILABLE"
                        | "PRICE_STALE"
                        | "STALE_VALUATION"
                        | "EVIDENCE_TIMEOUT"
                        | "RPC_READ_TIMEOUT"
                );
                actions.push(json!({"operation":if waiting{"wait_and_refresh"}else{"review_candidate"},"read_only":true,
                    "requires":if waiting{"Resolve the reported provider/cooldown/freshness cause before a new candidate. Exhausted logical retries are not reset by repeating the same ID."}else{"Correct the reported candidate or balance issue. Reuse still-active sufficient authority; a changed candidate gets its own durable attempt."}}));
            }
        }
        if let Some(plan) = &plan_context {
            actions.push(
                json!({"operation":"plans.status","plan_id":plan["plan_id"],"read_only":true}),
            );
            if plan["authority_active"] == false {
                blockers.push(json!({"code":"PLAN_AUTHORIZATION_INACTIVE","message":"Existing plan consent is expired or revoked; reconciliation and manual disposition remain available."}));
                actions.push(json!({"operation":"plans.authorize","replaces_revision_id":plan["revision_id"],"authorization":"fresh_touch_id",
                    "requires":"Resolve any active attempt, then review a replacement revision retaining completed-prefix effects and cumulative usage. Never rerun that prefix."}));
            }
        }
        Ok(
            json!({"operation":operation,"plan_context":plan_context,"blockers":blockers,"notes":notes,"actions":actions,
            "automatic_replay_allowed":false,"next_action":"Follow the applicable recovery action above. A manual disposition never renews future authority or substitutes for current balance checks."}),
        )
    }

    pub fn preview_operation_resolution(
        &self,
        operation_id: &str,
        input: &OperationResolutionInput,
    ) -> Result<Value> {
        let record = self.load_record(operation_id)?;
        validate_cash_attestation(&record, input)?;
        let store = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?;
        if !matches!(record.status, Status::Unknown | Status::Pending)
            || record.submission.is_none()
            || store
                .operation_resolution(operation_id)
                .map_err(map_store)?
                .is_some()
        {
            return Err(unavailable(
                "Only an unresolved submitted operation without an existing manual disposition can be resolved.",
            ));
        }
        let raw = to_value(&record)?;
        let submitted = store
            .submission(operation_id)
            .map_err(map_store)?
            .ok_or_else(|| unavailable("Original immutable submission missing."))?;
        let submission =
            json!({"kind":submitted.kind,"reference":submitted.reference,"data":submitted.data});
        if to_value(&record.submission)? != submission {
            return Err(unavailable("Original journal and submission disagree."));
        }
        let prepared = record.preview.as_ref().unwrap();
        let mut cash_ids: BTreeSet<_> = prepared
            .intent
            .debits
            .iter()
            .map(|row| (row.resource.clone(), row.amount.asset.clone()))
            .collect();
        cash_ids.extend(receipt_identities(&prepared.intent));
        for cost in &prepared.intent.maximum_costs {
            cash_ids.insert((cost.location.resource().clone(), cost.amount.asset.clone()));
        }
        let resources: Vec<_> = cash_ids
            .iter()
            .map(|(resource, _)| resource.0.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut keys = BTreeMap::new();
        for (resource, asset) in &cash_ids {
            keys.insert(
                resource_asset_key(&ResourceAmount {
                    resource: resource.clone(),
                    amount: Amount {
                        asset: asset.clone(),
                        atoms: gofin_domain::Quantity::zero(),
                    },
                })?,
                (resource.clone(), asset.clone()),
            );
        }
        for cost in &prepared.intent.maximum_costs {
            keys.insert(
                cost_key(&cost.location, &cost.amount.asset, &cost.treatment)?,
                (cost.location.resource().clone(), cost.amount.asset.clone()),
            );
        }
        let rows = store.reservations(operation_id).map_err(map_store)?;
        let reservations: Vec<_> = rows
            .iter()
            .map(|row| json!({"resource":row.resource,"units":row.units,"fee_units":row.fee_units}))
            .collect();
        let (released, remaining): (Vec<_>, Vec<_>) =
            reservations.iter().cloned().partition(|row| {
                row["resource"].as_str().is_some_and(|key| {
                    keys.contains_key(key)
                        || resources
                            .iter()
                            .any(|resource| key == format!("lock:{resource}"))
                })
            });
        let cash_only = cash_only(&record);
        let (released, remaining) = if cash_only && record.run_id.is_none() {
            (released, remaining)
        } else {
            (vec![], reservations.clone())
        };
        let mut cash = BTreeMap::<(ResourceId, gofin_domain::AssetId), BigUint>::new();
        for row in &rows {
            if let Some(identity) = keys.get(&row.resource) {
                *cash.entry(identity.clone()).or_default() +=
                    parse_uint(&row.units)? + parse_uint(&row.fee_units)?;
            }
        }
        let released_cash:Vec<_> = cash.into_iter().filter(|(_,amount)|!amount.is_zero())
            .map(|((resource,asset),amount)|json!({"resource":resource,"asset":asset,"amount_atoms":amount.to_string()})).collect();
        let plan = store
            .financial_plan_attempt_owner(operation_id)
            .map_err(map_store)?
            .map(|(id, _, _, _)| {
                store
                    .financial_plan(&id)
                    .map_err(map_store)
                    .and_then(|raw| raw.ok_or_else(|| unavailable("Original plan missing.")))
            })
            .transpose()?;
        let reservation = plan
            .as_ref()
            .map(|raw| from_value::<FinancialPlanAuthorizationRecord>(raw.clone()))
            .transpose()?
            .and_then(|plan| {
                plan.usage
                    .reservations
                    .into_iter()
                    .find(|row| row.attempt_id == operation_id)
            });
        let charged_debits = max_amounts(
            reservation
                .as_ref()
                .map(|row| row.debits.as_slice())
                .unwrap_or(&prepared.intent.debits),
            &input.actual_debits,
        )?;
        let charged_costs = max_costs(
            reservation
                .as_ref()
                .map(|row| row.costs.as_slice())
                .unwrap_or(&prepared.intent.maximum_costs),
            &input.actual_costs,
        )?;
        let intent = &prepared.intent;
        let breaches = input.actual_debits.iter().any(|actual| {
            intent
                .debits
                .iter()
                .find(|cap| {
                    cap.resource == actual.resource && cap.amount.asset == actual.amount.asset
                })
                .is_none_or(|cap| actual.amount.atoms > cap.amount.atoms)
        }) || input.actual_costs.iter().any(|actual| {
            intent
                .maximum_costs
                .iter()
                .find(|cap| {
                    cap.location == actual.location
                        && cap.amount.asset == actual.amount.asset
                        && cap.treatment == actual.treatment
                })
                .is_none_or(|cap| actual.amount.atoms > cap.amount.atoms)
        });
        let additional = extra_allocation(prepared, &input.actual_debits, &input.actual_costs)?;
        let review = json!({"version":1,"operation_id":record.id,"operation":record.operation,
            "original_request_id":record.request_id,"fingerprint":record.fingerprint,"submission":submission,
            "record_hash":binding_hash(&raw)?,"reservations_hash":binding_hash(&json!(reservations))?,
            "plan_hash":plan.as_ref().map(binding_hash).transpose()?,"attestation":input,
            "resources":if cash_only { resources } else {vec![]},"released_reservations":released,"released_cash":if cash_only {released_cash}else{vec![] },
            "remaining_reservations":remaining,
            "impact":{"historical_cap_breach":breaches,"future_authority_increased":false,
                "charged_debits":charged_debits,"charged_costs":charged_costs,"allocation_charges_retained":true,"plan_usage_retained":true,"additional_allocation_usd_micros":additional.to_string(),
                "receipt_provenance":"human_attested","plan_linked":plan.is_some()},
            "warnings":["The original submission may still execute. These amounts are human claims, not externally verified settlement.",
                "Reported receipts may fund dependent steps. Every new action still requires current available funds and its own remaining authority.",
                "Historical breaches and conservative risk charges remain. This decision never replays the original action."]});
        Ok(json!({"review_hash":binding_hash(&review)?,"review":review}))
    }

    pub async fn resolve_operation(
        &self,
        operation_id: &str,
        request_id: &str,
        expected_review_hash: &str,
        input: &OperationResolutionInput,
    ) -> Result<Value> {
        if request_id.trim().is_empty() || request_id.len() > 256 {
            return Err(unavailable("A bounded request ID is required."));
        }
        let _guard = self.write_queue.lock().await;
        if let Some(existing) = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .operation_resolution_request(request_id)
            .map_err(map_store)?
        {
            if existing["operation_id"] != operation_id
                || existing["review_hash"] != expected_review_hash
                || existing["review"]["attestation"] != to_value(input)?
            {
                return Err(GoFinError::new(
                    "REQUEST_ID_CONFLICT",
                    "recovery",
                    "Request ID already names a different resolution.",
                    "Use the original exact request.",
                ));
            }
            return Ok(existing);
        }
        let preview = self.preview_operation_resolution(operation_id, input)?;
        if preview["review_hash"] != expected_review_hash {
            return Err(unavailable(
                "The operation, plan or reservations changed after preview.",
            ));
        }
        let review = &preview["review"];
        let binding = json!({"version":1,"action":"operation_resolution","request_id":request_id,"review_hash":expected_review_hash,"review":review});
        let challenge=AuthorizationChallenge{purpose:"operation_resolution".into(),operation_id:operation_id.into(),
            binding_hash:binding_hash(&binding)?,binding,summary:"Accept exact human-reported outcome and reviewed accounting; original submission may still execute.".into(),
            nonce:format!("{}{}",Uuid::new_v4().simple(),Uuid::new_v4().simple()),
            expires_at:(self.clock.now()+chrono::Duration::seconds(90)).to_rfc3339()};
        gofin_domain::execution_progress::emit(
            gofin_domain::execution_progress::Stage::AwaitingApproval,
        );
        let audit = self.authorizer.authorize(&challenge).await?;
        validate_approval(&audit, &challenge, self.clock.now())?;
        if audit.source != Some(gofin_domain::AuthorizationSource::TouchId)
            || audit.yolo_provenance.is_some()
        {
            return Err(unavailable(
                "Manual financial disposition requires fresh Touch ID, including when Yolo is enabled.",
            ));
        }
        let mut store = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?;
        let transition = store
            .financial_plan_attempt_owner(operation_id)
            .map_err(map_store)?
            .map(|(plan_id, _, step_id, _)| -> Result<_> {
                let expected = store
                    .financial_plan(&plan_id)
                    .map_err(map_store)?
                    .ok_or_else(|| unavailable("Original plan missing."))?;
                let mut plan: FinancialPlanAuthorizationRecord = from_value(expected.clone())?;
                if plan.active_attempt_id.as_deref() != Some(operation_id) {
                    return Err(unavailable(
                        "The original operation no longer owns the active plan attempt.",
                    ));
                }
                // Preserve the full original reservation as conservative usage.
                // Larger reported actuals increase consumption, never authority.
                let reservation = plan
                    .usage
                    .reservations
                    .iter()
                    .find(|row| row.attempt_id == operation_id)
                    .cloned();
                let debits = max_amounts(
                    reservation
                        .as_ref()
                        .map(|row| row.debits.as_slice())
                        .unwrap_or(&[]),
                    &input.actual_debits,
                )?;
                let costs = max_costs(
                    reservation
                        .as_ref()
                        .map(|row| row.costs.as_slice())
                        .unwrap_or(&[]),
                    &input.actual_costs,
                )?;
                for debit in &debits {
                    consume_reported_funding(
                        &mut plan,
                        &step_id,
                        &debit.resource,
                        &debit.amount.asset,
                        &debit.amount.atoms.integer(),
                        false,
                    );
                }
                for cost in &costs {
                    if matches!(
                        cost.treatment,
                        CostTreatment::AdditionalDebit
                            | CostTreatment::VenueCalculatedAdditionalDebit
                    ) {
                        consume_reported_funding(
                            &mut plan,
                            &step_id,
                            cost.location.resource(),
                            &cost.amount.asset,
                            &cost.amount.atoms.integer(),
                            true,
                        );
                    }
                }
                plan.usage.consumed_debits = add_amounts(&plan.usage.consumed_debits, &debits)?;
                plan.usage.consumed_costs = add_cost_bounds(&plan.usage.consumed_costs, &costs)?;
                plan.usage
                    .reservations
                    .retain(|row| row.attempt_id != operation_id);
                plan.active_attempt_id = None;
                for receipt in &input.reported_receipts {
                    plan.usage.human_attested_proceeds.push(
                        gofin_domain::financial_plans::FinancialPlanProceeds {
                            producing_step_id: step_id.clone(),
                            resource: receipt.resource.clone(),
                            asset: receipt.amount.asset.clone(),
                            available: receipt.amount.atoms.clone(),
                            consumed: gofin_domain::Quantity::zero(),
                        },
                    );
                }
                Ok((plan_id, expected, to_value(&plan)?))
            })
            .transpose()?;
        store
            .persist_operation_resolution(
                request_id,
                review,
                &to_value(&audit)?,
                transition
                    .as_ref()
                    .map(|(id, before, after)| (id.as_str(), before, after)),
                &self.clock.now().to_rfc3339(),
            )
            .map_err(map_store)
    }

    pub async fn recover_operation_receipt(
        &self,
        operation_id: &str,
        reference: &RecoveryReference,
    ) -> Result<Value> {
        if reference.reference.trim().is_empty() || reference.reference.len() > 512 {
            return Err(unavailable(
                "A bounded original receipt reference is required.",
            ));
        }
        let _guard = self.write_queue.lock().await;
        let record = self.load_record(operation_id)?;
        let adapter = self
            .adapters
            .read()
            .map_err(|_| internal("registry lock poisoned"))?
            .get(&record.operation)
            .cloned()
            .ok_or_else(|| unavailable("The original operation adapter is unavailable."))?;
        let reconciliation = adapter.reconcile_with_reference(&record, reference).await?;
        let result = self.apply_reconciliation_with_baselines(record, reconciliation, vec![])?;
        self.operation_view(&result)
    }

    fn append_human_observation(
        &self,
        record: &OperationRecord,
        observed_debits: &[ResourceAmount],
        observed_costs: &[CostBound],
        mut observation: Value,
    ) -> Result<()> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?;
        let Some(resolution) = store.operation_resolution(&record.id).map_err(map_store)? else {
            return Ok(());
        };
        let (prior_debits, prior_costs) = human_charged_totals(&resolution)?;
        let observed_debits = add_amounts(&[], observed_debits)?;
        let observed_costs = add_cost_bounds(&[], observed_costs)?;
        let debits = max_amounts(&prior_debits, &observed_debits)?;
        let costs = max_costs(&prior_costs, &observed_costs)?;
        let additional = extra_allocation(
            record
                .preview
                .as_ref()
                .ok_or_else(|| unavailable("Original intent missing."))?,
            &debits,
            &costs,
        )?;
        observation["charged_debits"] = to_value(&debits)?;
        observation["charged_costs"] = to_value(&costs)?;
        observation["additional_allocation_usd_micros"] = json!(additional.to_string());
        let delta_debits = debits
            .iter()
            .filter_map(|row| {
                let old = prior_debits
                    .iter()
                    .find(|old| {
                        old.resource == row.resource && old.amount.asset == row.amount.asset
                    })
                    .map(|row| row.amount.atoms.integer())
                    .unwrap_or_default();
                (row.amount.atoms.integer() > old).then(|| {
                    let mut delta = row.clone();
                    delta.amount.atoms =
                        gofin_domain::Quantity::new((row.amount.atoms.integer() - old).to_string())
                            .expect("positive");
                    delta
                })
            })
            .collect::<Vec<_>>();
        let delta_costs = costs
            .iter()
            .filter_map(|row| {
                let old = prior_costs
                    .iter()
                    .find(|old| {
                        old.location == row.location
                            && old.amount.asset == row.amount.asset
                            && old.treatment == row.treatment
                    })
                    .map(|row| row.amount.atoms.integer())
                    .unwrap_or_default();
                (row.amount.atoms.integer() > old).then(|| {
                    let mut delta = row.clone();
                    delta.amount.atoms =
                        gofin_domain::Quantity::new((row.amount.atoms.integer() - old).to_string())
                            .expect("positive");
                    delta
                })
            })
            .collect::<Vec<_>>();
        let transition = store
            .financial_plan_attempt_owner(&record.id)
            .map_err(map_store)?
            .map(|(id, _, step, _)| -> Result<_> {
                let expected = store
                    .financial_plan(&id)
                    .map_err(map_store)?
                    .ok_or_else(|| unavailable("Original plan missing."))?;
                let mut plan: FinancialPlanAuthorizationRecord = from_value(expected.clone())?;
                for row in &delta_debits {
                    consume_reported_funding(
                        &mut plan,
                        &step,
                        &row.resource,
                        &row.amount.asset,
                        &row.amount.atoms.integer(),
                        false,
                    );
                }
                for row in &delta_costs {
                    if matches!(
                        row.treatment,
                        CostTreatment::AdditionalDebit
                            | CostTreatment::VenueCalculatedAdditionalDebit
                    ) {
                        consume_reported_funding(
                            &mut plan,
                            &step,
                            row.location.resource(),
                            &row.amount.asset,
                            &row.amount.atoms.integer(),
                            true,
                        );
                    }
                }
                plan.usage.consumed_debits =
                    add_amounts(&plan.usage.consumed_debits, &delta_debits)?;
                plan.usage.consumed_costs =
                    add_cost_bounds(&plan.usage.consumed_costs, &delta_costs)?;
                Ok((id, expected, to_value(&plan)?))
            })
            .transpose()?;
        store
            .record_resolution_confirmation_with_plan(
                &record.id,
                &observation,
                transition
                    .as_ref()
                    .map(|(id, expected, updated)| (id.as_str(), expected, updated)),
            )
            .map_err(map_store)
    }

    pub(super) fn record_human_consumed_costs(
        &self,
        record: &OperationRecord,
        proof: &gofin_domain::unresolved_consumption::UnresolvedCostConsumption,
    ) -> Result<()> {
        let costs = proof
            .costs
            .iter()
            .filter_map(|effect| match effect {
                FinancialEffect::Cost {
                    location,
                    amount,
                    treatment,
                    ..
                } => Some(CostBound {
                    location: location.clone(),
                    amount: amount.clone(),
                    treatment: treatment.clone(),
                }),
                _ => None,
            })
            .collect::<Vec<_>>();
        let resolution = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .operation_resolution(&record.id)
            .map_err(map_store)?;
        let Some(resolution) = resolution else {
            return Ok(());
        };
        let attested: OperationResolutionInput =
            from_value(resolution["review"]["attestation"].clone())?;
        let totals = add_cost_bounds(&[], &costs)?;
        let contradicted = totals.iter().any(|row| {
            attested
                .actual_costs
                .iter()
                .find(|old| {
                    old.location == row.location
                        && old.amount.asset == row.amount.asset
                        && old.treatment == row.treatment
                })
                .is_none_or(|old| row.amount.atoms > old.amount.atoms)
        });
        self.append_human_observation(record,&[],&costs,json!({"status":if contradicted{"contradicts"}else{"pending"},"provenance":"adapter_observation","unresolved_consumption":proof,"accounted_again":false,"next_action":"Later proven costs remain charged without granting receipts or wider future authority."}))
    }

    pub(super) fn record_human_resolution_observation(
        &self,
        record: &OperationRecord,
        reconciliation: &Reconciliation,
    ) -> Result<bool> {
        // Independently proven late costs must reach their existing cumulative
        // watermark path even after human acceptance; it never mints receipts.
        if reconciliation
            .evidence
            .product_get(gofin_domain::unresolved_consumption::KEY)?
            .is_some()
        {
            return Ok(false);
        }
        let resolution = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .operation_resolution(&record.id)
            .map_err(map_store)?;
        let Some(resolution) = resolution else {
            return Ok(false);
        };
        if let Some(verified) = &reconciliation.verified {
            if record.submission.as_ref() != Some(&verified.submission)
                || verified.proof_digest != verified_effect_digest(verified)?
            {
                return Err(unavailable(
                    "Later proof does not match the original submission or digest.",
                ));
            }
            let attested: OperationResolutionInput =
                from_value(resolution["review"]["attestation"].clone())?;
            let mut debits = Vec::new();
            let mut costs = Vec::new();
            let mut receipts = Vec::new();
            for effect in &verified.effects {
                match effect {
                    FinancialEffect::CashDebit { resource, amount } => {
                        debits.push(ResourceAmount {
                            resource: resource.clone(),
                            amount: amount.clone(),
                        })
                    }
                    FinancialEffect::UsableCredit { to, amount } => receipts.push(ResourceAmount {
                        resource: to.clone(),
                        amount: amount.clone(),
                    }),
                    FinancialEffect::Cost {
                        location,
                        amount,
                        treatment,
                        ..
                    } => costs.push(CostBound {
                        location: location.clone(),
                        amount: amount.clone(),
                        treatment: treatment.clone(),
                    }),
                    FinancialEffect::Transfer {
                        from,
                        to,
                        debit,
                        credit,
                    } => {
                        debits.push(ResourceAmount {
                            resource: from.clone(),
                            amount: debit.clone(),
                        });
                        receipts.push(ResourceAmount {
                            resource: to.clone(),
                            amount: credit.clone(),
                        });
                    }
                    FinancialEffect::Conversion {
                        resource,
                        spent,
                        received,
                    } => {
                        debits.push(ResourceAmount {
                            resource: resource.clone(),
                            amount: spent.clone(),
                        });
                        receipts.push(ResourceAmount {
                            resource: resource.clone(),
                            amount: received.clone(),
                        });
                    }
                    _ => {}
                }
            }
            let matches = (!cash_only(record)
                || (add_amounts(&[], &debits)? == add_amounts(&[], &attested.actual_debits)?
                    && add_cost_bounds(&[], &costs)?
                        == add_cost_bounds(&[], &attested.actual_costs)?
                    && add_amounts(&[], &receipts)?
                        == add_amounts(&[], &attested.reported_receipts)?))
                && (reconciliation.status == Status::Completed)
                    == (attested.outcome == OperationResolutionOutcome::Succeeded);
            let observation = json!({"status":if !matches!(reconciliation.status,Status::Completed|Status::Failed){"pending"}else if matches{"agrees"}else{"contradicts"},
                "provenance":"adapter_observation","observed_at":verified.observed_at,"verified_effects":verified,
                "accounted_again":false,"next_action":if matches{"Original evidence corroborates the human audit; conservative usage remains."}else{"Human claim and later external observation differ. Preserve both, inspect current balances and remaining authority before more writes."}});
            self.append_human_observation(record, &debits, &costs, observation)?;
        }
        // Outcome-only acceptance retained non-cash reservations and anchors.
        // Genuine later terminal evidence must still reach the owned settlement
        // path to release them. Manual plan usage is already charged, and its
        // inactive attempt is not accounted again by commit_terminal.
        Ok(cash_only(record)
            || reconciliation.verified.is_none()
            || !matches!(reconciliation.status, Status::Completed | Status::Failed))
    }
}

fn max_amounts(
    reserved: &[ResourceAmount],
    actual: &[ResourceAmount],
) -> Result<Vec<ResourceAmount>> {
    let mut result = reserved.to_vec();
    for item in actual {
        if let Some(row) = result
            .iter_mut()
            .find(|row| row.resource == item.resource && row.amount.asset == item.amount.asset)
        {
            row.amount.atoms = std::cmp::max(row.amount.atoms.clone(), item.amount.atoms.clone());
        } else {
            result.push(item.clone());
        }
    }
    Ok(result)
}
fn max_costs(reserved: &[CostBound], actual: &[CostBound]) -> Result<Vec<CostBound>> {
    let mut result = reserved.to_vec();
    for item in actual {
        if let Some(row) = result.iter_mut().find(|row| {
            row.location == item.location
                && row.amount.asset == item.amount.asset
                && row.treatment == item.treatment
        }) {
            row.amount.atoms = std::cmp::max(row.amount.atoms.clone(), item.amount.atoms.clone());
        } else {
            result.push(item.clone());
        }
    }
    Ok(result)
}

// Human acceptance records historical consumption even above producer proceeds.
// Consume at most what existed; full reported spending remains charged separately.
pub(super) fn consume_reported_funding(
    plan: &mut FinancialPlanAuthorizationRecord,
    step_id: &str,
    resource: &ResourceId,
    asset: &gofin_domain::AssetId,
    amount: &BigUint,
    fee: bool,
) {
    let producers: Vec<_> = plan
        .revision
        .spec
        .steps
        .iter()
        .find(|step| step.step_id == step_id)
        .into_iter()
        .flat_map(|step| step.funding_dependencies.iter())
        .filter(|dependency| {
            &dependency.resource == resource
                && &dependency.asset == asset
                && (!fee || dependency.includes_additional_costs())
        })
        .map(|dependency| dependency.producing_step_id.clone())
        .collect();
    let mut remaining = amount.clone();
    for row in plan
        .usage
        .verified_proceeds
        .iter_mut()
        .chain(plan.usage.human_attested_proceeds.iter_mut())
        .filter(|row| {
            producers.contains(&row.producing_step_id)
                && &row.resource == resource
                && &row.asset == asset
        })
    {
        let available = row.available.integer();
        let consumed = row.consumed.integer();
        if consumed > available {
            continue;
        }
        let used = std::cmp::min(available - consumed, remaining.clone());
        row.consumed = gofin_domain::Quantity::new((row.consumed.integer() + &used).to_string())
            .expect("nonnegative sum");
        remaining -= used;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Approver {
        calls: AtomicUsize,
        source: gofin_domain::AuthorizationSource,
        mode: &'static str,
        store: SharedStore,
    }
    #[async_trait::async_trait]
    impl gofin_domain::Authorizer for Approver {
        async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(c.purpose, "operation_resolution");
            if self.mode == "cancel" {
                return Err(unavailable("Synthetic cancellation"));
            }
            if self.mode == "mutate" {
                let mut store = self.store.lock().unwrap();
                let mut raw = store.get_record("original").unwrap().unwrap();
                raw["updated_at"] = json!("2026-01-01T00:00:00Z");
                store
                    .update_record("original", &raw, "2026-01-01T00:00:00Z")
                    .unwrap();
            }
            Ok(ApprovalAudit {
                source: Some(self.source.clone()),
                yolo_provenance: None,
                mechanism: "synthetic-touch-id".into(),
                binding_hash: c.binding_hash.clone(),
                challenge_nonce: c.nonce.clone(),
                approved_at: if self.mode == "expired" {
                    "2000-01-01T00:00:00Z".into()
                } else {
                    now()
                },
                expires_at: if self.mode == "expired" {
                    "2000-01-01T00:01:00Z".into()
                } else {
                    c.expires_at.clone()
                },
            })
        }
    }
    fn fixture(
        source: gofin_domain::AuthorizationSource,
        mode: &'static str,
    ) -> (Engine, SharedStore, Arc<Approver>, OperationResolutionInput) {
        let store = crate::reservation_overrides::tests::seeded_store();
        let approver = Arc::new(Approver {
            calls: AtomicUsize::new(0),
            source,
            mode,
            store: store.clone(),
        });
        let engine = Engine::new_with_shared_store(
            store.clone(),
            vec![],
            approver.clone(),
            EngineOptions::default(),
        )
        .unwrap();
        let record = engine.load_record("original").unwrap();
        let intent = &record.preview.unwrap().intent;
        let input = OperationResolutionInput {
            outcome: OperationResolutionOutcome::Succeeded,
            reason: "Human checked the original transfer".into(),
            actual_debits: intent.debits.clone(),
            actual_costs: intent.maximum_costs.clone(),
            reported_receipts: vec![],
        };
        (engine, store, approver, input)
    }
    #[tokio::test]
    async fn manual_resolution_preserves_raw_history_and_retries_exactly_once() {
        let (engine, store, approver, input) =
            fixture(gofin_domain::AuthorizationSource::TouchId, "ok");
        let before = engine.load_record("original").unwrap();
        let rows = store.lock().unwrap().reservations("original").unwrap();
        let review = engine
            .preview_operation_resolution("original", &input)
            .unwrap();
        let result = engine
            .resolve_operation(
                "original",
                "accept",
                review["review_hash"].as_str().unwrap(),
                &input,
            )
            .await
            .unwrap();
        assert_eq!(result["provenance"], "human_attested");
        assert_eq!(
            engine
                .resolve_operation(
                    "original",
                    "accept",
                    review["review_hash"].as_str().unwrap(),
                    &input
                )
                .await
                .unwrap(),
            result
        );
        assert_eq!(approver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            to_value(&engine.load_record("original").unwrap()).unwrap(),
            to_value(&before).unwrap()
        );
        assert_eq!(
            store.lock().unwrap().reservations("original").unwrap(),
            rows
        );
        assert!(engine.load_record("original").unwrap().effects.is_none());
        assert_eq!(
            ordinary_policy::effective_conflict_resources(&store.lock().unwrap(), &before).unwrap(),
            BTreeSet::from([ResourceId("synthetic:order".into())])
        );
        let view = engine.operation_recovery("original").unwrap();
        assert!(
            view["actions"]
                .as_array()
                .unwrap()
                .iter()
                .all(|action| action["operation"] != "operations.resolve-preview")
        );
        let restarted = Engine::new_with_shared_store(
            store,
            vec![],
            approver.clone(),
            EngineOptions::default(),
        )
        .unwrap();
        assert_eq!(
            restarted
                .resolve_operation(
                    "original",
                    "accept",
                    review["review_hash"].as_str().unwrap(),
                    &input
                )
                .await
                .unwrap(),
            result
        );
        let mut changed = input;
        changed.reason.push_str(" changed");
        assert_eq!(
            restarted
                .resolve_operation(
                    "original",
                    "accept",
                    review["review_hash"].as_str().unwrap(),
                    &changed
                )
                .await
                .unwrap_err()
                .code,
            "REQUEST_ID_CONFLICT"
        );
    }
    #[tokio::test]
    async fn manual_resolution_rejects_wrong_authority_cancel_expiry_and_changed_snapshot() {
        for (source, mode) in [
            (gofin_domain::AuthorizationSource::Yolo, "ok"),
            (gofin_domain::AuthorizationSource::NamedPlan, "ok"),
            (gofin_domain::AuthorizationSource::Sudo, "ok"),
            (gofin_domain::AuthorizationSource::TouchId, "cancel"),
            (gofin_domain::AuthorizationSource::TouchId, "expired"),
            (gofin_domain::AuthorizationSource::TouchId, "mutate"),
        ] {
            let (engine, store, _, input) = fixture(source, mode);
            let review = engine
                .preview_operation_resolution("original", &input)
                .unwrap();
            assert!(
                engine
                    .resolve_operation(
                        "original",
                        "accept",
                        review["review_hash"].as_str().unwrap(),
                        &input
                    )
                    .await
                    .is_err(),
                "{mode}"
            );
            assert!(
                store
                    .lock()
                    .unwrap()
                    .operation_resolution("original")
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                store
                    .lock()
                    .unwrap()
                    .all_effective_reservations()
                    .unwrap()
                    .len(),
                3
            );
        }
    }
    #[tokio::test]
    async fn outcome_only_with_position_cost_keeps_every_hold_and_allows_later_owned_proof() {
        let (engine, store, _, mut input) =
            fixture(gofin_domain::AuthorizationSource::TouchId, "ok");
        {
            let mut store = store.lock().unwrap();
            let mut raw = store.get_record("original").unwrap().unwrap();
            raw["preview"]["intent"]["maximum_costs"][0]["location"] = json!({"kind":"position","resource":"synthetic:account","position":"supply","position_kind":"supply"});
            raw["preview"]["intent"]["maximum_costs"][0]["treatment"] = json!("included_in_debit");
            raw["preview"]["intent"]["expected"] = json!([{
                "kind":"authority","resource":"synthetic:account","authority":"synthetic-allowance","before":"0","after":"1"
            }]);
            let prepared: PreparedOperation = from_value(raw["preview"].clone()).unwrap();
            raw["approval"] = to_value(&ApprovalAudit {
                source: Some(gofin_domain::AuthorizationSource::TouchId),
                yolo_provenance: None,
                mechanism: "synthetic-original".into(),
                binding_hash: binding_hash(&authorization_binding(&prepared)).unwrap(),
                challenge_nonce: "original".into(),
                approved_at: now(),
                expires_at: prepared.expires_at.clone(),
            })
            .unwrap();
            store.update_record("original", &raw, &now()).unwrap();
        }
        input.actual_debits.clear();
        input.actual_costs.clear();
        let review = engine
            .preview_operation_resolution("original", &input)
            .unwrap();
        assert_eq!(review["review"]["resources"], json!([]));
        assert_eq!(review["review"]["released_reservations"], json!([]));
        engine
            .resolve_operation(
                "original",
                "accept",
                review["review_hash"].as_str().unwrap(),
                &input,
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .lock()
                .unwrap()
                .all_effective_reservations()
                .unwrap()
                .len(),
            3
        );
        let record = engine.load_record("original").unwrap();
        let mut proof = VerifiedEffects {
            evidence_version: 1,
            submission: record.submission.clone().unwrap(),
            effects: vec![FinancialEffect::AuthorityChange {
                resource: ResourceId("synthetic:account".into()),
                authority: "synthetic-allowance".into(),
                before: "0".into(),
                after: "1".into(),
            }],
            evidence: json!({"synthetic":"owned-proof"}),
            observed_at: now(),
            proof_digest: String::new(),
        };
        proof.proof_digest = verified_effect_digest(&proof).unwrap();
        let reconciliation = Reconciliation {
            status: Status::Completed,
            evidence: json!({}),
            verified: Some(proof),
        };
        let settled = engine
            .apply_reconciliation_with_baselines(record, reconciliation, vec![])
            .unwrap();
        assert_eq!(settled.status, Status::Completed);
        assert!(matches!(
            settled.effects.unwrap().effects[0],
            FinancialEffect::AuthorityChange { .. }
        ));
        assert!(
            store
                .lock()
                .unwrap()
                .reservations("original")
                .unwrap()
                .is_empty()
        );
        let recovery = engine.operation_recovery("original").unwrap();
        assert!(
            recovery["blockers"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["code"] != "NON_CASH_HOLDS_RETAINED"
                    && row["code"] != "HUMAN_RESOLUTION_RECORDED")
        );
        assert_eq!(
            recovery["operation"]["manual_resolution"]["external_confirmation"]["status"],
            "agrees"
        );
    }
    #[tokio::test]
    async fn late_cash_observations_are_append_only_and_never_create_verified_effects() {
        let (engine, store, _, input) = fixture(gofin_domain::AuthorizationSource::TouchId, "ok");
        let review = engine
            .preview_operation_resolution("original", &input)
            .unwrap();
        engine
            .resolve_operation(
                "original",
                "accept",
                review["review_hash"].as_str().unwrap(),
                &input,
            )
            .await
            .unwrap();
        let record = engine.load_record("original").unwrap();
        let mut proof = VerifiedEffects {
            evidence_version: 1,
            submission: record.submission.clone().unwrap(),
            effects: vec![FinancialEffect::CashDebit {
                resource: input.actual_debits[0].resource.clone(),
                amount: input.actual_debits[0].amount.clone(),
            }],
            evidence: json!({"synthetic":"different-fee"}),
            observed_at: now(),
            proof_digest: String::new(),
        };
        proof.proof_digest = verified_effect_digest(&proof).unwrap();
        let reconciliation = Reconciliation {
            status: Status::Completed,
            evidence: json!({}),
            verified: Some(proof),
        };
        for _ in 0..2 {
            assert!(
                engine
                    .record_human_resolution_observation(&record, &reconciliation)
                    .unwrap()
            );
        }
        let confirmation = store
            .lock()
            .unwrap()
            .operation_resolution("original")
            .unwrap()
            .unwrap()["external_confirmation"]
            .clone();
        assert_eq!(confirmation["status"], "contradicts");
        assert_eq!(confirmation["observations"].as_array().unwrap().len(), 1);
        assert!(engine.load_record("original").unwrap().effects.is_none());
        assert!(
            engine.operation_recovery("original").unwrap()["blockers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["code"] == "HUMAN_RESOLUTION_CONTRADICTED")
        );
    }
    #[tokio::test]
    async fn late_proven_costs_keep_cumulative_budget_without_double_counting() {
        use gofin_domain::unresolved_consumption::{UnresolvedCostConsumption, attach};
        let (engine, store, _, input) = fixture(gofin_domain::AuthorizationSource::TouchId, "ok");
        {
            let mut db = store.lock().unwrap();
            let mut raw = db.get_record("original").unwrap().unwrap();
            raw["preview"]["valuation"]["prices"] = to_value(&vec![gofin_domain::AssetPrice {
                asset: input.actual_costs[0].amount.asset.clone(),
                usd_micros_per_unit: gofin_domain::Quantity::from_u64(1_000_000),
                unit_atoms: gofin_domain::Quantity::from_u64(1),
                observed_at: now(),
                source: "synthetic".into(),
            }])
            .unwrap();
            db.update_record("original", &raw, &now()).unwrap();
        }
        let review = engine
            .preview_operation_resolution("original", &input)
            .unwrap();
        engine
            .resolve_operation(
                "original",
                "accept",
                review["review_hash"].as_str().unwrap(),
                &input,
            )
            .await
            .unwrap();
        let record = engine.load_record("original").unwrap();
        let before =
            unresolved_consumption::record_charge(&store.lock().unwrap(), &record).unwrap();
        let cost = &input.actual_costs[0];
        let mut amount = cost.amount.clone();
        amount.atoms = gofin_domain::Quantity::from_u64(3);
        let proof = UnresolvedCostConsumption::new(
            record.submission.clone().unwrap(),
            vec![FinancialEffect::Cost {
                location: cost.location.clone(),
                amount,
                category: "late-owned-fee".into(),
                treatment: cost.treatment.clone(),
            }],
            json!({"proofEvents":[{"eventId":"fee-final","amount":"3"}]}),
            now(),
        )
        .unwrap();
        let mut reconciliation = Reconciliation {
            status: Status::Unknown,
            evidence: json!({}),
            verified: None,
        };
        attach(&mut reconciliation, &proof).unwrap();
        for _ in 0..2 {
            let retained = engine
                .apply_reconciliation_with_baselines(
                    engine.load_record("original").unwrap(),
                    reconciliation.clone(),
                    vec![],
                )
                .unwrap();
            assert_eq!(retained.error.unwrap().code, "COST_BOUND_EXCEEDED");
            assert!(retained.effects.is_none());
        }
        let current = engine.load_record("original").unwrap();
        let db = store.lock().unwrap();
        assert_eq!(
            db.unresolved_consumption_usd("original")
                .unwrap()
                .as_deref(),
            Some("1000000")
        );
        assert_eq!(
            unresolved_consumption::record_charge(&db, &current).unwrap(),
            before + BigUint::from(1_000_000u64)
        );
        assert_eq!(
            db.operation_resolution("original").unwrap().unwrap()["outcome"],
            "succeeded"
        );
    }

    #[tokio::test]
    async fn manual_failed_attempt_remains_in_consumer_fraction_accounting() {
        let (engine, store, _, mut input) =
            fixture(gofin_domain::AuthorizationSource::TouchId, "ok");
        input.outcome = OperationResolutionOutcome::Failed;
        let review = engine
            .preview_operation_resolution("original", &input)
            .unwrap();
        engine
            .resolve_operation(
                "original",
                "accept",
                review["review_hash"].as_str().unwrap(),
                &input,
            )
            .await
            .unwrap();
        let record = engine.load_record("original").unwrap();
        super::financial_plans::tests::assert_human_fraction_consumption(
            &store.lock().unwrap(),
            &record,
        );
    }

    #[test]
    fn cash_shaped_position_cost_or_run_never_releases_holds() {
        for position_cost in [true, false] {
            let (engine, store, _, mut input) =
                fixture(gofin_domain::AuthorizationSource::TouchId, "ok");
            let mut db = store.lock().unwrap();
            let mut raw = db.get_record("original").unwrap().unwrap();
            if position_cost {
                raw["preview"]["intent"]["maximum_costs"][0]["location"] = json!({"kind":"position","resource":"synthetic:account","position":"supply","position_kind":"supply"});
            } else {
                raw["run_id"] = json!("original-run");
            }
            db.update_record("original", &raw, &now()).unwrap();
            drop(db);
            input.actual_debits.clear();
            input.actual_costs.clear();
            let review = engine
                .preview_operation_resolution("original", &input)
                .unwrap();
            assert_eq!(review["review"]["resources"], json!([]));
            assert_eq!(review["review"]["released_reservations"], json!([]));
            assert_eq!(review["review"]["released_cash"], json!([]));
            assert_eq!(
                review["review"]["remaining_reservations"]
                    .as_array()
                    .unwrap()
                    .len(),
                3
            );
        }
    }

    fn linked_fixture() -> (Engine, SharedStore, OperationResolutionInput) {
        use gofin_domain::financial_plans::{
            FinancialPlanAttemptLinkage, FinancialPlanAuthorizationStatus, FinancialPlanReservation,
        };
        let mut db = Store::in_memory().unwrap();
        let mut raw = crate::reservation_overrides::tests::seed_record("original");
        let mut operation: OperationRecord = from_value(raw.clone()).unwrap();
        let prepared = operation.preview.as_mut().unwrap();
        prepared.valuation.as_mut().unwrap().prices = vec![gofin_domain::AssetPrice {
            asset: prepared.intent.debits[0].amount.asset.clone(),
            usd_micros_per_unit: gofin_domain::Quantity::from_u64(1_000_000),
            unit_atoms: gofin_domain::Quantity::from_u64(1),
            observed_at: now(),
            source: "synthetic".into(),
        }];
        raw = to_value(&operation).unwrap();
        let input = OperationResolutionInput {
            outcome: OperationResolutionOutcome::Succeeded,
            reason: "Human accepts original uncertain outcome".into(),
            actual_debits: prepared_clone_debits(&operation),
            actual_costs: operation
                .preview
                .as_ref()
                .unwrap()
                .intent
                .maximum_costs
                .clone(),
            reported_receipts: vec![],
        };
        let mut plan = super::financial_plans::tests::prior_record();
        plan.revision.status = FinancialPlanAuthorizationStatus::Active;
        plan.revision.expires_at = (Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();
        plan.usage = Default::default();
        plan.attempts.clear();
        plan.active_attempt_id = None;
        db.begin_financial_plan_request("plan-request", "plan-fingerprint", "plan", &now())
            .unwrap();
        db.commit_financial_plan_revision(
            "plan-request",
            "plan-fingerprint",
            "plan",
            "rev-1",
            None,
            &to_value(&plan.revision).unwrap(),
            &to_value(&plan).unwrap(),
            &now(),
        )
        .unwrap();
        let initial = to_value(&plan).unwrap();
        plan.active_attempt_id = Some("original".into());
        plan.attempts.push(FinancialPlanAttemptLinkage {
            plan_id: "plan".into(),
            revision_id: "rev-1".into(),
            step_id: "step-1".into(),
            attempt_id: "original".into(),
            request_fingerprint: operation.fingerprint.clone(),
            execution_binding_hash: "synthetic".into(),
            logical_request_id: None,
            submission_id: None,
            result: None,
        });
        plan.usage.reservations.push(FinancialPlanReservation {
            attempt_id: "original".into(),
            step_id: "step-1".into(),
            debits: input.actual_debits.clone(),
            costs: input.actual_costs.clone(),
        });
        db.create_financial_plan_attempt(
            "plan",
            "rev-1",
            "step-1",
            &initial,
            &to_value(&plan).unwrap(),
            "original",
            &operation.request_id,
            &operation.fingerprint,
            &operation.operation,
            &raw,
            &now(),
        )
        .unwrap();
        db.persist_submission(
            "original",
            &gofin_store::SubmissionRow {
                kind: "synthetic".into(),
                reference: "immutable-identity".into(),
                data: json!({"nonce":17}),
            },
            &now(),
        )
        .unwrap();
        raw = db.get_record("original").unwrap().unwrap();
        raw["status"] = json!("unknown");
        db.update_record("original", &raw, &now()).unwrap();
        let store = Arc::new(Mutex::new(db));
        let approver = Arc::new(Approver {
            calls: AtomicUsize::new(0),
            source: gofin_domain::AuthorizationSource::TouchId,
            mode: "ok",
            store: store.clone(),
        });
        let engine = Engine::new_with_shared_store(
            store.clone(),
            vec![],
            approver,
            EngineOptions::default(),
        )
        .unwrap();
        (engine, store, input)
    }
    fn prepared_clone_debits(record: &OperationRecord) -> Vec<ResourceAmount> {
        record.preview.as_ref().unwrap().intent.debits.clone()
    }

    #[tokio::test]
    async fn late_terminal_and_unresolved_proofs_raise_linked_plan_usage_once() {
        for terminal in [true, false] {
            let (engine, store, input) = linked_fixture();
            let review = engine
                .preview_operation_resolution("original", &input)
                .unwrap();
            engine
                .resolve_operation(
                    "original",
                    "accept",
                    review["review_hash"].as_str().unwrap(),
                    &input,
                )
                .await
                .unwrap();
            let original = engine.load_record("original").unwrap();
            let mut cost = input.actual_costs[0].clone();
            cost.amount.atoms = gofin_domain::Quantity::from_u64(3);
            let fee = FinancialEffect::Cost {
                location: cost.location.clone(),
                amount: cost.amount.clone(),
                category: "owned-fee".into(),
                treatment: cost.treatment.clone(),
            };
            let mut fee_first = fee.clone();
            let mut fee_second = fee;
            if let FinancialEffect::Cost {
                amount, category, ..
            } = &mut fee_first
            {
                amount.atoms = gofin_domain::Quantity::from_u64(1);
                *category = "network-fee".into();
            }
            if let FinancialEffect::Cost {
                amount, category, ..
            } = &mut fee_second
            {
                amount.atoms = gofin_domain::Quantity::from_u64(2);
                *category = "venue-fee".into();
            }
            let reconciliation = if terminal {
                let mut debit = input.actual_debits[0].clone();
                debit.amount.atoms = gofin_domain::Quantity::from_u64(5);
                let mut second = debit.clone();
                second.amount.atoms = gofin_domain::Quantity::from_u64(6);
                let mut proof = VerifiedEffects {
                    evidence_version: 1,
                    submission: original.submission.clone().unwrap(),
                    effects: vec![
                        FinancialEffect::CashDebit {
                            resource: debit.resource,
                            amount: debit.amount,
                        },
                        FinancialEffect::CashDebit {
                            resource: second.resource,
                            amount: second.amount,
                        },
                        fee_first,
                        fee_second,
                    ],
                    evidence: json!({"synthetic":"late-actuals"}),
                    observed_at: now(),
                    proof_digest: String::new(),
                };
                proof.proof_digest = verified_effect_digest(&proof).unwrap();
                Reconciliation {
                    status: Status::Completed,
                    evidence: json!({}),
                    verified: Some(proof),
                }
            } else {
                let proof = gofin_domain::unresolved_consumption::UnresolvedCostConsumption::new(
                    original.submission.clone().unwrap(),
                    vec![fee_first, fee_second],
                    json!({"proofEvents":[{"eventId":"late-fee","amount":"3"}]}),
                    now(),
                )
                .unwrap();
                let mut r = Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({}),
                    verified: None,
                };
                gofin_domain::unresolved_consumption::attach(&mut r, &proof).unwrap();
                r
            };
            for _ in 0..2 {
                engine
                    .apply_reconciliation_with_baselines(
                        engine.load_record("original").unwrap(),
                        reconciliation.clone(),
                        vec![],
                    )
                    .unwrap();
            }
            let db = store.lock().unwrap();
            let plan: FinancialPlanAuthorizationRecord =
                from_value(db.financial_plan("plan").unwrap().unwrap()).unwrap();
            assert_eq!(plan.usage.consumed_costs[0].amount.atoms.to_string(), "3");
            assert_eq!(
                plan.usage.consumed_debits[0].amount.atoms.to_string(),
                if terminal { "11" } else { "10" }
            );
            let current: OperationRecord =
                from_value(db.get_record("original").unwrap().unwrap()).unwrap();
            assert_eq!(
                unresolved_consumption::record_charge(&db, &current)
                    .unwrap()
                    .to_string(),
                if terminal { "14000000" } else { "13000000" }
            );
            assert!(current.effects.is_none());
            assert!(plan.active_attempt_id.is_none());
            assert_eq!(db.operation_resolution("original").unwrap().unwrap()["external_confirmation"]["observations"].as_array().unwrap().len(),1);
        }
    }

    #[test]
    fn recovery_distinguishes_expired_authority_and_unsubmitted_provider_failure() {
        let (engine, store, _) = linked_fixture();
        let mut db = store.lock().unwrap();
        let expected = db.financial_plan("plan").unwrap().unwrap();
        let mut expired = expected.clone();
        expired["revision"]["expires_at"] = json!("2000-01-01T00:00:00Z");
        db.compare_update_financial_plan("plan", &expected, &expired, &now())
            .unwrap();
        drop(db);
        let guide = engine.operation_recovery("original").unwrap();
        assert!(
            guide["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["operation"] == "plans.authorize")
        );
        assert!(
            guide["blockers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["code"] == "PLAN_AUTHORIZATION_INACTIVE")
        );
        let (engine, store, _, _) = fixture(gofin_domain::AuthorizationSource::TouchId, "ok");
        let mut db = store.lock().unwrap();
        let mut failed = crate::reservation_overrides::tests::seed_record("unsubmitted");
        failed["status"] = json!("failed");
        failed["error"] = to_value(&GoFinError::new(
            "EVIDENCE_TIMEOUT",
            "preparation",
            "Provider evidence timed out",
            "Wait before retry",
        ))
        .unwrap();
        db.create_request(
            "unsubmitted",
            "unsubmitted-request",
            "unsubmitted-fingerprint",
            "synthetic.transfer",
            None,
            "failed",
            &failed,
            &now(),
        )
        .unwrap();
        drop(db);
        let guide = engine.operation_recovery("unsubmitted").unwrap();
        assert!(
            guide["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["operation"] == "wait_and_refresh")
        );
        assert!(
            guide["actions"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["operation"] != "operations.resolve-preview")
        );
    }
}
