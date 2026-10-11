//! Composite Arbitrum USDC deposit into a Hyperliquid account.
//!
//! The source transfer uses the ordinary protected Ethereum wallet adapter.
//! A canonical source receipt is retained as funds-in-transit evidence; it is
//! never treated as a Hyperliquid credit. Automatic completion is deliberately
//! enabled only after the source transaction is linked to one exact
//! owner-scoped Hyperliquid ledger credit, directly by hash or through the
//! signed `VoteEthDepositAction` when the ledger uses a native transaction hash.

use async_trait::async_trait;
use fin_domain::{
    Adapter, Amount, AssetId, Check, CheckStatus, ConnectionMetadata, Definition, EffectConstraint,
    ExecutionActor, FinError, FinancialEffect, FinancialIntent, FinancialState, OperationKind,
    OperationRecord, PreparedOperation, Quantity, Reconciliation, ResourceAmount, ResourceId,
    StateSnapshot, Status, Submission, VerifiedEffects, now, verified_effect_digest,
};
use fin_ethereum::EthereumAdapter;
use fin_exchange::ccxt::HyperliquidExplorerRequest;
use fin_hyperliquid::{
    AccountMode, AccountModeState, AccountRole, CashWallet, HyperliquidProfile,
    HyperliquidTransport, usdc as hl_usdc,
};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

const ARBITRUM_ONE: u64 = 42161;
const ARBITRUM_USDC: &str = "0xaf88d065e77c8cc2239327c5edb3a432268e5831";
const HYPERLIQUID_BRIDGE2: &str = "0x2df1c51e09aecf9cacb7bc98cb1742757f163df7";
const MINIMUM_USDC: &str = "5";

