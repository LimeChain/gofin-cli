use crate::{KrakenEndpoint, KrakenResponse, contracts};
use async_trait::async_trait;
use gofin_domain::product_names::MetadataValueExt;
use gofin_domain::{ConnectionMetadata, Definition, GoFinError, Result};
use gofin_exchange::{
    ExchangeProfile,
    ccxt::CredentialProvider,
    kraken::{DispatchGuard, KrakenClient},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenAccount {
    pub name: String,
    pub resource: gofin_domain::ResourceId,
    pub credential_ref: String,
    pub physical_identity: String,
    #[serde(default)]
    pub wallet_account_id: Option<String>,
    #[serde(default)]
    pub default_wallet_account_id: Option<String>,
    pub identity_verified: bool,
    pub product: KrakenProduct,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KrakenProduct {
    Spot,
    Derivatives,
}
impl KrakenProduct {
    pub fn client(self) -> &'static str {
        match self {
            Self::Spot => "kraken",
            Self::Derivatives => "krakenfutures",
        }
    }
}
#[async_trait]
pub trait KrakenTransport: Send + Sync {
    async fn request(
        &self,
        endpoint: KrakenEndpoint,
        account: Option<&KrakenAccount>,
        parameters: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse>;
}
pub struct KrakenCcxtTransport {
    client: KrakenClient,
    credentials: Arc<dyn CredentialProvider>,
}
impl KrakenCcxtTransport {
    pub fn new(client: KrakenClient, credentials: Arc<dyn CredentialProvider>) -> Self {
        Self {
            client,
            credentials,
        }
    }
}
#[async_trait]
impl KrakenTransport for KrakenCcxtTransport {
    async fn request(
        &self,
        endpoint: KrakenEndpoint,
        account: Option<&KrakenAccount>,
        parameters: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        let credentials = account
            .map(|a| self.credentials.credentials(&a.credential_ref))
            .transpose()
            .map_err(|_| {
                error(
                    "KRAKEN_CREDENTIAL_UNAVAILABLE",
                    "Protected Kraken credentials are unavailable.",
                )
            })?;
        self.client
            .request(endpoint, credentials, parameters, guard)
            .await
            .map_err(|error| transport_error(endpoint, error))
    }
}
fn transport_error(endpoint: KrakenEndpoint, e: gofin_exchange::ccxt::CcxtError) -> GoFinError {
    use gofin_exchange::ccxt::CcxtErrorKind as K;
    let (code, message) = match e.kind {
        K::Authentication => (
            "KRAKEN_AUTHENTICATION_FAILED",
            "Kraken rejected authentication; API-level 2FA keys are not supported.",
        ),
        K::PermissionDenied => (
            "KRAKEN_PERMISSION_DENIED",
            "This key lacks the required permission, or approval expired before dispatch.",
        ),
        K::RateLimited => (
            "KRAKEN_RATE_LIMITED",
            "Kraken rate limited this request; reconcile any submitted write before retrying.",
        ),
        K::Timeout => (
            "KRAKEN_RESPONSE_TIMEOUT",
            "No complete Kraken response arrived; a submitted action may still have succeeded.",
        ),
        K::InvalidRequest => (
            "KRAKEN_INVALID_REQUEST",
            "This operation's request or credential format is invalid.",
        ),
        K::InvalidResponse => (
            "KRAKEN_INVALID_RESPONSE",
            "Kraken returned an invalid or oversized response.",
        ),
        _ => (
            "KRAKEN_UNAVAILABLE",
            "Kraken could not provide the requested operation result.",
        ),
    };
    error(code, message).with_details(json!({
        "operation":endpoint.operation(),
        "transport_kind":format!("{:?}", e.kind),
        "http_status":e.http_status,
        "funding_error_code":e.funding_error_code,
    }))
}
pub(crate) fn error(code: &str, message: &str) -> GoFinError {
    GoFinError::new(
        code,
        "kraken",
        message,
        "Inspect the typed operation schema, key permissions and the original operation status.",
    )
}

/// Separate product profiles share lifecycle while preserving venue meanings.
pub struct KrakenDispatch {
    pub endpoint: KrakenEndpoint,
    pub account: String,
    pub parameters: Value,
    pub submission: gofin_domain::Submission,
}

#[derive(Clone)]
pub struct KrakenProfile {
    pub(crate) product: KrakenProduct,
    pub(crate) accounts: Vec<KrakenAccount>,
    pub(crate) transport: Arc<dyn KrakenTransport>,
    pub(crate) prices: Option<Arc<dyn gofin_pricing::UsdPriceSource>>,
    pub(crate) recipient_verifier: Option<Arc<dyn crate::funding::KrakenRecipientVerifier>>,
    pub(crate) related_accounts: Vec<KrakenAccount>,
}
impl KrakenProfile {
    pub fn new(
        product: KrakenProduct,
        accounts: Vec<KrakenAccount>,
        transport: Arc<dyn KrakenTransport>,
    ) -> Result<Self> {
        let mut resources = std::collections::BTreeSet::new();
        let mut names = std::collections::BTreeSet::new();
        let mut physical_identities = std::collections::BTreeSet::new();
        for account in &accounts {
            if account.product != product
                || account.name.is_empty()
                || account.physical_identity.is_empty()
                || !resources.insert(account.resource.clone())
                || !names.insert(account.name.clone())
                || !physical_identities.insert(contracts::canonical_account_identity(
                    &account.physical_identity,
                ))
            {
                return Err(error(
                    "KRAKEN_ACCOUNT_INVALID",
                    "Kraken account product and physical resources must be distinct and explicit.",
                ));
            }
        }
        Ok(Self {
            product,
            accounts,
            transport,
            prices: None,
            recipient_verifier: None,
            related_accounts: vec![],
        })
    }
    pub fn with_prices(mut self, prices: Arc<dyn gofin_pricing::UsdPriceSource>) -> Self {
        self.prices = Some(prices);
        self
    }
    pub fn with_recipient_verifier(
        mut self,
        verifier: Arc<dyn crate::funding::KrakenRecipientVerifier>,
    ) -> Self {
        self.recipient_verifier = Some(verifier);
        self
    }
    pub fn with_related_accounts(mut self, accounts: Vec<KrakenAccount>) -> Self {
        self.related_accounts = accounts;
        self
    }
    pub(crate) fn account(&self, name: &str) -> Result<&KrakenAccount> {
        self.accounts
            .iter()
            .find(|a| a.name == name)
            .ok_or_else(|| {
                error(
                    "KRAKEN_ACCOUNT_UNKNOWN",
                    "Select an enrolled account for this Kraken product.",
                )
            })
    }
    /// Legacy deposit address reads cannot select a nondefault wallet account.
    /// Check the protected alias and, when configured, its authenticated ownership.
    pub async fn validate_legacy_deposit_default_scope(&self, name: &str) -> Result<()> {
        let account = self.account(name)?;
        if !account.identity_verified
            || account
                .wallet_account_id
                .as_ref()
                .is_some_and(|id| Some(id) != account.default_wallet_account_id.as_ref())
        {
            return Err(error(
                "KRAKEN_DEPOSIT_LEGACY_SCOPE_UNVERIFIED",
                "Legacy address verification requires the same verified default Kraken account scope.",
            ));
        }
        if account.default_wallet_account_id.is_some() {
            self.account_scopes(account).await?;
        }
        Ok(())
    }
    pub(crate) async fn json(
        &self,
        endpoint: KrakenEndpoint,
        account: Option<&KrakenAccount>,
        parameters: Value,
    ) -> Result<Value> {
        let mut parameters = parameters;
        if let Some(id) = account.and_then(|a| a.wallet_account_id.as_ref())
            && contracts::contract(endpoint)
                .schema
                .as_ref()
                .and_then(|s| s["properties"].get("account_id"))
                .is_some()
        {
            if parameters.get("account_id").is_some_and(|v| v != id) {
                return Err(error(
                    "KRAKEN_WALLET_ACCOUNT_MISMATCH",
                    "Read scope differs from its verified wallet account identity.",
                ));
            }
            parameters["account_id"] = json!(id);
        }
        contracts::validate_native(endpoint, &parameters)?;
        match self
            .transport
            .request(endpoint, account, parameters, None)
            .await?
        {
            KrakenResponse::Json(value) => Ok(value),
            KrakenResponse::Rejected { .. } => Err(error(
                "KRAKEN_ACTION_REJECTED",
                "Kraken rejected this request without a mutation.",
            )),
            KrakenResponse::Artifact { .. } => Err(error(
                "KRAKEN_ARTIFACT_OUTPUT_REQUIRED",
                "This operation returns an artifact; use its explicit output-file command.",
            )),
        }
    }
    async fn reconcile_rejected(
        &self,
        record: &gofin_domain::OperationRecord,
        submission: &gofin_domain::Submission,
    ) -> Result<Option<gofin_domain::Reconciliation>> {
        let Some(receipt) = record.dispatch_evidence.as_ref() else {
            return Ok(None);
        };
        if receipt.get("sealedRejection").is_none() {
            let response = &receipt["response"];
            if response["result"] == "error"
                || response.get("error").is_some_and(|v| {
                    !v.is_null()
                        && !v.as_array().is_some_and(Vec::is_empty)
                        && !v.as_str().is_some_and(str::is_empty)
                })
            {
                return Ok(Some(crate::trading::unresolved(
                    gofin_domain::Status::Unknown,
                    "Unsealed business rejection cannot prove a financial mutation or failure. Preserve this exact identity; do not adopt older matching orders or replay.",
                    json!({"endpoint":receipt["endpoint"]}),
                )));
            }
            return Ok(None);
        }
        let prepared = record.preview.as_ref().ok_or_else(|| {
            error(
                "KRAKEN_REJECTION_PROOF_INVALID",
                "Rejected submission has no protected original preview.",
            )
        })?;
        let mut planned = self.plan_submission(prepared).await?;
        if let Some(dispatch) = submission.data.get("dispatch") {
            let endpoint = KrakenEndpoint::ALL
                .iter()
                .find(|e| {
                    e.method() == dispatch["endpoint"]
                        && e.futures() == (self.product == KrakenProduct::Derivatives)
                        && e.mutating()
                })
                .copied()
                .ok_or_else(|| {
                    error(
                        "KRAKEN_REJECTION_PROOF_INVALID",
                        "Frozen rejection dispatch endpoint is unavailable for this exact product.",
                    )
                })?;
            if !dispatch["parameters"].is_object() {
                return Err(error(
                    "KRAKEN_REJECTION_PROOF_INVALID",
                    "Frozen dispatch parameters are unavailable.",
                ));
            }
            // Recovery uses the wire shape frozen in the original immutable
            // identity, even if a later release changes target-batch construction.
            planned.request.endpoint = endpoint;
            planned.request.parameters = dispatch["parameters"].clone();
        }
        // submittedAt is generated once during the original plan. All other
        // identities and frozen wire parameters must match the protected record.
        if receipt["submission"]
            != serde_json::to_value(submission).map_err(|_| {
                error(
                    "KRAKEN_REJECTION_PROOF_INVALID",
                    "Rejected submission identity invalid.",
                )
            })?
            || planned.submission.reference != submission.reference
            || planned.submission.kind != submission.kind
            || planned.submission.data["binding"] != submission.data["binding"]
            || planned.submission.data["operation"] != submission.data["operation"]
        {
            return Err(error(
                "KRAKEN_REJECTION_PROOF_INVALID",
                "Rejection is not bound to this original immutable submission.",
            ));
        }
        let digest = parameters_digest(&planned.request.parameters)?;
        if receipt["endpoint"] != planned.request.endpoint.method()
            || receipt["requestParametersSha256"] != digest
        {
            return Err(error(
                "KRAKEN_REJECTION_PROOF_INVALID",
                "Rejection does not match frozen request parameters.",
            ));
        }
        validate_rejection(
            &receipt["sealedRejection"],
            planned.request.endpoint,
            &digest,
        )?;
        if record.effects.as_ref().is_some_and(|proof| {
            proof.evidence["proofEvents"].as_array().is_some_and(|events|!events.is_empty())
                || proof.effects.iter().any(|effect| match effect {
                    gofin_domain::FinancialEffect::CompoundSubmissionMutations{children}=>children.iter().any(|child|matches!(child.outcome,gofin_domain::compound_mutations::ChildMutationOutcome::Accepted{..})),
                    gofin_domain::FinancialEffect::GroupedSubmissionAmendments{proof}=>proof.children.iter().any(|child|matches!(child.outcome,gofin_domain::compound_mutations::ChildMutationOutcome::Accepted{..})),
                    _=>false
                })
        }) {
            return Ok(Some(crate::trading::unresolved(gofin_domain::Status::Unknown,"A whole-request rejection contradicts already verified native mutation or fill evidence. Preserve the original proofs and reservations; never replay.",json!({"sealedRejection":receipt["sealedRejection"],"contradictoryPriorProof":true}))));
        }
        if prepared.binding["unifiedNativePartitions"].is_array() {
            let mut effects = Vec::new();
            let rejected = |tag: String, owner: String, original: gofin_domain::Submission| {
                gofin_domain::compound_mutations::MutationChildProof {
                    child_tag: tag,
                    operation_id: owner,
                    submission: original,
                    outcome: gofin_domain::compound_mutations::ChildMutationOutcome::Rejected {
                        code: "whole_request_no_mutation".into(),
                    },
                    native_evidence: json!({"sealedRejection":receipt["sealedRejection"]}),
                }
            };
            for expected in &prepared.intent.expected {
                match expected {
                    gofin_domain::EffectConstraint::GroupedSubmissionAmendments { group } => {
                        effects.push(gofin_domain::FinancialEffect::GroupedSubmissionAmendments {
                            proof: gofin_domain::grouped_amendments::GroupedAmendmentProof {
                                operation_id: group.operation_id.clone(),
                                submission: group.submission.clone(),
                                children: group
                                    .children
                                    .iter()
                                    .map(|c| {
                                        rejected(
                                            c.child_tag.clone(),
                                            group.operation_id.clone(),
                                            group.submission.clone(),
                                        )
                                    })
                                    .collect(),
                            },
                        })
                    }
                    gofin_domain::EffectConstraint::CompoundSubmissionMutations { children } => {
                        effects.push(gofin_domain::FinancialEffect::CompoundSubmissionMutations {
                            children: children
                                .iter()
                                .map(|c| {
                                    rejected(
                                        c.child_tag.clone(),
                                        c.operation_id.clone(),
                                        c.submission.clone(),
                                    )
                                })
                                .collect(),
                        })
                    }
                    _ => {}
                }
            }
            return Ok(Some(crate::trading::proof(
                submission,
                gofin_domain::Status::Failed,
                effects,
                json!({"sealedRejection":receipt["sealedRejection"],"proofEvents":[],"financialMutationOccurred":false}),
            )?));
        }
        if let Some(group) = prepared.binding.get("groupedAmendments") {
            let group: gofin_domain::grouped_amendments::GroupedAmendmentGroup =
                serde_json::from_value(group.clone()).map_err(|_| {
                    error(
                        "KRAKEN_REJECTION_PROOF_INVALID",
                        "Frozen grouped original invalid.",
                    )
                })?;
            let children = group
                .children
                .iter()
                .map(|c| gofin_domain::compound_mutations::MutationChildProof {
                    child_tag: c.child_tag.clone(),
                    operation_id: group.operation_id.clone(),
                    submission: group.submission.clone(),
                    outcome: gofin_domain::compound_mutations::ChildMutationOutcome::Rejected {
                        code: "whole_request_no_mutation".into(),
                    },
                    native_evidence: json!({"sealedRejection":receipt["sealedRejection"]}),
                })
                .collect();
            return Ok(Some(crate::trading::proof(
                submission,
                gofin_domain::Status::Failed,
                vec![gofin_domain::FinancialEffect::GroupedSubmissionAmendments {
                    proof: gofin_domain::grouped_amendments::GroupedAmendmentProof {
                        operation_id: group.operation_id,
                        submission: group.submission,
                        children,
                    },
                }],
                json!({"sealedRejection":receipt["sealedRejection"],"proofEvents":[],"financialMutationOccurred":false}),
            )?));
        }
        let children: Vec<gofin_domain::compound_mutations::MutationChild> =
            if let Some(children) = prepared.binding.get("compoundChildren") {
                serde_json::from_value(children.clone()).map_err(|_| {
                    error(
                        "KRAKEN_REJECTION_PROOF_INVALID",
                        "Frozen compound owners invalid.",
                    )
                })?
            } else {
                vec![]
            };
        let effects = if children.is_empty() {
            vec![]
        } else {
            vec![gofin_domain::FinancialEffect::CompoundSubmissionMutations {
                children: children
                    .into_iter()
                    .map(
                        |child| gofin_domain::compound_mutations::MutationChildProof {
                            child_tag: child.child_tag,
                            operation_id: child.operation_id,
                            submission: child.submission,
                            outcome:
                                gofin_domain::compound_mutations::ChildMutationOutcome::Rejected {
                                    code: "whole_request_no_mutation".into(),
                                },
                            native_evidence: json!({"sealedRejection":receipt["sealedRejection"]}),
                        },
                    )
                    .collect(),
            }]
        };
        Ok(Some(crate::trading::proof(
            submission,
            gofin_domain::Status::Failed,
            effects,
            json!({"sealedRejection":receipt["sealedRejection"],"proofEvents":[],"financialMutationOccurred":false}),
        )?))
    }
}
fn parameters_digest(parameters: &Value) -> Result<String> {
    use sha2::{Digest, Sha256};
    Ok(hex::encode(Sha256::digest(
        serde_json::to_vec(parameters).map_err(|_| {
            error(
                "KRAKEN_REQUEST_INVALID",
                "Exact request parameters cannot be encoded.",
            )
        })?,
    )))
}
fn validate_rejection(evidence: &Value, endpoint: KrakenEndpoint, digest: &str) -> Result<()> {
    let local = evidence["evidenceVersion"] == 2
        && evidence["receiptSource"] == "local_dispatch_guard"
        && evidence["dispatchOccurred"] == false
        && evidence["httpStatus"].is_null()
        && evidence["safeKind"] == "PermissionDenied"
        && evidence["phase"].as_str().is_some_and(|phase| {
            matches!(phase, "before_signing" | "before_http_dispatch")
                && (evidence["responseSha256"]
                    == hex::encode(Sha256::digest(format!(
                        "gofin-local-no-dispatch-v1:{phase}"
                    )))
                    || evidence["responseSha256"]
                        == hex::encode(Sha256::digest(format!("fin-local-no-dispatch-v1:{phase}"))))
        });
    let signing_failure = evidence["evidenceVersion"] == 3
        && evidence["receiptSource"] == "local_signing_failure"
        && evidence["dispatchOccurred"] == false
        && evidence["httpStatus"].is_null()
        && evidence["safeKind"] == "Rejected"
        && evidence["phase"] == "signing"
        && evidence["responseSha256"]
            == hex::encode(Sha256::digest(b"gofin-local-signing-failure-v1"));
    let native = evidence["evidenceVersion"] == 1
        && evidence["httpStatus"]
            .as_u64()
            .is_some_and(|status| (100..=599).contains(&status));
    if !(local || signing_failure || native)
        || evidence["classification"] != "whole_request_no_mutation"
        || evidence["endpoint"] != endpoint.method()
        || evidence["requestParametersSha256"] != digest
        || evidence["safeKind"].as_str().is_none_or(|s| {
            !matches!(
                s,
                "InvalidRequest"
                    | "InsufficientFunds"
                    | "Authentication"
                    | "PermissionDenied"
                    | "Rejected"
            )
        })
        || evidence["responseSha256"]
            .as_str()
            .is_none_or(|s| s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(error(
            "KRAKEN_REJECTION_PROOF_INVALID",
            "Transport rejection is not bound to the frozen endpoint and exact wire parameters.",
        ));
    }
    Ok(())
}

#[async_trait]
impl ExchangeProfile for KrakenProfile {
    type Request = KrakenDispatch;
    fn definitions(&self) -> Vec<Definition> {
        contracts::definitions(self.product.client())
    }
    fn submission_kind(&self) -> &str {
        match self.product {
            KrakenProduct::Spot => "kraken-rest-v1",
            KrakenProduct::Derivatives => "krakenfutures-rest-v1",
        }
    }
    fn connections(&self) -> Vec<ConnectionMetadata> {
        self.accounts.iter().map(|a|ConnectionMetadata{name:a.name.clone(),provider:self.product.client().into(),resource:a.resource.clone(),role:Some(match self.product{KrakenProduct::Spot=>"spot_and_margin",KrakenProduct::Derivatives=>"derivatives"}.into()),parent:None,network:None,address:None,credential_ref:None,signing_enabled:true,details:json!({"physical_identity":a.physical_identity,"identity_verified":a.identity_verified,"product":self.product,"default_wallet_account_id":a.default_wallet_account_id,"default_wallet_relationship_verification":if a.default_wallet_account_id.is_some(){"human_verified"}else{"not_selected"}})}).collect()
    }
    async fn snapshot(&self) -> Result<gofin_domain::StateSnapshot> {
        self.observe_financial().await
    }
    async fn snapshot_scoped(
        &self,
        scope: &gofin_domain::SnapshotScope,
    ) -> Result<gofin_domain::StateSnapshot> {
        self.observe_scoped_cash(scope).await
    }
    async fn validate_plan_feasibility(
        &self,
        operation: &str,
        input: &Value,
        _exact_scope: Option<&gofin_domain::financial_plans::FinancialPlanExactScope>,
    ) -> Result<Vec<String>> {
        self.validate_kraken_plan_feasibility(operation, input)
            .await
    }
    fn plan_exclusive_resources(
        &self,
        operation: &str,
        input: &Value,
    ) -> Result<Option<Vec<gofin_domain::ResourceId>>> {
        self.kraken_plan_exclusive_resources(operation, input)
    }
    async fn prepare(
        &self,
        operation: &str,
        input: &Value,
    ) -> Result<gofin_domain::PreparedOperation> {
        let endpoint = contracts::endpoint(operation)?;
        contracts::validate_gofin_controls(endpoint, input, None)?;
        if self.accounts.len() > 1 {
            self.check_native_custody_aliases().await?;
        }
        if contracts::contract(endpoint).client != self.product.client()
            || contracts::contract(endpoint).administrative
            || !contracts::contract(endpoint).mutating
        {
            return Err(error(
                "WRONG_OPERATION_KIND",
                "Select a financial write for this exact Kraken product.",
            ));
        }
        if crate::trading::is_trading(endpoint) {
            return self.prepare_trading(endpoint, input).await;
        }
        if crate::funding::is_funding(endpoint) {
            return self.prepare_funding(endpoint, input).await;
        }
        Err(error(
            "KRAKEN_FINANCIAL_IMPLEMENTATION_PENDING",
            "This typed operation has no reviewed financial intent and settlement handler. Nothing was dispatched.",
        ))
    }
    async fn prepare_with_compound_context(
        &self,
        operation: &str,
        input: &Value,
        context: &gofin_domain::PreparationContext,
        linked: &gofin_domain::compound_mutations::CompoundAmendmentContext,
    ) -> Result<gofin_domain::PreparedOperation> {
        contracts::validate_gofin_controls(contracts::endpoint(operation)?, input, None)?;
        if self.accounts.len() > 1 {
            self.check_native_custody_aliases().await?;
        }
        if contracts::endpoint(operation)? == KrakenEndpoint::KrakenfuturesPrivatePostBatchorder
            && !linked.children.is_empty()
        {
            return self.prepare_derivative_compound(input, linked).await;
        }
        self.prepare_with_linked_context(operation, input, context, &Default::default())
            .await
    }
    async fn prepare_with_linked_context(
        &self,
        operation: &str,
        input: &Value,
        _context: &gofin_domain::PreparationContext,
        linked: &gofin_domain::amendments::AmendmentContext,
    ) -> Result<gofin_domain::PreparedOperation> {
        let endpoint = contracts::endpoint(operation)?;
        let original_binding = linked
            .effective_scope()
            .map(|scope| &scope.binding)
            .or_else(|| linked.original.as_ref().map(|prepared| &prepared.binding));
        contracts::validate_gofin_controls(endpoint, input, original_binding)?;
        if input["validate"] == true {
            return self.prepare_trading(endpoint, input).await;
        }
        if self.accounts.len() > 1 {
            self.check_native_custody_aliases().await?;
        }
        let pinned = self.pinned_original_scope(linked).await?;
        if contracts::endpoint(operation)? == KrakenEndpoint::KrakenfuturesPrivatePostBatchorder
            && input["batchOrder"]
                .as_array()
                .is_some_and(|rows| rows.iter().any(|row| row["order"] == "edit"))
        {
            return self
                .prepare_derivative_edit_batch(input, pinned.as_ref())
                .await;
        }
        if contracts::endpoint(operation)? == KrakenEndpoint::KrakenfuturesPrivatePostEditorder {
            return self.prepare_derivative_edit(input, pinned.as_ref()).await;
        }
        if matches!(
            contracts::endpoint(operation)?,
            KrakenEndpoint::KrakenPrivatePostAmendOrder
                | KrakenEndpoint::KrakenPrivatePostEditOrder
        ) {
            let mut previous = pinned.as_ref().cloned();
            if let Some(scope) = &mut previous {
                attach_successor_lineage(&mut scope.binding, linked)?;
            }
            let endpoint = contracts::endpoint(operation)?;
            let (selection, parameters) = contracts::decode(endpoint, input)?;
            let account = self.account(
                selection
                    .account
                    .as_deref()
                    .ok_or_else(|| error("INVALID_INPUT", "Exact amendment account missing."))?,
            )?;
            self.spot_order_mutation_permissions(account, endpoint)
                .await?;
            if endpoint == KrakenEndpoint::KrakenPrivatePostEditOrder {
                self.spot_permissions(account, &["close-trades"]).await?;
            }
            if previous
                .as_ref()
                .is_some_and(|scope| scope.binding["mixedSpotMargin"].is_object())
            {
                return self
                    .prepare_mixed_batch_revision(
                        endpoint,
                        input,
                        account,
                        parameters,
                        &crate::trading::controls(input)?,
                        linked,
                    )
                    .await;
            }
            if previous
                .as_ref()
                .is_some_and(|s| s.binding["margin"].is_object())
            {
                if let Some(scope) = previous.as_ref().filter(|s| {
                    s.binding["margin"]["orders"]
                        .as_array()
                        .is_some_and(|orders| {
                            orders.iter().any(|o| o["conditionalParent"].is_string())
                        })
                }) {
                    return self
                        .prepare_issued_margin_child_revision(
                            endpoint,
                            input,
                            account,
                            parameters,
                            &crate::trading::controls(input)?,
                            scope,
                            linked.original_evidence.as_ref(),
                        )
                        .await;
                }
                return if endpoint == KrakenEndpoint::KrakenPrivatePostEditOrder {
                    self.prepare_margin_legacy_edit(
                        input,
                        account,
                        parameters,
                        &crate::trading::controls(input)?,
                        previous.as_ref(),
                    )
                    .await
                } else {
                    self.prepare_margin_amendment(
                        input,
                        account,
                        parameters,
                        &crate::trading::controls(input)?,
                        previous.as_ref(),
                    )
                    .await
                };
            }
            if let Some(scope) = previous.as_ref().filter(|scope| {
                scope.binding["conditionalClose"]
                    .as_array()
                    .is_some_and(|rows| !rows.is_empty())
            }) {
                return self
                    .prepare_issued_spot_child_revision(
                        endpoint,
                        input,
                        account,
                        parameters,
                        &crate::trading::controls(input)?,
                        scope,
                        linked.original_evidence.as_ref(),
                    )
                    .await;
            }
            if previous.as_ref().is_some_and(|s| {
                s.binding["orders"]
                    .as_array()
                    .is_some_and(|rows| rows.len() > 1)
            }) {
                return self
                    .prepare_spot_batch_revision(
                        endpoint,
                        input,
                        account,
                        parameters,
                        &crate::trading::controls(input)?,
                        previous.as_ref(),
                    )
                    .await;
            }
            return if endpoint == KrakenEndpoint::KrakenPrivatePostEditOrder {
                self.prepare_spot_legacy_edit(
                    input,
                    account,
                    parameters,
                    &crate::trading::controls(input)?,
                    previous.as_ref(),
                )
                .await
            } else {
                self.prepare_spot_amendment(
                    input,
                    account,
                    parameters,
                    &crate::trading::controls(input)?,
                    previous.as_ref(),
                )
                .await
            };
        }
        if matches!(
            contracts::endpoint(operation)?,
            KrakenEndpoint::KrakenfuturesPrivatePostCancelorder
                | KrakenEndpoint::KrakenfuturesPrivatePostCancelallorders
        ) && pinned.is_some()
        {
            let endpoint = contracts::endpoint(operation)?;
            let (_, parameters) = contracts::decode(endpoint, input)?;
            return self
                .prepare_derivative_control_with_scope(
                    endpoint,
                    input,
                    parameters,
                    pinned.as_ref().map(|s| &s.binding),
                )
                .await;
        }
        if matches!(
            contracts::endpoint(operation)?,
            KrakenEndpoint::KrakenPrivatePostCancelOrder
                | KrakenEndpoint::KrakenPrivatePostCancelOrderBatch
                | KrakenEndpoint::KrakenPrivatePostCancelAll
        ) && let Some(original) = linked.original.as_ref()
        {
            let mut scope = pinned
                .as_ref()
                .map(|s| s.binding.clone())
                .unwrap_or_else(|| original.binding.clone());
            attach_successor_lineage(&mut scope, linked)?;
            let endpoint = contracts::endpoint(operation)?;
            let (selection, parameters) = contracts::decode(endpoint, input)?;
            let account =
                self.account(selection.account.as_deref().ok_or_else(|| {
                    error("INVALID_INPUT", "Exact cancellation account missing.")
                })?)?;
            self.spot_permissions(
                account,
                &["query-open-trades", "query-closed-trades", "close-trades"],
            )
            .await?;
            return self
                .prepare_order_control_with_scope(
                    endpoint,
                    input,
                    account,
                    parameters,
                    &crate::trading::controls(input)?,
                    Some(&scope),
                )
                .await;
        }
        self.prepare(operation, input).await
    }
    async fn refresh_prepared(
        &self,
        prepared: &gofin_domain::PreparedOperation,
    ) -> Result<gofin_domain::PreparedOperation> {
        contracts::validate_gofin_controls(
            contracts::endpoint(&prepared.operation)?,
            &prepared.input,
            prepared
                .binding
                .get("amendmentPreviousScope")
                .and_then(|scope| scope.get("binding")),
        )?;
        if self.accounts.len() > 1 {
            self.check_native_custody_aliases().await?;
        }
        let mut current = if prepared.binding["unifiedOriginalContext"].is_object() {
            let linked = serde_json::from_value(prepared.binding["unifiedOriginalContext"].clone())
                .map_err(|_| {
                    error(
                        "KRAKEN_PREVIEW_INVALID",
                        "Frozen unified native ownership invalid.",
                    )
                })?;
            self.prepare_derivative_unified(&prepared.input, &linked)
                .await?
        } else if prepared.binding["groupedOriginalContext"].is_object() {
            let context: gofin_domain::amendments::AmendmentContext =
                serde_json::from_value(prepared.binding["groupedOriginalContext"].clone())
                    .map_err(|_| {
                        error(
                            "KRAKEN_PREVIEW_INVALID",
                            "Frozen grouped original context invalid.",
                        )
                    })?;
            let group: gofin_domain::grouped_amendments::GroupedAmendmentGroup =
                serde_json::from_value(prepared.binding["groupedAmendments"].clone()).map_err(
                    |_| {
                        error(
                            "KRAKEN_PREVIEW_INVALID",
                            "Frozen grouped ownership invalid.",
                        )
                    },
                )?;
            let linked = gofin_domain::compound_mutations::CompoundAmendmentContext {
                children: group
                    .children
                    .iter()
                    .map(|c| {
                        (
                            c.child_tag.clone(),
                            gofin_domain::compound_mutations::LinkedAmendmentContext {
                                operation_id: group.operation_id.clone(),
                                context: context.clone(),
                            },
                        )
                    })
                    .collect(),
            };
            self.prepare_derivative_grouped_edits(&prepared.input, &linked)
                .await?
        } else if prepared.binding["compoundContext"].is_object() {
            let linked = serde_json::from_value(prepared.binding["compoundContext"].clone())
                .map_err(|_| {
                    error(
                        "KRAKEN_PREVIEW_INVALID",
                        "Protected compound original scopes invalid.",
                    )
                })?;
            self.prepare_derivative_compound(&prepared.input, &linked)
                .await?
        } else if prepared.binding["cancellationOriginalScope"].is_object() {
            let endpoint = contracts::endpoint(&prepared.operation)?;
            let (selection, parameters) = contracts::decode(endpoint, &prepared.input)?;
            if self.product == KrakenProduct::Derivatives {
                return self
                    .prepare_derivative_control_with_scope(
                        endpoint,
                        &prepared.input,
                        parameters,
                        Some(&prepared.binding["cancellationOriginalScope"]),
                    )
                    .await;
            }
            let account =
                self.account(selection.account.as_deref().ok_or_else(|| {
                    error("INVALID_INPUT", "Exact cancellation account missing.")
                })?)?;
            self.spot_permissions(
                account,
                &["query-open-trades", "query-closed-trades", "close-trades"],
            )
            .await?;
            self.prepare_order_control_with_scope(
                endpoint,
                &prepared.input,
                account,
                parameters,
                &crate::trading::controls(&prepared.input)?,
                Some(&prepared.binding["cancellationOriginalScope"]),
            )
            .await?
        } else if prepared.binding["amendmentPreviousScope"].is_object() {
            let previous: gofin_domain::amendments::AmendmentScope =
                serde_json::from_value(prepared.binding["amendmentPreviousScope"].clone())
                    .map_err(|_| {
                        error(
                            "KRAKEN_PREVIEW_INVALID",
                            "Frozen prior amendment scope is invalid.",
                        )
                    })?;
            let endpoint = contracts::endpoint(&prepared.operation)?;
            if endpoint == KrakenEndpoint::KrakenfuturesPrivatePostBatchorder
                && prepared.binding["derivativeBatchEditTag"].is_string()
            {
                self.prepare_derivative_edit_batch(&prepared.input, Some(&previous))
                    .await?
            } else if endpoint == KrakenEndpoint::KrakenfuturesPrivatePostEditorder {
                self.prepare_derivative_edit(&prepared.input, Some(&previous))
                    .await?
            } else {
                let (selection, parameters) = contracts::decode(endpoint, &prepared.input)?;
                let account =
                    self.account(selection.account.as_deref().ok_or_else(|| {
                        error("INVALID_INPUT", "Exact amendment account missing.")
                    })?)?;
                self.spot_order_mutation_permissions(account, endpoint)
                    .await?;
                if previous.binding["mixedSpotMargin"].is_object() {
                    let context =
                        serde_json::from_value(prepared.binding["mixedAmendmentContext"].clone())
                            .map_err(|_| {
                            error(
                                "KRAKEN_PREVIEW_INVALID",
                                "Frozen original mixed amendment context is unavailable.",
                            )
                        })?;
                    self.prepare_mixed_batch_revision(
                        endpoint,
                        &prepared.input,
                        account,
                        parameters,
                        &crate::trading::controls(&prepared.input)?,
                        &context,
                    )
                    .await?
                } else if previous.binding["margin"].is_object() {
                    if prepared.binding["conditionalMarginChildRevision"] == true {
                        let evidence: gofin_domain::amendments::OriginalSubmissionEvidence =
                            serde_json::from_value(
                                prepared.binding["conditionalOriginalEvidence"].clone(),
                            )
                            .map_err(|_| {
                                error(
                                    "KRAKEN_PREVIEW_INVALID",
                                    "Frozen original margin child proof is unavailable.",
                                )
                            })?;
                        self.prepare_issued_margin_child_revision(
                            endpoint,
                            &prepared.input,
                            account,
                            parameters,
                            &crate::trading::controls(&prepared.input)?,
                            &previous,
                            Some(&evidence),
                        )
                        .await?
                    } else if endpoint == KrakenEndpoint::KrakenPrivatePostEditOrder {
                        self.spot_permissions(account, &["close-trades"]).await?;
                        self.prepare_margin_legacy_edit(
                            &prepared.input,
                            account,
                            parameters,
                            &crate::trading::controls(&prepared.input)?,
                            Some(&previous),
                        )
                        .await?
                    } else {
                        self.prepare_margin_amendment(
                            &prepared.input,
                            account,
                            parameters,
                            &crate::trading::controls(&prepared.input)?,
                            Some(&previous),
                        )
                        .await?
                    }
                } else if prepared.binding["conditionalChildRevision"] == true {
                    let original_evidence: gofin_domain::amendments::OriginalSubmissionEvidence =
                        serde_json::from_value(
                            prepared.binding["conditionalOriginalEvidence"].clone(),
                        )
                        .map_err(|_| {
                            error(
                                "KRAKEN_PREVIEW_INVALID",
                                "The frozen protected conditional entry proof is unavailable.",
                            )
                        })?;
                    self.prepare_issued_spot_child_revision(
                        endpoint,
                        &prepared.input,
                        account,
                        parameters,
                        &crate::trading::controls(&prepared.input)?,
                        &previous,
                        Some(&original_evidence),
                    )
                    .await?
                } else if previous.binding["orders"]
                    .as_array()
                    .is_some_and(|rows| rows.len() > 1)
                {
                    self.prepare_spot_batch_revision(
                        endpoint,
                        &prepared.input,
                        account,
                        parameters,
                        &crate::trading::controls(&prepared.input)?,
                        Some(&previous),
                    )
                    .await?
                } else if endpoint == KrakenEndpoint::KrakenPrivatePostEditOrder {
                    self.spot_permissions(account, &["close-trades"]).await?;
                    self.prepare_spot_legacy_edit(
                        &prepared.input,
                        account,
                        parameters,
                        &crate::trading::controls(&prepared.input)?,
                        Some(&previous),
                    )
                    .await?
                } else {
                    self.prepare_spot_amendment(
                        &prepared.input,
                        account,
                        parameters,
                        &crate::trading::controls(&prepared.input)?,
                        Some(&previous),
                    )
                    .await?
                }
            }
        } else {
            self.prepare(&prepared.operation, &prepared.input).await?
        };
        Self::preserve_withdrawal_quote_observation(&mut current, prepared);
        preserve_nested_baseline_times(&mut current.binding, &prepared.binding);
        preserve_admission_estimates(&mut current.binding, &prepared.binding);
        for (observed, approved) in current
            .intent
            .expected
            .iter_mut()
            .zip(&prepared.intent.expected)
        {
            if let (
                gofin_domain::EffectConstraint::SubmissionAmendment {
                    revision: observed, ..
                },
                gofin_domain::EffectConstraint::SubmissionAmendment {
                    revision: approved, ..
                },
            ) = (observed, approved)
            {
                preserve_nested_baseline_times(&mut observed.binding, &approved.binding);
                preserve_admission_estimates(&mut observed.binding, &approved.binding);
                preserve_valuation_observation_times(&mut observed.valuation, &approved.valuation);
            }
        }
        for (observed, approved) in current
            .intent
            .expected
            .iter_mut()
            .zip(&prepared.intent.expected)
        {
            if let (
                gofin_domain::EffectConstraint::CompoundSubmissionMutations { children: observed },
                gofin_domain::EffectConstraint::CompoundSubmissionMutations { children: approved },
            ) = (observed, approved)
            {
                for (observed, approved) in observed.iter_mut().zip(approved) {
                    if observed.child_tag != approved.child_tag
                        || observed.operation_id != approved.operation_id
                        || observed.submission != approved.submission
                    {
                        continue;
                    }
                    preserve_valuation_observation_times(
                        &mut observed.incremental_valuation,
                        &approved.incremental_valuation,
                    );
                    if let (
                        gofin_domain::compound_mutations::ChildMutation::Amend {
                            revision: observed,
                        },
                        gofin_domain::compound_mutations::ChildMutation::Amend {
                            revision: approved,
                        },
                    ) = (&mut observed.mutation, &approved.mutation)
                    {
                        preserve_nested_baseline_times(&mut observed.binding, &approved.binding);
                        preserve_admission_estimates(&mut observed.binding, &approved.binding);
                        preserve_valuation_observation_times(
                            &mut observed.valuation,
                            &approved.valuation,
                        );
                    }
                }
            }
        }
        for (observed, approved) in current
            .intent
            .expected
            .iter_mut()
            .zip(&prepared.intent.expected)
        {
            if let (
                gofin_domain::EffectConstraint::GroupedSubmissionAmendments { group: observed },
                gofin_domain::EffectConstraint::GroupedSubmissionAmendments { group: approved },
            ) = (observed, approved)
            {
                for (now, old) in observed.children.iter_mut().zip(&approved.children) {
                    if now.child_tag == old.child_tag {
                        preserve_valuation_observation_times(
                            &mut now.scope.valuation,
                            &old.scope.valuation,
                        );
                    }
                }
                preserve_valuation_observation_times(
                    &mut observed.approved_envelope.valuation,
                    &approved.approved_envelope.valuation,
                );
                preserve_valuation_observation_times(
                    &mut observed.incremental_valuation,
                    &approved.incremental_valuation,
                );
                if current.binding["unifiedNativePartitions"].is_array() {
                    for frame in current.binding["unifiedNativePartitions"]
                        .as_array_mut()
                        .unwrap()
                    {
                        if frame["groupedAmendments"]["operation_id"] == observed.operation_id {
                            frame["groupedAmendments"] = json!(observed);
                        }
                    }
                } else {
                    current.binding["groupedAmendments"] = json!(observed);
                }
            }
        }
        if let Some(children) = current
            .intent
            .expected
            .iter()
            .find_map(|effect| match effect {
                gofin_domain::EffectConstraint::CompoundSubmissionMutations { children } => {
                    Some(children)
                }
                _ => None,
            })
        {
            if current.binding["unifiedNativePartitions"].is_array() {
                for frame in current.binding["unifiedNativePartitions"]
                    .as_array_mut()
                    .unwrap()
                {
                    if frame["compoundChildren"].is_array() {
                        frame["compoundChildren"] = json!(children);
                    }
                }
            } else {
                current.binding["compoundChildren"] = json!(children);
            }
        }
        if prepared
            .binding
            .product_get(gofin_domain::BASELINE_ANCHORS_KEY)?
            .is_some()
        {
            let approved: Vec<gofin_domain::PositionAnchor> = serde_json::from_value(
                prepared
                    .binding
                    .product_value(gofin_domain::BASELINE_ANCHORS_KEY)?
                    .clone(),
            )
            .map_err(|_| {
                error(
                    "KRAKEN_PREVIEW_INVALID",
                    "Approved native baseline anchors invalid.",
                )
            })?;
            let mut observed: Vec<gofin_domain::PositionAnchor> = serde_json::from_value(
                current
                    .binding
                    .product_value(gofin_domain::BASELINE_ANCHORS_KEY)?
                    .clone(),
            )
            .map_err(|_| {
                error(
                    "KRAKEN_PREVIEW_CHANGED",
                    "Refreshed native baseline evidence missing.",
                )
            })?;
            for (anchor, old) in observed.iter_mut().zip(&approved) {
                anchor.observed_at = old.observed_at.clone();
            }
            if observed != approved {
                return Err(error(
                    "KRAKEN_PREVIEW_CHANGED",
                    "Native contract position changed since approval; review a new exact preview.",
                ));
            }
            current.binding[gofin_domain::BASELINE_ANCHORS_KEY] = prepared
                .binding
                .product_value(gofin_domain::BASELINE_ANCHORS_KEY)?
                .clone();
        }
        if prepared.valuation.as_ref().is_some_and(|v| {
            v.prices.is_empty()
                && v.movement_usd_micros.is_zero()
                && v.maximum_fee_usd_micros.is_zero()
                && v.additional_allocation_usd_micros.is_zero()
        }) {
            preserve_valuation_observation_times(&mut current.valuation, &prepared.valuation);
        }
        if prepared
            .binding
            .product_get(gofin_domain::value_bounds::KEY)?
            .is_some()
        {
            current.binding[gofin_domain::value_bounds::KEY] = prepared
                .binding
                .product_value(gofin_domain::value_bounds::KEY)?
                .clone();
            gofin_domain::value_bounds::validate(&current)?;
        }
        gofin_domain::product_names::preserve_legacy_client_ids(
            &prepared.binding,
            &mut current.binding,
        );
        Ok(current)
    }
    fn valuation_matches(
        &self,
        approved: &gofin_domain::PreparedOperation,
        refreshed: &gofin_domain::PreparedOperation,
    ) -> bool {
        if gofin_domain::product_names::validate(&approved.binding).is_err()
            || gofin_domain::product_names::validate(&refreshed.binding).is_err()
        {
            return false;
        }
        if approved
            .binding
            .product_get(gofin_domain::value_bounds::KEY)
            .ok()
            .flatten()
            .is_some()
        {
            gofin_domain::value_bounds::validate(refreshed).is_ok()
        } else {
            approved.valuation == refreshed.valuation
        }
    }
    async fn plan_submission(
        &self,
        current: &gofin_domain::PreparedOperation,
    ) -> Result<gofin_exchange::PlannedSubmission<Self::Request>> {
        let endpoint = contracts::endpoint(&current.operation)?;
        let account = current.binding["account"].as_str().ok_or_else(|| {
            error(
                "KRAKEN_PREVIEW_INVALID",
                "Exact account binding is missing.",
            )
        })?;
        let enrolled = self.account(account)?;
        if current.binding["physicalIdentity"] != enrolled.physical_identity {
            return Err(error(
                "KRAKEN_ACCOUNT_IDENTITY_CHANGED",
                "The protected physical Kraken identity changed after approval.",
            ));
        }
        let request_id = current.binding["requestId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                error(
                    "KRAKEN_PREVIEW_INVALID",
                    "Durable request identity is missing.",
                )
            })?;
        let reference = gofin_domain::binding_hash(
            &json!({"version":1,"product":self.product,"physicalIdentity":enrolled.physical_identity,"requestId":request_id,"operation":current.operation}),
        )?;
        let mut submission = gofin_domain::Submission {
            kind: self.submission_kind().into(),
            reference,
            data: json!({"binding":current.binding,"operation":current.operation,"submittedAt":gofin_domain::now()}),
        };
        let mut dispatch_endpoint = endpoint;
        let mut parameters = current.binding["parameters"].clone();
        // Freeze cancellation target identities even for cancel-all/userref selectors.
        if let Some(targets) = current.binding["targets"].as_array()
            && !targets.is_empty()
        {
            if endpoint.futures() {
                dispatch_endpoint = KrakenEndpoint::KrakenfuturesPrivatePostBatchorder;
                parameters = json!({"batchOrder":targets.iter().enumerate().map(|(i,t)|json!({"order":"cancel","order_tag":format!("gofin-cancel-{i}"),"order_id":t["orderId"]})).collect::<Vec<_>>()});
            } else {
                dispatch_endpoint = KrakenEndpoint::KrakenPrivatePostCancelOrderBatch;
                parameters = json!({"orders":targets.iter().map(|t|json!({"txid":t["orderId"]})).collect::<Vec<_>>()});
            }
        }
        contracts::validate_native(dispatch_endpoint, &parameters)?;
        submission.data["dispatch"] =
            json!({"endpoint":dispatch_endpoint.method(),"parameters":parameters});
        Ok(gofin_exchange::PlannedSubmission {
            submission: submission.clone(),
            request: KrakenDispatch {
                endpoint: dispatch_endpoint,
                account: account.into(),
                parameters,
                submission,
            },
        })
    }
    async fn dispatch(
        &self,
        request: &Self::Request,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> Result<Value> {
        journal.assert_authorized()?;
        let guard_journal = journal.clone();
        let guard: DispatchGuard = Arc::new(move || guard_journal.assert_authorized().is_ok());
        let account = self.account(&request.account)?;
        let response = self
            .transport
            .request(
                request.endpoint,
                Some(account),
                request.parameters.clone(),
                Some(guard),
            )
            .await?;
        match response {
            KrakenResponse::Json(mut response) => {
                redact_credentials(&mut response);
                journal.persist_dispatch_evidence(
                    &request.submission,
                    &json!({"endpoint":request.endpoint.method(),"response":response}),
                )?;
                Ok(response)
            }
            KrakenResponse::Rejected { kind, evidence } => {
                let digest = parameters_digest(&request.parameters)?;
                validate_rejection(&evidence, request.endpoint, &digest)?;
                if evidence["safeKind"] != format!("{kind:?}") {
                    return Err(error(
                        "KRAKEN_REJECTION_PROOF_INVALID",
                        "Transport rejection classification differs from its sealed evidence.",
                    ));
                }
                journal.persist_dispatch_evidence(&request.submission,&json!({"endpoint":request.endpoint.method(),"sealedRejection":evidence,"submission":request.submission,"requestParametersSha256":digest}))?;
                if evidence["receiptSource"] == "local_signing_failure" {
                    return Err(error(
                        "KRAKEN_ACTION_NOT_DISPATCHED",
                        "Local Kraken signing or nonce preparation failed before HTTP dispatch. No financial request was sent; inspect protected credentials and nonce storage before a new attempt.",
                    ));
                }
                if evidence["receiptSource"] == "local_dispatch_guard" {
                    return Err(error(
                        "KRAKEN_ACTION_NOT_DISPATCHED",
                        "Authorization was no longer valid before Kraken dispatch. No financial request was sent; inspect the settled original attempt and obtain fresh approval for a new request.",
                    ));
                }
                Err(error(
                    "KRAKEN_ACTION_REJECTED",
                    &format!(
                        "Kraken rejected this exact request without a mutation ({kind:?}); its durable receipt permits failure settlement."
                    ),
                ))
            }
            KrakenResponse::Artifact { .. } => Err(error(
                "KRAKEN_INVALID_RESPONSE",
                "A financial write cannot return an unbound artifact.",
            )),
        }
    }
    fn validate_response(
        &self,
        _submission: &gofin_domain::Submission,
        response: &Value,
    ) -> Result<()> {
        if response.get("error").is_some_and(|v| {
            v.as_array().is_some_and(|a| !a.is_empty()) || v.as_str().is_some_and(|s| !s.is_empty())
        }) || response["result"] == "error"
        {
            return Err(error(
                "KRAKEN_ACTION_REJECTED",
                "Kraken reported a business failure; reconcile the original persisted identity before another action.",
            ));
        }
        Ok(())
    }
    async fn reconcile_with_reference(
        &self,
        record: &gofin_domain::OperationRecord,
        reference: &gofin_domain::operation_recovery::RecoveryReference,
    ) -> Result<gofin_domain::Reconciliation> {
        let submission = record.submission.as_ref().ok_or_else(|| {
            error(
                "RECEIPT_RECOVERY_UNSUPPORTED",
                "No immutable submission exists for reference recovery.",
            )
        })?;
        self.reconcile_funding_reference(record, submission, reference)
            .await
    }
    async fn reconcile_submission(
        &self,
        record: &gofin_domain::OperationRecord,
        submission: &gofin_domain::Submission,
    ) -> Result<gofin_domain::Reconciliation> {
        if let Some(rejected) = self.reconcile_rejected(record, submission).await? {
            return Ok(rejected);
        }
        let endpoint = contracts::endpoint(&record.operation)?;
        if crate::trading::is_trading(endpoint) {
            return self.reconcile_trading(record, submission).await;
        }
        if crate::funding::is_funding(endpoint) {
            return self.reconcile_funding(record, submission).await;
        }
        Ok(crate::trading::unresolved(
            gofin_domain::Status::Unknown,
            "This original operation's settlement evidence is not implemented.",
            json!({}),
        ))
    }
    async fn reconcile_submission_with_context(
        &self,
        record: &gofin_domain::OperationRecord,
        submission: &gofin_domain::Submission,
        context: &gofin_domain::amendments::AmendmentContext,
    ) -> Result<gofin_domain::Reconciliation> {
        if let Some(rejected) = self.reconcile_rejected(record, submission).await? {
            return Ok(rejected);
        }
        if let Some(scope) = context.effective_scope() {
            let mut binding = scope.binding.clone();
            attach_successor_lineage(&mut binding, context)?;
            apply_accounting_envelope(&mut binding, context)?;
            return self
                .reconcile_trading_binding(record, submission, &binding)
                .await;
        }
        self.reconcile_submission(record, submission).await
    }
    async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
        let endpoint = contracts::endpoint(operation)?;
        let definition = contracts::contract(endpoint);
        if definition.client != self.product.client()
            || definition.mutating
            || definition.administrative
        {
            return Err(error(
                "WRONG_OPERATION_KIND",
                "This is not a read for the selected Kraken product.",
            ));
        }
        let (selection, parameters) = contracts::decode(endpoint, input)?;
        let account = selection
            .account
            .as_deref()
            .map(|n| self.account(n))
            .transpose()?;
        let observed = gofin_domain::now();
        let mut data = self.json(endpoint, account, parameters).await?;
        redact_credentials(&mut data);
        Ok(
            json!({"provider":self.product.client(),"operation":operation,"observedAt":observed,"data":data,"coverage":"Venue-specific bounded response; no aggregate wealth or exhaustive history claim."}),
        )
    }
}
pub(crate) fn redact_credentials(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.retain(|k, _| {
                !matches!(
                    k.to_ascii_lowercase().replace(['-', '_'], "").as_str(),
                    "apikey"
                        | "secret"
                        | "apisecret"
                        | "privatekey"
                        | "otp"
                        | "apisign"
                        | "authent"
                        | "password"
                        | "loginpassword"
                        | "cvc"
                        | "credential"
                        | "credentials"
                        | "passphrase"
                        | "authorization"
                        | "accesstoken"
                        | "authtoken"
                        | "sessiontoken"
                        | "refreshtoken"
                        | "signature"
                )
            });
            for v in fields.values_mut() {
                redact_credentials(v);
            }
        }
        Value::Array(values) => {
            for v in values {
                redact_credentials(v);
            }
        }
        _ => (),
    }
}

