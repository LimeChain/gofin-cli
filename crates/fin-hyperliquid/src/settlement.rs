//! Cumulative accounting from immutable order and fill receipts.
use crate::{
    fills::{FillSide, aggregate_fills},
    profile::*,
    read_profile::native_error,
    types::*,
    writes::*,
};
use fin_domain::*;
use fin_exchange::ccxt::{
    HyperliquidAction as Action, HyperliquidActionEnvelope as Envelope,
    HyperliquidInfoRequest as Info,
};
use num_bigint::BigInt;
use num_traits::Zero;
use serde_json::{Value, json};
use std::collections::BTreeMap;

struct DerivativePositionState {
    resource: ResourceId,
    symbol: String,
    base_asset: AssetId,
    initial: BigInt,
    current: BigInt,
    orders: Vec<Value>,
}

pub(crate) fn unresolved(status: Status, reason: &str, evidence: Value) -> Reconciliation {
    Reconciliation {
        status,
        evidence: json!({"reason":reason,"observations":evidence}),
        verified: None,
    }
}

/// Merge durable previously observed receipts with the latest bounded history
/// response. Exact repeats are collapsed; a changed receipt under the same
/// native event identity is never silently replaced.
fn merge_fill_history(previous: &[Value], current: &Value) -> Result<Option<Vec<Value>>> {
    let current = current
        .as_array()
        .ok_or_else(|| native_error("Fill history is not an array."))?;
    let mut rows = BTreeMap::<String, Value>::new();
    for row in previous.iter().chain(current) {
        let object = row
            .as_object()
            .ok_or_else(|| native_error("A native fill receipt is not an object."))?;
        let oid = object
            .get("oid")
            .and_then(Value::as_u64)
            .ok_or_else(|| native_error("A native fill receipt has no unsigned order ID."))?;
        let tid = object
            .get("tid")
            .and_then(Value::as_u64)
            .ok_or_else(|| native_error("A native fill receipt has no unsigned trade ID."))?;
        let hash = object
            .get("hash")
            .and_then(Value::as_str)
            .ok_or_else(|| native_error("A native fill receipt has no transaction hash."))?;
        let identity = format!("{oid}:{tid}:{}", hash.to_ascii_lowercase());
        if let Some(prior) = rows.get(&identity) {
            if prior != row {
                return Ok(None);
            }
        } else {
            rows.insert(identity, row.clone());
        }
    }
    Ok(Some(rows.into_values().collect()))
}

fn exact_order_statuses<'a>(
    record: &'a OperationRecord,
    submission: &Submission,
    expected_count: usize,
) -> Option<&'a Vec<Value>> {
    let dispatch = record.dispatch_evidence.as_ref()?;
    if dispatch["submission_reference"].as_str() != Some(submission.reference.as_str())
        || dispatch["response"]["status"].as_str() != Some("ok")
        || dispatch["response"]["response"]["type"].as_str() != Some("order")
    {
        return None;
    }
    let statuses = dispatch["response"]["response"]["data"]["statuses"].as_array()?;
    (statuses.len() == expected_count).then_some(statuses)
}

fn exact_order_rejection(status: &Value) -> Option<&str> {
    let error = status.get("error")?.as_str()?;
    (!error.trim().is_empty() && status.get("resting").is_none() && status.get("filled").is_none())
        .then_some(error)
}

fn proven_pre_dispatch_failure(
    record: &OperationRecord,
    submission: &Submission,
    account: &HyperliquidAccount,
    envelope: &Envelope,
    wire: &Value,
) -> Option<&'static str> {
    if record.effects.is_some() {
        return None;
    }
    let failure = record.error.as_ref()?;
    if failure.code == "HYPERLIQUID_PREDISPATCH_CREDENTIAL_FAILURE"
        && failure.stage == "pre-dispatch"
    {
        let evidence = record.dispatch_evidence.as_ref()?;
        let envelope_value = serde_json::to_value(envelope).ok()?;
        let exact = evidence["kind"] == "hyperliquid_pre_dispatch_failure"
            && evidence["version"] == 1
            && evidence["phase"] == "pre_dispatch"
            && evidence["outcome"] == "not_submitted"
            && evidence["reason"] == "protected_credential_lookup_failed"
            && evidence["submission_reference"].as_str() == Some(submission.reference.as_str())
            && evidence["owner"].as_str() == Some(account.owner.as_str())
            && evidence["execution_address"].as_str() == Some(account.execution_address())
            && evidence["envelope"] == envelope_value
            && &evidence["wire"] == wire
            && evidence["error_code"].as_str() == Some(failure.code.as_str())
            && evidence.get("response").is_none();
        return exact.then_some("typed_broker_pre_dispatch_credential_failure");
    }

    // One deployed broker version returned this exact default-trait error
    // before entering the CCXT action client. Keep that narrow record
    // recoverable; never generalize credential failures or errors that may
    // have followed a network dispatch.
    (record.dispatch_evidence.is_none()
        && failure.code == "CREDENTIAL_TYPE_MISMATCH"
        && failure.stage == "credentials"
        && failure.message
            == "This credential provider cannot access a protected Hyperliquid signer."
        && failure.next_action == "Use the protected installed credential provider."
        && failure.details.is_none())
    .then_some("legacy_default_provider_credential_failure")
}

fn proven_explicit_action_rejection(
    record: &OperationRecord,
    submission: &Submission,
) -> Option<&'static str> {
    if record.effects.is_some()
        || !record.error.as_ref().is_some_and(|failure| {
            failure.code == "HYPERLIQUID_ACTION_REJECTED" && failure.stage == "submission"
        })
    {
        return None;
    }
    let dispatch = record.dispatch_evidence.as_ref()?;
    if dispatch["source"].as_str() != Some("Hyperliquid /exchange")
        || dispatch["submission_reference"].as_str() != Some(submission.reference.as_str())
    {
        return None;
    }
    let response = dispatch.get("response")?.as_object()?;
    let exact_error = response.len() == 2
        && response.get("status").and_then(Value::as_str) == Some("err")
        && response
            .get("response")
            .and_then(Value::as_str)
            .is_some_and(|message| !message.trim().is_empty());
    exact_error.then_some("exact_top_level_hyperliquid_rejection")
}

fn exact_noop_ack(dispatch: &Value, submission: &Submission) -> Option<Value> {
    let ack = dispatch.get("response")?;
    if dispatch["source"].as_str() != Some("Hyperliquid /exchange")
        || dispatch["submission_reference"].as_str() != Some(submission.reference.as_str())
        || !ack.as_object().is_some_and(|object| {
            object.len() == 2
                && object.get("status").and_then(Value::as_str) == Some("ok")
                && object
                    .get("response")
                    .and_then(Value::as_object)
                    .is_some_and(|response| {
                        response.len() == 1
                            && response.get("type").and_then(Value::as_str) == Some("default")
                    })
        })
    {
        return None;
    }
    Some(ack.clone())
}

