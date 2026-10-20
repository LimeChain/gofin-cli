//! Coinbase App's asynchronous crypto-send flow for an enrolled native USDC
//! destination. The venue determines the actual fee after dispatch; the user
//! explicitly opts into that fee mode and supplies an accounting reserve.
use super::*;

use crate::funding::{CoinbaseUsdcNetwork, usdc_withdrawal_network};

fn withdrawal_network(binding: &Value) -> Result<&'static CoinbaseUsdcNetwork> {
    binding["network"]
        .as_str()
        .and_then(usdc_withdrawal_network)
        .ok_or_else(|| {
            failure(
                "COINBASE_WITHDRAWAL_BINDING_INVALID",
                "Withdrawal network is not a supported native-USDC route.",
            )
        })
}

fn withdrawal_body(
    network: &str,
    to: &str,
    amount: &str,
    idem: &str,
    travel: Option<&Value>,
) -> Result<String> {
    let mut body = json!({"type":"send","to":to,"amount":amount,"currency":"USDC","network":network,"idem":idem});
    if let Some(travel) = travel {
        let name = travel["beneficiary_name"].as_str().unwrap_or_default();
        let country = travel
            .pointer("/beneficiary_address/country")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if name.trim().is_empty()
            || name.chars().any(char::is_control)
            || name.chars().count() > 200
            || country.len() != 2
            || !country.bytes().all(|b| b.is_ascii_uppercase())
            || travel["beneficiary_wallet_type"] != "WALLET_TYPE_SELF_HOSTED"
            || travel["is_self"] != "IS_SELF_TRUE"
        {
            return Err(GoFinError::validation(
                "travelRuleData must describe this self-hosted wallet using a beneficiary name without control characters and an uppercase ISO alpha-2 country code.",
            ));
        }
        body["travel_rule_data"] = travel.clone();
    }
    gofin_domain::canonical_json(&body)
}

impl CoinbaseProfile {
    pub(super) async fn prepare_withdrawal(&self, input: &Value) -> Result<PreparedOperation> {
        self.prepare_withdrawal_with_funding_policy(input, false)
            .await
    }

