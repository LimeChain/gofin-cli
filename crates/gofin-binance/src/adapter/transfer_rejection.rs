//! Narrow recovery of a protected broker's recorded authentication rejection.
//! Binance rejects invalid signatures before financial execution. Missing
//! history or unchanged balances alone never proves a submission failed.
use super::*;

fn recorded_signature_rejection(error: &GoFinError) -> bool {
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

pub(super) fn recorded_withdrawal_whitelist_rejection(error: &GoFinError) -> bool {
    error.stage == "submission"
        && matches!(
            error.code.as_str(),
            "VENUE_REQUEST_FAILED" | "WITHDRAWAL_ADDRESS_NOT_WHITELISTED"
        )
        && error.details.as_deref()
            == Some(&json!({
                "transport": "binance-ccxt",
                "http_status": 400,
                "binance_code": -4035
            }))
}

pub(super) fn withdrawal_rejection_identity_matches(
    record: &OperationRecord,
    submission: &Submission,
    account: &BinanceAccount,
    data: &Value,
    binding: &Value,
    coin: &str,
    network: &str,
) -> bool {
    let Some(preview) = record.preview.as_ref() else {
        return false;
    };
    let Some(request_id) = record.input.get("requestId").and_then(Value::as_str) else {
        return false;
    };
    if preview.input.get("requestId").and_then(Value::as_str) != Some(request_id) {
        return false;
    }
    let Ok(client_id) = ClientIdentity::withdrawal(request_id) else {
        return false;
    };
    let Ok(params) = serde_json::from_value::<BinanceRequest>(binding["params"].clone()) else {
        return false;
    };
    let input = &record.input;
    record.operation == "binance.withdraw"
        && record.submission.as_ref() == Some(submission)
        && record.dispatch_evidence.is_none()
        && record
            .error
            .as_ref()
            .is_some_and(recorded_withdrawal_whitelist_rejection)
        && data.get("operation").and_then(Value::as_str) == Some("binance.withdraw")
        && data.get("venueId").is_none_or(Value::is_null)
        && data.get("binding") == Some(binding)
        && preview.binding == *binding
        && input.get("account").and_then(Value::as_str) == Some(account.name.as_str())
        && data.get("account").and_then(Value::as_str) == Some(account.name.as_str())
        && binding.get("account").and_then(Value::as_str) == Some(account.name.as_str())
        && matches!(account.role, AccountRole::Main)
        && preview.intent.debits.iter().any(|debit| {
            debit.resource == account.resource
                && debit.amount.asset.network == "binance"
                && debit.amount.asset.reference == coin
                && debit.amount.asset.decimals == 36
        })
        && data.get("destination") == binding.get("destination")
        && if input
            .get("destination")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
        {
            input.get("address").and_then(Value::as_str)
                == data.get("address").and_then(Value::as_str)
                && binding.get("receiptAddress").and_then(Value::as_str)
                    == data.get("address").and_then(Value::as_str)
        } else {
            data.get("destination") == input.get("destination")
        }
        && data.get("coin").and_then(Value::as_str) == Some(coin)
        && input.get("coin").and_then(Value::as_str) == Some(coin)
        && data.get("amount").and_then(Value::as_str) == input.get("amount").and_then(Value::as_str)
        && data.get("network").and_then(Value::as_str) == Some(network)
        && input.get("network").and_then(Value::as_str) == Some(network)
        && data.get("memo").and_then(Value::as_str).unwrap_or("")
            == input.get("memo").and_then(Value::as_str).unwrap_or("")
        && data.get("maximumFee") == binding.get("maximumFee")
        && data.get("maximumFee") == input.get("maxFee")
        && data.get("minimumReceive") == binding.get("minimumReceive")
        && data.get("minimumReceive") == input.get("minReceiveAmount")
        && input.get("walletType").and_then(Value::as_str) == Some("SPOT")
        && binding.get("withdrawOrderId").and_then(Value::as_str)
            == Some(submission.reference.as_str())
        && client_id.as_str() == submission.reference
        && params.endpoint == BinanceEndpoint::StandardWithdrawal
        && params.validate().is_ok()
        && params.get("withdrawOrderId") == Some(submission.reference.as_str())
        && params.get("coin") == Some(coin)
        && params.get("network") == Some(network)
        && params.get("address") == data.get("address").and_then(Value::as_str)
        && input
            .get("address")
            .and_then(Value::as_str)
            .is_none_or(|address| params.get("address") == Some(address))
        && params.get("addressTag").unwrap_or("")
            == data.get("memo").and_then(Value::as_str).unwrap_or("")
        && params.get("amount") == data.get("amount").and_then(Value::as_str)
        && params.get("walletType") == Some("0")
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
    ) -> Result<Option<Reconciliation>, GoFinError> {
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

    #[test]
    fn withdrawal_whitelist_rejection_requires_the_exact_protected_diagnostic() {
        let mut exact =
            http_failure_error("submission", 400, Some(-4035), HttpFailureKind::Rejected);
        assert!(recorded_withdrawal_whitelist_rejection(&exact));

        for (stage, status, code, kind) in [
            ("preview", 400, Some(-4035), HttpFailureKind::Rejected),
            ("submission", 403, Some(-4035), HttpFailureKind::Rejected),
            ("submission", 400, Some(-2010), HttpFailureKind::Rejected),
            (
                "submission",
                400,
                Some(-4035),
                HttpFailureKind::PermissionDenied,
            ),
        ] {
            assert!(!recorded_withdrawal_whitelist_rejection(
                &http_failure_error(stage, status, code, kind,)
            ));
        }

        exact.details = Some(Box::new(json!({
            "transport": "binance-ccxt",
            "http_status": 400,
            "binance_code": -4035,
            "extra": true
        })));
        assert!(!recorded_withdrawal_whitelist_rejection(&exact));
    }
}
