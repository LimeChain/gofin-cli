use super::*;
#[cfg(test)]
use gofin_domain::product_names::MetadataValueExt;
use gofin_domain::{AssetId, Quantity};

/// Build the native review from the approved durable boundary and usage, not a quote.
pub fn boundary_approval_review(
    prepared: &PreparedOperation,
    spec: &boundaries::BoundarySpec,
    prior: Option<&BoundaryAuthorization>,
    remaining_debits: &[ResourceAmount],
    remaining_costs: &[CostBound],
    duration: u64,
) -> Result<gofin_domain::approval_review::ApprovalReview> {
    use gofin_domain::approval_review::{
        ApprovalAccount, ApprovalAction, ApprovalLeg, ApprovalReview,
    };
    let source_maximum = if prepared.operation == "bridge.transfer" {
        serde_json::from_value::<gofin_bridge::BridgeBounds>(spec.parameters.clone())
            .map_err(|_| GoFinError::validation("Invalid bridge authorization bounds."))?
            .maximum_source_debit
    } else {
        None
    };
    let input = principal_debit(&prepared.operation, spec)?.map(|debit| ApprovalLeg {
        amount: Some(
            source_maximum
                .as_ref()
                .unwrap_or(&debit.amount.atoms)
                .to_decimal(debit.amount.asset.decimals),
        ),
        asset: Some(gofin_domain::approval_review::approval_asset_for(
            prepared,
            &debit.amount.asset,
        )),
        account: Some(ApprovalAccount {
            name: spec
                .fixed
                .get("wallet_name")
                .or_else(|| spec.fixed.get("account"))
                .or_else(|| spec.fixed.get("wallet"))
                .and_then(Value::as_str)
                .unwrap_or(&debit.resource.0)
                .strip_prefix(&format!("{}:", debit.amount.asset.network))
                .unwrap_or_else(|| {
                    spec.fixed
                        .get("wallet_name")
                        .or_else(|| spec.fixed.get("account"))
                        .or_else(|| spec.fixed.get("wallet"))
                        .and_then(Value::as_str)
                        .unwrap_or(&debit.resource.0)
                })
                .to_owned(),
            domain: Some(gofin_domain::approval_review::display_domain(
                &debit.amount.asset.network,
            )),
        }),
    });
    let mut review = ApprovalReview {
        version: 2,
        terms: vec![],
        advisories: vec![],
        action: ApprovalAction::AuthorizeBoundaries,
        service: prepared
            .operation
            .split('.')
            .next()
            .unwrap_or_default()
            .into(),
        market: Some(prepared.operation.clone()),
        input,
        output: None,
        other_details: None,
        description: None,
        step_count: None,
        valid_for_seconds: None,
    };
    let scope_summary = compact_boundary_scope(&prepared.operation, spec, Some(prepared))?;
    review.other_details = Some(if let Some(previous) = prior {
        format!(
            "{} Used: {}. Remaining: {}. Cumulative; {}s.",
            scope_summary,
            format_boundary_usage(&previous.spent_debits, &previous.spent_costs),
            format_boundary_usage(remaining_debits, remaining_costs),
            duration
        )
    } else {
        format!("{} {}s.", scope_summary, duration)
    });
    review.validate()?;
    Ok(review)
}

fn principal_debit<'a>(
    operation: &str,
    spec: &'a boundaries::BoundarySpec,
) -> Result<Option<&'a ResourceAmount>> {
    if operation != "bridge.transfer" {
        return Ok(spec.maximum_debits.first());
    }
    let asset = if !spec.fixed["source"].is_null() {
        serde_json::from_value::<gofin_bridge::EvmAsset>(spec.fixed["source"].clone())
            .map_err(|_| GoFinError::validation("Invalid bridge source asset."))?
            .asset_id()
    } else {
        serde_json::from_value::<gofin_bridge::SolanaAsset>(spec.fixed["solana_source"].clone())
            .map_err(|_| GoFinError::validation("Invalid bridge source asset."))?
            .asset_id()
    };
    Ok(spec
        .maximum_debits
        .iter()
        .find(|item| item.amount.asset == asset))
}