    pub(super) async fn prepare_withdrawal_with_funding_policy(
        &self,
        input: &Value,
        allow_future_funds: bool,
    ) -> Result<PreparedOperation> {
        let (account, _, input) = self.decode("coinbase.withdraw", input)?;
        let route = withdrawal_network(&input)?;
        if account.product != CoinbaseProduct::Advanced || !account.identity_verified {
            return Err(failure(
                "COINBASE_IDENTITY_UNVERIFIED",
                "Withdrawal requires a verified Advanced portfolio identity.",
            ));
        }
        self.verify_advanced_key(account, false, true).await?;

        let account_id = input["accountId"].as_str().expect("closed schema");
        uuid::Uuid::parse_str(account_id).map_err(|_| {
            failure(
                "COINBASE_APP_ACCOUNT_IDENTITY_MISMATCH",
                "App account ID must be the exact enrolled UUID.",
            )
        })?;
        self.verify_v2_account_scope(account, account_id).await?;
        let source_balance = self
            .advanced_currency_account_balance(account, account_id, "USDC")
            .await?;

        let amount = input["amount"].as_str().expect("closed schema");
        let fee_reserve = input["feeReserveUsdc"].as_str().expect("closed schema");
        let amount_atoms = exact_atoms(amount, 6)?;
        let reserve_atoms = exact_atoms(fee_reserve, 6)?;
        if amount_atoms.is_zero() {
            return Err(GoFinError::validation(
                "Withdrawal amount must be positive.",
            ));
        }
        if reserve_atoms.is_zero() {
            return Err(GoFinError::validation(
                "feeReserveUsdc must be a positive finite accounting reservation.",
            ));
        }
        // Venue cash can retain sub-micro USDC after trading fees. Only the
        // onchain principal is constrained to the token's six decimals.
        let required_balance =
            exact_atoms(amount, 36)?.checked_add(&exact_atoms(fee_reserve, 36)?)?;
        let available_balance = exact_atoms(&source_balance, 36)?;
        if !allow_future_funds && available_balance < required_balance {
            return Err(failure(
                "COINBASE_WITHDRAWAL_RESERVE_UNAVAILABLE",
                "Available USDC does not cover the principal plus the selected fee accounting reserve.",
            ));
        }

        let destination_name = input["destination"].as_str().expect("closed schema");
        let address = input["address"].as_str().expect("closed schema");
        let verifier = self.recipient_verifier.as_ref().ok_or_else(|| {
            failure(
                "COINBASE_RECIPIENT_VERIFIER_UNAVAILABLE",
                "An enrolled wallet and native USDC identity must be verified before withdrawal.",
            )
        })?;
        let destination = verifier
            .resolve(
                route.network,
                "USDC",
                Some(route.contract),
                address,
                Some(destination_name),
            )
            .await?
            .ok_or_else(|| {
                failure(
                    "COINBASE_DESTINATION_UNVERIFIED",
                    "The selected enrolled wallet could not be resolved as native USDC on the selected network.",
                )
            })?;
        if !route.matches_destination(&destination)
            || !destination.address.eq_ignore_ascii_case(address)
        {
            return Err(failure(
                "COINBASE_DESTINATION_IDENTITY_MISMATCH",
                "Destination must be the exact enrolled wallet and native USDC contract on the selected network.",
            ));
        }

        let idem = uuid::Uuid::new_v4().to_string();
        let travel = input.get("travelRuleData");
        let request_body =
            withdrawal_body(route.network, &destination.address, amount, &idem, travel)?;
        let destination_value = serde_json::to_value(&destination).map_err(|_| {
            failure(
                "COINBASE_DESTINATION_BINDING_INVALID",
                "The verified destination could not be bound to this withdrawal.",
            )
        })?;
        let source_asset = product_asset("USDC");
        let destination_atoms = exact_atoms(amount, destination.asset.decimals)?;
        let source_resource = account.resource.clone();
        let fee_reserve_source = exact_atoms(fee_reserve, source_asset.decimals)?;
        let debits = vec![ResourceAmount {
            resource: source_resource.clone(),
            amount: Amount {
                asset: source_asset.clone(),
                atoms: exact_atoms(amount, source_asset.decimals)?,
            },
        }];
        let costs = vec![gofin_domain::CostBound {
            location: AccountingLocation::Cash {
                resource: source_resource.clone(),
            },
            amount: Amount {
                asset: source_asset.clone(),
                atoms: fee_reserve_source.clone(),
            },
            treatment: CostTreatment::VenueCalculatedAdditionalDebit,
        }];
        let valuation = self.financial_valuation(&debits, &costs).await?;
        let mut prepared = PreparedOperation {
            operation: "coinbase.withdraw".into(),
            input: input.clone(),
            binding: json!({
                "version":1,"provider":"coinbase","account":account.name,
                "resource":account.resource,"physicalIdentity":account.physical_identity,
                "portfolioId":account.portfolio_id,"currencyAccountId":account_id,
                "network":route.network,"currency":"USDC","amount":amount,
                "amountAtoms":exact_atoms(amount, source_asset.decimals)?,
                "feeMode":"venue_calculated","feeReserveUsdc":fee_reserve,
                "feeReserveAtoms":fee_reserve_source,"destinationName":destination_name,
                "destinationAddress":destination.address,"destination":destination_value,
                "requestId":input["requestId"],"idem":idem,
                "travelRuleData":travel.cloned().unwrap_or(Value::Null),
                "requestBodyExact":request_body,"sourceAvailableBefore":source_balance
            }),
            summary: format!(
                "Send {amount} USDC from Coinbase to enrolled wallet {destination_name} on {}; Coinbase calculates the network fee.", route.network
            ),
            checks: vec![
                Check {
                    name: "transfer_permission".into(),
                    status: CheckStatus::Passed,
                    detail: "The Advanced key's current transfer permission and enrolled portfolio were verified.".into(),
                },
                Check {
                    name: "source_account_identity".into(),
                    status: CheckStatus::Passed,
                    detail: "The exact App USDC account UUID was intersected with the key-scoped Advanced portfolio account and its available balance.".into(),
                },
                Check {
                    name: "destination_identity".into(),
                    status: CheckStatus::Passed,
                    detail: "The exact enrolled wallet and native USDC contract were verified.".into(),
                },
            ],
            warnings: vec![
                "The selected network and enrolled receipt observer are supported by GoFin; Coinbase may still reject the send because of account eligibility or current network availability. Never substitute another network for an approved send.".into(),
                "Coinbase calculates the network fee when the send is processed. feeReserveUsdc is a finite accounting and policy reservation, not a fee estimate or venue-enforced cap; the actual fee will be recorded from the original transaction afterward.".into(),
                "Coinbase sends are asynchronous and may be delayed or cancelled; reconcile this original send identity and do not replay an uncertain request.".into(),
            ],
            observed_at: gofin_domain::now(),
            expires_at: (Utc::now() + Duration::seconds(45)).to_rfc3339(),
            valuation: Some(valuation),
            intent: FinancialIntent {
                debits,
                maximum_costs: costs,
                expected: vec![EffectConstraint::Transfer {
                    from: source_resource.clone(),
                    to: destination.resource.clone(),
                    debit_asset: source_asset,
                    credit_asset: destination.asset.clone(),
                    maximum_debit: exact_atoms(amount, 36)?,
                    minimum_credit: destination_atoms,
                }],
                locks: vec![source_resource, destination.resource.clone()],
            },
            operation_version: 1,
            action_deadline: None,
        };
        gofin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }

    pub(super) async fn plan_withdrawal_submission(
        &self,
        current: &PreparedOperation,
    ) -> Result<PlannedSubmission<CoinbaseDispatch>> {
        if current.operation != "coinbase.withdraw"
            || current.binding["feeMode"] != "venue_calculated"
            || current.binding["requestBodyExact"].as_str().is_none()
        {
            return Err(failure(
                "COINBASE_WITHDRAWAL_BINDING_INVALID",
                "Withdrawal candidate is not bound to explicit Coinbase-calculated fee consent.",
            ));
        }
        let account = current.binding["account"].as_str().ok_or_else(|| {
            failure(
                "COINBASE_WITHDRAWAL_BINDING_INVALID",
                "Source account is missing.",
            )
        })?;
        let cfg = self.account(account, CoinbaseProduct::Advanced)?;
        self.verify_advanced_key(cfg, false, true).await?;
        self.verify_v2_account_scope(
            cfg,
            current.binding["currencyAccountId"]
                .as_str()
                .unwrap_or_default(),
        )
        .await?;
        let available = self
            .advanced_currency_account_balance(
                cfg,
                current.binding["currencyAccountId"]
                    .as_str()
                    .unwrap_or_default(),
                "USDC",
            )
            .await?;
        let amount = current.binding["amount"].as_str().ok_or_else(|| {
            failure(
                "COINBASE_WITHDRAWAL_BINDING_INVALID",
                "Principal is missing.",
            )
        })?;
        let reserve = current.binding["feeReserveUsdc"].as_str().ok_or_else(|| {
            failure(
                "COINBASE_WITHDRAWAL_BINDING_INVALID",
                "Fee reserve is missing.",
            )
        })?;
        let required = exact_atoms(amount, 36)?.checked_add(&exact_atoms(reserve, 36)?)?;
        if exact_atoms(&available, 36)? < required {
            return Err(failure(
                "COINBASE_WITHDRAWAL_BALANCE_CHANGED",
                "Fresh available USDC no longer covers the principal plus accounting reserve.",
            ));
        }
        self.verify_withdrawal_destination(&current.binding).await?;
        let submission = gofin_domain::Submission {
            kind: "coinbase".into(),
            reference: current.binding["idem"].as_str().unwrap_or_default().into(),
            data: json!({"product":"coinbase","account":account,"binding":current.binding}),
        };
        if uuid::Uuid::parse_str(&submission.reference).is_err() {
            return Err(failure(
                "COINBASE_WITHDRAWAL_BINDING_INVALID",
                "Broker-generated Coinbase idempotency UUID is invalid.",
            ));
        }
        Ok(PlannedSubmission {
            submission: submission.clone(),
            request: CoinbaseDispatch {
                product: CoinbaseProduct::Advanced,
                endpoint: CoinbaseRoute::TransactionSend {
                    account_id: current.binding["currencyAccountId"]
                        .as_str()
                        .unwrap_or_default()
                        .into(),
                },
                account: account.into(),
                exact_body: current.binding["requestBodyExact"]
                    .as_str()
                    .expect("checked above")
                    .into(),
                submission,
                candidate: current.clone(),
            },
        })
    }