fn completed_noop_child_for_parent(
    child: &OperationRecord,
    parent: &OperationRecord,
    account: &HyperliquidAccount,
    parent_submission: &Submission,
) -> Result<Option<Value>> {
    if child.operation != "hyperliquid.account.invalidate_nonce"
        || !matches!(child.status, Status::Completed)
    {
        return Ok(None);
    }
    let Some(child_submission) = child.submission.as_ref() else {
        return Ok(None);
    };
    let Some(child_effects) = child.effects.as_ref() else {
        return Ok(None);
    };
    if child_submission.kind != SUBMISSION_KIND
        || !child_effects.effects.is_empty()
        || child_effects.submission != *child_submission
        || verified_effect_digest(child_effects).ok().as_deref()
            != Some(child_effects.proof_digest.as_str())
    {
        return Ok(None);
    }
    let Some(prepared) = child.preview.as_ref() else {
        return Ok(None);
    };
    let binding: ActionBinding = match ActionBinding::decode(&prepared.binding) {
        Ok(binding) => binding,
        Err(_) => return Ok(None),
    };
    let request: InvalidateNonceInput = match serde_json::from_value(child.input.clone()) {
        Ok(input) => input,
        Err(_) => return Ok(None),
    };
    let Some(recovery) = binding.recovery.as_ref() else {
        return Ok(None);
    };
    let Some(original_submission) = parent.submission.as_ref() else {
        return Ok(None);
    };
    if request.account != account.name
        || request.original_operation_id != parent.id
        || binding.account != *account
        || recovery.version != 1
        || recovery.parent_operation_id != parent.id
        || recovery.parent_submission != *original_submission
        || original_submission != parent_submission
        || !matches!(binding.action, Action::Noop {})
    {
        return Ok(None);
    }
    let original = match crate::writes::original_class_transfer(account, parent) {
        Ok(envelope) => envelope,
        Err(_) => return Ok(None),
    };
    let child_envelope: Envelope =
        match serde_json::from_value(child_submission.data["envelope"].clone()) {
            Ok(envelope) => envelope,
            Err(_) => return Ok(None),
        };
    if child_envelope.nonce_ms != original.nonce_ms
        || child_envelope.vault_address.is_some()
        || !matches!(child_envelope.action, Action::Noop {})
    {
        return Ok(None);
    }
    let Some(ack) = child
        .dispatch_evidence
        .as_ref()
        .and_then(|dispatch| exact_noop_ack(dispatch, child_submission))
    else {
        return Ok(None);
    };
    let proof = &child_effects.evidence;
    if proof["method"].as_str() != Some("exact_native_nonce_noop_ack")
        || proof["parent_operation_id"].as_str() != Some(parent.id.as_str())
        || proof["parent_submission"]
            != serde_json::to_value(parent_submission).unwrap_or(Value::Null)
        || proof["ack"] != ack
        || proof["native_nonce_ms"].as_u64() != Some(original.nonce_ms)
    {
        return Ok(None);
    }
    Ok(Some(json!({
        "method":"linked_exact_native_nonce_noop_ack",
        "child_operation_id":child.id,
        "child_submission":child_submission,
        "parent_operation_id":parent.id,
        "parent_submission":parent_submission,
        "native_nonce_ms":original.nonce_ms,
        "ack":ack
    })))
}

