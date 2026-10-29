//! A biometric decision may release exact account holds, never imply settlement.
use super::*;

fn rejected(message: &str) -> GoFinError {
    GoFinError::new(
        "RESERVATION_OVERRIDE_UNAVAILABLE",
        "reservation",
        message,
        "Inspect the original operation and reconcile it when evidence is available. The original submission may still execute.",
    )
}

impl Engine {
    pub fn preview_operation_lock_override(&self, operation_id: &str) -> Result<Value> {
        let store = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?;
        let raw = store
            .get_record(operation_id)
            .map_err(map_store)?
            .ok_or_else(|| rejected("The original operation does not exist."))?;
        let record: OperationRecord = from_value(raw.clone())?;
        if !matches!(record.status, Status::Unknown | Status::Pending) {
            return Err(rejected(
                "Only an unresolved, previously submitted operation can release account exclusivity.",
            ));
        }
        let submitted = store
            .submission(operation_id)
            .map_err(map_store)?
            .ok_or_else(|| rejected("The original durable submission identity is missing."))?;
        let submission =
            json!({"kind":submitted.kind,"reference":submitted.reference,"data":submitted.data});
        if to_value(&record.submission)? != submission {
            return Err(rejected(
                "The journal and immutable submission identities do not agree.",
            ));
        }
        let prepared = record
            .preview
            .as_ref()
            .ok_or_else(|| rejected("The original financial intent is unavailable."))?;
        let rows = store.reservations(operation_id).map_err(map_store)?;
        let reservations = Value::Array(rows.iter().map(|row| json!({"resource":row.resource,"units":row.units,"fee_units":row.fee_units})).collect());
        let overridden = store
            .operation_lock_overrides(operation_id)
            .map_err(map_store)?;
        // Derive cash account identities from typed amounts. Order, position,
        // nonce and protocol locks are never selected by string prefix guesses.
        let candidates: BTreeSet<_> =
            prepared
                .intent
                .debits
                .iter()
                .map(|debit| debit.resource.clone())
                .chain(prepared.intent.maximum_costs.iter().filter_map(
                    |cost| match &cost.location {
                        AccountingLocation::Cash { resource } => Some(resource.clone()),
                        AccountingLocation::Position { .. } => None,
                    },
                ))
                .collect();
        let resources: Vec<_> = candidates
            .into_iter()
            .filter(|resource| {
                !overridden.contains(&resource.0)
                    && rows.iter().any(|row| {
                        row.resource == format!("lock:{}", resource.0)
                            && row.units == "0"
                            && row.fee_units == "0"
                    })
            })
            .map(|resource| resource.0)
            .collect();
        if resources.is_empty() {
            return Err(rejected(
                "This operation has no remaining exclusive cash-account locks eligible for release.",
            ));
        }
        let mut release_keys = resources
            .iter()
            .map(|resource| format!("lock:{resource}"))
            .collect::<BTreeSet<_>>();
        let mut cash_identities = BTreeMap::new();
        for debit in &prepared.intent.debits {
            if resources.contains(&debit.resource.0) {
                let key = resource_asset_key(debit)?;
                release_keys.insert(key.clone());
                cash_identities.insert(key, (debit.resource.clone(), debit.amount.asset.clone()));
            }
        }
        for cost in &prepared.intent.maximum_costs {
            if let AccountingLocation::Cash { resource } = &cost.location
                && resources.contains(&resource.0)
            {
                for key in [
                    resource_asset_key(&ResourceAmount {
                        resource: resource.clone(),
                        amount: cost.amount.clone(),
                    })?,
                    cost_key(&cost.location, &cost.amount.asset, &cost.treatment)?,
                ] {
                    release_keys.insert(key.clone());
                    cash_identities.insert(key, (resource.clone(), cost.amount.asset.clone()));
                }
            }
        }
        let (released, remaining): (Vec<_>, Vec<_>) = reservations
            .as_array()
            .unwrap()
            .iter()
            .cloned()
            .partition(|row| {
                row["resource"]
                    .as_str()
                    .is_some_and(|key| release_keys.contains(key))
            });
        let mut cash = BTreeMap::<(ResourceId, gofin_domain::AssetId), BigUint>::new();
        for row in &rows {
            if let Some(identity) = cash_identities.get(&row.resource) {
                *cash.entry(identity.clone()).or_default() +=
                    parse_uint(&row.units)? + parse_uint(&row.fee_units)?;
            }
        }
        let released_cash = cash.into_iter().filter(|(_,amount)| !amount.is_zero()).map(|((resource,asset),amount)|json!({"resource":resource,"asset":asset,"amount_atoms":amount.to_string()})).collect::<Vec<_>>();
        let review = json!({
            "version":1,
            "operation_id":record.id,
            "operation":record.operation,
            "original_request_id":record.request_id,
            "fingerprint":record.fingerprint,
            "submission":submission,
            "resources":resources,
            "record_hash":binding_hash(&raw)?,
            "reservations_hash":binding_hash(&reservations)?,
            "released_reservations":released,
            "released_cash":released_cash,
            "remaining_reservations":remaining,
            "warning":"The original submission may still execute. Releasing these exact account, principal and fee holds allows the same funds to be reused and may create double spending or overlapping financial actions. Original history, rolling allocation charges and plan usage remain unchanged until verified settlement. This approval authorizes no new financial write."
        });
        Ok(json!({"review_hash":binding_hash(&review)?,"review":review}))
    }