    async fn verify_withdrawal_destination(
        &self,
        b: &Value,
    ) -> Result<crate::funding::CoinbaseDestination> {
        let route = withdrawal_network(b)?;
        let verifier = self.recipient_verifier.as_ref().ok_or_else(|| {
            failure(
                "COINBASE_RECIPIENT_VERIFIER_UNAVAILABLE",
                "Enrolled wallet verification is unavailable.",
            )
        })?;
        let dest: crate::funding::CoinbaseDestination =
            serde_json::from_value(b["destination"].clone()).map_err(|_| {
                failure(
                    "COINBASE_DESTINATION_BINDING_INVALID",
                    "Bound destination is invalid.",
                )
            })?;
        let live = verifier
            .resolve(
                route.network,
                "USDC",
                Some(route.contract),
                b["destinationAddress"].as_str().unwrap_or_default(),
                b["destinationName"].as_str(),
            )
            .await?;
        if live.as_ref().is_none_or(|v| {
            !v.enrolled
                || v.address != dest.address
                || v.resource != dest.resource
                || v.asset != dest.asset
                || !route.matches_destination(v)
                || !route.matches_destination(&dest)
        }) {
            return Err(failure(
                "COINBASE_DESTINATION_IDENTITY_MISMATCH",
                "The exact enrolled selected-network native-USDC destination no longer matches.",
            ));
        }
        Ok(dest)
    }

    pub(super) async fn revalidate_withdrawal_before_dispatch(
        &self,
        candidate: &PreparedOperation,
    ) -> Result<()> {
        let b = &candidate.binding;
        let route = withdrawal_network(b)?;
        if candidate.operation != "coinbase.withdraw"
            || b["feeMode"] != "venue_calculated"
            || b["currency"] != "USDC"
        {
            return Err(failure(
                "COINBASE_WITHDRAWAL_BINDING_INVALID",
                "Withdrawal binding is invalid.",
            ));
        }
        let account = self.account(
            b["account"].as_str().unwrap_or_default(),
            CoinbaseProduct::Advanced,
        )?;
        self.verify_advanced_key(account, false, true).await?;
        self.verify_v2_account_scope(account, b["currencyAccountId"].as_str().unwrap_or_default())
            .await?;
        let available = self
            .advanced_currency_account_balance(
                account,
                b["currencyAccountId"].as_str().unwrap_or_default(),
                "USDC",
            )
            .await?;
        let needed = exact_atoms(b["amount"].as_str().unwrap_or_default(), 36)?.checked_add(
            &exact_atoms(b["feeReserveUsdc"].as_str().unwrap_or_default(), 36)?,
        )?;
        if exact_atoms(&available, 36)? < needed {
            return Err(failure(
                "COINBASE_WITHDRAWAL_BALANCE_CHANGED",
                "Available balance no longer covers the principal and selected accounting reserve.",
            ));
        }
        let dest = self.verify_withdrawal_destination(b).await?;
        let travel = b.get("travelRuleData").filter(|v| !v.is_null());
        let body = withdrawal_body(
            route.network,
            &dest.address,
            b["amount"].as_str().unwrap_or_default(),
            b["idem"].as_str().unwrap_or_default(),
            travel,
        )?;
        if b["requestBodyExact"].as_str() != Some(&body) {
            return Err(failure(
                "COINBASE_WITHDRAWAL_BINDING_INVALID",
                "Exact native request body changed after approval.",
            ));
        }
        Ok(())
    }