impl HyperliquidProfile {
    pub async fn reconcile_action(
        &self,
        record: &OperationRecord,
        submission: &Submission,
    ) -> Result<Reconciliation> {
        let prepared = record
            .preview
            .as_ref()
            .ok_or_else(|| native_error("Original prepared action is missing."))?;
        let binding = ActionBinding::decode(&prepared.binding)?;
        let account = self.account(&binding.account.name)?;
        if account != &binding.account {
            return Ok(unresolved(
                Status::Unknown,
                "Protected account mapping changed; no action was replayed.",
                json!({}),
            ));
        }
        let envelope: Envelope = serde_json::from_value(submission.data["envelope"].clone())
            .map_err(|_| native_error("Original nonce/action envelope is missing."))?;
        let wire = envelope
            .action_wire_json()
            .map_err(|_| native_error("Original action cannot be reconstructed."))?;
        let scoped_identity = submission.data.get("execution_address").is_some();
        if envelope.vault_address.as_deref() != account.vault_address()
            || (scoped_identity
                && submission.data["execution_address"].as_str()
                    != Some(account.execution_address()))
            || (!scoped_identity && account.vault_address().is_some())
        {
            return Err(native_error(
                "Original submission target differs from the protected execution scope.",
            ));
        }
        let expected = if scoped_identity {
            binding_hash(
                &json!({"version":1,"owner":account.owner,"execution_address":account.execution_address(),"vault_address":account.vault_address(),"operation":record.operation,"wire":wire}),
            )?
        } else {
            // Owner submissions persisted before scoped trading retain their
            // original immutable identity; migration never creates a new action.
            binding_hash(
                &json!({"version":1,"owner":account.owner,"operation":record.operation,"wire":wire}),
            )?
        };
        if submission.reference != expected
            || submission.data["wire"] != wire
            || submission.data["owner"] != account.owner
        {
            return Err(native_error(
                "Submission identity is not the exact persisted owner/action/nonce.",
            ));
        }
        if let Some(reason) =
            proven_pre_dispatch_failure(record, submission, account, &envelope, &wire)
        {
            return self.verified_result(
                record,
                submission,
                Status::Failed,
                Vec::new(),
                json!({
                    "method":"protected-hyperliquid-credential-failure-before-dispatch",
                    "outcome":"not_submitted",
                    "proof":reason,
                    "submission_reference":submission.reference,
                    "owner":account.owner,
                    "execution_address":account.execution_address(),
                    "wire":wire,
                    "pre_dispatch_evidence":record.dispatch_evidence,
                }),
                BTreeMap::new(),
            );
        }
        if let Some(reason) = proven_explicit_action_rejection(record, submission) {
            return self.verified_result(
                record,
                submission,
                Status::Failed,
                Vec::new(),
                json!({
                    "method":"exact-hyperliquid-action-rejection",
                    "outcome":"not_accepted",
                    "proof":reason,
                    "submission_reference":submission.reference,
                    "owner":account.owner,
                    "execution_address":account.execution_address(),
                    "wire":wire,
                    "dispatch":record.dispatch_evidence,
                }),
                BTreeMap::new(),
            );
        }
        let mode_transition = matches!(
            record.operation.as_str(),
            "hyperliquid.account.set_abstraction"
                | "hyperliquid.account.set_dex_abstraction"
                | "hyperliquid.account.set_agent_abstraction"
        );
        if !mode_transition {
            let current_mode = self.verified_account_mode(account).await?;
            let mode_matches = match binding.account_mode.as_ref() {
                Some(frozen_mode) => &current_mode == frozen_mode,
                // Older pending writes did not persist an account-mode read.
                // Their legacy resource mapping is safe only under Separate.
                None => {
                    current_mode.mode == crate::account_mode::AccountMode::Separate
                        && current_mode.dex_abstraction != Some(true)
                }
            };
            if !mode_matches {
                return Ok(unresolved(
                    Status::Unknown,
                    "Hyperliquid account mode or signer role changed after the write was prepared; effects cannot be assigned to a cash wallet safely.",
                    json!({"prepared_mode":binding.account_mode,"current_mode":current_mode}),
                ));
            }
        }
        if let Some(reconciliation) = self.reconcile_twap(record, submission, &binding).await? {
            return Ok(reconciliation);
        }
        if let Some(reconciliation) = self
            .reconcile_account_action(record, submission, &binding)
            .await?
        {
            return Ok(reconciliation);
        }
        if matches!(envelope.action, Action::Order { .. } | Action::Amend { .. }) {
            return self.reconcile_orders(record, submission, &binding).await;
        }
        match &envelope.action {
            Action::Cancel { cancels } => {
                let mut proofs = Vec::new();
                for cancel in cancels {
                    let proof = self
                        .transport
                        .info(Info::OrderStatus {
                            owner: account.execution_address().to_owned(),
                            order_id: cancel.order_id,
                        })
                        .await?;
                    let status = proof["order"]["status"].as_str().unwrap_or("");
                    if status != "canceled" && !status.ends_with("Canceled") {
                        return Ok(unresolved(
                            Status::Pending,
                            "Cancellation has not proved removal of the original resting quantity.",
                            proof,
                        ));
                    }
                    proofs.push(proof);
                }
                self.verified_result(
                    record,
                    submission,
                    Status::Completed,
                    Vec::new(),
                    json!({"method":"exact-owner-order-cancellation","orders":proofs}),
                    BTreeMap::new(),
                )
            }
            Action::UsdClassTransfer {
                amount_usdc,
                to_perp,
                nonce,
            } => {
                if let Some(child) = self.nonces.recovery_child(&record.id)?
                    && let Some(proof) =
                        completed_noop_child_for_parent(&child, record, account, submission)?
                {
                    return self.verified_result(
                        record,
                        submission,
                        Status::Failed,
                        Vec::new(),
                        proof,
                        BTreeMap::new(),
                    );
                }
                let ledger = self
                    .transport
                    .info(Info::UserNonFundingLedgerUpdates {
                        owner: account.execution_address().to_owned(),
                        start_time_ms: envelope.nonce_ms.saturating_sub(5000),
                        end_time_ms: None,
                    })
                    .await?;
                let rows = ledger
                    .as_array()
                    .ok_or_else(|| native_error("Class transfer history is not an array."))?;
                let outcome = crate::account_actions::exact_class_transfer_tx_outcome(
                    self,
                    &account.owner,
                    *nonce,
                    &wire["action"],
                )
                .await?;
                let native_tx = match outcome {
                    Some(crate::account_actions::NativeClassTransferOutcome::Accepted(tx)) => tx,
                    Some(crate::account_actions::NativeClassTransferOutcome::Rejected {
                        tx,
                        error,
                    }) => {
                        let same_hash_ledger_rows: Vec<_> = rows
                            .iter()
                            .filter(|row| {
                                row["hash"].as_str().zip(tx["hash"].as_str()).is_some_and(
                                    |(ledger_hash, native_hash)| {
                                        ledger_hash.eq_ignore_ascii_case(native_hash)
                                    },
                                )
                            })
                            .collect();
                        if error != "Insufficient balance for token transfer"
                            || !same_hash_ledger_rows.is_empty()
                        {
                            return Ok(unresolved(
                                Status::Unknown,
                                "The exact native class-transfer transaction has an unrecognized error or contradictory ledger evidence.",
                                json!({"ledger":ledger,"native_tx":tx,"native_error":error}),
                            ));
                        }
                        return self.verified_result(
                            record,
                            submission,
                            Status::Failed,
                            Vec::new(),
                            json!({
                                "method":"exact-committed-native-class-transfer-rejection",
                                "native_tx":tx,
                                "native_error":error,
                                "ledger":ledger,
                                "wire":wire
                            }),
                            BTreeMap::new(),
                        );
                    }
                    None => {
                        return Ok(unresolved(
                            Status::Unknown,
                            "No unique native transaction proves the original class-transfer nonce.",
                            json!({"ledger":ledger}),
                        ));
                    }
                };
                if native_tx["action"]["type"] != "usdClassTransfer"
                    || native_tx["action"]["nonce"] != *nonce
                    || native_tx["action"]["toPerp"] != *to_perp
                    || native_tx["action"]["amount"]
                        .as_str()
                        .is_none_or(|value| q(value).ok() != q(amount_usdc).ok())
                {
                    return Ok(unresolved(
                        Status::Unknown,
                        "The native class-transfer transaction differs from the immutable action fields.",
                        json!({"ledger":ledger,"native_tx":native_tx,"wire":wire}),
                    ));
                }
                let matching: Vec<_> = rows
                    .iter()
                    .filter(|row| {
                        row["delta"]["type"] == "accountClassTransfer"
                            && row["hash"]
                                .as_str()
                                .zip(native_tx["hash"].as_str())
                                .is_some_and(|(ledger_hash, native_hash)| {
                                    ledger_hash.eq_ignore_ascii_case(native_hash)
                                })
                    })
                    .collect();
                let row = match matching.as_slice() {
                    [row] => *row,
                    _ => {
                        return Ok(unresolved(
                            Status::Pending,
                            "The proved class-transfer transaction has no unique ledger credit.",
                            json!({"ledger":ledger,"native_tx":native_tx}),
                        ));
                    }
                };
                if row["delta"]["toPerp"].as_bool() != Some(*to_perp)
                    || q(row["delta"]["usdc"]
                        .as_str()
                        .ok_or_else(|| native_error("Class transfer quantity missing."))?)?
                        != q(amount_usdc)?
                {
                    return Err(native_error(
                        "Class transfer receipt conflicts with original nonce, direction or quantity.",
                    ));
                }
                let from = account.resource(if *to_perp { "spot" } else { "perpetual" }, None);
                let to = account.resource(if *to_perp { "perpetual" } else { "spot" }, None);
                let amount = Amount {
                    asset: usdc(),
                    atoms: q(amount_usdc)?,
                };
                self.verified_result(
                    record,
                    submission,
                    Status::Completed,
                    vec![FinancialEffect::Transfer {
                        from,
                        to,
                        debit: amount.clone(),
                        credit: amount,
                    }],
                    json!({"method":"native-class-transfer-nonce-and-ledger-hash","receipt":row,"native_tx":native_tx}),
                    BTreeMap::new(),
                )
            }
            Action::UpdateLeverage {
                leverage, is_cross, ..
            } => {
                let before = binding
                    .authority_before
                    .as_ref()
                    .ok_or_else(|| native_error("Original leverage setting is missing."))?;
                let coin = before["native_coin"]
                    .as_str()
                    .ok_or_else(|| native_error("Original leverage market is missing."))?;
                let state = self
                    .transport
                    .info(Info::ActiveAssetData {
                        owner: account.execution_address().to_owned(),
                        coin: coin.into(),
                    })
                    .await?;
                let actual = leverage_state(&state, account.execution_address(), coin)?;
                let expected = format!(
                    "{}:{leverage}",
                    if *is_cross { "cross" } else { "isolated" }
                );
                let dex = before["dex"].as_str();
                let accepted = record.dispatch_evidence.as_ref().is_some_and(|dispatch| {
                    dispatch["submission_reference"] == submission.reference
                        && dispatch["response"]["status"] == "ok"
                });
                if !accepted || actual != expected {
                    return Ok(unresolved(
                        Status::Unknown,
                        "The original leverage action and exact post-action setting are not both proved.",
                        json!({"state":state,"expected":expected,"dispatch":record.dispatch_evidence}),
                    ));
                }
                self.verified_result(record, submission, Status::Completed, vec![FinancialEffect::AuthorityChange {
                    resource: account.resource("perpetual", dex), authority: format!("leverage:{coin}"),
                    before: before["leverage_before"].as_str().ok_or_else(|| native_error("Original leverage baseline is missing."))?.into(), after: expected,
                }], json!({"method":"correlated-native-action-and-exact-trading-state","state":state,"dispatch":record.dispatch_evidence}), BTreeMap::new())
            }
            Action::ScheduledCancel { .. } => {
                let Some(dispatch) = record.dispatch_evidence.as_ref() else {
                    return Ok(unresolved(
                        Status::Unknown,
                        "The authority action has no immutable dispatch receipt.",
                        json!({}),
                    ));
                };
                if dispatch["submission_reference"] != submission.reference
                    || dispatch["response"]["status"] != "ok"
                {
                    return Ok(unresolved(
                        Status::Unknown,
                        "Authority action has no exact successful dispatch receipt.",
                        dispatch.clone(),
                    ));
                }
                self.verified_result(record,submission,Status::Completed,Vec::new(),json!({"method":"exact-native-action-receipt","dispatch":dispatch,"action":wire}),BTreeMap::new())
            }
            Action::Noop {} => {
                if record.operation != "hyperliquid.account.invalidate_nonce" {
                    return Ok(unresolved(
                        Status::Unknown,
                        "A native Noop action is not bound to the reviewed nonce-recovery operation.",
                        json!({}),
                    ));
                }
                let Some(recovery) = binding.recovery.as_ref() else {
                    return Ok(unresolved(
                        Status::Unknown,
                        "The Noop submission has no exact durable parent link.",
                        json!({}),
                    ));
                };
                let Some(parent) = self.nonces.operation(&recovery.parent_operation_id)? else {
                    return Ok(unresolved(
                        Status::Unknown,
                        "The linked original transfer is unavailable from the protected journal.",
                        json!({"parent_operation_id":recovery.parent_operation_id}),
                    ));
                };
                if parent.id != recovery.parent_operation_id
                    || parent.submission.as_ref() != Some(&recovery.parent_submission)
                {
                    return Ok(unresolved(
                        Status::Unknown,
                        "The linked original submission changed; nonce recovery remains unresolved.",
                        json!({"parent_operation_id":parent.id}),
                    ));
                }
                let original = match crate::writes::original_class_transfer(account, &parent) {
                    Ok(envelope) => envelope,
                    Err(_) => {
                        return Ok(unresolved(
                            Status::Unknown,
                            "The original transfer is no longer a supported unresolved recovery target.",
                            json!({"parent_operation_id":parent.id}),
                        ));
                    }
                };
                if original.nonce_ms != envelope.nonce_ms {
                    return Ok(unresolved(
                        Status::Unknown,
                        "The Noop does not carry the exact original native nonce.",
                        json!({"parent_operation_id":parent.id}),
                    ));
                }
                let Some(dispatch) = record.dispatch_evidence.as_ref() else {
                    return Ok(unresolved(
                        Status::Unknown,
                        "The Noop has no immutable native dispatch acknowledgement.",
                        json!({}),
                    ));
                };
                let Some(ack) = exact_noop_ack(dispatch, submission) else {
                    return Ok(unresolved(
                        Status::Unknown,
                        "The native response does not prove an accepted Noop; the original transfer remains unresolved.",
                        dispatch.clone(),
                    ));
                };
                self.verified_result(
                    record,
                    submission,
                    Status::Completed,
                    Vec::new(),
                    json!({
                        "method":"exact_native_nonce_noop_ack",
                        "parent_operation_id":parent.id,
                        "parent_submission":recovery.parent_submission,
                        "native_nonce_ms":original.nonce_ms,
                        "ack":ack
                    }),
                    BTreeMap::new(),
                )
            }
            _ => Ok(unresolved(
                Status::Unknown,
                "Action-specific destination or authority proof is not yet available.",
                json!({"dispatch":record.dispatch_evidence}),
            )),
        }
    }
    async fn reconcile_orders(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        binding: &ActionBinding,
    ) -> Result<Reconciliation> {
        let since = submission.data["since_ms"]
            .as_u64()
            .ok_or_else(|| native_error("Original fill-history boundary missing."))?;
        let current_fills = self
            .transport
            .info(Info::UserFillsByTime {
                owner: binding.account.execution_address().to_owned(),
                start_time_ms: since,
                end_time_ms: chrono::Utc::now().timestamp_millis() as u64,
            })
            .await?;
        let previous_fills = record
            .effects
            .as_ref()
            .and_then(|effects| effects.evidence.get("fills"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let raw_fills = match merge_fill_history(&previous_fills, &current_fills)? {
            Some(fills) => fills,
            None => {
                return Ok(unresolved(
                    Status::Unknown,
                    "A previously observed fill identity changed its receipt; settlement remains unresolved.",
                    json!({"previous_rows":previous_fills.len(),"current_rows":current_fills.as_array().map_or(0, Vec::len)}),
                ));
            }
        };
        let fills = raw_fills.as_slice();
        let mut effects = Vec::new();
        let mut receipts = Vec::new();
        let mut digests = BTreeMap::new();
        let mut pending = false;
        let mut any_fill = false;
        let mut dispatch_rejections = Vec::new();
        // A batch can contain multiple fills against the same derivative market.
        // Their position anchors must form one sequential path, not several
        // competing deltas from the same pre-batch snapshot.
        let mut derivative_positions = BTreeMap::<String, DerivativePositionState>::new();
        for (ordinal, scope) in binding.orders.iter().enumerate() {
            if let Some(error) = exact_order_statuses(record, submission, binding.orders.len())
                .and_then(|statuses| statuses.get(ordinal))
                .and_then(exact_order_rejection)
            {
                dispatch_rejections
                    .push(json!({"ordinal":ordinal,"cloid":scope.cloid,"error":error}));
                receipts.push(json!({"client_order_id":scope.cloid,"dispatch_rejection":error}));
                continue;
            }
            let mut order = self
                .transport
                .info(Info::OrderStatusByClientId {
                    owner: binding.account.execution_address().to_owned(),
                    client_order_id: scope.cloid.clone(),
                })
                .await?;
            if order["status"] != "order"
                && let Some(oid) = record
                    .dispatch_evidence
                    .as_ref()
                    .and_then(|_| exact_order_statuses(record, submission, binding.orders.len()))
                    .and_then(|statuses| statuses.get(ordinal))
                    .and_then(|status| {
                        status["resting"]["oid"]
                            .as_u64()
                            .or_else(|| status["filled"]["oid"].as_u64())
                    })
            {
                order = self
                    .transport
                    .info(Info::OrderStatus {
                        owner: binding.account.execution_address().to_owned(),
                        order_id: oid,
                    })
                    .await?;
            }
            if order["status"] != "order" {
                return Ok(unresolved(
                    Status::Unknown,
                    "The original client identity has no authoritative order observation.",
                    order,
                ));
            }
            let native = &order["order"]["order"];
            let native_status = order["order"]["status"]
                .as_str()
                .ok_or_else(|| native_error("Native order status missing."))?;
            let oid = native["oid"].as_u64().ok_or_else(|| {
                native_error("Original client identity did not resolve to an integer order ID.")
            })?;
            let side = if matches!(scope.input.side, OrderSide::Buy) {
                "B"
            } else {
                "A"
            };
            if native["coin"] != scope.market.native_coin
                || native["side"] != side
                || native["cloid"] != scope.cloid
                || q(native["origSz"]
                    .as_str()
                    .ok_or_else(|| native_error("Original size missing."))?)?
                    != q(&scope.input.size)?
            {
                return Err(native_error(
                    "Order changed its approved market, side, client identity or original size.",
                ));
            }
            let total = aggregate_fills(
                fills,
                &scope.market.native_coin,
                if side == "B" {
                    FillSide::Buy
                } else {
                    FillSide::Sell
                },
                oid,
                since,
                &scope.input.size,
                (Some(&scope.min_price), Some(&scope.max_price)),
            )?;
            let remaining = q(native["sz"]
                .as_str()
                .ok_or_else(|| native_error("Remaining order quantity missing."))?)?;
            let terminal = native_status == "filled"
                || native_status == "canceled"
                || native_status.ends_with("Canceled")
                || native_status.ends_with("Rejected")
                || native_status == "rejected";
            if total.size.checked_add(&remaining)? != q(&scope.input.size)? {
                return Ok(unresolved(
                    Status::Pending,
                    "Native remaining size and immutable fill receipts do not yet cover the original quantity.",
                    json!({"order":order,"known_fill_size":total.size,"remaining":remaining,"original_size":scope.input.size,"history_rows":fills.len()}),
                ));
            }
            pending |= !terminal;
            any_fill |= !total.size.is_zero();
            let cash_resource = match binding.account_mode.as_ref() {
                Some(mode) => mode.cash_resource(
                    &binding.account,
                    if matches!(scope.input.market_type, MarketType::Spot) {
                        crate::account_mode::CashWallet::Spot
                    } else {
                        crate::account_mode::CashWallet::Perpetual
                    },
                    scope.input.dex.as_deref(),
                )?,
                None => binding.account.resource(
                    if matches!(scope.input.market_type, MarketType::Spot) {
                        "spot"
                    } else {
                        "perpetual"
                    },
                    scope.input.dex.as_deref(),
                ),
            };
            let resource = if matches!(scope.input.market_type, MarketType::Spot) {
                cash_resource.clone()
            } else {
                crate::account_mode::AccountModeState::derivative_resource(
                    &binding.account,
                    scope.input.dex.as_deref(),
                )
            };
            if !total.size.is_zero() {
                if matches!(scope.input.market_type, MarketType::Spot) {
                    let base = asset(&scope.market.base_asset.reference);
                    let (spent, received) = if side == "B" {
                        (
                            Amount {
                                asset: usdc(),
                                atoms: total.quote.clone(),
                            },
                            Amount {
                                asset: base,
                                atoms: total.size.clone(),
                            },
                        )
                    } else {
                        (
                            Amount {
                                asset: base,
                                atoms: total.size.clone(),
                            },
                            Amount {
                                asset: usdc(),
                                atoms: total.quote.clone(),
                            },
                        )
                    };
                    effects.push(FinancialEffect::Conversion {
                        resource: resource.clone(),
                        spent,
                        received,
                    });
                } else {
                    let position_key = format!("{}:{}", resource.0, scope.market.symbol);
                    let initial = SignedQuantity::new(&scope.position_before)?.integer();
                    let entry = derivative_positions.entry(position_key).or_insert_with(|| {
                        DerivativePositionState {
                            resource: resource.clone(),
                            symbol: scope.market.symbol.clone(),
                            base_asset: asset(&scope.market.base_asset.reference),
                            initial: initial.clone(),
                            current: initial,
                            orders: Vec::new(),
                        }
                    });
                    let state = entry;
                    if state.initial != SignedQuantity::new(&scope.position_before)?.integer() {
                        return Err(native_error(
                            "Same-market batch orders disagree on their immutable pre-batch position.",
                        ));
                    }
                    let before = state.current.clone();
                    let step = BigInt::from(total.size.integer());
                    let after = &before + if side == "B" { step } else { -step };
                    state.current = after;
                    state.orders.push(json!({"oid":oid,"cloid":scope.cloid,"fill_event_digests":total.event_digests}));
                    effects.push(FinancialEffect::Settlement {
                        resource: cash_resource.clone(),
                        asset: usdc(),
                        category: SettlementCategory::RealizedPnl,
                        delta: SignedQuantity::negative(&total.realized_loss),
                    });
                    if !total.realized_loss.is_zero() {
                        effects.push(FinancialEffect::Cost {
                            location: AccountingLocation::Cash {
                                resource: cash_resource.clone(),
                            },
                            amount: Amount {
                                asset: usdc(),
                                atoms: total.realized_loss.clone(),
                            },
                            category: "realized_loss".into(),
                            treatment: CostTreatment::IncludedInDebit,
                        });
                    }
                    effects.push(FinancialEffect::Settlement {
                        resource: cash_resource.clone(),
                        asset: usdc(),
                        category: SettlementCategory::RealizedPnl,
                        delta: SignedQuantity::positive(&total.realized_profit),
                    });
                }
            }
            let mut economic_fees = Quantity::zero();
            let mut fee_tokens: std::collections::BTreeSet<_> = total
                .fees_paid
                .keys()
                .chain(total.fee_rebates.keys())
                .chain(total.fees.keys())
                .cloned()
                .collect();
            if !total.size.is_zero() {
                if matches!(scope.input.market_type, MarketType::Spot) {
                    fee_tokens.insert(scope.market.base_asset.symbol.clone());
                }
                fee_tokens.insert("USDC".into());
            }
            for token in fee_tokens {
                let reference = if matches!(scope.input.market_type, MarketType::Spot)
                    && token == scope.market.base_asset.symbol
                {
                    scope.market.base_asset.reference.as_str()
                } else if token == "USDC" || token == scope.market.quote_asset.symbol {
                    if token != "USDC" {
                        return Err(native_error(
                            "Perpetual or spot fill quote fee token must be native USDC.",
                        ));
                    }
                    "USDC"
                } else {
                    return Err(native_error(
                        "Fill fee token is not an approved exact market asset.",
                    ));
                };
                let fee_asset = asset(reference);
                let paid = total.fees_paid.get(&token).cloned().unwrap_or_default();
                if !paid.is_zero() {
                    economic_fees = economic_fees.checked_add(&if token == "USDC" {
                        paid.clone()
                    } else {
                        product(&paid, &q(&scope.max_price)?)?
                    })?;
                    effects.push(FinancialEffect::Cost {
                        location: AccountingLocation::Cash {
                            resource: cash_resource.clone(),
                        },
                        amount: Amount {
                            asset: fee_asset.clone(),
                            atoms: paid,
                        },
                        category: "trading_fee_paid".into(),
                        treatment: if matches!(scope.input.market_type, MarketType::Spot)
                            && ((side == "B" && token == scope.market.base_asset.symbol)
                                || (side == "A" && token == "USDC"))
                        {
                            CostTreatment::DeductedFromCredit
                        } else {
                            CostTreatment::AdditionalDebit
                        },
                    });
                }
                let rebate = total.fee_rebates.get(&token).cloned().unwrap_or_default();
                if !total.size.is_zero() {
                    effects.push(FinancialEffect::UsableCredit {
                        to: cash_resource.clone(),
                        amount: Amount {
                            asset: fee_asset,
                            atoms: rebate,
                        },
                    });
                }
            }
            if economic_fees > q(&scope.input.max_fee_usdc)? {
                return Err(native_error(
                    "Matching fill fees exceed the economic fee bound.",
                ));
            }
            digests.extend(total.event_digests);
            receipts.push(json!({
                "order":order,
                "size":total.size,
                "quote":total.quote,
                "fees":total.fees,
                "fees_paid":total.fees_paid,
                "fee_rebates":total.fee_rebates,
                "closed_pnl":total.closed_pnl,
                "realized_profit":total.realized_profit,
                "realized_loss":total.realized_loss
            }));
        }
        // The engine consumes one exact post-state anchor per position key.
        // Fold a same-market native batch to its net post-state while retaining
        // every order's immutable fill digests in the anchor evidence.
        for state in derivative_positions.into_values() {
            let DerivativePositionState {
                resource,
                symbol,
                base_asset,
                initial: before,
                current: after,
                orders: order_receipts,
            } = state;
            for (suffix, kind, old, new) in [
                (
                    "long",
                    PositionKind::DerivativeLong,
                    before.clone().max(BigInt::zero()),
                    after.clone().max(BigInt::zero()),
                ),
                (
                    "short",
                    PositionKind::DerivativeShort,
                    (-before.clone()).max(BigInt::zero()),
                    (-after.clone()).max(BigInt::zero()),
                ),
            ] {
                let position = format!("{symbol}:{suffix}");
                let principal = Amount {
                    asset: base_asset.clone(),
                    atoms: Quantity::new(new.to_string())?,
                };
                effects.push(FinancialEffect::PositionChange {
                    resource: resource.clone(),
                    position: position.clone(),
                    asset: principal.asset.clone(),
                    delta: SignedQuantity::new((new - old).to_string())?,
                    scaled_receipt: None,
                    anchor: Some(PositionAnchor {
                        resource: resource.clone(),
                        position,
                        position_kind: kind,
                        principal,
                        scaled_receipt: None,
                        evidence_version: 1,
                        evidence: json!({
                            "start_position":before.to_string(),
                            "position_atoms":after.to_string(),
                            "orders":order_receipts
                        }),
                        observed_at: now(),
                    }),
                });
            }
        }
        let all_rejected = dispatch_rejections.len() == binding.orders.len();
        let status = if all_rejected {
            Status::Failed
        } else if pending {
            Status::Pending
        } else if !dispatch_rejections.is_empty() {
            if record.effects.is_some() {
                Status::Failed
            } else {
                // Persist one unresolved snapshot before terminalizing a mixed
                // batch so accepted-child effects have durable prior progress.
                Status::Pending
            }
        } else if any_fill {
            Status::Completed
        } else {
            Status::Failed
        };
        self.verified_result(record,submission,status,effects,json!({"method":"owner-client-id-and-immutable-fill-receipts","orders":receipts,"fills":raw_fills,"dispatch_rejections":dispatch_rejections.clone(),"partial_outcome":if dispatch_rejections.is_empty(){Value::Null}else{json!({"kind":"mixed_batch","rejected_children":dispatch_rejections})}}),digests)
    }
    pub(crate) fn verified_result(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        status: Status,
        effects: Vec<FinancialEffect>,
        mut evidence: Value,
        event_digests: BTreeMap<String, String>,
    ) -> Result<Reconciliation> {
        let mut canonical_effects = serde_json::to_value(&effects)
            .map_err(|_| native_error("Effect snapshot cannot be encoded."))?;
        for effect in canonical_effects.as_array_mut().into_iter().flatten() {
            if let Some(anchor) = effect.get_mut("anchor").and_then(Value::as_object_mut) {
                anchor.remove("observed_at");
            }
        }
        let snapshot = binding_hash(
            &json!({"submission":submission,"status":status,"effects":canonical_effects,"events":event_digests,"evidence":evidence}),
        )?;
        if let Some(previous) = &record.effects {
            let progress = SettlementProgress::from_evidence(&previous.evidence)?;
            if progress.as_ref().is_some_and(|p| p.snapshot_id == snapshot) {
                return Ok(Reconciliation {
                    status,
                    evidence: previous.evidence.clone(),
                    verified: Some(previous.clone()),
                });
            }
        }
        let sequence = record
            .effects
            .as_ref()
            .map(|proof| SettlementProgress::from_evidence(&proof.evidence))
            .transpose()?
            .flatten()
            .map_or(1, |p| p.sequence.saturating_add(1));
        evidence["settlement_progress"] = json!({"version":1,"sequence":sequence,"snapshot_id":snapshot,"event_digests":event_digests});
        let mut verified = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects,
            evidence: evidence.clone(),
            observed_at: now(),
            proof_digest: String::new(),
        };
        verified.proof_digest = verified_effect_digest(&verified)?;
        Ok(Reconciliation {
            status,
            evidence,
            verified: Some(verified),
        })
    }
}

#[cfg(test)]
mod pre_dispatch_recovery_tests {
    use super::*;
    use crate::profile::HyperliquidExecutionScope;
    use async_trait::async_trait;
    use std::sync::Arc;

