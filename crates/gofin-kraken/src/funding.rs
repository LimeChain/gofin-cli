//! Kraken funding and Earn interpretation. Original API versions, receipt IDs,
//! resolved routes and explicit verification requirements survive reconciliation.
//! No history amount/time search is allowed to replace a lost withdrawal ID.
use crate::{KrakenAccount, KrakenEndpoint as E, KrakenProfile, contracts};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use gofin_domain::{
    AccountingLocation, Amount, AssetId, Check, CheckStatus, CostBound, CostTreatment, Direction,
    EffectConstraint, FinancialEffect, FinancialIntent, GoFinError, OperationRecord,
    PreparedOperation, Quantity, Reconciliation, ResourceAmount, ResourceId, Result,
    SignedQuantity, Status, Submission, VerifiedEffects, binding_hash, now, verified_effect_digest,
};
use gofin_domain::{Definition, ExecutionActor, OperationKind};
use num_traits::Signed;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationRequirement {
    #[default]
    DestinationReceipt,
    VenueOnly,
}
/// Only broker-created observers resolve exact chain assets and verify receipts.
/// Venue symbols or client-provided decimals never establish destination identity.
#[async_trait]
pub trait KrakenRecipientVerifier: Send + Sync {
    async fn resolve(
        &self,
        network: &str,
        coin: &str,
        contract: Option<&str>,
        address: &str,
        destination_name: Option<&str>,
    ) -> Result<Option<KrakenDestination>>;
    async fn verify(&self, request: KrakenPayoutRequest) -> Result<KrakenPayoutResult>;
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KrakenDestination {
    /// External recipient proof establishes delivery but never custody credit.
    #[serde(default)]
    pub enrolled: bool,
    pub resource: ResourceId,
    pub address: String,
    /// EVM chain identity. Zero for non-EVM destinations added by newer routes.
    #[serde(default)]
    pub chain_id: u64,
    /// Exact destination family. Existing persisted EVM bindings omit this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    /// Solana cluster and SPL mint, when the destination is on Solana.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mint: Option<String>,
    pub asset: AssetId,
    pub confirmations: u32,
}
#[derive(Clone, Debug)]
pub struct KrakenPayoutRequest {
    pub destination: KrakenDestination,
    pub tx_hash: String,
    pub minimum: String,
    pub maximum: String,
}
#[derive(Clone, Debug)]
pub struct KrakenPayoutResult {
    pub status: Status,
    pub amount: Option<Quantity>,
    pub evidence: Value,
}

pub fn is_funding(e: E) -> bool {
    matches!(
        e,
        E::KrakenPrivatePostWithdraw
            | E::KrakenFundingPostFundingV1Withdrawals
            | E::KrakenPrivatePostWithdrawCancel
            | E::KrakenPrivatePostEarnAllocate
            | E::KrakenPrivatePostEarnDeallocate
            | E::KrakenFundingPutFundingV1DepositAddress
            | E::KrakenLegacyCreateDepositAddress
            | E::KrakenPrivatePostWalletTransfer
            | E::KrakenPrivatePostAccountTransfer
            | E::KrakenfuturesPrivatePostTransfer
            | E::KrakenfuturesPrivatePostTransferSubaccount
            | E::KrakenfuturesPrivatePostWithdrawal
    )
}

fn err(code: &str, message: &str) -> GoFinError {
    GoFinError::new(
        code,
        "funding",
        message,
        "Inspect the original operation status and exact funding route; never resubmit an uncertain action.",
    )
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            err(
                "KRAKEN_EVIDENCE_INVALID",
                "A required exact funding field is missing.",
            )
        })
}
fn result(v: &Value) -> Result<&Value> {
    if v.get("error")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
    {
        return Err(err(
            "KRAKEN_REQUEST_REJECTED",
            "Kraken reported a business error.",
        ));
    }
    Ok(v.get("result").filter(|r| !r.is_string()).unwrap_or(v))
}
fn qty(v: &Value, key: &str, decimals: u8) -> Result<Quantity> {
    native_quantity(text(v, key)?, decimals)
}
fn amount(asset: &AssetId, value: &str) -> Result<Amount> {
    Ok(Amount {
        asset: asset.clone(),
        atoms: native_quantity(value, asset.decimals)?,
    })
}
fn positive(asset: &AssetId, value: &str) -> Result<Amount> {
    let a = amount(asset, value)?;
    if a.atoms.is_zero() {
        return Err(GoFinError::validation("Funding amount must be positive."));
    }
    Ok(a)
}
fn unknown(reason: &str) -> Reconciliation {
    Reconciliation {
        status: Status::Unknown,
        evidence: json!({"reason":reason,"resubmissionAllowed":false}),
        verified: None,
    }
}
fn pending(reason: &str) -> Reconciliation {
    Reconciliation {
        status: Status::Pending,
        evidence: json!({"reason":reason,"resubmissionAllowed":false}),
        verified: None,
    }
}
fn terminal(
    submission: &Submission,
    status: Status,
    effects: Vec<FinancialEffect>,
    mut evidence: Value,
) -> Result<Reconciliation> {
    // Kraken includes fractional JSON timestamps in history and ledger rows.
    // Preserve those exact lexemes as strings in proof evidence so canonical
    // hashing cannot fail or round venue data.
    crate::trading::normalize_proof_numbers(&mut evidence)?;
    let mut bundle = VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects,
        evidence: evidence.clone(),
        observed_at: now(),
        proof_digest: String::new(),
    };
    bundle.proof_digest = verified_effect_digest(&bundle)?;
    Ok(Reconciliation {
        status,
        evidence,
        verified: Some(bundle),
    })
}
fn decode<T: for<'de> Deserialize<'de>>(v: &Value) -> Result<T> {
    serde_json::from_value(v.clone()).map_err(|_| {
        err(
            "KRAKEN_BINDING_INVALID",
            "Persisted funding evidence is malformed.",
        )
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WithdrawalBinding {
    api_version: String,
    source: ResourceId,
    asset: AssetId,
    venue_asset_code: String,
    #[serde(default = "currency_class")]
    asset_class: String,
    #[serde(default = "rebased_unit")]
    denomination: String,
    method_id: String,
    network: String,
    address_id: String,
    address: String,
    #[serde(default = "crypto_kind")]
    address_kind: String,
    #[serde(default)]
    destination_details: Value,
    tag: Option<String>,
    memo: Option<String>,
    gross: Quantity,
    net: Quantity,
    fee: Quantity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quoted_fee: Option<Quantity>,
    fee_asset: AssetId,
    fee_venue_code: String,
    fee_asset_class: String,
    verification: VerificationRequirement,
    destination: Option<KrakenDestination>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EarnBinding {
    #[serde(default)]
    accounting_version: u32,
    source: ResourceId,
    strategy: String,
    asset: AssetId,
    amount: Quantity,
    allocate: bool,
    total_before: Quantity,
    ledger_before: Vec<String>,
    lock_type: Value,
    max_fee: Quantity,
    minimum_usd: Option<Quantity>,
    status_before: Value,
    allocations_before: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimBinding {
    method_id: String,
    #[serde(default)]
    current_method_id: Option<String>,
    asset: AssetId,
    fee_asset: AssetId,
    fee: Quantity,
    addresses_before: Vec<String>,
    #[serde(default)]
    ledger_before: Option<Vec<String>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransferBinding {
    from: ResourceId,
    to: ResourceId,
    asset: AssetId,
    credit_asset: AssetId,
    amount: Quantity,
    source_account: String,
    #[serde(default)]
    dispatch_account: String,
    #[serde(default)]
    dispatch_physical_identity: String,
    destination_account: String,
    #[serde(default)]
    source_physical_identity: String,
    #[serde(default)]
    destination_physical_identity: String,
    #[serde(default)]
    source_product: Option<crate::KrakenProduct>,
    #[serde(default)]
    destination_product: Option<crate::KrakenProduct>,
    #[serde(default)]
    source_venue_uid: Option<String>,
    #[serde(default)]
    destination_venue_uid: Option<String>,
    #[serde(default)]
    relationship_iiban: Option<String>,
    from_wallet: String,
    to_wallet: String,
}

fn wallet_rows(response: &Value) -> Result<&Vec<Value>> {
    let data = result(response)?;
    if data["cursor"]["next"]
        .as_str()
        .is_some_and(|v| !v.is_empty())
    {
        return Err(err(
            "KRAKEN_WALLET_COVERAGE_INCOMPLETE",
            "Kraken returned an additional wallet page, but its published request schema provides no cursor. Wallet relationships cannot be proved exhaustively.",
        ));
    }
    let rows = data["accounts"]
        .as_array()
        .filter(|r| r.len() <= 500)
        .ok_or_else(|| {
            err(
                "KRAKEN_WALLET_IDENTITY_INVALID",
                "Authenticated wallet identities are absent or exceed the supported bound.",
            )
        })?;
    let mut ids = std::collections::BTreeSet::new();
    for row in rows {
        let id = text(row, "account_id")?;
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|v| v.is_ascii_alphanumeric() || v == b'-' || v == b'_')
            || !ids.insert(id)
        {
            return Err(err(
                "KRAKEN_WALLET_IDENTITY_INVALID",
                "Authenticated wallet identity is invalid or duplicated.",
            ));
        }
    }
    Ok(rows)
}
/// Authenticate ownership separately from the human-declared default alias.
pub fn validate_default_wallet_relationship(response: &Value, id: &str) -> Result<()> {
    let rows = wallet_rows(response)?;
    let row=rows.iter().find(|row|row["account_id"]==id&&wallet_active(row)).ok_or_else(||err("KRAKEN_DEFAULT_WALLET_RELATIONSHIP_UNVERIFIED", "The protected default-wallet ID is not an active authenticated owned wallet. Update the relationship in human configuration."))?;
    if !matches!(
        row["type"].as_str(),
        Some("main" | "spot" | "pay" | "prop_real")
    ) {
        return Err(err(
            "KRAKEN_WALLET_PRODUCT_UNSUPPORTED",
            "The authenticated default wallet has an unsupported product type.",
        ));
    }
    Ok(())
}
fn wallet_active(row: &Value) -> bool {
    row["status"] == "active" && row["flags"]["active"] == true
}

impl KrakenProfile {
    /// Wallet IDs are venue-owned identities. The default alias must come from
    /// protected enrollment, since ListWalletAccounts does not identify defaults.
    pub(crate) async fn account_scopes(
        &self,
        account: &KrakenAccount,
    ) -> Result<Vec<KrakenAccount>> {
        let Some(default_id) = account.default_wallet_account_id.as_deref() else {
            // The legacy default remains a bounded, exact operational scope.
            return Ok(vec![account.clone()]);
        };
        let response = self
            .json(
                E::KrakenPrivatePostListWalletAccounts,
                Some(account),
                json!({}),
            )
            .await?;
        validate_default_wallet_relationship(&response, default_id)?;
        let rows = wallet_rows(&response)?;
        let mut scopes = Vec::new();
        for row in rows.iter().filter(|row| wallet_active(row)) {
            let id = text(row, "account_id")?;
            // Unknown and paper wallets are never treated as real custodial cash.
            if !matches!(
                row["type"].as_str(),
                Some("main" | "spot" | "pay" | "prop_real")
            ) {
                continue;
            }
            let mut scope = account.clone();
            scope.wallet_account_id = Some(id.into());
            if id != default_id {
                scope.resource = ResourceId(format!("{}:wallet:{id}", account.resource.0));
            }
            scopes.push(scope);
        }
        if !scopes
            .iter()
            .any(|scope| scope.wallet_account_id.as_deref() == Some(default_id))
        {
            return Err(err(
                "KRAKEN_WALLET_PRODUCT_UNSUPPORTED",
                "The authenticated default wallet has an unsupported product type.",
            ));
        }
        Ok(scopes)
    }
    async fn funding_wallet(&self, account: &KrakenAccount, id: &str) -> Result<KrakenAccount> {
        if account.default_wallet_account_id.is_none() {
            return Err(err(
                "KRAKEN_DEFAULT_WALLET_RELATIONSHIP_UNVERIFIED",
                "Kraken does not identify its default-wallet alias. Add the exact default wallet ID through protected human enrollment before selecting account_id.",
            ));
        }
        self.account_scopes(account).await?.into_iter().find(|scope|scope.wallet_account_id.as_deref()==Some(id)).ok_or_else(||err("KRAKEN_WALLET_ACCOUNT_UNAVAILABLE", "The selected account_id is not an active authenticated owned wallet with a supported product."))
    }
    async fn permissions(&self, a: &KrakenAccount, required: &[&str]) -> Result<()> {
        let response = self
            .json(E::KrakenPrivatePostGetApiKeyInfo, Some(a), json!({}))
            .await?;
        let p = result(&response)?
            .get("permissions")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                err(
                    "KRAKEN_PERMISSION_EVIDENCE_MISSING",
                    "Kraken key permissions are unavailable.",
                )
            })?;
        if required
            .iter()
            .any(|r| !p.iter().any(|v| v.as_str() == Some(*r)))
        {
            return Err(err(
                "KRAKEN_PERMISSION_DENIED",
                "This funding operation requires additional Kraken key permissions.",
            ));
        }
        Ok(())
    }
    async fn methods(&self, a: &KrakenAccount, direction: &str) -> Result<Vec<Value>> {
        self.funding_pages(
            a,
            E::KrakenFundingGetFundingV1MethodsDirection,
            json!({"direction":direction,"limit":500}),
            "methods",
        )
        .await
    }
    pub(crate) async fn funding_pages(
        &self,
        a: &KrakenAccount,
        e: E,
        mut p: Value,
        key: &str,
    ) -> Result<Vec<Value>> {
        let mut rows = Vec::new();
        let mut cursors = std::collections::BTreeSet::new();
        for _ in 0..20 {
            let r = self.json(e, Some(a), p.clone()).await?;
            let data = result(&r)?;
            let page = data.get(key).and_then(Value::as_array).ok_or_else(|| {
                err(
                    "KRAKEN_EVIDENCE_INVALID",
                    "Funding list evidence is malformed.",
                )
            })?;
            rows.extend(page.iter().cloned());
            match data
                .get("next_cursor")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                None => return Ok(rows),
                Some(cursor) if cursors.insert(cursor.to_owned()) => p["cursor"] = json!(cursor),
                _ => {
                    return Err(err(
                        "KRAKEN_HISTORY_INCOMPLETE",
                        "Funding pagination repeated a cursor.",
                    ));
                }
            }
        }
        Err(err(
            "KRAKEN_HISTORY_INCOMPLETE",
            "Funding evidence exceeds the bounded pagination scope.",
        ))
    }
    async fn ledger(&self, a: &KrakenAccount, asset: &str) -> Result<Value> {
        let r = self
            .json(
                E::KrakenPrivatePostLedgers,
                Some(a),
                json!({"asset":asset,"without_count":true}),
            )
            .await?;
        let data = result(&r)?;
        let rows = data
            .get("ledger")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                err(
                    "KRAKEN_LEDGER_INVALID",
                    "Kraken did not provide exact ledger evidence.",
                )
            })?;
        Ok(Value::Object(rows.clone()))
    }
    async fn funding_asset(
        &self,
        a: &KrakenAccount,
        code: &str,
        class: &str,
        unit: &str,
    ) -> Result<AssetId> {
        self.funding_asset_inner(a, code, class, unit, None).await
    }
    async fn funding_asset_for_amount(
        &self,
        a: &KrakenAccount,
        code: &str,
        class: &str,
        unit: &str,
        value: &str,
    ) -> Result<AssetId> {
        self.funding_asset_inner(a, code, class, unit, Some(value))
            .await
    }
    async fn funding_asset_inner(
        &self,
        _a: &KrakenAccount,
        code: &str,
        class: &str,
        unit: &str,
        requested: Option<&str>,
    ) -> Result<AssetId> {
        if !matches!(class, "currency" | "tokenized_asset") || !matches!(unit, "base" | "rebased") {
            return Err(GoFinError::validation(
                "Funding requires currency or tokenized_asset with an exact base/rebased denomination.",
            ));
        }
        let response = self
            .json(
                E::KrakenPublicGetAssets,
                None,
                json!({"asset":code,"aclass":class}),
            )
            .await?;
        let metadata = result(&response)?.as_object().ok_or_else(|| {
            err(
                "KRAKEN_ASSET_METADATA_INVALID",
                "Tokenized asset metadata is not a native identity map.",
            )
        })?;
        let matches = metadata
            .iter()
            .filter(|(key, v)| key.as_str() == code || v["altname"].as_str() == Some(code))
            .collect::<Vec<_>>();
        if matches.len() != 1
            || matches[0].1.get("aclass").is_some_and(|v| v != class)
            || (class == "tokenized_asset" && matches[0].1["aclass"] != class)
            || matches[0].1["decimals"].as_u64().is_none_or(|n| n > 36)
        {
            return Err(err(
                "KRAKEN_ASSET_IDENTITY_UNVERIFIED",
                "No unique official tokenized native asset identity and precision was returned.",
            ));
        }
        if let Some(value) = requested {
            native_quantity(value,matches[0].1["decimals"].as_u64().expect("validated precision") as u8).map_err(|_|err("KRAKEN_FUNDING_AMOUNT_PRECISION", "Requested native amount exceeds this exact venue asset's published decimal precision; GoFin never rounds a financial request."))?;
        }
        Ok(AssetId {
            network: if class == "currency" {
                "kraken".into()
            } else {
                format!("kraken:tokenized_asset:{unit}")
            },
            reference: matches[0].0.clone(),
            decimals: 36,
        })
    }
    async fn ledger_asset(&self, a: &KrakenAccount, asset: &AssetId) -> Result<Value> {
        if let Some(unit) = asset.network.strip_prefix("kraken:tokenized_asset:") {
            let response=self.json(E::KrakenPrivatePostLedgers,Some(a),json!({"asset":asset.reference,"aclass":"tokenized_asset","rebase_multiplier":unit,"without_count":true})).await?;
            return result(&response)?
                .get("ledger")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| {
                    err(
                        "KRAKEN_LEDGER_INVALID",
                        "Tokenized ledger evidence is missing in the original denomination.",
                    )
                });
        }
        self.ledger(a, &asset.reference).await
    }
    async fn free(&self, a: &KrakenAccount, asset: &AssetId) -> Result<Quantity> {
        let mut params = json!({});
        if let Some(unit) = asset.network.strip_prefix("kraken:tokenized_asset:") {
            params["rebase_multiplier"] = json!(unit);
        }
        let r = self
            .json(E::KrakenPrivatePostBalanceEx, Some(a), params)
            .await?;
        let data = result(&r)?;
        let row = data.get(&asset.reference).ok_or_else(|| {
            err(
                "KRAKEN_BALANCE_MISSING",
                "The selected funding asset is absent from the account balance.",
            )
        })?;
        let total = qty(row, "balance", asset.decimals)?;
        let hold = row
            .get("hold_trade")
            .or_else(|| row.get("hold_trading"))
            .and_then(Value::as_str)
            .unwrap_or("0");
        // Credit/credit-used are liabilities, never a spendable cash balance.
        let used = row
            .get("credit_used")
            .and_then(Value::as_str)
            .unwrap_or("0");
        total
            .checked_sub(&native_quantity(hold, asset.decimals)?)?
            .checked_sub(&native_quantity(used, asset.decimals)?)
    }
    pub(crate) async fn prepare_funding(&self, e: E, input: &Value) -> Result<PreparedOperation> {
        let (selection, mut parameters) = contracts::decode(e, input)?;
        let enrolled = self.account(
            selection
                .account
                .as_deref()
                .ok_or_else(|| GoFinError::validation("Select a Kraken account."))?,
        )?;
        let scoped = if let Some(id) = parameters.get("account_id").and_then(Value::as_str) {
            self.funding_wallet(enrolled, id).await?
        } else {
            enrolled.clone()
        };
        let account = &scoped;
        let mut binding = json!({"account":account.name,"parameters":parameters,"requestId":input.get("requestId").and_then(Value::as_str).ok_or_else(||GoFinError::validation("A bounded requestId is required."))?,"product":account.product,"physicalIdentity":account.physical_identity,"walletAccountId":account.wallet_account_id,"defaultWalletAccountId":account.default_wallet_account_id,"walletResource":account.resource,"endpoint":e.method()});
        let mut intent = FinancialIntent {
            locks: vec![enrolled.resource.clone(), account.resource.clone()],
            ..Default::default()
        };
        let mut warnings=vec!["A submitted funding action is reconciled by its original identity; it is never automatically retried.".into()];
        let summary;
        match e {
            E::KrakenPrivatePostWithdraw | E::KrakenFundingPostFundingV1Withdrawals => {
                self.permissions(account, &["query-funds", "withdraw-funds", "query-ledger"])
                    .await?;
                let w = self
                    .prepare_withdrawal(account, e, &mut parameters, input)
                    .await?;
                let principal = Amount {
                    asset: w.asset.clone(),
                    atoms: w.gross.clone(),
                };
                if self.free(account, &w.asset).await? < w.gross
                    || (w.fee_asset != w.asset && self.free(account, &w.fee_asset).await? < w.fee)
                {
                    return Err(err(
                        "INSUFFICIENT_BALANCE",
                        "Available Kraken cash cannot cover this withdrawal and its fee.",
                    ));
                }
                intent.debits.push(ResourceAmount {
                    resource: w.source.clone(),
                    amount: principal,
                });
                intent.maximum_costs.push(CostBound {
                    location: AccountingLocation::Cash {
                        resource: w.source.clone(),
                    },
                    amount: Amount {
                        asset: w.fee_asset.clone(),
                        atoms: w.fee.clone(),
                    },
                    treatment: if w.fee_asset == w.asset {
                        CostTreatment::IncludedInDebit
                    } else {
                        CostTreatment::AdditionalDebit
                    },
                });
                intent.expected.push(EffectConstraint::Debit {
                    resource: w.source.clone(),
                    asset: w.asset.clone(),
                    maximum_debit: w.gross.clone(),
                });
                if let Some(d) = &w.destination
                    && d.enrolled
                {
                    intent.locks.push(d.resource.clone());
                    intent.expected.push(EffectConstraint::Credit {
                        to: d.resource.clone(),
                        asset: d.asset.clone(),
                        minimum_credit: Quantity::from_decimal(
                            &w.net.to_decimal(w.asset.decimals),
                            d.asset.decimals,
                        )?,
                    });
                }
                if w.verification == VerificationRequirement::VenueOnly {
                    warnings.push("Approval explicitly permits venue-only verification. Destination receipt remains unverified; GoFin records only proven source debit and fees.".into());
                }
                summary = format!(
                    "Withdraw {} {} from {} to {} on {}; quoted fee {}, maximum fee {} {}, verification {}.",
                    w.net.to_decimal(w.asset.decimals),
                    w.asset.reference,
                    account.name,
                    w.address,
                    w.network,
                    w.quoted_fee
                        .as_ref()
                        .unwrap_or(&w.fee)
                        .to_decimal(w.fee_asset.decimals),
                    w.fee.to_decimal(w.fee_asset.decimals),
                    w.fee_asset.reference,
                    if w.verification == VerificationRequirement::VenueOnly {
                        "venue only"
                    } else {
                        "confirmed destination receipt"
                    }
                );
                binding["funding"] = serde_json::to_value(w)
                    .map_err(|_| GoFinError::validation("Funding binding serialization failed."))?;
            }
            E::KrakenPrivatePostEarnAllocate | E::KrakenPrivatePostEarnDeallocate => {
                self.permissions(account, &["query-funds", "query-ledger", "earn-funds"])
                    .await?;
                let b = self.prepare_earn(account, e, &parameters, input).await?;
                let position = format!("kraken:earn:{}", b.strategy);
                intent.locks.push(ResourceId(format!(
                    "{}:earn:{}",
                    account.resource.0, b.strategy
                )));
                intent.expected.push(EffectConstraint::Position {
                    resource: b.source.clone(),
                    position: position.clone(),
                    asset: b.asset.clone(),
                    direction: if b.allocate {
                        Direction::Increase
                    } else {
                        Direction::Decrease
                    },
                    maximum_change: if b.allocate {
                        b.amount.clone()
                    } else {
                        b.amount.checked_add(&b.max_fee)?
                    },
                });
                let cash_origin = AccountingLocation::Cash {
                    resource: b.source.clone(),
                };
                let position_origin = AccountingLocation::Position {
                    resource: b.source.clone(),
                    position: position.clone(),
                    position_kind: gofin_domain::PositionKind::Supply,
                };
                intent.expected.push(EffectConstraint::OriginMovement {
                    from: if b.allocate {
                        cash_origin.clone()
                    } else {
                        position_origin.clone()
                    },
                    to: if b.allocate {
                        position_origin.clone()
                    } else {
                        cash_origin.clone()
                    },
                    debit_asset: b.asset.clone(),
                    credit_asset: b.asset.clone(),
                    maximum_debit: if b.allocate {
                        b.amount.clone()
                    } else {
                        b.amount.checked_add(&b.max_fee)?
                    },
                    minimum_credit: if b.allocate {
                        b.amount.checked_sub(&b.max_fee)?
                    } else {
                        b.amount.clone()
                    },
                });
                if !b.max_fee.is_zero() {
                    intent.maximum_costs.push(CostBound {
                        location: AccountingLocation::Cash {
                            resource: b.source.clone(),
                        },
                        amount: Amount {
                            asset: b.asset.clone(),
                            atoms: b.max_fee.clone(),
                        },
                        treatment: if b.allocate {
                            CostTreatment::AdditionalDebit
                        } else {
                            CostTreatment::DeductedFromCredit
                        },
                    });
                    intent.maximum_costs.push(CostBound {
                        location: if b.allocate {
                            cash_origin.clone()
                        } else {
                            position_origin.clone()
                        },
                        amount: Amount {
                            asset: b.asset.clone(),
                            atoms: b.max_fee.clone(),
                        },
                        treatment: CostTreatment::IncludedInDebit,
                    });
                }
                if b.allocate {
                    intent.debits.push(ResourceAmount {
                        resource: b.source.clone(),
                        amount: Amount {
                            asset: b.asset.clone(),
                            atoms: b.amount.clone(),
                        },
                    });
                    intent.expected.push(EffectConstraint::Debit {
                        resource: b.source.clone(),
                        asset: b.asset.clone(),
                        maximum_debit: b.amount.clone(),
                    });
                } else {
                    intent.expected.push(EffectConstraint::Credit {
                        to: b.source.clone(),
                        asset: b.asset.clone(),
                        minimum_credit: b.amount.checked_sub(&b.max_fee)?,
                    });
                }
                summary = format!(
                    "{} {} {} {} Earn strategy {} ({})",
                    if b.allocate { "Allocate" } else { "Deallocate" },
                    b.amount.to_decimal(b.asset.decimals),
                    b.asset.reference,
                    if b.allocate { "to" } else { "from" },
                    b.strategy,
                    b.lock_type["type"].as_str().unwrap_or("unknown lock")
                );
                warnings.push("Bonding, exit queues and unbonding can delay usable funds. No completion is inferred from the last-request status alone.".into());
                binding[gofin_domain::BASELINE_ANCHORS_KEY] = json!([
                    gofin_domain::PositionAnchor {
                        resource: b.source.clone(),
                        position: position.clone(),
                        position_kind: gofin_domain::PositionKind::Supply,
                        principal: Amount {
                            asset: b.asset.clone(),
                            atoms: b.total_before.clone()
                        },
                        scaled_receipt: None,
                        evidence_version: 1,
                        evidence: json!({"strategy":b.strategy,"allocations":b.allocations_before,"status":b.status_before,"physicalIdentity":account.physical_identity}),
                        observed_at: now()
                    }
                ]);
                binding["earn"] = serde_json::to_value(b)
                    .map_err(|_| GoFinError::validation("Earn binding serialization failed."))?;
            }
            E::KrakenFundingPutFundingV1DepositAddress | E::KrakenLegacyCreateDepositAddress => {
                self.permissions(
                    account,
                    if e == E::KrakenLegacyCreateDepositAddress {
                        &["query-funds"][..]
                    } else {
                        &["query-funds", "add-funds"][..]
                    },
                )
                .await?;
                let b = self.prepare_claim(account, e, &parameters).await?;
                if input
                    .get("maxFee")
                    .and_then(Value::as_str)
                    .is_some_and(|v| {
                        Quantity::from_decimal(v, b.fee_asset.decimals)
                            .map_or(true, |cap| b.fee > cap)
                    })
                {
                    return Err(err(
                        "KRAKEN_FEE_LIMIT",
                        "Address setup fee exceeds the approved maxFee.",
                    ));
                }
                let before = binding_hash(&json!(b.addresses_before))?;
                intent.expected.push(EffectConstraint::AuthorityTransition {
                    resource: account.resource.clone(),
                    authority: format!("kraken:deposit-address:{}", b.method_id),
                    before,
                    allowed_after: vec!["claimed".into()],
                });
                if !b.fee.is_zero() {
                    intent.maximum_costs.push(CostBound {
                        location: AccountingLocation::Cash {
                            resource: account.resource.clone(),
                        },
                        amount: Amount {
                            asset: b.fee_asset.clone(),
                            atoms: b.fee.clone(),
                        },
                        treatment: CostTreatment::AdditionalDebit,
                    });
                }
                summary = format!(
                    "Claim Kraken deposit address for {} ({}) on {}; maximum setup fee {} {}",
                    b.asset.reference,
                    b.method_id,
                    account.name,
                    b.fee.to_decimal(b.fee_asset.decimals),
                    b.fee_asset.reference
                );
                binding["claim"] = serde_json::to_value(b)
                    .map_err(|_| GoFinError::validation("Claim binding serialization failed."))?;
            }
            E::KrakenPrivatePostWithdrawCancel => {
                self.permissions(account, &["query-funds", "withdraw-funds", "query-ledger"])
                    .await?;
                let r = self
                    .json(
                        E::KrakenPrivatePostWithdrawStatus,
                        Some(account),
                        json!({"asset":text(&parameters,"asset")?}),
                    )
                    .await?;
                let row = unique(
                    result(&r)?.as_array().ok_or_else(|| {
                        err(
                            "KRAKEN_EVIDENCE_INVALID",
                            "Withdrawal status must be a list.",
                        )
                    })?,
                    |v| v["refid"] == parameters["refid"],
                )?;
                if !matches!(
                    row["status"].as_str(),
                    Some("Initial" | "Pending" | "Settled")
                ) {
                    return Err(err(
                        "KRAKEN_WITHDRAWAL_NOT_CANCELLABLE",
                        "This withdrawal is no longer cancellable.",
                    ));
                }
                let authority = format!("kraken:withdrawal:{}", text(&parameters, "refid")?);
                intent.expected.push(EffectConstraint::AuthorityTransition {
                    resource: account.resource.clone(),
                    authority,
                    before: text(&row, "status")?.into(),
                    allowed_after: vec!["canceled".into()],
                });
                binding["cancelBefore"] = row;
                summary = format!(
                    "Request cancellation of Kraken withdrawal {}",
                    text(&parameters, "refid")?
                );
                warnings.push("Cancellation may race settlement; it never proves that an already sent withdrawal was stopped.".into());
            }
            _ => {
                let t = self
                    .prepare_transfer(account, e, &parameters, input)
                    .await?;
                intent.locks.extend([t.from.clone(), t.to.clone()]);
                intent.debits.push(ResourceAmount {
                    resource: t.from.clone(),
                    amount: Amount {
                        asset: t.asset.clone(),
                        atoms: t.amount.clone(),
                    },
                });
                intent.expected.push(EffectConstraint::Transfer {
                    from: t.from.clone(),
                    to: t.to.clone(),
                    debit_asset: t.asset.clone(),
                    credit_asset: t.credit_asset.clone(),
                    maximum_debit: t.amount.clone(),
                    minimum_credit: t.amount.clone(),
                });
                summary = format!(
                    "Transfer {} {} from {} to {}",
                    t.amount.to_decimal(t.asset.decimals),
                    t.asset.reference,
                    t.from.0,
                    t.to.0
                );
                binding["transfer"] = serde_json::to_value(t).map_err(|_| {
                    GoFinError::validation("Transfer binding serialization failed.")
                })?;
            }
        }
        binding["parameters"] = parameters;
        intent.locks.sort();
        intent.locks.dedup();
        let mut valuation_debits = intent.debits.clone();
        if let Some(earn) = binding.get("earn") {
            let b: EarnBinding = decode(earn)?;
            if !b.allocate {
                valuation_debits.push(ResourceAmount {
                    resource: b.source,
                    amount: Amount {
                        asset: b.asset,
                        atoms: b.amount,
                    },
                });
            }
        }
        let valuation = if valuation_debits.is_empty() && intent.maximum_costs.is_empty() {
            None
        } else {
            Some(
                self.financial_valuation(&valuation_debits, &intent.maximum_costs)
                    .await?,
            )
        };
        if let Some(earn) = binding.get("earn") {
            let b: EarnBinding = decode(earn)?;
            if let Some(minimum) = b.minimum_usd
                && valuation
                    .as_ref()
                    .is_none_or(|v| v.movement_usd_micros < minimum)
            {
                return Err(err(
                    "KRAKEN_EARN_MINIMUM",
                    "Exact USD principal valuation is below this strategy's minimum allocation/deallocation.",
                ));
            }
        }
        let mut prepared=PreparedOperation{operation:e.operation().into(),input:input.clone(),binding,summary,checks:vec![Check{name:"exact_funding_scope".into(),status:CheckStatus::Passed,detail:"Account, product, route, exact quantities, permissions and settlement requirement are frozen.".into()}],warnings,observed_at:now(),expires_at:(Utc::now()+Duration::seconds(90)).to_rfc3339(),valuation,intent,action_deadline: None, operation_version:1};
        gofin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }
    pub(crate) fn preserve_withdrawal_quote_observation(
        current: &mut PreparedOperation,
        approved: &PreparedOperation,
    ) {
        let Some(old) = approved.binding.get("funding") else {
            return;
        };
        let Some(fresh) = current.binding.get("funding") else {
            return;
        };
        if old["api_version"] != "funding-v1"
            || old.get("quoted_fee").is_none()
            || fresh.get("quoted_fee").is_none()
            || current.binding["parameters"] != approved.binding["parameters"]
        {
            return;
        }
        let mut old_scope = old.clone();
        let mut fresh_scope = fresh.clone();
        old_scope
            .as_object_mut()
            .expect("typed binding")
            .remove("quoted_fee");
        fresh_scope
            .as_object_mut()
            .expect("typed binding")
            .remove("quoted_fee");
        if old_scope == fresh_scope {
            current.binding["funding"]["quoted_fee"] = old["quoted_fee"].clone();
        }
    }
    pub(crate) async fn validate_withdrawal_feasibility(&self, e: E, input: &Value) -> Result<()> {
        let (selection, mut parameters) = contracts::decode(e, input)?;
        let enrolled = self.account(
            selection
                .account
                .as_deref()
                .ok_or_else(|| GoFinError::validation("Select a Kraken account."))?,
        )?;
        let account = if let Some(id) = parameters.get("account_id").and_then(Value::as_str) {
            self.funding_wallet(enrolled, id).await?
        } else {
            enrolled.clone()
        };
        self.permissions(&account, &["query-funds", "withdraw-funds", "query-ledger"])
            .await?;
        self.prepare_withdrawal_with_account_limits(&account, e, &mut parameters, input, false)
            .await?;
        Ok(())
    }
    async fn prepare_withdrawal(
        &self,
        a: &KrakenAccount,
        e: E,
        p: &mut Value,
        input: &Value,
    ) -> Result<WithdrawalBinding> {
        self.prepare_withdrawal_with_account_limits(a, e, p, input, true)
            .await
    }
    async fn prepare_withdrawal_with_account_limits(
        &self,
        a: &KrakenAccount,
        e: E,
        p: &mut Value,
        input: &Value,
        check_account_limits: bool,
    ) -> Result<WithdrawalBinding> {
        let verification = match input.get("verificationRequirement").and_then(Value::as_str) {
            None | Some("destination_receipt") => VerificationRequirement::DestinationReceipt,
            Some("venue_only") => VerificationRequirement::VenueOnly,
            _ => {
                return Err(GoFinError::validation(
                    "Select destination_receipt or venue_only verification.",
                ));
            }
        };
        let class = if e == E::KrakenPrivatePostWithdraw {
            p.get("aclass")
                .and_then(Value::as_str)
                .unwrap_or("currency")
        } else {
            text(&p["amount"]["asset_amount"]["asset"], "class")?
        }
        .to_owned();
        let unit = if e == E::KrakenPrivatePostWithdraw {
            p.get("rebase_multiplier")
                .and_then(Value::as_str)
                .unwrap_or("rebased")
        } else {
            p["amount"]
                .get("rebase_multiplier")
                .and_then(Value::as_str)
                .unwrap_or("rebased")
        }
        .to_owned();
        if !matches!(class.as_str(), "currency" | "tokenized_asset")
            || !matches!(unit.as_str(), "base" | "rebased")
        {
            return Err(GoFinError::validation(
                "Funding asset class or denomination is unsupported.",
            ));
        }
        let (code, method_id, network, address_id, address, mut gross, net, mut fee): (
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            String,
        );
        let (tag, memo, contract): (Option<String>, Option<String>, Option<String>);
        let address_kind: String;
        let destination_details: Value;
        let fee_asset: AssetId;
        let fee_code: String;
        let fee_class: String;
        let mut quoted_fee = None;
        if e == E::KrakenPrivatePostWithdraw {
            code = text(p, "asset")?.to_owned();
            address_id = text(p, "key")?.to_owned();
            if class == "tokenized_asset" && unit == "base" {
                return Err(err(
                    "KRAKEN_LEGACY_BASE_QUOTE_UNAVAILABLE",
                    "Legacy WithdrawInfo cannot request base denomination; use current Funding withdrawal for a base tokenized asset.",
                ));
            }
            let i = self
                .json(
                    E::KrakenPrivatePostWithdrawInfo,
                    Some(a),
                    json!({"asset":code,"key":address_id,"amount":text(p,"amount")?}),
                )
                .await?;
            let info = result(&i)?;
            method_id = text(info, "method")?.into();
            let addresses = self
                .json(
                    E::KrakenPrivatePostWithdrawAddresses,
                    Some(a),
                    json!({"asset":code,"key":address_id,"verified":true}),
                )
                .await?;
            let row = unique(
                result(&addresses)?.as_array().ok_or_else(|| {
                    err(
                        "KRAKEN_EVIDENCE_INVALID",
                        "Withdrawal address evidence must be a list.",
                    )
                })?,
                |r| r["key"] == address_id && r["method"] == method_id && r["verified"] == true,
            )?;
            address = text(&row, "address")?.into();
            address_kind = "crypto".into();
            tag = row.get("tag").and_then(Value::as_str).map(String::from);
            memo = row.get("memo").and_then(Value::as_str).map(String::from);
            destination_details = json!({"crypto":{"address":address,"tag":tag,"memo":memo}});
            if p.get("address")
                .and_then(Value::as_str)
                .is_some_and(|v| v != address)
            {
                return Err(err(
                    "KRAKEN_DESTINATION_MISMATCH",
                    "The explicit address differs from the authenticated saved withdrawal key.",
                ));
            }
            let methods = self
                .json(
                    E::KrakenPrivatePostWithdrawMethods,
                    Some(a),
                    json!({"asset":code}),
                )
                .await?;
            let route = unique(
                result(&methods)?.as_array().ok_or_else(|| {
                    err(
                        "KRAKEN_EVIDENCE_INVALID",
                        "Withdrawal methods must be a list.",
                    )
                })?,
                |v| v["method"] == method_id,
            )?;
            network = route
                .get("network")
                .and_then(Value::as_str)
                .unwrap_or(&method_id)
                .into();
            contract = route
                .get("contract_address")
                .and_then(Value::as_str)
                .map(String::from);
            gross = text(p, "amount")?.into();
            net = text(info, "amount")?.into();
            fee = text(info, "fee")?.into();
            let asset = self
                .funding_asset_for_amount(a, &code, &class, &unit, text(p, "amount")?)
                .await?;
            fee_asset = asset.clone();
            fee_code = code.clone();
            fee_class = class.clone();
            if let Some(minimum) = route.get("minimum").and_then(Value::as_str)
                && amount(&asset, &net)?.atoms < amount(&asset, minimum)?.atoms
            {
                return Err(err(
                    "KRAKEN_FUNDING_MINIMUM",
                    "Net withdrawal is below this method's minimum.",
                ));
            }
            if check_account_limits
                && amount(&asset, &net)?.atoms > qty(info, "limit", asset.decimals)?
            {
                return Err(err(
                    "KRAKEN_WITHDRAWAL_LIMIT",
                    "The withdrawal exceeds Kraken's current net limit.",
                ));
            }
            if let Some(cap) = p.get("max_fee").and_then(Value::as_str)
                && amount(&asset, &fee)?.atoms > amount(&asset, cap)?.atoms
            {
                return Err(err(
                    "KRAKEN_FEE_LIMIT",
                    "Observed withdrawal fee exceeds max_fee.",
                ));
            }
        } else {
            code = text(&p["amount"]["asset_amount"]["asset"], "name")?.into();
            let mut method_params =
                json!({"direction":"withdraw","asset":{"class":class,"name":code},"limit":500});
            if class == "tokenized_asset" {
                method_params["rebase_multiplier"] = json!(unit);
            }
            let methods = self
                .funding_pages(
                    a,
                    E::KrakenFundingGetFundingV1MethodsDirection,
                    method_params,
                    "methods",
                )
                .await?;
            let scope = &p["scope"];
            let route = unique(&methods, |m| {
                m["asset"]["name"] == code
                    && m["asset"]["class"] == class
                    && (scope
                        .get("method_id")
                        .is_some_and(|id| m["method_id"] == *id)
                        || scope
                            .get("network_id")
                            .is_some_and(|id| m["network"]["network_id"] == *id))
            })?;
            method_id = text(&route, "method_id")?.into();
            network = route["network"]
                .get("network_name")
                .and_then(Value::as_str)
                .unwrap_or(text(&route, "method_name")?)
                .into();
            contract = route["network"]
                .get("contract_address")
                .and_then(Value::as_str)
                .map(String::from);
            address_id = text(p, "address_id")?.into();
            let rows = self
                .funding_pages(
                    a,
                    E::KrakenFundingGetFundingV1Addresses,
                    json!({"scope":{"method_id":method_id},"limit":500}),
                    "addresses",
                )
                .await?;
            let addr = unique(&rows, |r| {
                r["address_id"] == address_id && r["verified"] == true
            })?;
            // A broad address scope is safe only after the method's exact network has been resolved.
            let mut scope_ok = addr["scope"]["method_id"].as_str() == Some(&method_id)
                || addr["scope"]["network_id"]
                    .as_str()
                    .zip(route["network"]["network_id"].as_str())
                    .is_some_and(|(left, right)| left == right);
            if let Some(group) = addr["scope"]
                .get("network_group_id")
                .and_then(Value::as_str)
            {
                let response = self
                    .json(E::KrakenFundingGetFundingV1Networks, Some(a), json!({}))
                    .await?;
                let groups = result(&response)?
                    .get("network_groups")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        err(
                            "KRAKEN_NETWORK_GROUP_INVALID",
                            "Current network group membership evidence is missing.",
                        )
                    })?;
                let g = unique(groups, |v| v["network_group_id"] == group)?;
                scope_ok = g
                    .get("network_ids")
                    .and_then(Value::as_array)
                    .is_some_and(|ids| ids.contains(&route["network"]["network_id"]));
            }
            if !scope_ok {
                return Err(err(
                    "KRAKEN_ADDRESS_SCOPE_MISMATCH",
                    "Saved address scope does not match this exact withdrawal method/network.",
                ));
            }
            destination_details = material_destination_details(&addr["address_details"]);
            if let Some(crypto) = addr["address_details"].get("crypto") {
                address = text(crypto, "address")?.into();
                address_kind = "crypto".into();
                tag = crypto.get("tag").and_then(Value::as_str).map(String::from);
                memo = crypto.get("memo").and_then(Value::as_str).map(String::from);
            } else if let Some(fiat) = addr["address_details"].get("fiat") {
                if verification != VerificationRequirement::VenueOnly {
                    return Err(err(
                        "KRAKEN_FIAT_RECEIPT_OBSERVER_UNSUPPORTED",
                        "Bank/provider withdrawals require explicit venue_only verification consent.",
                    ));
                }
                address = ["iban", "account", "address"]
                    .iter()
                    .find_map(|key| {
                        fiat.get(key)
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                    })
                    .ok_or_else(|| {
                        err(
                            "KRAKEN_FIAT_DESTINATION_UNAVAILABLE",
                            "Saved fiat destination lacks material bank account/address evidence.",
                        )
                    })?
                    .into();
                address_kind = "fiat".into();
                tag = fiat.get("tag").and_then(Value::as_str).map(String::from);
                memo = fiat.get("memo").and_then(Value::as_str).map(String::from);
            } else {
                return Err(err(
                    "KRAKEN_DESTINATION_UNKNOWN",
                    "Saved destination has no supported typed crypto or fiat identity.",
                ));
            }
            if p.get("expected_address")
                .and_then(Value::as_str)
                .is_some_and(|s| s != address)
            {
                return Err(err(
                    "KRAKEN_DESTINATION_MISMATCH",
                    "Expected address differs from the authenticated saved destination.",
                ));
            }
            let value = text(&p["amount"]["asset_amount"], "amount")?;
            let included = p["fee"]
                .get("fee_included")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mut quote_input =
                json!({"method_id":method_id,"amount":value,"fee_included":included});
            if class == "tokenized_asset" {
                quote_input["rebase_multiplier"] = json!(unit);
            }
            if p["fee"].get("quoted_fee").is_some()
                && class == "tokenized_asset"
                && p["fee"]["quoted_fee"]
                    .get("rebase_multiplier")
                    .and_then(Value::as_str)
                    .unwrap_or("rebased")
                    != unit
            {
                return Err(err(
                    "KRAKEN_DENOMINATION_MISMATCH",
                    "Fee quote and principal must use the same exact denomination.",
                ));
            }
            if let Some(token) = p["fee"]["quoted_fee"].get("token").and_then(Value::as_str) {
                quote_input["withdrawal_fee_token"] = json!(token);
            }
            let q = self
                .json(
                    E::KrakenFundingGetFundingV1FeesMethodId,
                    Some(a),
                    quote_input,
                )
                .await?;
            let quote = result(&q)?;
            if [
                &quote["gross_amount"]["asset"],
                &quote["net_amount"]["asset"],
            ]
            .iter()
            .any(|asset| asset["name"] != code || asset["class"] != class)
            {
                return Err(err(
                    "KRAKEN_FEE_ASSET_UNSUPPORTED",
                    "Gross and net funding amounts must retain the exact selected native asset class.",
                ));
            }
            gross = text(&quote["gross_amount"], "amount")?.into();
            net = text(&quote["net_amount"], "amount")?.into();
            fee = text(&quote["fee"], "amount")?.into();
            let asset = self
                .funding_asset_for_amount(a, &code, &class, &unit, value)
                .await?;
            fee_code = text(&quote["fee"]["asset"], "name")?.into();
            fee_class = text(&quote["fee"]["asset"], "class")?.into();
            fee_asset = if fee_code == code && fee_class == class {
                asset.clone()
            } else {
                self.funding_asset(a, &fee_code, &fee_class, &unit).await?
            };

            let net_q = amount(&asset, &net)?.atoms;
            if let Some(min) = route.get("minimum_amount").and_then(Value::as_str)
                && net_q < amount(&asset, min)?.atoms
            {
                return Err(err(
                    "KRAKEN_FUNDING_MINIMUM",
                    "Net withdrawal is below this method's minimum.",
                ));
            }
            if let Some(max) = route.get("maximum_amount").and_then(Value::as_str)
                && net_q > amount(&asset, max)?.atoms
            {
                return Err(err(
                    "KRAKEN_FUNDING_LIMIT",
                    "Net withdrawal exceeds this method's maximum.",
                ));
            }
            let caller_cap = input
                .get("maxFee")
                .and_then(Value::as_str)
                .map(|v| amount(&fee_asset, v))
                .transpose()?;
            if class == "tokenized_asset"
                && p["fee"]["current_fee"].get("max_fee").is_some()
                && p["fee"]["current_fee"]["max_fee"]
                    .get("rebase_multiplier")
                    .and_then(Value::as_str)
                    .unwrap_or("rebased")
                    != unit
            {
                return Err(err(
                    "KRAKEN_DENOMINATION_MISMATCH",
                    "Native current fee cap and principal must use the same exact denomination.",
                ));
            }
            let wire_cap = p["fee"]["current_fee"]["max_fee"].get("asset_amount");
            if let Some(cap) = wire_cap
                && (cap["asset"]["name"] != fee_code
                    || cap["asset"]["class"] != fee_class
                    || amount(&fee_asset, &fee)?.atoms > qty(cap, "amount", 36)?)
            {
                return Err(err(
                    "KRAKEN_FEE_LIMIT",
                    "Current fee exceeds the native current_fee cap or uses another asset.",
                ));
            }
            if caller_cap
                .as_ref()
                .is_some_and(|cap| amount(&fee_asset, &fee).is_ok_and(|fee| fee.atoms > cap.atoms))
            {
                return Err(err(
                    "KRAKEN_FEE_LIMIT",
                    "Current funding fee exceeds maxFee.",
                ));
            }
            // Keep quoted economics distinct from the finite submitted ceiling.
            // Fixed-gross and quote-token requests retain their exact net/fee
            // semantics; only fixed-net current-fee requests can retain headroom.
            if p["fee"].get("quoted_fee").is_none() {
                if !included {
                    let observed = amount(&fee_asset, &fee)?.atoms;
                    let expected_quote_gross = if fee_asset == asset {
                        amount(&asset, &net)?.atoms.checked_add(&observed)?
                    } else {
                        amount(&asset, &net)?.atoms
                    };
                    if amount(&asset, &gross)?.atoms != expected_quote_gross {
                        return Err(err(
                            "KRAKEN_FEE_ACCOUNTING_INVALID",
                            "Quoted gross does not equal exact net plus the quoted same-asset fee.",
                        ));
                    }
                    let native_cap = wire_cap
                        .map(|cap| qty(cap, "amount", fee_asset.decimals))
                        .transpose()?;
                    let maximum = match (native_cap, caller_cap) {
                        (Some(a), Some(b)) => std::cmp::min(a, b.atoms),
                        (Some(a), None) => a,
                        (None, Some(b)) => b.atoms,
                        (None, None) => observed.clone(),
                    };
                    quoted_fee = Some(observed);
                    fee = maximum.to_decimal(fee_asset.decimals);
                    gross = if fee_asset == asset {
                        amount(&asset, &net)?
                            .atoms
                            .checked_add(&maximum)?
                            .to_decimal(asset.decimals)
                    } else {
                        net.clone()
                    };
                }
                p["fee"] = json!({"fee_included":included,"current_fee":{"max_fee":{"asset_amount":{"asset":{"class":fee_class,"name":fee_code},"amount":fee}}}});
            }
            if check_account_limits {
                let mut limit_input = json!({"asset_class":class,"asset":code});
                if class == "tokenized_asset" {
                    limit_input["preferred_asset"] = json!({"class":"currency","name":"USD"});
                }
                let limits = self
                    .json(
                        E::KrakenFundingGetFundingV1LimitsWithdrawalAssetClassAsset,
                        Some(a),
                        limit_input,
                    )
                    .await?;
                let limit = unique(
                    result(&limits)?
                        .get("withdrawal_limits")
                        .and_then(Value::as_array)
                        .ok_or_else(|| {
                            err("KRAKEN_EVIDENCE_INVALID", "Funding limits are missing.")
                        })?,
                    |r| r["method_id"] == method_id,
                )?;
                let maximum = &limit["maximum_amount"];
                if class == "tokenized_asset" {
                    if maximum["asset"]["class"] != "currency"
                        || !matches!(maximum["asset"]["name"].as_str(), Some("USD" | "ZUSD"))
                    {
                        return Err(err(
                            "KRAKEN_LIMIT_DENOMINATION_UNVERIFIED",
                            "Tokenized account limits must be returned in the explicitly requested USD unit.",
                        ));
                    }
                    let value = self
                        .financial_valuation(
                            &[ResourceAmount {
                                resource: a.resource.clone(),
                                amount: amount(&asset, &gross)?,
                            }],
                            &[],
                        )
                        .await?;
                    let cap_usd = qty(maximum, "amount", 36)?.mul_div_floor(
                        &Quantity::from_u64(1_000_000),
                        &Quantity::new(format!("1{}", "0".repeat(36)))?,
                    )?;
                    if value.movement_usd_micros > cap_usd {
                        return Err(err(
                            "KRAKEN_FUNDING_LIMIT",
                            "Tokenized gross withdrawal exceeds current USD account limits in its exact priced denomination.",
                        ));
                    }
                } else if maximum["asset"]["name"] != code
                    || maximum["asset"]["class"] != class
                    || amount(&asset, &gross)?.atoms > qty(maximum, "amount", 36)?
                {
                    return Err(err(
                        "KRAKEN_FUNDING_LIMIT",
                        "Gross withdrawal exceeds current method/account limits.",
                    ));
                }
            }
        }
        if class == "tokenized_asset" {
            if e == E::KrakenPrivatePostWithdraw {
                p["rebase_multiplier"] = json!(unit);
            } else {
                p["amount"]["rebase_multiplier"] = json!(unit);
                if p["fee"].get("current_fee").is_some() {
                    p["fee"]["current_fee"]["max_fee"]["rebase_multiplier"] = json!(unit);
                } else {
                    p["fee"]["quoted_fee"]["rebase_multiplier"] = json!(unit);
                }
            }
        }
        let asset = self.funding_asset(a, &code, &class, &unit).await?;
        let gross = positive(&asset, &gross)?.atoms;
        let net = positive(&asset, &net)?.atoms;
        let fee = amount(&fee_asset, &fee)?.atoms;
        let gross_expected = if fee_asset == asset {
            net.checked_add(&fee)?
        } else {
            net.clone()
        };
        if gross != gross_expected {
            return Err(err(
                "KRAKEN_FEE_ACCOUNTING_INVALID",
                "Exact gross amount does not equal net amount plus the same-asset fee.",
            ));
        }
        let requested = native_quantity(
            if e == E::KrakenPrivatePostWithdraw {
                text(p, "amount")?
            } else {
                text(&p["amount"]["asset_amount"], "amount")?
            },
            36,
        )?;
        if (e == E::KrakenPrivatePostWithdraw || p["fee"]["fee_included"] == true)
            && requested != gross
            || (e != E::KrakenPrivatePostWithdraw
                && p["fee"]["fee_included"] != true
                && requested != net)
        {
            return Err(err(
                "KRAKEN_FUNDING_QUOTE_MISMATCH",
                "Exact quote principal does not match the request's included/additional fee semantics.",
            ));
        }
        if e == E::KrakenPrivatePostWithdraw {
            p["address"] = json!(address);
            p["max_fee"] = json!(fee.to_decimal(asset.decimals));
        } else if address_kind == "crypto" {
            p["expected_address"] = json!(address);
        }
        let destination = if verification == VerificationRequirement::DestinationReceipt {
            if class == "tokenized_asset" && unit != "base" {
                return Err(err(
                    "KRAKEN_DENOMINATION_OBSERVER_UNSUPPORTED",
                    "Onchain token receipts prove SPV base tokens, not rebased equity units. Use base denomination or explicitly approve venue_only.",
                ));
            }
            if tag.as_deref().is_some_and(|s| !s.is_empty())
                || memo.as_deref().is_some_and(|s| !s.is_empty())
            {
                return Err(err(
                    "KRAKEN_DESTINATION_OBSERVER_UNSUPPORTED",
                    "This memo/tag route has no independent reviewed receipt observer; explicit venue_only consent is required.",
                ));
            }
            let v=self.recipient_verifier.as_ref().ok_or_else(||err("KRAKEN_DESTINATION_OBSERVER_UNAVAILABLE","Destination verification requires a configured reviewed chain observer; otherwise explicitly approve venue_only."))?;
            let d=v.resolve(&network,&code,contract.as_deref(),&address,input.get("destination").and_then(Value::as_str)).await?.ok_or_else(||err("KRAKEN_DESTINATION_OBSERVER_UNSUPPORTED","This exact network/asset/destination is not independently observable; explicit venue_only consent is required."))?;
            Quantity::from_decimal(&net.to_decimal(asset.decimals), d.asset.decimals)?;
            Some(d)
        } else {
            None
        };
        Ok(WithdrawalBinding {
            api_version: if e == E::KrakenPrivatePostWithdraw {
                "legacy-v0"
            } else {
                "funding-v1"
            }
            .into(),
            source: a.resource.clone(),
            asset,
            venue_asset_code: code,
            asset_class: class,
            denomination: unit,
            method_id,
            network,
            address_id,
            address,
            address_kind,
            destination_details,
            tag,
            memo,
            gross,
            net,
            fee,
            quoted_fee,
            fee_asset,
            fee_venue_code: fee_code,
            fee_asset_class: fee_class,
            verification,
            destination,
        })
    }
    /// A strategy may be on a later native cursor page. Complete the bounded
    /// traversal before adopting it, so a duplicate/conflicting ID cannot hide
    /// after the first match.
    async fn earn_strategy(&self, a: &KrakenAccount, strategy: &str) -> Result<Value> {
        let mut parameters = json!({"limit":500,"ascending":true});
        let mut cursors = std::collections::BTreeSet::new();
        let mut identities = std::collections::BTreeSet::new();
        let mut selected = None;
        for _ in 0..20 {
            let response = self
                .json(
                    E::KrakenPrivatePostEarnStrategies,
                    Some(a),
                    parameters.clone(),
                )
                .await?;
            let data = result(&response)?;
            let rows = data["items"]
                .as_array()
                .filter(|rows| rows.len() <= 500)
                .ok_or_else(|| {
                    err(
                        "KRAKEN_EARN_COVERAGE_INCOMPLETE",
                        "Earn strategy page is malformed or exceeds its requested bound.",
                    )
                })?;
            for row in rows {
                let id = text(row, "id")?;
                if !identities.insert(id.to_owned()) {
                    return Err(err(
                        "KRAKEN_EARN_AMBIGUOUS",
                        "Earn strategy identity appears more than once across cursor pages; no duplicate or conflicting strategy is adopted.",
                    ));
                }
                if id == strategy {
                    selected = Some(row.clone());
                }
            }
            match data.get("next_cursor") {
                None|Some(Value::Null)=>return selected.ok_or_else(||err("KRAKEN_EARN_STRATEGY_UNAVAILABLE","The exact selected strategy is absent from the complete bounded native strategy inventory.")),
                Some(Value::String(cursor)) if cursor.is_empty()=>return selected.ok_or_else(||err("KRAKEN_EARN_STRATEGY_UNAVAILABLE","The exact selected strategy is absent from the complete bounded native strategy inventory.")),
                Some(Value::String(cursor)) if cursors.insert(cursor.clone())=>parameters["cursor"]=json!(cursor),
                _=>return Err(err("KRAKEN_EARN_COVERAGE_INCOMPLETE","Earn strategy pagination has a malformed or repeated cursor; no exhaustive identity is available.")),
            }
        }
        Err(err(
            "KRAKEN_EARN_COVERAGE_INCOMPLETE",
            "Earn strategies exceed twenty bounded cursor pages; no partial strategy identity is adopted.",
        ))
    }
    async fn prepare_earn(
        &self,
        a: &KrakenAccount,
        e: E,
        p: &Value,
        input: &Value,
    ) -> Result<EarnBinding> {
        let strategy = text(p, "strategy_id")?;
        let s = self.earn_strategy(a, strategy).await?;
        let allocate = e == E::KrakenPrivatePostEarnAllocate;
        if s[if allocate {
            "can_allocate"
        } else {
            "can_deallocate"
        }] != true
        {
            return Err(err(
                "KRAKEN_EARN_RESTRICTED",
                "The selected strategy cannot perform this action for this account.",
            ));
        }
        let asset = self
            .funding_asset_for_amount(
                a,
                text(&s, "asset")?,
                "currency",
                "rebased",
                text(p, "amount")?,
            )
            .await?;
        let amount = positive(&asset, text(p, "amount")?)?.atoms;
        let raw_fee = s
            .get(if allocate {
                "allocation_fee"
            } else {
                "deallocation_fee"
            })
            .ok_or_else(|| {
                err(
                    "KRAKEN_EARN_FEE_INVALID",
                    "Earn strategy fee metadata is missing.",
                )
            })?;
        let fee = crate::observation::exact(raw_fee)?;
        let max_fee = match input.get("maxFee").and_then(Value::as_str) {
            Some(cap) => Quantity::from_decimal(cap, 36)?,
            None if native_quantity(&fee, 36)?.is_zero() => Quantity::zero(),
            None => {
                return Err(err(
                    "KRAKEN_EARN_FEE_CAP_REQUIRED",
                    "This strategy charges a nonzero fee. Supply maxFee in exact native asset units; metadata alone does not establish a percentage denominator.",
                ));
            }
        };
        if max_fee >= amount {
            return Err(GoFinError::validation(
                "Earn maxFee must be smaller than the exact principal.",
            ));
        }
        let status = self
            .json(
                if allocate {
                    E::KrakenPrivatePostEarnAllocateStatus
                } else {
                    E::KrakenPrivatePostEarnDeallocateStatus
                },
                Some(a),
                json!({"strategy_id":strategy}),
            )
            .await?;
        if result(&status)?["pending"] != false {
            return Err(err(
                "KRAKEN_EARN_BUSY",
                "This strategy has an unresolved prior venue action.",
            ));
        }
        let allocations = self
            .json(E::KrakenPrivatePostEarnAllocations, Some(a), json!({}))
            .await?;
        if result(&allocations)?
            .get("next_cursor")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
        {
            return Err(err(
                "KRAKEN_EARN_COVERAGE_INCOMPLETE",
                "Earn allocations span additional pages; no exhaustive baseline is available.",
            ));
        }
        let items = result(&allocations)?
            .get("items")
            .and_then(Value::as_array)
            .ok_or_else(|| err("KRAKEN_EVIDENCE_INVALID", "Earn allocations are missing."))?;
        let matching: Vec<_> = items
            .iter()
            .filter(|v| v["strategy_id"] == strategy)
            .collect();
        if matching.len() > 1 {
            return Err(err(
                "KRAKEN_EARN_AMBIGUOUS",
                "Earn strategy allocation is not unique.",
            ));
        }
        let total_before = match matching.first() {
            Some(v) => qty(&v["amount_allocated"]["total"], "native", asset.decimals)?,
            None => Quantity::zero(),
        };
        if allocate && self.free(a, &asset).await? < amount.checked_add(&max_fee)? {
            return Err(err(
                "INSUFFICIENT_BALANCE",
                "Available Kraken cash cannot cover this Earn allocation.",
            ));
        }
        if !allocate && total_before < amount.checked_add(&max_fee)? {
            return Err(err(
                "INSUFFICIENT_POSITION",
                "Earn strategy allocation is smaller than this deallocation.",
            ));
        }
        if allocate
            && let Some(cap) = s.get("user_cap").and_then(Value::as_str)
            && total_before.checked_add(&amount)? > native_quantity(cap, asset.decimals)?
        {
            return Err(err(
                "KRAKEN_EARN_LIMIT",
                "Earn allocation exceeds this strategy's user cap.",
            ));
        }
        let l = self.ledger(a, "all").await?;
        Ok(EarnBinding {
            accounting_version: 1,
            source: a.resource.clone(),
            strategy: strategy.into(),
            asset,
            amount,
            allocate,
            total_before,
            max_fee,
            minimum_usd: s
                .get("user_min_allocation")
                .and_then(Value::as_str)
                .map(|usd| {
                    native_quantity(usd, 36)?.mul_div_ceil(
                        &Quantity::from_u64(1_000_000),
                        &Quantity::new(format!("1{}", "0".repeat(36)))?,
                    )
                })
                .transpose()?,
            ledger_before: l
                .as_object()
                .expect("validated ledger")
                .keys()
                .cloned()
                .collect(),
            lock_type: s["lock_type"].clone(),
            status_before: status,
            allocations_before: allocations,
        })
    }
    async fn prepare_claim(&self, a: &KrakenAccount, e: E, p: &Value) -> Result<ClaimBinding> {
        if e == E::KrakenLegacyCreateDepositAddress {
            let code = text(p, "asset")?;
            let method = text(p, "method")?;
            let response = self
                .json(
                    E::KrakenPrivatePostDepositMethods,
                    Some(a),
                    json!({"asset":code}),
                )
                .await?;
            let row = unique(
                result(&response)?.as_array().ok_or_else(|| {
                    err("KRAKEN_EVIDENCE_INVALID", "Deposit methods are missing.")
                })?,
                |v| v["method"] == method,
            )?;
            if row["gen-address"] != true {
                return Err(err(
                    "KRAKEN_ADDRESS_GENERATION_UNSUPPORTED",
                    "The selected legacy deposit method cannot generate a new address.",
                ));
            }
            let class = p
                .get("aclass")
                .and_then(Value::as_str)
                .unwrap_or("currency");
            let asset = self.funding_asset(a, code, class, "rebased").await?;
            // Legacy method metadata's fee is a funding fee, not a proven setup charge.
            if row["address-setup-fee"] == true {
                let current = self.methods(a, "deposit").await?;
                let mut matches = Vec::new();
                for candidate in current
                    .iter()
                    .filter(|v| v["method_name"] == method && v["asset"]["class"] == class)
                {
                    let identity = self
                        .funding_asset(a, text(&candidate["asset"], "name")?, class, "rebased")
                        .await?;
                    if identity == asset {
                        matches.push(candidate);
                    }
                }
                if matches.len() != 1 {
                    return Err(err(
                        "KRAKEN_ADDRESS_SETUP_FEE_UNVERIFIED",
                        "Legacy setup fee requires one exact corresponding current method identity. Use current claimed-address discovery to resolve this route.",
                    ));
                }
                let current_id = text(matches[0], "method_id")?;
                let mut claim = Box::pin(self.prepare_claim(
                    a,
                    E::KrakenFundingPutFundingV1DepositAddress,
                    &json!({"method_id":current_id}),
                ))
                .await?;
                claim.method_id = method.into();
                claim.current_method_id = Some(current_id.into());
                return Ok(claim);
            }
            Ok(ClaimBinding {
                method_id: method.into(),
                current_method_id: None,
                fee_asset: asset.clone(),
                asset,
                fee: Quantity::zero(),
                addresses_before: vec![],
                ledger_before: None,
            })
        } else {
            let method = text(p, "method_id")?;
            let methods = self.methods(a, "deposit").await?;
            let row = unique(&methods, |v| v["method_id"] == method)?;
            let asset = self
                .funding_asset(
                    a,
                    text(&row["asset"], "name")?,
                    text(&row["asset"], "class")?,
                    "rebased",
                )
                .await?;
            if !matches!(
                row["deposit"]["address_generation"]["status"].as_str(),
                Some("unlimited" | "limited")
            ) {
                return Err(err(
                    "KRAKEN_ADDRESS_GENERATION_UNSUPPORTED",
                    "The selected deposit method cannot claim an address.",
                ));
            }
            let (fee_asset, fee) = if let Some(f) = row["deposit"].get("address_setup_fee") {
                let fee_asset = self
                    .funding_asset(
                        a,
                        text(&f["asset"], "name")?,
                        text(&f["asset"], "class")?,
                        "rebased",
                    )
                    .await?;
                let fee = qty(f, "amount", fee_asset.decimals)?;
                (fee_asset, fee)
            } else {
                (asset.clone(), Quantity::zero())
            };
            // The venue claims shared address families through the actual requested method.
            let addresses = self
                .funding_pages(
                    a,
                    E::KrakenFundingGetFundingV2DepositAddresses,
                    json!({"scope":{"method_id":method},"limit":500}),
                    "addresses",
                )
                .await?;
            if row["deposit"]["address_generation"]["status"] == "limited" {
                let limit = row["deposit"]["address_generation"]["limit"]
                    .as_u64()
                    .filter(|v| *v > 0)
                    .ok_or_else(|| {
                        err(
                            "KRAKEN_ADDRESS_GENERATION_LIMIT_UNVERIFIED",
                            "Limited address generation has no positive exact limit.",
                        )
                    })?;
                for address in &addresses {
                    if let Some(last_used) = address.get("last_used").filter(|v| !v.is_null())
                        && last_used
                            .as_str()
                            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
                            .is_none()
                    {
                        return Err(err(
                            "KRAKEN_ADDRESS_GENERATION_LIMIT_UNVERIFIED",
                            "Claimed address usage timestamp is malformed; its unused-address count cannot be established.",
                        ));
                    }
                }
                let unused = addresses
                    .iter()
                    .filter(|address| address.get("last_used").is_none_or(Value::is_null))
                    .count() as u64;
                if unused >= limit {
                    return Err(err(
                        "KRAKEN_ADDRESS_GENERATION_LIMIT",
                        "Current unused claimed addresses have reached this method's generation limit; reuse an existing address.",
                    ));
                }
            }
            let before = addresses
                .iter()
                .filter_map(|row| row.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect();
            let ledger_before = if fee.is_zero() {
                None
            } else {
                self.permissions(a, &["query-ledger"]).await?;
                Some(
                    self.ledger_asset(a, &fee_asset)
                        .await?
                        .as_object()
                        .expect("validated ledger")
                        .keys()
                        .cloned()
                        .collect(),
                )
            };
            Ok(ClaimBinding {
                method_id: method.into(),
                current_method_id: Some(method.into()),
                asset,
                fee_asset,
                fee,
                addresses_before: before,
                ledger_before,
            })
        }
    }
    async fn transfer_identity(
        &self,
        a: &KrakenAccount,
        write: bool,
    ) -> Result<(Option<String>, Option<String>)> {
        fn normalize(value: &str) -> String {
            value.chars().filter(|c| !c.is_whitespace()).collect()
        }
        if a.product == crate::KrakenProduct::Spot {
            let data = self
                .spot_permissions(
                    a,
                    if write {
                        &["query-funds", "query-ledger", "withdraw-funds"]
                    } else {
                        &["query-funds", "query-ledger"]
                    },
                )
                .await?;
            let iib=data.get("iiban").and_then(Value::as_str).filter(|v|!v.is_empty()).map(normalize).ok_or_else(||err("KRAKEN_TRANSFER_IDENTITY_UNVERIFIED", "Spot key metadata did not supply an authoritative IIBAN for this transfer."))?;
            if iib != normalize(&a.physical_identity) {
                return Err(err(
                    "KRAKEN_TRANSFER_IDENTITY_MISMATCH",
                    "Authenticated Spot IIBAN differs from its protected enrolled identity.",
                ));
            }
            return Ok((None, Some(iib)));
        }
        let response = self
            .json(E::KrakenfuturesAuthGetApiKeysV3Check, Some(a), json!({}))
            .await?;
        let data = result(&response)?;
        if !matches!(
            data["permissions"]["general"].as_str(),
            Some("READ_ONLY" | "FULL_ACCESS")
        ) || (write && data["permissions"]["transfer"] != "FULL_ACCESS")
        {
            return Err(err(
                "KRAKEN_PERMISSION_DENIED",
                "Derivatives funding needs readable general access and source transfer FULL_ACCESS.",
            ));
        }
        let uid = text(data, "accountUid")?.to_owned();
        let iib = data["iiban"]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(normalize);
        if !uid.eq_ignore_ascii_case(&a.physical_identity)
            && iib.as_deref() != Some(normalize(&a.physical_identity).as_str())
        {
            return Err(err(
                "KRAKEN_TRANSFER_IDENTITY_MISMATCH",
                "Authenticated Derivatives accountUid/IIBAN differs from its protected enrolled identity.",
            ));
        }
        Ok((Some(uid), iib))
    }
    async fn prepare_transfer(
        &self,
        a: &KrakenAccount,
        e: E,
        p: &Value,
        input: &Value,
    ) -> Result<TransferBinding> {
        let code = p
            .get("asset")
            .or_else(|| p.get("unit"))
            .or_else(|| p.get("currency"))
            .and_then(Value::as_str)
            .ok_or_else(|| GoFinError::validation("Select an exact transfer asset."))?;
        // Spot Assets publishes currencies shared across the two Kraken products.
        let spot_code = if a.product == crate::KrakenProduct::Derivatives {
            match code {
                "xbt" | "XBT" | "BTC" => "XXBT",
                "eth" | "ETH" => "XETH",
                "USD" => "ZUSD",
                v => v,
            }
        } else {
            code
        };
        let exact_asset = self.spot_asset(a, spot_code).await?;
        let asset = AssetId {
            network: a.product.client().into(),
            reference: if a.product == crate::KrakenProduct::Derivatives {
                code.into()
            } else {
                exact_asset.reference.clone()
            },
            decimals: 36,
        };
        let value = positive(&asset, text(p, "amount")?)?.atoms;
        let dest_name = input
            .get("destinationAccount")
            .and_then(Value::as_str)
            .unwrap_or(&a.name);
        let dest = self
            .related_accounts
            .iter()
            .chain(self.accounts.iter())
            .find(|v| v.name == dest_name)
            .ok_or_else(|| {
                err(
                    "KRAKEN_TRANSFER_DESTINATION_UNKNOWN",
                    "The destination account must be enrolled in this Kraken product.",
                )
            })?;
        let (dispatch_uid, dispatch_iiban) = self.transfer_identity(a, true).await?;
        let source_name = input
            .get("sourceAccount")
            .and_then(Value::as_str)
            .unwrap_or(&a.name);
        let source = if source_name == a.name {
            a
        } else {
            if e != E::KrakenfuturesPrivatePostTransferSubaccount {
                return Err(GoFinError::validation(
                    "sourceAccount applies only to Derivatives subaccount transfers.",
                ));
            }
            self.related_accounts
                .iter()
                .chain(self.accounts.iter())
                .find(|candidate| {
                    candidate.name == source_name
                        && candidate.product == crate::KrakenProduct::Derivatives
                })
                .ok_or_else(|| {
                    err(
                        "KRAKEN_TRANSFER_SOURCE_UNKNOWN",
                        "Select a separately enrolled readable Derivatives source account.",
                    )
                })?
        };
        let (source_uid, source_iiban) = if source.name == a.name {
            (dispatch_uid.clone(), dispatch_iiban.clone())
        } else {
            self.transfer_identity(source, false).await?
        };
        let (destination_uid, destination_iiban) = if dest.name == a.name {
            (dispatch_uid.clone(), dispatch_iiban.clone())
        } else if dest.name == source.name {
            (source_uid.clone(), source_iiban.clone())
        } else {
            self.transfer_identity(dest, false).await?
        };
        if e == E::KrakenfuturesPrivatePostTransferSubaccount
            && (source.name != a.name || dest.name != a.name)
        {
            let response = self
                .json(E::KrakenfuturesPrivateGetSubaccounts, Some(a), json!({}))
                .await?;
            let data = result(&response)?;
            if data["masterAccountUid"].as_str() != dispatch_uid.as_deref() {
                return Err(err(
                    "KRAKEN_SUBACCOUNT_RELATIONSHIP_UNVERIFIED",
                    "The dispatch credential is not the authenticated master of the selected subaccounts.",
                ));
            }
            let children = data["subaccounts"]
                .as_array()
                .filter(|rows| rows.len() <= 500)
                .ok_or_else(|| {
                    err(
                        "KRAKEN_SUBACCOUNT_RELATIONSHIP_UNVERIFIED",
                        "Authenticated bounded subaccount membership is unavailable.",
                    )
                })?;
            for uid in [source_uid.as_deref(), destination_uid.as_deref()]
                .into_iter()
                .flatten()
                .filter(|uid| Some(*uid) != dispatch_uid.as_deref())
            {
                if children
                    .iter()
                    .filter(|row| row["accountUid"] == uid)
                    .count()
                    != 1
                {
                    return Err(err(
                        "KRAKEN_SUBACCOUNT_RELATIONSHIP_UNVERIFIED",
                        "A selected source/destination UID is not one uniquely authenticated owned subaccount of this dispatch key.",
                    ));
                }
            }
        }
        let relationship_iiban = if source.product != dest.product {
            if source_iiban.is_none() || source_iiban != destination_iiban {
                return Err(err(
                    "KRAKEN_CROSS_PRODUCT_IDENTITY_UNVERIFIED",
                    "Spot and Derivatives credentials do not report the same authoritative IIBAN. Select verified related product accounts.",
                ));
            }
            source_iiban.clone()
        } else {
            None
        };
        let from_wallet = p
            .get("fromAccount")
            .or_else(|| p.get("from"))
            .or_else(|| p.get("sourceWallet"))
            .and_then(Value::as_str)
            .unwrap_or("cash");
        let to_wallet = p
            .get("toAccount")
            .or_else(|| p.get("to"))
            .and_then(Value::as_str)
            .unwrap_or("spot");
        if e == E::KrakenPrivatePostAccountTransfer
            && (p["from"] != a.physical_identity || p["to"] != dest.physical_identity)
        {
            return Err(err(
                "KRAKEN_TRANSFER_IDENTITY_MISMATCH",
                "Transfer account IDs must match protected enrolled source/destination identities.",
            ));
        }
        if e == E::KrakenfuturesPrivatePostTransferSubaccount
            && (p["fromUser"].as_str() != source_uid.as_deref()
                || p["toUser"].as_str() != destination_uid.as_deref())
        {
            return Err(err(
                "KRAKEN_TRANSFER_IDENTITY_MISMATCH",
                "Transfer user IDs must match enrolled source/destination identities.",
            ));
        }
        if e == E::KrakenPrivatePostWalletTransfer
            && (a.product != crate::KrakenProduct::Spot
                || dest.product != crate::KrakenProduct::Derivatives
                || from_wallet != "Spot Wallet"
                || to_wallet != "Futures Wallet")
        {
            return Err(err(
                "KRAKEN_WALLET_ROUTE_UNSUPPORTED",
                "Spot-to-derivatives transfer requires separately enrolled related product accounts and the exact legacy wallet route.",
            ));
        }
        if e == E::KrakenfuturesPrivatePostWithdrawal && dest.product != crate::KrakenProduct::Spot
        {
            return Err(err(
                "KRAKEN_CROSS_PRODUCT_IDENTITY_UNVERIFIED",
                "Derivatives-to-spot withdrawal requires a separately enrolled, verified related spot account.",
            ));
        }
        let source_wallet = if from_wallet == "Spot Wallet" {
            "cash"
        } else {
            from_wallet
        };
        let destination_wallet = if to_wallet == "Futures Wallet" {
            "cash"
        } else {
            to_wallet
        };
        let from = if source.product == crate::KrakenProduct::Spot {
            source.resource.clone()
        } else {
            ResourceId(format!("{}:wallet:{}", source.resource.0, source_wallet))
        };
        let to = if dest.product == crate::KrakenProduct::Spot {
            dest.resource.clone()
        } else {
            ResourceId(format!("{}:wallet:{}", dest.resource.0, destination_wallet))
        };
        if source.product == crate::KrakenProduct::Spot {
            if self.free(source, &asset).await? < value {
                return Err(err(
                    "INSUFFICIENT_BALANCE",
                    "Available spot funds cannot cover this exact internal transfer.",
                ));
            }
        } else {
            let accounts = self
                .json(E::KrakenfuturesPrivateGetAccounts, Some(source), json!({}))
                .await?;
            let wallet = accounts["accounts"].get(source_wallet).ok_or_else(|| {
                err(
                    "KRAKEN_WALLET_UNKNOWN",
                    "Source derivatives wallet is not present in authenticated account metadata.",
                )
            })?;
            let observed = wallet["balances"]
                .get(code)
                .or_else(|| {
                    wallet["currencies"]
                        .get(code)
                        .and_then(|v| v.get("quantity"))
                })
                .ok_or_else(|| {
                    err(
                        "KRAKEN_TRANSFER_ASSET_MISSING",
                        "Source derivatives wallet has no exact balance for this collateral asset.",
                    )
                })?;
            let signed_available = crate::derivative_fee_units::signed_native(observed)?;
            if signed_available.is_negative() {
                return Err(err(
                    "KRAKEN_TRANSFER_SOURCE_LIABILITY_UNSUPPORTED",
                    "The selected source wallet has a native currency deficit instead of transferable cash. No absolute value, collateral valuation or guessed repayment is used as source principal.",
                ));
            }
            let available = Quantity::new(signed_available.to_string())?;
            if available < value {
                return Err(err(
                    "INSUFFICIENT_BALANCE",
                    "Observed derivatives custody cannot cover this exact transfer; margin availability is also enforced by Kraken.",
                ));
            }
        }
        if dest.product == crate::KrakenProduct::Derivatives {
            let destination = self
                .json(E::KrakenfuturesPrivateGetAccounts, Some(dest), json!({}))
                .await?;
            if destination["accounts"].get(destination_wallet).is_none() {
                return Err(err(
                    "KRAKEN_WALLET_UNKNOWN",
                    "Destination derivatives wallet is absent from authenticated metadata.",
                ));
            }
            let wallet = &destination["accounts"][destination_wallet];
            if let Some(native) = wallet["balances"].get(code).or_else(|| {
                wallet["currencies"]
                    .get(code)
                    .and_then(|v| v.get("quantity"))
            }) && crate::derivative_fee_units::signed_native(native)?.is_negative()
            {
                return Err(err(
                    "KRAKEN_COLLATERAL_LIABILITY_SETTLEMENT_UNPROVEN",
                    "The destination has a native currency deficit. This transfer would repay liability before creating usable collateral; GoFin cannot authorize plain cash credit without exact repayment effects.",
                ));
            }
        }
        if from == to {
            return Err(GoFinError::validation(
                "Select distinct source and destination wallets.",
            ));
        }
        let credit_asset = AssetId {
            network: dest.product.client().into(),
            reference: if dest.product == crate::KrakenProduct::Spot {
                exact_asset.reference
            } else {
                code.into()
            },
            decimals: 36,
        };
        Ok(TransferBinding {
            from,
            to,
            asset,
            credit_asset,
            amount: value,
            source_account: source.name.clone(),
            dispatch_account: a.name.clone(),
            dispatch_physical_identity: a.physical_identity.clone(),
            destination_account: dest.name.clone(),
            source_physical_identity: source.physical_identity.clone(),
            destination_physical_identity: dest.physical_identity.clone(),
            source_product: Some(source.product),
            destination_product: Some(dest.product),
            source_venue_uid: source_uid,
            destination_venue_uid: destination_uid,
            relationship_iiban,
            from_wallet: from_wallet.into(),
            to_wallet: to_wallet.into(),
        })
    }
    pub(crate) async fn reconcile_funding(
        &self,
        record: &OperationRecord,
        submission: &Submission,
    ) -> Result<Reconciliation> {
        self.reconcile_funding_inner(record, submission, None).await
    }
    pub(crate) async fn reconcile_funding_reference(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        reference: &gofin_domain::operation_recovery::RecoveryReference,
    ) -> Result<Reconciliation> {
        let binding = submission.data.get("binding").unwrap_or(&submission.data);
        if binding.get("funding").is_none() || reference.reference.trim().is_empty() {
            return Err(err(
                "RECEIPT_RECOVERY_UNSUPPORTED",
                "Reference recovery requires an original withdrawal binding and a nonempty reference.",
            ));
        }
        self.reconcile_funding_inner(record, submission, Some(reference))
            .await
    }
    async fn reconcile_funding_inner(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        reference: Option<&gofin_domain::operation_recovery::RecoveryReference>,
    ) -> Result<Reconciliation> {
        let binding = submission.data.get("binding").unwrap_or(&submission.data);
        let enrolled = self.account(text(binding, "account")?)?;
        let scoped = if let Some(id) = binding.get("walletAccountId").and_then(Value::as_str) {
            let Some(default_id) = binding
                .get("defaultWalletAccountId")
                .and_then(Value::as_str)
            else {
                return Ok(unknown(
                    "The original selected-wallet binding has no immutable default alias relationship; no current enrollment may replace it.",
                ));
            };
            let response = match self
                .json(
                    E::KrakenPrivatePostListWalletAccounts,
                    Some(enrolled),
                    json!({}),
                )
                .await
            {
                Ok(response) => response,
                Err(_) => {
                    return Ok(unknown(
                        "The original selected wallet ownership cannot currently be authenticated; no settlement or replay is authorized.",
                    ));
                }
            };
            if validate_default_wallet_relationship(&response, id).is_err() {
                return Ok(unknown(
                    "The original selected wallet is no longer uniquely active and owned; its original financial reservation remains unresolved.",
                ));
            }
            let mut resolved = enrolled.clone();
            resolved.wallet_account_id = Some(id.into());
            resolved.default_wallet_account_id = Some(default_id.into());
            if id != default_id {
                resolved.resource = ResourceId(format!("{}:wallet:{id}", enrolled.resource.0));
            }
            if binding.get("walletResource") != Some(&json!(resolved.resource))
                || binding["parameters"]["account_id"] != id
            {
                return Ok(unknown(
                    "The original verified wallet custody relationship changed; no settlement identity is inferred.",
                ));
            }
            resolved
        } else {
            enrolled.clone()
        };
        let a = &scoped;

        let Some(dispatch) = record
            .dispatch_evidence
            .as_ref()
            .and_then(|v| v.get("response"))
        else {
            return Ok(unknown(
                "The authenticated dispatch response was not durably recorded. No venue identity can be inferred or replayed.",
            ));
        };
        if let Some(b) = binding.get("funding") {
            return self
                .reconcile_withdrawal_reference(a, decode(b)?, submission, dispatch, reference)
                .await;
        }
        if let Some(b) = binding.get("earn") {
            return self
                .reconcile_earn(a, decode(b)?, submission, dispatch)
                .await;
        }
        if let Some(b) = binding.get("claim") {
            let claim: ClaimBinding = decode(b)?;
            let r = result(dispatch)?;
            // The authenticated claim response proves the exact assigned address;
            // only zero-fee claims can settle without separately identified fee rows.
            let addresses = if claim.api_is_legacy(binding) {
                r.as_array().cloned().unwrap_or_default()
            } else {
                vec![r["address_details"].clone()]
            };
            if addresses.is_empty()
                || addresses.iter().any(|v| {
                    !v.get("address")
                        .and_then(Value::as_str)
                        .is_some_and(|s| !s.is_empty())
                        && !v["crypto"]["address"]
                            .as_str()
                            .is_some_and(|s| !s.is_empty())
                        && !["iban", "account", "address"]
                            .iter()
                            .any(|key| v["fiat"][key].as_str().is_some_and(|s| !s.is_empty()))
                })
            {
                return Ok(unknown("Kraken did not prove an assigned deposit address."));
            }
            let mut effects = vec![FinancialEffect::AuthorityChange {
                resource: a.resource.clone(),
                authority: format!("kraken:deposit-address:{}", claim.method_id),
                before: binding_hash(&json!(claim.addresses_before))?,
                after: "claimed".into(),
            }];
            let mut evidence = json!({"assignedAddresses":addresses,"setupFee":claim.fee.to_decimal(claim.fee_asset.decimals),"financialMovement":!claim.fee.is_zero()});
            if !claim.fee.is_zero() {
                let Some(ledger_before) = claim.ledger_before.as_ref() else {
                    return Ok(unknown(
                        "Original nonzero setup fee record lacks its pre-dispatch ledger baseline; no prior charge can be rebooked or inferred.",
                    ));
                };
                let claimed = self
                    .funding_pages(
                        a,
                        E::KrakenFundingGetFundingV2DepositAddresses,
                        json!({"scope":{"method_id":claim.current_method_id.as_deref().unwrap_or(&claim.method_id)},"limit":500}),
                        "addresses",
                    )
                    .await?;
                let assigned = unique(&claimed, |v| {
                    v["method_id"]
                        == claim
                            .current_method_id
                            .as_deref()
                            .unwrap_or(&claim.method_id)
                        && if claim.api_is_legacy(binding) {
                            addresses.iter().any(|old| {
                                old["address"].as_str().is_some_and(|address| {
                                    v["address_details"]["crypto"]["address"] == address
                                })
                            })
                        } else {
                            v["address_details"] == r["address_details"]
                        }
                })?;
                let Some(id) = assigned
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                else {
                    return Ok(pending(
                        "Address assignment is acknowledged but no stable address identity can correlate its setup charge; no fee is inferred.",
                    ));
                };
                let l = self.ledger_asset(a, &claim.fee_asset).await?;
                let rows: Vec<_> = l
                    .as_object()
                    .expect("validated ledger")
                    .iter()
                    .filter(|(row_id, v)| {
                        !ledger_before.iter().any(|old| old == *row_id)
                            && v["refid"] == id
                            && v["asset"] == claim.fee_asset.reference
                            && matches!(
                                v["type"].as_str(),
                                Some("spend" | "withdrawal" | "custodytransfer")
                            )
                    })
                    .collect();
                if rows.len() != 1 {
                    return Ok(pending(
                        "Address assignment is verified, but its setup fee lacks one correlated ledger identity; financial reservations remain held.",
                    ));
                }
                let row = rows[0].1;
                let negative = signed(text(row, "amount")?, claim.fee_asset.decimals)?;
                let charged = negative.magnitude().checked_add(&qty(
                    row,
                    "fee",
                    claim.fee_asset.decimals,
                )?)?;
                if !negative.is_negative() && !negative.is_zero() {
                    return Ok(unknown(
                        "Correlated address setup charge differs from the exact approved fee.",
                    ));
                }
                effects.push(FinancialEffect::Cost {
                    location: AccountingLocation::Cash {
                        resource: a.resource.clone(),
                    },
                    amount: Amount {
                        asset: claim.fee_asset.clone(),
                        atoms: charged.clone(),
                    },
                    category: "kraken_deposit_address_setup".into(),
                    treatment: CostTreatment::AdditionalDebit,
                });
                evidence["setupLedger"] = json!({"identity":rows[0].0,"entry":row});
                evidence["chargedFee"] = json!(charged);
                evidence["ledgerBefore"] = json!(claim.ledger_before);
                if charged > claim.fee {
                    evidence["proofEvents"] = json!([{"eventId":format!("kraken:address-setup:{}",rows[0].0),"addressId":id,"assignedAddress":assigned,"ledgerId":rows[0].0,"entry":row}]);
                    return crate::unresolved_consumption::cost_overrun(
                        submission, &effects, evidence,
                    );
                }
            }
            return terminal(submission, Status::Completed, effects, evidence);
        }
        if let Some(b) = binding.get("transfer") {
            return self
                .reconcile_transfer(a, decode(b)?, submission, dispatch)
                .await;
        }
        if binding.get("cancelBefore").is_some() {
            let p = &binding["parameters"];
            let r = self
                .json(
                    E::KrakenPrivatePostWithdrawStatus,
                    Some(a),
                    json!({"asset":p["asset"]}),
                )
                .await?;
            let rows = result(&r)?.as_array().ok_or_else(|| {
                err(
                    "KRAKEN_EVIDENCE_INVALID",
                    "Withdrawal status is not a list.",
                )
            })?;
            let row = match unique(rows, |v| v["refid"] == p["refid"]) {
                Ok(v) => v,
                Err(_) => {
                    return Ok(unknown(
                        "The original withdrawal is not uniquely visible; cancellation is unresolved.",
                    ));
                }
            };
            if row["status-prop"] != "canceled" {
                return Ok(pending(
                    "Cancellation has not been proven; the original withdrawal may still settle.",
                ));
            }
            return terminal(
                submission,
                Status::Completed,
                vec![FinancialEffect::AuthorityChange {
                    resource: a.resource.clone(),
                    authority: format!("kraken:withdrawal:{}", text(p, "refid")?),
                    before: text(&binding["cancelBefore"], "status")?.into(),
                    after: "canceled".into(),
                }],
                json!({"withdrawal":row,"cancellation":true,"originalWithdrawalAccounting":"reconcile its original operation separately"}),
            );
        }
        Ok(unknown(
            "Persisted funding operation has no recognized settlement binding.",
        ))
    }
    #[cfg(test)]
    async fn reconcile_withdrawal(
        &self,
        a: &KrakenAccount,
        b: WithdrawalBinding,
        submission: &Submission,
        dispatch: &Value,
    ) -> Result<Reconciliation> {
        self.reconcile_withdrawal_reference(a, b, submission, dispatch, None)
            .await
    }
    async fn reconcile_withdrawal_reference(
        &self,
        a: &KrakenAccount,
        b: WithdrawalBinding,
        submission: &Submission,
        dispatch: &Value,
        recovery: Option<&gofin_domain::operation_recovery::RecoveryReference>,
    ) -> Result<Reconciliation> {
        let response = result(dispatch)?;
        let reference = if b.api_version == "legacy-v0" {
            text(response, "refid")
        } else {
            text(response, "withdrawal_id")
        };
        let Ok(reference) = reference else {
            return Ok(unknown(
                "The original dispatch has no durable withdrawal reference; matching by amount or time is forbidden.",
            ));
        };
        if recovery.is_some_and(|r| {
            r.kind == gofin_domain::operation_recovery::RecoveryReferenceKind::VenueReference
                && r.reference != reference
        }) {
            return Ok(unknown(
                "The supplied venue reference differs from the original durable withdrawal identity; no matching by amount or recipient is permitted.",
            ));
        }
        let mut dispatch_fee_cap = b.fee.clone();
        let (status, tx_hash, row) = if b.api_version == "legacy-v0" {
            let mut query =
                json!({"asset":b.venue_asset_code,"method":b.method_id,"aclass":b.asset_class});
            if b.asset_class == "tokenized_asset" {
                query["rebase_multiplier"] = json!(b.denomination);
            }
            let r = self
                .json(E::KrakenPrivatePostWithdrawStatus, Some(a), query)
                .await?;
            let list = result(&r)?.as_array().ok_or_else(|| {
                err(
                    "KRAKEN_EVIDENCE_INVALID",
                    "Legacy withdrawal status is malformed.",
                )
            })?;
            let row = match unique(list, |v| v["refid"] == reference) {
                Ok(v) => v,
                Err(_) => {
                    return Ok(unknown(
                        "The original withdrawal reference is absent or ambiguous in bounded history.",
                    ));
                }
            };
            if row["key"] != b.address_id
                || row["method"] != b.method_id
                || row["info"] != b.address
                || row["asset"] != b.asset.reference
                || qty(&row, "amount", b.asset.decimals)? != b.net
            {
                return Ok(unknown(
                    "Legacy withdrawal status differs from the frozen asset, recipient, method or exact amounts.",
                ));
            }
            (
                text(&row, "status")?.to_lowercase(),
                row.get("txid").and_then(Value::as_str).map(String::from),
                row,
            )
        } else {
            let mut query = json!({"asset":{"class":b.asset_class,"name":b.venue_asset_code},"scope":{"method_id":b.method_id},"limit":500});
            if b.asset_class == "tokenized_asset" {
                query["rebase_multiplier"] = json!(b.denomination);
            }
            let rows = self
                .funding_pages(
                    a,
                    E::KrakenFundingGetFundingV1Withdrawals,
                    query,
                    "withdrawals",
                )
                .await?;
            let row = match unique(&rows, |v| v["withdrawal_id"] == reference) {
                Ok(v) => v,
                Err(_) => {
                    return Ok(unknown(
                        "The original Funding v1 withdrawal is absent or ambiguous; no legacy fallback or new write is allowed.",
                    ));
                }
            };
            if row["address_id"] != b.address_id
                || row["method_id"] != b.method_id
                || row["amount"]["asset"]["name"] != b.venue_asset_code
                || row["fee"]["asset"]["name"] != b.fee_venue_code
                || row["amount"]["asset"]["class"] != b.asset_class
                || row["fee"]["asset"]["class"] != b.fee_asset_class
            {
                return Ok(unknown(
                    "Current withdrawal history differs from the frozen identity.",
                ));
            }
            if b.asset_class == "tokenized_asset"
                && ["gross_amount", "net_amount", "fee"].iter().any(|name| {
                    response[*name]
                        .get("rebase_multiplier")
                        .and_then(Value::as_str)
                        .is_some_and(|unit| unit != b.denomination)
                })
            {
                return Ok(unknown(
                    "Withdrawal response denomination differs from the immutable approval scope.",
                ));
            }
            if ["gross_amount", "net_amount"].iter().any(|name| {
                response[*name]["asset_amount"]["asset"]["class"] != b.asset_class
                    || response[*name]["asset_amount"]["asset"]["name"] != b.venue_asset_code
            }) || response["fee"]["asset_amount"]["asset"]["class"] != b.fee_asset_class
                || response["fee"]["asset_amount"]["asset"]["name"] != b.fee_venue_code
            {
                return Ok(unknown(
                    "Funding response principal or fee identity differs from the immutable approved denomination.",
                ));
            }
            // The persisted response fixes exact principal and fee denomination.
            // A lower same-asset fee may reduce gross debit, never change net or
            // exceed the immutable submitted caps. Fee-included requests whose
            // net changes remain unresolved rather than widening receipt scope.
            let dispatch_gross = qty(
                &response["gross_amount"]["asset_amount"],
                "amount",
                b.asset.decimals,
            )?;
            let dispatch_net = qty(
                &response["net_amount"]["asset_amount"],
                "amount",
                b.asset.decimals,
            )?;
            dispatch_fee_cap = qty(
                &response["fee"]["asset_amount"],
                "amount",
                b.fee_asset.decimals,
            )?;
            let expected_gross = if b.asset == b.fee_asset {
                dispatch_net.checked_add(&dispatch_fee_cap)?
            } else {
                dispatch_net.clone()
            };
            if dispatch_net != b.net
                || dispatch_gross != expected_gross
                || dispatch_gross > b.gross
                || dispatch_fee_cap > b.fee
            {
                return Ok(unknown(
                    "Funding dispatch principal or fee accounting differs from the immutable submitted caps.",
                ));
            }
            (
                text(&row, "status")?.to_owned(),
                row.get("onchain_transaction")
                    .and_then(Value::as_str)
                    .map(String::from),
                row,
            )
        };
        if recovery.is_some_and(|r| {
            r.kind == gofin_domain::operation_recovery::RecoveryReferenceKind::TransactionHash
                && tx_hash.as_deref() != Some(r.reference.as_str())
        }) {
            return Ok(unknown(
                "The supplied transaction is not independently linked by Kraken history to the original durable withdrawal identity.",
            ));
        }
        // Kraken may expose a withdrawal as Settled before its legacy status
        // endpoint calls it Success. Treat that venue state as the success
        // branch, while preserving the independently required source-ledger
        // debit and (when approved) destination-receipt proof below.
        if !matches!(
            status.as_str(),
            "success" | "settled" | "failure" | "failed"
        ) {
            return Ok(pending(
                "The venue withdrawal is not terminal; debit alone does not prove successful delivery.",
            ));
        }
        let reported_fee = if b.api_version == "legacy-v0" {
            qty(&row, "fee", b.fee_asset.decimals)?
        } else {
            qty(&row["fee"], "amount", b.fee_asset.decimals)?
        };
        let actual_gross = if b.asset == b.fee_asset {
            b.net.checked_add(&reported_fee)?
        } else {
            b.net.clone()
        };
        if b.api_version != "legacy-v0" {
            let history_amount = qty(&row["amount"], "amount", b.asset.decimals)?;
            if history_amount != b.net && history_amount != actual_gross {
                return Ok(unknown(
                    "Withdrawal history amount disagrees with exact net and final fee accounting.",
                ));
            }
        }
        let ledger = self.ledger_asset(a, &b.asset).await?;
        let separate_fee = if b.fee_asset != b.asset {
            Some(self.ledger_asset(a, &b.fee_asset).await?)
        } else {
            None
        };
        let correlated: Vec<_> = ledger
            .as_object()
            .expect("validated ledger")
            .iter()
            .filter(|(_, v)| v["refid"] == reference && v["asset"] == b.asset.reference)
            .collect();
        let matched: Vec<_> = correlated
            .iter()
            .copied()
            .filter(|(_, v)| v["type"] == "withdrawal")
            .collect();
        if !matches!(status.as_str(), "success" | "settled") {
            let debits: Vec<_> = matched
                .iter()
                .copied()
                .filter(|(_, v)| v["amount"].as_str().is_some_and(|n| n.starts_with('-')))
                .collect();
            let refunds: Vec<_> = correlated
                .iter()
                .copied()
                .filter(|(_, v)| {
                    matches!(
                        v["type"].as_str(),
                        Some("deposit" | "transfer" | "withdrawal")
                    ) && v["amount"].as_str().is_some_and(|n| !n.starts_with('-'))
                })
                .collect();
            if debits.len() != 1 || refunds.len() != 1 {
                return Ok(unknown(
                    "Failed withdrawal does not have one original debit and one uniquely correlated refund.",
                ));
            }
            if signed(text(debits[0].1, "amount")?, b.asset.decimals)?
                != SignedQuantity::negative(&b.net)
                || qty(refunds[0].1, "amount", b.asset.decimals)? != b.net
            {
                return Ok(unknown(
                    "Withdrawal debit/refund amounts differ from the original exact principal.",
                ));
            }
            let charged = if let Some(fee_ledger) = &separate_fee {
                if !qty(debits[0].1, "fee", b.asset.decimals)?.is_zero()
                    || !qty(refunds[0].1, "fee", b.asset.decimals)?.is_zero()
                {
                    return Ok(unknown(
                        "A separate-fee withdrawal also charged the principal asset; this exceeds the approved fee denomination.",
                    ));
                }
                let fee_rows = fee_ledger
                    .as_object()
                    .expect("validated ledger")
                    .iter()
                    .filter(|(_, v)| v["refid"] == reference && v["asset"] == b.fee_asset.reference)
                    .collect::<Vec<_>>();
                let fee_debits = fee_rows
                    .iter()
                    .copied()
                    .filter(|(_, v)| {
                        v["type"] == "withdrawal"
                            && v["amount"].as_str().is_some_and(|n| n.starts_with('-'))
                    })
                    .collect::<Vec<_>>();
                let fee_returns = fee_rows
                    .iter()
                    .copied()
                    .filter(|(_, v)| {
                        matches!(
                            v["type"].as_str(),
                            Some("deposit" | "transfer" | "withdrawal")
                        ) && v["amount"].as_str().is_some_and(|n| !n.starts_with('-'))
                    })
                    .collect::<Vec<_>>();
                if fee_debits.len() != 1
                    || fee_returns.len() > 1
                    || fee_rows.len() != fee_debits.len() + fee_returns.len()
                {
                    return Ok(unknown(
                        "Failed withdrawal fee asset lacks one original debit and at most one uniquely correlated return.",
                    ));
                }
                let debit = signed(text(fee_debits[0].1, "amount")?, b.fee_asset.decimals)?;
                let total = debit.magnitude().checked_add(&qty(
                    fee_debits[0].1,
                    "fee",
                    b.fee_asset.decimals,
                )?)?;
                if let Some((_, returned)) = fee_returns.first() {
                    let refunded = qty(returned, "amount", b.fee_asset.decimals)?;
                    let return_fee = qty(returned, "fee", b.fee_asset.decimals)?;
                    if refunded > total || return_fee > refunded {
                        return Ok(unknown(
                            "The fee-asset refund is inconsistent with its original debit.",
                        ));
                    }
                    total.checked_sub(&refunded.checked_sub(&return_fee)?)?
                } else {
                    total
                }
            } else {
                qty(debits[0].1, "fee", b.asset.decimals)?.checked_add(&qty(
                    refunds[0].1,
                    "fee",
                    b.asset.decimals,
                )?)?
            };
            if charged > b.fee {
                let costs = vec![FinancialEffect::Cost {
                    location: AccountingLocation::Cash {
                        resource: b.source.clone(),
                    },
                    amount: Amount {
                        asset: b.fee_asset.clone(),
                        atoms: charged,
                    },
                    category: "kraken_failed_withdrawal_fee".into(),
                    treatment: if b.fee_asset == b.asset {
                        CostTreatment::IncludedInDebit
                    } else {
                        CostTreatment::AdditionalDebit
                    },
                }];
                return crate::unresolved_consumption::cost_overrun(
                    submission,
                    &costs,
                    json!({"withdrawal":row,"proofEvents":[{"eventId":format!("kraken:withdrawal-fee:{}",reference),"withdrawalId":reference,"principalLedger":correlated,"feeLedger":separate_fee,"principalReturned":true}]}),
                );
            }
            let effects = if charged.is_zero() {
                vec![]
            } else {
                vec![
                    FinancialEffect::CashDebit {
                        resource: b.source.clone(),
                        amount: Amount {
                            asset: b.fee_asset.clone(),
                            atoms: charged.clone(),
                        },
                    },
                    FinancialEffect::Cost {
                        location: AccountingLocation::Cash {
                            resource: b.source.clone(),
                        },
                        amount: Amount {
                            asset: b.fee_asset.clone(),
                            atoms: charged.clone(),
                        },
                        category: "kraken_failed_withdrawal_fee".into(),
                        treatment: CostTreatment::IncludedInDebit,
                    },
                ]
            };
            return terminal(
                submission,
                Status::Failed,
                effects,
                json!({"withdrawal":row,"ledger":correlated,"feeLedger":separate_fee,"returnedPrincipal":b.net,"chargedFee":charged}),
            );
        }
        if matched.len() != 1 {
            return Ok(unknown(
                "Success requires one uniquely correlated withdrawal debit ledger row.",
            ));
        }
        let (ledger_id, debit) = matched[0];
        if signed(text(debit, "amount")?, b.asset.decimals)? == SignedQuantity::negative(&b.net)
            && reported_fee > b.fee
        {
            let (observed, fee_row) = if let Some(fees) = &separate_fee {
                let rows = fees
                    .as_object()
                    .expect("validated ledger")
                    .iter()
                    .filter(|(_, v)| {
                        v["refid"] == reference
                            && v["asset"] == b.fee_asset.reference
                            && v["type"] == "withdrawal"
                    })
                    .collect::<Vec<_>>();
                if rows.len() != 1 || !qty(debit, "fee", b.asset.decimals)?.is_zero() {
                    return Ok(unknown(
                        "Withdrawal fee overrun lacks one exact independent fee-asset ledger leg.",
                    ));
                }
                let paid = signed(text(rows[0].1, "amount")?, 36)?;
                if !paid.is_negative() {
                    return Ok(unknown("Withdrawal fee overrun lacks a native fee debit."));
                }
                (
                    paid.magnitude().checked_add(&qty(rows[0].1, "fee", 36)?)?,
                    json!({"id":rows[0].0,"entry":rows[0].1}),
                )
            } else {
                (qty(debit, "fee", b.asset.decimals)?, Value::Null)
            };
            if observed != reported_fee {
                return Ok(unknown(
                    "Withdrawal history and exact correlated ledger fee disagree; no fee overrun is inferred.",
                ));
            }
            let costs = vec![FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: b.source.clone(),
                },
                amount: Amount {
                    asset: b.fee_asset.clone(),
                    atoms: observed,
                },
                category: "kraken_withdrawal_fee".into(),
                treatment: if b.fee_asset == b.asset {
                    CostTreatment::IncludedInDebit
                } else {
                    CostTreatment::AdditionalDebit
                },
            }];
            return crate::unresolved_consumption::cost_overrun(
                submission,
                &costs,
                json!({"withdrawal":row,"proofEvents":[{"eventId":format!("kraken:withdrawal-fee:{}:{}",reference,ledger_id),"withdrawalId":reference,"ledgerId":ledger_id,"sourceLedger":debit,"feeLedger":fee_row}]}),
            );
        }
        if b.api_version == "legacy-v0" && reported_fee != b.fee {
            return Ok(unknown(
                "The final legacy fee differs from its frozen exact quote; no settlement is inferred.",
            ));
        }
        if (b.quoted_fee.is_none() && reported_fee > dispatch_fee_cap)
            || reported_fee > b.fee
            || actual_gross > b.gross
        {
            return Ok(unknown(
                "The final native fee or gross debit exceeds its immutable submitted accounting; no settlement is inferred.",
            ));
        }
        let principal_signed = signed(text(debit, "amount")?, b.asset.decimals)?;
        if principal_signed != SignedQuantity::negative(&b.net)
            || qty(debit, "fee", b.asset.decimals)?
                != if separate_fee.is_some() {
                    Quantity::zero()
                } else {
                    reported_fee.clone()
                }
        {
            return Ok(unknown(
                "Correlated source ledger does not prove the exact net principal and fee.",
            ));
        }
        let mut fee_proof = Value::Null;
        if let Some(fees) = &separate_fee {
            let rows = fees
                .as_object()
                .expect("validated fee ledger")
                .iter()
                .filter(|(_, v)| v["refid"] == reference && v["asset"] == b.fee_asset.reference)
                .collect::<Vec<_>>();
            if rows.len() != 1 {
                return Ok(unknown(
                    "Withdrawal fee requires one independently correlated exact fee-asset debit.",
                ));
            }
            let fee_debit = signed(text(rows[0].1, "amount")?, b.fee_asset.decimals)?;
            if !fee_debit.is_negative()
                || fee_debit.magnitude().checked_add(&qty(
                    rows[0].1,
                    "fee",
                    b.fee_asset.decimals,
                )?)? != reported_fee
            {
                return Ok(unknown(
                    "Correlated fee-asset debit differs from the original exact fee authorization.",
                ));
            }
            fee_proof = json!({"identity":rows[0].0,"entry":rows[0].1});
        }
        let mut effects = vec![FinancialEffect::CashDebit {
            resource: b.source.clone(),
            amount: Amount {
                asset: b.asset.clone(),
                atoms: actual_gross.clone(),
            },
        }];
        if !reported_fee.is_zero() {
            effects.push(FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: b.source.clone(),
                },
                amount: Amount {
                    asset: b.fee_asset.clone(),
                    atoms: reported_fee.clone(),
                },
                category: "kraken_withdrawal_fee".into(),
                treatment: if b.fee_asset == b.asset {
                    CostTreatment::IncludedInDebit
                } else {
                    CostTreatment::AdditionalDebit
                },
            });
        }
        let mut evidence = json!({"withdrawalId":reference,"apiVersion":b.api_version,"sourceLedgerId":ledger_id,"withdrawal":row,"sourceLedger":debit,"feeLedger":fee_proof,"verification":{"level":"venue_only","destinationReceipt":"unverified"}});
        if let Some(reference) = recovery {
            evidence["recoveryReference"] = json!(reference);
        }
        if b.verification == VerificationRequirement::DestinationReceipt {
            let Some(destination) = b.destination else {
                return Ok(unknown("Original destination receipt scope is missing."));
            };
            let Some(tx_hash) = tx_hash else {
                return Ok(pending(
                    "Kraken has not supplied a correlated destination transaction ID.",
                ));
            };
            let Some(verifier) = self.recipient_verifier.as_ref() else {
                return Ok(unknown(
                    "The original destination observer is unavailable; verification cannot be downgraded.",
                ));
            };
            let receipt = verifier
                .verify(KrakenPayoutRequest {
                    destination: destination.clone(),
                    tx_hash,
                    minimum: b.net.to_decimal(b.asset.decimals),
                    maximum: b.net.to_decimal(b.asset.decimals),
                })
                .await?;
            if receipt.status != Status::Completed {
                return Ok(Reconciliation {
                    status: if receipt.status == Status::Pending {
                        Status::Pending
                    } else {
                        Status::Unknown
                    },
                    evidence: json!({"withdrawalId":reference,"destinationReceipt":receipt.evidence,"reason":"Venue source debit is proven but destination completion remains unresolved; reservations are retained."}),
                    verified: None,
                });
            }
            let q = receipt.amount.ok_or_else(|| {
                err(
                    "KRAKEN_DESTINATION_PROOF_INVALID",
                    "Completed receipt omitted its exact quantity.",
                )
            })?;
            if q != Quantity::from_decimal(
                &b.net.to_decimal(b.asset.decimals),
                destination.asset.decimals,
            )? {
                return Ok(unknown(
                    "Destination receipt amount differs from the exact net withdrawal.",
                ));
            }
            if destination.enrolled {
                effects.push(FinancialEffect::UsableCredit {
                    to: destination.resource,
                    amount: Amount {
                        asset: destination.asset,
                        atoms: q,
                    },
                });
            }
            evidence["verification"] =
                json!({"level":"destination_receipt","destinationReceipt":receipt.evidence});
        }
        terminal(submission, Status::Completed, effects, evidence)
    }
    async fn reconcile_earn(
        &self,
        a: &KrakenAccount,
        b: EarnBinding,
        submission: &Submission,
        dispatch: &Value,
    ) -> Result<Reconciliation> {
        if b.accounting_version > 1 || (b.accounting_version == 0 && !b.max_fee.is_zero()) {
            return Ok(crate::trading::unresolved(
                Status::Unknown,
                "The original Earn accounting contract does not bind valid fee-origin provenance. Preserve its original approval, receipts and reservations; inspect original strategy/ledger evidence without rewriting or replaying this request.",
                json!({"compatibilityCode":"KRAKEN_EARN_ACCOUNTING_VERSION_UNSUPPORTED","accountingVersion":b.accounting_version}),
            ));
        }
        if result(dispatch)? != &json!(true) {
            return Ok(unknown(
                "The original Earn acknowledgment is not an authenticated acceptance.",
            ));
        }
        let r = self
            .json(
                if b.allocate {
                    E::KrakenPrivatePostEarnAllocateStatus
                } else {
                    E::KrakenPrivatePostEarnDeallocateStatus
                },
                Some(a),
                json!({"strategy_id":b.strategy}),
            )
            .await?;
        if result(&r)?["pending"] != false {
            return Ok(pending("The original strategy action is still pending."));
        }
        let allocations = self
            .json(E::KrakenPrivatePostEarnAllocations, Some(a), json!({}))
            .await?;
        let rows = result(&allocations)?
            .get("items")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                err(
                    "KRAKEN_EVIDENCE_INVALID",
                    "Earn allocation evidence is malformed.",
                )
            })?;
        let row = match unique(rows, |v| v["strategy_id"] == b.strategy) {
            Ok(v) => v,
            Err(_) => {
                return Ok(unknown(
                    "The selected Earn strategy has no unique allocation evidence.",
                ));
            }
        };
        let total = qty(
            &row["amount_allocated"]["total"],
            "native",
            b.asset.decimals,
        )?;
        let ledger = self.ledger(a, "all").await?;
        let mut candidates = std::collections::BTreeMap::<String, Vec<(String, Value)>>::new();
        for (id, v) in ledger.as_object().expect("validated ledger") {
            if b.ledger_before.contains(id)
                || !earn_asset(v["asset"].as_str().unwrap_or(""), &b.asset.reference)
                || !matches!(v["type"].as_str(), Some("staking" | "transfer"))
            {
                continue;
            }
            if let Some(refid) = v["refid"].as_str().filter(|s| !s.is_empty()) {
                candidates
                    .entry(refid.into())
                    .or_default()
                    .push((id.clone(), v.clone()));
            }
        }
        let valid: Vec<_> = candidates
            .into_iter()
            .filter(|(_, rows)| {
                rows.len() == 2
                    && rows.iter().any(|(_, v)| {
                        text(v, "amount").is_ok_and(|s| {
                            signed(s, b.asset.decimals)
                                .is_ok_and(|q| q == SignedQuantity::negative(&b.amount))
                        })
                    })
                    && rows.iter().any(|(_, v)| {
                        text(v, "amount").is_ok_and(|s| {
                            native_quantity(s, b.asset.decimals).is_ok_and(|q| q == b.amount)
                        })
                    })
            })
            .collect();
        // Positive immutable acknowledgment + last-request successful completion
        // under the persistent account/strategy lock + unique coherent new ledger
        // identity + allocation corroboration together establish this operation.
        // An amount match without any one of these independent pieces does not.
        let before_items = result(&b.allocations_before)?
            .get("items")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                err(
                    "KRAKEN_BINDING_INVALID",
                    "Original Earn allocation baseline is missing.",
                )
            })?;
        if rows
            .iter()
            .filter(|v| v["strategy_id"] != b.strategy)
            .any(|v| {
                before_items
                    .iter()
                    .find(|old| old["strategy_id"] == v["strategy_id"])
                    .is_none_or(|old| old["amount_allocated"] != v["amount_allocated"])
            })
            || before_items
                .iter()
                .filter(|v| v["strategy_id"] != b.strategy)
                .any(|old| {
                    rows.iter()
                        .find(|v| v["strategy_id"] == old["strategy_id"])
                        .is_none_or(|v| v["amount_allocated"] != old["amount_allocated"])
                })
        {
            return Ok(unknown(
                "Concurrent unexpected strategy allocation changes make Earn correlation ambiguous.",
            ));
        }
        if valid.len() != 1 {
            return Ok(unknown(
                "Earn completion needs a unique coherent new ledger pair in addition to strategy evidence. Keep its lock; no replay.",
            ));
        }
        let (reference, rows) = &valid[0];
        let negative = rows
            .iter()
            .find(|(_, v)| v["amount"].as_str().is_some_and(|s| s.starts_with('-')))
            .ok_or_else(|| {
                err(
                    "KRAKEN_EARN_LEDGER_INVALID",
                    "Earn source debit is missing.",
                )
            })?;
        let positive = rows
            .iter()
            .find(|(_, v)| v["amount"].as_str().is_some_and(|s| !s.starts_with('-')))
            .ok_or_else(|| {
                err(
                    "KRAKEN_EARN_LEDGER_INVALID",
                    "Earn destination credit is missing.",
                )
            })?;
        let cash = if b.allocate { negative } else { positive };
        let position_row = if b.allocate { positive } else { negative };
        if cash.1["asset"] != b.asset.reference || position_row.1["asset"] == b.asset.reference {
            return Ok(unknown(
                "Earn cash and position legs must have distinct venue custody denominations.",
            ));
        }
        let source_fee = qty(&negative.1, "fee", 36)?;
        let target_fee = qty(&positive.1, "fee", 36)?;
        if b.accounting_version == 0 && (!source_fee.is_zero() || !target_fee.is_zero()) {
            return Ok(unknown(
                "The original zero-fee Earn scope did not authorize this new native fee identity or provenance. Preserve its original receipt; no retroactive contract or replay is allowed.",
            ));
        }
        let aggregate_overrun = source_fee.checked_add(&target_fee)? > b.max_fee;
        let origin_overrun = source_fee > b.max_fee || target_fee > b.max_fee;
        let position = format!("kraken:earn:{}", b.strategy);
        let (position_change, cash_credit, position_fee, cash_fee) = if b.allocate {
            (
                b.amount.checked_sub(&target_fee)?,
                None,
                target_fee,
                source_fee,
            )
        } else {
            (
                b.amount.checked_add(&source_fee)?,
                Some(b.amount.clone()),
                source_fee,
                target_fee,
            )
        };
        let expected_total = if b.allocate {
            b.total_before.checked_add(&position_change)?
        } else {
            b.total_before.checked_sub(&position_change)?
        };
        if total != expected_total {
            return Ok(unknown(
                "Earn ledger principal/fees do not match the exact strategy allocation change; unexpected activity remains unresolved.",
            ));
        }
        let change = if b.allocate {
            SignedQuantity::positive(&position_change)
        } else {
            SignedQuantity::negative(&position_change)
        };
        let cash_origin = AccountingLocation::Cash {
            resource: b.source.clone(),
        };
        let position_origin = AccountingLocation::Position {
            resource: b.source.clone(),
            position: position.clone(),
            position_kind: gofin_domain::PositionKind::Supply,
        };
        let mut effects = vec![FinancialEffect::PositionChange {
            resource: b.source.clone(),
            position: position.clone(),
            asset: b.asset.clone(),
            delta: change,
            scaled_receipt: None,
            anchor: Some(gofin_domain::PositionAnchor {
                resource: b.source.clone(),
                position: position.clone(),
                position_kind: gofin_domain::PositionKind::Supply,
                principal: Amount {
                    asset: b.asset.clone(),
                    atoms: total,
                },
                scaled_receipt: None,
                evidence_version: 1,
                evidence: json!({"strategy":b.strategy,"allocation":row,"ledgerReference":reference,"ledgerRows":rows,"lastRequest":r,"physicalIdentity":a.physical_identity}),
                observed_at: now(),
            }),
        }];
        if b.accounting_version > 0 {
            effects.push(FinancialEffect::OriginMovement {
                from: if b.allocate {
                    cash_origin.clone()
                } else {
                    position_origin.clone()
                },
                to: if b.allocate {
                    position_origin.clone()
                } else {
                    cash_origin.clone()
                },
                debit: Amount {
                    asset: b.asset.clone(),
                    atoms: if b.allocate {
                        b.amount.clone()
                    } else {
                        position_change.clone()
                    },
                },
                credit: Amount {
                    asset: b.asset.clone(),
                    atoms: if b.allocate {
                        position_change.clone()
                    } else {
                        b.amount.clone()
                    },
                },
            });
        }

        if b.allocate {
            effects.push(FinancialEffect::CashDebit {
                resource: b.source.clone(),
                amount: Amount {
                    asset: b.asset.clone(),
                    atoms: b.amount.clone(),
                },
            });
        } else {
            effects.push(FinancialEffect::UsableCredit {
                to: b.source.clone(),
                amount: Amount {
                    asset: b.asset.clone(),
                    atoms: cash_credit.expect("deallocation credit"),
                },
            });
        }
        if !cash_fee.is_zero() {
            effects.push(FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: b.source.clone(),
                },
                amount: Amount {
                    asset: b.asset.clone(),
                    atoms: cash_fee,
                },
                category: "kraken_earn_fee".into(),
                treatment: if b.allocate {
                    CostTreatment::AdditionalDebit
                } else {
                    CostTreatment::DeductedFromCredit
                },
            });
        }
        if !position_fee.is_zero() {
            effects.push(FinancialEffect::Cost {
                location: if b.allocate {
                    cash_origin
                } else {
                    position_origin
                },
                amount: Amount {
                    asset: b.asset.clone(),
                    atoms: position_fee,
                },
                category: "kraken_earn_fee".into(),
                treatment: CostTreatment::IncludedInDebit,
            });
        }
        let evidence = json!({"strategy":b.strategy,"ledgerReference":reference,"ledgerRows":rows,"allocation":row,"lastRequest":r,"statusBefore":b.status_before,"allocationsBefore":b.allocations_before});
        if aggregate_overrun {
            let mut evidence = evidence;
            evidence["proofEvents"] = json!([{"eventId":format!("kraken:earn:{}:{}:{}",b.strategy,negative.0,positive.0),"strategy":b.strategy,"ledgerReference":reference,"ledgerRows":rows,"allocation":row,"lastRequest":r}]);
            let mut reconciliation =
                crate::unresolved_consumption::cost_overrun(submission, &effects, evidence)?;
            if !origin_overrun {
                use gofin_domain::unresolved_consumption::{ConsumptionReason, KEY, digest, read};
                let mut proof = read(&reconciliation.evidence)?.expect("fresh typed cost proof");
                proof.reason = ConsumptionReason::ProvenUnsettledCost;
                proof.proof_digest = digest(&proof)?;
                reconciliation.evidence[KEY] = serde_json::to_value(proof).map_err(|_| {
                    GoFinError::validation("Earn cost observation serialization failed.")
                })?;
            }
            return Ok(reconciliation);
        }
        terminal(submission, Status::Completed, effects, evidence)
    }
    async fn transfer_leg(
        &self,
        a: &KrakenAccount,
        wallet: &str,
        asset: &AssetId,
        id: &str,
        debit: bool,
        venue_uid: Option<&str>,
    ) -> Result<Option<Value>> {
        if a.product == crate::KrakenProduct::Spot {
            let l = self.ledger(a, &asset.reference).await?;
            let matching: Vec<_> = l
                .as_object()
                .expect("validated ledger")
                .iter()
                .filter(|(_, v)| {
                    v["refid"] == id
                        && v["asset"] == asset.reference
                        && matches!(
                            v["type"].as_str(),
                            Some("transfer" | "deposit" | "withdrawal")
                        )
                })
                .filter(|(_, v)| {
                    text(v, "amount").is_ok_and(|amount| amount.starts_with('-') == debit)
                })
                .collect();
            if matching.len() != 1 {
                return Ok(None);
            }
            let (ledger_id, row) = matching[0];
            let delta = signed(text(row, "amount")?, 36)?;
            if !qty(row, "fee", 36)?.is_zero() {
                return Ok(None);
            }
            return Ok(Some(
                json!({"identity":ledger_id,"delta":delta,"entry":row,"api":"legacy-v0-ledger"}),
            ));
        }
        let response=self.json(E::KrakenfuturesHistoryGetAccountLog,Some(a),json!({"count":500,"info":["transfer","subaccount transfer","cross-exchange transfer","deposit","withdrawal"]})).await?;
        if venue_uid.is_none() || response["accountUid"].as_str() != venue_uid {
            return Err(err(
                "KRAKEN_TRANSFER_ACCOUNT_MISMATCH",
                "Derivatives account log identity differs from the protected enrolled account.",
            ));
        }
        let rows = response["logs"].as_array().ok_or_else(|| {
            err(
                "KRAKEN_TRANSFER_LEDGER_INVALID",
                "Derivatives account log does not provide its typed entries.",
            )
        })?;
        let matching: Vec<_> = rows
            .iter()
            .filter(|v| {
                v["execution"] == id
                    && v["margin_account"] == wallet
                    && v["asset"]
                        .as_str()
                        .is_some_and(|code| code.eq_ignore_ascii_case(&asset.reference))
                    && matches!(
                        v["info"].as_str(),
                        Some(
                            "transfer"
                                | "subaccount transfer"
                                | "cross-exchange transfer"
                                | "deposit"
                                | "withdrawal"
                        )
                    )
            })
            .collect();
        if matching.len() != 1 {
            return Ok(None);
        }
        let row = matching[0];
        let old = native_quantity(&crate::observation::exact(&row["old_balance"])?, 36)?;
        let new = native_quantity(&crate::observation::exact(&row["new_balance"])?, 36)?;
        let delta = if debit {
            SignedQuantity::negative(&old.checked_sub(&new)?)
        } else {
            SignedQuantity::positive(&new.checked_sub(&old)?)
        };
        if row.get("fee").is_some_and(|v| {
            !v.is_null() && crate::observation::atoms(v).map_or(true, |q| !q.is_zero())
        }) {
            return Ok(None);
        }
        let identity = text(row, "booking_uid")?;
        Ok(Some(
            json!({"identity":identity,"delta":delta,"entry":row,"api":"derivatives-account-log"}),
        ))
    }
    async fn reconcile_transfer(
        &self,
        a: &KrakenAccount,
        b: TransferBinding,
        submission: &Submission,
        dispatch: &Value,
    ) -> Result<Reconciliation> {
        let r = result(dispatch)?;
        if r.get("result")
            .and_then(Value::as_str)
            .is_some_and(|status| status != "success")
        {
            return Ok(unknown(
                "The original transfer response did not acknowledge successful dispatch; no returned identity authorizes settlement.",
            ));
        }
        let Some(id) = r
            .get("transfer_id")
            .or_else(|| r.get("refid"))
            .or_else(|| r.get("uid"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            return Ok(unknown(
                "The transfer response lacks a durable transfer identity; matching balances cannot prove this transfer.",
            ));
        };
        let dest = self
            .related_accounts
            .iter()
            .chain(self.accounts.iter())
            .find(|v| v.name == b.destination_account)
            .ok_or_else(|| {
                err(
                    "KRAKEN_TRANSFER_DESTINATION_MISSING",
                    "Original destination account is no longer enrolled.",
                )
            })?;
        if b.dispatch_account != a.name || b.dispatch_physical_identity != a.physical_identity {
            return Ok(unknown(
                "The original dispatch credential account changed; a new enrollment cannot replace its authority identity.",
            ));
        }
        let source = if b.source_account == a.name {
            a
        } else {
            match self
                .related_accounts
                .iter()
                .chain(self.accounts.iter())
                .find(|candidate| candidate.name == b.source_account)
            {
                Some(source) => source,
                None => {
                    return Ok(unknown(
                        "The original independently enrolled transfer source is no longer available.",
                    ));
                }
            }
        };
        if b.source_physical_identity != source.physical_identity
            || b.destination_physical_identity != dest.physical_identity
            || b.source_product != Some(source.product)
            || b.destination_product != Some(dest.product)
        {
            return Ok(unknown(
                "The original enrolled transfer account identity changed or is absent; no newly enrolled account may replace it.",
            ));
        }
        let source_wallet = if b.from_wallet == "Spot Wallet" {
            "cash"
        } else {
            &b.from_wallet
        };
        let destination_wallet = if b.to_wallet == "Futures Wallet" {
            "cash"
        } else {
            &b.to_wallet
        };
        let source = self
            .transfer_leg(
                source,
                source_wallet,
                &b.asset,
                id,
                true,
                b.source_venue_uid.as_deref(),
            )
            .await?;
        let target = self
            .transfer_leg(
                dest,
                destination_wallet,
                &b.credit_asset,
                id,
                false,
                b.destination_venue_uid.as_deref(),
            )
            .await?;
        let (Some(source), Some(target)) = (source, target) else {
            return Ok(pending(
                "Original transfer identity has not yet proven both exact source and destination ledger legs.",
            ));
        };
        let debit: SignedQuantity = decode(&source["delta"])?;
        let credit: SignedQuantity = decode(&target["delta"])?;
        if debit != SignedQuantity::negative(&b.amount)
            || credit != SignedQuantity::positive(&b.amount)
        {
            return Ok(unknown(
                "Correlated transfer ledger legs differ from the exact approved principal.",
            ));
        }
        terminal(
            submission,
            Status::Completed,
            vec![FinancialEffect::Transfer {
                from: b.from,
                to: b.to,
                debit: Amount {
                    asset: b.asset,
                    atoms: b.amount.clone(),
                },
                credit: Amount {
                    asset: b.credit_asset,
                    atoms: b.amount,
                },
            }],
            json!({"transferId":id,"sourceLedger":source,"destinationLedger":target,"relationshipIiban":b.relationship_iiban,"sourceVenueUid":b.source_venue_uid,"destinationVenueUid":b.destination_venue_uid}),
        )
    }
}
impl ClaimBinding {
    fn api_is_legacy(&self, binding: &Value) -> bool {
        binding["endpoint"] == "private_post_create_deposit_address"
    }
}
fn unique<F: Fn(&Value) -> bool>(rows: &[Value], predicate: F) -> Result<Value> {
    let matching: Vec<_> = rows.iter().filter(|v| predicate(v)).collect();
    if matching.len() != 1 {
        return Err(err(
            "KRAKEN_EVIDENCE_AMBIGUOUS",
            "Funding evidence must resolve exactly one route or identity.",
        ));
    }
    Ok(matching[0].clone())
}