    pub(super) async fn reconcile_withdrawal(
        &self,
        record: &OperationRecord,
        submission: &gofin_domain::Submission,
    ) -> Result<Reconciliation> {
        let b = &submission.data["binding"];
        let Ok(route) = withdrawal_network(b) else {
            return Ok(coinbase_unknown(
                "immutable withdrawal network is unsupported",
            ));
        };
        if submission.kind != "coinbase"
            || submission.data["product"] != "coinbase"
            || record.operation != "coinbase.withdraw"
            || record
                .preview
                .as_ref()
                .is_none_or(|p| p.operation != record.operation || p.binding != *b)
            || !self.configuration_binding_matches(b)
            || b["idem"] != submission.reference
            || b["feeMode"] != "venue_calculated"
            || b["currency"] != "USDC"
        {
            return Ok(coinbase_unknown(
                "withdrawal differs from its immutable approved submission",
            ));
        }
        let (http, receipt) = match record
            .dispatch_evidence
            .as_ref()
            .and_then(captured_configuration_response)
        {
            Some(v) => v,
            None if record.dispatch_evidence.is_none() => (0, Value::Null),
            None => {
                return Ok(coinbase_unknown(
                    "captured send receipt digest or bytes are invalid",
                ));
            }
        };
        if (400..500).contains(&http)
            && receipt
                .pointer("/errors/0/id")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            && !matches!(http, 408 | 409 | 429)
        {
            let mut v = VerifiedEffects {
                evidence_version: 1,
                submission: submission.clone(),
                effects: Vec::new(),
                evidence: json!({"nativeRejection":receipt,"httpStatus":http}),
                observed_at: gofin_domain::now(),
                proof_digest: String::new(),
            };
            v.proof_digest = gofin_domain::verified_effect_digest(&v)?;
            return Ok(Reconciliation {
                status: Status::Failed,
                evidence: v.evidence.clone(),
                verified: Some(v),
            });
        }
        let txid = receipt
            .pointer("/data/id")
            .and_then(Value::as_str)
            .or_else(|| receipt.get("id").and_then(Value::as_str));
        if txid.is_some()
            && (receipt
                .pointer("/data/idem")
                .and_then(Value::as_str)
                .is_some_and(|i| i != submission.reference)
                || receipt
                    .pointer("/data/type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t != "send"))
        {
            return Ok(coinbase_unknown(
                "accepted receipt does not identify the original idempotent send",
            ));
        }
        let found_id;
        let txid = if let Some(id) = txid {
            id
        } else {
            // A lost/malformed accepted response may still be correlated by the
            // broker-generated native idempotency key. Require exactly one hit.
            let mut cursor: Option<String> = None;
            let mut seen = std::collections::BTreeSet::new();
            let mut matches: Vec<String> = Vec::new();
            let mut complete = false;
            for _ in 0..64 {
                let query = CoinbaseQuery {
                    starting_after: cursor.clone(),
                    limit: Some(100),
                    ..Default::default()
                };
                let resp = self
                    .request_for_account(
                        b["account"].as_str().unwrap_or_default(),
                        CoinbaseProduct::Advanced,
                        CoinbaseRoute::Transactions {
                            account_id: b["currencyAccountId"].as_str().unwrap_or_default().into(),
                            query,
                        },
                        CoinbaseRequest::get(),
                        None,
                    )
                    .await;
                let Ok(resp) = resp else {
                    return Ok(coinbase_unknown("original idem lookup failed"));
                };
                if !(200..300).contains(&resp.status) {
                    return Ok(coinbase_unknown(
                        "original idem lookup is not yet conclusive",
                    ));
                }
                let page: Value = match serde_json::from_slice(&resp.body) {
                    Ok(v) => v,
                    Err(_) => {
                        return Ok(coinbase_unknown(
                            "original idem lookup response was invalid",
                        ));
                    }
                };
                let Some(rows) = page.pointer("/data").and_then(Value::as_array) else {
                    return Ok(coinbase_unknown(
                        "original idem lookup omitted transaction rows",
                    ));
                };
                for row in rows.iter().filter(|r| {
                    r.get("idem").and_then(Value::as_str) == Some(submission.reference.as_str())
                }) {
                    if let Some(id) = row.get("id").and_then(Value::as_str) {
                        matches.push(id.to_string());
                    }
                }
                if matches.len() > 1 {
                    return Ok(coinbase_unknown(
                        "multiple transactions matched the original idempotency key",
                    ));
                }
                let Some(pagination) = page.get("pagination").and_then(Value::as_object) else {
                    return Ok(coinbase_unknown(
                        "native idem search omitted pagination evidence",
                    ));
                };
                match pagination.get("next_starting_after") {
                    Some(Value::Null) => {
                        complete = true;
                        break;
                    }
                    Some(Value::String(next)) if next.is_empty() => {
                        complete = true;
                        break;
                    }
                    Some(Value::String(next))
                        if Some(next) != cursor.as_ref() && seen.insert(next.clone()) =>
                    {
                        cursor = Some(next.clone())
                    }
                    _ => {
                        return Ok(coinbase_unknown(
                            "native idem search pagination was malformed or repeated a cursor",
                        ));
                    }
                }
            }
            if !complete || matches.len() != 1 {
                return Ok(coinbase_unknown(
                    "bounded native idem search was incomplete or did not identify exactly one transaction",
                ));
            }
            let id = &matches[0];
            found_id = id.to_string();
            &found_id
        };
        let response = match self
            .request_for_account(
                b["account"].as_str().unwrap_or_default(),
                CoinbaseProduct::Advanced,
                CoinbaseRoute::Transaction {
                    account_id: b["currencyAccountId"].as_str().unwrap_or_default().into(),
                    transaction_id: txid.into(),
                },
                CoinbaseRequest::get(),
                None,
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Ok(coinbase_unknown_request(
                    "original transaction lookup failed",
                    &e,
                ));
            }
        };
        if !(200..300).contains(&response.status) {
            return Ok(coinbase_unknown("original transaction is not yet readable"));
        }
        let tx: Value = match serde_json::from_slice(&response.body) {
            Ok(v) => v,
            Err(_) => {
                return Ok(coinbase_unknown(
                    "original transaction response was not valid JSON",
                ));
            }
        };
        let d = tx.get("data").unwrap_or(&Value::Null);
        let status = d
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        if ["pending", "processing", "created", "queued"].contains(&status.as_str()) {
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"transaction":tx}),
                verified: None,
            });
        }
        if status != "completed" {
            return Ok(coinbase_unknown(
                "original transaction has not completed successfully",
            ));
        }
        let n = d.get("network").unwrap_or(&Value::Null);
        let amount = n
            .pointer("/transaction_amount/amount")
            .and_then(Value::as_str);
        let fee = n.pointer("/transaction_fee/amount").and_then(Value::as_str);
        let signed = d.pointer("/amount/amount").and_then(Value::as_str);
        let hash = n.get("hash").and_then(Value::as_str);
        let to = d.pointer("/to/address").and_then(Value::as_str);
        let principal = b["amount"].as_str().unwrap_or_default();
        let (Some(fee), Some(signed), Some(hash), Some(to)) = (fee, signed, hash, to) else {
            return Ok(coinbase_unknown(
                "completed send omitted actual fee, source debit, destination, or tx hash",
            ));
        };
        if d.get("id").and_then(Value::as_str) != Some(txid)
            || d.get("type").and_then(Value::as_str) != Some("send")
            || (n.get("transaction_amount").is_some()
                && (amount.is_none()
                    || n.pointer("/transaction_amount/currency")
                        .and_then(Value::as_str)
                        != Some("USDC")
                    || !decimal_sum_equals(principal, "0", amount.unwrap_or_default())?))
            || n.pointer("/transaction_fee/currency")
                .and_then(Value::as_str)
                != Some("USDC")
            || d.pointer("/amount/currency").and_then(Value::as_str) != Some("USDC")
            || !decimal_sum_equals(principal, fee, signed.strip_prefix('-').unwrap_or_default())?
            || !signed.starts_with('-')
            || !to.eq_ignore_ascii_case(b["destinationAddress"].as_str().unwrap_or_default())
            || n.get("network_name")
                .and_then(Value::as_str)
                .is_none_or(|v| !v.eq_ignore_ascii_case(route.network))
            || hash.len() != 66
            || !hash.starts_with("0x")
            || !hash[2..].bytes().all(|c| c.is_ascii_hexdigit())
            || exact_atoms(fee, 6).is_err()
            || d.get("idem")
                .and_then(Value::as_str)
                .is_some_and(|v| v != submission.reference)
            || d.get("resource_path")
                .and_then(Value::as_str)
                .is_some_and(|v| {
                    !v.ends_with(&format!(
                        "/accounts/{}/transactions/{}",
                        b["currencyAccountId"].as_str().unwrap_or_default(),
                        txid
                    ))
                })
        {
            return Ok(coinbase_unknown(
                "completed send identity, network, or debit arithmetic did not match the approved withdrawal",
            ));
        }
        let verifier = self.recipient_verifier.as_ref().ok_or_else(|| {
            failure(
                "COINBASE_RECIPIENT_VERIFIER_UNAVAILABLE",
                "Independent selected-network settlement verification is unavailable.",
            )
        })?;
        let dest: crate::funding::CoinbaseDestination =
            serde_json::from_value(b["destination"].clone()).map_err(|_| {
                failure(
                    "COINBASE_DESTINATION_BINDING_INVALID",
                    "Bound destination is invalid.",
                )
            })?;
        let live = verifier
            .resolve(
                route.network,
                "USDC",
                Some(route.contract),
                &dest.address,
                b["destinationName"].as_str(),
            )
            .await?;
        if live.as_ref().is_none_or(|v| {
            !v.enrolled
                || v.address != dest.address
                || v.resource != dest.resource
                || v.asset != dest.asset
                || !route.matches_destination(v)
                || !route.matches_destination(&dest)
        }) {
            return Ok(coinbase_unknown(
                "enrolled destination identity changed before settlement verification",
            ));
        }
        let payout = verifier
            .verify(crate::funding::CoinbasePayoutRequest {
                destination: dest.clone(),
                tx_hash: hash.into(),
                minimum: principal.into(),
                maximum: principal.into(),
            })
            .await?;
        if payout.status != Status::Completed
            || payout.amount.as_ref().is_none_or(|q| {
                q.as_str() != exact_atoms(principal, 6).unwrap_or_default().to_string()
            })
        {
            return Ok(Reconciliation {
                status: Status::Unknown,
                verified: None,
                evidence: json!({
                    "reason":"independent destination receipt does not prove the exact principal",
                    "transactionId":txid,"txHash":hash,"actualFeeUsdc":fee,
                    "signedSourceDebitUsdc":signed,"destinationStatus":payout.status,
                    "destinationAmountAtoms":payout.amount,"destinationReceipt":payout.evidence,
                    "resubmissionAllowed":false
                }),
            });
        }
        let source = self
            .account(
                b["account"].as_str().unwrap_or_default(),
                CoinbaseProduct::Advanced,
            )?
            .resource
            .clone();
        let mut verified = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects: vec![
                FinancialEffect::Transfer {
                    from: source.clone(),
                    to: dest.resource.clone(),
                    debit: Amount {
                        asset: product_asset("USDC"),
                        atoms: exact_atoms(principal, 36)?,
                    },
                    credit: Amount {
                        asset: dest.asset,
                        atoms: exact_atoms(principal, 6)?,
                    },
                },
                FinancialEffect::Cost {
                    location: AccountingLocation::Cash { resource: source },
                    amount: Amount {
                        asset: product_asset("USDC"),
                        atoms: exact_atoms(fee, 36)?,
                    },
                    category: "venue_withdrawal_fee".into(),
                    treatment: CostTreatment::VenueCalculatedAdditionalDebit,
                },
            ],
            evidence: json!({"transaction":tx,"transactionId":txid,"txHash":hash,"actualFeeUsdc":fee,"signedSourceDebitUsdc":signed,"feeReserveUsdc":b["feeReserveUsdc"],"feeWithinReserve":exact_atoms(fee,6)? <= exact_atoms(b["feeReserveUsdc"].as_str().unwrap_or_default(),6)?,"destinationReceipt":payout.evidence}),
            observed_at: gofin_domain::now(),
            proof_digest: String::new(),
        };
        verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
        Ok(Reconciliation {
            status: Status::Completed,
            evidence: verified.evidence.clone(),
            verified: Some(verified),
        })
    }
}
