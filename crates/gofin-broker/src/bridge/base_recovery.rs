//! Explicit read-side recovery planning; this never approves or submits a write.
use super::*;

pub(super) fn base_recovery_definition() -> Definition {
    Definition {
        name: "bridge.recovery".into(),
        description: "Recover the same Base withdrawal or CCTP burn: declare a replacement after proven stage failure, or explicitly request expired CCTP attestation refresh.".into(),
        kind: OperationKind::Read,
        execution_actor: gofin_domain::ExecutionActor::AgentReadable,
        input_schema: serde_json::to_value(schema_for!(BridgeStatusInput)).expect("schema"),
        output_schema: json!({"type":"object","required":["transfer_id","steps","created","next_action"]}),
        error_schema: schema_for_fin_error(),
        examples: vec![json!({"transferId":"TRANSFER_ID_FROM_QUOTE"})],
        coverage: "Base canonical withdrawals and CCTP V2 transfers with settled source proof; no source replay.".into(),
        completion: "Persists new deferred step identities; all prior operations, source proof, bounds and fees remain intact. Repeated reads return the same unsubmitted pair.".into(),
        semantics: json!({"may_submit":false,"may_request_cctp_reattestation":true,"financial_authorization":false,"approval_renewal":false,"agent_supplied_calldata":false,"next":"Obtain fresh exact approval or revise the same named plan before executing the returned stages."}),
    }
}