fn attach_successor_lineage(
    binding: &mut Value,
    context: &gofin_domain::amendments::AmendmentContext,
) -> Result<()> {
    if binding["mixedSpotMargin"].is_object() {
        for family in ["cash", "margin"] {
            let projected = crate::mixed_batch_edit::project_family_context(context, family)?;
            attach_successor_lineage(&mut binding["mixedSpotMargin"][family], &projected)?;
        }
        return Ok(());
    }
    for revision in &context.revisions {
        let Some(successor) = &revision.successor else {
            continue;
        };
        let margin = binding["margin"].is_object();
        let orders = if margin {
            binding["margin"]["orders"].as_array_mut()
        } else {
            binding["orders"].as_array_mut()
        }
        .ok_or_else(|| {
            error(
                "KRAKEN_AMENDMENT_PROOF_PENDING",
                "Successor lineage requires exact original order scopes.",
            )
        })?;
        let index = successor.data["batchOrderIndex"]
            .as_u64()
            .map(|n| n as usize)
            .or_else(|| (orders.len() == 1).then_some(0))
            .ok_or_else(|| {
                error(
                    "KRAKEN_AMENDMENT_PROOF_PENDING",
                    "A batched legacy successor requires its frozen original member index.",
                )
            })?;
        let approved = if margin {
            &revision.scope.binding["margin"]["orders"][index]
        } else {
            &revision.scope.binding["orders"][index]
        };
        let order = orders.get_mut(index).ok_or_else(|| {
            error(
                "KRAKEN_SUBMISSION_INVALID",
                "Legacy successor member is outside its original batch.",
            )
        })?;
        if order["clientId"] != approved["clientId"]
            || order["pair"] != approved["pair"]
            || order["buy"] != approved["buy"]
        {
            return Err(error(
                "KRAKEN_AMENDMENT_ECONOMIC_SHAPE_CHANGED",
                "Legacy successor changed its frozen original member identity.",
            ));
        }
        let mut ids = order["orderIds"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<std::collections::BTreeSet<_>>()
            })
            .unwrap_or_default();
        for field in ["predecessorOrderId", "venueOrderId"] {
            ids.insert(
                successor.data[field]
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| {
                        error(
                            "KRAKEN_SUBMISSION_INVALID",
                            "Exact legacy native predecessor/successor identity missing.",
                        )
                    })?
                    .to_owned(),
            );
        }
        order["orderIds"] = json!(ids.into_iter().collect::<Vec<_>>());
        let predecessor = successor.data["predecessorOrderId"].as_str().unwrap();
        let native = successor.data["venueOrderId"].as_str().unwrap();
        if binding["conditionalChildRevisions"][predecessor].is_object() {
            let mut override_scope = binding["conditionalChildRevisions"][predecessor].clone();
            override_scope["predecessorOrderId"] = json!(predecessor);
            if override_scope["legacyRevision"] == true {
                override_scope["parameters"]["volume"] =
                    override_scope["nativeSuccessorQuantity"].clone();
                if override_scope.get("approvedQuantity").is_some() {
                    override_scope["approvedQuantity"] =
                        json!(gofin_domain::Quantity::from_decimal(
                            override_scope["nativeSuccessorQuantity"]
                                .as_str()
                                .ok_or_else(|| error(
                                    "KRAKEN_SUBMISSION_INVALID",
                                    "Legacy child successor quantity missing."
                                ))?,
                            36
                        )?);
                }
                binding["conditionalChildRevisions"][predecessor]["parameters"] =
                    override_scope["predecessorParameters"].clone();
                binding["conditionalChildRevisions"][predecessor]["retiredBySuccessor"] =
                    json!(native);
            }
            binding["conditionalChildRevisions"][native] = override_scope;
        }
    }
    Ok(())
}

