use super::*;
use fin_domain::{FinancialIntent, ResourceId, Submission, amendments::AmendmentScope};

fn submission(reference: &str) -> Submission {
    Submission {
        kind: "SYNTHETIC-MUTATION-ONLY".into(),
        reference: reference.into(),
        data: json!({"synthetic":true}),
    }
}
fn rows(id: &str, units: &str) -> Vec<ReservationRow> {
    vec![ReservationRow {
        resource: format!("lock:SYNTHETIC-{id}"),
        units: units.into(),
        fee_units: "0".into(),
    }]
}
fn original(store: &mut Store, id: &str) -> Value {
    let submission = submission(&format!("SYNTHETIC-original-{id}"));
    let record = json!({"id":id,"request_id":format!("SYNTHETIC-request-{id}"),"fingerprint":format!("SYNTHETIC-fingerprint-{id}"),"status":"submitted","run_id":null,"submission":submission,"preview":{"intent":{"locks":[format!("SYNTHETIC-{id}")]}}});
    store
        .create_request(
            id,
            record["request_id"].as_str().unwrap(),
            record["fingerprint"].as_str().unwrap(),
            "SYNTHETIC.order",
            None,
            "submitted",
            &record,
            "t0",
        )
        .unwrap();
    store
        .persist_submission(
            id,
            &SubmissionRow {
                kind: submission.kind,
                reference: submission.reference,
                data: submission.data,
            },
            "t0",
        )
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO reservations VALUES(?1,?2,?3,'0')",
            params![
                id,
                format!("lock:SYNTHETIC-{id}"),
                "100000000000000000000000001"
            ],
        )
        .unwrap();
    store.get_record(id).unwrap().unwrap()
}
fn prepared(store: &mut Store, amend_b: bool) -> Value {
    original(store, "a");
    original(store, "b");
    let children: [MutationChild; 2] = std::array::from_fn(|index| {
        let id = if index == 0 { "a" } else { "b" };
        MutationChild {
            child_tag: format!("SYNTHETIC-child-{id}"),
            operation_id: id.into(),
            submission: submission(&format!("SYNTHETIC-original-{id}")),
            mutation: if index == 0 || amend_b {
                ChildMutation::Amend {
                    revision: Box::new(AmendmentScope {
                        intent: FinancialIntent {
                            locks: vec![ResourceId(format!("SYNTHETIC-{id}"))],
                            ..Default::default()
                        },
                        binding: json!({"syntheticLimit":"2"}),
                        valuation: None,
                    }),
                }
            } else {
                ChildMutation::Cancel
            },
            incremental_valuation: None,
        }
    });
    let record = json!({"id":"batch","request_id":"SYNTHETIC-batch-request","fingerprint":"SYNTHETIC-batch-fingerprint","status":"awaiting_approval","run_id":null,"submission":null,"preview":{"intent":{"locks":["SYNTHETIC-a","SYNTHETIC-b"],"expected":[EffectConstraint::CompoundSubmissionMutations{children:children.into()}]}}});
    store
        .create_request(
            "batch",
            "SYNTHETIC-batch-request",
            "SYNTHETIC-batch-fingerprint",
            "SYNTHETIC.batch",
            None,
            "awaiting_approval",
            &record,
            "t1",
        )
        .unwrap();
    record
}
fn reserved(store: &mut Store, amend_b: bool) -> Value {
    let prepared = prepared(store, amend_b);
    let snapshot = store.list_records().unwrap();
    let mut reservations = rows("a", "0");
    reservations.extend(rows("b", "0"));
    store
        .persist_compound_amendment_reservation("batch", &reservations, &prepared, &snapshot, "t1")
        .unwrap();
    let batch = submission("SYNTHETIC-batch");
    store
        .persist_submission(
            "batch",
            &SubmissionRow {
                kind: batch.kind,
                reference: batch.reference,
                data: batch.data,
            },
            "t2",
        )
        .unwrap();
    store.get_record("batch").unwrap().unwrap()
}
fn child(store: &Store, tag: &str, outcome: ChildMutationOutcome) -> CompoundChildSettlement {
    let batch = store.get_record("batch").unwrap().unwrap();
    let link = links(&batch)
        .unwrap()
        .into_iter()
        .find(|child| child.operation_id == tag)
        .unwrap();
    let proof = serde_json::to_value(MutationChildProof {
        child_tag: link.child_tag.clone(),
        operation_id: tag.into(),
        submission: link.submission.clone(),
        outcome: outcome.clone(),
        native_evidence: json!({"nativeIdentity":format!("SYNTHETIC-native-{tag}")}),
    })
    .unwrap();
    let revision = match (&link.mutation, &outcome) {
        (ChildMutation::Amend { revision }, ChildMutationOutcome::Accepted { successor }) => {
            Some(ApprovedAmendment {
                amendment_operation_id: "batch".into(),
                amendment_submission: serde_json::from_value(batch["submission"].clone()).unwrap(),
                scope: *revision.clone(),
                successor: successor.clone(),
                proof_digest: fin_domain::binding_hash(&proof).unwrap(),
            })
        }
        _ => None,
    };
    CompoundChildSettlement {
        child_tag: link.child_tag,
        original_id: tag.into(),
        expected_original: store.get_record(tag).unwrap().unwrap(),
        expected_original_reservations: store.operation_reservations(tag).unwrap(),
        reservations: rows(
            tag,
            if revision.is_some() {
                "110000000000000000000000001"
            } else {
                "100000000000000000000000001"
            },
        ),
        revision,
        proof,
    }
}
fn cumulative(children: &[CompoundChildSettlement]) -> (Value, Value) {
    let effects = serde_json::to_value(vec![FinancialEffect::CompoundSubmissionMutations {
        children: children
            .iter()
            .map(|child| serde_json::from_value(child.proof.clone()).unwrap())
            .collect(),
    }])
    .unwrap();
    (
        json!({"effects":effects,"proof_digest":"SYNTHETIC-cumulative-proof","evidence":{"proofEvents":[]}}),
        effects,
    )
}
fn pending(record: &Value) -> Value {
    let mut record = record.clone();
    record["status"] = json!("pending");
    record
}
fn remaining(store: &Store, id: &str) -> String {
    store
        .conn
        .query_row(
            "SELECT units FROM reservations WHERE operation_id=?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn partial_children_survive_restart_and_duplicate_progress_never_rewinds_original() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("compound.sqlite");
    let mut store = Store::open(&path).unwrap();
    let submitted = reserved(&mut store, false);
    let a = child(
        &store,
        "a",
        ChildMutationOutcome::Accepted { successor: None },
    );
    let (proof, effects) = cumulative(std::slice::from_ref(&a));
    let record = pending(&submitted);
    store
        .commit_compound_progress(
            "batch",
            &submitted,
            &record,
            &proof,
            &effects,
            std::slice::from_ref(&a),
            "t3",
        )
        .unwrap();
    assert_eq!(store.amendment_revisions("a").unwrap().len(), 1);
    assert_eq!(store.reservation_count("batch").unwrap(), 2);
    assert_eq!(remaining(&store, "b"), "100000000000000000000000001");
    store
        .conn
        .execute(
            "UPDATE reservations SET units='17' WHERE operation_id='a'",
            [],
        )
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    store
        .commit_compound_progress(
            "batch",
            &record,
            &record,
            &proof,
            &effects,
            std::slice::from_ref(&a),
            "t4",
        )
        .unwrap();
    assert_eq!(remaining(&store, "a"), "17");
    assert_eq!(store.amendment_revisions("a").unwrap().len(), 1);
    let b = child(
        &store,
        "b",
        ChildMutationOutcome::Accepted { successor: None },
    );
    let (proof, effects) = cumulative(&[a, b.clone()]);
    let mut completed = record.clone();
    completed["status"] = json!("completed");
    store
        .commit_compound_settlement(
            &Settlement {
                operation_id: "batch".into(),
                terminal_record: completed,
                proof,
                effects,
                run_id: None,
                run_record: None,
            },
            &record,
            &[b],
            "t5",
        )
        .unwrap();
    assert_eq!(store.reservation_count("batch").unwrap(), 0);
    assert_eq!(remaining(&store, "a"), "17");
    assert_eq!(remaining(&store, "b"), "100000000000000000000000001");
    assert_eq!(
        store.get_record("b").unwrap().unwrap()["status"],
        "submitted"
    );
    assert!(store.amendment_revisions("b").unwrap().is_empty());
}

#[test]
fn child_revision_and_proof_failure_roll_back_every_table_and_original_reserve() {
    let mut store = Store::in_memory().unwrap();
    let submitted = reserved(&mut store, false);
    let a = child(
        &store,
        "a",
        ChildMutationOutcome::Accepted { successor: None },
    );
    let (proof, effects) = cumulative(std::slice::from_ref(&a));
    store.conn.execute_batch("CREATE TRIGGER fail_compound_proof BEFORE INSERT ON proof_records BEGIN SELECT RAISE(ABORT,'synthetic proof failure'); END;").unwrap();
    assert!(
        store
            .commit_compound_progress(
                "batch",
                &submitted,
                &pending(&submitted),
                &proof,
                &effects,
                &[a],
                "t3"
            )
            .is_err()
    );
    assert_eq!(store.get_record("batch").unwrap(), Some(submitted));
    assert!(store.compound_child_proofs("batch").unwrap().is_empty());
    assert!(store.amendment_revisions("a").unwrap().is_empty());
    assert_eq!(remaining(&store, "a"), "100000000000000000000000001");
    assert_eq!(
        store
            .conn
            .query_row("SELECT COUNT(*) FROM effect_ledger", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn missing_child_blocks_terminal_commit_and_rejected_child_preserves_old_owner() {
    let mut store = Store::in_memory().unwrap();
    let submitted = reserved(&mut store, false);
    let a = child(
        &store,
        "a",
        ChildMutationOutcome::Accepted { successor: None },
    );
    let (proof, effects) = cumulative(std::slice::from_ref(&a));
    let mut completed = submitted.clone();
    completed["status"] = json!("completed");
    assert!(
        store
            .commit_compound_settlement(
                &Settlement {
                    operation_id: "batch".into(),
                    terminal_record: completed.clone(),
                    proof,
                    effects,
                    run_id: None,
                    run_record: None
                },
                &submitted,
                std::slice::from_ref(&a),
                "t3"
            )
            .is_err()
    );
    assert!(store.amendment_revisions("a").unwrap().is_empty());
    assert!(store.compound_child_proofs("batch").unwrap().is_empty());
    let b = child(
        &store,
        "b",
        ChildMutationOutcome::Rejected {
            code: "SYNTHETIC-ORDER-CLOSED".into(),
        },
    );
    let original_b = b.expected_original.clone();
    let (proof, effects) = cumulative(&[a.clone(), b.clone()]);
    store
        .commit_compound_settlement(
            &Settlement {
                operation_id: "batch".into(),
                terminal_record: completed,
                proof,
                effects,
                run_id: None,
                run_record: None,
            },
            &submitted,
            &[a, b],
            "t4",
        )
        .unwrap();
    assert_eq!(store.get_record("b").unwrap(), Some(original_b));
    assert_eq!(remaining(&store, "b"), "100000000000000000000000001");
    assert_eq!(store.compound_child_proofs("batch").unwrap().len(), 2);
}

#[test]
fn wrong_or_changed_child_scope_proof_and_original_race_never_revise_authority() {
    for failure in [
        "tag",
        "scope",
        "submission",
        "digest",
        "original",
        "original_reservations",
        "cumulative",
        "preview",
    ] {
        let mut store = Store::in_memory().unwrap();
        let submitted = reserved(&mut store, false);
        let mut a = child(
            &store,
            "a",
            ChildMutationOutcome::Accepted { successor: None },
        );
        let mut record = pending(&submitted);
        match failure {
            "tag" => a.child_tag = "SYNTHETIC-unknown".into(),
            "scope" => a.revision.as_mut().unwrap().scope.binding = json!({"different":true}),
            "submission" => {
                a.revision.as_mut().unwrap().amendment_submission.reference =
                    "SYNTHETIC-other".into()
            }
            "digest" => a.revision.as_mut().unwrap().proof_digest = "SYNTHETIC-wrong-proof".into(),
            "original" => {
                let mut original = a.expected_original.clone();
                original["status"] = json!("pending");
                store.update_record("a", &original, "t3").unwrap();
            }
            "original_reservations" => {
                store
                    .conn
                    .execute(
                        "UPDATE reservations SET units='19' WHERE operation_id='a'",
                        [],
                    )
                    .unwrap();
            }
            "preview" => record["preview"]["intent"]["locks"]
                .as_array_mut()
                .unwrap()
                .push(json!("SYNTHETIC-unauthorized")),
            _ => {}
        }
        let (mut proof, effects) = cumulative(std::slice::from_ref(&a));
        if failure == "cumulative" {
            proof["effects"] = json!([]);
        }
        assert!(
            store
                .commit_compound_progress(
                    "batch",
                    &submitted,
                    &record,
                    &proof,
                    &effects,
                    &[a],
                    "t4"
                )
                .is_err(),
            "{failure}"
        );
        assert!(store.amendment_revisions("a").unwrap().is_empty());
        assert!(store.compound_child_proofs("batch").unwrap().is_empty());
    }
    let mut store = Store::in_memory().unwrap();
    let submitted = reserved(&mut store, false);
    let a = child(
        &store,
        "a",
        ChildMutationOutcome::Accepted { successor: None },
    );
    let record = pending(&submitted);
    let (proof, effects) = cumulative(std::slice::from_ref(&a));
    store
        .commit_compound_progress(
            "batch",
            &submitted,
            &record,
            &proof,
            &effects,
            std::slice::from_ref(&a),
            "t3",
        )
        .unwrap();
    let mut changed = a;
    changed.proof["native_evidence"] = json!({"different":true});
    let (proof, effects) = cumulative(std::slice::from_ref(&changed));
    assert!(
        store
            .commit_compound_progress(
                "batch",
                &record,
                &record,
                &proof,
                &effects,
                &[changed],
                "t4"
            )
            .is_err()
    );
    assert_eq!(store.amendment_revisions("a").unwrap().len(), 1);
}

#[test]
fn pending_single_or_compound_links_block_shared_original_even_without_lock_rows() {
    for compound in [false, true] {
        let mut store = Store::in_memory().unwrap();
        let record = prepared(&mut store, false);
        let expected = if compound {
            record["preview"]["intent"]["expected"].clone()
        } else {
            let link = links(&record).unwrap().remove(0);
            json!([EffectConstraint::SubmissionAmendment {
                operation_id: link.operation_id,
                submission: link.submission,
                revision: match link.mutation {
                    ChildMutation::Amend { revision } => revision,
                    _ => unreachable!(),
                }
            }])
        };
        let conflicting = json!({"id":"other-mutation","request_id":"SYNTHETIC-other-request","fingerprint":"SYNTHETIC-other-fingerprint","status":"awaiting_approval","run_id":null,"submission":null,"preview":{"intent":{"locks":["SYNTHETIC-a"],"expected":expected}}});
        store
            .create_request(
                "other-mutation",
                "SYNTHETIC-other-request",
                "SYNTHETIC-other-fingerprint",
                "SYNTHETIC.amend",
                None,
                "awaiting_approval",
                &conflicting,
                "t1",
            )
            .unwrap();
        let snapshot = store.list_records().unwrap();
        assert!(matches!(
            store.persist_compound_amendment_reservation("batch", &[], &record, &snapshot, "t2"),
            Err(StoreError::ConcurrentReservation)
        ));
        assert_eq!(store.reservation_count("batch").unwrap(), 0);
    }
    let mut store = Store::in_memory().unwrap();
    let record = prepared(&mut store, false);
    let snapshot = store.list_records().unwrap();
    assert!(matches!(
        store.persist_compound_amendment_reservation("batch", &[], &record, &snapshot, "t2"),
        Err(StoreError::ReservationConflict)
    ));
}

#[test]
fn successor_identity_is_unique_across_native_children_and_old_single_revisions() {
    let mut store = Store::in_memory().unwrap();
    let submitted = reserved(&mut store, true);
    let successor = Some(submission("SYNTHETIC-shared-successor"));
    let a = child(
        &store,
        "a",
        ChildMutationOutcome::Accepted {
            successor: successor.clone(),
        },
    );
    let b = child(&store, "b", ChildMutationOutcome::Accepted { successor });
    let (proof, effects) = cumulative(&[a.clone(), b.clone()]);
    assert!(
        store
            .commit_compound_progress(
                "batch",
                &submitted,
                &pending(&submitted),
                &proof,
                &effects,
                &[a.clone(), b],
                "t3"
            )
            .is_err()
    );
    assert!(store.amendment_revisions("a").unwrap().is_empty());
    let (proof, effects) = cumulative(std::slice::from_ref(&a));
    let record = pending(&submitted);
    store
        .commit_compound_progress("batch", &submitted, &record, &proof, &effects, &[a], "t4")
        .unwrap();
    let b = child(
        &store,
        "b",
        ChildMutationOutcome::Accepted {
            successor: Some(submission("SYNTHETIC-other-successor")),
        },
    );
    store.conn.execute("INSERT INTO operation_amendments VALUES('b',1,'a',?1,'{}','SYNTHETIC-MUTATION-ONLY','SYNTHETIC-other-successor')",[serde_json::to_string(b.revision.as_ref().unwrap()).unwrap()]).unwrap();
    let mut previous = store.compound_child_proofs("batch").unwrap();
    previous.push(b.proof.clone());
    let effects = json!([{"kind":"compound_submission_mutations","children":previous}]);
    let proof = json!({"effects":effects});
    assert!(
        store
            .commit_compound_progress("batch", &record, &record, &proof, &effects, &[b], "t5")
            .is_err()
    );
}

#[test]
fn declared_child_linkage_reserves_only_proven_original_owners_and_allows_new_locks() {
    for failure in [
        "duplicate_tag",
        "missing_lock",
        "wrong_submission",
        "unrelated_owner",
    ] {
        let mut store = Store::in_memory().unwrap();
        let mut record = prepared(&mut store, false);
        match failure {
            "duplicate_tag" => {
                record["preview"]["intent"]["expected"][0]["children"][1]["child_tag"] =
                    json!("SYNTHETIC-child-a")
            }
            "missing_lock" => record["preview"]["intent"]["locks"] = json!(["SYNTHETIC-a"]),
            "wrong_submission" => {
                record["preview"]["intent"]["expected"][0]["children"][1]["submission"]["reference"] =
                    json!("SYNTHETIC-other")
            }
            "unrelated_owner" => {
                original(&mut store, "other");
                store
                    .conn
                    .execute(
                        "INSERT INTO reservations VALUES('other','lock:SYNTHETIC-a','1','0')",
                        [],
                    )
                    .unwrap();
            }
            _ => {}
        }
        let snapshot = store.list_records().unwrap();
        let mut reservations = rows("a", "0");
        reservations.extend(rows("b", "0"));
        assert!(
            store
                .persist_compound_amendment_reservation(
                    "batch",
                    &reservations,
                    &record,
                    &snapshot,
                    "t2"
                )
                .is_err(),
            "{failure}"
        );
        assert_eq!(store.reservation_count("batch").unwrap(), 0);
        assert!(store.compound_child_proofs("batch").unwrap().is_empty());
    }
    let mut store = Store::in_memory().unwrap();
    let mut record = prepared(&mut store, false);
    record["preview"]["intent"]["locks"]
        .as_array_mut()
        .unwrap()
        .push(json!("SYNTHETIC-new"));
    record["preview"]["intent"]["expected"]
        .as_array_mut()
        .unwrap()
        .push(
            serde_json::to_value(EffectConstraint::AuthorityTransition {
                resource: ResourceId("SYNTHETIC-new".into()),
                authority: "SYNTHETIC-new-order".into(),
                before: "absent".into(),
                allowed_after: vec!["accepted".into()],
            })
            .unwrap(),
        );
    let snapshot = store.list_records().unwrap();
    let mut reservations = rows("a", "0");
    reservations.extend(rows("b", "0"));
    reservations.extend(rows("new", "0"));
    store
        .persist_compound_amendment_reservation("batch", &reservations, &record, &snapshot, "t2")
        .unwrap();
    assert_eq!(store.reservation_count("batch").unwrap(), 3);
}

#[test]
fn version_eleven_upgrade_preserves_original_json_and_old_revision_ordinal() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v11.sqlite");
    let mut store = Store::open(&path).unwrap();
    let original = original(&mut store, "a");
    let scope = AmendmentScope {
        intent: FinancialIntent {
            locks: vec![ResourceId("SYNTHETIC-a".into())],
            ..Default::default()
        },
        binding: json!({"SYNTHETIC-old-scope":true}),
        valuation: None,
    };
    let revision = ApprovedAmendment {
        amendment_operation_id: "a".into(),
        amendment_submission: submission("SYNTHETIC-old-amendment"),
        scope,
        successor: None,
        proof_digest: "SYNTHETIC-old-proof".into(),
    };
    store.conn.execute("INSERT INTO operation_amendments VALUES('a',1,'a',?1,'{\"synthetic\":true}',NULL,NULL)",[serde_json::to_string(&revision).unwrap()]).unwrap();
    store
        .conn
        .execute_batch("DROP TABLE IF EXISTS unresolved_cost_events; DROP TABLE IF EXISTS unresolved_cost_consumption; DROP TABLE IF EXISTS grouped_amendment_scopes; DROP TABLE IF EXISTS grouped_amendment_children; DROP TABLE IF EXISTS grouped_amendment_revisions; DROP TABLE IF EXISTS grouped_amendments; DROP TABLE compound_amendment_children; PRAGMA user_version=11;")
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.get_record("a").unwrap(), Some(original));
    assert_eq!(store.amendment_revisions("a").unwrap(), vec![revision]);
    assert_eq!(remaining(&store, "a"), "100000000000000000000000001");
    let tx = store.conn.transaction().unwrap();
    assert_eq!(amendments::next_revision_tx(&tx, "a").unwrap(), 2);
    tx.commit().unwrap();
    assert_eq!(
        store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
}
