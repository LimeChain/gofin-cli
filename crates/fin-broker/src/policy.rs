//! Policy mutation is called only by protected human enrollment, under LOCK_EX.
use crate::{BrokerConfigV2, Store};
use fin_discovery::{PolicySetArgs, PolicyView};
use fin_domain::{AuthorizationChallenge, FinError, OperationRecord, Result, binding_hash};
use serde_json::{Value, json};
use std::future::Future;

fn failure(code: &str, message: &str) -> FinError {
    FinError::new(
        code,
        "configuration",
        message,
        "Reconcile pending operations before running fin policy set in your own Terminal. No policy change was made.",
    )
}

fn require_idle(store: &Store) -> Result<()> {
    for value in store.list_records().map_err(|_| {
        failure(
            "STORE_UNAVAILABLE",
            "Operation state could not be inspected.",
        )
    })? {
        let record: OperationRecord = serde_json::from_value(value).map_err(|_| {
            failure(
                "STORE_INVALID_RECORD",
                "An operation record could not be inspected; preserve the journal.",
            )
        })?;
        if !record.status.terminal() {
            return Err(failure(
                "POLICY_HAS_PENDING_OPERATION",
                "Reconcile every nonterminal operation before changing limits.",
            ));
        }
    }
    Ok(())
}