impl BridgeAdapter {
    pub(super) async fn base_recovery(&self, input: &Value) -> gofin_domain::Result<Value> {
        let request: BridgeStatusInput = serde_json::from_value(input.clone())
            .map_err(|_| GoFinError::validation("Use the bridge.recovery transferId schema."))?;
        let state = self.state()?;
        let original = state
            .transfer(&request.transfer_id)
            .map_err(store_error)?
            .ok_or_else(|| GoFinError::validation("Original bridge transfer was not found."))?;
        let saved = self.saved_quote(&original)?;
        if saved.quote.provider != "base_canonical"
            || saved.quote.intent.source.chain_id != 8453
            || saved.quote.intent.destination_chain_id != Some(1)
            || original.grant_id.is_some()
            || !matches!(
                original.state,
                BridgeTransferState::InTransit
                    | BridgeTransferState::DestinationPending
                    | BridgeTransferState::NeedsAttention
            )
        {
            return Err(GoFinError::validation(
                "Recovery planning requires an unresolved original Base canonical withdrawal.",
            ));
        }
        let locator = original
            .transit
            .as_ref()
            .and_then(|transit| transit.destination_locator.as_ref())
            .ok_or_else(|| {
                GoFinError::validation("The settled original withdrawal source proof is required.")
            })?;
        let withdrawal = saved_base_withdrawal(locator)?;
        let engine = self
            .engine
            .lock()
            .map_err(|_| internal_error())?
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(internal_error)?;
        let operations = engine.operations()?;
        for link in original
            .steps
            .iter()
            .filter(|link| link.kind.is_destination_stage())
        {
            if link.state == BridgeStepState::Submitted
                || link.operation_id.as_ref().is_some_and(|id| {
                    operations
                        .iter()
                        .find(|operation| &operation.id == id)
                        .is_none_or(|operation| !operation.status.terminal())
                })
            {
                return Err(error(
                    "BRIDGE_RECOVERY_ATTEMPT_UNRESOLVED",
                    "reconciliation",
                    "An original proof or finalization attempt is still unresolved.",
                    "Reconcile that exact operation before declaring recovery stages.",
                ));
            }
        }
        if let Some(pair) = unsubmitted_recovery_pair(&original) {
            return Ok(recovery_response(
                &original.id,
                &withdrawal.withdrawal_hash,
                &pair,
                false,
                original.plan_scope.is_some(),
            ));
        }

        let readiness = self
            .base_provider()?
            .withdrawal_readiness(&withdrawal.withdrawal, &saved.quote.intent.source_address)
            .await?;
        if readiness.finalized {
            return Err(GoFinError::validation(
                "The withdrawal is already finalized; inspect bridge.status.",
            ));
        }
        let failed = original.steps.iter().rev().find(|link| {
            link.kind.is_destination_stage()
                && link.state == BridgeStepState::NeedsAttention
                && link.operation_id.as_ref().is_some_and(|id| {
                    operations
                        .iter()
                        .any(|operation| &operation.id == id && operation.status == Status::Failed)
                })
        });
        let (parent, reason, needs_proof) = if readiness.proof_invalidated {
            let proof = original
                .steps
                .iter()
                .rev()
                .find(|link| {
                    link.kind == BridgeStepKind::Prove
                        && link.state == BridgeStepState::Settled
                        && link.operation_id.as_ref().is_some_and(|id| {
                            operations.iter().any(|operation| {
                                &operation.id == id && operation.status == Status::Completed
                            })
                        })
                })
                .ok_or_else(|| {
                    GoFinError::validation(
                        "An invalidated recovery needs the original successful proof identity.",
                    )
                })?;
            (proof, "proof_invalidated", true)
        } else if let Some(failed) = failed {
            (failed, "stage_reverted", readiness.proven_at == 0)
        } else {
            return Err(error(
                "BRIDGE_RECOVERY_NOT_REQUIRED",
                "read",
                "No invalidated settled proof or canonically reverted stage needs recovery.",
                "Inspect bridge.status and preserve the current valid proof.",
            ));
        };
        let updated = append_recovery_pair(
            &original,
            &saved,
            parent,
            &withdrawal.withdrawal_hash,
            reason,
            needs_proof,
        )?;
        let committed = match state.append_base_recovery(&original, &updated) {
            Ok(record) => record,
            Err(_) => {
                // A competing read may have created exactly this recovery pair.
                let current = state
                    .transfer(&original.id)
                    .map_err(store_error)?
                    .ok_or_else(internal_error)?;
                if current.transit != original.transit
                    || current.approved_bounds != original.approved_bounds
                    || current.plan_scope != original.plan_scope
                {
                    return Err(GoFinError::validation(
                        "Recovery parent changed; inspect the original transfer.",
                    ));
                }
                let pair = unsubmitted_recovery_pair(&current).ok_or_else(|| {
                    GoFinError::validation(
                        "The recovery parent changed concurrently; retry this read.",
                    )
                })?;
                return Ok(recovery_response(
                    &current.id,
                    &withdrawal.withdrawal_hash,
                    &pair,
                    false,
                    current.plan_scope.is_some(),
                ));
            }
        };
        let pair = unsubmitted_recovery_pair(&committed).ok_or_else(internal_error)?;
        Ok(recovery_response(
            &committed.id,
            &withdrawal.withdrawal_hash,
            &pair,
            true,
            committed.plan_scope.is_some(),
        ))
    }
}

fn recovery_response(
    id: &str,
    hash: &str,
    pair: &[BridgeStepLink],
    created: bool,
    plan_owned: bool,
) -> Value {
    json!({"transfer_id":id,"withdrawal_hash":hash,"created":created,"steps":pair,"next_action":if plan_owned {"Review the new stages and revise the same named plan before execution. Prior authority and consumed fees remain unchanged; exact writes cannot adopt this plan-owned parent."} else {"Review the new stages and obtain fresh exact approval before execution. No write or consent renewal occurred."}})
}

fn unsubmitted_recovery_pair(record: &BridgeTransferRecord) -> Option<Vec<BridgeStepLink>> {
    let marker = &record.quote_revisions.last()?.payload["base_recovery"];
    let ids = marker["step_ids"].as_array()?;
    if ids.is_empty() || ids.len() > 2 {
        return None;
    }
    let pair = ids
        .iter()
        .map(|id| {
            record
                .steps
                .iter()
                .find(|link| Some(link.step_id.as_str()) == id.as_str())
                .cloned()
        })
        .collect::<Option<Vec<_>>>()?;
    ((pair.len() == 2
        && pair[0].kind == BridgeStepKind::Prove
        && pair[1].kind == BridgeStepKind::Finalize)
        || (pair.len() == 1 && pair[0].kind == BridgeStepKind::Finalize))
        .then_some(pair)
        .filter(|pair| {
            pair.iter().all(|link| {
                link.state == BridgeStepState::Unsubmitted && link.operation_id.is_none()
            })
        })
}