    fn request_identity() -> (HyperliquidAccount, Submission, Envelope, Value) {
        let account = HyperliquidAccount {
            name: "synthetic".into(),
            owner: "0x1111111111111111111111111111111111111111".into(),
            credential_ref: None,
            signing_enabled: false,
            execution_scope: HyperliquidExecutionScope::Owner,
        };
        let envelope = Envelope {
            nonce_ms: 123,
            vault_address: None,
            action: Action::UsdClassTransfer {
                amount_usdc: "1".into(),
                to_perp: true,
                nonce: 123,
            },
        };
        let wire = envelope.action_wire_json().unwrap();
        let reference = binding_hash(&json!({
            "version":1,
            "owner":account.owner,
            "execution_address":account.execution_address(),
            "vault_address":account.vault_address(),
            "operation":"hyperliquid.account.transfer_class",
            "wire":wire
        }))
        .unwrap();
        let submission = Submission {
            kind: SUBMISSION_KIND.into(),
            reference,
            data: json!({
                "owner":account.owner,
                "execution_address":account.execution_address(),
                "account":account.name,
                "envelope":envelope,
                "wire":wire,
                "operation":"hyperliquid.account.transfer_class",
                "since_ms":118
            }),
        };
        (account, submission, envelope, wire)
    }

    fn record(error: FinError, dispatch_evidence: Option<Value>) -> OperationRecord {
        OperationRecord {
            id: "synthetic-operation".into(),
            request_id: "synthetic-request".into(),
            fingerprint: "synthetic-fingerprint".into(),
            fingerprint_version: 1,
            operation: "hyperliquid.account.transfer_class".into(),
            input: json!({}),
            status: Status::Unknown,
            created_at: "2026-10-10T00:00:00Z".into(),
            updated_at: "2026-10-10T00:00:01Z".into(),
            preview: None,
            approval: None,
            submission: None,
            dispatch_evidence,
            effects: None,
            last_reconciliation_evidence: None,
            error: Some(error),
            run_id: None,
        }
    }