pub(super) async fn update<A, F, S>(
    config: &BrokerConfigV2,
    store: &Store,
    args: &PolicySetArgs,
    approve: A,
    save: S,
) -> Result<Value>
where
    A: FnOnce(Value) -> F,
    F: Future<Output = Result<AuthorizationChallenge>>,
    S: FnOnce(&BrokerConfigV2) -> Result<()>,
{
    let (movement, allocation) = args.micros()?;
    require_idle(store)?;
    let mut candidate = config.clone();
    candidate.policy.max_movement_usd_micros = movement;
    candidate.policy.max_run_allocation_usd_micros = allocation;
    if let Some(enabled) = args.allow_unpriced_exchange_assets {
        candidate.policy.allow_unpriced_exchange_assets = enabled;
    }
    candidate.validate()?;
    let binding = json!({"action":"policy_set", "previous_policy":config.policy, "policy":candidate.policy,
        "configuration_hash":binding_hash(&serde_json::to_value(&candidate).map_err(|_| FinError::validation("Configuration encoding failed."))?)?,
        "financial_movement":false,"future_financial_authority":false});
    let approved = approve(binding.clone()).await?;
    // Caller holds the exclusive maintenance lock throughout approval and save.
    require_idle(store)?;
    // A slow audit or final journal check cannot extend the 90-second consent.
    if approved.purpose != "configuration" || approved.binding != binding {
        return Err(FinError::new(
            "INVALID_CHALLENGE",
            "configuration",
            "Policy approval does not bind these exact limits.",
            "Run fin policy set again in your own Terminal.",
        ));
    }
    fin_authorization::validate_challenge(&approved, &fin_domain::now()).map_err(|failure| {
        if failure.code == "AUTHORIZATION_EXPIRED" {
            fin_human_terminal::configuration_approval_expired("policy_set")
        } else {
            failure
        }
    })?;
    save(&candidate).map_err(|_| FinError::new("POLICY_SAVE_UNCERTAIN", "configuration", "Saving the policy did not confirm durability. Both limits may already be active.", "Run fin policy show after broker reload before deciding whether to retry. No financial write was performed."))?;
    let mut view = PolicyView::new(
        &candidate.policy.max_movement_usd_micros,
        &candidate.policy.max_run_allocation_usd_micros,
    )?;
    view.set_unpriced_policy(candidate.policy.allow_unpriced_exchange_assets);
    view.next_action = "Run fin policy show --json to verify. If the broker reports a configuration change, allow launchd to restart it and repeat policy show.".into();
    Ok(serde_json::to_value(view).expect("serializable policy view"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BrokerPolicy, NetworkConfig, atomic_replace};
    use std::{cell::Cell, collections::BTreeMap};
    fn synthetic_approval(binding: Value) -> AuthorizationChallenge {
        AuthorizationChallenge {
            purpose: "configuration".into(),
            operation_id: "configuration:SYNTHETIC-policy".into(),
            binding_hash: binding_hash(&binding).unwrap(),
            binding,
            summary: "Synthetic policy approval".into(),
            nonce: "a".repeat(64),
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339(),
        }
    }
    fn config() -> BrokerConfigV2 {
        BrokerConfigV2 {
            version: 2,
            approver_uid: 501,
            networks: vec![NetworkConfig::ethereum_mainnet()],
            deployments: vec![],
            connections: vec![],
            policy: BrokerPolicy::beta(),
            extensions: BTreeMap::new(),
        }
    }
    fn args() -> PolicySetArgs {
        PolicySetArgs {
            max_movement_usd: "1000.000001".into(),
            max_run_allocation_usd: "5000".into(),
            allow_unpriced_exchange_assets: None,
        }
    }
    #[tokio::test]
    async fn policy_update_is_exact_atomic_and_changes_only_two_limits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        let previous = config();
        let original = serde_json::to_vec(&previous).unwrap();
        std::fs::write(&path, &original).unwrap();
        let store = Store::in_memory().unwrap();
        let approved = Cell::new(false);
        let outcome = update(
            &previous,
            &store,
            &args(),
            |binding| {
                assert_eq!(std::fs::read(&path).unwrap(), original);
                assert_eq!(
                    binding["previous_policy"],
                    serde_json::to_value(&previous.policy).unwrap()
                );
                assert_eq!(binding["policy"]["max_movement_usd_micros"], "1000000001");
                approved.set(true);
                std::future::ready(Ok(synthetic_approval(binding)))
            },
            |candidate| {
                assert!(approved.get());
                atomic_replace(
                    directory.path(),
                    &path,
                    &serde_json::to_vec(candidate).unwrap(),
                )
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome["max_movement_usd"], "1000.000001");
        assert!(outcome.get("verification_rule").is_none());
        let saved: BrokerConfigV2 = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.policy.max_movement_usd_micros, "1000000001");
        assert_eq!(saved.policy.max_run_allocation_usd_micros, "5000000000");
        let mut expected = previous.clone();
        expected.policy = saved.policy.clone();
        assert_eq!(
            serde_json::to_value(expected).unwrap(),
            serde_json::to_value(&saved).unwrap()
        );
        assert_eq!(
            saved.policy.require_run_for_writes,
            previous.policy.require_run_for_writes
        );
        assert_eq!(
            saved.policy.preview_ttl_seconds,
            previous.policy.preview_ttl_seconds
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        // Rename failure leaves the existing target intact and removes staging.
        let blocked = directory.path().join("directory-target");
        std::fs::create_dir(&blocked).unwrap();
        assert!(atomic_replace(directory.path(), &blocked, b"candidate").is_err());
        assert!(blocked.is_dir());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    }
    #[tokio::test]
    async fn policy_zero_requires_approval_and_removes_only_personal_ceilings() {
        let input = PolicySetArgs {
            max_movement_usd: "0".into(),
            max_run_allocation_usd: "0.000000".into(),
            allow_unpriced_exchange_assets: None,
        };
        let approved = Cell::new(false);
        let result = update(
            &config(),
            &Store::in_memory().unwrap(),
            &input,
            |binding| {
                assert_eq!(binding["policy"]["max_movement_usd_micros"], "0");
                approved.set(true);
                std::future::ready(Ok(synthetic_approval(binding)))
            },
            |candidate| {
                assert!(approved.get());
                assert_eq!(candidate.policy.max_run_allocation_usd_micros, "0");
                assert!(!candidate.policy.require_run_for_writes);
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(result["max_movement_usd"], "0");
        assert_eq!(result["max_run_allocation_usd"], "0");
        let denied = update(
            &config(),
            &Store::in_memory().unwrap(),
            &input,
            |_| {
                std::future::ready(Err(FinError::new(
                    "AUTHORIZATION_DENIED",
                    "authorization",
                    "Synthetic denial",
                    "Retry yourself.",
                )))
            },
            |_| panic!("no unapproved unlimited policy"),
        )
        .await
        .unwrap_err();
        assert_eq!(denied.code, "AUTHORIZATION_DENIED");
    }

    #[tokio::test]
    async fn policy_save_failure_never_reports_success_and_preserves_atomic_pair() {
        for fail_after_rename in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("config.json");
            let previous = config();
            std::fs::write(&path, serde_json::to_vec(&previous).unwrap()).unwrap();
            let result = update(
                &previous,
                &Store::in_memory().unwrap(),
                &args(),
                |binding| std::future::ready(Ok(synthetic_approval(binding))),
                |candidate| {
                    if fail_after_rename {
                        atomic_replace(
                            directory.path(),
                            &path,
                            &serde_json::to_vec(candidate).unwrap(),
                        )?;
                    }
                    Err(FinError::validation(
                        "Synthetic disk synchronization failure",
                    ))
                },
            )
            .await
            .unwrap_err();
            assert_eq!(result.code, "POLICY_SAVE_UNCERTAIN");
            let observed: BrokerConfigV2 =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let expected = if fail_after_rename {
                ("1000000001", "5000000000")
            } else {
                ("100000000", "100000000")
            };
            assert_eq!(
                (
                    observed.policy.max_movement_usd_micros.as_str(),
                    observed.policy.max_run_allocation_usd_micros.as_str()
                ),
                expected
            );
        }
    }

    #[tokio::test]
    async fn policy_approval_must_still_be_current_and_exact_at_commit() {
        for expired in [true, false] {
            let error = update(
                &config(),
                &Store::in_memory().unwrap(),
                &args(),
                |binding| {
                    let mut approved = synthetic_approval(binding);
                    if expired {
                        approved.expires_at =
                            (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
                    } else {
                        approved.binding["policy"]["max_movement_usd_micros"] = json!("2000000000");
                        approved.binding_hash = binding_hash(&approved.binding).unwrap();
                    }
                    std::future::ready(Ok(approved))
                },
                |_| panic!("no save after expired or mismatched approval"),
            )
            .await
            .unwrap_err();
            assert_eq!(
                error.code,
                if expired {
                    "AUTHORIZATION_EXPIRED"
                } else {
                    "INVALID_CHALLENGE"
                }
            );
        }
    }

    #[tokio::test]
    async fn policy_denial_expiry_and_audit_failure_never_save() {
        let previous = config();
        let original = serde_json::to_value(&previous).unwrap();
        for code in [
            "AUTHORIZATION_DENIED",
            "AUTHORIZATION_EXPIRED",
            "HUMAN_TERMINAL_REQUIRED",
            "CONFIG_AUDIT_FAILED",
        ] {
            let error = update(
                &previous,
                &Store::in_memory().unwrap(),
                &args(),
                |_| {
                    std::future::ready(Err(FinError::new(
                        code,
                        "authorization",
                        "Synthetic refusal",
                        "Retry yourself.",
                    )))
                },
                |_| panic!("must not save"),
            )
            .await
            .unwrap_err();
            assert_eq!(error.code, code);
            assert_eq!(serde_json::to_value(&previous).unwrap(), original);
        }
    }
    #[tokio::test]
    async fn policy_rejects_invalid_input_before_approval_and_keeps_both_limits() {
        for value in ["-1", "1e3", "18446744073709.551616", "1.0000001"] {
            for field in [true, false] {
                let mut input = args();
                if field {
                    input.max_movement_usd = value.into();
                } else {
                    input.max_run_allocation_usd = value.into();
                }
                let error = update(
                    &config(),
                    &Store::in_memory().unwrap(),
                    &input,
                    |_| -> std::future::Ready<Result<AuthorizationChallenge>> {
                        panic!("no approval for invalid input")
                    },
                    |_| panic!("no save"),
                )
                .await
                .unwrap_err();
                assert_eq!(error.code, "INVALID_POLICY_AMOUNT");
            }
        }
        let mut saved = config();
        for value in ["-1", "18446744073709551616"] {
            saved.policy.max_movement_usd_micros = value.into();
            assert_eq!(saved.validate().unwrap_err().code, "CONFIG_POLICY_INVALID");
        }
    }
    #[tokio::test]
    async fn policy_active_and_cleanup_runs_preserve_records_and_reservations() {
        for status in ["active", "cleanup_outstanding"] {
            let mut store = Store::in_memory().unwrap();
            let run = json!({"id":"synthetic-run","status":status,"reservations":{"synthetic-operation":{"maximum_fee_usd_micros":"99"}}});
            store
                .create_run("synthetic-run", &run, &fin_domain::now())
                .unwrap();
            update(
                &config(),
                &store,
                &args(),
                |binding| async move { Ok(synthetic_approval(binding)) },
                |_| Ok(()),
            )
            .await
            .unwrap();
            assert_eq!(store.get_run("synthetic-run").unwrap().unwrap(), run);
        }
    }
    #[tokio::test]
    async fn policy_pending_operations_are_not_discarded_or_replayed() {
        for status in ["awaiting_approval", "submitted", "pending", "unknown"] {
            let mut store = Store::in_memory().unwrap();
            let record = json!({"id":"synthetic-operation","request_id":"synthetic-request","fingerprint":"synthetic-fingerprint","fingerprint_version":1,"operation":"synthetic.transfer","input":{},"status":status,"created_at":fin_domain::now(),"updated_at":fin_domain::now(),"preview":null,"approval":null,"submission":null,"effects":null,"last_reconciliation_evidence":null,"error":null,"run_id":null});
            store
                .create_request(
                    "synthetic-operation",
                    "synthetic-request",
                    "synthetic-fingerprint",
                    "synthetic.transfer",
                    None,
                    status,
                    &record,
                    &fin_domain::now(),
                )
                .unwrap();
            let error = update(
                &config(),
                &store,
                &args(),
                |_| -> std::future::Ready<Result<AuthorizationChallenge>> {
                    panic!("must refuse before approval")
                },
                |_| panic!("no save"),
            )
            .await
            .unwrap_err();
            assert_eq!(error.code, "POLICY_HAS_PENDING_OPERATION");
            assert_eq!(store.list_records().unwrap(), vec![record]);
        }
    }
    #[test]
    fn policy_set_is_not_an_agent_rpc_method_and_fields_cannot_change_authority() {
        assert!(serde_json::from_value::<crate::BrokerRequest>(json!({"method":"policy_set","max_movement_usd":"1000","max_run_allocation_usd":"5000"})).is_err());
        let mut request = serde_json::to_value(args()).unwrap();
        for key in [
            "approval",
            "require_run_for_writes",
            "verification_limit",
            "unlimited",
        ] {
            request[key] = json!(false);
            assert!(serde_json::from_value::<PolicySetArgs>(request.clone()).is_err());
            request.as_object_mut().unwrap().remove(key);
        }
    }
    #[tokio::test]
    async fn unpriced_exception_defaults_closed_preserves_omission_and_binds_approval() {
        let mut previous = config();
        assert!(!previous.policy.allow_unpriced_exchange_assets);
        let mut old = serde_json::to_value(&previous).unwrap();
        old["policy"]
            .as_object_mut()
            .unwrap()
            .remove("allow_unpriced_exchange_assets");
        assert!(
            !serde_json::from_value::<BrokerConfigV2>(old)
                .unwrap()
                .policy
                .allow_unpriced_exchange_assets
        );
        for requested in [Some(true), None, Some(false)] {
            let expected = requested.unwrap_or(previous.policy.allow_unpriced_exchange_assets);
            let mut input = args();
            input.allow_unpriced_exchange_assets = requested;
            let result = update(
                &previous,
                &Store::in_memory().unwrap(),
                &input,
                |binding| {
                    assert_eq!(
                        binding["policy"]["allow_unpriced_exchange_assets"],
                        expected
                    );
                    assert_eq!(
                        binding["previous_policy"]["allow_unpriced_exchange_assets"],
                        previous.policy.allow_unpriced_exchange_assets
                    );
                    std::future::ready(Ok(synthetic_approval(binding)))
                },
                |saved| {
                    assert_eq!(saved.policy.allow_unpriced_exchange_assets, expected);
                    Ok(())
                },
            )
            .await
            .unwrap();
            assert_eq!(result["allow_unpriced_exchange_assets"], expected);
            assert_eq!(result["unpriced_assets_warning"].is_string(), expected);
            previous.policy.allow_unpriced_exchange_assets = expected;
        }
        let mut input = args();
        input.allow_unpriced_exchange_assets = Some(true);
        let refusal = update(
            &previous,
            &Store::in_memory().unwrap(),
            &input,
            |binding| {
                let mut approved = synthetic_approval(binding);
                approved.binding["policy"]["allow_unpriced_exchange_assets"] = json!(false);
                approved.binding_hash = binding_hash(&approved.binding).unwrap();
                std::future::ready(Ok(approved))
            },
            |_| panic!("exception requires exact protected approval"),
        )
        .await
        .unwrap_err();
        assert_eq!(refusal.code, "INVALID_CHALLENGE");
    }
}