fn compact_boundary_scope(
    operation: &str,
    spec: &boundaries::BoundarySpec,
    prepared: Option<&PreparedOperation>,
) -> Result<String> {
    let asset_name = |asset: &AssetId| {
        prepared
            .map(|p| gofin_domain::approval_review::approval_asset_for(p, asset).name)
            .unwrap_or_else(|| gofin_domain::approval_review::display_asset(asset))
    };
    if operation == "jupiter.swap" {
        let param = &spec.parameters;
        let input: AssetId = serde_json::from_value(spec.fixed["input_asset"].clone())
            .map_err(|_| GoFinError::validation("Invalid bounded swap input asset."))?;
        let output: AssetId = serde_json::from_value(spec.fixed["output_asset"].clone())
            .map_err(|_| GoFinError::validation("Invalid bounded swap output asset."))?;
        let quantity = |key: &str, decimals| -> Result<String> {
            let value = param[key]
                .as_str()
                .ok_or_else(|| GoFinError::validation("Missing bounded swap rate."))?;
            Ok(Quantity::new(value)?.to_decimal(decimals))
        };
        let number = |key: &str| -> Result<u64> {
            param[key]
                .as_u64()
                .ok_or_else(|| GoFinError::validation("Missing bounded swap limit."))
        };
        return Ok(format!(
            "{} min rate {}/{} {}; slippage {}bps; platform {}bps; network fee {} SOL; funding {} SOL; quote {}s; meta aggregator.",
            asset_name(&output),
            quantity("minimum_output_per_input_numerator", output.decimals)?,
            quantity("minimum_output_per_input_denominator", input.decimals)?,
            asset_name(&input),
            number("maximum_slippage_bps")?,
            number("maximum_platform_fee_bps")?,
            Quantity::from_u64(number("maximum_network_fee_lamports")?).to_decimal(9),
            Quantity::from_u64(number("maximum_account_funding_lamports")?).to_decimal(9),
            number("maximum_quote_lifetime_seconds")?
        ));
    }
    if spec.fixed.get("token_in").is_some()
        && spec.fixed.get("token_out").is_some()
        && spec.parameters.get("minimum_rate").is_some()
    {
        let input: AssetId = serde_json::from_value(spec.fixed["token_in"].clone())
            .map_err(|_| GoFinError::validation("Invalid AMM input asset."))?;
        let output: AssetId = serde_json::from_value(spec.fixed["token_out"].clone())
            .map_err(|_| GoFinError::validation("Invalid AMM output asset."))?;
        let p = &spec.parameters;
        let rate = |key: &str, decimals| -> Result<String> {
            Ok(Quantity::new(
                p["minimum_rate"][key]
                    .as_str()
                    .ok_or_else(|| GoFinError::validation("Missing AMM rate."))?,
            )?
            .to_decimal(decimals))
        };
        let value = |key: &str| -> Result<String> {
            let v = &p[key];
            if let Some(s) = v.as_str() {
                Ok(s.to_owned())
            } else if v.is_number() {
                Ok(v.to_string())
            } else {
                Err(GoFinError::validation("Missing AMM bound."))
            }
        };
        let mut route = if operation == "swap.execute" {
            format!(
                "DEX {}",
                spec.fixed["allowed_dexes"]
                    .as_array()
                    .ok_or_else(|| GoFinError::validation("Missing approved DEXes."))?
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("/")
            )
        } else {
            format!(
                "{} {}",
                spec.fixed["dex"].as_str().unwrap_or("uniswap"),
                spec.fixed["protocol"].as_str().unwrap_or("v3")
            )
        };
        if let Some(tiers) = p["allowed_fee_tiers"]
            .as_array()
            .filter(|tiers| !tiers.is_empty())
        {
            route.push_str(&format!(
                " tiers {}ppm",
                tiers
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("/")
            ));
        }
        if let Some(fee) = p["max_pool_fee_ppm"].as_u64() {
            route.push_str(&format!(" pool fee ≤{fee}ppm"));
        }
        if let Some(spacing) = spec.fixed["tick_spacing"].as_i64() {
            route.push_str(&format!(" spacing {spacing}"));
        }
        if let Some(key) = spec.fixed.get("v4_pool_key").filter(|v| v.is_object()) {
            route.push_str(&format!(" pool {}", key));
        }
        let permission = if input.reference == "native" {
            "native input"
        } else if spec.fixed["protocol"] == "v4" {
            "finite Permit2"
        } else {
            "finite allowance/permit"
        };
        let absolute = p["absolute_minimum_output"]
            .as_str()
            .map(|v| format!("; min {v}"))
            .unwrap_or_default();
        return Ok(format!(
            "{}: {} min rate {}/{}; fee ≤{} ETH; slip {}bps; impact {}bps; deadline {}s; {}{}.",
            route,
            asset_name(&output),
            rate("output_atoms", output.decimals)?,
            rate("input_atoms", input.decimals)?,
            value("max_fee_eth")?,
            value("max_slippage_bps")?,
            value("max_price_impact_bps")?,
            value("max_deadline_seconds")?,
            permission,
            absolute
        ));
    }
    if operation == "bridge.transfer" {
        let bounds: gofin_bridge::BridgeBounds = serde_json::from_value(spec.parameters.clone())
            .map_err(|_| GoFinError::validation("Invalid bridge authorization bounds."))?;
        let source = principal_debit(operation, spec)?
            .ok_or_else(|| GoFinError::validation("Bridge source bound is missing."))?;
        let mut parts = Vec::new();
        if let Some(input) = bounds.input {
            parts.push(format!(
                "in {}–{}",
                input.minimum.to_decimal(source.amount.asset.decimals),
                input.maximum.to_decimal(source.amount.asset.decimals)
            ));
        }
        if let Some(receipt) = bounds.minimum_net_receipt {
            parts.push(format!(
                "≥{} {} {}",
                receipt.atoms.to_decimal(receipt.asset.decimals),
                asset_name(&receipt.asset),
                gofin_domain::approval_review::display_domain(&receipt.asset.network)
            ));
        }
        if let Some(rate) = bounds.minimum_output_input_ratio {
            parts.push(format!(
                "min ratio {}/{} atoms",
                rate.numerator, rate.denominator
            ));
        }
        for fee in bounds.fee_caps {
            parts.push(format!(
                "{:?} ≤{} {}",
                fee.kind,
                fee.maximum.to_decimal(fee.asset.decimals),
                asset_name(&fee.asset)
            ));
            if let Some(usd) = fee.maximum_usd_micros {
                parts.push(format!("fee ≤${}", Quantity::from_u64(usd).to_decimal(6)));
            }
        }
        if let Some(input) = bounds.input_usd_micros {
            parts.push(format!(
                "in ${}–${}",
                input.minimum.to_decimal(6),
                input.maximum.to_decimal(6)
            ));
        }
        if let Some(price) = bounds.source_usd_per_unit_micros {
            parts.push(format!(
                "px ${}–${}",
                price.minimum.to_decimal(6),
                price.maximum.to_decimal(6)
            ));
        }
        if let Some(total) = bounds.maximum_total_usd_micros {
            parts.push(format!(
                "total ≤${}",
                Quantity::from_u64(total).to_decimal(6)
            ));
        }
        if let Some(deadline) = bounds.latest_protocol_deadline {
            parts.push(format!("deadline {deadline}"));
        }
        if let Some(steps) = spec.fixed["steps"].as_array() {
            let names = steps
                .iter()
                .filter_map(|v| v["step_kind"].as_str())
                .collect::<Vec<_>>();
            if names.len() != steps.len() {
                return Err(GoFinError::validation(
                    "Bridge step scope cannot be reviewed.",
                ));
            }
            parts.push(names.join("/"));
        }
        return Ok(parts.join("; ") + ".");
    }
    if operation.starts_with("aave.") {
        return Ok(spec.summary.replace(" Market, permissions and risk bounds remain fixed; later balances are checked only at dispatch.", ""));
    }
    Ok(spec.summary.clone())
}