fin_evm::alloy::sol! {
    interface HyperliquidBridgeDeployment {
        function usdcToken() external view returns (address);
        function paused() external view returns (bool);
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct DepositDeployment {
    bridge: String,
    token: String,
    bridge_code_hash: String,
}

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
    max_amount_usdc: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DepositBinding {
    inner_input: Value,
    inner_preview: PreparedOperation,
    account: String,
    owner: String,
    source_wallet: String,
    amount: Quantity,
    source_asset: AssetId,
    source_resource: ResourceId,
    target_resource: ResourceId,
    /// Frozen destination account mode. Old pending records omit this field
    /// and retain their original target resource for recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_mode: Option<AccountModeState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deployment: Option<DepositDeployment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    usd_ceiling: Option<Value>,
}

/// Broker-only composition. The owner is also the L1 sender because Bridge2
/// credits the signer of the Arbitrum USDC transfer.
pub struct HyperliquidEthereumDepositAdapter {
    profile: Arc<HyperliquidProfile>,
    ethereum: Arc<EthereumAdapter>,
    transport: Arc<dyn HyperliquidTransport>,
}

impl HyperliquidEthereumDepositAdapter {
    pub fn new(
        profile: Arc<HyperliquidProfile>,
        ethereum: Arc<EthereumAdapter>,
        transport: Arc<dyn HyperliquidTransport>,
    ) -> Self {
        Self {
            profile,
            ethereum,
            transport,
        }
    }

    fn input(raw: &Value) -> Result<DepositInput, FinError> {
        serde_json::from_value(raw.clone()).map_err(|_| {
            error(
                "INVALID_INPUT",
                "validation",
                "Hyperliquid deposit input is malformed or contains unknown fields.",
                "Use the hyperliquid.account.deposit schema.",
            )
        })
    }

    fn account(&self, name: &str) -> Result<&fin_hyperliquid::HyperliquidAccount, FinError> {
        self.profile.account(name)
    }

    fn wallet(&self, name: &str) -> Result<ConnectionMetadata, FinError> {
        self.ethereum
            .connections()
            .into_iter()
            .find(|connection| {
                connection.name == name && connection.network.as_deref() == Some("ethereum:42161")
            })
            .ok_or_else(|| {
                error(
                    "ETHEREUM_WALLET_UNKNOWN",
                    "validation",
                    "Selected wallet is not enrolled on Arbitrum One.",
                    "Select a configured signing wallet on Arbitrum One.",
                )
            })
    }

    fn source_asset() -> AssetId {
        AssetId {
            network: "ethereum:42161".into(),
            reference: ARBITRUM_USDC.into(),
            decimals: 6,
        }
    }

    fn source_wallet_matches_owner(
        chain_id: u64,
        signing_enabled: bool,
        wallet_address: Option<&str>,
        owner: &str,
    ) -> bool {
        chain_id == ARBITRUM_ONE
            && signing_enabled
            && wallet_address.is_some_and(|address| address.eq_ignore_ascii_case(owner))
    }

    fn inner_input(input: &DepositInput) -> Value {
        json!({
            "wallet": input.wallet,
            "network": "evm",
            "chain_id": ARBITRUM_ONE,
            "request_id": input.request_id,
            "max_fee_eth": input.max_fee_eth,
            "token": ARBITRUM_USDC,
            "amount": input.amount,
            "recipient": HYPERLIQUID_BRIDGE2,
        })
    }

    fn intent(
        inner: &PreparedOperation,
        amount: &Quantity,
        source_asset: &AssetId,
        target_resource: &ResourceId,
    ) -> Result<FinancialIntent, FinError> {
        let source = inner
            .intent
            .debits
            .first()
            .filter(|debit| debit.amount.asset == *source_asset)
            .map(|debit| debit.resource.clone())
            .ok_or_else(|| {
                error(
                    "INNER_PREVIEW_INVALID",
                    "preview",
                    "Ethereum transfer preview did not preserve the exact Arbitrum USDC identity.",
                    "Refresh the wallet transfer preview.",
                )
            })?;
        let mut locks = vec![source.clone(), target_resource.clone()];
        locks.sort();
        locks.dedup();
        Ok(FinancialIntent {
            debits: vec![ResourceAmount {
                resource: source.clone(),
                amount: Amount {
                    asset: source_asset.clone(),
                    atoms: amount.clone(),
                },
            }],
            maximum_costs: inner.intent.maximum_costs.clone(),
            expected: vec![EffectConstraint::Transfer {
                from: source,
                to: target_resource.clone(),
                debit_asset: source_asset.clone(),
                credit_asset: hl_usdc(),
                maximum_debit: amount.clone(),
                minimum_credit: Quantity::from_decimal(
                    &amount.to_decimal(source_asset.decimals),
                    36,
                )
                .map_err(|_| {
                    error(
                        "INVALID_AMOUNT",
                        "preview",
                        "Amount cannot be represented exactly by Hyperliquid USDC precision.",
                        "Use an exact finite USDC amount.",
                    )
                })?,
            }],
            locks,
        })
    }

    pub fn operation_definition() -> Definition {
        Definition {
            name: "hyperliquid.account.deposit".into(),
            description: "Deposit Arbitrum One USDC to the enrolled Hyperliquid owner through the fixed legacy Bridge2 route.".into(),
            kind: OperationKind::Write,
            execution_actor: ExecutionActor::AgentInvocable,
            input_schema: serde_json::to_value(schema_for!(DepositInput)).unwrap_or(Value::Null),
            output_schema: json!({"type":"object"}),
            error_schema: schema_for!(FinError).into(),
            examples: vec![json!({"requestId":"deposit-001","account":"hl-account","wallet":"wallet","amount":"5","maxFeeEth":"0.0001","maxAmountUsdc":"100"})],
            coverage: "Synthetic composite proof tests and official Bridge2 watcher source for VoteEthDepositAction source-hash linkage; live acceptance remains unverified.".into(),
            completion: "Canonical successful source receipt in one Arbitrum sequenced block, without waiting for Ethereum inclusion or finality, and one exact owner/amount Hyperliquid ledger credit whose hash is the original Arbitrum transaction hash or whose native VoteEthDeposit action embeds it.".into(),
            semantics: json!({
                "sourceChainId": ARBITRUM_ONE,
                "sourceToken": ARBITRUM_USDC,
                "sourceTokenDecimals": 6,
                "bridge": HYPERLIQUID_BRIDGE2,
                "minimumUsdc": MINIMUM_USDC,
                "owner": "must equal the exact enrolled source wallet address; the bridge credits the source signer",
                "recovery": "reconcile the original operation after uncertainty; never resubmit",
                "completion": "canonical source receipt plus one exact owner/amount ledger credit linked by source hash or native VoteEthDeposit action",
                "hashSemantics": "accept an owner-scoped exact deposit ledger row whose hash equals the original Arbitrum source transaction hash; if the ledger row uses a distinct native hash, require txDetails VoteEthDepositAction.ethTxHash to equal that source hash",
                "intermediateStatus": "source-confirmed funds remain in transit and are not usable Hyperliquid collateral",
                "maxAmountUsdc": "human-reviewed upper bound; exact amount must not exceed it"
            }),
        }
    }

    async fn deposit_deployment(&self) -> Result<DepositDeployment, FinError> {
        self.ethereum.runtime.assert_chain().await?;
        let rpc = &self.ethereum.runtime.rpc;
        let block = fin_evm::rpc_u64(&rpc.request("eth_blockNumber", json!([])).await?)?;
        let tag = format!("{block:#x}");
        let before = rpc
            .request("eth_getBlockByNumber", json!([tag, false]))
            .await?;
        let block_hash = fin_evm::hash(before["hash"].as_str().unwrap_or(""))?;
        let bridge = fin_evm::address(HYPERLIQUID_BRIDGE2)?;
        let code = fin_evm::rpc_bytes(
            &rpc.request("eth_getCode", json!([HYPERLIQUID_BRIDGE2, tag]))
                .await?,
        )?;
        if code.is_empty() {
            return Err(error(
                "BRIDGE_DEPLOYMENT_UNVERIFIED",
                "preview",
                "The reviewed Bridge2 address has no deployed code at the pinned Arbitrum block.",
                "Retry after verifying the reviewed deposit deployment; no source transfer was submitted.",
            ));
        }
        let token = self
            .ethereum
            .runtime
            .call(
                bridge,
                &HyperliquidBridgeDeployment::usdcTokenCall {},
                Some(block),
            )
            .await?;
        let paused = self
            .ethereum
            .runtime
            .call(
                bridge,
                &HyperliquidBridgeDeployment::pausedCall {},
                Some(block),
            )
            .await?;
        let after = rpc
            .request("eth_getBlockByNumber", json!([tag, false]))
            .await?;
        if token != fin_evm::address(ARBITRUM_USDC)?
            || paused
            || fin_evm::rpc_u64(&before["number"])? != block
            || fin_evm::rpc_u64(&after["number"])? != block
            || fin_evm::hash(after["hash"].as_str().unwrap_or(""))? != block_hash
        {
            return Err(error(
                "BRIDGE_DEPLOYMENT_UNVERIFIED",
                "preview",
                "The pinned Bridge2 token, availability or canonical block does not match the reviewed Arbitrum USDC route.",
                "Verify the current deployment and retry a fresh preview; no source transfer was submitted.",
            ));
        }
        Ok(DepositDeployment {
            bridge: HYPERLIQUID_BRIDGE2.into(),
            token: ARBITRUM_USDC.into(),
            bridge_code_hash: format!("{:#x}", fin_evm::alloy::primitives::keccak256(code)),
        })
    }

    /// Resolve the deposit destination without treating a missing role as a
    /// signer. A new owner may receive an EVM deposit before Hyperliquid has
    /// initialized its user role, but only an explicit native `default` or
    /// `disabled` abstraction response establishes the separate-wallet
    /// destination mapping for that first deposit.
    async fn deposit_destination_mode(
        &self,
        account: &fin_hyperliquid::HyperliquidAccount,
    ) -> Result<AccountModeState, FinError> {
        let observed = self.profile.recipient_account_mode(account).await?;
        match observed.role {
            AccountRole::MasterUser => Ok(observed.mode),
            AccountRole::Missing => {
                if !matches!(
                    observed.mode.native_abstraction.as_str(),
                    "default" | "disabled"
                ) || observed.mode.mode != AccountMode::Separate
                    || observed.mode.dex_abstraction == Some(true)
                {
                    return Err(mode_error(
                        "A role-missing deposit recipient must explicitly report default or disabled account abstraction with DEX abstraction inactive.",
                    ));
                }
                Ok(observed.mode)
            }
        }
    }

    async fn prepare_deposit(&self, raw: &Value) -> Result<PreparedOperation, FinError> {
        let input = Self::input(raw)?;
        if input.request_id.is_empty() || input.request_id.len() > 128 {
            return Err(error(
                "INVALID_REQUEST_ID",
                "validation",
                "Request ID must be non-empty and at most 128 characters.",
                "Use a new bounded request ID.",
            ));
        }
        let account = self.account(&input.account)?;
        let wallet = self.wallet(&input.wallet)?;
        if account.vault_address().is_some()
            || !account
                .execution_address()
                .eq_ignore_ascii_case(&account.owner)
            || !Self::source_wallet_matches_owner(
                self.ethereum.config.chain_id,
                wallet.signing_enabled,
                wallet.address.as_deref(),
                &account.owner,
            )
        {
            return Err(error(
                "DEPOSIT_OWNER_MISMATCH",
                "preview",
                "The signing wallet must be on Arbitrum One and exactly match the enrolled Hyperliquid owner.",
                "Select the Arbitrum signing wallet for the same owner address.",
            ));
        }
        let account_mode = self.deposit_destination_mode(account).await?;
        account_mode.ensure_supported_cash_accounting()?;
        let amount = Quantity::from_decimal(&input.amount, 6).map_err(|_| {
            error(
                "INVALID_AMOUNT",
                "preview",
                "USDC amount must be a positive exact decimal with at most six places.",
                "Use an exact Arbitrum USDC amount.",
            )
        })?;
        if amount.is_zero()
            || amount < Quantity::from_decimal(MINIMUM_USDC, 6).expect("constant")
            || amount > Quantity::from_decimal(&input.max_amount_usdc, 6).map_err(|_| {
                error(
                    "INVALID_AMOUNT_BOUND",
                    "preview",
                    "Maximum USDC amount must be an exact positive decimal with at most six places.",
                    "Set a valid maximum at least as large as the deposit amount.",
                )
            })?
        {
            return Err(error(
                "DEPOSIT_AMOUNT_OUT_OF_BOUNDS",
                "preview",
                "Deposit must be at least 5 USDC and no greater than the reviewed maximum.",
                "Choose an exact amount within the supported minimum and maximum.",
            ));
        }
        let deployment = self.deposit_deployment().await?;
        let source_asset = Self::source_asset();
        let inner_input = Self::inner_input(&input);
        let inner_preview = self
            .ethereum
            .prepare("wallet.transfer", &inner_input)
            .await?;
        if inner_preview.intent.debits.len() != 1
            || inner_preview.intent.debits[0].amount.asset != source_asset
            || inner_preview.intent.debits[0].amount.atoms != amount
            || inner_preview.intent.expected.iter().all(|expectation| {
                !matches!(expectation, EffectConstraint::Transfer { to, .. }
                    if to.0.eq_ignore_ascii_case(&format!("ethereum:{ARBITRUM_ONE}:{HYPERLIQUID_BRIDGE2}")))
            })
        {
            return Err(error(
                "INNER_PREVIEW_INVALID",
                "preview",
                "Ethereum did not freeze the exact approved Arbitrum USDC transfer to Bridge2.",
                "Do not continue with a changed token, sender, amount, chain, or recipient.",
            ));
        }
        let target_resource = account_mode.cash_resource(account, CashWallet::Perpetual, None)?;
        let intent = Self::intent(&inner_preview, &amount, &source_asset, &target_resource)?;
        let mut valuation = inner_preview.valuation.clone().ok_or_else(|| {
            error(
                "PRICE_DATA_UNAVAILABLE",
                "preview",
                "Exact source-asset and network-fee pricing is required for this deposit.",
                "Configure a supported fresh price source before previewing the deposit.",
            )
        })?;
        let quote = self.profile.quote_usdc().await?;
        if quote.asset != hl_usdc() {
            return Err(error(
                "PRICE_DATA_INVALID",
                "preview",
                "Hyperliquid USDC quote is not bound to the exact venue asset identity.",
                "Refresh exact asset pricing before previewing this deposit.",
            ));
        }
        if !valuation
            .prices
            .iter()
            .any(|price| price.asset == quote.asset)
        {
            valuation.prices.push(quote);
        }
        let binding = DepositBinding {
            inner_input,
            inner_preview: inner_preview.clone(),
            account: input.account.clone(),
            owner: account.owner.to_ascii_lowercase(),
            source_wallet: input.wallet.clone(),
            amount: amount.clone(),
            source_asset: source_asset.clone(),
            source_resource: intent.debits[0].resource.clone(),
            target_resource,
            account_mode: Some(account_mode),
            deployment: Some(deployment),
            usd_ceiling: None,
        };
        Ok(PreparedOperation {
            operation: "hyperliquid.account.deposit".into(),
            input: raw.clone(),
            binding: serde_json::to_value(binding).map_err(|_| internal_error())?,
            summary: format!(
                "Deposit {} USDC from {} on Arbitrum One to Hyperliquid owner {} through Bridge2. Maximum source fee: {} ETH.",
                input.amount,
                input.wallet,
                account.owner,
                input.max_fee_eth,
            ),
            checks: vec![
                Check {
                    name: "fixed_bridge_route".into(),
                    status: CheckStatus::Passed,
                    detail: "Route is fixed to canonical Arbitrum USDC and Hyperliquid Bridge2.".into(),
                },
                Check {
                    name: "source_owner_matches_hyperliquid_owner".into(),
                    status: CheckStatus::Passed,
                    detail: "The Arbitrum signing wallet exactly matches the enrolled Hyperliquid owner.".into(),
                },
            ],
            warnings: vec!["A canonical source receipt is funds-in-transit evidence only. Hyperliquid credit and spendability are not established until exact ledger correlation is proven.".into()],
            observed_at: inner_preview.observed_at.clone(),
            expires_at: inner_preview.expires_at.clone(),
            valuation: Some(valuation),
            intent,
            operation_version: 1,
        })
    }

    fn binding(prepared: &PreparedOperation) -> Result<DepositBinding, FinError> {
        serde_json::from_value(prepared.binding.clone()).map_err(|_| internal_error())
    }

    async fn correlate_ledger_candidate(
        &self,
        input: &DepositInput,
        binding: &DepositBinding,
        tx_hash: &str,
        source_confirmed_at_ms: u64,
    ) -> Result<(Vec<Value>, Vec<(Value, Option<Value>)>), FinError> {
        let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
        let value = self
            .profile
            .read_operation(
                "hyperliquid.ledger",
                &json!({
                    "account": input.account,
                    "since_ms": source_confirmed_at_ms,
                    "until_ms": now_ms,
                }),
            )
            .await?;
        let rows = value["data"]["rows"].as_array().cloned().ok_or_else(|| {
            error(
                "HYPERLIQUID_LEDGER_INVALID",
                "reconciliation",
                "Hyperliquid did not return bounded ledger rows for the enrolled owner.",
                "Keep the original operation pending and reconcile it again.",
            )
        })?;
        let candidates = ledger_candidates(&rows, &binding.amount, source_confirmed_at_ms);
        let mut exact_matches = Vec::new();
        for row in &candidates {
            let Some(native_hash) = row["hash"].as_str() else {
                continue;
            };
            if !valid_native_hash(native_hash) {
                continue;
            }
            // Hyperliquid may place the original Arbitrum transaction hash
            // directly in its owner-scoped ledger row. The source receipt
            // independently proves sender, Bridge2 recipient, token and exact
            // amount; candidate filtering above proves exact deposit amount
            // and time. A distinct ledger hash still requires Explorer proof.
            if source_hash_ledger_match(row, tx_hash) {
                exact_matches.push((row.clone(), None));
                continue;
            }
            let details = self
                .transport
                .explorer(HyperliquidExplorerRequest::TxDetails {
                    hash: native_hash.to_owned(),
                })
                .await?;
            if exact_deposit_action(
                &details,
                native_hash,
                &binding.owner,
                &binding.amount,
                tx_hash,
                source_confirmed_at_ms,
                row["time"].as_u64().unwrap_or_default(),
            ) {
                exact_matches.push((row.clone(), Some(details)));
            }
        }
        Ok((candidates, exact_matches))
    }
}

fn source_receipt_event_digest(
    source: &VerifiedEffects,
    submission: &Submission,
) -> Result<String, FinError> {
    if source.submission != *submission
        || source.evidence["original_hash"].as_str() != Some(submission.reference.as_str())
        || source.evidence["success"] != true
        || source.evidence["block_number"].as_u64().is_none()
        || source.evidence["block_hash"].as_str().is_none()
    {
        return Err(error(
            "SOURCE_PROOF_INVALID",
            "reconciliation",
            "Canonical source receipt identity is incomplete or differs from the original transaction.",
            "Keep the original deposit pending and reconcile its exact source transaction again.",
        ));
    }
    fin_domain::binding_hash(&json!({
        "submission": submission,
        "block_number": source.evidence["block_number"],
        "block_hash": source.evidence["block_hash"],
        "fee_wei": source.evidence["fee_wei"],
        "effects": source.effects,
    }))
    .map_err(|_| internal_error())
}

fn source_progress_bundle(
    record: &OperationRecord,
    binding: &DepositBinding,
    submission: &Submission,
    source: VerifiedEffects,
    pending_evidence: Value,
) -> Result<VerifiedEffects, FinError> {
    let source_digest = source_receipt_event_digest(&source, submission)?;
    let source_event_id = format!("source:{}", submission.reference);
    if let Some(previous) = record.effects.as_ref() {
        let same_source_progress = fin_domain::SettlementProgress::from_evidence(
            &previous.evidence,
        )?
        .is_some_and(|progress| {
            progress.sequence == 1
                && progress.event_digests.get(&source_event_id) == Some(&source_digest)
        });
        if previous.submission == *submission
            && same_source_progress
            && previous.proof_digest == verified_effect_digest(previous)?
        {
            return Ok(previous.clone());
        }
    }

    let bridge_resource = ResourceId(format!(
        "ethereum:{ARBITRUM_ONE}:{}",
        HYPERLIQUID_BRIDGE2.to_ascii_lowercase()
    ));
    let mut exact_source_transfer = None;
    let mut costs = Vec::new();
    for effect in &source.effects {
        match effect {
            FinancialEffect::Transfer {
                from,
                to,
                debit,
                credit,
            } if from == &binding.source_resource
                && to == &bridge_resource
                && debit.asset == binding.source_asset
                && debit.atoms == binding.amount
                && credit.asset == binding.source_asset
                && credit.atoms == binding.amount =>
            {
                if exact_source_transfer.replace(()).is_some() {
                    return Err(internal_error());
                }
            }
            FinancialEffect::Cost { .. } => costs.push(effect.clone()),
            _ => return Err(internal_error()),
        }
    }
    if exact_source_transfer.is_none() {
        return Err(error(
            "SOURCE_PROOF_INVALID",
            "reconciliation",
            "Canonical source receipt did not prove the exact prepared USDC debit to Bridge2.",
            "Keep the original deposit unresolved and inspect its source receipt proof.",
        ));
    }
    let principal = Amount {
        asset: binding.source_asset.clone(),
        atoms: binding.amount.clone(),
    };
    let evidence_id = format!(
        "ethereum:{ARBITRUM_ONE}:{}:transaction",
        submission.reference.to_ascii_lowercase()
    );
    let mut event_digests = BTreeMap::new();
    event_digests.insert(source_event_id, source_digest);
    let evidence = json!({
        "method":"canonical-arbitrum-source-receipt",
        "sourceReceipt":source.evidence,
        "sourceProof":source,
        "depositState":"source_confirmed_funds_in_transit",
        "usable":false,
        "transit":{
            "transfer_id":submission.reference,
            "source_submission":submission.reference,
            "evidence_id":evidence_id,
            "resource":binding.source_resource.0,
            "recipient_resource":binding.target_resource.0,
            "amount_atoms":binding.amount.as_str(),
            "asset":binding.source_asset,
        },
        "settlement_progress": fin_domain::SettlementProgress {
            version:1,
            sequence:1,
            snapshot_id:format!("source:{}", submission.reference),
            event_digests,
        },
        "pendingDetails":pending_evidence,
    });
    let mut effects = vec![
        FinancialEffect::CashDebit {
            resource: binding.source_resource.clone(),
            amount: principal.clone(),
        },
        FinancialEffect::TransitPrincipal {
            transfer_id: submission.reference.clone(),
            amount: principal,
        },
    ];
    effects.extend(costs);
    let mut progress = VerifiedEffects {
        evidence_version: source.evidence_version,
        submission: submission.clone(),
        effects,
        evidence,
        observed_at: source.observed_at,
        proof_digest: String::new(),
    };
    progress.proof_digest = verified_effect_digest(&progress)?;
    Ok(progress)
}

fn ledger_candidates(
    rows: &[Value],
    expected_amount: &Quantity,
    source_confirmed_at_ms: u64,
) -> Vec<Value> {
    let mut amount_candidates = Vec::new();
    for row in rows {
        if row["delta"]["type"] != "deposit"
            || !row["time"]
                .as_u64()
                .is_some_and(|time| time >= source_confirmed_at_ms)
        {
            continue;
        }
        let exact_amount = row["delta"]["usdc"]
            .as_str()
            .and_then(|amount| Quantity::from_decimal(amount, 6).ok())
            .is_some_and(|amount| amount == *expected_amount);
        if !exact_amount {
            continue;
        }
        amount_candidates.push(row.clone());
    }
    amount_candidates
}

fn source_hash_ledger_match(row: &Value, source_hash: &str) -> bool {
    row["hash"]
        .as_str()
        .is_some_and(|hash| valid_native_hash(hash) && hash.eq_ignore_ascii_case(source_hash))
}

fn source_search_start_ms(operation_created_at: &str, preview_observed_at: &str) -> u64 {
    let timestamp_ms = |value: &str| {
        chrono::DateTime::parse_from_rfc3339(value)
            .ok()
            .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
    };
    match (
        timestamp_ms(operation_created_at),
        timestamp_ms(preview_observed_at),
    ) {
        (Some(created), Some(preview)) => created.min(preview),
        (Some(created), None) => created,
        (None, Some(preview)) => preview,
        // Malformed persisted times fail closed without using a later
        // reconciliation observation as the lower bound.
        (None, None) => u64::MAX,
    }
}

fn valid_native_hash(hash: &str) -> bool {
    hash.len() == 66 && hash.starts_with("0x") && hash[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn exact_deposit_action(
    response: &Value,
    native_hash: &str,
    owner: &str,
    expected_amount: &Quantity,
    source_hash: &str,
    source_confirmed_at_ms: u64,
    ledger_time_ms: u64,
) -> bool {
    let tx = &response["tx"];
    let action = &tx["action"];
    let action_type = action["type"].as_str().unwrap_or("");
    let source_amount_micro = expected_amount.as_str().parse::<u64>().ok();
    let action_usd = tx_amount(&action["usd"]);
    response["type"] == "txDetails"
        && tx["hash"]
            .as_str()
            .is_some_and(|hash| hash.eq_ignore_ascii_case(native_hash))
        && tx["error"].is_null()
        && tx["time"]
            .as_u64()
            .is_some_and(|time| time >= source_confirmed_at_ms && time == ledger_time_ms)
        && tx["block"].as_u64().is_some_and(|block| block > 0)
        && matches!(action_type, "voteEthDeposit" | "VoteEthDepositAction")
        && action["user"]
            .as_str()
            .is_some_and(|user| user.eq_ignore_ascii_case(owner))
        && source_amount_micro.is_some()
        && action_usd == source_amount_micro
        && action_source_hash(action).is_some_and(|hash| hash.eq_ignore_ascii_case(source_hash))
}

fn tx_amount(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn action_source_hash(action: &Value) -> Option<&str> {
    action
        .get("ethTxHash")
        .or_else(|| action.get("eth_tx_hash"))?
        .as_str()
}

fn internal_error() -> FinError {
    error(
        "INTERNAL_ERROR",
        "broker",
        "Protected Hyperliquid deposit record is invalid.",
        "Keep the operation unresolved and inspect protected broker diagnostics.",
    )
}

fn mode_error(message: &str) -> FinError {
    error(
        "HYPERLIQUID_DEPOSIT_MODE_UNVERIFIED",
        "preview",
        message,
        "Wait for an explicit supported Hyperliquid account mode before authorizing the Arbitrum source transfer.",
    )
}

#[async_trait]
impl Adapter for HyperliquidEthereumDepositAdapter {
    fn definitions(&self) -> Vec<Definition> {
        vec![Self::operation_definition()]
    }

    fn authorization_identities(&self) -> Vec<ConnectionMetadata> {
        let mut identities = self
            .profile
            .accounts
            .iter()
            .map(|account| account.metadata())
            .collect::<Vec<_>>();
        identities.extend(self.ethereum.authorization_identities());
        identities.sort_by(|a, b| {
            (&a.provider, &a.name, &a.resource).cmp(&(&b.provider, &b.name, &b.resource))
        });
        identities.dedup();
        identities
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
            "Hyperliquid deposit is a write operation.",
            "Preview hyperliquid.account.deposit before an authorized execute.",
        ))
    }

    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation, FinError> {
        if operation != "hyperliquid.account.deposit" {
            return Err(error(
                "UNSUPPORTED_OPERATION",
                "preview",
                "This composite supports only the fixed Hyperliquid Bridge2 deposit route.",
                "Select hyperliquid.account.deposit.",
            ));
        }
        self.prepare_deposit(input).await
    }

    async fn revalidate(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<PreparedOperation, FinError> {
        if prepared.operation != "hyperliquid.account.deposit" || prepared.operation_version != 1 {
            return Err(error(
                "PREVIEW_CHANGED",
                "submission",
                "This Hyperliquid deposit preview cannot authorize a new source transfer.",
                "Obtain a fresh preview and approval.",
            ));
        }
        let binding = Self::binding(prepared)?;
        if let Some(approved_mode) = binding.account_mode.as_ref() {
            let account = self.account(&binding.account)?;
            let fresh_mode = self.deposit_destination_mode(account).await?;
            if &fresh_mode != approved_mode {
                return Err(error(
                    "PREVIEW_CHANGED",
                    "submission",
                    "The Hyperliquid deposit destination account mode changed after review.",
                    "Prepare a fresh deposit preview; the source transfer was not submitted.",
                ));
            }
        }
        let deployment = self.deposit_deployment().await?;
        if binding
            .deployment
            .as_ref()
            .is_some_and(|approved| approved != &deployment)
        {
            return Err(error(
                "PREVIEW_CHANGED",
                "submission",
                "The reviewed Bridge2 deployment changed after approval.",
                "Prepare a fresh deposit preview; the original source transfer was not submitted.",
            ));
        }
        let inner = self.ethereum.revalidate(&binding.inner_preview).await?;
        if inner.binding != binding.inner_preview.binding || inner.input != binding.inner_input {
            return Err(error(
                "PREVIEW_CHANGED",
                "submission",
                "The immutable source transfer binding changed.",
                "Obtain a fresh preview and approval.",
            ));
        }
        Ok(prepared.clone())
    }

    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn fin_domain::SubmissionJournal>,
    ) -> Result<Submission, FinError> {
        let fresh = self.revalidate(prepared).await?;
        let binding = Self::binding(&fresh)?;
        self.ethereum.submit(&binding.inner_preview, journal).await
    }

    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation, FinError> {
        let unknown = |evidence| Reconciliation {
            status: Status::Unknown,
            evidence,
            verified: None,
        };
        let Some(submission) = record.submission.clone() else {
            return Ok(unknown(
                json!({"reason":"Original Arbitrum submission identity was not durably recorded."}),
            ));
        };
        let Some(prepared) = record.preview.as_ref() else {
            return Ok(unknown(
                json!({"reason":"Original composite preview is missing."}),
            ));
        };
        if prepared.operation_version != 1 {
            return Ok(unknown(
                json!({"reason":"Persisted Hyperliquid deposit preview version is unsupported; original write was not replayed."}),
            ));
        }
        let binding = match Self::binding(prepared) {
            Ok(binding) => binding,
            Err(_) => {
                return Ok(unknown(
                    json!({"reason":"Persisted protected Hyperliquid deposit binding is malformed."}),
                ));
            }
        };
        let input = match Self::input(&record.input) {
            Ok(input) => input,
            Err(_) => {
                return Ok(unknown(
                    json!({"reason":"Persisted typed deposit input is malformed."}),
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
        if chain.status == Status::Failed {
            return Ok(Reconciliation {
                status: Status::Failed,
                evidence: json!({"source":chain.evidence,"hyperliquidCredit":"not applicable after failed source transaction"}),
                verified: chain.verified,
            });
        }
        if chain.status != Status::Completed {
            return Ok(Reconciliation {
                status: chain.status,
                evidence: json!({"source":chain.evidence,"depositState":"source transaction not yet canonically confirmed; do not resubmit"}),
                verified: None,
            });
        }
        let Some(mut verified) = chain.verified.clone() else {
            return Ok(unknown(
                json!({"source":chain.evidence,"reason":"Canonical source receipt did not include verified financial effects."}),
            ));
        };
        if verified.submission != submission {
            return Ok(unknown(
                json!({"source":chain.evidence,"reason":"Source proof differs from the original persisted transaction identity."}),
            ));
        }
        // `verified.observed_at` is when this status read reconciled the
        // source, not when the source transaction was submitted. A credit may
        // have arrived before a later status read. The persisted operation and
        // preview timestamps predate dispatch; exact source-hash and receipt
        // correlation still bind any selected credit to this operation.
        let source_confirmed_at_ms =
            source_search_start_ms(&record.created_at, &prepared.observed_at);
        let (amount_candidates, exact_matches) = match self
            .correlate_ledger_candidate(
                &input,
                &binding,
                &submission.reference,
                source_confirmed_at_ms,
            )
            .await
        {
            Ok(matches) => matches,
            Err(error) => {
                let evidence = json!({"source":chain.evidence,"sourceProof":verified,"depositState":"source_confirmed_funds_in_transit","ledgerError":error.code,"usable":false});
                let progress = source_progress_bundle(
                    record,
                    &binding,
                    &submission,
                    chain.verified.clone().ok_or_else(internal_error)?,
                    evidence.clone(),
                )?;
                return Ok(Reconciliation {
                    status: Status::Pending,
                    evidence,
                    verified: Some(progress),
                });
            }
        };
        let owner_still_matches = self
            .profile
            .account(&input.account)
            .is_ok_and(|account| account.owner.eq_ignore_ascii_case(&binding.owner));
        if !owner_still_matches || exact_matches.len() != 1 {
            let evidence = json!({
                "source":chain.evidence,
                "sourceProof":verified,
                "depositState":"source_confirmed_funds_in_transit",
                "usable":false,
                "ownerMatched":owner_still_matches,
                "exactSourceHashRows":exact_matches.len(),
                "amountAndTimeCandidates":amount_candidates,
                "correlation":"pending: awaiting one exact owner-scoped ledger credit linked by source hash or native VoteEthDeposit action"
            });
            let source_proof = chain.verified.clone().ok_or_else(internal_error)?;
            let progress = source_progress_bundle(
                record,
                &binding,
                &submission,
                source_proof,
                evidence.clone(),
            )?;
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence,
                verified: Some(progress),
            });
        }
        let (ledger_row, tx_details) = exact_matches.into_iter().next().expect("one exact match");
        let correlation_method = if tx_details.is_some() {
            "native_vote_eth_deposit_action"
        } else {
            "owner_scoped_source_hash_ledger_row"
        };
        let destination_mode_matches = match self.account(&input.account) {
            Ok(account) => match self.deposit_destination_mode(account).await {
                Ok(fresh) => match binding.account_mode.as_ref() {
                    Some(approved) => &fresh == approved,
                    // Legacy unresolved previews targeted the separate
                    // perpetual wallet. They remain recoverable only while
                    // that exact native mapping is still in effect.
                    None => {
                        fresh.mode == AccountMode::Separate && fresh.dex_abstraction != Some(true)
                    }
                },
                Err(_) => false,
            },
            Err(_) => false,
        };
        if !destination_mode_matches {
            let evidence = json!({
                "source":chain.evidence,
                "sourceProof":verified,
                "hyperliquidCredit":{"ledger":ledger_row,"txDetails":tx_details,"correlationMethod":correlation_method},
                "depositState":"source_confirmed_funds_in_transit",
                "usable":false,
                "reason":"The current destination account mode does not match the reviewed destination resource mapping."
            });
            let progress = source_progress_bundle(
                record,
                &binding,
                &submission,
                chain.verified.clone().ok_or_else(internal_error)?,
                evidence.clone(),
            )?;
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence,
                verified: Some(progress),
            });
        }
        let credit = Quantity::from_decimal(
            ledger_row["delta"]["usdc"]
                .as_str()
                .ok_or_else(internal_error)?,
            36,
        )
        .map_err(|_| internal_error())?;
        let expected_credit =
            Quantity::from_decimal(&input.amount, 36).map_err(|_| internal_error())?;
        if credit != expected_credit {
            let evidence = json!({
                "source":chain.evidence,
                "sourceProof":verified,
                "depositState":"source_confirmed_funds_in_transit",
                "usable":false,
                "reason":"Exact native ledger credit did not match the prepared amount at venue precision."
            });
            let source_proof = chain.verified.clone().ok_or_else(internal_error)?;
            let progress = source_progress_bundle(
                record,
                &binding,
                &submission,
                source_proof,
                evidence.clone(),
            )?;
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence,
                verified: Some(progress),
            });
        }
        let bridge_resource = ResourceId(format!(
            "ethereum:{ARBITRUM_ONE}:{}",
            HYPERLIQUID_BRIDGE2.to_ascii_lowercase()
        ));
        let mut transfer_proved = false;
        for effect in &mut verified.effects {
            if let FinancialEffect::Transfer {
                from,
                to,
                debit,
                credit: effect_credit,
            } = effect
                && *from == binding.source_resource
                && *to == bridge_resource
                && debit.asset == binding.source_asset
                && debit.atoms == binding.amount
                && effect_credit.asset == binding.source_asset
                && effect_credit.atoms == binding.amount
            {
                *to = binding.target_resource.clone();
                effect_credit.asset = hl_usdc();
                effect_credit.atoms = credit.clone();
                transfer_proved = true;
            }
        }
        if !transfer_proved {
            return Ok(unknown(json!({
                "source":chain.evidence,
                "sourceProof":verified,
                "reason":"Verified source receipt did not contain the exact reviewed transfer effect."
            })));
        }
        verified.evidence = json!({
            "method":"arbitrum-bridge2-source-receipt-plus-owner-scoped-hyperliquid-ledger-credit",
            "source":chain.evidence,
            "sourceProof":verified.evidence,
            "hyperliquidCredit":{"ledger":ledger_row.clone(),"txDetails":tx_details.clone(),"correlationMethod":correlation_method},
            "correlation":{"sourceTransactionHash":submission.reference,"owner":binding.owner,"amount":binding.amount,"method":correlation_method},
        });
        let source_proof = chain.verified.clone().ok_or_else(internal_error)?;
        let source_digest = source_receipt_event_digest(&source_proof, &submission)?;
        let native_digest = fin_domain::binding_hash(&json!({
            "ledger":ledger_row,
            "txDetails":tx_details,
            "sourceTransactionHash":submission.reference,
        }))
        .map_err(|_| internal_error())?;
        let mut event_digests = BTreeMap::new();
        event_digests.insert(format!("source:{}", submission.reference), source_digest);
        event_digests.insert(
            format!(
                "destination:{}",
                ledger_row["hash"].as_str().unwrap_or_default()
            ),
            native_digest,
        );
        verified.evidence["settlement_progress"] =
            serde_json::to_value(fin_domain::SettlementProgress {
                version: 1,
                sequence: 2,
                snapshot_id: format!("credit:{}", ledger_row["hash"].as_str().unwrap_or_default()),
                event_digests,
            })
            .map_err(|_| internal_error())?;
        verified.observed_at = source_proof.observed_at.clone();
        verified.proof_digest = verified_effect_digest(&verified)?;
        Ok(Reconciliation {
            status: Status::Completed,
            evidence: verified.evidence.clone(),
            verified: Some(verified),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fin_domain::{
        AccountingLocation, ApprovalAudit, AssetPrice, AuthorizationChallenge, Authorizer,
        CostBound, CostTreatment, FinError, OperationRecord, SubmissionJournal,
        authorization_binding, binding_hash,
    };
    use fin_engine::{Engine, EngineOptions};
    use fin_ethereum::{EthereumAdapter, EthereumConfig, WalletConnection, WalletValuation};
    use fin_evm::{EvmRpc, EvmRuntime, FrozenTransaction};
    use fin_exchange::ccxt::{HyperliquidActionEnvelope, HyperliquidInfoRequest};
    use fin_hyperliquid::{
        DeferredExecutionState, HyperliquidAccount, HyperliquidPrices, HyperliquidTransport,
    };
    use fin_store::{Store, SubmissionRow};
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct InnerPrepareRpc;

    #[async_trait]
    impl EvmRpc for InnerPrepareRpc {
        async fn request(&self, method: &str, params: Value) -> fin_domain::Result<Value> {
            let word = |value: u128| format!("0x{value:064x}");
            match method {
                "eth_chainId" => Ok(json!("0xa4b1")),
                "eth_getCode" => Ok(json!("0x6000")),
                "eth_call" => {
                    let data = params[0]["data"].as_str().unwrap_or_default();
                    let selector = data.get(0..10).unwrap_or_default();
                    let result = if params[0].get("from").is_some() {
                        word(1)
                    } else {
                        match selector {
                            "0x313ce567" => word(6),
                            "0x95d89b41" => format!("0x{:0<64}", "55534443"),
                            "0x70a08231" => word(100_000_000),
                            _ => {
                                return Err(FinError::validation(
                                    "Unexpected synthetic token metadata call.",
                                ));
                            }
                        }
                    };
                    Ok(json!(result))
                }
                "eth_estimateGas" => Ok(json!("0x186a0")),
                "eth_getBlockByNumber" => Ok(json!({"baseFeePerGas":"0x1"})),
                "eth_maxPriorityFeePerGas" => Ok(json!("0x989680")),
                "eth_getTransactionCount" => Ok(json!("0x0")),
                "eth_getBalance" => Ok(json!("0xde0b6b3a7640000")),
                _ => Err(FinError::validation(
                    "Unexpected synthetic EVM preparation request.",
                )),
            }
        }
    }

    struct InnerPreparePrices;

    #[async_trait]
    impl WalletValuation for InnerPreparePrices {
        async fn quote_usd(
            &self,
            _asset: &AssetId,
            _units: Quantity,
        ) -> fin_domain::Result<(Quantity, String)> {
            Ok((
                Quantity::new("1").unwrap(),
                "synthetic contract test".into(),
            ))
        }
    }

    fn real_source_adapter() -> EthereumAdapter {
        let runtime = Arc::new(EvmRuntime::new(
            Arc::new(InnerPrepareRpc),
            ARBITRUM_ONE,
            None,
        ));
        EthereumAdapter::new(
            EthereumConfig::wallet_only(ARBITRUM_ONE, 1, vec![ARBITRUM_USDC.into()]),
            vec![WalletConnection {
                name: "synthetic-arbitrum".into(),
                address: "0x1111111111111111111111111111111111111111".into(),
                signing_enabled: true,
            }],
            runtime,
        )
        .unwrap()
        .with_wallet_valuation(Arc::new(InnerPreparePrices))
    }

    #[tokio::test]
    async fn composite_source_input_is_accepted_by_the_real_ethereum_adapter() {
        let input = DepositInput {
            request_id: "source-contract-test".into(),
            account: "synthetic-hl".into(),
            wallet: "synthetic-arbitrum".into(),
            amount: "5".into(),
            max_fee_eth: "0.0001".into(),
            max_amount_usdc: "10".into(),
        };
        let source = HyperliquidEthereumDepositAdapter::inner_input(&input);
        assert_eq!(source["chain_id"], ARBITRUM_ONE);
        assert_eq!(source["request_id"], input.request_id);
        assert_eq!(source["max_fee_eth"], input.max_fee_eth);
        assert!(source.get("chainId").is_none());

        let prepared = real_source_adapter()
            .prepare("wallet.transfer", &source)
            .await
            .unwrap();
        assert_eq!(prepared.input, source);
        assert_eq!(
            prepared.intent.debits[0].amount.asset.reference,
            ARBITRUM_USDC
        );
        assert_eq!(prepared.intent.expected.len(), 1);
    }

    struct SyntheticDepositRpc {
        owner: String,
        transaction_hash: String,
        block_hash: String,
        transaction: FrozenTransaction,
    }

    #[async_trait]
    impl EvmRpc for SyntheticDepositRpc {
        async fn request(&self, method: &str, _params: Value) -> fin_domain::Result<Value> {
            let result = match method {
                "eth_chainId" => json!("0xa4b1"),
                "eth_getTransactionReceipt" => json!({
                    "transactionHash":self.transaction_hash,
                    "blockNumber":"0x2a",
                    "blockHash":self.block_hash,
                    "status":"0x1",
                    "gasUsed":"0x5208",
                    "effectiveGasPrice":"0x1",
                    "logs":[{
                        "address":ARBITRUM_USDC,
                        "topics":[
                            format!("{:#x}", fin_evm::alloy::primitives::keccak256("Transfer(address,address,uint256)")),
                            format!("0x{:0>64}", self.owner.trim_start_matches("0x")),
                            format!("0x{:0>64}", HYPERLIQUID_BRIDGE2.trim_start_matches("0x")),
                        ],
                        "data":format!("0x{:0>64x}", 5_000_000u64),
                        "removed":false,
                        "transactionHash":self.transaction_hash,
                        "blockHash":self.block_hash,
                        "blockNumber":"0x2a"
                    }]
                }),
                "eth_getTransactionByHash" => json!({
                    "hash":self.transaction_hash,
                    "blockHash":self.block_hash,
                    "blockNumber":"0x2a",
                    "chainId":"0xa4b1",
                    "from":self.transaction.sender,
                    "to":self.transaction.to,
                    "type":"0x2",
                    "nonce":"0x7",
                    "gas":format!("{:#x}",self.transaction.gas),
                    "value":"0x0",
                    "input":self.transaction.calldata,
                    "maxFeePerGas":format!("{:#x}",self.transaction.max_fee_per_gas.as_str().parse::<u128>().unwrap()),
                    "maxPriorityFeePerGas":format!("{:#x}",self.transaction.max_priority_fee_per_gas.as_str().parse::<u128>().unwrap()),
                    "accessList":[]
                }),
                "eth_getBlockByNumber" => json!({"hash":self.block_hash,"number":"0x2a"}),
                "eth_blockNumber" => json!("0x2b"),
                _ => return Err(FinError::validation("Unexpected synthetic EVM read.")),
            };
            Ok(result)
        }
    }

    struct SyntheticDepositTransport {
        native_hash: String,
        ledger_hash_override: std::sync::Mutex<Option<String>>,
        ledger_time_override: std::sync::atomic::AtomicU64,
        duplicate_credit_rows: std::sync::atomic::AtomicBool,
        explorer_calls: AtomicUsize,
        action_source_hash: std::sync::Mutex<String>,
        credit_visible: std::sync::atomic::AtomicBool,
        ledger_time: std::sync::atomic::AtomicU64,
        dispatches: AtomicUsize,
        role: std::sync::Mutex<Value>,
        abstraction: std::sync::Mutex<Value>,
        dex_abstraction: std::sync::Mutex<Value>,
    }

    #[async_trait]
    impl HyperliquidTransport for SyntheticDepositTransport {
        async fn info(&self, request: HyperliquidInfoRequest) -> Result<Value, FinError> {
            let (start_time_ms, end_time_ms) = match request {
                HyperliquidInfoRequest::UserRole { .. } => {
                    return Ok(self.role.lock().unwrap().clone());
                }
                HyperliquidInfoRequest::UserAbstractionState { .. } => {
                    return Ok(self.abstraction.lock().unwrap().clone());
                }
                HyperliquidInfoRequest::UserDexAbstractionState { .. } => {
                    return Ok(self.dex_abstraction.lock().unwrap().clone());
                }
                HyperliquidInfoRequest::UserNonFundingLedgerUpdates {
                    start_time_ms,
                    end_time_ms: Some(end_time_ms),
                    ..
                } => (start_time_ms, end_time_ms),
                _ => {
                    return Err(FinError::validation(
                        "Unexpected synthetic Hyperliquid query.",
                    ));
                }
            };
            let row_time_ms = self.ledger_time_override.load(Ordering::SeqCst);
            let row_time_ms = if row_time_ms == 0 {
                end_time_ms
            } else {
                row_time_ms
            };
            self.ledger_time.store(row_time_ms, Ordering::SeqCst);
            let rows = if self.credit_visible.load(Ordering::SeqCst) && end_time_ms >= start_time_ms
            {
                let row = json!({
                    "time":row_time_ms,
                    "hash":self.ledger_hash_override.lock().unwrap().clone().unwrap_or_else(|| self.native_hash.clone()),
                    "delta":{"type":"deposit","usdc":"5.000000"}
                });
                if self.duplicate_credit_rows.load(Ordering::SeqCst) {
                    vec![row.clone(), row]
                } else {
                    vec![row]
                }
            } else {
                vec![]
            };
            Ok(json!(rows))
        }

        async fn explorer(&self, request: HyperliquidExplorerRequest) -> Result<Value, FinError> {
            self.explorer_calls.fetch_add(1, Ordering::SeqCst);
            let HyperliquidExplorerRequest::TxDetails { hash } = request else {
                return Err(FinError::validation("Unexpected synthetic explorer query."));
            };
            let time = self.ledger_time.load(Ordering::SeqCst);
            Ok(json!({
                "type":"txDetails",
                "tx":{
                    "hash":hash,
                    "time":time,
                    "block":42,
                    "error":null,
                    "action":{
                        "type":"voteEthDeposit",
                        "user":"0x1111111111111111111111111111111111111111",
                        "usd":5000000,
                        "ethTxHash":self.action_source_hash.lock().unwrap().clone(),
                    }
                }
            }))
        }

        async fn dispatch(
            &self,
            _account: &HyperliquidAccount,
            _envelope: &HyperliquidActionEnvelope,
            _journal: Arc<dyn SubmissionJournal>,
        ) -> Result<Value, FinError> {
            self.dispatches.fetch_add(1, Ordering::SeqCst);
            Err(FinError::validation(
                "Deposit reconciliation must never dispatch.",
            ))
        }
    }

    struct SyntheticPrices;
    #[async_trait]
    impl HyperliquidPrices for SyntheticPrices {
        async fn usdc_price(&self) -> Result<AssetPrice, FinError> {
            Ok(AssetPrice {
                asset: hl_usdc(),
                unit_atoms: Quantity::new("1000000").unwrap(),
                usd_micros_per_unit: Quantity::new("1000000").unwrap(),
                observed_at: now(),
                source: "synthetic deposit test".into(),
            })
        }
    }

    struct SyntheticAuthorizer;
    #[async_trait]
    impl Authorizer for SyntheticAuthorizer {
        async fn authorize(
            &self,
            challenge: &AuthorizationChallenge,
        ) -> fin_domain::Result<ApprovalAudit> {
            Ok(ApprovalAudit {
                mechanism: "synthetic deposit test".into(),
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: now(),
                expires_at: challenge.expires_at.clone(),
            })
        }
    }

    struct DeploymentRpc {
        mode: u8,
        block_reads: std::sync::atomic::AtomicUsize,
    }
    #[async_trait]
    impl EvmRpc for DeploymentRpc {
        async fn request(&self, method: &str, params: Value) -> fin_domain::Result<Value> {
            use fin_evm::alloy::sol_types::{SolCall, SolValue};
            Ok(match method {
                "eth_chainId" => json!("0xa4b1"),
                "eth_blockNumber" => json!("0x10"),
                "eth_getBlockByNumber" => {
                    assert_eq!(params[0], "0x10");
                    let n = self
                        .block_reads
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    json!({"number":"0x10","hash":format!("0x{}", if self.mode==4 && n>0 {"22"} else {"11"}.repeat(32))})
                }
                "eth_getCode" => {
                    assert_eq!(params, json!([HYPERLIQUID_BRIDGE2, "0x10"]));
                    json!(if self.mode == 1 { "0x" } else { "0x6000" })
                }
                "eth_call" => {
                    assert_eq!(params[0]["to"], HYPERLIQUID_BRIDGE2);
                    assert_eq!(params[1], "0x10");
                    let call = params[0]["data"].as_str().unwrap();
                    if call
                        == fin_evm::hex_bytes(&HyperliquidBridgeDeployment::usdcTokenCall::SELECTOR)
                    {
                        let token = if self.mode == 2 {
                            "0x2222222222222222222222222222222222222222"
                        } else {
                            ARBITRUM_USDC
                        };
                        json!(fin_evm::hex_bytes(&fin_evm::address(token)?.abi_encode()))
                    } else {
                        assert_eq!(
                            call,
                            fin_evm::hex_bytes(&HyperliquidBridgeDeployment::pausedCall::SELECTOR)
                        );
                        json!(fin_evm::hex_bytes(&(self.mode == 3).abi_encode()))
                    }
                }
                _ => panic!("Unexpected synthetic deployment method: {method}"),
            })
        }
    }

    #[tokio::test]
    async fn pinned_deposit_deployment_rejects_missing_code_foreign_token_pause_and_reorg() {
        for mode in 0..=4 {
            let mut adapter = composite_for_no_submission_restart();
            adapter.ethereum = Arc::new(
                EthereumAdapter::new(
                    EthereumConfig::wallet_only(ARBITRUM_ONE, 1, vec![]),
                    vec![WalletConnection {
                        name: "synthetic".into(),
                        address: "0x1111111111111111111111111111111111111111".into(),
                        signing_enabled: false,
                    }],
                    Arc::new(fin_evm::EvmRuntime::new(
                        Arc::new(DeploymentRpc {
                            mode,
                            block_reads: Default::default(),
                        }),
                        ARBITRUM_ONE,
                        None,
                    )),
                )
                .unwrap(),
            );
            let result = adapter.deposit_deployment().await;
            if mode == 0 {
                let proof = result.unwrap();
                assert_eq!(proof.bridge, HYPERLIQUID_BRIDGE2);
                assert_eq!(proof.token, ARBITRUM_USDC);
                assert_eq!(proof.bridge_code_hash.len(), 66);
            } else {
                assert_eq!(result.unwrap_err().code, "BRIDGE_DEPLOYMENT_UNVERIFIED");
            }
        }
    }

    struct NoCallRpc;
    #[async_trait]
    impl EvmRpc for NoCallRpc {
        async fn request(&self, _method: &str, _params: Value) -> fin_domain::Result<Value> {
            Err(FinError::validation("Synthetic RPC must not be called."))
        }
    }

    struct NoCallTransport;
    #[async_trait]
    impl HyperliquidTransport for NoCallTransport {
        async fn info(&self, _request: HyperliquidInfoRequest) -> Result<Value, FinError> {
            Err(FinError::validation(
                "Synthetic Hyperliquid read must not be called.",
            ))
        }
        async fn dispatch(
            &self,
            _account: &HyperliquidAccount,
            _envelope: &HyperliquidActionEnvelope,
            _journal: Arc<dyn SubmissionJournal>,
        ) -> Result<Value, FinError> {
            Err(FinError::validation("Synthetic write must not be called."))
        }
    }

    struct NoCallPrices;
    #[async_trait]
    impl HyperliquidPrices for NoCallPrices {
        async fn usdc_price(&self) -> Result<AssetPrice, FinError> {
            Err(FinError::validation(
                "Synthetic price read must not be called.",
            ))
        }
    }

    fn composite_for_no_submission_restart() -> HyperliquidEthereumDepositAdapter {
        let owner = "0x1111111111111111111111111111111111111111";
        let transport = Arc::new(NoCallTransport);
        let profile = Arc::new(
            HyperliquidProfile::new(
                vec![HyperliquidAccount {
                    name: "synthetic-hl".into(),
                    owner: owner.into(),
                    credential_ref: None,
                    signing_enabled: false,
                    execution_scope: Default::default(),
                }],
                transport.clone(),
                Arc::new(NoCallPrices),
                Arc::new(DeferredExecutionState::default()),
            )
            .unwrap(),
        );
        let ethereum = Arc::new(
            EthereumAdapter::new(
                EthereumConfig::wallet_only(ARBITRUM_ONE, 1, vec![]),
                vec![WalletConnection {
                    name: "synthetic-arbitrum".into(),
                    address: owner.into(),
                    signing_enabled: true,
                }],
                Arc::new(EvmRuntime::new(Arc::new(NoCallRpc), ARBITRUM_ONE, None)),
            )
            .unwrap(),
        );
        HyperliquidEthereumDepositAdapter::new(profile, ethereum, transport)
    }

    fn engine_with_submitted_deposit(
        path: &std::path::Path,
    ) -> (
        Engine,
        Arc<HyperliquidEthereumDepositAdapter>,
        Arc<SyntheticDepositTransport>,
        String,
        String,
    ) {
        let owner = "0x1111111111111111111111111111111111111111".to_owned();
        let source_hash = format!("0x{}", "ab".repeat(32));
        let native_hash = format!("0x{}", "cd".repeat(32));
        let block_hash = format!("0x{}", "ef".repeat(32));
        let transaction = FrozenTransaction {
            chain_id: ARBITRUM_ONE,
            sender: owner.clone(),
            to: ARBITRUM_USDC.into(),
            value: Quantity::zero(),
            calldata: "0xdeadbeef".into(),
            nonce: 7,
            gas: 100_000,
            max_fee_per_gas: Quantity::new("2").unwrap(),
            max_priority_fee_per_gas: Quantity::new("1").unwrap(),
            rollup_fee: None,
        };
        let rpc = Arc::new(SyntheticDepositRpc {
            owner: owner.clone(),
            transaction_hash: source_hash.clone(),
            block_hash,
            transaction: transaction.clone(),
        });
        let runtime = Arc::new(EvmRuntime::new(rpc, ARBITRUM_ONE, None));
        let ethereum = Arc::new(
            EthereumAdapter::new(
                EthereumConfig::wallet_only(ARBITRUM_ONE, 1, vec![]),
                vec![WalletConnection {
                    name: "synthetic-arbitrum".into(),
                    address: owner.clone(),
                    signing_enabled: false,
                }],
                runtime,
            )
            .unwrap(),
        );
        let transport = Arc::new(SyntheticDepositTransport {
            native_hash,
            ledger_hash_override: std::sync::Mutex::new(None),
            ledger_time_override: std::sync::atomic::AtomicU64::new(0),
            duplicate_credit_rows: std::sync::atomic::AtomicBool::new(false),
            explorer_calls: AtomicUsize::new(0),
            action_source_hash: std::sync::Mutex::new(format!("0x{}", "11".repeat(32))),
            credit_visible: std::sync::atomic::AtomicBool::new(false),
            ledger_time: std::sync::atomic::AtomicU64::new(0),
            dispatches: AtomicUsize::new(0),
            role: std::sync::Mutex::new(json!({"role":"user"})),
            abstraction: std::sync::Mutex::new(json!({"abstraction":"disabled"})),
            dex_abstraction: std::sync::Mutex::new(json!(false)),
        });
        let account = HyperliquidAccount {
            name: "synthetic-hl".into(),
            owner: owner.clone(),
            credential_ref: None,
            signing_enabled: false,
            execution_scope: Default::default(),
        };
        let profile = Arc::new(
            HyperliquidProfile::new(
                vec![account.clone()],
                transport.clone(),
                Arc::new(SyntheticPrices),
                Arc::new(DeferredExecutionState::default()),
            )
            .unwrap(),
        );
        let adapter = Arc::new(HyperliquidEthereumDepositAdapter::new(
            profile.clone(),
            ethereum,
            transport.clone(),
        ));

        let source_asset = HyperliquidEthereumDepositAdapter::source_asset();
        let source_resource = ResourceId(format!("ethereum:{ARBITRUM_ONE}:{owner}"));
        let target_resource = account.resource("perpetual", None);
        let amount = Quantity::from_decimal("5", 6).unwrap();
        let native_asset = AssetId {
            network: format!("ethereum:{ARBITRUM_ONE}"),
            reference: "native".into(),
            decimals: 18,
        };
        let inner_input = json!({
            "wallet":"synthetic-arbitrum",
            "network":"evm",
            "chain_id":ARBITRUM_ONE,
            "request_id":"synthetic-deposit",
            "max_fee_eth":"0.0001",
            "token":ARBITRUM_USDC,
            "amount":"5",
            "recipient":HYPERLIQUID_BRIDGE2,
        });
        let bridge_resource = ResourceId(format!("ethereum:{ARBITRUM_ONE}:{HYPERLIQUID_BRIDGE2}"));
        let maximum_gas = Quantity::from_decimal("0.0001", 18).unwrap();
        let inner_intent = FinancialIntent {
            debits: vec![ResourceAmount {
                resource: source_resource.clone(),
                amount: Amount {
                    asset: source_asset.clone(),
                    atoms: amount.clone(),
                },
            }],
            maximum_costs: vec![CostBound {
                location: AccountingLocation::Cash {
                    resource: source_resource.clone(),
                },
                amount: Amount {
                    asset: native_asset,
                    atoms: maximum_gas,
                },
                treatment: CostTreatment::AdditionalDebit,
            }],
            expected: vec![EffectConstraint::Transfer {
                from: source_resource.clone(),
                to: bridge_resource,
                debit_asset: source_asset.clone(),
                credit_asset: source_asset.clone(),
                maximum_debit: amount.clone(),
                minimum_credit: amount.clone(),
            }],
            locks: vec![source_resource.clone()],
        };
        let inner_preview = PreparedOperation {
            operation: "wallet.transfer".into(),
            input: inner_input.clone(),
            binding: json!({
                "transaction":transaction,
                "resource":source_resource,
                "token":source_asset,
                "units":amount,
                "recipient":HYPERLIQUID_BRIDGE2,
                "spender":null,
                "output":null,
                "minimum_output":null,
                "deadline":null,
                "pool":null,
                "fee_tier":null,
                "allowance_before":null,
                "supplied_before":null,
                "debt_before":null,
                "collateral_before":null,
                "enabled":null,
                "a_token":null,
                "debt_token":null,
                "scaled_before":null,
                "repay_all":false,
                "all_amount":false,
                "supply_anchor":null,
                "debt_anchor":null,
                "debt_rounding_maximum":null,
            }),
            summary: "Synthetic reviewed Arbitrum USDC transfer".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: now(),
            expires_at: now(),
            valuation: None,
            intent: inner_intent,
            operation_version: 1,
        };
        let deposit_input = json!({
            "requestId":"synthetic-deposit",
            "account":"synthetic-hl",
            "wallet":"synthetic-arbitrum",
            "amount":"5",
            "maxFeeEth":"0.0001",
            "maxAmountUsdc":"5",
        });
        let intent = HyperliquidEthereumDepositAdapter::intent(
            &inner_preview,
            &amount,
            &source_asset,
            &target_resource,
        )
        .unwrap();
        let binding = DepositBinding {
            inner_input: inner_input.clone(),
            inner_preview: inner_preview.clone(),
            account: "synthetic-hl".into(),
            owner: owner.clone(),
            source_wallet: "synthetic-arbitrum".into(),
            amount: amount.clone(),
            source_asset,
            source_resource,
            target_resource,
            account_mode: Some(AccountModeState {
                owner: owner.clone(),
                execution_address: owner.clone(),
                native_abstraction: "disabled".into(),
                mode: AccountMode::Separate,
                dex_abstraction: Some(false),
            }),
            deployment: Some(DepositDeployment {
                bridge: HYPERLIQUID_BRIDGE2.into(),
                token: ARBITRUM_USDC.into(),
                bridge_code_hash: format!("0x{}", "22".repeat(32)),
            }),
            usd_ceiling: None,
        };
        let prepared = PreparedOperation {
            operation: "hyperliquid.account.deposit".into(),
            input: deposit_input.clone(),
            binding: serde_json::to_value(&binding).unwrap(),
            summary: "Synthetic Bridge2 deposit".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: now(),
            expires_at: now(),
            valuation: None,
            intent,
            operation_version: 1,
        };
        let source_submission = Submission {
            kind: "ethereum".into(),
            reference: source_hash.clone(),
            data: json!({
                "transaction":transaction,
                "metadata":{"operation":"wallet.transfer","resource":binding.source_resource},
            }),
        };
        let operation_id = "synthetic-deposit-operation".to_owned();
        let mut record = OperationRecord {
            id: operation_id.clone(),
            request_id: "synthetic-deposit".into(),
            fingerprint: "synthetic-deposit-fingerprint".into(),
            fingerprint_version: 1,
            operation: "hyperliquid.account.deposit".into(),
            input: deposit_input,
            status: Status::Pending,
            created_at: now(),
            updated_at: now(),
            preview: Some(prepared.clone()),
            approval: None,
            submission: Some(source_submission.clone()),
            dispatch_evidence: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        record.approval = Some(ApprovalAudit {
            mechanism: "synthetic test approval".into(),
            binding_hash: binding_hash(&authorization_binding(&prepared)).unwrap(),
            challenge_nonce: "synthetic-deposit-nonce".into(),
            approved_at: now(),
            expires_at: now(),
        });
        let mut store = Store::open(path).unwrap();
        store
            .create_request(
                &record.id,
                &record.request_id,
                &record.fingerprint,
                &record.operation,
                None,
                "pending",
                &serde_json::to_value(&record).unwrap(),
                &record.created_at,
            )
            .unwrap();
        store
            .persist_submission(
                &record.id,
                &SubmissionRow {
                    kind: source_submission.kind,
                    reference: source_submission.reference,
                    data: source_submission.data,
                },
                &record.updated_at,
            )
            .unwrap();
        let engine = Engine::new(
            store,
            vec![adapter.clone()],
            Arc::new(SyntheticAuthorizer),
            EngineOptions::default(),
        )
        .unwrap();
        (engine, adapter, transport, operation_id, source_hash)
    }

    #[tokio::test]
    async fn deposit_destination_mode_requires_explicit_supported_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let (_, adapter, transport, _, _) =
            engine_with_submitted_deposit(&dir.path().join("mode.sqlite"));
        let account = adapter.profile.account("synthetic-hl").unwrap();

        *transport.role.lock().unwrap() = json!({"role":"user"});
        *transport.abstraction.lock().unwrap() = json!({"abstraction":"unifiedAccount"});
        let unified = adapter.deposit_destination_mode(account).await.unwrap();
        assert_eq!(unified.mode, AccountMode::Unified);
        assert_eq!(
            unified
                .cash_resource(account, CashWallet::Perpetual, None)
                .unwrap(),
            account.resource("unified", None)
        );

        *transport.role.lock().unwrap() = json!({"role":"missing"});
        *transport.abstraction.lock().unwrap() = json!({"abstraction":"default"});
        *transport.dex_abstraction.lock().unwrap() = Value::Null;
        let new_owner = adapter.deposit_destination_mode(account).await.unwrap();
        assert_eq!(new_owner.mode, AccountMode::Separate);
        assert_eq!(new_owner.dex_abstraction, None);
        assert_eq!(
            new_owner
                .cash_resource(account, CashWallet::Perpetual, None)
                .unwrap(),
            account.resource("perpetual", None)
        );

        *transport.abstraction.lock().unwrap() = Value::Null;
        assert!(adapter.deposit_destination_mode(account).await.is_err());
        assert_eq!(transport.dispatches.load(Ordering::SeqCst), 0);
        *transport.abstraction.lock().unwrap() = json!({"abstraction":"portfolioMargin"});
        assert!(adapter.deposit_destination_mode(account).await.is_err());
        *transport.role.lock().unwrap() = json!({"role":"agent","data":{"master":account.owner}});
        *transport.abstraction.lock().unwrap() = json!({"abstraction":"default"});
        assert!(adapter.deposit_destination_mode(account).await.is_err());
    }

    #[tokio::test]
    async fn engine_persists_source_debit_across_restart_then_credits_only_exact_native_deposit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hyperliquid-deposit-progress.sqlite");
        let (engine, adapter, transport, operation_id, source_hash) =
            engine_with_submitted_deposit(&path);

        let source_only = engine.status(&operation_id, true).await.unwrap();
        assert_eq!(source_only.status, Status::Pending);
        let source_effects = source_only.effects.as_ref().unwrap();
        assert!(source_effects.effects.iter().any(|effect| matches!(
            effect,
            FinancialEffect::CashDebit { amount, .. } if amount.atoms == Quantity::from_decimal("5", 6).unwrap()
        )));
        assert!(source_effects.effects.iter().any(|effect| matches!(
            effect,
            FinancialEffect::TransitPrincipal { amount, .. } if amount.atoms == Quantity::from_decimal("5", 6).unwrap()
        )));
        assert!(source_effects.effects.iter().any(|effect| matches!(
            effect,
            FinancialEffect::Cost { category, .. } if category == "ethereum_gas"
        )));
        assert_eq!(
            fin_domain::SettlementProgress::from_evidence(&source_effects.evidence)
                .unwrap()
                .unwrap()
                .sequence,
            1
        );
        assert_eq!(transport.dispatches.load(Ordering::SeqCst), 0);
        drop(engine);

        transport.credit_visible.store(true, Ordering::SeqCst);
        let engine = Engine::new(
            Store::open(&path).unwrap(),
            vec![adapter.clone()],
            Arc::new(SyntheticAuthorizer),
            EngineOptions::default(),
        )
        .unwrap();
        let mismatched = engine.status(&operation_id, true).await.unwrap();
        assert_eq!(mismatched.status, Status::Pending);
        assert_eq!(mismatched.effects.as_ref(), Some(source_effects));
        assert_eq!(transport.dispatches.load(Ordering::SeqCst), 0);

        *transport.action_source_hash.lock().unwrap() = source_hash.clone();
        *transport.abstraction.lock().unwrap() = json!({"abstraction":"unifiedAccount"});
        let changed_mode = engine.status(&operation_id, true).await.unwrap();
        assert_eq!(changed_mode.status, Status::Pending);
        let changed_mode_effects = &changed_mode.effects.as_ref().unwrap().effects;
        assert!(changed_mode_effects.iter().any(|effect| matches!(
            effect,
            FinancialEffect::TransitPrincipal { amount, .. }
                if amount.atoms == Quantity::from_decimal("5", 6).unwrap()
        )));
        assert!(
            !changed_mode_effects
                .iter()
                .any(|effect| matches!(effect, FinancialEffect::Transfer { .. }))
        );

        *transport.abstraction.lock().unwrap() = json!({"abstraction":"disabled"});
        let completed = engine.status(&operation_id, true).await.unwrap();
        assert_eq!(
            completed.status,
            Status::Completed,
            "reconciliation evidence: {:?}",
            completed.last_reconciliation_evidence
        );
        let terminal_effects = &completed.effects.as_ref().unwrap().effects;
        assert!(terminal_effects.iter().any(|effect| matches!(
            effect,
            FinancialEffect::Transfer { debit, credit, .. }
                if debit.atoms == Quantity::from_decimal("5", 6).unwrap()
                    && credit.atoms == Quantity::from_decimal("5", 36).unwrap()
                    && credit.asset == hl_usdc()
        )));
        assert!(
            !terminal_effects
                .iter()
                .any(|effect| matches!(effect, FinancialEffect::TransitPrincipal { .. }))
        );
        assert_eq!(
            terminal_effects
                .iter()
                .filter(|effect| matches!(effect, FinancialEffect::Cost { category, .. } if category == "ethereum_gas"))
                .count(),
            1,
            "source gas is carried forward once, not re-applied with destination credit"
        );
        let progress = fin_domain::SettlementProgress::from_evidence(
            &completed.effects.as_ref().unwrap().evidence,
        )
        .unwrap()
        .unwrap();
        assert_eq!(progress.sequence, 2);
        assert!(
            progress
                .event_digests
                .contains_key(&format!("source:{source_hash}"))
        );
        assert!(
            progress
                .event_digests
                .contains_key(&format!("destination:{}", transport.native_hash))
        );
        assert_eq!(transport.dispatches.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn earlier_source_hash_credit_survives_later_reconciliation_and_duplicates_stay_pending()
    {
        let directory = tempfile::tempdir().unwrap();
        let path = directory
            .path()
            .join("hyperliquid-deposit-early-credit.sqlite");
        let (engine, adapter, transport, operation_id, source_hash) =
            engine_with_submitted_deposit(&path);

        let source_only = engine.status(&operation_id, true).await.unwrap();
        assert_eq!(source_only.status, Status::Pending);
        drop(engine);

        let persisted = Store::open(&path)
            .unwrap()
            .get_record(&operation_id)
            .unwrap()
            .unwrap();
        let record: OperationRecord = serde_json::from_value(persisted).unwrap();
        let created_ms = chrono::DateTime::parse_from_rfc3339(&record.created_at)
            .unwrap()
            .timestamp_millis() as u64;
        let earlier_credit_time_ms = created_ms + 1;
        transport.credit_visible.store(true, Ordering::SeqCst);
        *transport.ledger_hash_override.lock().unwrap() = Some(source_hash.clone());
        transport
            .ledger_time_override
            .store(earlier_credit_time_ms, Ordering::SeqCst);
        transport
            .duplicate_credit_rows
            .store(true, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(chrono::Utc::now().timestamp_millis() as u64 > earlier_credit_time_ms);

        let engine = Engine::new(
            Store::open(&path).unwrap(),
            vec![adapter.clone()],
            Arc::new(SyntheticAuthorizer),
            EngineOptions::default(),
        )
        .unwrap();
        let duplicate = engine.status(&operation_id, true).await.unwrap();
        assert_eq!(duplicate.status, Status::Pending);
        assert!(duplicate.effects.as_ref().unwrap().effects.iter().any(|effect| {
            matches!(effect, FinancialEffect::TransitPrincipal { transfer_id, .. } if transfer_id == &source_hash)
        }));
        assert_eq!(transport.explorer_calls.load(Ordering::SeqCst), 0);

        transport
            .duplicate_credit_rows
            .store(false, Ordering::SeqCst);
        let completed = engine.status(&operation_id, true).await.unwrap();
        assert_eq!(
            completed.status,
            Status::Completed,
            "reconciliation evidence: {:?}",
            completed.last_reconciliation_evidence
        );
        let evidence = &completed.effects.as_ref().unwrap().evidence;
        assert_eq!(evidence["hyperliquidCredit"]["ledger"]["hash"], source_hash);
        assert_eq!(
            evidence["correlation"]["method"],
            "owner_scoped_source_hash_ledger_row"
        );
        assert_eq!(transport.explorer_calls.load(Ordering::SeqCst), 0);
        assert_eq!(transport.dispatches.load(Ordering::SeqCst), 0);
    }

    fn rows() -> Vec<Value> {
        vec![
            json!({"time":1200,"hash":"0xsource","delta":{"type":"deposit","usdc":"5.000000"}}),
            json!({"time":1300,"hash":"0xother","delta":{"type":"deposit","usdc":"5"}}),
            json!({"time":1400,"hash":"0xsource","delta":{"type":"deposit","usdc":"5.1"}}),
            json!({"time":1500,"hash":"0xsource","delta":{"type":"withdraw","usdc":"5"}}),
        ]
    }

    #[test]
    fn route_is_fixed_to_canonical_arbitrum_usdc_bridge2_and_minimum() {
        assert_eq!(ARBITRUM_ONE, 42161);
        assert_eq!(ARBITRUM_USDC, "0xaf88d065e77c8cc2239327c5edb3a432268e5831");
        assert_eq!(
            HYPERLIQUID_BRIDGE2,
            "0x2df1c51e09aecf9cacb7bc98cb1742757f163df7"
        );
        assert_eq!(
            Quantity::from_decimal(MINIMUM_USDC, 6)
                .unwrap()
                .to_decimal(6),
            "5"
        );
    }

    #[test]
    fn source_wallet_must_be_signing_enabled_on_arbitrum_and_equal_owner() {
        let owner = "0x1111111111111111111111111111111111111111";
        assert!(
            HyperliquidEthereumDepositAdapter::source_wallet_matches_owner(
                ARBITRUM_ONE,
                true,
                Some(owner),
                owner
            )
        );
        assert!(
            HyperliquidEthereumDepositAdapter::source_wallet_matches_owner(
                ARBITRUM_ONE,
                true,
                Some("0x1111111111111111111111111111111111111111"),
                "0x1111111111111111111111111111111111111111"
            )
        );
        assert!(
            !HyperliquidEthereumDepositAdapter::source_wallet_matches_owner(
                ARBITRUM_ONE,
                true,
                Some("0x2222222222222222222222222222222222222222"),
                owner
            )
        );
        assert!(
            !HyperliquidEthereumDepositAdapter::source_wallet_matches_owner(
                1,
                true,
                Some(owner),
                owner
            )
        );
        assert!(
            !HyperliquidEthereumDepositAdapter::source_wallet_matches_owner(
                ARBITRUM_ONE,
                false,
                Some(owner),
                owner
            )
        );
    }

    #[test]
    fn exact_decimal_comparison_does_not_use_float_or_balance_delta() {
        let target = Quantity::from_decimal("5", 6).unwrap();
        let exact = Quantity::from_decimal("5.000000", 6).unwrap();
        let too_large = Quantity::from_decimal("5.000001", 6).unwrap();
        assert_eq!(target, exact);
        assert_ne!(target, too_large);
        assert_eq!(
            rows()
                .iter()
                .filter(|row| row["delta"]["type"] == "deposit")
                .count(),
            3
        );
    }

    #[test]
    fn same_amount_unrelated_hash_is_only_a_candidate_never_a_correlation() {
        let input = rows();
        let amount = Quantity::from_decimal("5", 6).unwrap();
        let candidates = ledger_candidates(&input, &amount, 1100);
        assert_eq!(candidates.len(), 2);

        let too_early = ledger_candidates(&input, &amount, 1400);
        assert!(too_early.is_empty());
    }

    #[test]
    fn persisted_creation_time_includes_a_credit_that_predates_reconciliation() {
        let created_at = "2026-10-10T10:00:00.000Z";
        let preview_at = "2026-10-10T10:00:01.000Z";
        let later_reconciliation_at = "2026-10-10T10:05:00.000Z";
        let start_ms = source_search_start_ms(created_at, preview_at);
        let credit_time_ms = chrono::DateTime::parse_from_rfc3339("2026-10-10T10:02:00.000Z")
            .unwrap()
            .timestamp_millis() as u64;
        let reconciliation_ms = chrono::DateTime::parse_from_rfc3339(later_reconciliation_at)
            .unwrap()
            .timestamp_millis() as u64;
        assert!(credit_time_ms >= start_ms);
        assert!(credit_time_ms < reconciliation_ms);

        let rows = vec![json!({
            "time":credit_time_ms,
            "hash":format!("0x{}", "ab".repeat(32)),
            "delta":{"type":"deposit","usdc":"5.000000"}
        })];
        assert_eq!(
            ledger_candidates(&rows, &Quantity::from_decimal("5", 6).unwrap(), start_ms).len(),
            1
        );
        assert!(
            ledger_candidates(
                &rows,
                &Quantity::from_decimal("5", 6).unwrap(),
                reconciliation_ms,
            )
            .is_empty()
        );
    }

    #[test]
    fn source_hash_ledger_link_requires_one_exact_256_bit_hash() {
        let source_hash = format!("0x{}", "ab".repeat(32));
        assert!(source_hash_ledger_match(
            &json!({"hash":format!("0x{}", source_hash[2..].to_ascii_uppercase())}),
            &source_hash,
        ));
        assert!(!source_hash_ledger_match(
            &json!({"hash":format!("0x{}", "cd".repeat(32))}),
            &source_hash,
        ));
        assert!(!source_hash_ledger_match(
            &json!({"hash":"0xsource"}),
            &source_hash,
        ));
    }

    #[test]
    fn explorer_deposit_vote_must_embed_exact_source_hash_owner_and_raw_amount() {
        let native_hash = format!("0x{}", "aa".repeat(32));
        let source_hash = format!("0x{}", "bb".repeat(32));
        let owner = "0x1111111111111111111111111111111111111111";
        let expected = Quantity::from_decimal("5", 6).unwrap();
        let response = json!({
            "type":"txDetails",
            "tx":{
                "hash":native_hash,
                "user":"0x3333333333333333333333333333333333333333",
                "time":1200,
                "block":42,
                "error":null,
                "action":{"type":"voteEthDeposit","user":owner,"usd":5000000,"ethTxHash":source_hash}
            }
        });
        assert!(exact_deposit_action(
            &response,
            &native_hash,
            owner,
            &expected,
            &source_hash,
            1100,
            1200,
        ));
        let mut bad_source = response.clone();
        bad_source["tx"]["action"]["ethTxHash"] = json!(format!("0x{}", "cc".repeat(32)));
        assert!(!exact_deposit_action(
            &bad_source,
            &native_hash,
            owner,
            &expected,
            &source_hash,
            1100,
            1200,
        ));
        let mut bad_amount = response.clone();
        bad_amount["tx"]["action"]["usd"] = json!(5);
        assert!(!exact_deposit_action(
            &bad_amount,
            &native_hash,
            owner,
            &expected,
            &source_hash,
            1100,
            1200,
        ));
        let mut failed_vote = response.clone();
        failed_vote["tx"]["error"] = json!("rejected");
        assert!(!exact_deposit_action(
            &failed_vote,
            &native_hash,
            owner,
            &expected,
            &source_hash,
            1100,
            1200,
        ));
        assert!(!exact_deposit_action(
            &response,
            &native_hash,
            owner,
            &expected,
            &source_hash,
            1100,
            1201,
        ));
    }

    #[test]
    fn delayed_credit_appears_only_after_ledger_row_arrives_and_duplicates_are_ambiguous() {
        let amount = Quantity::from_decimal("5", 6).unwrap();
        let mut history = vec![];
        assert!(ledger_candidates(&history, &amount, 1000).is_empty());
        history.push(json!({"time":1200,"hash":"0xsource","delta":{"type":"deposit","usdc":"5"}}));
        assert_eq!(ledger_candidates(&history, &amount, 1000).len(), 1);
        history.push(
            json!({"time":1201,"hash":"0xsource","delta":{"type":"deposit","usdc":"5.000000"}}),
        );
        assert_eq!(ledger_candidates(&history, &amount, 1000).len(), 2);
    }

    #[test]
    fn canonical_source_receipt_becomes_idempotent_debit_and_nonspendable_transit_progress() {
        let source_asset = HyperliquidEthereumDepositAdapter::source_asset();
        let amount = Quantity::from_decimal("5", 6).unwrap();
        let source_resource =
            ResourceId("ethereum:42161:0x1111111111111111111111111111111111111111".into());
        let target_resource =
            ResourceId("hyperliquid:0x1111111111111111111111111111111111111111:perpetual".into());
        let bridge_resource = ResourceId(format!("ethereum:{ARBITRUM_ONE}:{HYPERLIQUID_BRIDGE2}"));
        let submission = Submission {
            kind: "ethereum_tx".into(),
            reference: format!("0x{}", "ab".repeat(32)),
            data: json!({"nonce":"synthetic-7"}),
        };
        let mut record = OperationRecord {
            id: "synthetic-deposit-progress".into(),
            request_id: "synthetic-deposit-progress".into(),
            fingerprint: "synthetic".into(),
            fingerprint_version: 1,
            operation: "hyperliquid.account.deposit".into(),
            input: json!({}),
            status: Status::Pending,
            created_at: now(),
            updated_at: now(),
            preview: None,
            approval: None,
            submission: Some(submission.clone()),
            dispatch_evidence: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let binding = DepositBinding {
            inner_input: json!({}),
            inner_preview: PreparedOperation {
                operation: "wallet.transfer".into(),
                input: json!({}),
                binding: json!({}),
                summary: String::new(),
                checks: vec![],
                warnings: vec![],
                observed_at: now(),
                expires_at: now(),
                valuation: None,
                intent: FinancialIntent {
                    debits: vec![],
                    maximum_costs: vec![],
                    expected: vec![],
                    locks: vec![],
                },
                operation_version: 1,
            },
            account: "synthetic-hl".into(),
            owner: "0x1111111111111111111111111111111111111111".into(),
            source_wallet: "synthetic-arbitrum".into(),
            amount: amount.clone(),
            source_asset: source_asset.clone(),
            source_resource: source_resource.clone(),
            target_resource: target_resource.clone(),
            account_mode: None,
            deployment: None,
            usd_ceiling: None,
        };
        let mut source = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects: vec![
                FinancialEffect::Cost {
                    location: fin_domain::AccountingLocation::Cash {
                        resource: source_resource.clone(),
                    },
                    amount: Amount {
                        asset: AssetId {
                            network: "ethereum:42161".into(),
                            reference: "native".into(),
                            decimals: 18,
                        },
                        atoms: Quantity::new("1234").unwrap(),
                    },
                    category: "ethereum_gas".into(),
                    treatment: fin_domain::CostTreatment::AdditionalDebit,
                },
                FinancialEffect::Transfer {
                    from: source_resource.clone(),
                    to: bridge_resource,
                    debit: Amount {
                        asset: source_asset.clone(),
                        atoms: amount.clone(),
                    },
                    credit: Amount {
                        asset: source_asset.clone(),
                        atoms: amount.clone(),
                    },
                },
            ],
            evidence: json!({
                "original_hash":submission.reference,
                "success":true,
                "block_number":42,
                "block_hash":format!("0x{}", "cd".repeat(32)),
                "fee_wei":"1234",
            }),
            observed_at: "2026-01-01T00:00:00Z".into(),
            proof_digest: String::new(),
        };
        source.proof_digest = verified_effect_digest(&source).unwrap();

        let first = source_progress_bundle(
            &record,
            &binding,
            &submission,
            source.clone(),
            json!({"reason":"destination credit not indexed yet"}),
        )
        .unwrap();
        assert_eq!(first.effects.len(), 3);
        assert!(
            matches!(first.effects[0], FinancialEffect::CashDebit { ref resource, amount: ref actual } if resource == &source_resource && actual.atoms == amount)
        );
        assert!(
            matches!(first.effects[1], FinancialEffect::TransitPrincipal { ref transfer_id, amount: ref actual } if transfer_id == &submission.reference && actual.atoms == amount)
        );
        assert!(
            matches!(first.effects[2], FinancialEffect::Cost { ref amount, .. } if amount.atoms == Quantity::new("1234").unwrap())
        );
        assert_eq!(
            first.evidence["transit"]["recipient_resource"],
            target_resource.0
        );
        assert_eq!(
            fin_domain::SettlementProgress::from_evidence(&first.evidence)
                .unwrap()
                .unwrap()
                .sequence,
            1
        );

        record.effects = Some(first.clone());
        let repeated = source_progress_bundle(
            &record,
            &binding,
            &submission,
            source,
            json!({"reason":"restart and retry"}),
        )
        .unwrap();
        assert_eq!(repeated, first);
    }

    #[test]
    fn deposit_definition_describes_exact_source_hash_correlation() {
        let def = HyperliquidEthereumDepositAdapter::operation_definition();
        assert!(
            def.semantics["hashSemantics"]
                .as_str()
                .unwrap()
                .contains("VoteEthDepositAction")
        );
        assert!(
            def.semantics["completion"]
                .as_str()
                .unwrap()
                .contains("VoteEthDeposit")
        );
    }

    #[tokio::test]
    async fn restart_without_original_submission_remains_unknown_and_never_calls_venue_or_chain() {
        let adapter = composite_for_no_submission_restart();
        let record = OperationRecord {
            id: "synthetic-deposit".into(),
            request_id: "synthetic".into(),
            fingerprint: "synthetic-fingerprint".into(),
            fingerprint_version: 1,
            operation: "hyperliquid.account.deposit".into(),
            input: json!({"requestId":"synthetic","account":"synthetic-hl","wallet":"synthetic-arbitrum","amount":"5","maxFeeEth":"0.0001","maxAmountUsdc":"5"}),
            status: Status::Unknown,
            created_at: now(),
            updated_at: now(),
            preview: None,
            approval: None,
            submission: None,
            dispatch_evidence: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let result = adapter.reconcile(&record).await.unwrap();
        assert_eq!(result.status, Status::Unknown);
        assert!(result.verified.is_none());
        assert!(
            result.evidence["reason"]
                .as_str()
                .unwrap()
                .contains("submission identity")
        );
    }
}