fn preserve_nested_baseline_times(current: &mut Value, approved: &Value) {
    if current.get("principal").is_some()
        && current.get("position_kind").is_some()
        && current.get("evidence_version").is_some()
    {
        let mut observed = current.clone();
        let mut prior = approved.clone();
        if let (Some(observed), Some(prior)) = (observed.as_object_mut(), prior.as_object_mut()) {
            observed.remove("observed_at");
            prior.remove("observed_at");
        }
        if observed == prior
            && let Some(time) = approved.get("observed_at")
        {
            current["observed_at"] = time.clone();
        }
    }
    match (current, approved) {
        (Value::Object(now), Value::Object(old)) => {
            for (key, value) in now {
                if let Some(prior) = old.get(key) {
                    preserve_nested_baseline_times(value, prior);
                }
            }
        }
        (Value::Array(now), Value::Array(old)) => {
            for (value, prior) in now.iter_mut().zip(old) {
                preserve_nested_baseline_times(value, prior);
            }
        }
        _ => (),
    }
}

fn apply_accounting_envelope(
    binding: &mut Value,
    context: &gofin_domain::amendments::AmendmentContext,
) -> Result<()> {
    // This shadow binding is derived only from protected approved scopes. It
    // keeps prior cost ceilings for late fills without rewriting any approval.
    let mut costs: Vec<gofin_domain::CostBound> = context
        .original
        .as_ref()
        .map(|p| p.intent.maximum_costs.clone())
        .unwrap_or_default();
    for scope in context.revisions.iter().map(|r| &r.scope) {
        for cost in &scope.intent.maximum_costs {
            if let Some(old) = costs.iter_mut().find(|old| {
                old.location == cost.location
                    && old.amount.asset == cost.amount.asset
                    && old.treatment == cost.treatment
            }) {
                old.amount.atoms = old.amount.atoms.clone().max(cost.amount.atoms.clone());
            } else {
                costs.push(cost.clone());
            }
        }
    }
    binding["_finAccountingCostBounds"] = json!(costs);
    if binding["mixedSpotMargin"].is_object() {
        for family in ["cash", "margin"] {
            let projected = crate::mixed_batch_edit::project_family_context(context, family)?;
            apply_accounting_envelope(&mut binding["mixedSpotMargin"][family], &projected)?;
        }
        return Ok(());
    }
    if binding["orderMinimumReceive"].is_array() {
        binding["completionOrderMinimumReceive"] = binding["orderMinimumReceive"].clone();
    }
    if binding["controls"]["minimumReceive"].is_string() {
        binding["completionMinimumReceive"] = binding["controls"]["minimumReceive"].clone();
    }
    let original = context.original.as_ref().map(|p| &p.binding);
    for earlier in original
        .into_iter()
        .chain(context.revisions.iter().map(|r| &r.scope.binding))
    {
        if let (Some(current), Some(previous)) = (
            binding.get_mut("orders").and_then(Value::as_array_mut),
            earlier["orders"].as_array(),
        ) {
            for order in current {
                let client = order["clientId"].clone();
                if let Some(old) = previous.iter().find(|o| o["clientId"] == client) {
                    if order["pair"] != old["pair"]
                        || order["buy"] != old["buy"]
                        || order["base"] != old["base"]
                        || order["quote"] != old["quote"]
                    {
                        return Err(error(
                            "KRAKEN_AMENDMENT_ECONOMIC_SHAPE_CHANGED",
                            "Accounting revisions changed an original exact market or asset identity.",
                        ));
                    }
                    for key in ["volume", "maximumSpend"] {
                        let current: gofin_domain::Quantity =
                            serde_json::from_value(order[key].clone()).map_err(|_| {
                                error(
                                    "KRAKEN_SUBMISSION_INVALID",
                                    "Current order accounting bound invalid.",
                                )
                            })?;
                        let prior: gofin_domain::Quantity =
                            serde_json::from_value(old[key].clone()).map_err(|_| {
                                error(
                                    "KRAKEN_SUBMISSION_INVALID",
                                    "Prior order accounting bound invalid.",
                                )
                            })?;
                        if prior > current {
                            order[key] = json!(prior);
                        }
                    }
                }
            }
            if let (Some(current), Some(prior)) = (
                binding["orderMinimumReceive"].as_array(),
                earlier["orderMinimumReceive"].as_array(),
            ) {
                if current.len() != prior.len() {
                    return Err(error(
                        "KRAKEN_AMENDMENT_ECONOMIC_SHAPE_CHANGED",
                        "Per-member receipt scopes changed original batch length.",
                    ));
                }
                let floors = current
                    .iter()
                    .zip(prior)
                    .map(|(a, b)| -> Result<Value> {
                        let a = gofin_domain::Quantity::from_decimal(
                            a.as_str().ok_or_else(|| {
                                error("KRAKEN_SUBMISSION_INVALID", "Member receipt floor invalid.")
                            })?,
                            36,
                        )?;
                        let b = gofin_domain::Quantity::from_decimal(
                            b.as_str().ok_or_else(|| {
                                error(
                                    "KRAKEN_SUBMISSION_INVALID",
                                    "Prior member receipt floor invalid.",
                                )
                            })?,
                            36,
                        )?;
                        Ok(json!(a.min(b).to_decimal(36)))
                    })
                    .collect::<Result<Vec<_>>>()?;
                binding["orderMinimumReceive"] = json!(floors);
            }
            let current = binding["controls"]["minimumReceive"]
                .as_str()
                .map(|v| gofin_domain::Quantity::from_decimal(v, 36))
                .transpose()?
                .unwrap_or_default();
            let prior = earlier["controls"]["minimumReceive"]
                .as_str()
                .map(|v| gofin_domain::Quantity::from_decimal(v, 36))
                .transpose()?
                .unwrap_or_default();
            if prior < current {
                binding["controls"]["minimumReceive"] = json!(prior.to_decimal(36));
            }
        }
        if let (Some(current), Some(previous)) = (
            binding
                .get_mut("margin")
                .and_then(|m| m.get_mut("orders"))
                .and_then(Value::as_array_mut),
            earlier["margin"]["orders"].as_array(),
        ) {
            for order in current {
                if let Some(old) = previous.iter().find(|o| o["clientId"] == order["clientId"]) {
                    for key in [
                        "pair",
                        "base",
                        "quote",
                        "buy",
                        "leverage",
                        "closing",
                        "settling",
                        "prior",
                        "priorAnchor",
                        "priorDebtAnchor",
                    ] {
                        if order[key] != old[key] {
                            return Err(error(
                                "KRAKEN_AMENDMENT_ECONOMIC_SHAPE_CHANGED",
                                "Margin revision changed original market, direction or loan baseline.",
                            ));
                        }
                    }
                    for key in ["volume", "maximumLoan"] {
                        let now: gofin_domain::Quantity =
                            serde_json::from_value(order[key].clone()).map_err(|_| {
                                error(
                                    "KRAKEN_SUBMISSION_INVALID",
                                    "Current margin accounting bound invalid.",
                                )
                            })?;
                        let prior: gofin_domain::Quantity =
                            serde_json::from_value(old[key].clone()).map_err(|_| {
                                error(
                                    "KRAKEN_SUBMISSION_INVALID",
                                    "Prior margin accounting bound invalid.",
                                )
                            })?;
                        if prior > now {
                            order[key] = json!(prior);
                        }
                    }
                }
            }
        }
        if let (Some(current), Some(prior)) = (
            binding
                .get_mut("derivativeBatch")
                .and_then(Value::as_array_mut),
            earlier["derivativeBatch"].as_array(),
        ) {
            for child in current {
                if !child["derivative"].is_object() {
                    continue;
                }
                if let Some(old) = prior
                    .iter()
                    .find(|old| old["derivative"]["clientId"] == child["derivative"]["clientId"])
                {
                    let size: gofin_domain::Quantity = serde_json::from_value(
                        child["derivative"]["size"].clone(),
                    )
                    .map_err(|_| error("KRAKEN_SUBMISSION_INVALID", "Member size invalid."))?;
                    let previous: gofin_domain::Quantity =
                        serde_json::from_value(old["derivative"]["size"].clone()).map_err(
                            |_| error("KRAKEN_SUBMISSION_INVALID", "Prior member size invalid."),
                        )?;
                    if child["derivative"]["symbol"] != old["derivative"]["symbol"]
                        || child["parameters"]["side"] != old["parameters"]["side"]
                        || child["derivative"]["collateral"] != old["derivative"]["collateral"]
                    {
                        return Err(error(
                            "KRAKEN_AMENDMENT_ECONOMIC_SHAPE_CHANGED",
                            "Derivative member identity changed.",
                        ));
                    }
                    child["derivative"]["size"] = json!(size.max(previous));
                }
            }
        }
        if binding["derivative"].is_object() && earlier["derivative"].is_object() {
            if binding["derivative"]["clientId"] != earlier["derivative"]["clientId"]
                || binding["derivative"]["symbol"] != earlier["derivative"]["symbol"]
            {
                return Err(error(
                    "KRAKEN_AMENDMENT_ECONOMIC_SHAPE_CHANGED",
                    "Derivative accounting revision changed original exact order identity.",
                ));
            }
            let current: gofin_domain::Quantity =
                serde_json::from_value(binding["derivative"]["size"].clone())
                    .map_err(|_| error("KRAKEN_SUBMISSION_INVALID", "Derivative size invalid."))?;
            let prior: gofin_domain::Quantity =
                serde_json::from_value(earlier["derivative"]["size"].clone()).map_err(|_| {
                    error(
                        "KRAKEN_SUBMISSION_INVALID",
                        "Earlier derivative size invalid.",
                    )
                })?;
            if prior > current {
                binding["derivative"]["size"] = json!(prior);
            }
        }
    }
    Ok(())
}

