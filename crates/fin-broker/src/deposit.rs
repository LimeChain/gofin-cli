//! Protected composite for an exact native ETH transfer into an enrolled
//! Binance SPOT account. It exposes no arbitrary endpoint or recipient.

use async_trait::async_trait;
use fin_binance::BinanceAdapter;
use fin_domain::{
    Adapter, Amount, AssetId, Check, CheckStatus, ConnectionMetadata, Definition, EffectConstraint,
    ExecutionActor, FinError, FinancialEffect, FinancialIntent, FinancialState, OperationKind,
    OperationRecord, PreparedOperation, Quantity, Reconciliation, ResourceAmount, ResourceId,
    ReturnRouteContext, ReturnRouteProof, StateSnapshot, Status, Submission, binding_hash, now,
    verified_effect_digest,
};
use fin_ethereum::{BrokerVerifiedNativeDepositTarget, EthereumAdapter};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

fn error(code: &str, stage: &str, message: &str, next: &str) -> FinError {
    FinError::new(code, stage, message, next)
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct DepositInput {
    request_id: String,
    account: String,
    wallet: String,
    amount: String,
    max_fee_eth: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DepositBinding {
    target: BrokerVerifiedNativeDepositTarget,
    inner_input: Value,
    inner_preview: PreparedOperation,
    source_free_before: String,
}

/// Broker-only composition over the existing fixed Binance and Ethereum
/// adapters. Construct this only after both adapters have protected signers.
pub struct BinanceEthereumDepositAdapter {
    binance: Arc<BinanceAdapter>,
    ethereum: Arc<EthereumAdapter>,
}

impl BinanceEthereumDepositAdapter {
    pub fn new(binance: Arc<BinanceAdapter>, ethereum: Arc<EthereumAdapter>) -> Self {
        Self { binance, ethereum }
    }

    fn input(raw: &Value) -> Result<DepositInput, FinError> {
        serde_json::from_value(raw.clone()).map_err(|_| {
            error(
                "INVALID_INPUT",
                "validation",
                "Native Binance deposit input is malformed or contains unknown fields.",
                "Use the fixed binance.deposit schema.",
            )
        })
    }

    fn account(&self, name: &str) -> Result<ConnectionMetadata, FinError> {
        self.binance
            .connections()
            .into_iter()
            .find(|connection| {
                connection.name == name && connection.role.as_deref() == Some("main")
            })
            .ok_or_else(|| {
                error(
                    "BINANCE_MAIN_REQUIRED",
                    "validation",
                    "Native deposit credit must target an enrolled Binance main SPOT account.",
                    "Select a configured main account.",
                )
            })
    }

    fn wallet(&self, name: &str) -> Result<ConnectionMetadata, FinError> {
        self.ethereum
            .connections()
            .into_iter()
            .find(|connection| {
                connection.name == name && connection.network.as_deref() == Some("ethereum:1")
            })
            .ok_or_else(|| {
                error(
                    "ETHEREUM_WALLET_UNKNOWN",
                    "validation",
                    "Selected wallet is not enrolled on Ethereum mainnet.",
                    "Select a configured Ethereum mainnet wallet.",
                )
            })
    }

    async fn target(
        &self,
        account_name: &str,
    ) -> Result<BrokerVerifiedNativeDepositTarget, FinError> {
        let account = self.account(account_name)?;
        let networks = self
            .binance
            .read(
                "binance.networks",
                &json!({"account":account_name,"coin":"ETH"}),
            )
            .await?;
        let eth_asset = networks["assets"]
            .as_array()
            .and_then(|assets| {
                let matches = assets
                    .iter()
                    .filter(|asset| asset["coin"] == "ETH")
                    .collect::<Vec<_>>();
                (matches.len() == 1).then(|| matches[0])
            })
            .ok_or_else(|| {
                error(
                    "DEPOSIT_NETWORK_UNAVAILABLE",
                    "preview",
                    "Binance did not return one unambiguous ETH network record.",
                    "Retry after reviewing the venue's current funding network.",
                )
            })?;
        let network = eth_asset["networks"]
            .as_array()
            .and_then(|networks| {
                let matches = networks
                    .iter()
                    .filter(|network| network["network"] == "ETH")
                    .collect::<Vec<_>>();
                (matches.len() == 1).then(|| matches[0])
            })
            .ok_or_else(|| {
                error(
                    "DEPOSIT_NETWORK_UNAVAILABLE",
                    "preview",
                    "Binance did not return one unambiguous Ethereum network record.",
                    "Select the current ETH network returned by Binance.",
                )
            })?;
        if network["depositEnable"] != true || network["coin"] != "ETH" {
            return Err(error(
                "DEPOSIT_DISABLED",
                "preview",
                "Binance ETH network does not currently accept deposits.",
                "Wait until Binance enables this exact network.",
            ));
        }
        // Binance's documented depositDust field is the minimum creditable
        // amount. Missing values fail closed; it is never inferred locally.
        let minimum_decimal = network["depositDust"].as_str().ok_or_else(|| {
            error(
                "DEPOSIT_MINIMUM_UNKNOWN",
                "preview",
                "Binance did not provide its current minimum creditable deposit amount.",
                "Do not send funds until Binance provides an exact minimum.",
            )
        })?;
        let minimum_deposit = Quantity::from_decimal(minimum_decimal, 36).map_err(|_| {
            error(
                "DEPOSIT_MINIMUM_INVALID",
                "preview",
                "Binance's deposit minimum is not an exact supported decimal.",
                "Review the venue response before funding.",
            )
        })?;
        let address_result = self
            .binance
            .read(
                "binance.deposit-address",
                &json!({"account":account_name,"coin":"ETH","network":"ETH"}),
            )
            .await?;
        let address = address_result["address"].as_str().ok_or_else(|| {
            error(
                "DEPOSIT_ADDRESS_INVALID",
                "preview",
                "Binance did not return an exact deposit address.",
                "Refresh the address through the protected venue adapter.",
            )
        })?;
        if address_result["coin"] != "ETH"
            || address_result["network"] != "ETH"
            || address_result["memo"]
                .as_str()
                .is_none_or(|memo| !memo.is_empty())
            || address.len() != 42
            || !address.starts_with("0x")
            || !address[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(error(
                "DEPOSIT_ADDRESS_UNSUPPORTED",
                "preview",
                "Binance deposit target is not a plain Ethereum ETH address without a memo.",
                "Do not send funds to a tagged or non-Ethereum target.",
            ));
        }
        let target_evidence = json!({
            "accountResource":account.resource,
            "coin":"ETH",
            "network":"ETH",
            "address":address.to_ascii_lowercase(),
            "memo":"",
            "minimumDeposit":minimum_deposit,
            "minimumConfirmations":address_result["minimumConfirmations"],
            "unlockConfirmations":address_result["unlockConfirmations"],
            "depositEnabled":true,
            "minimumSource":"Binance capital config depositDust (minimum creditable amount)"
        });
        let evidence_digest = binding_hash(&target_evidence).map_err(|_| {
            error(
                "DEPOSIT_TARGET_INVALID",
                "preview",
                "Binance deposit evidence could not be canonically bound.",
                "Retry the protected funding read.",
            )
        })?;
        let observed_at = now();
        let expires_at = chrono::DateTime::parse_from_rfc3339(&observed_at)
            .map_err(|_| {
                error(
                    "CLOCK_INVALID",
                    "preview",
                    "System UTC is invalid.",
                    "Retry.",
                )
            })?
            .with_timezone(&chrono::Utc)
            .checked_add_signed(chrono::Duration::seconds(90))
            .ok_or_else(|| {
                error(
                    "CLOCK_INVALID",
                    "preview",
                    "Deposit target expiry overflowed.",
                    "Retry.",
                )
            })?
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        Ok(BrokerVerifiedNativeDepositTarget {
            chain_id: 1,
            account_resource: account.resource,
            address: address.into(),
            asset: AssetId {
                network: "binance".into(),
                reference: "ETH".into(),
                decimals: 36,
            },
            minimum_deposit,
            network: "ETH".into(),
            observed_at,
            expires_at,
            evidence_digest,
        })
    }

    fn inner_input(input: &DepositInput, target: &BrokerVerifiedNativeDepositTarget) -> Value {
        json!({
            "wallet":input.wallet,
            "network":"ethereum",
            "chain_id":1,
            "request_id":input.request_id,
            "max_fee_eth":input.max_fee_eth,
            "token":"native",
            "amount":input.amount,
            "recipient":target.address
        })
    }

    async fn source_free(&self, account: &str) -> Result<String, FinError> {
        let balances = self
            .binance
            .read("binance.balances", &json!({"account":account}))
            .await?;
        let rows = balances["balances"].as_array().ok_or_else(|| {
            error(
                "BALANCE_OBSERVATION_INVALID",
                "preview",
                "Binance did not return its exact SPOT balance set.",
                "Refresh the protected account balance.",
            )
        })?;
        let eth = rows
            .iter()
            .filter(|row| row["asset"] == "ETH")
            .collect::<Vec<_>>();
        if eth.len() != 1 {
            return Err(error(
                "BALANCE_OBSERVATION_INVALID",
                "preview",
                "Binance ETH balance is missing or ambiguous.",
                "Refresh the protected account balance.",
            ));
        }
        let free = eth[0]["free"].as_str().ok_or_else(|| {
            error(
                "BALANCE_OBSERVATION_INVALID",
                "preview",
                "Binance ETH free balance is not an exact decimal string.",
                "Do not continue with rounded balances.",
            )
        })?;
        Quantity::from_decimal(free, 36).map_err(|_| {
            error(
                "BALANCE_OBSERVATION_INVALID",
                "preview",
                "Binance ETH free balance exceeds supported exact precision.",
                "Do not continue with rounded balances.",
            )
        })?;
        Ok(free.into())
    }

    fn cross_custody_intent(
        inner: &PreparedOperation,
        target: &BrokerVerifiedNativeDepositTarget,
        amount: &str,
    ) -> Result<FinancialIntent, FinError> {
        let source = inner
            .intent
            .debits
            .first()
            .map(|debit| debit.resource.clone())
            .ok_or_else(|| {
                error(
                    "INNER_PREVIEW_INVALID",
                    "preview",
                    "Ethereum transfer preview omitted its source resource.",
                    "Refresh the Ethereum wallet preview.",
                )
            })?;
        let debit_atoms = Quantity::from_decimal(amount, 18).map_err(|_| {
            error(
                "INVALID_AMOUNT",
                "preview",
                "Native amount exceeds the exact Ethereum precision.",
                "Use at most 18 decimal places.",
            )
        })?;
        let credit_atoms = Quantity::from_decimal(amount, 36).map_err(|_| {
            error(
                "INVALID_AMOUNT",
                "preview",
                "Venue credit exceeds the exact Binance precision.",
                "Use an exact amount with at most 18 decimals.",
            )
        })?;
        let native = AssetId {
            network: "ethereum:1".into(),
            reference: "native".into(),
            decimals: 18,
        };
        let mut locks = vec![source.clone(), target.account_resource.clone()];
        locks.sort();
        locks.dedup();
        Ok(FinancialIntent {
            debits: vec![ResourceAmount {
                resource: source.clone(),
                amount: Amount {
                    asset: native.clone(),
                    atoms: debit_atoms.clone(),
                },
            }],
            maximum_costs: inner.intent.maximum_costs.clone(),
            expected: vec![EffectConstraint::Transfer {
                from: source,
                to: target.account_resource.clone(),
                debit_asset: native,
                credit_asset: target.asset.clone(),
                maximum_debit: debit_atoms,
                minimum_credit: credit_atoms,
            }],
            locks,
        })
    }

    pub fn operation_definition() -> Definition {
        Definition {
            name: "binance.deposit".into(),
            description: "Send an exact finite native ETH amount from an enrolled Ethereum mainnet wallet to the current fixed Binance ETH deposit address; completion requires canonical chain proof and one usable matching Binance credit.".into(),
            kind: OperationKind::Write,
            execution_actor: ExecutionActor::AgentInvocable,
            input_schema: serde_json::to_value(schema_for!(DepositInput)).unwrap_or(Value::Null),
            output_schema: json!({"type":"object"}),
            error_schema: schema_for!(FinError).into(),
            examples: vec![],
            coverage: "Broker-composite synthetic transport and canonical fake-EVM proof; production access and live funding acceptance are not verified.".into(),
            completion: "Original transaction identity is canonical and finalized, and exactly one current usable ETH deposit row plus matching Binance free-balance delta is observed.".into(),
            semantics: json!({"recipient":"fixed from current protected Binance ETH deposit-address endpoint","amount":"exact finite native ETH; amount=all is unsupported","completion":"chain submission alone is insufficient; deposit history status 1 and usable balance delta are required","venueAssetDecimals":36,"ethereumNativeDecimals":18}),
        }
    }

    async fn prepare_deposit(
        &self,
        operation: &str,
        raw: &Value,
    ) -> Result<PreparedOperation, FinError> {
        let input = Self::input(raw)?;
        let account = self.account(&input.account)?;
        let wallet = self.wallet(&input.wallet)?;
        if !wallet.signing_enabled || input.request_id.is_empty() || input.request_id.len() > 128 {
            return Err(error(
                "DEPOSIT_CONFIGURATION_INVALID",
                "preview",
                "Deposit requires a signing-enabled named wallet and bounded request ID.",
                "Select an enrolled signing wallet and a new request ID.",
            ));
        }
        let target = self.target(&input.account).await?;
        let amount_atoms = Quantity::from_decimal(&input.amount, 18).map_err(|_| {
            error(
                "INVALID_AMOUNT",
                "preview",
                "Deposit amount must be a positive exact decimal with at most 18 places.",
                "Use a finite native ETH amount.",
            )
        })?;
        if amount_atoms.is_zero() {
            return Err(error(
                "INVALID_AMOUNT",
                "preview",
                "Deposit amount must be positive.",
                "Use a finite native ETH amount.",
            ));
        }
        let venue_atoms = Quantity::from_decimal(&input.amount, 36).map_err(|_| {
            error(
                "INVALID_AMOUNT",
                "preview",
                "Deposit amount cannot be represented exactly by Binance ETH precision.",
                "Use at most 18 decimal places.",
            )
        })?;
        if venue_atoms < target.minimum_deposit {
            return Err(error(
                "DEPOSIT_MINIMUM",
                "preview",
                "Selected exact amount is below Binance's observed minimum creditable amount.",
                "Increase the amount or choose another supported route.",
            ));
        }
        let source_free_before = self.source_free(&input.account).await?;
        let inner_input = Self::inner_input(&input, &target);
        let inner_preview = self
            .ethereum
            .prepare("wallet.transfer", &inner_input)
            .await?;
        let intent = Self::cross_custody_intent(&inner_preview, &target, &input.amount)?;
        let binding = DepositBinding {
            target,
            inner_input,
            inner_preview: inner_preview.clone(),
            source_free_before,
        };
        Ok(PreparedOperation {
            operation: operation.into(),
            input: raw.clone(),
            binding: serde_json::to_value(binding).map_err(|_| internal_error())?,
            summary: format!("Send exactly {} native ETH from {} to {}'s freshly observed Binance ETH deposit address. Completion waits for canonical Ethereum finality, a matching Binance usable-credit history row, and the exact Binance free-balance increase. Maximum Ethereum fee {} ETH.", input.amount, input.wallet, account.name, input.max_fee_eth),
            checks: vec![Check { name:"venue_deposit_enabled".into(), status:CheckStatus::Passed, detail:"Current Binance ETH network reports deposit enabled and an exact minimum creditable amount.".into() }, Check { name:"plain_eth_address".into(), status:CheckStatus::Passed, detail:"Current protected Binance address has no memo and is on the fixed ETH network.".into() }, Check { name:"ethereum_native_preview".into(), status:CheckStatus::Passed, detail:"Ordinary Ethereum wallet.transfer froze the exact finite native value, sender, recipient, chain and fee cap.".into() }],
            warnings: vec!["A successful Ethereum transaction does not complete this operation until Binance records one usable credited deposit.".into()],
            observed_at: inner_preview.observed_at.clone(),
            expires_at: inner_preview.expires_at.clone(),
            valuation: inner_preview.valuation.clone(),
            intent,
            operation_version: 1,
        })
    }
}

fn internal_error() -> FinError {
    error(
        "INTERNAL_ERROR",
        "broker",
        "Protected deposit record is invalid.",
        "Inspect the protected broker diagnostics.",
    )
}

#[async_trait]
impl Adapter for BinanceEthereumDepositAdapter {
    fn definitions(&self) -> Vec<Definition> {
        vec![Self::operation_definition()]
    }

    fn connections(&self) -> Vec<ConnectionMetadata> {
        Vec::new()
    }

    async fn snapshot(&self) -> Result<StateSnapshot, FinError> {
        Ok(StateSnapshot {
            state: FinancialState::default(),
            observed_at: now(),
            complete: true,
            coverage: Vec::new(),
            pending: Vec::new(),
        })
    }

    async fn read(&self, _operation: &str, _input: &Value) -> Result<Value, FinError> {
        Err(error(
            "WRONG_OPERATION_KIND",
            "read",
            "Native deposit is a write operation.",
            "Use preview before an authorized execute.",
        ))
    }

    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation, FinError> {
        if operation != "binance.deposit" {
            return Err(error(
                "UNSUPPORTED_OPERATION",
                "preview",
                "Composite supports only fixed native Binance ETH deposit.",
                "Select binance.deposit.",
            ));
        }
        self.prepare_deposit(operation, input).await
    }

    async fn prepare_with_context(
        &self,
        operation: &str,
        input: &Value,
        _context: &fin_domain::PreparationContext,
    ) -> Result<PreparedOperation, FinError> {
        self.prepare(operation, input).await
    }

    async fn preflight_return_route(
        &self,
        operation: &str,
        raw: &Value,
        context: &ReturnRouteContext,
    ) -> Result<ReturnRouteProof, FinError> {
        if operation != "binance.deposit" {
            return Err(error(
                "UNSUPPORTED_OPERATION",
                "preflight",
                "Composite supports only fixed native Binance ETH deposit.",
                "Select binance.deposit.",
            ));
        }
        let input = Self::input(raw)?;
        self.account(&input.account)?;
        let target = self.target(&input.account).await?;
        let inner_input = Self::inner_input(&input, &target);
        let mut route_context = context.clone();
        if !route_context
            .projected
            .cash
            .iter()
            .any(|cash| cash.resource == target.account_resource && cash.asset == target.asset)
        {
            let observed_free = self.source_free(&input.account).await?;
            let exact_free = Quantity::from_decimal(&observed_free, target.asset.decimals)
                .map_err(|_| {
                    error(
                        "BALANCE_INVALID",
                        "preflight",
                        "Current Binance free balance is not an exact supported decimal.",
                        "Retry after reviewing the SPOT balance response.",
                    )
                })?;
            route_context
                .projected
                .cash
                .push(fin_domain::ProjectedCash {
                    resource: target.account_resource.clone(),
                    asset: target.asset.clone(),
                    amount: fin_domain::QuantityInterval {
                        minimum: exact_free.clone(),
                        maximum: exact_free,
                    },
                });
        }
        let mut proof = self
            .ethereum
            .preflight_native_deposit_transfer(&inner_input, &route_context, &target)
            .await?;
        proof.operation = operation.into();
        proof.input_hash = binding_hash(raw)?;
        proof.evidence["composite"] = json!({"operation":operation,"account":input.account,"wallet":input.wallet,"targetEvidenceDigest":target.evidence_digest,"completion":"canonical transaction plus exact matching usable venue credit required"});
        Ok(proof)
    }

    async fn revalidate(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<PreparedOperation, FinError> {
        if prepared.operation != "binance.deposit" {
            return Err(error(
                "UNSUPPORTED_OPERATION",
                "submission",
                "Unexpected composite operation.",
                "Preview binance.deposit again.",
            ));
        }
        let fresh = self.prepare("binance.deposit", &prepared.input).await?;
        let original: DepositBinding =
            serde_json::from_value(prepared.binding.clone()).map_err(|_| internal_error())?;
        let current: DepositBinding =
            serde_json::from_value(fresh.binding.clone()).map_err(|_| internal_error())?;
        if original.target.chain_id != current.target.chain_id
            || original.target.account_resource != current.target.account_resource
            || !original
                .target
                .address
                .eq_ignore_ascii_case(&current.target.address)
            || original.target.asset != current.target.asset
            || original.target.minimum_deposit != current.target.minimum_deposit
            || original.target.network != current.target.network
            || original.target.evidence_digest != current.target.evidence_digest
            || original.inner_input != current.inner_input
            || original.inner_preview.binding != current.inner_preview.binding
            || original.source_free_before != current.source_free_before
        {
            return Err(error(
                "PREVIEW_CHANGED",
                "submission",
                "Binance target, source balance, or Ethereum transfer binding changed.",
                "Obtain a fresh preview and approval.",
            ));
        }
        let mut fresh = fresh;
        // Keep the approved immutable target evidence in the returned binding;
        // fresh timestamp fields are used for expiry only and never replace it.
        fresh.binding = prepared.binding.clone();
        Ok(fresh)
    }

    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn fin_domain::SubmissionJournal>,
    ) -> Result<Submission, FinError> {
        let fresh = self.revalidate(prepared).await?;
        let binding: DepositBinding =
            serde_json::from_value(fresh.binding.clone()).map_err(|_| internal_error())?;
        self.ethereum.submit(&binding.inner_preview, journal).await
    }

    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation, FinError> {
        let unknown = |evidence: Value| Reconciliation {
            status: Status::Unknown,
            evidence,
            verified: None,
        };
        let Some(submission) = record.submission.clone() else {
            return Ok(unknown(
                json!({"reason":"Original EVM transaction identity was not durably recorded."}),
            ));
        };
        let Some(prepared) = record.preview.clone() else {
            return Ok(unknown(
                json!({"reason":"Original composite preview is missing."}),
            ));
        };
        let binding: DepositBinding = match serde_json::from_value(prepared.binding.clone()) {
            Ok(value) => value,
            Err(_) => {
                return Ok(unknown(
                    json!({"reason":"Persisted protected deposit binding is malformed."}),
                ));
            }
        };
        let inner_record = OperationRecord {
            operation: "wallet.transfer".into(),
            input: binding.inner_input.clone(),
            preview: Some(binding.inner_preview.clone()),
            ..record.clone()
        };
        let chain = self.ethereum.reconcile(&inner_record).await?;
        if chain.status != Status::Completed {
            return Ok(Reconciliation {
                status: chain.status,
                evidence: json!({"chain":chain.evidence,"venueCredit":"not yet proved"}),
                verified: chain.verified,
            });
        }
        let Some(mut verified) = chain.verified else {
            return Ok(unknown(
                json!({"reason":"Canonical Ethereum completion did not include verified financial effects."}),
            ));
        };
        if verified.submission != submission {
            return Ok(unknown(
                json!({"reason":"Ethereum proof submission differs from the original persisted identity."}),
            ));
        }
        let tx_hash = submission.reference.clone();
        let input = Self::input(&record.input)?;
        let history = match self
            .binance
            .read(
                "binance.deposit-history",
                &json!({"account":input.account,"coin":"ETH","txHash":tx_hash}),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                return Ok(unknown(
                    json!({"chain":chain.evidence,"venueHistoryError":error.code}),
                ));
            }
        };
        if history["complete"] != true {
            return Ok(unknown(
                json!({"chain":chain.evidence,"venueHistory":"bounded history incomplete"}),
            ));
        }
        let records = history["records"].as_array().cloned().unwrap_or_default();
        if records.len() != 1 {
            return Ok(unknown(
                json!({"chain":chain.evidence,"matchingVenueRows":records.len()}),
            ));
        }
        let row = &records[0];
        if row["creditState"] != "credited"
            || row["status"].as_u64() != Some(1)
            || row["coin"] != "ETH"
            || row["network"] != binding.target.network
            || !row["address"]
                .as_str()
                .is_some_and(|address| address.eq_ignore_ascii_case(&binding.target.address))
            || !row["txId"]
                .as_str()
                .is_some_and(|hash| hash.eq_ignore_ascii_case(&tx_hash))
            || row["addressTag"]
                .as_str()
                .is_some_and(|tag| !tag.is_empty())
        {
            return Ok(unknown(
                json!({"chain":chain.evidence,"venueRow":row,"reason":"Deposit row is not one exact usable ETH credit for the original transaction and target."}),
            ));
        }
        let credit = row["amount"].as_str().ok_or_else(internal_error)?;
        let credited_atoms = Quantity::from_decimal(credit, 36).map_err(|_| internal_error())?;
        if credited_atoms < binding.target.minimum_deposit {
            return Ok(unknown(
                json!({"chain":chain.evidence,"venueRow":row,"reason":"Usable venue credit is below the current protected deposit floor."}),
            ));
        }
        let after = match self.source_free(&input.account).await {
            Ok(value) => value,
            Err(error) => {
                return Ok(unknown(
                    json!({"chain":chain.evidence,"balanceError":error.code}),
                ));
            }
        };
        let before_atoms = Quantity::from_decimal(&binding.source_free_before, 36)
            .map_err(|_| internal_error())?;
        let after_atoms = Quantity::from_decimal(&after, 36).map_err(|_| internal_error())?;
        let before_integer = before_atoms.integer();
        let after_integer = after_atoms.integer();
        if after_integer < before_integer {
            return Ok(unknown(
                json!({"chain":chain.evidence,"venueRow":row,"sourceFreeBefore":binding.source_free_before,"sourceFreeAfter":after,"reason":"Binance free balance did not increase by the venue deposit credit."}),
            ));
        }
        let actual_delta = after_integer - before_integer;
        if actual_delta != credited_atoms.integer() {
            return Ok(unknown(
                json!({"chain":chain.evidence,"venueRow":row,"sourceFreeBefore":binding.source_free_before,"sourceFreeAfter":after,"reason":"Usable history amount and exact Binance free-balance increase do not match."}),
            ));
        }
        let mut effects = Vec::new();
        let mut transfer_seen = false;
        let expected_source = binding
            .inner_preview
            .intent
            .debits
            .first()
            .map(|debit| debit.resource.clone())
            .ok_or_else(internal_error)?;
        let expected_destination = ResourceId(format!(
            "ethereum:1:{}",
            binding.target.address.to_ascii_lowercase()
        ));
        let expected_native = AssetId {
            network: "ethereum:1".into(),
            reference: "native".into(),
            decimals: 18,
        };
        for effect in verified.effects.drain(..) {
            match effect {
                FinancialEffect::Transfer {
                    from,
                    to,
                    debit,
                    credit: chain_credit,
                } if !transfer_seen => {
                    let debit_atoms =
                        Quantity::from_decimal(&debit.atoms.to_decimal(debit.asset.decimals), 18)
                            .map_err(|_| internal_error())?;
                    let expected_credit = Quantity::from_decimal(&debit_atoms.to_decimal(18), 36)
                        .map_err(|_| internal_error())?;
                    if from != expected_source
                        || to != expected_destination
                        || debit.asset != expected_native
                        || chain_credit.asset != expected_native
                        || chain_credit.atoms != debit.atoms
                        || binding.inner_preview.intent.debits[0].amount.atoms != debit.atoms
                        || expected_credit != credited_atoms
                    {
                        return Ok(unknown(
                            json!({"chain":chain.evidence,"venueRow":row,"reason":"Venue credited quantity does not exactly match the original native transaction principal."}),
                        ));
                    }
                    transfer_seen = true;
                    effects.push(FinancialEffect::Transfer {
                        from,
                        to: binding.target.account_resource.clone(),
                        debit,
                        credit: Amount {
                            asset: binding.target.asset.clone(),
                            atoms: credited_atoms.clone(),
                        },
                    });
                }
                FinancialEffect::Cost { .. } => effects.push(effect),
                _ => {
                    return Ok(unknown(
                        json!({"chain":chain.evidence,"reason":"Unexpected extra chain effect is not safe to translate into venue custody."}),
                    ));
                }
            }
        }
        if !transfer_seen {
            return Ok(unknown(
                json!({"chain":chain.evidence,"reason":"Canonical chain proof omitted the expected native transfer."}),
            ));
        }
        verified.effects = effects;
        verified.evidence = json!({"method":"ethereum-canonical-and-binance-usable-deposit-v1","chain":chain.evidence,"venueHistory":history,"venueBalance":{"before":binding.source_free_before,"after":after,"delta":credit},"targetEvidenceDigest":binding.target.evidence_digest,"originalSubmission":submission});
        verified.observed_at = now();
        verified.proof_digest = verified_effect_digest(&verified)?;
        Ok(Reconciliation {
            status: Status::Completed,
            evidence: verified.evidence.clone(),
            verified: Some(verified),
        })
    }
}
