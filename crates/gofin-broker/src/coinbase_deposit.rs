//! A frozen Base transfer completes only after exact Coinbase usable credit.
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gofin_coinbase::{
    CoinbaseProduct, CoinbaseProfile, CoinbaseQuery, CoinbaseRequest, CoinbaseRoute,
};
use gofin_domain::{
    Adapter, Amount, AssetId, Check, CheckStatus, Definition, EffectConstraint, ExecutionActor,
    FinancialEffect, FinancialIntent, FinancialState, GoFinError, OperationKind, OperationRecord,
    PreparedOperation, Quantity, Reconciliation, ResourceId, Result, RevalidatedOperation,
    StateSnapshot, Status, Submission, SubmissionJournal, now, verified_effect_digest,
};
use gofin_exchange::ExchangeProfile;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};

const BASE_USDC: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CoinbaseDepositInput {
    pub account: String,
    pub wallet: String,
    pub account_id: String,
    pub address_id: String,
    pub amount: String,
    pub request_id: String,
    pub max_fee_eth: String,
}
pub fn definition() -> Definition {
    Definition { name: "coinbase.deposit".into(),
        description: "Deposit native USDC from an enrolled Base wallet into an existing Coinbase Base address and verify usable credit.".into(),
        kind: OperationKind::Write, execution_actor: ExecutionActor::AgentInvocable,
        input_schema: schema_for!(CoinbaseDepositInput).into(), output_schema: json!({"type":"object"}),
        error_schema: schema_for!(GoFinError).into(),
        examples: vec![json!({"account":"coinbase","wallet":"wallet","accountId":"currency-account-uuid",
            "addressId":"existing-base-address-id","amount":"5","requestId":"deposit-001","maxFeeEth":"0.00001"})],
        coverage: "Base native USDC with existing exact Coinbase currency account and network-specific deposit address.".into(),
        completion: "Canonical source transfer, transaction-hash-correlated completed receive and independently observed usable Coinbase credit.".into(),
        semantics: json!({"chainId":8453,"network":"base","sourceContract":BASE_USDC,
            "addressCreation":"separate typed operation","sourceReceiptAloneCompletes":false,
            "unknownOutcome":"reconcile the original transaction; never replay"}) }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    account: String,
    account_id: String,
    address_id: String,
    address: String,
    resource: ResourceId,
    physical_identity: String,
    venue_asset: AssetId,
    gross: Quantity,
    free_before: Quantity,
    inner: PreparedOperation,
    #[serde(
        default,
        rename = "_gofin_usd_ceiling_v1",
        alias = "_fin_usd_ceiling_v1",
        skip_serializing_if = "Option::is_none"
    )]
    value_bounds: Option<Value>,
}
pub struct CoinbaseBaseDepositAdapter {
    profile: Arc<CoinbaseProfile>,
    base: Arc<gofin_ethereum::EthereumAdapter>,
}
fn error(code: &str, message: &str) -> GoFinError {
    GoFinError::new(
        code,
        "coinbase_deposit",
        message,
        "Inspect the original source and Coinbase transaction identities; never repeat an uncertain deposit.",
    )
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            error(
                "COINBASE_DEPOSIT_EVIDENCE_INVALID",
                "Exact Coinbase deposit evidence is missing.",
            )
        })
}
fn pending(reason: &str, detail: Value) -> Reconciliation {
    Reconciliation {
        status: Status::Pending,
        verified: None,
        evidence: json!({"reason":reason,"detail":detail,"awaiting":"venue_usable_credit","resubmissionAllowed":false}),
    }
}
impl CoinbaseBaseDepositAdapter {
    pub fn new(
        profile: Arc<CoinbaseProfile>,
        base: Arc<gofin_ethereum::EthereumAdapter>,
    ) -> Result<Self> {
        if base.config.chain_id != 8453 {
            return Err(GoFinError::validation(
                "Coinbase Base funding requires the Base runtime.",
            ));
        }
        Ok(Self { profile, base })
    }
    async fn read(&self, account: &str, route: CoinbaseRoute) -> Result<Value> {
        let response = self
            .profile
            .request_for_account(
                account,
                CoinbaseProduct::Advanced,
                route,
                CoinbaseRequest::get(),
                None,
            )
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(error(
                "COINBASE_DEPOSIT_READ_FAILED",
                "Coinbase funding evidence is unavailable or access was denied.",
            ));
        }
        serde_json::from_slice(&response.body).map_err(|_| {
            error(
                "COINBASE_DEPOSIT_EVIDENCE_INVALID",
                "Funding evidence is malformed.",
            )
        })
    }
    async fn balance(&self, account: &str, account_id: &str) -> Result<Quantity> {
        let value = self
            .read(
                account,
                CoinbaseRoute::Account {
                    account_id: account_id.into(),
                },
            )
            .await?;
        let row = &value["account"];
        if row["uuid"] != account_id
            || row["currency"] != "USDC"
            || row["active"].as_bool() != Some(true)
            || row["ready"].as_bool() != Some(true)
            || row["available_balance"]["currency"] != "USDC"
        {
            return Err(error(
                "COINBASE_DEPOSIT_ACCOUNT_INVALID",
                "Currency account identity or usable denomination differs from the selected route.",
            ));
        }
        Quantity::from_decimal(text(&row["available_balance"], "value")?, 36)
    }
    async fn app_rows(&self, account: &str, currency_id: Option<&str>) -> Result<Vec<Value>> {
        let mut cursor = None;
        let mut seen = BTreeSet::new();
        let mut rows = Vec::new();
        for _ in 0..64 {
            let query = CoinbaseQuery {
                limit: Some(100),
                starting_after: cursor.clone(),
                ..Default::default()
            };
            let route = match currency_id {
                Some(id) => CoinbaseRoute::Transactions {
                    account_id: id.into(),
                    query,
                },
                None => CoinbaseRoute::LegacyAccounts { query },
            };
            let value = self.read(account, route).await?;
            rows.extend(
                value["data"]
                    .as_array()
                    .ok_or_else(|| {
                        error(
                            "COINBASE_DEPOSIT_EVIDENCE_INVALID",
                            "App funding list is missing.",
                        )
                    })?
                    .iter()
                    .cloned(),
            );
            match value["pagination"]["next_starting_after"]
                .as_str()
                .filter(|s| !s.is_empty())
            {
                None => return Ok(rows),
                Some(next) if seen.insert(next.to_owned()) => cursor = Some(next.to_owned()),
                _ => {
                    return Err(error(
                        "COINBASE_DEPOSIT_COVERAGE_INCOMPLETE",
                        "App funding pagination repeats a cursor.",
                    ));
                }
            }
        }
        Err(error(
            "COINBASE_DEPOSIT_COVERAGE_INCOMPLETE",
            "App funding history exceeds the bounded page scope.",
        ))
    }
    async fn route(&self, input: &CoinbaseDepositInput) -> Result<(String, String, ResourceId)> {
        let account = self
            .profile
            .accounts()
            .iter()
            .find(|row| {
                row.name == input.account
                    && row.product == CoinbaseProduct::Advanced
                    && row.identity_verified
            })
            .ok_or_else(|| {
                error(
                    "COINBASE_DEPOSIT_ACCOUNT_INVALID",
                    "An authenticated Coinbase portfolio is required.",
                )
            })?;
        let permissions = self
            .read(&input.account, CoinbaseRoute::KeyPermissions)
            .await?;
        if permissions["portfolio_uuid"] != account.physical_identity
            || permissions["can_view"] != true
        {
            return Err(error(
                "COINBASE_DEPOSIT_PORTFOLIO_MISMATCH",
                "Current permissions do not attest the original readable portfolio.",
            ));
        }
        let rows = self.app_rows(&input.account, None).await?;
        let selected = rows
            .iter()
            .filter(|row| row["id"] == input.account_id && row["currency"]["code"] == "USDC")
            .collect::<Vec<_>>();
        if selected.len() != 1 {
            return Err(error(
                "COINBASE_DEPOSIT_ACCOUNT_INVALID",
                "App UUID must independently attest one exact USDC currency account.",
            ));
        }
        self.balance(&input.account, &input.account_id).await?;
        let value = self
            .read(
                &input.account,
                CoinbaseRoute::DepositAddresses {
                    account_id: input.account_id.clone(),
                },
            )
            .await?;
        if value["pagination"]["next_starting_after"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
        {
            return Err(error(
                "COINBASE_DEPOSIT_COVERAGE_INCOMPLETE",
                "Deposit addresses exceed the complete observed page.",
            ));
        }
        let rows = value["data"].as_array().ok_or_else(|| {
            error(
                "COINBASE_DEPOSIT_ADDRESS_INVALID",
                "Address list is missing.",
            )
        })?;
        let selected = rows
            .iter()
            .filter(|row| row["id"] == input.address_id)
            .collect::<Vec<_>>();
        if selected.len() != 1 {
            return Err(error(
                "COINBASE_DEPOSIT_ADDRESS_INVALID",
                "Select one existing Coinbase Base deposit address.",
            ));
        }
        let row = selected[0];
        let network = row["network"]
            .as_str()
            .or_else(|| row["network"]["name"].as_str());
        if network != Some("base")
            || row
                .get("destination_tag")
                .is_some_and(|tag| !tag.is_null() && tag.as_str() != Some(""))
        {
            return Err(error(
                "COINBASE_DEPOSIT_NETWORK_UNVERIFIED",
                "The address must explicitly attest Base and require no tag.",
            ));
        }
        Ok((
            gofin_evm::lower_address(gofin_evm::address(text(row, "address")?)?),
            account.physical_identity.clone(),
            account.resource.clone(),
        ))
    }
    async fn prepare_deposit(
        &self,
        value: &Value,
        old: Option<&Binding>,
    ) -> Result<PreparedOperation> {
        let input: CoinbaseDepositInput = serde_json::from_value(value.clone())
            .map_err(|_| GoFinError::validation("Use the closed coinbase.deposit schema."))?;
        if input.request_id.is_empty() || input.request_id.len() > 128 {
            return Err(GoFinError::validation(
                "A bounded deposit requestId is required.",
            ));
        }
        let source_atoms = Quantity::from_decimal(&input.amount, 6)?;
        let gross = Quantity::from_decimal(&input.amount, 36)?;
        if gross.is_zero() {
            return Err(GoFinError::validation("Deposit amount must be positive."));
        }
        let (address, physical_identity, resource) = self.route(&input).await?;
        let free_before = self.balance(&input.account, &input.account_id).await?;
        let inner_input = json!({"wallet":input.wallet,"network":"evm","chain_id":8453,"request_id":input.request_id,
            "max_fee_eth":input.max_fee_eth,"token":BASE_USDC,"amount":input.amount,"recipient":address});
        let inner = if let Some(binding) = old {
            if binding.inner.input != inner_input {
                return Err(error(
                    "PREVIEW_CHANGED",
                    "Deposit destination differs from the approved envelope.",
                ));
            }
            self.base.revalidate(&binding.inner).await?
        } else {
            self.base.prepare("wallet.transfer", &inner_input).await?
        };
        let source = inner.intent.debits.first().ok_or_else(|| {
            error(
                "COINBASE_DEPOSIT_SOURCE_INVALID",
                "Wallet preview omitted its source debit.",
            )
        })?;
        if source.amount.atoms != source_atoms
            || source.amount.asset.network != "ethereum:8453"
            || source.amount.asset.reference != BASE_USDC
            || source.amount.asset.decimals != 6
        {
            return Err(error(
                "COINBASE_DEPOSIT_SOURCE_INVALID",
                "Wallet preview differs from the native Base USDC identity and amount.",
            ));
        }
        let venue_asset = AssetId {
            network: "coinbase".into(),
            reference: "USDC".into(),
            decimals: 36,
        };
        let mut locks = inner.intent.locks.clone();
        locks.push(resource.clone());
        locks.sort();
        locks.dedup();
        let intent = FinancialIntent {
            debits: inner.intent.debits.clone(),
            maximum_costs: inner.intent.maximum_costs.clone(),
            locks,
            expected: vec![EffectConstraint::Transfer {
                from: source.resource.clone(),
                to: resource.clone(),
                debit_asset: source.amount.asset.clone(),
                credit_asset: venue_asset.clone(),
                maximum_debit: source_atoms,
                minimum_credit: gross.clone(),
            }],
        };
        let binding = Binding {
            account: input.account.clone(),
            account_id: input.account_id,
            address_id: input.address_id,
            address,
            resource,
            physical_identity,
            venue_asset,
            gross,
            free_before,
            inner: inner.clone(),
            value_bounds: None,
        };
        let mut prepared = PreparedOperation { operation: "coinbase.deposit".into(), input: value.clone(),
            binding: serde_json::to_value(binding).map_err(|_| GoFinError::validation("Deposit serialization failed."))?,
            summary: format!("Deposit {} native USDC from {} on Base into Coinbase {}", input.amount, input.wallet, input.account),
            checks: vec![Check { name: "exact_base_route".into(), status: CheckStatus::Passed,
                detail: "Existing Base address, exact App/Advanced UUID, permissioned portfolio and frozen native-USDC source envelope verified.".into() }],
            warnings: vec!["Source inclusion does not establish usable Coinbase credit. Any unexpected venue deduction retains the original pending identity.".into()],
            observed_at: inner.observed_at, expires_at: inner.expires_at, valuation: inner.valuation,
            intent, operation_version: 1, action_deadline: None };
        gofin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }
}
#[async_trait]
impl Adapter for CoinbaseBaseDepositAdapter {
    fn definitions(&self) -> Vec<Definition> {
        vec![definition()]
    }
    fn authorization_identities(&self) -> Vec<gofin_domain::ConnectionMetadata> {
        let mut rows = self.profile.authorization_identities();
        rows.extend(self.base.authorization_identities());
        rows
    }
    fn snapshot_identities(&self) -> Vec<gofin_domain::ConnectionMetadata> {
        // The constituent adapters observe custody. Reusing their identities
        // here would select this composition as a second scoped observer.
        vec![]
    }
    async fn snapshot(&self) -> Result<StateSnapshot> {
        Ok(StateSnapshot {
            state: FinancialState::default(),
            observed_at: now(),
            complete: true,
            coverage: vec!["Composition owns no separate holdings; the enrolled wallet and Coinbase profile observe their exact custody resources.".into()],
            pending: vec![],
        })
    }
    async fn read(&self, _: &str, _: &Value) -> Result<Value> {
        Err(GoFinError::validation(
            "Coinbase deposit is a financial write; preview it first.",
        ))
    }
    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        if operation != "coinbase.deposit" {
            return Err(GoFinError::validation("Unsupported deposit operation."));
        }
        self.prepare_deposit(input, None).await
    }
    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        if prepared.operation != "coinbase.deposit"
            || prepared.operation_version != 1
            || DateTime::parse_from_rfc3339(&prepared.expires_at)
                .map_or(true, |expiry| expiry <= Utc::now())
        {
            return Err(error(
                "PREVIEW_EXPIRED",
                "Original deposit preview expired or has an unsupported version.",
            ));
        }
        let old: Binding = serde_json::from_value(prepared.binding.clone()).map_err(|_| {
            error(
                "COINBASE_DEPOSIT_BINDING_INVALID",
                "Original binding is malformed.",
            )
        })?;
        let mut fresh = self.prepare_deposit(&prepared.input, Some(&old)).await?;
        let new: Binding = serde_json::from_value(fresh.binding.clone())
            .map_err(|_| GoFinError::validation("Fresh binding is malformed."))?;
        if old.address != new.address
            || old.resource != new.resource
            || old.physical_identity != new.physical_identity
            || old.free_before != new.free_before
            || old.inner.binding != new.inner.binding
        {
            return Err(error(
                "PREVIEW_CHANGED",
                "Deposit route, custody, balance or source envelope changed.",
            ));
        }
        gofin_domain::value_bounds::carry(prepared, &mut fresh)?;
        fresh.binding = prepared.binding.clone();
        Ok(fresh)
    }
    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        let validated = RevalidatedOperation::after_revalidation(
            self.revalidate(prepared).await?,
            Arc::new(gofin_domain::clock::SystemClock),
        )?;
        self.submit_revalidated(&validated, journal).await
    }
    async fn submit_revalidated(
        &self,
        validated: &RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        validated.assert_fresh()?;
        let prepared = validated.prepared();
        journal.assert_candidate(prepared)?;
        let binding: Binding = serde_json::from_value(prepared.binding.clone()).map_err(|_| {
            error(
                "COINBASE_DEPOSIT_BINDING_INVALID",
                "Original binding is malformed.",
            )
        })?;
        if prepared.operation != "coinbase.deposit"
            || prepared.operation_version != 1
            || binding.inner.operation != "wallet.transfer"
        {
            return Err(error(
                "COINBASE_DEPOSIT_BINDING_INVALID",
                "Deposit must bind one exact wallet transfer.",
            ));
        }
        let transaction: gofin_evm::FrozenTransaction =
            serde_json::from_value(binding.inner.binding["transaction"].clone()).map_err(|_| {
                error(
                    "COINBASE_DEPOSIT_BINDING_INVALID",
                    "Source envelope is malformed.",
                )
            })?;
        self.base.runtime.submit_nested_revalidated(&transaction,
            json!({"operation":"wallet.transfer","resource":text(&binding.inner.binding,"resource")?}), journal, validated, "/inner/binding").await
    }
    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
        let prepared = record.preview.as_ref().ok_or_else(|| {
            error(
                "COINBASE_DEPOSIT_BINDING_INVALID",
                "Original preview is missing.",
            )
        })?;
        let binding: Binding = serde_json::from_value(prepared.binding.clone()).map_err(|_| {
            error(
                "COINBASE_DEPOSIT_BINDING_INVALID",
                "Original binding is malformed.",
            )
        })?;
        let submission = record.submission.as_ref().ok_or_else(|| {
            error(
                "COINBASE_DEPOSIT_SUBMISSION_MISSING",
                "Original source transaction is missing.",
            )
        })?;
        if !self.profile.accounts().iter().any(|row| {
            row.name == binding.account
                && row.resource == binding.resource
                && row.physical_identity == binding.physical_identity
                && row.product == CoinbaseProduct::Advanced
        }) {
            return Err(error(
                "COINBASE_DEPOSIT_IDENTITY_CHANGED",
                "Original Coinbase portfolio is no longer configured.",
            ));
        }
        let source = self
            .base
            .reconcile(&OperationRecord {
                operation: "wallet.transfer".into(),
                input: binding.inner.input.clone(),
                preview: Some(binding.inner.clone()),
                ..record.clone()
            })
            .await?;
        if source.status != Status::Completed {
            return Ok(source);
        }
        let Some(mut proof) = source.verified else {
            return Ok(pending(
                "Canonical source lacks typed effects.",
                source.evidence,
            ));
        };
        if proof.submission != *submission {
            return Err(error(
                "COINBASE_DEPOSIT_PROOF_MISMATCH",
                "Source evidence differs from the original submission.",
            ));
        }
        let rows = self
            .app_rows(&binding.account, Some(&binding.account_id))
            .await?;
        let selected = rows
            .iter()
            .filter(|row| {
                row["network"]["hash"]
                    .as_str()
                    .or_else(|| row["network"]["transaction_hash"].as_str())
                    .is_some_and(|hash| hash.eq_ignore_ascii_case(&submission.reference))
            })
            .collect::<Vec<_>>();
        if selected.len() != 1 {
            return Ok(pending(
                "Exactly one receive must correlate to the original source hash.",
                source.evidence,
            ));
        }
        let row = selected[0];
        let network = row["network"]["name"]
            .as_str()
            .or_else(|| row["network"]["network"].as_str());
        if row["type"] != "receive"
            || row["status"] != "completed"
            || network != Some("base")
            || row["amount"]["currency"] != "USDC"
            || row["to"]["address"]
                .as_str()
                .is_none_or(|address| !address.eq_ignore_ascii_case(&binding.address))
        {
            return Ok(pending(
                "Receive is not completed with the original Base route identity.",
                json!({"source":source.evidence,"venue":row}),
            ));
        }
        let credit = Quantity::from_decimal(text(&row["amount"], "amount")?, 36)?;
        if credit != binding.gross
            || self.balance(&binding.account, &binding.account_id).await?
                < binding.free_before.checked_add(&credit)?
        {
            return Ok(pending(
                "Exact native-USDC credit is not independently available for trading.",
                json!({"source":source.evidence,"venue":row}),
            ));
        }
        let debit = binding.inner.intent.debits.first().ok_or_else(|| {
            error(
                "COINBASE_DEPOSIT_BINDING_INVALID",
                "Original source debit is missing.",
            )
        })?;
        let recipient = ResourceId(format!("ethereum:8453:{}", binding.address));
        let mut effects = vec![];
        let mut matched = false;
        for effect in proof.effects {
            match effect {
                FinancialEffect::Transfer {
                    from,
                    to,
                    debit: actual_debit,
                    credit: actual_credit,
                } if !matched
                    && from == debit.resource
                    && to == recipient
                    && actual_debit == debit.amount
                    && actual_credit == actual_debit =>
                {
                    matched = true;
                    effects.push(FinancialEffect::Transfer {
                        from,
                        to: binding.resource.clone(),
                        debit: actual_debit,
                        credit: Amount {
                            asset: binding.venue_asset.clone(),
                            atoms: credit.clone(),
                        },
                    });
                }
                FinancialEffect::Cost { .. } => effects.push(effect),
                _ => {
                    return Err(error(
                        "COINBASE_DEPOSIT_PROOF_MISMATCH",
                        "Unexpected source effects cannot become Coinbase custody credit.",
                    ));
                }
            }
        }
        if !matched {
            return Err(error(
                "COINBASE_DEPOSIT_PROOF_MISMATCH",
                "Canonical source omitted the exact deposit transfer.",
            ));
        }
        proof.effects = effects;
        proof.evidence = json!({"method":"canonical-base-and-coinbase-usable-credit-v1","source":source.evidence,
            "venue":{"id":row["id"],"type":row["type"],"status":row["status"],"amount":row["amount"],
                "network":{"name":"base","hash":submission.reference},"to":{"address":binding.address}},"freeBefore":binding.free_before});
        proof.observed_at = now();
        proof.proof_digest = verified_effect_digest(&proof)?;
        Ok(Reconciliation {
            status: Status::Completed,
            evidence: proof.evidence.clone(),
            verified: Some(proof),
        })
    }
}
