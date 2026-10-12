use crate::{profile::*, read_profile::native_error, types::InvalidateNonceInput, writes::*};
use async_trait::async_trait;
use fin_domain::*;
use fin_exchange::ccxt::{HyperliquidAction as Action, HyperliquidActionEnvelope as Envelope};
use fin_exchange::{ExchangeProfile, PlannedSubmission};
use serde_json::{Value, json};
use std::sync::Arc;

#[async_trait]
impl ExchangeProfile for HyperliquidProfile {
    type Request = ActionRequest;
    fn attach_execution_state(
        &self,
        state: Arc<dyn fin_domain::execution_state::ExecutionState>,
    ) -> Result<()> {
        self.nonces.attach(state)
    }
    fn definitions(&self) -> Vec<Definition> {
        crate::catalog::definitions()
            .into_iter()
            .filter(|d| !self.excluded_operations.contains(&d.name))
            .collect()
    }
    fn submission_kind(&self) -> &str {
        SUBMISSION_KIND
    }
    fn connections(&self) -> Vec<ConnectionMetadata> {
        self.accounts
            .iter()
            .map(HyperliquidAccount::metadata)
            .collect()
    }
    async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
        self.read_operation(operation, input).await
    }
    async fn snapshot(&self) -> Result<StateSnapshot> {
        self.financial_snapshot().await
    }
    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        let mut prepared = self.prepare_action(operation, input).await?;
        if prepared
            .valuation
            .as_ref()
            .is_some_and(|valuation| !valuation.prices.is_empty())
        {
            fin_domain::value_bounds::bind(&mut prepared);
        }
        Ok(prepared)
    }
    async fn refresh_prepared(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        let mut refreshed = self
            .prepare_action(&prepared.operation, &prepared.input)
            .await?;
        if prepared
            .binding
            .get(fin_domain::value_bounds::KEY)
            .is_some()
        {
            fin_domain::value_bounds::carry(prepared, &mut refreshed)?;
        }
        Ok(refreshed)
    }
    fn validate_recovery_child(
        &self,
        prepared: &PreparedOperation,
        parent: &OperationRecord,
    ) -> Result<Option<RecoveryBinding>> {
        if prepared.operation != "hyperliquid.account.invalidate_nonce" {
            return Ok(None);
        }
        let binding = ActionBinding::decode(&prepared.binding)?;
        let recovery = binding.recovery.clone().ok_or_else(|| {
            FinError::validation("The native nonce recovery has no durable parent link.")
        })?;
        let request: InvalidateNonceInput = serde_json::from_value(prepared.input.clone())
            .map_err(|_| FinError::validation("The nonce-recovery request is invalid."))?;
        let account = self.account(&request.account)?;
        let original = crate::writes::original_class_transfer(account, parent)?;
        let lock = ResourceId(format!(
            "hyperliquid:mainnet:nonce:{}:{}",
            account.owner.to_ascii_lowercase(),
            original.nonce_ms
        ));
        if recovery.version != 1
            || recovery.parent_operation_id != parent.id
            || recovery.parent_submission
                != parent.submission.clone().ok_or_else(|| {
                    FinError::validation("The original native submission is unavailable.")
                })?
            || request.original_operation_id != parent.id
            || binding.account != *account
            || binding.account_mode.as_ref().is_none_or(|mode| {
                mode.owner != account.owner
                    || mode.execution_address != account.owner
                    || mode.mode != crate::account_mode::AccountMode::Separate
                    || mode.dex_abstraction == Some(true)
            })
            || !matches!(binding.action, Action::Noop {})
            || !binding.orders.is_empty()
            || binding.replacement.is_some()
            || !prepared.intent.debits.is_empty()
            || !prepared.intent.maximum_costs.is_empty()
            || !prepared.intent.expected.is_empty()
            || prepared.intent.locks.as_slice() != [lock]
            || prepared.valuation.as_ref().is_none_or(|valuation| {
                !valuation.movement_usd_micros.is_zero()
                    || !valuation.maximum_fee_usd_micros.is_zero()
                    || !valuation.additional_allocation_usd_micros.is_zero()
            })
        {
            return Err(FinError::validation(
                "The recovery child does not match the exact owner, original submission, nonce lock and zero-effect intent.",
            ));
        }
        Ok(Some(recovery))
    }
    fn valuation_matches(&self, approved: &PreparedOperation, fresh: &PreparedOperation) -> bool {
        if approved
            .binding
            .get(fin_domain::value_bounds::KEY)
            .is_some()
        {
            return approved.binding.get(fin_domain::value_bounds::KEY)
                == fresh.binding.get(fin_domain::value_bounds::KEY)
                && fin_domain::value_bounds::validate(fresh).is_ok();
        }
        match (&approved.valuation, &fresh.valuation) {
            (Some(a), Some(b)) => {
                b.movement_usd_micros <= a.movement_usd_micros
                    && b.maximum_fee_usd_micros <= a.maximum_fee_usd_micros
                    && b.additional_allocation_usd_micros <= a.additional_allocation_usd_micros
            }
            (None, None) => true,
            _ => false,
        }
    }
    async fn plan_submission(
        &self,
        current: &PreparedOperation,
    ) -> Result<PlannedSubmission<ActionRequest>> {
        let binding = ActionBinding::decode(&current.binding)?;
        let account = self.account(&binding.account.name)?.clone();
        if account != binding.account {
            return Err(FinError::validation("Protected account identity changed."));
        }
        let current_mode = self.verified_account_mode(&account).await?;
        let frozen_mode = binding.account_mode.as_ref().ok_or_else(|| {
            FinError::validation(
                "This prepared write has no frozen native account-mode evidence; prepare it again before dispatch.",
            )
        })?;
        if frozen_mode != &current_mode {
            return Err(FinError::new(
                "HYPERLIQUID_ACCOUNT_MODE_CHANGED",
                "dispatch",
                "Hyperliquid account mode or signer role changed after preparation.",
                "Refresh account status and prepare a new write; the stored action was not dispatched.",
            ));
        }
        if matches!(
            current.operation.as_str(),
            "hyperliquid.orders.place"
                | "hyperliquid.orders.batch"
                | "hyperliquid.orders.edit"
                | "hyperliquid.orders.edit_batch"
                | "hyperliquid.orders.twap"
                | "hyperliquid.account.add_margin"
                | "hyperliquid.account.reduce_margin"
        ) {
            current_mode.ensure_supported_cash_accounting()?;
        }
        let (nonce, action) = if let Some(recovery) = &binding.recovery {
            if current.operation != "hyperliquid.account.invalidate_nonce"
                || !matches!(binding.action, Action::Noop {})
                || recovery.version != 1
            {
                return Err(FinError::validation(
                    "The durable nonce-recovery binding is incomplete or unsupported.",
                ));
            }
            let parent = self
                .nonces
                .operation(&recovery.parent_operation_id)?
                .ok_or_else(|| FinError::validation("The original transfer is unavailable."))?;
            if parent.id != recovery.parent_operation_id
                || parent.submission.as_ref() != Some(&recovery.parent_submission)
            {
                return Err(FinError::validation(
                    "The original transfer submission changed after nonce-recovery preparation.",
                ));
            }
            let original = crate::writes::original_class_transfer(&account, &parent)?;
            if !nonce_is_current(
                original.nonce_ms,
                chrono::Utc::now().timestamp_millis() as u64,
            ) {
                return Err(FinError::validation(
                    "The original nonce left Hyperliquid's native validity window before dispatch.",
                ));
            }
            (original.nonce_ms, Action::Noop {})
        } else {
            let nonce = self
                .nonces
                .allocate(&account.owner, chrono::Utc::now().timestamp_millis() as u64)?;
            let mut action = binding.action;
            match &mut action {
                Action::UsdClassTransfer { nonce: field, .. }
                | Action::ApproveBuilderFee { nonce: field, .. } => *field = nonce,
                Action::UsdSend { time_ms, .. } | Action::Withdraw { time_ms, .. } => {
                    *time_ms = nonce
                }
                _ => {}
            }
            (nonce, action)
        };
        let envelope = Envelope {
            nonce_ms: nonce,
            vault_address: account.vault_address().map(str::to_owned),
            action,
        };
        let wire = envelope
            .action_wire_json()
            .map_err(|_| native_error("The exact action cannot be encoded."))?;
        let reference = binding_hash(
            &json!({"version":1,"owner":account.owner,"execution_address":account.execution_address(),"vault_address":account.vault_address(),"operation":current.operation,"wire":wire}),
        )?;
        let submission = Submission {
            kind: SUBMISSION_KIND.into(),
            reference,
            data: json!({"owner":account.owner,"execution_address":account.execution_address(),"account":account.name,"envelope":envelope,"wire":wire,"operation":current.operation,"since_ms":nonce.saturating_sub(5000)}),
        };
        Ok(PlannedSubmission {
            submission: submission.clone(),
            request: ActionRequest {
                account,
                envelope,
                submission,
            },
        })
    }
    async fn dispatch(
        &self,
        request: &ActionRequest,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Value> {
        journal.assert_authorized()?;
        let response = match self
            .transport
            .dispatch(&request.account, &request.envelope, journal.clone())
            .await
        {
            Ok(response) => response,
            Err(error) if error.code == "HYPERLIQUID_PREDISPATCH_CREDENTIAL_FAILURE" => {
                let wire = request
                    .envelope
                    .action_wire_json()
                    .map_err(|_| native_error("Pre-dispatch action cannot be reconstructed."))?;
                journal.persist_dispatch_evidence(
                    &request.submission,
                    &json!({
                        "kind":"hyperliquid_pre_dispatch_failure",
                        "version":1,
                        "phase":"pre_dispatch",
                        "outcome":"not_submitted",
                        "reason":"protected_credential_lookup_failed",
                        "submission_reference":request.submission.reference,
                        "owner":request.account.owner,
                        "execution_address":request.account.execution_address(),
                        "envelope":request.envelope,
                        "wire":wire,
                        "error_code":error.code,
                    }),
                )?;
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        journal.persist_dispatch_evidence(&request.submission,&json!({"source":"Hyperliquid /exchange","submission_reference":request.submission.reference,"response":response}))?;
        Ok(response)
    }
    fn validate_response(&self, _submission: &Submission, response: &Value) -> Result<()> {
        if response["status"] == "ok" {
            Ok(())
        } else if response["status"] == "err"
            && response["response"]
                .as_str()
                .is_some_and(|message| !message.trim().is_empty())
            && response.as_object().is_some_and(|object| object.len() == 2)
        {
            Err(FinError::new(
                "HYPERLIQUID_ACTION_REJECTED",
                "submission",
                "Hyperliquid explicitly rejected the submitted action.",
                "Reconcile the original operation; do not submit it again.",
            ))
        } else {
            Err(FinError::new(
                "HYPERLIQUID_SUBMISSION_UNCERTAIN",
                "submission",
                "The venue did not return a validated action receipt.",
                "Reconcile the original operation; do not resubmit.",
            ))
        }
    }
    async fn reconcile_submission(
        &self,
        record: &OperationRecord,
        submission: &Submission,
    ) -> Result<Reconciliation> {
        self.reconcile_action(record, submission).await
    }
}

#[cfg(test)]
mod delegated_plan_tests {
    use super::*;
    use crate::writes::ActionBinding;
    use async_trait::async_trait;
    use fin_domain::AssetPrice;
    use fin_exchange::ExchangeProfile;
    use fin_exchange::ccxt::HyperliquidInfoRequest as Info;
    use std::sync::Mutex;

    const OWNER: &str = "0x1111111111111111111111111111111111111111";
    const SUB: &str = "0x2222222222222222222222222222222222222222";

    struct Transport {
        subaccounts: Value,
    }
    #[async_trait]
    impl HyperliquidTransport for Transport {
        async fn info(&self, request: fin_exchange::ccxt::HyperliquidInfoRequest) -> Result<Value> {
            match request {
                Info::SubAccounts { owner } if owner == OWNER => Ok(self.subaccounts.clone()),
                Info::UserRole { owner } if owner == OWNER => Ok(json!({"role":"user"})),
                Info::UserAbstractionState { owner } if owner == SUB => {
                    Ok(json!({"abstraction":"disabled"}))
                }
                Info::UserDexAbstractionState { owner } if owner == SUB => Ok(json!(false)),
                _ => Err(FinError::validation("unexpected synthetic info request")),
            }
        }
        async fn dispatch(
            &self,
            _: &HyperliquidAccount,
            _: &Envelope,
            _: Arc<dyn SubmissionJournal>,
        ) -> Result<Value> {
            Err(FinError::validation(
                "dispatch is not part of the planning test",
            ))
        }
    }
    struct Prices;
    #[async_trait]
    impl HyperliquidPrices for Prices {
        async fn usdc_price(&self) -> Result<AssetPrice> {
            Err(FinError::validation(
                "price is not part of the planning test",
            ))
        }
    }
    #[derive(Default)]
    struct Nonces(Mutex<Vec<String>>);
    impl HyperliquidNonces for Nonces {
        fn allocate(&self, owner: &str, _: u64) -> Result<u64> {
            self.0.lock().unwrap().push(owner.to_owned());
            Ok(77)
        }
    }

    #[derive(Default)]
    struct PersistedDispatchEvidence(Mutex<Option<(Submission, Value)>>);
    impl SubmissionJournal for PersistedDispatchEvidence {
        fn assert_authorized(&self) -> Result<()> {
            Ok(())
        }
        fn persist_submission(&self, _: &Submission) -> Result<()> {
            Ok(())
        }
        fn persist_dispatch_evidence(
            &self,
            submission: &Submission,
            evidence: &Value,
        ) -> Result<()> {
            *self.0.lock().unwrap() = Some((submission.clone(), evidence.clone()));
            Ok(())
        }
    }

    struct CredentialFailureTransport;
    #[async_trait]
    impl HyperliquidTransport for CredentialFailureTransport {
        async fn info(&self, _: fin_exchange::ccxt::HyperliquidInfoRequest) -> Result<Value> {
            Err(FinError::validation("info is not used in dispatch test"))
        }
        async fn dispatch(
            &self,
            _: &HyperliquidAccount,
            _: &Envelope,
            _: Arc<dyn SubmissionJournal>,
        ) -> Result<Value> {
            Err(FinError::new(
                "HYPERLIQUID_PREDISPATCH_CREDENTIAL_FAILURE",
                "pre-dispatch",
                "Protected credential lookup failed before venue dispatch.",
                "Repair the protected credential mapping, then reconcile this operation.",
            ))
        }
    }

    #[tokio::test]
    async fn delegated_submission_binds_execution_scope_but_allocates_master_nonce() {
        let account = HyperliquidAccount {
            name: "synthetic-subaccount".into(),
            owner: OWNER.into(),
            credential_ref: Some("synthetic-key-ref".into()),
            signing_enabled: true,
            execution_scope: HyperliquidExecutionScope::Subaccount {
                address: SUB.into(),
            },
        };
        let nonce_state = Arc::new(Nonces::default());
        let profile = HyperliquidProfile::new(
            vec![account.clone()],
            Arc::new(Transport {
                subaccounts: json!([{"master":OWNER,"subAccountUser":SUB}]),
            }),
            Arc::new(Prices),
            nonce_state.clone(),
        )
        .unwrap();
        let binding = ActionBinding {
            account,
            account_mode: Some(crate::account_mode::AccountModeState {
                owner: OWNER.into(),
                execution_address: SUB.into(),
                native_abstraction: "disabled".into(),
                mode: crate::account_mode::AccountMode::Separate,
                dex_abstraction: Some(false),
            }),
            action: Action::UpdateLeverage {
                asset: 0,
                leverage: 3,
                is_cross: false,
            },
            orders: vec![],
            authority_before: None,
            replacement: None,
            recovery: None,
            metadata: Default::default(),
        };
        let current = PreparedOperation {
            operation: "hyperliquid.account.set_leverage".into(),
            input: json!({"account":"synthetic-subaccount"}),
            binding: serde_json::to_value(binding).unwrap(),
            summary: "synthetic".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: fin_domain::now(),
            expires_at: fin_domain::now(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        };

        let planned = profile.plan_submission(&current).await.unwrap();
        assert_eq!(planned.request.envelope.vault_address.as_deref(), Some(SUB));
        assert_eq!(planned.request.envelope.nonce_ms, 77);
        assert_eq!(nonce_state.0.lock().unwrap().as_slice(), [OWNER]);
        assert_eq!(planned.submission.data["owner"], OWNER);
        assert_eq!(planned.submission.data["execution_address"], SUB);
        assert_eq!(planned.submission.data["envelope"]["vault_address"], SUB);

        let blocked_nonces = Arc::new(Nonces::default());
        let blocked = HyperliquidProfile::new(
            vec![HyperliquidAccount {
                name: "synthetic-subaccount".into(),
                owner: OWNER.into(),
                credential_ref: Some("synthetic-key-ref".into()),
                signing_enabled: true,
                execution_scope: HyperliquidExecutionScope::Subaccount {
                    address: SUB.into(),
                },
            }],
            Arc::new(Transport {
                subaccounts: json!([{"master":OWNER,"subAccountUser":OWNER}]),
            }),
            Arc::new(Prices),
            blocked_nonces.clone(),
        )
        .unwrap();
        assert!(blocked.plan_submission(&current).await.is_err());
        assert!(blocked_nonces.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn typed_local_signer_failure_is_persisted_against_exact_submission() {
        let account = HyperliquidAccount {
            name: "synthetic".into(),
            owner: OWNER.into(),
            credential_ref: None,
            signing_enabled: false,
            execution_scope: HyperliquidExecutionScope::Owner,
        };
        let profile = HyperliquidProfile::new(
            vec![account.clone()],
            Arc::new(CredentialFailureTransport),
            Arc::new(Prices),
            Arc::new(Nonces::default()),
        )
        .unwrap();
        let envelope = Envelope {
            nonce_ms: 77,
            vault_address: None,
            action: Action::UsdClassTransfer {
                amount_usdc: "1".into(),
                to_perp: true,
                nonce: 77,
            },
        };
        let wire = envelope.action_wire_json().unwrap();
        let submission = Submission {
            kind: SUBMISSION_KIND.into(),
            reference: "synthetic-exact-submission".into(),
            data: json!({
                "owner":OWNER,
                "execution_address":OWNER,
                "envelope":envelope,
                "wire":wire
            }),
        };
        let journal = Arc::new(PersistedDispatchEvidence::default());
        let request = ActionRequest {
            account: account.clone(),
            envelope: envelope.clone(),
            submission: submission.clone(),
        };

        let error = profile
            .dispatch(&request, journal.clone())
            .await
            .expect_err("local credential error must stop dispatch");
        assert_eq!(error.code, "HYPERLIQUID_PREDISPATCH_CREDENTIAL_FAILURE");
        let (persisted_submission, evidence) = journal
            .0
            .lock()
            .unwrap()
            .clone()
            .expect("pre-dispatch failure must be journaled");
        assert_eq!(persisted_submission, submission);
        assert_eq!(evidence["outcome"], "not_submitted");
        assert_eq!(evidence["submission_reference"], submission.reference);
        assert_eq!(evidence["owner"], account.owner);
        assert_eq!(
            evidence["envelope"],
            serde_json::to_value(envelope).unwrap()
        );
        assert_eq!(evidence["wire"], wire);
        assert!(evidence.get("response").is_none());
    }

    #[test]
    fn response_validation_distinguishes_exact_top_level_rejection_from_ambiguous_error() {
        let account = HyperliquidAccount {
            name: "synthetic".into(),
            owner: OWNER.into(),
            credential_ref: None,
            signing_enabled: false,
            execution_scope: HyperliquidExecutionScope::Owner,
        };
        let profile = HyperliquidProfile::new(
            vec![account],
            Arc::new(Transport {
                subaccounts: json!([]),
            }),
            Arc::new(Prices),
            Arc::new(Nonces::default()),
        )
        .unwrap();
        let submission = Submission {
            kind: SUBMISSION_KIND.into(),
            reference: "synthetic".into(),
            data: json!({}),
        };

        assert_eq!(
            profile
                .validate_response(&submission, &json!({"status":"err","response":"Rejected"}))
                .unwrap_err()
                .code,
            "HYPERLIQUID_ACTION_REJECTED"
        );
        assert_eq!(
            profile
                .validate_response(
                    &submission,
                    &json!({"status":"err","response":{"message":"Rejected"}})
                )
                .unwrap_err()
                .code,
            "HYPERLIQUID_SUBMISSION_UNCERTAIN"
        );
    }
}

#[cfg(test)]
mod nonce_recovery_plan_tests {
    use super::*;
    use crate::account_mode::{AccountMode, AccountModeState};
    use async_trait::async_trait;
    use fin_domain::AssetPrice;
    use fin_exchange::ccxt::HyperliquidInfoRequest as Info;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const OWNER: &str = "0x1111111111111111111111111111111111111111";
    const PARENT_ID: &str = "synthetic-transfer-parent";

    fn account() -> HyperliquidAccount {
        HyperliquidAccount {
            name: "synthetic-owner".into(),
            owner: OWNER.into(),
            credential_ref: Some("synthetic-protected-owner-key".into()),
            signing_enabled: true,
            execution_scope: HyperliquidExecutionScope::Owner,
        }
    }

    fn parent(account: &HyperliquidAccount, nonce: u64) -> OperationRecord {
        let envelope = Envelope {
            nonce_ms: nonce,
            vault_address: None,
            action: Action::UsdClassTransfer {
                amount_usdc: "1".into(),
                to_perp: false,
                nonce,
            },
        };
        let wire = envelope.action_wire_json().unwrap();
        let input = json!({
            "request_id":"synthetic-transfer-request",
            "account":account.name,
            "asset":"USDC",
            "amount":"1",
            "destination":"spot",
            "max_amount_usdc":"1",
            "max_fee_usdc":"0.01"
        });
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
            account_mode: Some(AccountModeState {
                owner: account.owner.clone(),
                execution_address: account.owner.clone(),
                native_abstraction: "disabled".into(),
                mode: AccountMode::Separate,
                dex_abstraction: Some(false),
            }),
            action: envelope.action.clone(),
            orders: Vec::new(),
            authority_before: None,
            replacement: None,
            recovery: None,
            metadata: Default::default(),
        };
        OperationRecord {
            id: PARENT_ID.into(),
            request_id: "synthetic-transfer-request".into(),
            fingerprint: "synthetic-fingerprint".into(),
            fingerprint_version: 1,
            operation: "hyperliquid.account.transfer".into(),
            input: input.clone(),
            status: Status::Unknown,
            created_at: now(),
            updated_at: now(),
            preview: Some(PreparedOperation {
                operation: "hyperliquid.account.transfer".into(),
                input,
                binding: serde_json::to_value(binding).unwrap(),
                summary: "synthetic original class transfer".into(),
                checks: Vec::new(),
                warnings: Vec::new(),
                observed_at: now(),
                expires_at: (chrono::Utc::now() + chrono::Duration::minutes(1)).to_rfc3339(),
                valuation: None,
                intent: FinancialIntent {
                    debits: vec![ResourceAmount {
                        resource: account.resource("perpetual", None),
                        amount: Amount {
                            asset: crate::profile::usdc(),
                            atoms: crate::writes::q("1").unwrap(),
                        },
                    }],
                    maximum_costs: vec![CostBound {
                        location: AccountingLocation::Cash {
                            resource: account.resource("perpetual", None),
                        },
                        amount: Amount {
                            asset: crate::profile::usdc(),
                            atoms: crate::writes::q("0.01").unwrap(),
                        },
                        treatment: CostTreatment::AdditionalDebit,
                    }],
                    expected: vec![EffectConstraint::Transfer {
                        from: account.resource("perpetual", None),
                        to: account.resource("spot", None),
                        debit_asset: crate::profile::usdc(),
                        credit_asset: crate::profile::usdc(),
                        maximum_debit: crate::writes::q("1").unwrap(),
                        minimum_credit: crate::writes::q("1").unwrap(),
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
            dispatch_evidence: Some(json!({"may_have_dispatched":true})),
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        }
    }

    struct ReadTransport;
    #[async_trait]
    impl HyperliquidTransport for ReadTransport {
        async fn info(&self, request: Info) -> Result<Value> {
            match request {
                Info::UserRole { owner } if owner == OWNER => Ok(json!({"role":"user"})),
                Info::UserAbstractionState { owner } if owner == OWNER => {
                    Ok(json!({"abstraction":"disabled"}))
                }
                Info::UserDexAbstractionState { owner } if owner == OWNER => Ok(json!(false)),
                _ => Err(FinError::validation(
                    "unexpected nonce recovery info request",
                )),
            }
        }
        async fn dispatch(
            &self,
            _: &HyperliquidAccount,
            _: &Envelope,
            _: Arc<dyn SubmissionJournal>,
        ) -> Result<Value> {
            Err(FinError::validation("planning must not dispatch"))
        }
    }

    struct NoPrices;
    #[async_trait]
    impl HyperliquidPrices for NoPrices {
        async fn usdc_price(&self) -> Result<AssetPrice> {
            Err(FinError::validation(
                "nonce recovery must not request a quote",
            ))
        }
    }

    struct RecoveryState {
        original: OperationRecord,
        allocations: AtomicUsize,
        child: Option<OperationRecord>,
    }
    impl HyperliquidNonces for RecoveryState {
        fn allocate(&self, _: &str, _: u64) -> Result<u64> {
            self.allocations.fetch_add(1, Ordering::SeqCst);
            Ok(u64::MAX)
        }
        fn operation(&self, id: &str) -> Result<Option<OperationRecord>> {
            Ok((id == self.original.id).then(|| self.original.clone()))
        }
        fn recovery_child(&self, parent_id: &str) -> Result<Option<OperationRecord>> {
            Ok((parent_id == self.original.id)
                .then(|| self.child.clone())
                .flatten())
        }
    }

    #[tokio::test]
    async fn recovery_uses_exact_original_nonce_without_allocating_a_new_one() {
        let account = account();
        let nonce = chrono::Utc::now().timestamp_millis() as u64 - 100;
        let original = parent(&account, nonce);
        let state = Arc::new(RecoveryState {
            original: original.clone(),
            allocations: AtomicUsize::new(0),
            child: None,
        });
        let profile = HyperliquidProfile::new(
            vec![account.clone()],
            Arc::new(ReadTransport),
            Arc::new(NoPrices),
            state.clone(),
        )
        .unwrap();
        let input = json!({
            "request_id":"synthetic-recovery-request",
            "account":account.name,
            "original_operation_id":PARENT_ID
        });
        let prepared = profile
            .prepare_action("hyperliquid.account.invalidate_nonce", &input)
            .await
            .unwrap();
        assert!(prepared.intent.debits.is_empty());
        assert!(prepared.intent.maximum_costs.is_empty());
        assert!(prepared.intent.expected.is_empty());
        assert_eq!(prepared.intent.locks.len(), 1);
        assert_eq!(
            profile
                .validate_recovery_child(&prepared, &original)
                .unwrap()
                .unwrap()
                .parent_submission,
            original.submission.clone().unwrap()
        );

        let plan = profile.plan_submission(&prepared).await.unwrap();
        assert_eq!(plan.request.envelope.nonce_ms, nonce);
        assert!(matches!(plan.request.envelope.action, Action::Noop {}));
        assert_eq!(state.allocations.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn existing_linked_child_blocks_duplicate_noop_preparation() {
        let account = account();
        let original = parent(&account, chrono::Utc::now().timestamp_millis() as u64 - 100);
        let state = Arc::new(RecoveryState {
            original: original.clone(),
            allocations: AtomicUsize::new(0),
            child: Some(original.clone()),
        });
        let profile = HyperliquidProfile::new(
            vec![account.clone()],
            Arc::new(ReadTransport),
            Arc::new(NoPrices),
            state,
        )
        .unwrap();
        let error = profile
            .prepare_action(
                "hyperliquid.account.invalidate_nonce",
                &json!({
                    "request_id":"synthetic-duplicate-request",
                    "account":account.name,
                    "original_operation_id":PARENT_ID
                }),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "HYPERLIQUID_NONCE_RECOVERY_ALREADY_ATTEMPTED");
    }

    #[tokio::test]
    async fn exact_durable_awaiting_child_can_be_revalidated_without_a_second_noop() {
        let account = account();
        let original = parent(&account, chrono::Utc::now().timestamp_millis() as u64 - 100);
        let input = json!({
            "request_id":"synthetic-recovery-request",
            "account":account.name,
            "original_operation_id":PARENT_ID
        });
        let state_without_child = Arc::new(RecoveryState {
            original: original.clone(),
            allocations: AtomicUsize::new(0),
            child: None,
        });
        let profile_without_child = HyperliquidProfile::new(
            vec![account.clone()],
            Arc::new(ReadTransport),
            Arc::new(NoPrices),
            state_without_child,
        )
        .unwrap();
        let approved = profile_without_child
            .prepare_action("hyperliquid.account.invalidate_nonce", &input)
            .await
            .unwrap();
        let child = OperationRecord {
            id: "synthetic-recovery-child-id".into(),
            request_id: "synthetic-recovery-request".into(),
            fingerprint: "synthetic-fingerprint".into(),
            fingerprint_version: 1,
            operation: "hyperliquid.account.invalidate_nonce".into(),
            input: input.clone(),
            status: Status::AwaitingApproval,
            created_at: now(),
            updated_at: now(),
            preview: Some(approved.clone()),
            approval: None,
            submission: None,
            dispatch_evidence: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let state_with_child = Arc::new(RecoveryState {
            original,
            allocations: AtomicUsize::new(0),
            child: Some(child),
        });
        let revalidator = HyperliquidProfile::new(
            vec![account],
            Arc::new(ReadTransport),
            Arc::new(NoPrices),
            state_with_child,
        )
        .unwrap();
        let adapter = fin_exchange::ExchangeAdapter::new(revalidator);
        let refreshed = adapter.revalidate(&approved).await.unwrap();
        assert_eq!(refreshed.operation, approved.operation);
        assert_eq!(refreshed.input, approved.input);
        assert_eq!(refreshed.binding, approved.binding);
        assert_eq!(refreshed.intent, approved.intent);
    }
}
