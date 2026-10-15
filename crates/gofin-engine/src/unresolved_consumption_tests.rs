// Included inside the engine test module. No venue transport or live credentials.
struct CostConsumptionFixture {
    writes: AtomicUsize,
}
#[async_trait]
impl Adapter for CostConsumptionFixture {
    fn definitions(&self) -> Vec<Definition> {
        vec![SyntheticAdapter::write_definition()]
    }
    async fn read(&self, _: &str, _: &Value) -> Result<Value> {
        Ok(json!({}))
    }
    async fn snapshot(&self) -> Result<StateSnapshot> {
        Ok(StateSnapshot {
            observed_at: now(),
            complete: true,
            pending: vec![],
            state: FinancialState {
                holdings: ["binance-account", "other-account"]
                    .into_iter()
                    .map(|name| ResourceAmount {
                        resource: resource(name),
                        amount: Amount {
                            asset: asset("binance", "ETH", 18),
                            atoms: quantity("10000000000000000000"),
                        },
                    })
                    .collect(),
                ..Default::default()
            },
            coverage: vec!["synthetic independently observed balances".into()],
        })
    }
    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        let mut prepared = SyntheticAdapter::new().prepare(operation, input).await?;
        let owner = input["source"].as_str().unwrap_or("binance-account");
        prepared.binding["source"] = json!(owner);
        prepared.intent.debits[0].resource = resource(owner);
        if let EffectConstraint::Transfer { from, to, .. } = &mut prepared.intent.expected[0] {
            *from = resource(owner);
            if owner == "other-account" {
                *to = resource("other-wallet");
                prepared.binding["target"] = json!("other-wallet");
            }
        }
        if input["conversion"] == true {
            prepared.intent.expected = vec![EffectConstraint::Conversion {
                resource: resource(owner),
                input: asset("binance", "ETH", 18),
                maximum_input: quantity("1000000000000000000"),
                output: asset("binance", "USDC", 6),
                minimum_output: quantity("990000"),
            }];
        }
        prepared.intent.maximum_costs = vec![CostBound {
            location: AccountingLocation::Cash {
                resource: resource(owner),
            },
            amount: Amount {
                asset: asset("binance", "ETH", 18),
                atoms: quantity("10000000000000000"),
            },
            treatment: CostTreatment::AdditionalDebit,
        }];
        let valuation = prepared.valuation.as_mut().unwrap();
        valuation.maximum_fee_usd_micros = quantity("10000");
        valuation.prices = vec![gofin_domain::AssetPrice {
            asset: asset("binance", "ETH", 18),
            usd_micros_per_unit: quantity("1000000"),
            unit_atoms: quantity("1000000000000000000"),
            observed_at: now(),
            source: "synthetic exact cost quote".into(),
        }];
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
        journal.assert_authorized()?;
        let submission = Submission {
            kind: "synthetic-cost-dispatch".into(),
            reference: format!(
                "synthetic:{}",
                prepared.input["source"]
                    .as_str()
                    .unwrap_or("binance-account")
            ),
            data: json!({"request":prepared.input}),
        };
        journal.persist_submission(&submission)?;
        self.writes.fetch_add(1, Ordering::SeqCst);
        Ok(submission)
    }
    async fn reconcile(&self, _: &OperationRecord) -> Result<Reconciliation> {
        Ok(Reconciliation {
            status: Status::Pending,
            evidence: json!({"synthetic":"awaiting receipt"}),
            verified: None,
        })
    }
}
fn cost_consumption_engine(path: &std::path::Path, adapter: Arc<CostConsumptionFixture>) -> Engine {
    Engine::new(
        Store::open(path).unwrap(),
        vec![adapter],
        Arc::new(SyntheticAuthorizer),
        EngineOptions {
            max_movement_and_cost_usd_micros: "10000000".into(),
            max_run_allocation_usd_micros: "2025000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap()
}
fn consumed_costs(
    original: &OperationRecord,
    amount: &str,
    events: Value,
) -> gofin_domain::unresolved_consumption::UnresolvedCostConsumption {
    gofin_domain::unresolved_consumption::UnresolvedCostConsumption::new(
        original.submission.clone().unwrap(),
        vec![FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: resource("binance-account"),
            },
            amount: Amount {
                asset: asset("binance", "ETH", 18),
                atoms: quantity(amount),
            },
            category: "synthetic-native-cost".into(),
            treatment: CostTreatment::AdditionalDebit,
        }],
        json!({"proofEvents":events}),
        now(),
    )
    .unwrap()
}
fn cost_reconciliation(
    proof: &gofin_domain::unresolved_consumption::UnresolvedCostConsumption,
) -> Reconciliation {
    let mut result = Reconciliation {
        status: Status::Unknown,
        evidence: json!({"code":"synthetic fee exceeds approved cap"}),
        verified: None,
    };
    gofin_domain::unresolved_consumption::attach(&mut result, proof).unwrap();
    result
}
#[tokio::test]
async fn unresolved_proven_fee_consumption_survives_restart_and_blocks_another_accounts_shared_budget()
 {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("synthetic-consumed-cost.sqlite");
    let adapter = Arc::new(CostConsumptionFixture {
        writes: AtomicUsize::new(0),
    });
    let engine = cost_consumption_engine(&path, adapter.clone());
    let original = engine
        .execute(OperationRequest {
            request_id: "synthetic-cost-original".into(),
            operation: "synthetic.transfer".into(),
            input: json!({}),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(original.status, Status::Pending, "{:?}", original.error);
    assert_eq!(engine.allocation_used_usd_micros().unwrap(), "1010000");
    let reservations = engine
        .store
        .lock()
        .unwrap()
        .operation_reservations(&original.id)
        .unwrap();
    let event = json!({"eventId":"synthetic-fee-first","asset":"ETH","amount":"0.03"});
    let proof = consumed_costs(&original, "30000000000000000", json!([event]));
    let retained = engine
        .apply_reconciliation(original.clone(), cost_reconciliation(&proof))
        .unwrap();
    assert_eq!(retained.status, Status::Unknown);
    assert_eq!(retained.preview, original.preview);
    assert_eq!(retained.submission, original.submission);
    assert_eq!(retained.approval, original.approval);
    assert_eq!(retained.effects, original.effects);
    assert_eq!(
        engine
            .store
            .lock()
            .unwrap()
            .operation_reservations(&original.id)
            .unwrap(),
        reservations
    );
    assert_eq!(engine.allocation_used_usd_micros().unwrap(), "1030000");
    let duplicate = engine
        .apply_reconciliation(retained.clone(), cost_reconciliation(&proof))
        .unwrap();
    assert_eq!(engine.allocation_used_usd_micros().unwrap(), "1030000");
    drop(engine);
    let engine = cost_consumption_engine(&path, adapter.clone());
    let restored = engine.load_record(&original.id).unwrap();
    assert_eq!(restored, duplicate);
    assert_eq!(engine.allocation_used_usd_micros().unwrap(), "1030000");
    let other = engine
        .execute(OperationRequest {
            request_id: "synthetic-cost-other-account".into(),
            operation: "synthetic.transfer".into(),
            input: json!({"source":"other-account"}),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(other.status, Status::Failed, "{:?}", other.error);
    assert_eq!(other.error.as_ref().unwrap().code, "ALLOCATION_LIMIT");
    assert!(other.submission.is_none());
    assert_eq!(adapter.writes.load(Ordering::SeqCst), 1);
    let mut terminal = VerifiedEffects {
        evidence_version: 1,
        submission: original.submission.clone().unwrap(),
        effects: vec![],
        evidence: json!({"proofEvents":[]}),
        observed_at: now(),
        proof_digest: String::new(),
    };
    terminal.proof_digest = verified_effect_digest(&terminal).unwrap();
    assert!(
        engine
            .apply_reconciliation(
                restored,
                Reconciliation {
                    status: Status::Failed,
                    evidence: terminal.evidence.clone(),
                    verified: Some(terminal)
                }
            )
            .is_err()
    );
    assert_eq!(
        engine.load_record(&original.id).unwrap().status,
        Status::Unknown
    );
    assert_eq!(
        engine
            .store
            .lock()
            .unwrap()
            .operation_reservations(&original.id)
            .unwrap(),
        reservations
    );
}
#[tokio::test]
async fn unresolved_consumption_cannot_change_submission_cost_scope_or_immutable_native_events() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = Arc::new(CostConsumptionFixture {
        writes: AtomicUsize::new(0),
    });
    let engine =
        cost_consumption_engine(&dir.path().join("synthetic-negative-cost.sqlite"), adapter);
    let original = engine
        .execute(OperationRequest {
            request_id: "synthetic-negative-cost".into(),
            operation: "synthetic.transfer".into(),
            input: json!({}),
            run_id: None,
        })
        .await
        .unwrap();
    let event = json!({"eventId":"synthetic-fee-first","asset":"ETH","amount":"0.03"});
    let proof = consumed_costs(&original, "30000000000000000", json!([event]));
    for kind in [
        "submission",
        "asset",
        "location",
        "treatment",
        "digest",
        "event_missing",
        "grant",
    ] {
        let mut invalid = proof.clone();
        match kind {
            "submission" => invalid.submission.reference = "wrong".into(),
            "asset" => {
                let FinancialEffect::Cost { amount, .. } = &mut invalid.costs[0] else {
                    unreachable!()
                };
                amount.asset.reference = "unapproved".into();
            }
            "location" => {
                let FinancialEffect::Cost { location, .. } = &mut invalid.costs[0] else {
                    unreachable!()
                };
                *location = AccountingLocation::Cash {
                    resource: resource("other-account"),
                };
            }
            "treatment" => {
                let FinancialEffect::Cost { treatment, .. } = &mut invalid.costs[0] else {
                    unreachable!()
                };
                *treatment = CostTreatment::DeductedFromCredit;
            }
            "event_missing" => invalid.evidence = json!({"proofEvents":[]}),
            "grant" => invalid.costs.push(transfer_effect()),
            _ => {}
        }
        invalid.proof_digest = gofin_domain::unresolved_consumption::digest(&invalid).unwrap();
        if kind == "digest" {
            invalid.proof_digest = "wrong".into();
        }
        assert!(
            engine
                .apply_reconciliation(original.clone(), cost_reconciliation(&invalid))
                .is_err(),
            "accepted invalid {kind}"
        );
        assert_eq!(engine.load_record(&original.id).unwrap(), original);
        assert!(
            engine
                .store
                .lock()
                .unwrap()
                .unresolved_consumption_usd(&original.id)
                .unwrap()
                .is_none()
        );
    }
    let retained = engine
        .apply_reconciliation(original, cost_reconciliation(&proof))
        .unwrap();
    let changed = consumed_costs(
        &retained,
        "40000000000000000",
        json!([{"eventId":"synthetic-fee-first","asset":"ETH","amount":"0.04"}]),
    );
    assert!(
        engine
            .apply_reconciliation(retained.clone(), cost_reconciliation(&changed))
            .is_err()
    );
    let retracted = consumed_costs(
        &retained,
        "20000000000000000",
        json!([{"eventId":"synthetic-fee-first","asset":"ETH","amount":"0.03"}]),
    );
    assert!(
        engine
            .apply_reconciliation(retained.clone(), cost_reconciliation(&retracted))
            .is_err()
    );
    assert_eq!(engine.load_record(&retained.id).unwrap(), retained);
    assert_eq!(engine.allocation_used_usd_micros().unwrap(), "1030000");
}
#[tokio::test]
async fn unresolved_fee_overrun_after_partial_fill_retains_zero_fee_placeholders_and_prior_native_events()
 {
    let dir = tempfile::tempdir().unwrap();
    let engine = cost_consumption_engine(
        &dir.path().join("synthetic-partial-cost.sqlite"),
        Arc::new(CostConsumptionFixture {
            writes: AtomicUsize::new(0),
        }),
    );
    let original = engine
        .execute(OperationRequest {
            request_id: "synthetic-partial-cost-original".into(),
            operation: "synthetic.transfer".into(),
            input: json!({"conversion":true}),
            run_id: None,
        })
        .await
        .unwrap();
    let fill = json!({"eventId":"synthetic-first-fill","spent":"0.5","fee":"0.001"});
    let transfer = FinancialEffect::Conversion {
        resource: resource("binance-account"),
        spent: Amount {
            asset: asset("binance", "ETH", 18),
            atoms: quantity("500000000000000000"),
        },
        received: Amount {
            asset: asset("binance", "USDC", 6),
            atoms: quantity("497500"),
        },
    };
    let paid = FinancialEffect::Cost {
        location: AccountingLocation::Cash {
            resource: resource("binance-account"),
        },
        amount: Amount {
            asset: asset("binance", "ETH", 18),
            atoms: quantity("1000000000000000"),
        },
        category: "synthetic-native-cost".into(),
        treatment: CostTreatment::AdditionalDebit,
    };
    let mut zero = paid.clone();
    let FinancialEffect::Cost {
        amount, category, ..
    } = &mut zero
    else {
        unreachable!()
    };
    amount.atoms = Quantity::zero();
    *category = "synthetic-zero-placeholder".into();
    let mut proof = VerifiedEffects {
        evidence_version: 1,
        submission: original.submission.clone().unwrap(),
        effects: vec![transfer, paid, zero],
        evidence: json!({"proofEvents":[fill]}),
        observed_at: now(),
        proof_digest: String::new(),
    };
    proof.proof_digest = verified_effect_digest(&proof).unwrap();
    let partial = engine
        .apply_reconciliation(
            original,
            Reconciliation {
                status: Status::Pending,
                evidence: proof.evidence.clone(),
                verified: Some(proof.clone()),
            },
        )
        .unwrap();
    let excess = consumed_costs(
        &partial,
        "30000000000000000",
        json!([fill,{"eventId":"synthetic-second-fee","asset":"ETH","amount":"0.029"}]),
    );
    let retained = engine
        .apply_reconciliation(partial, cost_reconciliation(&excess))
        .unwrap();
    assert_eq!(retained.status, Status::Unknown);
    assert_eq!(retained.effects, Some(proof));
    assert_eq!(engine.allocation_used_usd_micros().unwrap(), "1030000");
    assert_eq!(
        engine
            .store
            .lock()
            .unwrap()
            .unresolved_consumption_proof(&retained.id)
            .unwrap()
            .unwrap()
            .evidence["proofEvents"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn unsettled_in_bound_costs_persist_without_false_overrun_and_cannot_hide_later_excess() {
    use gofin_domain::unresolved_consumption::{ConsumptionReason, digest};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unsettled-costs.sqlite");
    let adapter = Arc::new(CostConsumptionFixture {
        writes: AtomicUsize::new(0),
    });
    let engine = cost_consumption_engine(&path, adapter.clone());
    let original = engine
        .execute(OperationRequest {
            request_id: "unsettled-cost-original".into(),
            operation: "synthetic.transfer".into(),
            input: json!({}),
            run_id: None,
        })
        .await
        .unwrap();
    let reservations = engine
        .store
        .lock()
        .unwrap()
        .operation_reservations(&original.id)
        .unwrap();
    let events = json!([{ "eventId": "unsettled-fee-first", "atoms": "5000000000000000" }]);
    let mut proof = consumed_costs(&original, "5000000000000000", events.clone());
    // The old default proof still fails closed inside its approved per-key cap.
    assert!(
        engine
            .commit_unresolved_cost_consumption(
                &original,
                &cost_reconciliation(&proof),
                &original.preview.as_ref().unwrap().intent
            )
            .is_err()
    );
    proof.reason = ConsumptionReason::ProvenUnsettledCost;
    proof.proof_digest = digest(&proof).unwrap();
    let retained = engine
        .apply_reconciliation(original, cost_reconciliation(&proof))
        .unwrap();
    assert_eq!(retained.status, Status::Unknown);
    assert_eq!(
        retained.error.as_ref().unwrap().code,
        "EXECUTION_COSTS_UNSETTLED"
    );
    assert_eq!(
        engine
            .store
            .lock()
            .unwrap()
            .unresolved_consumption_usd(&retained.id)
            .unwrap(),
        Some("0".into())
    );
    assert!(retained.effects.is_none());
    assert_eq!(engine.allocation_used_usd_micros().unwrap(), "1010000");
    assert_eq!(
        engine
            .store
            .lock()
            .unwrap()
            .operation_reservations(&retained.id)
            .unwrap(),
        reservations
    );
    drop(engine);
    let engine = cost_consumption_engine(&path, adapter);
    let restored = engine.load_record(&retained.id).unwrap();
    let duplicate = engine
        .apply_reconciliation(restored, cost_reconciliation(&proof))
        .unwrap();
    assert_eq!(
        engine
            .store
            .lock()
            .unwrap()
            .unresolved_consumption_usd(&duplicate.id)
            .unwrap(),
        Some("0".into())
    );
    let mut excess = consumed_costs(
        &duplicate,
        "30000000000000000",
        json!([events[0].clone(), {"eventId":"unsettled-fee-second", "atoms":"25000000000000000"}]),
    );
    excess.reason = ConsumptionReason::ProvenUnsettledCost;
    excess.proof_digest = digest(&excess).unwrap();
    let held = engine
        .apply_reconciliation(duplicate, cost_reconciliation(&excess))
        .unwrap();
    assert_eq!(held.error.as_ref().unwrap().code, "COST_BOUND_EXCEEDED");
    assert_eq!(
        engine
            .store
            .lock()
            .unwrap()
            .unresolved_consumption_usd(&held.id)
            .unwrap(),
        Some("20000".into())
    );
    let mut empty = VerifiedEffects {
        evidence_version: 1,
        submission: held.submission.clone().unwrap(),
        effects: vec![],
        evidence: json!({"proofEvents":[]}),
        observed_at: now(),
        proof_digest: String::new(),
    };
    empty.proof_digest = verified_effect_digest(&empty).unwrap();
    assert!(
        engine
            .validate_consumed_costs_before_terminal(&held, &empty)
            .is_err()
    );
    assert_eq!(engine.load_record(&held.id).unwrap(), held);
    assert_eq!(
        engine
            .store
            .lock()
            .unwrap()
            .operation_reservations(&held.id)
            .unwrap(),
        reservations
    );
}