    fn record_with_preview(
        account: HyperliquidAccount,
        submission: &Submission,
        error: FinError,
        dispatch_evidence: Option<Value>,
    ) -> OperationRecord {
        let action: ActionBinding = serde_json::from_value(json!({
            "account":account,
            "account_mode":null,
            "action":submission.data["envelope"]["action"],
            "orders":[],
            "authority_before":null
        }))
        .unwrap();
        let mut record = record(error, dispatch_evidence);
        record.submission = Some(submission.clone());
        record.preview = Some(PreparedOperation {
            operation: record.operation.clone(),
            input: json!({}),
            binding: serde_json::to_value(action).unwrap(),
            summary: "synthetic recovery".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: "2026-10-10T00:00:00Z".into(),
            expires_at: "2026-10-10T00:05:00Z".into(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        });
        record
    }

    fn legacy_error() -> FinError {
        FinError::new(
            "CREDENTIAL_TYPE_MISMATCH",
            "credentials",
            "This credential provider cannot access a protected Hyperliquid signer.",
            "Use the protected installed credential provider.",
        )
    }

    struct ReadUnavailable;
    #[async_trait]
    impl HyperliquidTransport for ReadUnavailable {
        async fn info(&self, _: Info) -> Result<Value> {
            Err(FinError::validation(
                "unexpected network read in recovery test",
            ))
        }
        async fn dispatch(
            &self,
            _: &HyperliquidAccount,
            _: &Envelope,
            _: Arc<dyn SubmissionJournal>,
        ) -> Result<Value> {
            Err(FinError::validation("reconciliation must never dispatch"))
        }
    }

    struct NoPrices;
    #[async_trait]
    impl HyperliquidPrices for NoPrices {
        async fn usdc_price(&self) -> Result<AssetPrice> {
            Err(FinError::validation("price is not used during recovery"))
        }
    }

    struct NoNonces;
    impl HyperliquidNonces for NoNonces {
        fn allocate(&self, _: &str, _: u64) -> Result<u64> {
            Err(FinError::validation(
                "reconciliation must not allocate a nonce",
            ))
        }
    }

    #[test]
    fn exact_legacy_local_signer_failure_is_retirable_only_without_dispatch_evidence() {
        let (account, submission, envelope, wire) = request_identity();
        let legacy = record(legacy_error(), None);
        assert_eq!(
            proven_pre_dispatch_failure(&legacy, &submission, &account, &envelope, &wire),
            Some("legacy_default_provider_credential_failure")
        );

        let with_evidence = record(legacy_error(), Some(json!({"response":{"status":"ok"}})));
        assert_eq!(
            proven_pre_dispatch_failure(&with_evidence, &submission, &account, &envelope, &wire),
            None
        );
        let mut altered = legacy_error();
        altered.message.push_str(" (different failure)");
        let changed = record(altered, None);
        assert_eq!(
            proven_pre_dispatch_failure(&changed, &submission, &account, &envelope, &wire),
            None
        );
    }

    #[test]
    fn typed_pre_dispatch_marker_must_bind_exact_submission_and_action() {
        let (account, submission, envelope, wire) = request_identity();
        let error = FinError::new(
            "HYPERLIQUID_PREDISPATCH_CREDENTIAL_FAILURE",
            "pre-dispatch",
            "Protected credential lookup failed before venue dispatch.",
            "Repair the protected credential mapping, then reconcile this operation.",
        );
        let marker = json!({
            "kind":"hyperliquid_pre_dispatch_failure",
            "version":1,
            "phase":"pre_dispatch",
            "outcome":"not_submitted",
            "reason":"protected_credential_lookup_failed",
            "submission_reference":submission.reference,
            "owner":account.owner,
            "execution_address":account.execution_address(),
            "envelope":envelope,
            "wire":wire,
            "error_code":error.code
        });
        let exact = record(error.clone(), Some(marker.clone()));
        assert_eq!(
            proven_pre_dispatch_failure(&exact, &submission, &account, &envelope, &wire),
            Some("typed_broker_pre_dispatch_credential_failure")
        );

        let mut tampered = marker;
        tampered["submission_reference"] = json!("another-submission");
        let changed = record(error.clone(), Some(tampered));
        assert_eq!(
            proven_pre_dispatch_failure(&changed, &submission, &account, &envelope, &wire),
            None
        );
        let timeout = record(
            FinError::new(
                "HYPERLIQUID_SUBMISSION_UNCERTAIN",
                "submission",
                "The network response was lost.",
                "Reconcile the original submission; do not resubmit.",
            ),
            None,
        );
        assert_eq!(
            proven_pre_dispatch_failure(&timeout, &submission, &account, &envelope, &wire),
            None
        );
    }

    #[test]
    fn only_exact_bound_top_level_error_receipt_proves_rejection() {
        let (account, submission, _, _) = request_identity();
        let error = FinError::new(
            "HYPERLIQUID_ACTION_REJECTED",
            "submission",
            "Hyperliquid explicitly rejected the submitted action.",
            "Reconcile the original operation; do not submit it again.",
        );
        let evidence = json!({
            "source":"Hyperliquid /exchange",
            "submission_reference":submission.reference,
            "response":{"status":"err","response":"Insufficient balance"}
        });
        let exact = record(error.clone(), Some(evidence.clone()));
        assert_eq!(
            proven_explicit_action_rejection(&exact, &submission),
            Some("exact_top_level_hyperliquid_rejection")
        );

        let mut wrong_reference = evidence.clone();
        wrong_reference["submission_reference"] = json!("other-action");
        assert_eq!(
            proven_explicit_action_rejection(
                &record(error.clone(), Some(wrong_reference)),
                &submission
            ),
            None
        );
        for response in [
            json!({"status":"err"}),
            json!({"status":"err","response":"  "}),
            json!({"status":"err","response":"Rejected","type":"unexpected"}),
            json!({"status":"ok","response":{"type":"error","data":"Rejected"}}),
        ] {
            let invalid = json!({
                "source":"Hyperliquid /exchange",
                "submission_reference":submission.reference,
                "response":response
            });
            assert_eq!(
                proven_explicit_action_rejection(
                    &record(error.clone(), Some(invalid)),
                    &submission
                ),
                None
            );
        }
        let timeout = record(
            FinError::new(
                "HYPERLIQUID_SUBMISSION_UNCERTAIN",
                "submission",
                "The network response was lost.",
                "Reconcile the original submission; do not resubmit.",
            ),
            None,
        );
        assert_eq!(
            proven_explicit_action_rejection(&timeout, &submission),
            None
        );
        let native_error_row = json!({
            "source":"Hyperliquid /exchange",
            "submission_reference":submission.reference,
            "response":{"status":"ok","response":{"type":"error","data":{"statuses":[{"error":"Rejected"}]}}}
        });
        assert_eq!(
            proven_explicit_action_rejection(&record(error, Some(native_error_row)), &submission),
            None
        );

        // The helper must never infer rejection from an unbound status string.
        assert!(account.owner.starts_with("0x"));
    }

    #[tokio::test]
    async fn exact_legacy_failure_reconciles_to_failed_without_network_reads() {
        let (account, submission, _, _) = request_identity();
        let profile = HyperliquidProfile::new(
            vec![account.clone()],
            Arc::new(ReadUnavailable),
            Arc::new(NoPrices),
            Arc::new(NoNonces),
        )
        .unwrap();
        let record = record_with_preview(account, &submission, legacy_error(), None);

        let result = profile
            .reconcile_action(&record, &submission)
            .await
            .unwrap();
        assert_eq!(result.status, Status::Failed);
        let verified = result.verified.expect("terminal failure needs proof");
        assert!(verified.effects.is_empty());
        assert_eq!(verified.submission, submission);
        assert_eq!(
            result.evidence["proof"],
            "legacy_default_provider_credential_failure"
        );
    }

    #[tokio::test]
    async fn exact_top_level_rejection_reconciles_failed_with_no_effects_or_native_reads() {
        let (account, submission, _, _) = request_identity();
        let profile = HyperliquidProfile::new(
            vec![account.clone()],
            Arc::new(ReadUnavailable),
            Arc::new(NoPrices),
            Arc::new(NoNonces),
        )
        .unwrap();
        let dispatch = json!({
            "source":"Hyperliquid /exchange",
            "submission_reference":submission.reference,
            "response":{"status":"err","response":"Insufficient balance"}
        });
        let record = record_with_preview(
            account,
            &submission,
            FinError::new(
                "HYPERLIQUID_ACTION_REJECTED",
                "submission",
                "Hyperliquid explicitly rejected the submitted action.",
                "Reconcile the original operation; do not submit it again.",
            ),
            Some(dispatch),
        );

        let result = profile
            .reconcile_action(&record, &submission)
            .await
            .unwrap();
        assert_eq!(result.status, Status::Failed);
        let verified = result.verified.expect("terminal rejection needs proof");
        assert!(verified.effects.is_empty());
        assert_eq!(verified.submission, submission);
        assert_eq!(
            result.evidence["proof"],
            "exact_top_level_hyperliquid_rejection"
        );
    }
}

#[cfg(test)]
mod nonce_invalidation_recovery_tests {
    use super::*;
    use crate::{
        account_mode::{AccountMode, AccountModeState},
        profile::HyperliquidExecutionScope,
    };

