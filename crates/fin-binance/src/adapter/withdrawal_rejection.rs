//! Recovery for Binance's definite withdrawal-address whitelist rejection.
//!
//! A response code alone is not enough to release a withdrawal reservation:
//! reconciliation also requires complete identity-filtered history and an
//! unchanged source balance for the original journaled request.
use super::*;

pub(super) fn recorded_whitelist_rejection(error: &FinError) -> bool {
    if error.stage != "submission"
        || !matches!(
            error.code.as_str(),
            "VENUE_REQUEST_FAILED" | "WITHDRAWAL_ADDRESS_NOT_WHITELISTED"
        )
    {
        return false;
    }
    if let Some(details) = &error.details {
        return details.as_ref()
            == &json!({
                "transport": "binance-ccxt", "http_status": 400, "binance_code": -4035
            });
    }

    // Accept only the exact sanitized legacy message produced before protocol
    // details and the actionable error code were added. Never infer rejection
    // from arbitrary upstream text.
    error.code == "VENUE_REQUEST_FAILED"
        && error.message
            == "Binance rejected the request; reconcile any persisted submission before retrying. [HTTP 400; Binance code -4035]"
}

pub(super) fn try_reconcile(
    record: &OperationRecord,
    submission: &Submission,
    history_complete: bool,
    matching_history_rows: usize,
    source_free_now: &ExactDecimal,
) -> Result<Option<Reconciliation>, FinError> {
    if !history_complete
        || matching_history_rows != 0
        || record.operation != "binance.withdraw"
        || record.status.terminal()
        || record.submission.as_ref() != Some(submission)
        || record.dispatch_evidence.is_some()
        || submission.kind != "binance"
        || submission.data["operation"] != "binance.withdraw"
        || submission.reference.is_empty()
        || submission.data["binding"]["withdrawOrderId"] != submission.reference
        || record
            .error
            .as_ref()
            .is_none_or(|error| !recorded_whitelist_rejection(error))
    {
        return Ok(None);
    }

    let source_before = observed_decimal(
        submission.data["sourceFreeBefore"]
            .as_str()
            .unwrap_or_default(),
    )?;
    if source_free_now.checked_cmp(&source_before).is_ne() {
        return Ok(None);
    }

    let evidence = json!({
        "method": "recorded-binance-withdrawal-whitelist-rejection-v1",
        "withdrawOrderId": submission.reference,
        "submissionIdentity": submission.reference,
        "httpStatus": 400,
        "binanceCode": -4035,
        "historyComplete": true,
        "matchingHistoryRows": 0,
        "sourceFreeBefore": source_before.canonical(),
        "sourceFree": source_free_now.canonical(),
        "sourceDebitObserved": "0",
        "payoutEffects": "none",
        "noEffects": true,
        "reason": "Binance definitively rejected this exact withdrawal because its destination is not whitelisted; complete withdrawal history has no matching record and the source balance is unchanged."
    });
    let verified = make_verified(submission.clone(), vec![], evidence.clone())?;
    Ok(Some(Reconciliation {
        status: Status::Failed,
        evidence,
        verified: Some(verified),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn submission() -> Submission {
        Submission {
            kind: "binance".into(),
            reference: "withdraw-order-synthetic-1".into(),
            data: json!({
                "operation": "binance.withdraw",
                "sourceFreeBefore": "1.25",
                "binding": {"withdrawOrderId": "withdraw-order-synthetic-1"}
            }),
        }
    }

    fn record(submission: Submission) -> OperationRecord {
        OperationRecord {
            id: "synthetic-operation".into(),
            request_id: "synthetic-request".into(),
            fingerprint: "synthetic-fingerprint".into(),
            fingerprint_version: 1,
            operation: "binance.withdraw".into(),
            input: json!({}),
            status: Status::Unknown,
            created_at: now(),
            updated_at: now(),
            preview: None,
            approval: None,
            submission: Some(submission),
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: Some(http_failure_error(
                "submission",
                400,
                Some(-4035),
                HttpFailureKind::Rejected,
            )),
            run_id: None,
        }
    }

    fn reconcile(
        record: &OperationRecord,
        submission: &Submission,
        complete: bool,
        rows: usize,
        current_balance: &str,
    ) -> Result<Option<Reconciliation>, FinError> {
        let current_balance = observed_decimal(current_balance)?;
        try_reconcile(record, submission, complete, rows, &current_balance)
    }

    #[test]
    fn exact_rejection_with_complete_empty_history_and_unchanged_balance_proves_no_effects() {
        let submission = submission();
        let record = record(submission.clone());
        let result = reconcile(&record, &submission, true, 0, "1.25")
            .unwrap()
            .unwrap();

        assert_eq!(result.status, Status::Failed);
        assert_eq!(result.evidence["withdrawOrderId"], submission.reference);
        assert_eq!(result.evidence["binanceCode"], -4035);
        assert_eq!(result.evidence["historyComplete"], true);
        assert_eq!(result.evidence["matchingHistoryRows"], 0);
        assert_eq!(result.evidence["sourceDebitObserved"], "0");
        assert_eq!(result.evidence["payoutEffects"], "none");
        let verified = result.verified.unwrap();
        assert_eq!(verified.submission, submission);
        assert!(verified.effects.is_empty());
    }

    #[test]
    fn incomplete_or_conflicting_observations_never_release_the_withdrawal() {
        let submission = submission();
        let record = record(submission.clone());
        for (complete, rows, balance) in [
            (false, 0, "1.25"),
            (true, 1, "1.25"),
            (true, 0, "1.24"),
            (true, 0, "1.26"),
        ] {
            assert!(
                reconcile(&record, &submission, complete, rows, balance)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn only_original_nonterminal_withdrawal_and_exact_recorded_code_qualify() {
        let submission = submission();
        let mut changed_record = record(submission.clone());
        changed_record.operation = "binance.transfer".into();
        assert!(
            reconcile(&changed_record, &submission, true, 0, "1.25")
                .unwrap()
                .is_none()
        );

        let mut mismatched_submission = submission.clone();
        mismatched_submission.reference = "another-order".into();
        assert!(
            reconcile(
                &record(submission.clone()),
                &mismatched_submission,
                true,
                0,
                "1.25"
            )
            .unwrap()
            .is_none()
        );

        let mut bad_binding = submission.clone();
        bad_binding.data["binding"]["withdrawOrderId"] = json!("another-order");
        assert!(
            reconcile(&record(bad_binding.clone()), &bad_binding, true, 0, "1.25")
                .unwrap()
                .is_none()
        );

        let mut wrong_code = record(submission.clone());
        wrong_code.error = Some(http_failure_error(
            "submission",
            400,
            Some(-1022),
            HttpFailureKind::Rejected,
        ));
        assert!(
            reconcile(&wrong_code, &submission, true, 0, "1.25")
                .unwrap()
                .is_none()
        );

        let mut mapped_rejection = record(submission.clone());
        mapped_rejection.error = Some(
            FinError::new(
                "WITHDRAWAL_ADDRESS_NOT_WHITELISTED",
                "submission",
                "Synthetic actionable whitelist rejection.",
                "Whitelist the destination, then submit a new request.",
            )
            .with_details(json!({
                "transport": "binance-ccxt",
                "http_status": 400,
                "binance_code": -4035
            })),
        );
        assert!(
            reconcile(&mapped_rejection, &submission, true, 0, "1.25")
                .unwrap()
                .is_some()
        );

        let mut mismatched_details = record(submission.clone());
        mismatched_details.error = Some(
            FinError::new(
                "WITHDRAWAL_ADDRESS_NOT_WHITELISTED",
                "submission",
                "Synthetic actionable whitelist rejection.",
                "Whitelist the destination, then submit a new request.",
            )
            .with_details(json!({
                "transport": "binance-ccxt",
                "http_status": 400,
                "binance_code": -1022
            })),
        );
        assert!(
            reconcile(&mismatched_details, &submission, true, 0, "1.25")
                .unwrap()
                .is_none()
        );

        let mut legacy_rejection = record(submission.clone());
        legacy_rejection.error = Some(FinError::new(
            "VENUE_REQUEST_FAILED",
            "submission",
            "Binance rejected the request; reconcile any persisted submission before retrying. [HTTP 400; Binance code -4035]",
            "Inspect operation status and reconcile the original request before starting another financial action.",
        ));
        legacy_rejection.error.as_mut().unwrap().details = None;
        assert!(
            reconcile(&legacy_rejection, &submission, true, 0, "1.25")
                .unwrap()
                .is_some()
        );

        let mut dispatched = record(submission.clone());
        dispatched.dispatch_evidence = Some(json!({"response": "received"}));
        assert!(
            reconcile(&dispatched, &submission, true, 0, "1.25")
                .unwrap()
                .is_none()
        );

        let mut terminal = record(submission.clone());
        terminal.status = Status::Failed;
        assert!(
            reconcile(&terminal, &submission, true, 0, "1.25")
                .unwrap()
                .is_none()
        );
    }
}