impl Engine {
    /// Build the terminal review material for a boundary grant. This is a
    /// read-only preview: it creates neither durable authority nor a challenge.
    pub async fn preview_boundary_authorization(
        &self,
        _request: BoundaryAuthorizationRequest,
    ) -> Result<Value> {
        Err(retired_execution_interface())
    }

    /// Obtain fresh human approval for an adapter-validated boundary grant and
    /// persist it as protected broker state.
    pub async fn authorize_boundary(
        &self,
        _request: BoundaryAuthorizationRequest,
    ) -> Result<BoundaryAuthorization> {
        Err(retired_execution_interface())
    }

    /// Inspect original attempt bindings without reconciliation or new authority.
    pub fn boundary_rpc_read_scope(&self, id: &str) -> Result<bool> {
        let record = self.load_boundary_record(id)?;
        let operation_ids = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .boundary_intent_attempts(&record.authorization.intent_id)
            .map_err(map_store)?;
        for operation_id in operation_ids {
            if self.status_rpc_read_scope(&operation_id)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub async fn boundary_status(&self, id: &str) -> Result<BoundaryStatus> {
        let record = self.load_boundary_record(id)?;
        let ids = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .boundary_intent_attempts(&record.authorization.intent_id)
            .map_err(map_store)?;
        let mut attempts = Vec::new();
        for id in ids {
            let operation = self.load_record(&id)?;
            attempts.push(BoundaryAttemptSummary {
                id: operation.id,
                request_id: operation.request_id,
                operation: operation.operation,
                input: operation.input,
                status: operation.status,
                authorization_source: operation.approval.as_ref().and_then(|a| a.source.clone()),
                yolo_provenance: operation
                    .approval
                    .as_ref()
                    .and_then(|a| a.yolo_provenance.clone()),
                submission: operation.submission,
                effects: operation.effects,
                error: operation.error,
            });
        }
        Ok(BoundaryStatus {
            authorization: record.authorization,
            attempts,
        })
    }

    /// Revocation is a durable broker decision and never asks for approval.
    pub async fn revoke_boundary(&self, id: &str) -> Result<BoundaryStatus> {
        let mut record = self.load_boundary_record(id)?;
        if record.authorization.status == BoundaryAuthorizationStatus::Active {
            let before = record.clone();
            record.authorization.status = BoundaryAuthorizationStatus::Revoked;
            self.save_boundary_record(&before, &record)?;
        }
        let operation_ids = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .boundary_attempts(id)
            .map_err(map_store)?;
        let mut attempts = Vec::new();
        for operation_id in operation_ids {
            let operation = self.load_record(&operation_id)?;
            attempts.push(BoundaryAttemptSummary {
                id: operation.id,
                request_id: operation.request_id,
                operation: operation.operation,
                input: operation.input,
                status: operation.status,
                authorization_source: operation
                    .approval
                    .as_ref()
                    .and_then(|audit| audit.source.clone()),
                yolo_provenance: operation
                    .approval
                    .as_ref()
                    .and_then(|audit| audit.yolo_provenance.clone()),
                submission: operation.submission,
                effects: operation.effects,
                error: operation.error,
            });
        }
        Ok(BoundaryStatus {
            authorization: record.authorization,
            attempts,
        })
    }

    pub async fn preview_boundary(
        &self,
        _id: &str,
        _operation: &str,
        _input: &Value,
    ) -> Result<BoundaryPreview> {
        Err(retired_execution_interface())
    }

    /// Execute one concrete request under an existing authorization. The
    /// request identity and attempt link are committed before adapter dispatch.
    pub async fn execute_boundary(
        &self,
        _id: &str,
        _request: OperationRequest,
    ) -> Result<OperationRecord> {
        Err(retired_execution_interface())
    }

    /// Replace a specifically identified unsubmitted candidate. The durable
    /// cancellation wins atomically against submission dispatch before this
    /// method waits for the engine's local write queue.
    pub async fn execute_boundary_replacing(
        &self,
        _id: &str,
        _request: OperationRequest,
        _previous_attempt_id: &str,
    ) -> Result<OperationRecord> {
        Err(retired_execution_interface())
    }

    #[cfg(test)]
    pub(super) fn bridge_boundary_prospective_debit(
        &self,
        bridge_scope: Option<&BridgeBoundaryScope>,
        prepared: &PreparedOperation,
        spec: &BoundarySpec,
    ) -> Result<Option<gofin_domain::BridgeProspectiveDebit>> {
        let missing = spec
            .maximum_debits
            .iter()
            .filter(|limit| {
                !prepared.intent.debits.iter().any(|debit| {
                    debit.resource == limit.resource && debit.amount.asset == limit.amount.asset
                })
            })
            .collect::<Vec<_>>();
        let Some((transfer_id, step_id, step_kind, _, _)) = bridge_scope else {
            return Ok(None);
        };
        if !missing.is_empty()
            && (!matches!(
                step_kind,
                gofin_domain::BridgeStepKind::Approval
                    | gofin_domain::BridgeStepKind::ResetAllowance
            ) || missing.len() != 1)
        {
            return Ok(None);
        }
        let marker_value = prepared
            .binding
            .product_get(gofin_domain::BRIDGE_STEP_METADATA_KEY)?
            .and_then(|marker| marker.get("prospective_debit"));
        let Some(marker_value) = marker_value else {
            return if missing.is_empty() {
                Ok(None)
            } else {
                Err(boundary_error(
                    "An allowance boundary with future principal must carry its typed declared-deposit scope.",
                ))
            };
        };
        let marker: gofin_domain::BridgeProspectiveDebit =
            serde_json::from_value(marker_value.clone())
                .map_err(|_| boundary_error("The bridge prospective debit scope is malformed."))?;
        if marker.deposit_step_id.trim().is_empty()
            || marker.maximum.atoms.is_zero()
            || !prepared.intent.locks.contains(&marker.resource)
        {
            return Err(boundary_error(
                "The prospective source debit does not identify the locked wallet and a positive finite amount.",
            ));
        }
        let store = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?;
        let transfer = store
            .bridge_transfer(transfer_id)
            .map_err(map_store)?
            .ok_or_else(|| boundary_error("The bridge transfer is not in the durable plan."))?;
        if transfer.prospective_debit.as_ref() != Some(&marker) {
            return Err(boundary_error(
                "The prepared prospective debit differs from the durable bridge plan.",
            ));
        }
        let current = transfer
            .steps
            .iter()
            .find(|step| step.step_id == *step_id)
            .ok_or_else(|| boundary_error("The current bridge step is not declared."))?;
        let deposit = transfer
            .steps
            .iter()
            .find(|step| step.step_id == marker.deposit_step_id)
            .ok_or_else(|| boundary_error("The prospective Deposit step is not declared."))?;
        let kind_matches_deposit = match step_kind {
            gofin_domain::BridgeStepKind::Approval
            | gofin_domain::BridgeStepKind::ResetAllowance => {
                deposit.step_id != *step_id
                    && bridge_step_is_downstream(&transfer.steps, &deposit.step_id, step_id)
            }
            gofin_domain::BridgeStepKind::Deposit => deposit.step_id == *step_id,
            gofin_domain::BridgeStepKind::Prove
            | gofin_domain::BridgeStepKind::Finalize
            | gofin_domain::BridgeStepKind::Mint => false,
        };
        if current.kind != *step_kind
            || current.operation_id.is_some()
            || current.state != gofin_domain::BridgeStepState::Unsubmitted
            || deposit.kind != gofin_domain::BridgeStepKind::Deposit
            || deposit.operation_id.is_some()
            || deposit.state != gofin_domain::BridgeStepState::Unsubmitted
            || !kind_matches_deposit
        {
            return Err(boundary_error(
                "The prospective source debit is not tied to a ready allowance step and its declared downstream Deposit.",
            ));
        }
        let expected = gofin_domain::ResourceAmount {
            resource: marker.resource.clone(),
            amount: marker.maximum.clone(),
        };
        if !missing.is_empty()
            && (missing[0].resource != expected.resource
                || missing[0].amount.asset != expected.amount.asset
                || missing[0].amount.atoms != expected.amount.atoms)
        {
            return Err(boundary_error(
                "The authorization's future principal cap differs from the exact durable Deposit scope.",
            ));
        }
        Ok(Some(marker))
    }
}

#[cfg(test)]
pub(super) type BridgeBoundaryScope = (
    String,
    String,
    gofin_domain::BridgeStepKind,
    Value,
    gofin_domain::BridgeSettlementExpectation,
);

#[cfg(test)]
fn bridge_step_is_downstream(
    steps: &[gofin_domain::BridgeStepLink],
    dependent_step_id: &str,
    ancestor_step_id: &str,
) -> bool {
    if dependent_step_id == ancestor_step_id {
        return true;
    }
    let mut pending = steps
        .iter()
        .find(|step| step.step_id == dependent_step_id)
        .map(|step| step.depends_on.clone())
        .unwrap_or_default();
    let mut visited = std::collections::BTreeSet::new();
    while let Some(step_id) = pending.pop() {
        if step_id == ancestor_step_id {
            return true;
        }
        if visited.insert(step_id.clone())
            && let Some(step) = steps.iter().find(|step| step.step_id == step_id)
        {
            pending.extend(step.depends_on.iter().cloned());
        }
    }
    false
}

#[cfg(test)]
pub(super) fn bridge_boundary_scope(
    prepared: &PreparedOperation,
    input: &Value,
) -> Result<Option<BridgeBoundaryScope>> {
    let Some(marker) = prepared
        .binding
        .product_get(gofin_domain::BRIDGE_STEP_METADATA_KEY)?
    else {
        return Ok(None);
    };
    let text = |field: &str| {
        marker
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| boundary_error("Finite bridge grant marker is incomplete."))
    };
    let transfer_id = text("transfer_id")?;
    let step_id = text("step_id")?;
    let step_kind: gofin_domain::BridgeStepKind = serde_json::from_value(
        marker
            .get("kind")
            .cloned()
            .ok_or_else(|| boundary_error("Finite bridge grant marker lacks its step kind."))?,
    )
    .map_err(|_| boundary_error("Finite bridge grant marker has an unknown step kind."))?;
    if super::bridge_input_field(input, "transfer_id", "transferId")?.and_then(Value::as_str)
        != Some(transfer_id.as_str())
        || super::bridge_input_field(input, "step_id", "stepId")?.and_then(Value::as_str)
            != Some(step_id.as_str())
        || super::bridge_input_field(input, "step_kind", "stepKind")?
            .cloned()
            .and_then(|value| serde_json::from_value::<gofin_domain::BridgeStepKind>(value).ok())
            != Some(step_kind.clone())
    {
        return Err(boundary_error(
            "Finite bridge grant step differs from its typed public request.",
        ));
    }
    let bounds = marker
        .get("bounds")
        .cloned()
        .ok_or_else(|| boundary_error("Finite bridge grant marker lacks selected bounds."))?;
    let expectation = serde_json::from_value(
        marker
            .get("settlement_expectation")
            .cloned()
            .ok_or_else(|| boundary_error("Finite bridge grant marker lacks settlement scope."))?,
    )
    .map_err(|_| boundary_error("Finite bridge grant settlement scope is malformed."))?;
    Ok(Some((transfer_id, step_id, step_kind, bounds, expectation)))
}

pub(super) fn bridge_step_claim(
    request: &OperationRequest,
    grant_id: Option<&str>,
    operation_id: &str,
    engine: &Engine,
) -> Result<Option<gofin_domain::BridgeStepLink>> {
    let input = &request.input;
    let transfer_id_value = super::bridge_input_field(input, "transfer_id", "transferId")?;
    let step_id_value = super::bridge_input_field(input, "step_id", "stepId")?;
    let step_kind_value = super::bridge_input_field(input, "step_kind", "stepKind")?;
    let present = [transfer_id_value, step_id_value, step_kind_value]
        .iter()
        .filter(|value| value.is_some())
        .count();
    if present == 0 {
        // Preserve legacy exact operation grants. Finite bridge transfers use
        // the complete typed triplet and take the durable claim path below.
        return Ok(None);
    }
    if present != 3 {
        return Err(boundary_error(
            "A finite bridge-step attempt requires transfer_id, step_id and step_kind together.",
        ));
    }
    let transfer_id = transfer_id_value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| boundary_error("The bridge transfer ID must be a nonempty string."))?;
    let step_id = step_id_value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| boundary_error("The bridge step ID must be a nonempty string."))?;
    let kind: gofin_domain::BridgeStepKind =
        serde_json::from_value(step_kind_value.cloned().unwrap_or(Value::Null)).map_err(|_| {
            boundary_error("The bridge step kind is not one of the declared finite kinds.")
        })?;
    let store = engine
        .store
        .lock()
        .map_err(|_| internal("store lock poisoned"))?;
    let transfer = store
        .bridge_transfer(transfer_id)
        .map_err(map_store)?
        .ok_or_else(|| boundary_error("The bridge transfer is not present in the durable plan."))?;
    if transfer.grant_id.as_deref() != grant_id || transfer.plan_scope.is_some() {
        return Err(boundary_error(
            "The bridge transfer is not bound to this finite authorization.",
        ));
    }
    let planned = transfer
        .steps
        .iter()
        .find(|step| step.step_id == step_id)
        .ok_or_else(|| boundary_error("The bridge step is not declared in this transfer."))?;
    if planned.kind != kind
        || planned.grant_id.as_deref() != grant_id
        || planned.operation_id.is_some()
        || planned.state != gofin_domain::BridgeStepState::Unsubmitted
        || planned.depends_on.iter().any(|dependency| {
            !transfer.steps.iter().any(|step| {
                step.step_id == *dependency && step.state == gofin_domain::BridgeStepState::Settled
            })
        })
    {
        return Err(boundary_error(
            "The bridge step is outside the approved plan, already claimed, or has unsettled dependencies.",
        ));
    }
    let mut claimed = planned.clone();
    claimed.operation_id = Some(operation_id.to_owned());
    claimed.state = gofin_domain::BridgeStepState::Submitted;
    Ok(Some(claimed))
}