    const OWNER: &str = "0x1111111111111111111111111111111111111111";
    const PARENT_ID: &str = "synthetic-parent-operation";
    const REQUEST_ID: &str = "synthetic-child-request";

    fn account() -> HyperliquidAccount {
        HyperliquidAccount {
            name: "synthetic-owner".into(),
            owner: OWNER.into(),
            credential_ref: Some("synthetic-owner-key".into()),
            signing_enabled: true,
            execution_scope: HyperliquidExecutionScope::Owner,
        }
    }

    fn mode(account: &HyperliquidAccount) -> AccountModeState {
        AccountModeState {
            owner: account.owner.clone(),
            execution_address: account.owner.clone(),
            native_abstraction: "disabled".into(),
            mode: AccountMode::Separate,
            dex_abstraction: Some(false),
        }
    }

    fn parent_record(account: &HyperliquidAccount, nonce: u64) -> OperationRecord {
        let envelope = Envelope {
            nonce_ms: nonce,
            vault_address: None,
            action: Action::UsdClassTransfer {
                amount_usdc: "1.25".into(),
                to_perp: true,
                nonce,
            },
        };
        let wire = envelope.action_wire_json().unwrap();
        let submission = Submission {
            kind: SUBMISSION_KIND.into(),
            reference: binding_hash(&json!({
                "version":1,
                "owner":account.owner,
                "execution_address":account.execution_address(),
                "vault_address":account.vault_address(),
                "operation":"hyperliquid.account.transfer",
                "wire":wire
            }))
            .unwrap(),
            data: json!({
                "owner":account.owner,
                "execution_address":account.execution_address(),
                "account":account.name,
                "envelope":envelope,
                "wire":wire,
                "operation":"hyperliquid.account.transfer",
                "since_ms":nonce.saturating_sub(5000)
            }),
        };
        let binding = ActionBinding {
            account: account.clone(),
            account_mode: Some(mode(account)),
            action: envelope.action.clone(),
            orders: Vec::new(),
            authority_before: None,
            replacement: None,
            recovery: None,
            metadata: Default::default(),
        };
        OperationRecord {
            id: PARENT_ID.into(),
            request_id: "synthetic-parent-request".into(),
            fingerprint: "synthetic-parent-fingerprint".into(),
            fingerprint_version: 1,
            operation: "hyperliquid.account.transfer".into(),
            input: json!({
                "request_id":"synthetic-parent-request",
                "account":account.name,
                "asset":"USDC",
                "amount":"1.25",
                "destination":"perpetual",
                "max_amount_usdc":"1.25",
                "max_fee_usdc":"0.01"
            }),
            status: Status::Unknown,
            created_at: "2026-10-10T00:00:00Z".into(),
            updated_at: "2026-10-10T00:00:01Z".into(),
            preview: Some(PreparedOperation {
                operation: "hyperliquid.account.transfer".into(),
                input: json!({
                    "request_id":"synthetic-parent-request",
                    "account":account.name,
                    "asset":"USDC",
                    "amount":"1.25",
                    "destination":"perpetual",
                    "max_amount_usdc":"1.25",
                    "max_fee_usdc":"0.01"
                }),
                binding: serde_json::to_value(binding).unwrap(),
                summary: "synthetic owner class transfer".into(),
                checks: Vec::new(),
                warnings: Vec::new(),
                observed_at: "2026-10-10T00:00:00Z".into(),
                expires_at: "2026-10-10T00:01:00Z".into(),
                valuation: None,
                intent: FinancialIntent {
                    debits: vec![ResourceAmount {
                        resource: account.resource("spot", None),
                        amount: Amount {
                            asset: usdc(),
                            atoms: q("1.25").unwrap(),
                        },
                    }],
                    maximum_costs: vec![CostBound {
                        location: AccountingLocation::Cash {
                            resource: account.resource("spot", None),
                        },
                        amount: Amount {
                            asset: usdc(),
                            atoms: q("0.01").unwrap(),
                        },
                        treatment: CostTreatment::AdditionalDebit,
                    }],
                    expected: vec![EffectConstraint::Transfer {
                        from: account.resource("spot", None),
                        to: account.resource("perpetual", None),
                        debit_asset: usdc(),
                        credit_asset: usdc(),
                        maximum_debit: q("1.25").unwrap(),
                        minimum_credit: q("1.25").unwrap(),
                    }],
                    locks: vec![
                        account.resource("perpetual", None),
                        account.resource("spot", None),
                    ],
                },
                operation_version: 1,
                action_deadline: None,
            }),
            approval: None,
            submission: Some(submission),
            dispatch_evidence: Some(json!({"unknown_response":true})),
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        }
    }

