use super::*;

fn lifecycle_fixture(
    previous_principal: Option<&str>,
) -> (
    OperationRecord,
    ResourceId,
    gofin_domain::AssetId,
    String,
    String,
) {
    let wallet = resource("solana:mainnet-beta:wallet:owner");
    let native = asset("solana:mainnet-beta", "native", 9);
    let position = "account-funding:ATA111".to_owned();
    let account = "ATA111".to_owned();
    let cap = quantity("50");
    let constraint = EffectConstraint::AccountFundingLifecycle {
        resource: wallet.clone(),
        asset: native.clone(),
        accounts: vec![gofin_domain::RefundableFundingLimit {
            position: position.clone(),
            account: account.clone(),
            maximum_amount: cap.clone(),
        }],
    };
    let mut record = record(vec![constraint], vec![]);
    let prepared = record.preview.as_mut().unwrap();
    prepared.intent.debits = vec![ResourceAmount {
        resource: wallet.clone(),
        amount: Amount {
            asset: native.clone(),
            atoms: cap,
        },
    }];
    prepared.intent.maximum_costs = vec![CostBound {
        location: AccountingLocation::Cash {
            resource: wallet.clone(),
        },
        amount: Amount {
            asset: native.clone(),
            atoms: quantity("20"),
        },
        treatment: CostTreatment::AdditionalDebit,
    }];
    prepared.intent.locks = vec![wallet.clone()];
    prepared.input = json!({"synthetic":"account-funding"});
    let authorization_hash = binding_hash(&authorization_binding(prepared)).unwrap();
    let prepared_binding_hash = binding_hash(&prepared.binding).unwrap();
    record.approval.as_mut().unwrap().binding_hash = authorization_hash;
    if let Some(principal) = previous_principal {
        let mut baseline = position_anchor(
            wallet.0.as_str(),
            &position,
            PositionKind::RefundableAccountFunding,
            principal,
        );
        baseline.principal.asset = native.clone();
        baseline.observed_at = "2026-01-01T00:00:10Z".into();
        record.position_baseline = Some(gofin_domain::PositionAnchorBaseline {
            version: 1,
            captured_at: "2026-01-01T00:00:10Z".into(),
            operation_id: record.id.clone(),
            prepared_binding_hash,
            scope: vec![wallet.clone()],
            coverage: vec![wallet.0.clone()],
            anchors: vec![baseline],
        });
    }
    (record, wallet, native, position, account)
}

#[expect(clippy::too_many_arguments)]
fn funding_progress(
    wallet: &ResourceId,
    asset: &gofin_domain::AssetId,
    position: &str,
    account: &str,
    funded: &str,
    returned: &str,
    remaining: &str,
    total_anchor: Option<&str>,
) -> FinancialEffect {
    let anchor = total_anchor.map(|total| {
        let mut anchor = position_anchor(
            wallet.0.as_str(),
            position,
            PositionKind::RefundableAccountFunding,
            total,
        );
        anchor.principal.asset = asset.clone();
        anchor.observed_at = "2026-01-01T00:00:25Z".into();
        anchor.evidence = json!({"account":account,"refund_authority":"wallet"});
        anchor
    });
    FinancialEffect::AccountFundingProgress {
        resource: wallet.clone(),
        position: position.into(),
        account: account.into(),
        asset: asset.clone(),
        funded: quantity(funded),
        returned: quantity(returned),
        remaining: quantity(remaining),
        state: if remaining == "0" {
            gofin_domain::AccountFundingState::Closed
        } else {
            gofin_domain::AccountFundingState::Open
        },
        anchor,
    }
}

fn event_digest(byte: char) -> String {
    std::iter::repeat_n(byte, 64).collect()
}