fn append_recovery_pair(
    original: &BridgeTransferRecord,
    saved: &SavedQuote,
    parent: &BridgeStepLink,
    withdrawal_hash: &str,
    reason: &str,
    needs_proof: bool,
) -> gofin_domain::Result<BridgeTransferRecord> {
    let mut next = original.clone();
    let deposit = original
        .steps
        .iter()
        .find(|link| link.kind == BridgeStepKind::Deposit && link.state == BridgeStepState::Settled)
        .ok_or_else(|| {
            GoFinError::validation("Original settled initiation identity is missing.")
        })?;
    let ordinal = u8::try_from(saved.plan.steps.len()).map_err(|_| {
        GoFinError::validation("Recovery step history reached its finite ordinal bound.")
    })?;
    let final_ordinal = ordinal.checked_add(u8::from(needs_proof)).ok_or_else(|| {
        GoFinError::validation("Recovery step history reached its finite ordinal bound.")
    })?;
    let deposit_ordinal = saved
        .step_ids
        .iter()
        .find_map(|(ordinal, id)| (id == &deposit.step_id).then_some(*ordinal))
        .ok_or_else(internal_error)?;
    let prove_id = uuid::Uuid::new_v4().to_string();
    let finalize_id = uuid::Uuid::new_v4().to_string();
    for link in &mut next.steps {
        if link.kind == BridgeStepKind::Finalize
            && link.state == BridgeStepState::Unsubmitted
            && link.operation_id.is_none()
        {
            link.state = BridgeStepState::Superseded;
        }
    }
    let mut plan = saved.plan.clone();
    let mut ids = saved.step_ids.clone();
    let mut declarations = Vec::new();
    if needs_proof {
        declarations.push((
            ordinal,
            ProviderStepKind::Prove,
            prove_id.clone(),
            vec![deposit_ordinal],
        ));
    }
    declarations.push((
        final_ordinal,
        ProviderStepKind::Finalize,
        finalize_id.clone(),
        vec![if needs_proof {
            ordinal
        } else {
            deposit_ordinal
        }],
    ));
    for (ordinal, kind, id, dependencies) in declarations {
        let step = BridgeStep {
            ordinal,
            kind,
            solana_mint: None,
            call: Some(EvmCall {
                chain_id: 1,
                to: gofin_bridge::base_canonical::PORTAL.into(),
                value: Quantity::zero(),
                data: "0x".into(),
            }),
            expected_sender: saved.quote.intent.source_address.clone(),
            maximum_debit: vec![],
            dependencies: dependencies.clone(),
        };
        let depends_on = dependencies
            .iter()
            .map(|dep| ids.get(dep).cloned().ok_or_else(internal_error))
            .collect::<gofin_domain::Result<Vec<_>>>()?;
        ids.insert(ordinal, id.clone());
        plan.steps.push(step);
        next.steps.push(BridgeStepLink {
            transfer_id: next.id.clone(),
            grant_id: None,
            plan_authority: None,
            step_id: id,
            operation_id: None,
            kind: domain_step_kind(&kind),
            depends_on,
            state: BridgeStepState::Unsubmitted,
        });
    }
    let mut payload = original
        .quote_revisions
        .last()
        .ok_or_else(internal_error)?
        .payload
        .clone();
    payload["plan"] = json!(plan);
    payload["step_ids"] = json!(ids);
    payload["base_recovery"] = json!({"withdrawal_hash":withdrawal_hash,"reason":reason,"parent_step_id":parent.step_id,"parent_operation_id":parent.operation_id,"step_ids":if needs_proof {vec![prove_id,finalize_id]} else {vec![finalize_id]}});
    let previous = original.quote_revisions.last().ok_or_else(internal_error)?;
    next.quote_revisions.push(BridgeQuoteRevision {
        revision: previous
            .revision
            .checked_add(1)
            .ok_or_else(internal_error)?,
        provider: previous.provider.clone(),
        route_id: previous.route_id.clone(),
        order_id: previous.order_id.clone(),
        quote_hash: binding_hash(&payload)?,
        payload,
        observed_at: now(),
        valid_until: previous.valid_until.clone(),
    });
    next.version = original.version.checked_add(1).ok_or_else(internal_error)?;
    next.state = BridgeTransferState::DestinationPending;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (BridgeTransferRecord, SavedQuote) {
        let source = EvmAsset {
            chain_id: 8453,
            token: None,
            decimals: 18,
        };
        let destination = EvmAsset {
            chain_id: 1,
            token: None,
            decimals: 18,
        };
        let owner = "0x1111111111111111111111111111111111111111".to_owned();
        let intent = BridgeIntent {
            wallet: gofin_domain::ResourceId(format!("ethereum:8453:{owner}")),
            source_address: owner.clone(),
            source: source.clone(),
            solana_destination: None,
            destination_chain_id: Some(1),
            destination_assets: vec![destination.clone()],
            recipient: owner.clone(),
            refund_to: owner.clone(),
            allowed_refund_assets: vec![],
            amount: Quantity::from_u64(100),
        };
        let steps = [
            ProviderStepKind::Deposit,
            ProviderStepKind::Prove,
            ProviderStepKind::Finalize,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, kind)| BridgeStep {
            ordinal: index as u8,
            kind,
            solana_mint: None,
            call: Some(EvmCall {
                chain_id: if index == 0 { 8453 } else { 1 },
                to: gofin_bridge::base_canonical::PORTAL.into(),
                value: Quantity::zero(),
                data: if index == 0 {
                    "0x1234".into()
                } else {
                    "0x".into()
                },
            }),
            expected_sender: owner.clone(),
            maximum_debit: if index == 0 {
                vec![source.amount(Quantity::from_u64(100))]
            } else {
                vec![]
            },
            dependencies: if index == 0 {
                vec![]
            } else {
                vec![index as u8 - 1]
            },
        })
        .collect::<Vec<_>>();
        let quote = BridgeQuote {
            cctp: None,
            quote_id: "SYNTHETIC-quote".into(),
            provider: "base_canonical".into(),
            route_id: "SYNTHETIC-route".into(),
            intent,
            gross_debit: source.amount(Quantity::from_u64(100)),
            expected_receipt: destination.amount(Quantity::from_u64(100)),
            minimum_receipt: destination.amount(Quantity::from_u64(100)),
            fees: vec![],
            approval: None,
            protocol_steps: steps.clone(),
            estimated_arrival_seconds: None,
            observed_at: now(),
            valid_until: now(),
            class: gofin_bridge::QuoteClass::Executable,
            refund_paths: vec![],
            order_solver: None,
            protocol_order_id: None,
            provider_request_id: None,
        };
        let bounds = BridgeBounds::default();
        let plan = PreparedBridge {
            quote: quote.clone(),
            bounds: bounds.clone(),
            steps,
            source_order_id: None,
            fixed_scope_digest: "SYNTHETIC-original-scope".into(),
        };
        let step_ids = [
            (0, "deposit".into()),
            (1, "prove".into()),
            (2, "finalize".into()),
        ]
        .into_iter()
        .collect::<BTreeMap<_, _>>();
        let saved = SavedQuote {
            quote,
            bounds,
            plan,
            step_ids,
        };
        let payload = json!({"quote":saved.quote,"bounds":saved.bounds,"plan":saved.plan,"step_ids":saved.step_ids});
        let links = saved
            .plan
            .steps
            .iter()
            .map(|step| BridgeStepLink {
                transfer_id: "SYNTHETIC-parent".into(),
                grant_id: None,
                plan_authority: None,
                step_id: saved.step_ids[&step.ordinal].clone(),
                operation_id: if step.ordinal < 2 {
                    Some(format!("SYNTHETIC-original-{}", step.ordinal))
                } else {
                    None
                },
                kind: domain_step_kind(&step.kind),
                depends_on: step
                    .dependencies
                    .iter()
                    .map(|index| saved.step_ids[index].clone())
                    .collect(),
                state: if step.ordinal < 2 {
                    BridgeStepState::Settled
                } else {
                    BridgeStepState::Unsubmitted
                },
            })
            .collect();
        let parent = BridgeTransferRecord {
            version: 1,
            id: "SYNTHETIC-parent".into(),
            grant_id: None,
            plan_scope: Some(gofin_domain::BridgePlanScope {
                plan_id: "SYNTHETIC-existing-plan".into(),
                revision_id: "SYNTHETIC-revision".into(),
            }),
            metadata: json!({"SYNTHETIC":"original parent"}),
            settlement_expectation: None,
            approved_bounds: Some(json!({"SYNTHETIC":"original caps"})),
            prospective_debit: None,
            state: BridgeTransferState::DestinationPending,
            quote_revisions: vec![BridgeQuoteRevision {
                revision: 1,
                provider: "base_canonical".into(),
                route_id: "SYNTHETIC-route".into(),
                order_id: None,
                quote_hash: binding_hash(&payload).unwrap(),
                payload,
                observed_at: now(),
                valid_until: now(),
            }],
            steps: links,
            allocation_owner: BridgeAllocationOwner::Transfer {
                transfer_id: "SYNTHETIC-parent".into(),
            },
            transit: None,
            applied_proofs: vec!["SYNTHETIC-consumed-source-proof".into()],
            settlement_proofs: vec![],
        };
        (parent, saved)
    }
    #[test]
    fn invalidated_proof_appends_new_finite_pair_and_preserves_all_source_and_plan_history() {
        let (old, saved) = fixture();
        let next = append_recovery_pair(
            &old,
            &saved,
            &old.steps[1],
            "SYNTHETIC-withdrawal",
            "proof_invalidated",
            true,
        )
        .unwrap();
        assert_eq!(next.plan_scope, old.plan_scope);
        assert_eq!(next.steps[0], old.steps[0]);
        assert_eq!(next.steps[1], old.steps[1]);
        assert_eq!(next.steps[2].state, BridgeStepState::Superseded);
        assert_eq!(next.applied_proofs, old.applied_proofs);
        assert_eq!(
            next.quote_revisions[1].payload["quote"],
            old.quote_revisions[0].payload["quote"]
        );
        let pair = unsubmitted_recovery_pair(&next).unwrap();
        assert_eq!(pair.len(), 2);
        assert_eq!(pair[0].depends_on, vec!["deposit"]);
        assert_eq!(pair[1].depends_on, vec![pair[0].step_id.clone()]);
        assert!(
            pair.iter()
                .all(|link| link.operation_id.is_none() && link.plan_authority.is_none())
        );
        assert_eq!(unsubmitted_recovery_pair(&next).unwrap(), pair);
        assert_eq!(
            next.quote_revisions[1].payload["plan"]["steps"][3]["call"]["data"],
            "0x"
        );
    }
    #[test]
    fn failed_final_with_valid_proof_gets_one_new_final_and_never_repeats_proving() {
        let (mut old, saved) = fixture();
        old.steps[2].state = BridgeStepState::NeedsAttention;
        old.steps[2].operation_id = Some("SYNTHETIC-failed-final".into());
        let next = append_recovery_pair(
            &old,
            &saved,
            &old.steps[2],
            "SYNTHETIC-withdrawal",
            "stage_reverted",
            false,
        )
        .unwrap();
        assert_eq!(next.steps[2], old.steps[2]);
        let pair = unsubmitted_recovery_pair(&next).unwrap();
        assert_eq!(pair.len(), 1);
        assert_eq!(pair[0].kind, BridgeStepKind::Finalize);
        assert_eq!(pair[0].depends_on, vec!["deposit"]);
    }
    #[test]
    fn finite_ordinal_exhaustion_declares_nothing() {
        let (old, mut saved) = fixture();
        saved.plan.steps = vec![saved.plan.steps[0].clone(); 255];
        assert!(
            append_recovery_pair(
                &old,
                &saved,
                &old.steps[1],
                "SYNTHETIC-withdrawal",
                "proof_invalidated",
                true
            )
            .is_err()
        );
    }
}