    fn completed_child(
        account: &HyperliquidAccount,
        parent: &OperationRecord,
        ack: Value,
    ) -> OperationRecord {
        let parent_submission = parent.submission.clone().unwrap();
        let parent_envelope = crate::writes::original_class_transfer(account, parent).unwrap();
        let recovery = RecoveryBinding {
            version: 1,
            parent_operation_id: parent.id.clone(),
            parent_submission: parent_submission.clone(),
        };
        let binding = ActionBinding {
            account: account.clone(),
            account_mode: Some(mode(account)),
            action: Action::Noop {},
            orders: Vec::new(),
            authority_before: None,
            replacement: None,
            recovery: Some(recovery),
            metadata: Default::default(),
        };
        let envelope = Envelope {
            nonce_ms: parent_envelope.nonce_ms,
            vault_address: None,
            action: Action::Noop {},
        };
        let wire = envelope.action_wire_json().unwrap();
        let child_submission = Submission {
            kind: SUBMISSION_KIND.into(),
            reference: binding_hash(&json!({
                "version":1,
                "owner":account.owner,
                "execution_address":account.execution_address(),
                "vault_address":account.vault_address(),
                "operation":"hyperliquid.account.invalidate_nonce",
                "wire":wire
            }))
            .unwrap(),
            data: json!({
                "owner":account.owner,
                "execution_address":account.execution_address(),
                "account":account.name,
                "envelope":envelope,
                "wire":wire,
                "operation":"hyperliquid.account.invalidate_nonce",
                "since_ms":envelope.nonce_ms.saturating_sub(5000)
            }),
        };
        let request = json!({
            "request_id":REQUEST_ID,
            "account":account.name,
            "original_operation_id":parent.id
        });
        let recovery_evidence = json!({
            "method":"exact_native_nonce_noop_ack",
            "parent_operation_id":parent.id,
            "parent_submission":parent_submission,
            "native_nonce_ms":envelope.nonce_ms,
            "ack":ack
        });
        let mut effects = VerifiedEffects {
            evidence_version: 1,
            submission: child_submission.clone(),
            effects: Vec::new(),
            evidence: recovery_evidence,
            observed_at: "2026-10-10T00:00:02Z".into(),
            proof_digest: String::new(),
        };
        effects.proof_digest = verified_effect_digest(&effects).unwrap();
        OperationRecord {
            id: "synthetic-child-operation".into(),
            request_id: REQUEST_ID.into(),
            fingerprint: "synthetic-child-fingerprint".into(),
            fingerprint_version: 1,
            operation: "hyperliquid.account.invalidate_nonce".into(),
            input: request.clone(),
            status: Status::Completed,
            created_at: "2026-10-10T00:00:01Z".into(),
            updated_at: "2026-10-10T00:00:02Z".into(),
            preview: Some(PreparedOperation {
                operation: "hyperliquid.account.invalidate_nonce".into(),
                input: request,
                binding: serde_json::to_value(binding).unwrap(),
                summary: "synthetic nonce recovery".into(),
                checks: Vec::new(),
                warnings: Vec::new(),
                observed_at: "2026-10-10T00:00:00Z".into(),
                expires_at: "2026-10-10T00:01:00Z".into(),
                valuation: None,
                intent: FinancialIntent {
                    locks: vec![ResourceId(format!(
                        "hyperliquid:mainnet:nonce:{}:{}",
                        account.owner.to_ascii_lowercase(),
                        envelope.nonce_ms
                    ))],
                    ..FinancialIntent::default()
                },
                operation_version: 1,
                action_deadline: None,
            }),
            approval: None,
            submission: Some(child_submission.clone()),
            dispatch_evidence: Some(json!({
                "source":"Hyperliquid /exchange",
                "submission_reference":child_submission.reference,
                "response":ack
            })),
            effects: Some(effects),
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        }
    }