// Admission observations are checked afresh before this normalization. They do
// not alter the approved native order, custody amounts or exposure ceilings.
fn preserve_admission_estimates(current: &mut Value, approved: &Value) {
    for key in [
        "cashValuation",
        "marginValuation",
        "valuation",
        "incremental_valuation",
    ] {
        if let (Some(now), Some(old)) = (current.get(key), approved.get(key))
            && let (Ok(now), Ok(old)) = (
                serde_json::from_value::<Option<gofin_domain::Valuation>>(now.clone()),
                serde_json::from_value::<Option<gofin_domain::Valuation>>(old.clone()),
            )
        {
            let mut now = now;
            preserve_valuation_observation_times(&mut now, &old);
            if let Ok(value) = serde_json::to_value(now) {
                current[key] = value;
            }
        }
    }
    if let Some(prior) = approved.get("marginFeeAdmission")
        && current.get("marginFeeAdmission").is_some()
    {
        current["marginFeeAdmission"] = prior.clone();
    }
    if let (Some(now), Some(old)) = (
        current.get_mut("derivative").and_then(Value::as_object_mut),
        approved.get("derivative").and_then(Value::as_object),
    ) {
        for key in [
            "minimumRequiredFeeAtoms",
            "observedFeePrice",
            "initialMarginRate",
            "initialMargin",
        ] {
            if let Some(value) = old.get(key) {
                now.insert(key.into(), value.clone());
            }
        }
    }
    if let (Some(now), Some(old)) = (
        current.get_mut("orders").and_then(Value::as_array_mut),
        approved.get("orders").and_then(Value::as_array),
    ) {
        for (order, prior) in now.iter_mut().zip(old) {
            if let Some(value) = prior.get("quotedFeeNotional") {
                order["quotedFeeNotional"] = value.clone();
            }
        }
    }
    match (current, approved) {
        (Value::Object(now), Value::Object(old)) => {
            for (key, value) in now {
                if let Some(prior) = old.get(key) {
                    preserve_admission_estimates(value, prior);
                }
            }
        }
        (Value::Array(now), Value::Array(old)) => {
            for (value, prior) in now.iter_mut().zip(old) {
                preserve_admission_estimates(value, prior);
            }
        }
        _ => (),
    }
}