#[test]
fn account_funding_progress_books_only_new_debit_and_refund_and_preserves_baseline() {
    let (mut record, wallet, native, position, account) = lifecycle_fixture(Some("10"));
    let created_digest = event_digest('a');
    let initial = progress_proof(
        &record,
        1,
        "funding-1",
        &[("create", &created_digest)],
        vec![funding_progress(
            &wallet,
            &native,
            &position,
            &account,
            "50",
            "0",
            "50",
            Some("60"),
        )],
    );
    validate_effects_mode(&record, &initial, &Status::Pending, true).unwrap();
    let initial_delta = cumulative_boundary_delta(None, &initial).unwrap();
    assert!(initial_delta.effects.iter().any(|effect| matches!(effect,
        FinancialEffect::CashDebit { resource, amount }
            if resource == &wallet && amount.asset == native && amount.atoms.as_str() == "50"
    )));
    assert!(
        !initial_delta
            .effects
            .iter()
            .any(|effect| matches!(effect, FinancialEffect::UsableCredit { .. }))
    );

    record.effects = Some(initial.clone());
    let returned_digest = event_digest('b');
    let partial = progress_proof(
        &record,
        2,
        "funding-2",
        &[("create", &created_digest), ("return-1", &returned_digest)],
        vec![funding_progress(
            &wallet,
            &native,
            &position,
            &account,
            "50",
            "20",
            "30",
            Some("40"),
        )],
    );
    validate_cumulative_settlement_components(Some(&initial.effects), &partial.effects).unwrap();
    validate_effects_mode(&record, &partial, &Status::Pending, true).unwrap();
    let partial_delta = cumulative_boundary_delta(Some(&initial), &partial).unwrap();
    assert!(
        !partial_delta
            .effects
            .iter()
            .any(|effect| matches!(effect, FinancialEffect::CashDebit { .. }))
    );
    assert!(partial_delta.effects.iter().any(|effect| matches!(effect,
        FinancialEffect::UsableCredit { to, amount }
            if to == &wallet && amount.asset == native && amount.atoms.as_str() == "20"
    )));

    record.effects = Some(partial.clone());
    let terminal_digest = event_digest('c');
    let terminal = progress_proof(
        &record,
        3,
        "funding-3",
        &[
            ("create", &created_digest),
            ("return-1", &returned_digest),
            ("return-2", &terminal_digest),
        ],
        vec![funding_progress(
            &wallet,
            &native,
            &position,
            &account,
            "50",
            "50",
            "0",
            Some("10"),
        )],
    );
    validate_cumulative_settlement_components(Some(&partial.effects), &terminal.effects).unwrap();
    validate_effects_mode(&record, &terminal, &Status::Completed, false).unwrap();
    let terminal_delta = cumulative_boundary_delta(Some(&partial), &terminal).unwrap();
    assert_eq!(
        terminal_delta
            .effects
            .iter()
            .filter_map(|effect| match effect {
                FinancialEffect::UsableCredit { amount, .. } => Some(amount.atoms.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["30"]
    );
    assert_eq!(
        terminal_delta
            .effects
            .iter()
            .filter(|effect| matches!(effect, FinancialEffect::CashDebit { .. }))
            .count(),
        0,
        "cumulative refunds never debit or rebook the original rent"
    );
}

#[test]
fn boundary_cumulative_funding_counts_provenance_only_once() {
    let (record, wallet, native, position, account) = lifecycle_fixture(None);
    let mut open = proof(
        &record,
        vec![funding_progress(
            &wallet,
            &native,
            &position,
            &account,
            "50",
            "0",
            "50",
            Some("50"),
        )],
    );
    // The adapter may include a provenance link alongside its cumulative
    // account-funding state. The typed progress row owns the gross debit;
    // the link must not charge those same atoms a second time.
    open.effects.push(FinancialEffect::OriginMovement {
        from: AccountingLocation::Cash {
            resource: wallet.clone(),
        },
        to: AccountingLocation::Position {
            resource: wallet.clone(),
            position,
            position_kind: PositionKind::RefundableAccountFunding,
        },
        debit: Amount {
            asset: native.clone(),
            atoms: quantity("50"),
        },
        credit: Amount {
            asset: native,
            atoms: quantity("50"),
        },
    });

    let delta = cumulative_boundary_delta(None, &open).unwrap();
    let debit_atoms = delta
        .effects
        .iter()
        .filter_map(|effect| match effect {
            FinancialEffect::CashDebit { amount, .. } => Some(amount.atoms.integer()),
            _ => None,
        })
        .fold(BigUint::zero(), |total, amount| total + amount);
    assert_eq!(debit_atoms, BigUint::from(50u8));
}

#[test]
fn open_pending_request_releases_only_wallet_lock_when_request_lock_is_retained() {
    let wallet = resource("solana:mainnet-beta:wallet:owner");
    let request = resource("solana:mainnet-beta:request:request-pda");
    let position_resource = wallet.clone();
    let constraint = EffectConstraint::PendingRequestLifecycle {
        wallet_resource: wallet.clone(),
        request_resource: request.clone(),
        request_id: "request-pda".into(),
        request_kind: "tpsl".into(),
        conditional_positions: vec![gofin_domain::PendingRequestPositionBound {
            resource: position_resource,
            position: "position-pda".into(),
            position_kind: PositionKind::DerivativeLong,
            asset: asset("solana:mainnet-beta", "position-usd:mint", 6),
            minimum_delta: gofin_domain::SignedQuantity::new("-100").unwrap(),
            maximum_delta: gofin_domain::SignedQuantity::new("0").unwrap(),
        }],
        conditional_credits: vec![],
        conditional_pnl: vec![],
        protection: None,
    };
    let mut operation = record(vec![constraint], vec![]);
    let prepared = operation.preview.as_mut().unwrap();
    prepared.intent.locks = vec![wallet.clone(), request.clone()];
    let open = FinancialEffect::PendingRequestProgress {
        wallet_resource: wallet.clone(),
        request_resource: request.clone(),
        request_id: "request-pda".into(),
        request_kind: "tpsl".into(),
        state: gofin_domain::PendingRequestState::Open,
        positions: vec![],
        credits: vec![],
        realized_pnl: vec![],
        protection: None,
    };

    assert_eq!(
        proved_escrow_wallet_locks(prepared, std::slice::from_ref(&open)),
        BTreeSet::from([wallet.clone()]),
        "an exact open request releases only the broad wallet lock"
    );

    let mut no_request_lock = prepared.clone();
    no_request_lock.intent.locks = vec![wallet.clone()];
    assert!(proved_escrow_wallet_locks(&no_request_lock, std::slice::from_ref(&open)).is_empty());

    let FinancialEffect::PendingRequestProgress {
        wallet_resource,
        request_resource,
        request_id,
        request_kind,
        positions,
        credits,
        realized_pnl,
        protection,
        ..
    } = open.clone()
    else {
        unreachable!()
    };
    let closed = FinancialEffect::PendingRequestProgress {
        wallet_resource,
        request_resource,
        request_id,
        request_kind,
        state: gofin_domain::PendingRequestState::Cancelled,
        positions,
        credits,
        realized_pnl,
        protection,
    };
    assert!(proved_escrow_wallet_locks(prepared, &[closed]).is_empty());
}

#[test]
fn account_funding_progress_rejects_duplicate_identities_and_bad_conservation() {
    let (record, wallet, native, position, account) = lifecycle_fixture(None);
    let progress = funding_progress(
        &wallet,
        &native,
        &position,
        &account,
        "50",
        "20",
        "30",
        Some("30"),
    );
    let verified = proof(&record, vec![progress.clone()]);
    validate_effects_mode(&record, &verified, &Status::Completed, false).unwrap();

    let over_return = funding_progress(
        &wallet,
        &native,
        &position,
        &account,
        "50",
        "51",
        "0",
        Some("0"),
    );
    assert_eq!(
        validate_effects_mode(
            &record,
            &proof(&record, vec![over_return]),
            &Status::Completed,
            false,
        )
        .unwrap_err()
        .code,
        "ACCOUNT_FUNDING_LIFECYCLE_INVALID"
    );

    let wrong_remaining = funding_progress(
        &wallet,
        &native,
        &position,
        &account,
        "50",
        "20",
        "29",
        Some("29"),
    );
    assert_eq!(
        validate_effects_mode(
            &record,
            &proof(&record, vec![wrong_remaining]),
            &Status::Completed,
            false,
        )
        .unwrap_err()
        .code,
        "ACCOUNT_FUNDING_LIFECYCLE_INVALID"
    );

    let missing_close_anchor =
        funding_progress(&wallet, &native, &position, &account, "50", "50", "0", None);
    assert_eq!(
        validate_effects_mode(
            &record,
            &proof(&record, vec![missing_close_anchor]),
            &Status::Completed,
            false,
        )
        .unwrap_err()
        .code,
        "ACCOUNT_FUNDING_LIFECYCLE_INVALID"
    );

    let mut duplicate_constraint = record.clone();
    let duplicate = match duplicate_constraint
        .preview
        .as_ref()
        .unwrap()
        .intent
        .expected[0]
        .clone()
    {
        EffectConstraint::AccountFundingLifecycle {
            resource, asset, ..
        } => EffectConstraint::AccountFundingLifecycle {
            resource,
            asset,
            accounts: vec![
                gofin_domain::RefundableFundingLimit {
                    position: position.clone(),
                    account: account.clone(),
                    maximum_amount: quantity("50"),
                },
                gofin_domain::RefundableFundingLimit {
                    position: "another-position".into(),
                    account: account.clone(),
                    maximum_amount: quantity("50"),
                },
            ],
        },
        _ => unreachable!(),
    };
    duplicate_constraint
        .preview
        .as_mut()
        .unwrap()
        .intent
        .expected[0] = duplicate;
    duplicate_constraint.approval.as_mut().unwrap().binding_hash = binding_hash(
        &authorization_binding(duplicate_constraint.preview.as_ref().unwrap()),
    )
    .unwrap();
    assert_eq!(
        validate_effects_mode(
            &duplicate_constraint,
            &proof(&duplicate_constraint, vec![progress]),
            &Status::Completed,
            false,
        )
        .unwrap_err()
        .code,
        "ACCOUNT_FUNDING_LIFECYCLE_INVALID"
    );
}

#[test]
fn account_funding_pending_to_terminal_survives_store_restart_without_double_credit() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("account-funding-lifecycle.sqlite");
    let (operation, wallet, native, position, account) = lifecycle_fixture(Some("10"));
    let mut operation = operation;
    operation.preview.as_mut().unwrap().intent.locks.clear();
    operation.approval.as_mut().unwrap().binding_hash =
        binding_hash(&authorization_binding(operation.preview.as_ref().unwrap())).unwrap();
    let submission = operation.submission.clone().unwrap();
    let mut store = Store::open(&path).unwrap();
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
    store
        .reserve(
            &operation.id,
            &store_reservations_for_intent(operation.preview.as_ref().unwrap()).unwrap(),
        )
        .unwrap();
    let engine = Engine::new(
        store,
        vec![],
        Arc::new(SyntheticAuthorizer),
        EngineOptions::default(),
    )
    .unwrap();

    let created_digest = event_digest('a');
    let pending = progress_proof(
        &operation,
        1,
        "account-funding:1",
        &[("create", &created_digest)],
        vec![funding_progress(
            &wallet,
            &native,
            &position,
            &account,
            "50",
            "0",
            "50",
            Some("60"),
        )],
    );
    let pending_record = engine
        .apply_reconciliation(
            operation,
            Reconciliation {
                status: Status::Pending,
                evidence: pending.evidence.clone(),
                verified: Some(pending),
            },
        )
        .unwrap();
    assert_eq!(pending_record.status, Status::Pending);
    assert!(
        engine
            .store
            .lock()
            .unwrap()
            .reservations(&pending_record.id)
            .unwrap()
            .iter()
            .any(|reservation| {
                reservation.resource
                    == resource_asset_key(&ResourceAmount {
                        resource: wallet.clone(),
                        amount: Amount {
                            asset: native.clone(),
                            atoms: quantity("0"),
                        },
                    })
                    .unwrap()
                    && reservation.units == "0"
                    && reservation.fee_units == "20"
            })
    );

    // The surviving pending fee row was produced by actual reconciliation.
    // A separate write cannot reserve against the obsolete cost-key namespace
    // or consume the cash already committed to that fee ceiling.
    {
        let mut store = engine.store.lock().unwrap();
        let mut second = record(vec![], vec![]);
        second.id = "next-cash-write".into();
        second.request_id = "next-cash-request".into();
        second.fingerprint = "next-cash-fingerprint".into();
        second.status = Status::AwaitingApproval;
        second.submission = None;
        store
            .create_request(
                "next-cash-write",
                "next-cash-request",
                "next-cash-fingerprint",
                "synthetic.transfer",
                None,
                "awaiting_approval",
                &to_value(&second).unwrap(),
                "next-cash-created",
            )
            .unwrap();
        let current = store.list_records().unwrap();
        let resource = resource_asset_key(&ResourceAmount {
            resource: wallet.clone(),
            amount: Amount {
                asset: native.clone(),
                atoms: quantity("95"),
            },
        })
        .unwrap();
        let rows = [ReservationRow {
            resource,
            units: "95".into(),
            fee_units: "0".into(),
        }];
        let result = store.persist_operation_reservation_with_cash_capacities(
            "next-cash-write",
            &rows,
            &to_value(&second).unwrap(),
            &current,
            "next-cash-reserved",
            &[gofin_store::CashReservationCapacity {
                resource: resource_asset_key(&ResourceAmount {
                    resource: wallet.clone(),
                    amount: Amount {
                        asset: native.clone(),
                        atoms: quantity("0"),
                    },
                })
                .unwrap(),
                maximum_total_units: "100".into(),
            }],
        );
        assert!(
            matches!(
                result,
                Err(gofin_store::StoreError::ConcurrentReservationAt {
                    reason: "cash_capacity_exceeded"
                })
            ),
            "expected the produced fee reserve to exhaust scoped cash capacity; got {result:?}"
        );
    }
    drop(engine);

    // Recovery begins from the exact persisted cumulative proof, not from
    // process-local state or a refreshed snapshot.
    let engine = Engine::new(
        Store::open(&path).unwrap(),
        vec![],
        Arc::new(SyntheticAuthorizer),
        EngineOptions::default(),
    )
    .unwrap();
    let operation = engine
        .operations()
        .unwrap()
        .into_iter()
        .find(|row| row.id == pending_record.id)
        .unwrap();
    assert_eq!(operation.effects, pending_record.effects);

    let returned_digest = event_digest('b');
    let terminal = progress_proof(
        &operation,
        2,
        "account-funding:2",
        &[("create", &created_digest), ("return", &returned_digest)],
        vec![funding_progress(
            &wallet,
            &native,
            &position,
            &account,
            "50",
            "50",
            "0",
            Some("10"),
        )],
    );
    let accounting_delta =
        cumulative_boundary_delta(operation.effects.as_ref(), &terminal).unwrap();
    assert!(
        accounting_delta
            .effects
            .iter()
            .any(|effect| matches!(effect,
                FinancialEffect::UsableCredit { to, amount }
                    if to == &wallet && amount.asset == native && amount.atoms.as_str() == "50"
            ))
    );
    assert!(
        !accounting_delta
            .effects
            .iter()
            .any(|effect| matches!(effect, FinancialEffect::CashDebit { .. }))
    );
    assert!(accounting_delta.effects.iter().any(|effect| matches!(effect,
        FinancialEffect::PositionChange { position: actual, delta, .. }
            if actual == &position && delta.is_negative() && delta.magnitude().as_str() == "50"
    )));
    let completed = engine
        .apply_reconciliation(
            operation,
            Reconciliation {
                status: Status::Completed,
                evidence: terminal.evidence.clone(),
                verified: Some(terminal.clone()),
            },
        )
        .unwrap();
    assert_eq!(completed.status, Status::Completed);
    assert!(
        engine
            .store
            .lock()
            .unwrap()
            .reservations(&completed.id)
            .unwrap()
            .is_empty()
    );
    drop(engine);

    let restarted = Engine::new(
        Store::open(&path).unwrap(),
        vec![],
        Arc::new(SyntheticAuthorizer),
        EngineOptions::default(),
    )
    .unwrap();
    let persisted = restarted
        .operations()
        .unwrap()
        .into_iter()
        .find(|row| row.id == completed.id)
        .unwrap();
    assert_eq!(persisted.status, Status::Completed);
    assert_eq!(persisted.effects, Some(terminal));
    assert!(
        restarted
            .store
            .lock()
            .unwrap()
            .reservations(&completed.id)
            .unwrap()
            .is_empty()
    );
}
