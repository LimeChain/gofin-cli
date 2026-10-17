//! Same-account USDT spot/futures margin transfers with exact venue correlation.
use super::*;

fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str, GoFinError> {
    v[key].as_str().ok_or_else(|| {
        error(
            "FUTURES_EVIDENCE_INVALID",
            "futures",
            format!("Missing exact {key}."),
        )
    })
}
fn dec(v: &Value, key: &str) -> Result<ExactDecimal, GoFinError> {
    observed_decimal(text(v, key)?)
}
fn asset() -> AssetId {
    AssetId {
        network: "binance".into(),
        reference: "USDT".into(),
        decimals: 36,
    }
}
fn amt(v: &ExactDecimal) -> Result<Amount, GoFinError> {
    Ok(Amount {
        asset: asset(),
        atoms: decimal_to_atoms(v, 36)?,
    })
}
fn resource(a: &BinanceAccount) -> ResourceId {
    ResourceId(format!("{}:USD_M", a.resource.0))
}
fn unknown(reason: &str) -> Reconciliation {
    Reconciliation {
        status: Status::Unknown,
        evidence: json!({"reason":reason}),
        verified: None,
    }
}
fn definition(
    name: &str,
    description: &str,
    schema: Value,
    example: Value,
    completion: &str,
) -> Definition {
    Definition {
        name: name.into(),
        description: description.into(),
        kind: OperationKind::Write,
        execution_actor: ExecutionActor::AgentInvocable,
        input_schema: schema,
        output_schema: schema_for!(OperationRecord).into(),
        error_schema: schema_for!(GoFinError).into(),
        examples: vec![example],
        coverage: "Synthetic verified; live approval and venue acceptance tracked separately."
            .into(),
        completion: completion.into(),
        semantics: json!({"walletType":"USD_M","asset":"USDT","approval":"fresh per write","recovery":"Reconcile original identity without replay."}),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(in crate::adapter) enum FuturesTransferDirection {
    SpotToFutures,
    FuturesToSpot,
}
impl FuturesTransferDirection {
    fn wallets(&self) -> (&'static str, &'static str) {
        match self {
            Self::SpotToFutures => ("SPOT", "USDT_FUTURE"),
            Self::FuturesToSpot => ("USDT_FUTURE", "SPOT"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(in crate::adapter) struct FuturesTransferInput {
    pub request_id: String,
    /// Configured main authority, or "self" for a standalone subaccount using its own credential.
    pub authority: String,
    pub account: String,
    pub direction: FuturesTransferDirection,
    #[schemars(schema_with = "super::decimal_schema::positive")]
    pub amount: String,
}

pub(super) fn definitions() -> Vec<Definition> {
    let mut entry = definition(
        "binance.futures.transfer",
        "Move exact USDT between SPOT and USD-M futures using a configured main authority, or authority self for a standalone subaccount.",
        schema_for!(FuturesTransferInput).into(),
        json!({"request_id":"margin-funding-1","authority":"main","account":"main","direction":"SPOT_TO_FUTURES","amount":"20"}),
        "Complete only after exact successful transfer history and independent wallet debit/credit. Self-authorized transfers require a durably captured venue receipt; a lost response remains unknown without replay.",
    );
    entry.examples.push(json!({"request_id":"sub-margin-funding-1","authority":"self","account":"standalone-sub","direction":"SPOT_TO_FUTURES","amount":"20"}));
    entry.semantics["authority"] = json!(
        "A configured main authority uses universalTransfer and clientTranId. The literal self is accepted only for a standalone subaccount, signing its own wallet transfer with its enrolled credential."
    );
    entry.semantics["recovery"] = json!(
        "The self route has no venue clientTranId: the returned tranId must be durably captured before exact history and wallet-delta reconciliation. A lost response without a receipt remains unknown and is never inferred from amount/time or replayed."
    );
    vec![entry]
}

impl BinanceProfile {
    async fn futures_mode(&self, account: &BinanceAccount) -> Result<Value, GoFinError> {
        self.account_identity(account).await?;
        if self.permissions(account).await?.enable_futures != Some(true) {
            return Err(error(
                "MISSING_PERMISSION",
                "preview",
                "Selected credential must report futures permission.",
            ));
        }
        let mode = self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::FuturesAccountConfig),
                "preview",
            )
            .await?;
        if mode["dualSidePosition"] != false || mode["multiAssetsMargin"] != false {
            return Err(error(
                "FUTURES_MODE_UNSUPPORTED",
                "preview",
                "One-way single-asset futures mode is required.",
            ));
        }
        Ok(mode)
    }
    async fn futures_usdt(&self, account: &BinanceAccount) -> Result<Value, GoFinError> {
        let value = self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::FuturesAccount),
                "futures",
            )
            .await?;
        let rows = value["assets"].as_array().ok_or_else(|| {
            error(
                "FUTURES_EVIDENCE_INVALID",
                "futures",
                "Missing futures assets.",
            )
        })?;
        let found: Vec<_> = rows.iter().filter(|r| r["asset"] == "USDT").collect();
        if found.len() != 1 {
            return Err(error(
                "FUTURES_EVIDENCE_INVALID",
                "futures",
                "Expected one USDT balance.",
            ));
        }
        Ok(found[0].clone())
    }
    pub(super) async fn futures_funding_submission(
        &self,
        current: &PreparedOperation,
    ) -> Result<PlannedSubmission<BinanceDispatch>, GoFinError> {
        let b = &current.binding;
        let request: BinanceRequest =
            serde_json::from_value(b["params"].clone()).map_err(domain_error)?;
        let submission = Submission {
            kind: "binance".into(),
            reference: text(b, "clientId")?.into(),
            data: json!({"operation":current.operation,"binding":b,"submittedAt":now()}),
        };
        Ok(PlannedSubmission {
            submission: submission.clone(),
            request: BinanceDispatch {
                authority: text(b, "authority")?.into(),
                request,
                noop: None,
                receipt_submission: (b["route"] == "self_wallet").then_some(submission),
            },
        })
    }
    async fn futures_transfer_balances(
        &self,
        account: &BinanceAccount,
        authority: &BinanceAccount,
    ) -> Result<(ExactDecimal, ExactDecimal, ExactDecimal), GoFinError> {
        let balances = self.spot_balances(account, Some(authority)).await?;
        let rows: Vec<_> = balances.iter().filter(|row| row.asset == "USDT").collect();
        if rows.len() > 1 {
            return Err(error(
                "FUTURES_EVIDENCE_INVALID",
                "futures",
                "Duplicate spot USDT balances.",
            ));
        }
        let spot = rows
            .first()
            .map(|row| row.free.clone())
            .unwrap_or_else(ExactDecimal::zero);
        let futures = self.futures_usdt(account).await?;
        Ok((
            spot,
            dec(&futures, "walletBalance")?,
            dec(&futures, "maxWithdrawAmount")?,
        ))
    }

    pub(super) async fn prepare_futures_transfer(
        &self,
        input: FuturesTransferInput,
    ) -> Result<PreparedOperation, GoFinError> {
        let account = self.account(&input.account)?;
        let self_wallet = input.authority == "self";
        let authority = if self_wallet {
            if !matches!(account.role, AccountRole::Sub) || account.parent.is_some() {
                return Err(error(
                    "INVALID_HIERARCHY",
                    "preview",
                    "Self authority requires a standalone subaccount.",
                ));
            }
            account
        } else {
            self.require_main(&input.authority)?
        };
        self.account_identity(authority).await?;
        if account.name != authority.name {
            self.verify_subaccount(account, authority).await?;
        }
        let permission = self.permissions(authority).await?;
        if (!self_wallet && permission.enable_internal_transfer != Some(true))
            || permission.permits_universal_transfer != Some(true)
        {
            return Err(error(
                "MISSING_PERMISSION",
                "preview",
                "Selected credential must report universal-transfer permission; a main authority also requires internal-transfer permission.",
            ));
        }
        let mode = self.futures_mode(account).await?;
        let (from_wallet, to_wallet) = input.direction.wallets();
        let permission_field = if from_wallet == "SPOT" {
            "canDeposit"
        } else {
            "canWithdraw"
        };
        if mode[permission_field] != true {
            return Err(error(
                "MISSING_PERMISSION",
                "preview",
                format!("Futures account must report {permission_field} permission."),
            ));
        }
        let quantity = amount(&input.amount)?;
        let (spot_before, futures_before, withdrawable) =
            self.futures_transfer_balances(account, authority).await?;
        let (source_before, destination_before, source, destination) = if from_wallet == "SPOT" {
            (
                &spot_before,
                &futures_before,
                account.resource.clone(),
                resource(account),
            )
        } else {
            if withdrawable.checked_cmp(&quantity).is_lt() {
                return Err(error(
                    "INSUFFICIENT_BALANCE",
                    "preview",
                    "Futures maximum transferable USDT does not cover the requested amount.",
                ));
            }
            (
                &futures_before,
                &spot_before,
                resource(account),
                account.resource.clone(),
            )
        };
        if source_before.checked_cmp(&quantity).is_lt() {
            return Err(error(
                "INSUFFICIENT_BALANCE",
                "preview",
                "Source wallet available USDT does not cover the requested amount.",
            ));
        }
        let identity =
            ClientIdentity::derive("binance.futures.transfer", &input.request_id, "gofin")
                .map_err(domain_error)?;
        let request = if self_wallet {
            BinanceRequest::new(BinanceEndpoint::WalletTransfer)
                .push(
                    "type",
                    if from_wallet == "SPOT" {
                        "MAIN_UMFUTURE"
                    } else {
                        "UMFUTURE_MAIN"
                    },
                )
                .push("asset", "USDT")
                .push("amount", quantity.canonical())
        } else {
            let mut request = BinanceRequest::new(BinanceEndpoint::UniversalTransfer)
                .push("asset", "USDT")
                .push("amount", quantity.canonical())
                .push("fromAccountType", from_wallet)
                .push("toAccountType", to_wallet)
                .push("clientTranId", identity.as_str());
            if let Some(email) = account.email.as_deref() {
                request = request.push("fromEmail", email).push("toEmail", email);
            }
            request
        };
        request.validate().map_err(domain_error)?;
        let mut binding = json!({
            "authority":authority.name,"account":account.name,"resource":resource(account),
            "source":source,"destination":destination,"asset":asset(),"amount":quantity.canonical(),
            "fromWallet":from_wallet,"toWallet":to_wallet,"email":account.email,
            "clientId":identity.as_str(),"params":request,
            "sourceBefore":source_before.canonical(),"destinationBefore":destination_before.canonical()
        });
        if self_wallet {
            binding["route"] = json!("self_wallet");
        }
        let value = amt(&quantity)?;
        let mut locks = vec![
            account.resource.clone(),
            resource(account),
            authority.resource.clone(),
        ];
        locks.sort_by(|a, b| a.0.cmp(&b.0));
        locks.dedup();
        let intent = FinancialIntent {
            debits: vec![ResourceAmount {
                resource: source.clone(),
                amount: value.clone(),
            }],
            maximum_costs: vec![],
            expected: vec![EffectConstraint::Transfer {
                from: source,
                to: destination,
                debit_asset: asset(),
                credit_asset: asset(),
                maximum_debit: value.atoms.clone(),
                minimum_credit: value.atoms,
            }],
            locks,
        };
        let warnings = if from_wallet == "SPOT" {
            vec!["Transferred USDT becomes futures collateral and can be lost through trading, funding charges or liquidation; cross margin can expose the wallet balance.".into()]
        } else {
            vec![]
        };
        let valuation = self.valuation("USDT", &quantity.canonical(), "0").await?;
        Ok(prepared(
            "binance.futures.transfer",
            serde_json::to_value(&input).map_err(domain_error)?,
            binding,
            format!(
                "Transfer {} USDT in {} from {} to {} using authority {}; internal transfer fee 0.",
                quantity.canonical(),
                account.name,
                from_wallet,
                to_wallet,
                authority.name
            ),
            vec![Self::check(
                "margin_transfer",
                CheckStatus::Passed,
                "Account identity, authority, futures mode, transfer permission and transferable balance were verified.",
            )],
            warnings,
            valuation,
            intent,
        ))
    }

    /// Own-wallet transfers have no clientTranId. Only the receipt durably linked
    /// during authorized dispatch may select a history row; amount/time never do.
    async fn self_wallet_history(
        &self,
        record: &OperationRecord,
        account: &BinanceAccount,
        request: &BinanceRequest,
        start_ms: u64,
        end_ms: u64,
    ) -> std::result::Result<TransferHistoryRow, &'static str> {
        let receipt = record.dispatch_evidence.as_ref().ok_or(
            "Own-wallet transfer receipt was not durably captured; retain the original submission without replay or inference from amount/time.",
        )?;
        let venue_id = receipt["venueId"]
            .as_str()
            .ok_or("Malformed own-wallet receipt.")?;
        let venue_id = VenueIdentifier::parse(&json!(venue_id))
            .map_err(|_| "Malformed own-wallet receipt.")?;
        if *receipt != json!({"route":"self_wallet","venueId":venue_id.as_str()}) {
            return Err("Own-wallet receipt does not match its dispatch route.");
        }
        let transfer_type = request.get("type").ok_or("Missing own-wallet direction.")?;
        let mut found = Vec::new();
        let mut fetched = 0u64;
        let mut total = None;
        let mut complete = false;
        for page in 1..=20u64 {
            let response = self
                .call(
                    account,
                    BinanceRequest::new(BinanceEndpoint::WalletTransferHistory)
                        .push("type", transfer_type)
                        .push("startTime", start_ms.to_string())
                        .push("endTime", end_ms.to_string())
                        .push("current", page.to_string())
                        .push("size", "100"),
                    "reconciliation",
                )
                .await
                .map_err(|_| "Own-wallet transfer history is unavailable.")?;
            let count = response["total"]
                .as_u64()
                .ok_or("Malformed own-wallet history total.")?;
            // Binance omits rows entirely for an empty history query. This is
            // absence of evidence, never proof of a failed or completed write.
            let rows = match response.get("rows") {
                None if count == 0 => &[][..],
                Some(Value::Array(rows)) => rows.as_slice(),
                _ => return Err("Malformed own-wallet history rows."),
            };
            if rows.len() > 100 || total.is_some_and(|v| v != count) {
                return Err("Own-wallet history pagination is inconsistent.");
            }
            total = Some(count);
            fetched += rows.len() as u64;
            if fetched > count {
                return Err("Own-wallet history count is inconsistent.");
            }
            for row in rows {
                let id = VenueIdentifier::parse(&row["tranId"])
                    .map_err(|_| "Malformed own-wallet history identity.")?;
                if id == venue_id {
                    found.push(row.clone());
                }
            }
            if found.len() > 1 {
                return Err("Duplicate own-wallet receipt history is ambiguous.");
            }
            if fetched == count {
                complete = true;
                break;
            }
            if rows.len() < 100 {
                break;
            }
        }
        if !complete || found.len() != 1 {
            return Err(
                "One matching venue receipt was not proven in complete bounded own-wallet history.",
            );
        }
        let row = found.remove(0);
        if row["type"] != transfer_type {
            return Err("Own-wallet history direction differs from the approved transfer.");
        }
        // Normalize wallet names only after verifying Binance's native type.
        // Preserve the native status and do not manufacture a client identity.
        let (from, to) = if transfer_type == "MAIN_UMFUTURE" {
            ("SPOT", "USDT_FUTURE")
        } else {
            ("USDT_FUTURE", "SPOT")
        };
        Ok(TransferHistoryRow {
            tran_id: venue_id,
            client_tran_id: None,
            asset: row["asset"]
                .as_str()
                .ok_or("Missing own-wallet history asset.")?
                .into(),
            amount: observed_decimal(
                row["amount"]
                    .as_str()
                    .ok_or("Missing own-wallet history amount.")?,
            )
            .map_err(|_| "Malformed own-wallet history amount.")?,
            from_account_type: from.into(),
            to_account_type: to.into(),
            from_email: None,
            to_email: None,
            status: row["status"]
                .as_str()
                .ok_or("Missing own-wallet history status.")?
                .into(),
            timestamp: row["timestamp"]
                .as_u64()
                .ok_or("Missing own-wallet history timestamp.")?,
        })
    }

    pub(super) async fn reconcile_futures_transfer(
        &self,
        _record: &OperationRecord,
        submission: &Submission,
    ) -> Result<Reconciliation, GoFinError> {
        let binding = &submission.data["binding"];
        if submission.data["operation"] != _record.operation
            || _record
                .preview
                .as_ref()
                .is_none_or(|p| p.binding != *binding)
            || binding["clientId"] != submission.reference
        {
            return Ok(unknown(
                "Persisted transfer identity differs from its approved binding.",
            ));
        }
        let self_wallet = binding["route"] == "self_wallet";
        let account = self.account(text(binding, "account")?)?;
        let authority = if self_wallet {
            self.account(text(binding, "authority")?)?
        } else {
            self.require_main(text(binding, "authority")?)?
        };
        let quantity = amount(text(binding, "amount")?)?;
        let request: BinanceRequest =
            serde_json::from_value(binding["params"].clone()).map_err(domain_error)?;
        let input: FuturesTransferInput =
            serde_json::from_value(_record.input.clone()).map_err(domain_error)?;
        let (from_wallet, to_wallet) = input.direction.wallets();
        let (source, destination) = if from_wallet == "SPOT" {
            (account.resource.clone(), resource(account))
        } else {
            (resource(account), account.resource.clone())
        };
        let expected_id =
            ClientIdentity::derive("binance.futures.transfer", &input.request_id, "gofin")
                .map_err(domain_error)?;
        let valid_route = if self_wallet {
            input.authority == "self"
                && account.name == authority.name
                && matches!(account.role, AccountRole::Sub)
                && account.parent.is_none()
                && account.email.is_none()
                && request.endpoint == BinanceEndpoint::WalletTransfer
                && request.get("type")
                    == Some(if from_wallet == "SPOT" {
                        "MAIN_UMFUTURE"
                    } else {
                        "UMFUTURE_MAIN"
                    })
        } else {
            input.authority == authority.name
                && (account.name == authority.name
                    || account.parent.as_deref() == Some(authority.name.as_str()))
                && request.endpoint == BinanceEndpoint::UniversalTransfer
                && request.get("clientTranId") == Some(submission.reference.as_str())
                && request.get("fromAccountType") == Some(from_wallet)
                && request.get("toAccountType") == Some(to_wallet)
                && request.get("fromEmail") == account.email.as_deref()
                && request.get("toEmail") == account.email.as_deref()
        };
        if !valid_route
            || input.account != account.name
            || amount(&input.amount)?.checked_cmp(&quantity).is_ne()
            || request.validate().is_err()
            || expected_id.as_str() != submission.reference
            || request.get("asset") != Some("USDT")
            || request.get("amount") != Some(quantity.canonical().as_str())
            || binding["resource"] != json!(resource(account))
            || binding["source"] != json!(source)
            || binding["destination"] != json!(destination)
            || binding["asset"] != json!(asset())
            || binding["fromWallet"] != from_wallet
            || binding["toWallet"] != to_wallet
            || binding["email"] != json!(account.email)
        {
            return Ok(unknown(
                "Persisted margin transfer does not match its exact account, resources or request identity.",
            ));
        }
        self.account_identity(authority).await?;
        self.account_identity(account).await?;
        if account.name != authority.name {
            self.verify_subaccount(account, authority).await?;
        }
        let started = DateTime::parse_from_rfc3339(text(&submission.data, "submittedAt")?)
            .map_err(domain_error)?
            .with_timezone(&Utc);
        let end = Utc::now();
        if started > end || end - started >= Duration::days(7) {
            return Ok(unknown(
                "Margin transfer is outside the bounded history window; reconcile the original identity manually.",
            ));
        }
        let start_ms = started.timestamp_millis().max(0) as u64;
        // Own-wallet history observed from Binance has whole-second precision.
        // Include that preceding second while still matching only the exact
        // venue receipt captured during dispatch, never amount/time alone.
        let start_ms = if self_wallet {
            start_ms.saturating_sub(1_000)
        } else {
            start_ms
        };
        let end_ms = end.timestamp_millis().max(0) as u64;
        let row = if self_wallet {
            match self
                .self_wallet_history(_record, account, &request, start_ms, end_ms)
                .await
            {
                Ok(row) => row,
                Err(reason) => return Ok(unknown(reason)),
            }
        } else {
            let mut found = Vec::new();
            let mut complete = false;
            let mut fetched = 0u64;
            let mut expected_total = None;
            for page in 1..=20u64 {
                let mut history = BinanceRequest::new(BinanceEndpoint::UniversalTransferHistory)
                    .push("startTime", start_ms.to_string())
                    .push("endTime", end_ms.to_string())
                    .push("clientTranId", submission.reference.clone())
                    .push("page", page.to_string())
                    .push("limit", "500");
                if let Some(email) = &account.email {
                    // The GET history endpoint forbids sending both email selectors.
                    history = history.push("fromEmail", email);
                }
                let response = match self.call(authority, history, "reconciliation").await {
                    Ok(value) => value,
                    Err(_) => {
                        return Ok(unknown(
                            "Margin transfer history is unavailable; retain the original identity.",
                        ));
                    }
                };
                let batch: TransferHistoryResponse = match parse_response(response) {
                    Ok(value) => value,
                    Err(_) => return Ok(unknown("Margin transfer history is malformed.")),
                };
                if batch.result.len() > 500
                    || (expected_total.is_some() && batch.total_count != expected_total)
                {
                    return Ok(unknown(
                        "Margin transfer history pagination is inconsistent.",
                    ));
                }
                expected_total = batch.total_count;
                let page_len = batch.result.len();
                fetched += page_len as u64;
                found.extend(batch.result.into_iter().filter(|row| {
                    row.client_tran_id.as_deref() == Some(submission.reference.as_str())
                }));
                if found.len() > 1 {
                    return Ok(unknown(
                        "Multiple matching margin transfer history rows are ambiguous.",
                    ));
                }
                if batch.total_count.is_some_and(|total| fetched > total) {
                    return Ok(unknown("Margin transfer history count is inconsistent."));
                }
                if batch.total_count == Some(fetched)
                    || (page_len < 500 && batch.total_count.is_none())
                {
                    complete = true;
                    break;
                }
                if page_len < 500 {
                    break;
                }
            }
            if !complete || found.len() != 1 {
                return Ok(unknown(
                    "One matching margin transfer was not proven in complete bounded history; do not replay.",
                ));
            }
            found.remove(0)
        };
        let email_matches = if let Some(email) = &account.email {
            row.from_email.as_ref() == Some(email) && row.to_email.as_ref() == Some(email)
        } else {
            row.from_email == row.to_email
                && row
                    .from_email
                    .as_deref()
                    .is_none_or(|email| !email.is_empty())
        };
        if row.asset != "USDT"
            || row.amount.checked_cmp(&quantity).is_ne()
            || row.from_account_type != from_wallet
            || row.to_account_type != to_wallet
            || !email_matches
            || row.timestamp < start_ms
            || row.timestamp > end_ms
        {
            return Ok(unknown(
                "Margin transfer history does not match the exact asset, quantity, wallets, account and submission time.",
            ));
        }
        let (spot_now, futures_now, _) =
            match self.futures_transfer_balances(account, authority).await {
                Ok(value) => value,
                Err(_) => {
                    return Ok(unknown(
                        "Independent margin transfer wallet balances are unavailable.",
                    ));
                }
            };
        let (source_now, destination_now) = if from_wallet == "SPOT" {
            (&spot_now, &futures_now)
        } else {
            (&futures_now, &spot_now)
        };
        let source_before = dec(binding, "sourceBefore")?;
        let destination_before = dec(binding, "destinationBefore")?;
        let mut evidence = json!({"clientTranId":submission.reference,"venueId":row.tran_id.as_str(),"account":account.name,"asset":"USDT","amount":quantity.canonical(),"fromWallet":from_wallet,"toWallet":to_wallet,"venueStatus":row.status,"historyComplete":true,"sourceBefore":source_before.canonical(),"sourceAfter":source_now.canonical(),"destinationBefore":destination_before.canonical(),"destinationAfter":destination_now.canonical(),"futuresBalanceField":"walletBalance"});
        if self_wallet {
            evidence
                .as_object_mut()
                .expect("object")
                .remove("clientTranId");
            evidence["localSubmissionId"] = json!(submission.reference);
            evidence["route"] = json!("self_wallet");
        }
        if row.status == "FAILED" || row.status == "FAILURE" {
            if source_before.checked_cmp(source_now).is_ne()
                || destination_before.checked_cmp(destination_now).is_ne()
            {
                return Ok(unknown(
                    "Failed margin transfer has unexplained wallet balance movements.",
                ));
            }
            return Ok(Reconciliation {
                status: Status::Failed,
                verified: Some(make_verified(submission.clone(), vec![], evidence.clone())?),
                evidence,
            });
        }
        if row.status != if self_wallet { "CONFIRMED" } else { "SUCCESS" } {
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence,
                verified: None,
            });
        }
        let debit = decimal_delta(&source_before, source_now);
        let credit = decimal_delta(destination_now, &destination_before);
        if !debit
            .as_ref()
            .is_some_and(|v| v.checked_cmp(&quantity).is_eq())
            || !credit
                .as_ref()
                .is_some_and(|v| v.checked_cmp(&quantity).is_eq())
        {
            return Ok(unknown(
                "Successful history lacks exact independent source debit and destination wallet credit; concurrent activity or funding may need reconciliation.",
            ));
        }
        let effects = vec![FinancialEffect::Transfer {
            from: source,
            to: destination,
            debit: amt(&quantity)?,
            credit: amt(&quantity)?,
        }];
        Ok(Reconciliation {
            status: Status::Completed,
            verified: Some(make_verified(
                submission.clone(),
                effects,
                evidence.clone(),
            )?),
            evidence,
        })
    }
}