/// Native decimal parsing preserves every nonzero digit and the original wire
/// string. A positive sign or trailing zeros do not change a quantity's value.
pub fn native_quantity(value: &str, decimals: u8) -> Result<Quantity> {
    if value.is_empty() || value.len() > 256 {
        return Err(GoFinError::validation(
            "Native amount is absent or exceeds the exact bounded decimal representation.",
        ));
    }
    let value = value.strip_prefix('+').unwrap_or(value);
    let mut pieces = value.split('.');
    let integer = pieces.next().unwrap_or("");
    let fraction = pieces.next();
    if pieces.next().is_some()
        || (!integer.is_empty() && !integer.bytes().all(|c| c.is_ascii_digit()))
        || fraction.is_some_and(|v| !v.bytes().all(|c| c.is_ascii_digit()))
        || (integer.is_empty() && fraction.is_none_or(str::is_empty))
    {
        return Err(GoFinError::validation(
            "Native amount must be a finite unsigned decimal string.",
        ));
    }
    let fraction = fraction.unwrap_or("").trim_end_matches('0');
    let integer = if integer.is_empty() { "0" } else { integer };
    let normalized = if fraction.is_empty() {
        integer.to_owned()
    } else {
        format!("{integer}.{fraction}")
    };
    Quantity::from_decimal(&normalized, decimals)
}
fn signed(value: &str, decimals: u8) -> Result<SignedQuantity> {
    let (negative, unsigned) = match value.strip_prefix('-') {
        Some(v) if !v.starts_with('+') => (true, v),
        Some(_) => {
            return Err(GoFinError::validation(
                "Native ledger amount has multiple signs.",
            ));
        }
        None => (false, value),
    };
    let q = native_quantity(unsigned, decimals)?;
    Ok(if negative {
        SignedQuantity::negative(&q)
    } else {
        SignedQuantity::positive(&q)
    })
}