    pub async fn override_operation_locks(
        &self,
        operation_id: &str,
        request_id: &str,
        expected_review_hash: &str,
    ) -> Result<Value> {
        if request_id.trim().is_empty() || request_id.len() > 256 || expected_review_hash.is_empty()
        {
            return Err(rejected(
                "An exact override request identity and reviewed hash are required.",
            ));
        }
        let _guard = self.write_queue.lock().await;
        {
            let store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            if let Some(existing) = store
                .operation_lock_override(request_id)
                .map_err(map_store)?
            {
                if existing["operation_id"] != operation_id
                    || existing["review_hash"] != expected_review_hash
                {
                    return Err(GoFinError::new(
                        "REQUEST_ID_CONFLICT",
                        "reservation",
                        "This override request identity already names a different review.",
                        "Use the original request identity only for its exact approved review.",
                    ));
                }
                return Ok(existing);
            }
        }
        let preview = self.preview_operation_lock_override(operation_id)?;
        if preview["review_hash"] != expected_review_hash {
            return Err(rejected(
                "The original operation or reservation scope changed after review. Preview it again before requesting approval.",
            ));
        }
        let review = &preview["review"];
        let binding = json!({"action":"reservation_override","version":1,"request_id":request_id,"review_hash":expected_review_hash,"review":review});
        let challenge = AuthorizationChallenge {
            review: None,
            purpose: "reservation_override".into(),
            operation_id: operation_id.into(),
            binding_hash: binding_hash(&binding)?,
            binding,
            summary: format!(
                "Release reviewed account and cash reservations for {}. The original submission may still execute and the same funds may be reused; historical risk and plan usage remain charged.",
                record_label(review)
            ),
            nonce: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
            expires_at: (self.clock.now() + chrono::Duration::seconds(90)).to_rfc3339(),
        };
        gofin_domain::execution_progress::emit(
            gofin_domain::execution_progress::Stage::AwaitingApproval,
        );
        let audit = self.authorizer.authorize(&challenge).await?;
        validate_approval(&audit, &challenge, self.clock.now())?;
        if audit.source != Some(gofin_domain::AuthorizationSource::TouchId)
            || audit.yolo_provenance.is_some()
        {
            return Err(rejected(
                "Releasing an unresolved account lock requires fresh Touch ID for this exact review.",
            ));
        }
        self.store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .persist_operation_lock_override(
                request_id,
                review,
                &to_value(&audit)?,
                &self.clock.now().to_rfc3339(),
            )
            .map_err(map_store)
    }
}