fn preserve_valuation_observation_times(
    current: &mut Option<gofin_domain::Valuation>,
    approved: &Option<gofin_domain::Valuation>,
) {
    let (Some(current), Some(approved)) = (current, approved) else {
        return;
    };
    let mut economics = current.clone();
    economics.observed_at = approved.observed_at.clone();
    for (price, prior) in economics.prices.iter_mut().zip(&approved.prices) {
        price.observed_at = prior.observed_at.clone();
    }
    if economics == *approved {
        current.observed_at = approved.observed_at.clone();
        for (price, prior) in current.prices.iter_mut().zip(&approved.prices) {
            price.observed_at = prior.observed_at.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transport_diagnostics_identify_failed_read_without_changing_retry_classification() {
        use gofin_exchange::ccxt::{CcxtError, CcxtErrorKind, FundingErrorCode};
        let endpoint = KrakenEndpoint::KrakenFundingGetFundingV2DepositAddresses;
        let mut native = CcxtError::new(CcxtErrorKind::Rejected);
        native.http_status = Some(400);
        native.funding_error_code = Some(FundingErrorCode::InvalidArgument);
        let error = transport_error(endpoint, native);
        assert_eq!(error.code, "KRAKEN_UNAVAILABLE");
        assert_eq!(
            error.details.as_ref().unwrap()["operation"],
            endpoint.operation()
        );
        assert_eq!(error.details.as_ref().unwrap()["http_status"], 400);
        assert_eq!(
            error.details.as_ref().unwrap()["funding_error_code"],
            "invalid_argument"
        );
        assert_eq!(
            error.details.as_ref().unwrap()["transport_kind"],
            "Rejected"
        );
        let unavailable = transport_error(endpoint, CcxtError::new(CcxtErrorKind::Unavailable));
        assert_eq!(unavailable.code, error.code);
        assert_eq!(
            unavailable.details.as_ref().unwrap()["transport_kind"],
            "Unavailable"
        );
        assert!(unavailable.details.as_ref().unwrap()["http_status"].is_null());
        assert!(unavailable.details.as_ref().unwrap()["funding_error_code"].is_null());
    }
    #[tokio::test]
    async fn legacy_deposit_address_witness_cannot_cross_default_account_scope() {
        for (selected, default, verified, allowed) in [
            (None, None, true, true),
            (None, None, false, false),
            (Some("secondary"), None, true, false),
            (Some("secondary"), Some("default"), true, false),
        ] {
            let account = KrakenAccount {
                name: "scope-fixture".into(),
                resource: gofin_domain::ResourceId("kraken:scope-fixture".into()),
                credential_ref: "synthetic-only".into(),
                physical_identity: "synthetic-uid".into(),
                wallet_account_id: selected.map(str::to_owned),
                default_wallet_account_id: default.map(str::to_owned),
                identity_verified: verified,
                product: KrakenProduct::Spot,
            };
            let profile =
                KrakenProfile::new(KrakenProduct::Spot, vec![account], Arc::new(Fixture)).unwrap();
            let result = profile
                .validate_legacy_deposit_default_scope("scope-fixture")
                .await;
            assert_eq!(result.is_ok(), allowed);
            if !allowed {
                assert_eq!(
                    result.unwrap_err().code,
                    "KRAKEN_DEPOSIT_LEGACY_SCOPE_UNVERIFIED"
                );
            }
        }
    }
    #[tokio::test]
    async fn legacy_deposit_default_alias_requires_current_authenticated_relationship() {
        struct Wallets(Value);
        #[async_trait]
        impl KrakenTransport for Wallets {
            async fn request(
                &self,
                endpoint: KrakenEndpoint,
                _: Option<&KrakenAccount>,
                _: Value,
                guard: Option<DispatchGuard>,
            ) -> Result<KrakenResponse> {
                assert_eq!(
                    endpoint,
                    KrakenEndpoint::KrakenPrivatePostListWalletAccounts
                );
                assert!(guard.is_none());
                Ok(KrakenResponse::Json(self.0.clone()))
            }
        }
        for (response, expected) in [
            (json!({"result":{}}), Some("KRAKEN_WALLET_IDENTITY_INVALID")),
            (
                json!({"result":{"accounts":[]}}),
                Some("KRAKEN_DEFAULT_WALLET_RELATIONSHIP_UNVERIFIED"),
            ),
            (
                json!({"result":{"accounts":[{"account_id":"default","status":"inactive","flags":{"active":false},"type":"spot"}]}}),
                Some("KRAKEN_DEFAULT_WALLET_RELATIONSHIP_UNVERIFIED"),
            ),
            (
                json!({"result":{"accounts":[{"account_id":"default","status":"active","flags":{"active":true},"type":"spot"}]}}),
                None,
            ),
        ] {
            let account = KrakenAccount {
                name: "scope-fixture".into(),
                resource: gofin_domain::ResourceId("kraken:scope-fixture".into()),
                credential_ref: "synthetic-only".into(),
                physical_identity: "synthetic-uid".into(),
                wallet_account_id: Some("default".into()),
                default_wallet_account_id: Some("default".into()),
                identity_verified: true,
                product: KrakenProduct::Spot,
            };
            let profile = KrakenProfile::new(
                KrakenProduct::Spot,
                vec![account],
                Arc::new(Wallets(response)),
            )
            .unwrap();
            let result = profile
                .validate_legacy_deposit_default_scope("scope-fixture")
                .await;
            assert_eq!(result.err().as_ref().map(|e| e.code.as_str()), expected);
        }
    }
    struct Fixture;
    #[async_trait]
    impl KrakenTransport for Fixture {
        async fn request(
            &self,
            endpoint: KrakenEndpoint,
            _: Option<&KrakenAccount>,
            _: Value,
            guard: Option<DispatchGuard>,
        ) -> Result<KrakenResponse> {
            assert!(!endpoint.mutating());
            assert!(guard.is_none());
            Ok(KrakenResponse::Json(
                json!({"result":{"amount": "0.123456789012345678901234567890","api_key":"synthetic-only","nested":{"access_token":"synthetic-only","token":"quote-route-token"}}}),
            ))
        }
    }
    #[tokio::test]
    async fn public_read_preserves_quantities_and_redacts_credentials() {
        let profile = KrakenProfile::new(KrakenProduct::Spot, vec![], Arc::new(Fixture)).unwrap();
        let output = profile
            .read(KrakenEndpoint::KrakenPublicGetTime.operation(), &json!({}))
            .await
            .unwrap();
        assert_eq!(
            output["data"]["result"]["amount"],
            "0.123456789012345678901234567890"
        );
        assert!(output["data"]["result"].get("api_key").is_none());
        assert!(
            output["data"]["result"]["nested"]
                .get("access_token")
                .is_none()
        );
        assert_eq!(
            output["data"]["result"]["nested"]["token"],
            "quote-route-token"
        );
    }
    #[tokio::test]
    async fn read_never_dispatches_financial_or_administrative_actions() {
        let profile = KrakenProfile::new(KrakenProduct::Spot, vec![], Arc::new(Fixture)).unwrap();
        assert!(
            profile
                .read(
                    KrakenEndpoint::KrakenPrivatePostAddOrder.operation(),
                    &json!({})
                )
                .await
                .is_err()
        );
        assert!(
            profile
                .read(
                    KrakenEndpoint::KrakenPrivatePostCreateSubaccount.operation(),
                    &json!({})
                )
                .await
                .is_err()
        );
    }
}