impl KrakenProfile {
    pub(crate) async fn observe_earn(
        &self,
        account: &KrakenAccount,
    ) -> Result<Vec<gofin_domain::PositionState>> {
        let response = self
            .json(
                E::KrakenPrivatePostEarnAllocations,
                Some(account),
                json!({}),
            )
            .await?;
        let data = result(&response)?;
        if data
            .get("next_cursor")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
        {
            return Err(err(
                "KRAKEN_EARN_COVERAGE_INCOMPLETE",
                "Earn allocation coverage is incomplete.",
            ));
        }
        let items = data
            .get("items")
            .and_then(Value::as_array)
            .ok_or_else(|| err("KRAKEN_EVIDENCE_INVALID", "Earn allocations are missing."))?;
        let mut positions = Vec::new();
        let mut strategies = std::collections::BTreeSet::new();
        for row in items {
            let strategy = text(row, "strategy_id")?;
            if !strategies.insert(strategy.to_owned()) {
                return Err(err(
                    "KRAKEN_EARN_AMBIGUOUS",
                    "Earn allocation strategy identity is duplicated.",
                ));
            }
            let asset = self.spot_asset(account, text(row, "native_asset")?).await?;
            let atoms = qty(&row["amount_allocated"]["total"], "native", asset.decimals)?;
            if atoms.is_zero() {
                continue;
            }
            positions.push(gofin_domain::PositionState {
                resource: account.resource.clone(),
                position: format!("kraken:earn:{strategy}"),
                kind: gofin_domain::PositionKind::Supply,
                amount: Amount { asset:asset.clone(), atoms:atoms.clone() },
                scaled_receipt: None,
                anchor: Some(gofin_domain::PositionAnchor{resource:account.resource.clone(),position:format!("kraken:earn:{strategy}"),position_kind:gofin_domain::PositionKind::Supply,principal:Amount{asset:asset.clone(),atoms:atoms.clone()},scaled_receipt:None,evidence_version:1,evidence:json!({"strategy":strategy,"allocation":row,"physicalIdentity":account.physical_identity}),observed_at:now()}),
            });
        }
        Ok(positions)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct KrakenDepositInput {
    pub request_id: String,
    pub account: String,
    pub wallet: String,
    pub method_id: String,
    #[serde(default)]
    pub chain_id: u64,
    pub amount: String,
    #[serde(default)]
    pub max_fee_eth: String,
    #[serde(default)]
    pub address_id: Option<String>,
    /// Kraken Funding v2 returns claimed crypto addresses without an address
    /// identifier. When a shared-address family has multiple claimed entries,
    /// bind the exact destination by its returned address string.
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub max_deposit_fee: Option<String>,
    #[serde(default)]
    pub denomination: Option<FundingDenomination>,
    /// Additive Solana source selector. EVM requests continue using chainId.
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub cluster: Option<String>,
    /// Explicit SPL mint; omitted for native SOL. Venue route must independently
    /// match this identity, and no symbol is used to infer arbitrary mints.
    #[serde(default)]
    pub mint: Option<String>,
    #[serde(default)]
    pub max_fee_sol: Option<String>,
    #[serde(default)]
    pub max_account_funding_sol: Option<String>,
}

pub fn deposit_definition() -> Definition {
    let mut input_schema = serde_json::to_value(schema_for!(KrakenDepositInput))
        .expect("deposit input schema serializes");
    input_schema["oneOf"] = json!([
        {"title":"Reviewed EVM source","required":["chainId","maxFeeEth"],"not":{"anyOf":[{"required":["network"]},{"required":["cluster"]},{"required":["mint"]},{"required":["maxFeeSol"]},{"required":["maxAccountFundingSol"]}]}},
        {"title":"Solana mainnet-beta source","required":["network","cluster","maxFeeSol"],"properties":{"network":{"const":"solana"},"cluster":{"const":"mainnet-beta"}},"not":{"anyOf":[{"required":["chainId"]},{"required":["maxFeeEth"]},{"required":["token"]},{"required":["denomination"]}]}}
    ]);
    Definition{name:"kraken.deposit".into(),description:"Deposit an exact native/ERC20 asset from a protected reviewed EVM wallet or native SOL or pinned mainnet USDC SPL from a protected Solana mainnet-beta wallet into an existing claimed Kraken address. Correlated finalized source receipt, exact usable Kraken credit and ledger proof establish completion.".into(),kind:OperationKind::Write,execution_actor:ExecutionActor::AgentInvocable,input_schema,output_schema:json!({"type":"object"}),error_schema:schema_for!(GoFinError).into(),examples:vec![json!({"requestId":"deposit-001","account":"kraken-spot","wallet":"wallet","methodId":"venue-method-id","chainId":1,"amount":"0.001","maxFeeEth":"0.0001"}),json!({"requestId":"deposit-sol-001","account":"kraken-spot","wallet":"sol-wallet","methodId":"venue-sol-method","network":"solana","cluster":"mainnet-beta","amount":"0.1","maxFeeSol":"0.00001"})],coverage:"Reviewed configured EVM chains and Solana mainnet-beta, using exact current Kraken currency methods, claimed addresses and asset routes.".into(),completion:"Exact source identity plus one usable transaction-correlated Kraken credit and ledger row.".into(),semantics:json!({"discoveryApi":"funding-v1-v2","creditCorrelationApi":"legacy-v0","addressClaim":"separately approved, never automatic","sourceReceiptAloneCompletes":false,"sourceNetworks":["reviewed EVM","solana:mainnet-beta"],"solanaAssets":["native SOL","pinned mainnet USDC mint"],"unknownOutcome":"reconcile original identity; never replay"})}
}

impl KrakenProfile {
    /// Broker-only financial composition: exact source/venue identities are
    /// supplied by reviewed adapters, never by a public arbitrary quote command.
    pub async fn value_funding_composition(
        &self,
        debits: &[ResourceAmount],
        costs: &[CostBound],
    ) -> Result<gofin_domain::Valuation> {
        self.financial_valuation(debits, costs).await
    }
}

fn earn_asset(code: &str, base: &str) -> bool {
    code == base
        || [".B", ".S", ".M", ".F"]
            .iter()
            .any(|suffix| code == format!("{base}{suffix}"))
}

fn crypto_kind() -> String {
    "crypto".into()
}

fn material_destination_details(value: &Value) -> Value {
    if let Some(crypto) = value.get("crypto") {
        return json!({"crypto":{"address":crypto["address"],"tag":crypto["tag"],"memo":crypto["memo"]}});
    }
    let mut fields = serde_json::Map::new();
    if let Some(fiat) = value.get("fiat") {
        for key in [
            "account",
            "iban",
            "masked_iban",
            "address",
            "routing",
            "sort",
            "swift",
            "bic",
            "bsb",
            "bank",
            "bank_code",
            "branch",
            "bank_address",
            "name_on_account",
            "beneficiary_name",
            "intermediary_bank",
            "intermediary_routing",
            "intermediary_swift",
            "memo",
            "tag",
        ] {
            if let Some(v) = fiat.get(key) {
                fields.insert(key.into(), v.clone());
            }
        }
    }
    json!({"fiat":fields})
}

fn currency_class() -> String {
    "currency".into()
}
fn rebased_unit() -> String {
    "rebased".into()
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FundingDenomination {
    Base,
    Rebased,
}
impl FundingDenomination {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Rebased => "rebased",
        }
    }
}

#[cfg(test)]
#[path = "funding/tests.rs"]
mod tests;