fn record_label(review: &Value) -> &str {
    review["operation"]
        .as_str()
        .unwrap_or("the original operation")
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Approver {
        calls: AtomicUsize,
        source: gofin_domain::AuthorizationSource,
        mutate: Option<SharedStore>,
    }
    #[async_trait]
    impl gofin_domain::Authorizer for Approver {
        async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
            assert_eq!(challenge.purpose, "reservation_override");
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(store) = &self.mutate {
                let mut store = store.lock().unwrap();
                let mut record = store.get_record("original").unwrap().unwrap();
                record["updated_at"] = json!("2026-01-01T00:00:01Z");
                store
                    .update_record("original", &record, "2026-01-01T00:00:01Z")
                    .unwrap();
            }
            Ok(ApprovalAudit {
                source: Some(self.source.clone()),
                yolo_provenance: None,
                mechanism: "synthetic-biometric-fixture".into(),
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: now(),
                expires_at: challenge.expires_at.clone(),
            })
        }
    }

    pub(crate) fn seeded_store() -> SharedStore {
        let mut store = Store::in_memory().unwrap();
        let mut record = seed_record("original");
        store
            .create_request(
                "original",
                "original-request",
                "original-fingerprint",
                "synthetic.transfer",
                None,
                "awaiting_approval",
                &record,
                &now(),
            )
            .unwrap();
        let rows = vec![
            ReservationRow {
                resource: "lock:synthetic:account".into(),
                units: "0".into(),
                fee_units: "0".into(),
            },
            ReservationRow {
                resource: "lock:synthetic:order".into(),
                units: "0".into(),
                fee_units: "0".into(),
            },
            ReservationRow {
                resource: cash_key(),
                units: "10".into(),
                fee_units: "2".into(),
            },
        ];
        store.reserve("original", &rows).unwrap();
        store
            .persist_submission(
                "original",
                &SubmissionRow {
                    kind: "synthetic".into(),
                    reference: "immutable-identity".into(),
                    data: json!({"nonce":17}),
                },
                &now(),
            )
            .unwrap();
        record = store.get_record("original").unwrap().unwrap();
        record["status"] = json!("unknown");
        store.update_record("original", &record, &now()).unwrap();
        Arc::new(Mutex::new(store))
    }

    fn cash_key() -> String {
        resource_asset_key(&ResourceAmount {
            resource: ResourceId("synthetic:account".into()),
            amount: Amount {
                asset: gofin_domain::AssetId {
                    network: "synthetic".into(),
                    reference: "COIN".into(),
                    decimals: 0,
                },
                atoms: gofin_domain::Quantity::from_u64(10),
            },
        })
        .unwrap()
    }

    pub(crate) fn seed_record(id: &str) -> Value {
        let now = now();
        json!({
            "id":id,"request_id":format!("{id}-request"),"fingerprint":format!("{id}-fingerprint"),"fingerprint_version":1,
            "operation":"synthetic.transfer","input":{},"status":"awaiting_approval","created_at":now,"updated_at":now,
            "preview":{
                "operation":"synthetic.transfer","input":{},"binding":{},"summary":"Synthetic transfer",
                "checks":[],"warnings":[],"observed_at":now,"expires_at":(Utc::now()+chrono::Duration::seconds(90)).to_rfc3339(),
                "valuation":{"movement_usd_micros":"10000000","maximum_fee_usd_micros":"2000000","additional_allocation_usd_micros":"0","observed_at":now,"source":"synthetic","prices":[]},
                "intent":{"debits":[{"resource":"synthetic:account","amount":{"asset":{"network":"synthetic","reference":"COIN","decimals":0},"atoms":"10"}}],
                    "maximum_costs":[{"location":{"kind":"cash","resource":"synthetic:account"},"amount":{"asset":{"network":"synthetic","reference":"COIN","decimals":0},"atoms":"2"},"treatment":"additional_debit"}],
                    "expected":[],"locks":["synthetic:account","synthetic:order"]},
                "operation_version":1
            },
            "approval":null,"submission":null,"effects":null,"last_reconciliation_evidence":null,"error":null,"run_id":null
        })
    }

    fn engine(
        store: SharedStore,
        source: gofin_domain::AuthorizationSource,
        mutate: bool,
    ) -> (Engine, Arc<Approver>) {
        let authorizer = Arc::new(Approver {
            calls: AtomicUsize::new(0),
            source,
            mutate: mutate.then(|| store.clone()),
        });
        (
            Engine::new_with_shared_store(
                store,
                vec![],
                authorizer.clone(),
                EngineOptions::default(),
            )
            .unwrap(),
            authorizer,
        )
    }

    #[tokio::test]
    async fn override_is_biometric_idempotent_and_preserves_submission_risk_and_specific_locks() {
        let store = seeded_store();
        let (engine, approver) = engine(
            store.clone(),
            gofin_domain::AuthorizationSource::TouchId,
            false,
        );
        let before = engine.load_record("original").unwrap();
        let original_rows = store.lock().unwrap().reservations("original").unwrap();
        let charge = ordinary_policy::record_charge(&before).unwrap();
        let preview = engine.preview_operation_lock_override("original").unwrap();
        assert_eq!(preview["review"]["resources"], json!(["synthetic:account"]));
        assert_eq!(preview["review"]["released_cash"][0]["amount_atoms"], "12");
        assert_eq!(
            preview["review"]["released_reservations"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            preview["review"]["remaining_reservations"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let hash = preview["review_hash"].as_str().unwrap();
        let result = engine
            .override_operation_locks("original", "override-request", hash)
            .await
            .unwrap();
        assert_eq!(result["status"], "released");
        assert_eq!(
            engine
                .override_operation_locks("original", "override-request", hash)
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
            original_rows
        );
        assert_eq!(ordinary_policy::record_charge(&before).unwrap(), charge);
        assert_eq!(
            ordinary_policy::effective_conflict_resources(&store.lock().unwrap(), &before).unwrap(),
            BTreeSet::from([ResourceId("synthetic:order".into())])
        );
        assert!(engine.preview_operation_lock_override("original").is_err());
        assert!(
            engine
                .override_operation_locks("original", "override-request", "different")
                .await
                .is_err()
        );
        let reopened = Engine::new_with_shared_store(
            store.clone(),
            vec![],
            approver.clone(),
            EngineOptions::default(),
        )
        .unwrap();
        assert_eq!(
            reopened
                .override_operation_locks("original", "override-request", hash)
                .await
                .unwrap(),
            result
        );
        assert_eq!(approver.calls.load(Ordering::SeqCst), 1);

        // SQL claims reuse only the reviewed holds; the new hold must fit.
        let mut store = store.lock().unwrap();
        let candidate = seed_record("next");
        store
            .create_request(
                "next",
                "next-request",
                "next-fingerprint",
                "synthetic.transfer",
                None,
                "awaiting_approval",
                &candidate,
                &now(),
            )
            .unwrap();
        let snapshot = store.list_records().unwrap();
        let requested = vec![
            ReservationRow {
                resource: "lock:synthetic:account".into(),
                units: "0".into(),
                fee_units: "0".into(),
            },
            ReservationRow {
                resource: cash_key(),
                units: "3".into(),
                fee_units: "0".into(),
            },
        ];
        assert!(
            store
                .persist_operation_reservation_with_cash_capacities(
                    "next",
                    &requested,
                    &candidate,
                    &snapshot,
                    &now(),
                    &[CashReservationCapacity {
                        resource: cash_key(),
                        maximum_total_units: "2".into()
                    }]
                )
                .is_err()
        );
        store
            .persist_operation_reservation_with_cash_capacities(
                "next",
                &requested,
                &candidate,
                &snapshot,
                &now(),
                &[CashReservationCapacity {
                    resource: cash_key(),
                    maximum_total_units: "3".into(),
                }],
            )
            .unwrap();
        assert_eq!(store.reservations("original").unwrap(), original_rows);
        let unrelated = seed_record("unrelated");
        store
            .create_request(
                "unrelated",
                "unrelated-request",
                "unrelated-fingerprint",
                "synthetic.transfer",
                None,
                "awaiting_approval",
                &unrelated,
                &now(),
            )
            .unwrap();
        store
            .reserve(
                "unrelated",
                &[ReservationRow {
                    resource: cash_key(),
                    units: "4".into(),
                    fee_units: "0".into(),
                }],
            )
            .unwrap();
        let snapshot = store.list_records().unwrap();
        assert!(
            store
                .persist_operation_reservation_with_cash_capacities(
                    "next",
                    &requested,
                    &candidate,
                    &snapshot,
                    &now(),
                    &[CashReservationCapacity {
                        resource: cash_key(),
                        maximum_total_units: "3".into()
                    }]
                )
                .is_err()
        );
        store
            .persist_operation_reservation_with_cash_capacities(
                "next",
                &requested,
                &candidate,
                &snapshot,
                &now(),
                &[CashReservationCapacity {
                    resource: cash_key(),
                    maximum_total_units: "7".into(),
                }],
            )
            .unwrap();
    }

    #[tokio::test]
    async fn override_rejects_nonbiometric_authority_stale_review_and_concurrent_change() {
        for source in [
            gofin_domain::AuthorizationSource::Yolo,
            gofin_domain::AuthorizationSource::Sudo,
            gofin_domain::AuthorizationSource::NamedPlan,
        ] {
            let store = seeded_store();
            let (engine, approver) = engine(store.clone(), source, false);
            assert!(
                engine
                    .override_operation_locks("original", "override", "stale")
                    .await
                    .is_err()
            );
            assert_eq!(approver.calls.load(Ordering::SeqCst), 0);
            let preview = engine.preview_operation_lock_override("original").unwrap();
            assert!(
                engine
                    .override_operation_locks(
                        "original",
                        "override",
                        preview["review_hash"].as_str().unwrap()
                    )
                    .await
                    .is_err()
            );
            assert!(
                store
                    .lock()
                    .unwrap()
                    .operation_lock_overrides("original")
                    .unwrap()
                    .is_empty()
            );
        }
        let store = seeded_store();
        let (engine, _) = engine(
            store.clone(),
            gofin_domain::AuthorizationSource::TouchId,
            true,
        );
        let preview = engine.preview_operation_lock_override("original").unwrap();
        assert!(
            engine
                .override_operation_locks(
                    "original",
                    "override",
                    preview["review_hash"].as_str().unwrap()
                )
                .await
                .is_err()
        );
        assert!(
            store
                .lock()
                .unwrap()
                .operation_lock_overrides("original")
                .unwrap()
                .is_empty()
        );
    }
}
