//! Narrow recovery of a protected broker's recorded authentication rejection.
//! Binance rejects invalid signatures before financial execution. Missing
//! history or unchanged balances alone never proves a submission failed.
use super::*;

fn recorded_signature_rejection(error: &FinError) -> bool {
    if error.stage != "submission"
        || !matches!(
            error.code.as_str(),
            "VENUE_REQUEST_FAILED" | "VENUE_PERMISSION"
        )
    {
        return false;
    }
    if let Some(details) = &error.details {
        return details.as_ref()
            == &json!({
                "transport": "binance-ccxt", "http_status": 400, "binance_code": -1022
            });
    }
    // Historical protected records carried these sanitized protocol fields in
    // an exact broker-generated message. Do not parse arbitrary upstream text,
    // loosely match a substring, or accept a different status/error code.
    [HttpFailureKind::Rejected, HttpFailureKind::PermissionDenied]
        .into_iter()
        .any(|kind| {
            let expected = http_failure_error("submission", 400, Some(-1022), kind);
            error.code == expected.code && error.message == expected.message
        })
}

impl BinanceProfile {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn reconcile_signature_rejection(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        from: &BinanceAccount,
        to: &BinanceAccount,
        authority: &BinanceAccount,
        _requested: &ExactDecimal,
        history_complete: bool,
    ) -> Result<Option<Reconciliation>, FinError> {
        if !history_complete
            || record.operation != "binance.transfer"
            || record.submission.as_ref() != Some(submission)
            || record.dispatch_evidence.is_some()
            || record
                .error
                .as_ref()
                .is_none_or(|error| !recorded_signature_rejection(error))
        {
            return Ok(None);
        }
        let source_before = observed_decimal(
            submission.data["sourceFreeBefore"]
                .as_str()
                .unwrap_or_default(),
        )?;
        let destination_before = observed_decimal(
            submission.data["destinationFreeBefore"]
                .as_str()
                .unwrap_or_default(),
        )?;
        let source_balances = self.spot_balances(from, Some(authority)).await?;
        let destination_balances = self.spot_balances(to, Some(authority)).await?;
        let coin = submission.data["binding"]["coin"]
            .as_str()
            .unwrap_or_default();
        let source_now = Self::free(&source_balances, coin)
            .cloned()
            .unwrap_or_else(ExactDecimal::zero);
        let destination_now = Self::free(&destination_balances, coin)
            .cloned()
            .unwrap_or_else(ExactDecimal::zero);
        if source_now.checked_cmp(&source_before).is_ne()
            || destination_now.checked_cmp(&destination_before).is_ne()
        {
            return Ok(None);
        }
        let evidence = json!({
            "method": "recorded-binance-signature-rejection-v1",
            "submissionIdentity": submission.reference,
            "httpStatus": 400,
            "binanceCode": -1022,
            "historyComplete": true,
            "matchingHistoryRows": 0,
            "sourceFree": source_now.canonical(),
            "destinationFree": destination_now.canonical(),
            "noEffects": true,
            "reason": "Protected broker recorded an invalid-signature rejection for this exact dispatch; complete history contains no matching transfer and both balances are unchanged."
        });
        let verified = make_verified(submission.clone(), vec![], evidence.clone())?;
        Ok(Some(Reconciliation {
            status: Status::Failed,
            evidence,
            verified: Some(verified),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_protected_submission_signature_rejections_qualify() {
        for kind in [HttpFailureKind::Rejected, HttpFailureKind::PermissionDenied] {
            let current = http_failure_error("submission", 400, Some(-1022), kind);
            assert!(recorded_signature_rejection(&current));
            let mut legacy = current.clone();
            legacy.details = None;
            assert!(recorded_signature_rejection(&legacy));
            legacy.message.push_str(" unrelated");
            assert!(!recorded_signature_rejection(&legacy));
            let mut mismatched = current.clone();
            mismatched.details = Some(Box::new(
                json!({"transport":"binance-ccxt","http_status":500,"binance_code":-1022}),
            ));
            assert!(!recorded_signature_rejection(&mismatched));
        }
        for stage in ["preview", "validation", "reconciliation"] {
            assert!(!recorded_signature_rejection(&http_failure_error(
                stage,
                400,
                Some(-1022),
                HttpFailureKind::Rejected
            )));
        }
        for code in [-1007, -1006, -2010, -2015, -1021] {
            assert!(!recorded_signature_rejection(&http_failure_error(
                "submission",
                400,
                Some(code),
                HttpFailureKind::Rejected
            )));
        }
        assert!(!recorded_signature_rejection(&http_failure_error(
            "submission",
            500,
            Some(-1022),
            HttpFailureKind::Rejected
        )));
        assert!(!recorded_signature_rejection(&error(
            "VENUE_TIMEOUT",
            "submission",
            "Binance timed out; -1022"
        )));
    }
}