    #[test]
    fn original_transfer_must_be_exact_owner_signed_unresolved_class_transfer() {
        let account = account();
        let now = chrono::Utc::now().timestamp_millis() as u64;
        let nonce = now - 100;
        let parent = parent_record(&account, nonce);
        assert!(crate::writes::original_class_transfer(&account, &parent).is_ok());

        let mut tampered = parent.clone();
        tampered.submission.as_mut().unwrap().data["wire"] = json!("different");
        assert!(crate::writes::original_class_transfer(&account, &tampered).is_err());

        let mut used = parent.clone();
        used.status = Status::Completed;
        assert!(crate::writes::original_class_transfer(&account, &used).is_err());

        let mut changed_signer = account.clone();
        changed_signer.credential_ref = Some("different-synthetic-key".into());
        assert!(crate::writes::original_class_transfer(&changed_signer, &parent).is_err());
    }

    #[test]
    fn noop_ack_requires_exact_application_success_not_http_or_error_response() {
        let submission = Submission {
            kind: SUBMISSION_KIND.into(),
            reference: "synthetic-reference".into(),
            data: json!({}),
        };
        let accepted = json!({
            "source":"Hyperliquid /exchange",
            "submission_reference":"synthetic-reference",
            "response":{"status":"ok","response":{"type":"default"}}
        });
        assert!(exact_noop_ack(&accepted, &submission).is_some());

        let used_nonce = json!({
            "source":"Hyperliquid /exchange",
            "submission_reference":"synthetic-reference",
            "response":{"status":"err","response":"Nonce already used"}
        });
        assert!(exact_noop_ack(&used_nonce, &submission).is_none());
        assert!(exact_noop_ack(&json!({"http_status":200}), &submission).is_none());
    }

    #[test]
    fn parent_failure_requires_durable_completed_linked_noop_proof() {
        let account = account();
        let parent = parent_record(&account, chrono::Utc::now().timestamp_millis() as u64 - 100);
        let parent_submission = parent.submission.as_ref().unwrap();
        let accepted = json!({"status":"ok","response":{"type":"default"}});
        let child = completed_child(&account, &parent, accepted);
        assert!(
            completed_noop_child_for_parent(&child, &parent, &account, parent_submission)
                .unwrap()
                .is_some()
        );

        let mut lost_response = child.clone();
        lost_response.dispatch_evidence = None;
        assert!(
            completed_noop_child_for_parent(&lost_response, &parent, &account, parent_submission)
                .unwrap()
                .is_none()
        );

        let mut wrong_parent = parent.clone();
        wrong_parent
            .submission
            .as_mut()
            .unwrap()
            .reference
            .push_str("-tampered");
        assert!(
            completed_noop_child_for_parent(&child, &wrong_parent, &account, parent_submission)
                .unwrap()
                .is_none()
        );

        let mut rejected = child;
        rejected.dispatch_evidence.as_mut().unwrap()["response"] =
            json!({"status":"err","response":"Nonce already used"});
        assert!(
            completed_noop_child_for_parent(&rejected, &parent, &account, parent_submission)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn persisted_noop_proof_remains_valid_after_original_nonce_window_expires() {
        let account = account();
        let now = chrono::Utc::now().timestamp_millis() as u64;
        let expired_nonce = now.saturating_sub(2 * 24 * 60 * 60 * 1_000 + 1);
        let parent = parent_record(&account, expired_nonce);
        let parent_submission = parent.submission.as_ref().unwrap();
        let child = completed_child(
            &account,
            &parent,
            json!({"status":"ok","response":{"type":"default"}}),
        );

        assert!(!crate::writes::nonce_is_current(expired_nonce, now));
        assert!(
            completed_noop_child_for_parent(&child, &parent, &account, parent_submission)
                .unwrap()
                .is_some()
        );
    }
}