#[cfg(test)]
mod approval_review_tests {
    use super::*;
    #[test]
    fn boundary_review_uses_authorized_maximum_not_quote() {
        let asset = AssetId {
            network: "solana".into(),
            reference: "USDC".into(),
            decimals: 6,
        };
        let debit = ResourceAmount {
            resource: ResourceId("wallet:alice".into()),
            amount: Amount {
                asset: asset.clone(),
                atoms: Quantity::new("5000000").unwrap(),
            },
        };
        let prepared = PreparedOperation {
            operation: "binance.spot.order-place".into(),
            operation_version: 1,
            input: json!({}),
            binding: json!({}),
            summary: String::new(),
            checks: vec![],
            warnings: vec![],
            observed_at: String::new(),
            expires_at: String::new(),
            valuation: None,
            intent: FinancialIntent::default(),
            action_deadline: None,
        };
        let spec = boundaries::BoundarySpec {
            fixed: json!({"account":"desk"}),
            parameters: json!({}),
            maximum_debits: vec![debit],
            maximum_costs: vec![],
            summary: "limit 1; fees 0.1 USDC".into(),
        };
        let review = boundary_approval_review(&prepared, &spec, None, &[], &[], 600).unwrap();
        assert_eq!(
            review.action,
            gofin_domain::approval_review::ApprovalAction::AuthorizeBoundaries
        );
        let leg = review.input.unwrap();
        assert_eq!(leg.amount.as_deref(), Some("5"));
        assert_eq!(leg.asset.as_ref().map(|a| a.name.as_str()), Some("USDC"));
        assert_eq!(leg.account.unwrap().name, "desk");
        assert!(review.other_details.unwrap().contains("600s"));
        let mut named = prepared;
        named.binding["approval_asset_metadata"] =
            json!([{"name":"Treasury USD", "identifier":asset}]);
        let named_review = boundary_approval_review(&named, &spec, None, &[], &[], 600).unwrap();
        let named_asset = named_review.input.unwrap().asset.unwrap();
        assert_eq!(named_asset.name, "Treasury USD");
        assert_eq!(named_asset.identifier, asset);
        named.binding["approval_asset_metadata"][0]["identifier"]["decimals"] = json!(9);
        let fallback = boundary_approval_review(&named, &spec, None, &[], &[], 600).unwrap();
        assert_eq!(fallback.input.unwrap().asset.unwrap().name, "USDC");
    }
    #[test]
    fn amm_boundary_includes_rate_route_and_risk_limits() {
        let spec = boundaries::BoundarySpec {
            fixed: json!({"token_in":{"network":"ethereum:1","reference":"native","decimals":18},"token_out":{"network":"ethereum:1","reference":"USDC","decimals":6},"protocol":"v3","dex":"uniswap"}),
            parameters: json!({"minimum_rate":{"output_atoms":"2970000","input_atoms":"1000000000000000"},"max_fee_eth":"0.001","max_slippage_bps":100,"max_price_impact_bps":40,"absolute_minimum_output":"2.97","allowed_fee_tiers":[500],"max_deadline_seconds":600}),
            maximum_debits: vec![],
            maximum_costs: vec![],
            summary: "verbose".into(),
        };
        let result = compact_boundary_scope("uniswap.swap", &spec, None).unwrap();
        for text in [
            "uniswap v3 tiers 500ppm",
            "USDC min rate 2.97/0.001",
            "fee ≤0.001 ETH",
            "slip 100bps",
            "impact 40bps",
            "deadline 600s",
            "native input",
            "min 2.97",
        ] {
            assert!(result.contains(text), "{result}");
        }
        assert!(result.len() < 200);
    }
    #[test]
    fn jupiter_boundary_formats_exact_rates_and_distinct_cost_caps() {
        let spec = boundaries::BoundarySpec {
            fixed: json!({"input_asset":{"network":"solana","reference":"USDC","decimals":6},"output_asset":{"network":"solana","reference":"SOL","decimals":9}}),
            parameters: json!({"minimum_output_per_input_numerator":"1000000000","minimum_output_per_input_denominator":"100000000","maximum_slippage_bps":50,"maximum_platform_fee_bps":5,"maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":2000000,"maximum_quote_lifetime_seconds":60}),
            maximum_debits: vec![],
            maximum_costs: vec![],
            summary: "verbose".into(),
        };
        let result = compact_boundary_scope("jupiter.swap", &spec, None).unwrap();
        assert!(result.contains("SOL min rate 1/100 USDC"));
        assert!(result.contains("network fee 0.00001 SOL"));
        assert!(result.contains("funding 0.002 SOL"));
        assert!(result.contains("slippage 50bps; platform 5bps"));
        assert!(result.len() < 200);
    }
}
