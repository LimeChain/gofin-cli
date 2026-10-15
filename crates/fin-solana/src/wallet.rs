use std::sync::Arc;

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    CLUSTER, NETWORK, failure,
    key::SolanaSigner,
    parse_address,
    rpc::SolanaRpc,
    token::{
        TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, decode_rpc_account_data, parse_mint,
        parse_token_account, required_token_account_len,
    },
    transaction::NormalizedInstruction,
    transaction::SolanaRuntime,
};
use fin_domain::{
    AccountingLocation, Adapter, Amount, AssetId, AuthorityState, Check, CheckStatus,
    ConnectionMetadata, CostBound, CostTreatment, Definition, EffectConstraint, ExecutionActor,
    FinError, FinancialEffect, FinancialIntent, FinancialState, OperationKind, OperationRecord,
    PositionAnchor, PositionState, PreparedOperation, Reconciliation, ResourceAmount, ResourceId,
    Result, SnapshotScope, StateSnapshot, Status, Submission, SubmissionJournal, VerifiedEffects,
};
use fin_pricing::UsdPriceSource;

const MAX_MULTIPLE_ACCOUNTS_ADDRESSES: usize = 100;

struct WalletBalanceObservation {
    balances: Value,
    accounts: std::collections::BTreeMap<String, (u64, crate::token::TokenAccountInfo)>,
}

/// Read-only wallet observation plus a protected signer/runtime pair usable by
/// protocol adapters. Financial writes remain the responsibility of reviewed
/// operations and must pass through the engine's journal/approval flow.
pub struct SolanaWalletAdapter {
    name: String,
    address: String,
    signer: Option<Arc<SolanaSigner>>,
    runtime: Arc<SolanaRuntime>,
    _prices: Arc<dyn UsdPriceSource>,
}

impl SolanaWalletAdapter {
    pub fn new(
        name: impl Into<String>,
        address: impl AsRef<str>,
        signer: Option<Arc<SolanaSigner>>,
        rpc: Arc<dyn SolanaRpc>,
        prices: Arc<dyn UsdPriceSource>,
    ) -> Result<Self> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(FinError::validation(
                "A non-empty Solana wallet name is required.",
            ));
        }
        let address = parse_address(address.as_ref())?;
        if signer.as_ref().is_some_and(|s| s.address() != address) {
            return Err(failure(
                "SOLANA_SIGNER_ADDRESS_MISMATCH",
                "connection",
                "The protected signer does not match the enrolled wallet address.",
                "Use the signer registered for this exact Solana wallet.",
            ));
        }
        Ok(Self {
            name,
            address,
            signer,
            runtime: Arc::new(SolanaRuntime::new(rpc)),
            _prices: prices,
        })
    }

    pub fn address(&self) -> &str {
        &self.address
    }
    pub fn runtime(&self) -> Arc<SolanaRuntime> {
        self.runtime.clone()
    }
    /// Available only inside the protected broker composition; never serialize it.
    pub fn signer(&self) -> Option<Arc<SolanaSigner>> {
        self.signer.clone()
    }

    /// Dispatch one already prepared wallet transfer under a composite
    /// operation's final revalidation and journal identity. The outer
    /// candidate remains the only authority checked by the durable journal;
    /// the child candidate supplies only the exact frozen Solana transaction.
    /// Callers must bind the child recipient, asset and amount to the outer
    /// candidate before constructing `validated`.
    pub async fn submit_nested_revalidated(
        &self,
        validated: &fin_domain::RevalidatedOperation,
        child: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        validated.assert_fresh()?;
        if child.operation != "wallet.transfer"
            || child.input.get("wallet").and_then(Value::as_str) != Some(self.name.as_str())
            || child.input.get("network").and_then(Value::as_str) != Some("solana")
            || child.input.get("cluster").and_then(Value::as_str) != Some(CLUSTER)
        {
            return Err(FinError::validation(
                "The nested Solana candidate must be a transfer for this enrolled wallet on mainnet-beta.",
            ));
        }
        fin_domain::value_bounds::validate_at(child, chrono::Utc::now())?;
        if child
            .checks
            .iter()
            .any(|check| check.status == fin_domain::CheckStatus::Failed)
        {
            return Err(FinError::validation(
                "The nested Solana transfer candidate has a failed check.",
            ));
        }
        let signer = self.signer.as_ref().ok_or_else(|| {
            failure(
                "SOLANA_WALLET_READ_ONLY",
                "dispatch",
                "This enrolled wallet is read-only.",
                "Enroll a protected Solana signer before dispatch.",
            )
        })?;
        let frozen = self.freeze_prepared(child).await?;
        if frozen.signer() != self.address {
            return Err(failure(
                "SOLANA_SIGNER_BINDING_MISMATCH",
                "dispatch",
                "The nested transaction does not use this enrolled wallet.",
                "Prepare the child transfer again for the selected wallet.",
            ));
        }
        self.runtime.validate_fee(&frozen).await?;
        validated.assert_fresh()?;
        self.runtime
            .dispatch(&frozen, signer, validated.prepared(), journal)
            .await
    }

    pub async fn balances(&self) -> Result<Value> {
        Ok(self.observe_balances().await?.balances)
    }

    async fn observe_balances(&self) -> Result<WalletBalanceObservation> {
        let lamports = self
            .runtime
            .rpc()
            .request(
                "getBalance",
                json!([self.address,{"commitment":"finalized"}]),
            )
            .await?
            .get("value")
            .and_then(Value::as_u64)
            .ok_or_else(crate::parse_error)?;
        let epoch = self
            .runtime
            .rpc()
            .request("getEpochInfo", json!([{"commitment":"finalized"}]))
            .await?
            .get("epoch")
            .and_then(Value::as_u64)
            .ok_or_else(crate::parse_error)?;
        let mut accounts = Vec::new();
        let mut observed_accounts = std::collections::BTreeMap::new();
        for program in [TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID] {
            let response = self.runtime.rpc().request("getTokenAccountsByOwner", json!([self.address,{"programId":program},{"encoding":"base64","commitment":"finalized"}])).await?;
            let entries = response
                .get("value")
                .and_then(Value::as_array)
                .ok_or_else(crate::parse_error)?;
            for entry in entries {
                let address = entry
                    .get("pubkey")
                    .and_then(Value::as_str)
                    .ok_or_else(crate::parse_error)?;
                let account = entry.get("account").ok_or_else(crate::parse_error)?;
                if account.get("owner").and_then(Value::as_str) != Some(program) {
                    return Err(crate::parse_error());
                }
                let data =
                    decode_rpc_account_data(account.get("data").ok_or_else(crate::parse_error)?)?;
                let parsed = parse_token_account(program, address, &data)?;
                if parsed.token_owner != self.address {
                    return Err(crate::parse_error());
                }
                let lamports = account
                    .get("lamports")
                    .and_then(Value::as_u64)
                    .ok_or_else(crate::parse_error)?;
                if observed_accounts
                    .insert(address.to_owned(), (lamports, parsed.clone()))
                    .is_some()
                {
                    return Err(crate::parse_error());
                }
                accounts.push(parsed);
            }
        }
        let mut mints = accounts.iter().map(|a| a.mint.clone()).collect::<Vec<_>>();
        mints.sort();
        mints.dedup();
        let mint_programs = accounts
            .iter()
            .map(|account| (account.mint.clone(), account.program_id.clone()))
            .collect::<std::collections::BTreeMap<_, _>>();
        // A mint is pinned and decoded from binary RPC data; symbols are never
        // used to infer asset identity or valuation.
        let mut decimals = serde_json::Map::new();
        for chunk in mints.chunks(MAX_MULTIPLE_ACCOUNTS_ADDRESSES) {
            let response = self
                .runtime
                .rpc()
                .request(
                    "getMultipleAccounts",
                    json!([chunk,{"encoding":"base64","commitment":"finalized"}]),
                )
                .await?;
            let values = response
                .get("value")
                .and_then(Value::as_array)
                .filter(|values| values.len() == chunk.len())
                .ok_or_else(crate::parse_error)?;
            for (mint, account) in chunk.iter().zip(values) {
                let account = account.as_object().ok_or_else(crate::parse_error)?;
                let owner = account
                    .get("owner")
                    .and_then(Value::as_str)
                    .ok_or_else(crate::parse_error)?;
                if mint_programs
                    .get(mint)
                    .is_some_and(|program| program != owner)
                {
                    return Err(crate::parse_error());
                }
                let data =
                    decode_rpc_account_data(account.get("data").ok_or_else(crate::parse_error)?)?;
                let descriptor = parse_mint(owner, &data, epoch)?;
                decimals.insert(mint.clone(), json!(descriptor.decimals));
            }
        }
        let mut token_accounts = Vec::new();
        for account in accounts {
            let decimals = decimals
                .get(&account.mint)
                .and_then(Value::as_u64)
                .ok_or_else(crate::parse_error)?;
            token_accounts.push(json!({"address":account.address,"asset":{"network":NETWORK,"reference":account.mint,"decimals":decimals},"amount_atoms":account.amount_atoms.to_string(),"state":account.state,"token_owner":account.token_owner,"program_id":account.program_id,"delegate":account.delegate,"delegated_amount_atoms":account.delegated_amount_atoms.to_string(),"close_authority":account.close_authority,"is_native":account.is_native,"rent_exempt_reserve_lamports":account.rent_exempt_reserve_lamports.map(|v|v.to_string()),"withheld_transfer_fee_atoms":account.withheld_transfer_fee_atoms.map(|v|v.to_string()),"memo_required":account.memo_required,"confidential_transfer_approved":account.confidential_transfer_approved,"allow_non_confidential_credits":account.allow_non_confidential_credits,"extensions":account.extensions}));
        }
        Ok(WalletBalanceObservation {
            balances: json!({"network":NETWORK,"cluster":CLUSTER,"wallet":self.address,"observed_commitment":"finalized","native":{"asset":{"network":NETWORK,"reference":"native","decimals":9},"amount_atoms":lamports.to_string()},"token_accounts":token_accounts,"coverage":{"kind":"wallet_owned_spl_accounts","programs":[TOKEN_PROGRAM_ID,TOKEN_2022_PROGRAM_ID],"address_associated_only":false,"complete_for_program_account_queries":true,"portfolio_complete":false,"scope_note":"Other applications may hold positions, debts, or assets outside these wallet-owned token accounts."}}),
            accounts: observed_accounts,
        })
    }

    async fn token(&self, input: &Value) -> Result<Value> {
        let mint = input
            .get("mint")
            .and_then(Value::as_str)
            .ok_or_else(|| FinError::validation("wallet.token requires a base58 mint address."))?;
        let mint = parse_address(mint)?;
        let epoch = self
            .runtime
            .rpc()
            .request("getEpochInfo", json!([{"commitment":"finalized"}]))
            .await?
            .get("epoch")
            .and_then(Value::as_u64)
            .ok_or_else(crate::parse_error)?;
        let response = self
            .runtime
            .rpc()
            .request(
                "getAccountInfo",
                json!([mint,{"encoding":"base64","commitment":"finalized"}]),
            )
            .await?;
        let account = response
            .get("value")
            .filter(|v| !v.is_null())
            .ok_or_else(|| {
                failure(
                    "SOLANA_MINT_NOT_FOUND",
                    "token_read",
                    "The mint account was not found on mainnet-beta.",
                    "Verify the exact mint address and network.",
                )
            })?;
        let owner = account
            .get("owner")
            .and_then(Value::as_str)
            .ok_or_else(crate::parse_error)?;
        let data = decode_rpc_account_data(account.get("data").ok_or_else(crate::parse_error)?)?;
        let descriptor = parse_mint(owner, &data, epoch)?;
        let mut descriptor = serde_json::to_value(descriptor).map_err(|_| crate::parse_error())?;
        descriptor["supply_atoms"] = json!(
            descriptor["supply_atoms"]
                .as_u64()
                .ok_or_else(crate::parse_error)?
                .to_string()
        );
        if let Some(maximum_fee) = descriptor
            .get("transfer_fee")
            .and_then(Value::as_object)
            .and_then(|fee| fee.get("maximum_fee_atoms"))
            .and_then(Value::as_u64)
        {
            descriptor["transfer_fee"]["maximum_fee_atoms"] = json!(maximum_fee.to_string());
        }
        Ok(
            json!({"network":NETWORK,"cluster":CLUSTER,"mint":mint,"observed_commitment":"finalized","descriptor":descriptor}),
        )
    }

    async fn prepare_native_transfer(&self, input: &Value) -> Result<PreparedOperation> {
        let signer = self.signer.as_ref().ok_or_else(|| {
            failure(
                "SOLANA_WALLET_READ_ONLY",
                "prepare",
                "This enrolled wallet is read-only.",
                "Enroll a protected Solana signer before requesting a transfer.",
            )
        })?;
        if input.get("wallet").and_then(Value::as_str) != Some(self.name.as_str())
            || input.get("network").and_then(Value::as_str) != Some("solana")
            || input.get("cluster").and_then(Value::as_str) != Some(CLUSTER)
        {
            return Err(FinError::validation(
                "Select this enrolled wallet and the Solana mainnet-beta cluster.",
            ));
        }
        let recipient = parse_address(input.get("recipient").and_then(Value::as_str).ok_or_else(
            || FinError::validation("wallet.transfer requires a recipient address."),
        )?)?;
        if recipient == self.address {
            return Err(FinError::validation(
                "A native transfer recipient must differ from the enrolled wallet.",
            ));
        }
        let request_id = input
            .get("requestId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 128)
            .ok_or_else(|| FinError::validation("wallet.transfer requires a stable requestId."))?;
        let amount = fin_domain::Quantity::from_decimal(
            input.get("amount").and_then(Value::as_str).ok_or_else(|| {
                FinError::validation("wallet.transfer requires an exact decimal amount.")
            })?,
            9,
        )?;
        let lamports = amount
            .as_str()
            .parse::<u64>()
            .map_err(|_| FinError::validation("SOL amount exceeds the exact u64 lamport range."))?;
        if lamports == 0 {
            return Err(FinError::validation(
                "The transfer amount must be positive.",
            ));
        }
        let max_fee = fin_domain::Quantity::from_decimal(
            input
                .get("max_fee_sol")
                .and_then(Value::as_str)
                .ok_or_else(|| FinError::validation("wallet.transfer requires max_fee_sol."))?,
            9,
        )?;
        let max_fee_lamports = max_fee.as_str().parse::<u64>().map_err(|_| {
            FinError::validation("Maximum fee exceeds the exact u64 lamport range.")
        })?;
        let payer = self
            .address
            .parse::<solana_address::Address>()
            .map_err(|_| crate::parse_error())?;
        let destination = recipient
            .parse::<solana_address::Address>()
            .map_err(|_| crate::parse_error())?;
        let mut instruction_data = 2u32.to_le_bytes().to_vec();
        instruction_data.extend_from_slice(&lamports.to_le_bytes());
        let instruction = solana_instruction::Instruction {
            program_id: "11111111111111111111111111111111"
                .parse()
                .map_err(|_| crate::parse_error())?,
            accounts: vec![
                solana_instruction::AccountMeta::new(payer, true),
                solana_instruction::AccountMeta::new(destination, false),
            ],
            data: instruction_data,
        };
        let normalized = normalize_instruction(instruction);
        let latest = self
            .runtime
            .rpc()
            .request("getLatestBlockhash", json!([{"commitment":"confirmed"}]))
            .await?;
        let value = latest.get("value").ok_or_else(crate::parse_error)?;
        let blockhash = value
            .get("blockhash")
            .and_then(Value::as_str)
            .ok_or_else(crate::parse_error)?;
        let last_valid = value
            .get("lastValidBlockHeight")
            .and_then(Value::as_u64)
            .ok_or_else(crate::parse_error)?;
        let frozen = self
            .runtime
            .freeze_instructions(
                std::slice::from_ref(&normalized),
                &[],
                blockhash,
                signer,
                last_valid,
                max_fee_lamports,
                Some(request_id.into()),
                &[],
            )
            .await?;
        let fee = self.runtime.validate_fee(&frozen).await?;
        self.runtime.simulate(&frozen).await?;
        let balance = self
            .runtime
            .rpc()
            .request(
                "getBalance",
                json!([self.address,{"commitment":"finalized"}]),
            )
            .await?
            .get("value")
            .and_then(Value::as_u64)
            .ok_or_else(crate::parse_error)?;
        if lamports
            .checked_add(fee)
            .is_none_or(|required| balance < required)
        {
            return Err(failure(
                "SOLANA_INSUFFICIENT_SOL",
                "prepare",
                "The wallet does not have enough SOL for the exact transfer and estimated network fee.",
                "Choose a lower amount or fund the enrolled wallet.",
            ));
        }
        let native = AssetId {
            network: NETWORK.into(),
            reference: "native".into(),
            decimals: 9,
        };
        let quote = self._prices.quote(&native).await?;
        ensure_fresh_quote(&quote)?;
        let unit_atoms = fin_domain::Quantity::new("1000000000")?;
        let price = fin_domain::Quantity::from_decimal(&quote.usd_per_unit, 18)?.mul_div_floor(
            &fin_domain::Quantity::from_u64(1),
            &fin_domain::Quantity::new("1000000000000")?,
        )?;
        let amount_usd = amount.mul_div_ceil(&price, &unit_atoms)?;
        let max_fee_usd = max_fee.mul_div_ceil(&price, &unit_atoms)?;
        let resource = ResourceId(format!("solana:mainnet-beta:{}", self.address));
        let recipient_resource = ResourceId(format!("solana:mainnet-beta:{recipient}"));
        let intent = FinancialIntent {
            debits: vec![ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: native.clone(),
                    atoms: amount.clone(),
                },
            }],
            maximum_costs: vec![CostBound {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: Amount {
                    asset: native.clone(),
                    atoms: max_fee.clone(),
                },
                treatment: CostTreatment::AdditionalDebit,
            }],
            expected: vec![EffectConstraint::Transfer {
                from: resource.clone(),
                to: recipient_resource,
                debit_asset: native.clone(),
                credit_asset: native.clone(),
                maximum_debit: amount.clone(),
                minimum_credit: amount.clone(),
            }],
            locks: vec![resource],
        };
        let valuation = fin_domain::Valuation {
            movement_usd_micros: amount_usd.clone(),
            maximum_fee_usd_micros: max_fee_usd.clone(),
            additional_allocation_usd_micros: fin_domain::Quantity::from_u64(0),
            observed_at: quote.observed_at.clone(),
            source: quote.source.clone(),
            prices: vec![fin_domain::AssetPrice {
                asset: native,
                usd_micros_per_unit: price,
                unit_atoms,
                observed_at: quote.observed_at,
                source: quote.source,
            }],
        };
        let mut prepared = PreparedOperation {
            operation: "wallet.transfer".into(),
            input: input.clone(),
            binding: json!({"message_sha256":frozen.message_sha256(),"instructions":[normalized],"blockhash":frozen.recent_blockhash(),"last_valid_block_height":frozen.last_valid_block_height(),"max_fee_lamports":max_fee_lamports,"amount_lamports":lamports,"recipient":recipient,"request_id":request_id}),
            summary: format!("Transfer {} SOL to {}", amount.to_decimal(9), recipient),
            checks: vec![
                Check {
                    name: "fee_limit".into(),
                    status: CheckStatus::Passed,
                    detail: format!(
                        "Estimated fee {fee} lamports is within the {max_fee_lamports} lamport limit."
                    ),
                },
                Check {
                    name: "simulation".into(),
                    status: CheckStatus::Passed,
                    detail: "The frozen transfer passed confirmed-commitment simulation.".into(),
                },
            ],
            warnings: vec![],
            observed_at: fin_domain::now(),
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339(),
            valuation: Some(valuation),
            intent,
            action_deadline: None,
            operation_version: 1,
        };
        fin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }

    async fn prepare_wrap(&self, input: &Value) -> Result<PreparedOperation> {
        let signer = self.signer.as_ref().ok_or_else(|| {
            failure(
                "SOLANA_WALLET_READ_ONLY",
                "prepare",
                "This enrolled wallet is read-only.",
                "Enroll a protected Solana signer before requesting a wrap.",
            )
        })?;
        if input.get("wallet").and_then(Value::as_str) != Some(self.name.as_str())
            || input.get("network").and_then(Value::as_str) != Some("solana")
            || input.get("cluster").and_then(Value::as_str) != Some(CLUSTER)
        {
            return Err(FinError::validation(
                "Select this enrolled wallet and the Solana mainnet-beta cluster.",
            ));
        }
        let request_id = input
            .get("requestId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 128)
            .ok_or_else(|| FinError::validation("wallet.wrap requires a stable requestId."))?;
        let mint = "So11111111111111111111111111111111111111112";
        let account = match input.get("token_account").and_then(Value::as_str) {
            Some(value) => parse_address(value)?,
            None => crate::associated_token_address(&self.address, mint, TOKEN_PROGRAM_ID)?,
        };
        let derived_account =
            crate::associated_token_address(&self.address, mint, TOKEN_PROGRAM_ID)?;
        let account_result = self
            .runtime
            .rpc()
            .request(
                "getAccountInfo",
                json!([account,{"encoding":"base64","commitment":"finalized"}]),
            )
            .await?;
        let destination_created = account_result.get("value").is_some_and(Value::is_null);
        if destination_created {
            if account != derived_account {
                return Err(FinError::validation(
                    "A missing wrapped-SOL account can only be created at the wallet's associated token address.",
                ));
            }
        } else {
            let (info, program) = self.read_token_account(&account).await?;
            if program != TOKEN_PROGRAM_ID
                || info.token_owner != self.address
                || info.mint != mint
                || !info.is_native
                || info.state != "initialized"
                || info.delegate.is_some()
            {
                return Err(failure(
                    "SOLANA_WRAPPED_SOL_ACCOUNT_INVALID",
                    "prepare",
                    "The selected account is not an initialized wallet-owned native wrapped-SOL token account.",
                    "Use the wallet's initialized wrapped-SOL account without an active delegate.",
                ));
            }
        }
        let account_funding_lamports = if destination_created {
            let rent = self
                .runtime
                .rpc()
                .request(
                    "getMinimumBalanceForRentExemption",
                    json!([165,{"commitment":"finalized"}]),
                )
                .await?
                .as_u64()
                .ok_or_else(crate::parse_error)?;
            let cap = fin_domain::Quantity::from_decimal(
                input
                    .get("max_account_funding_sol")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        FinError::validation(
                            "Creating the wrapped-SOL account requires max_account_funding_sol.",
                        )
                    })?,
                9,
            )?
            .as_str()
            .parse::<u64>()
            .map_err(|_| {
                FinError::validation("Account funding limit exceeds the exact u64 lamport range.")
            })?;
            if rent > cap {
                return Err(failure(
                    "SOLANA_ACCOUNT_FUNDING_LIMIT",
                    "prepare",
                    "Wrapped-SOL account rent exceeds max_account_funding_sol.",
                    "Approve a sufficient explicit account-funding limit or provide an existing account.",
                ));
            }
            rent
        } else {
            0
        };
        let amount = fin_domain::Quantity::from_decimal(
            input
                .get("amount")
                .and_then(Value::as_str)
                .ok_or_else(|| FinError::validation("wallet.wrap requires an exact SOL amount."))?,
            9,
        )?;
        let lamports = amount
            .as_str()
            .parse::<u64>()
            .map_err(|_| FinError::validation("SOL amount exceeds the exact u64 lamport range."))?;
        if lamports == 0 {
            return Err(FinError::validation("The wrap amount must be positive."));
        }
        let max_fee = fin_domain::Quantity::from_decimal(
            input
                .get("max_fee_sol")
                .and_then(Value::as_str)
                .ok_or_else(|| FinError::validation("wallet.wrap requires max_fee_sol."))?,
            9,
        )?;
        let max_fee_lamports = max_fee.as_str().parse::<u64>().map_err(|_| {
            FinError::validation("Maximum fee exceeds the exact u64 lamport range.")
        })?;
        let payer = self
            .address
            .parse::<solana_address::Address>()
            .map_err(|_| crate::parse_error())?;
        let destination = account
            .parse::<solana_address::Address>()
            .map_err(|_| crate::parse_error())?;
        let mut system_data = 2u32.to_le_bytes().to_vec();
        system_data.extend_from_slice(&lamports.to_le_bytes());
        let system_transfer = normalize_instruction(solana_instruction::Instruction {
            program_id: "11111111111111111111111111111111"
                .parse()
                .map_err(|_| crate::parse_error())?,
            accounts: vec![
                solana_instruction::AccountMeta::new(payer, true),
                solana_instruction::AccountMeta::new(destination, false),
            ],
            data: system_data,
        });
        let sync = normalize_instruction(solana_instruction::Instruction {
            program_id: TOKEN_PROGRAM_ID.parse().map_err(|_| crate::parse_error())?,
            accounts: vec![solana_instruction::AccountMeta::new(destination, false)],
            data: vec![17],
        });
        let latest = self
            .runtime
            .rpc()
            .request("getLatestBlockhash", json!([{"commitment":"confirmed"}]))
            .await?;
        let value = latest.get("value").ok_or_else(crate::parse_error)?;
        let last_valid = value
            .get("lastValidBlockHeight")
            .and_then(Value::as_u64)
            .ok_or_else(crate::parse_error)?;
        let blockhash = value
            .get("blockhash")
            .and_then(Value::as_str)
            .ok_or_else(crate::parse_error)?;
        let mut instructions = Vec::new();
        if destination_created {
            let create = spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent(
                &self.address.parse::<solana_pubkey3::Pubkey>().map_err(|_| crate::parse_error())?,
                &self.address.parse::<solana_pubkey3::Pubkey>().map_err(|_| crate::parse_error())?,
                &mint.parse::<solana_pubkey3::Pubkey>().map_err(|_| crate::parse_error())?,
                &TOKEN_PROGRAM_ID.parse::<solana_pubkey3::Pubkey>().map_err(|_| crate::parse_error())?,
            );
            instructions.push(normalize_old_instruction(create)?);
        }
        instructions.push(system_transfer);
        instructions.push(sync);
        let frozen = self
            .runtime
            .freeze_instructions(
                &instructions,
                &[],
                blockhash,
                signer,
                last_valid,
                max_fee_lamports,
                Some(request_id.into()),
                &[],
            )
            .await?;
        let fee = self.runtime.validate_fee(&frozen).await?;
        self.runtime.simulate(&frozen).await?;
        let balance = self
            .runtime
            .rpc()
            .request(
                "getBalance",
                json!([self.address,{"commitment":"finalized"}]),
            )
            .await?
            .get("value")
            .and_then(Value::as_u64)
            .ok_or_else(crate::parse_error)?;
        let required_balance = lamports
            .checked_add(fee)
            .and_then(|value| value.checked_add(account_funding_lamports))
            .ok_or_else(crate::parse_error)?;
        if balance < required_balance {
            return Err(failure(
                "SOLANA_INSUFFICIENT_SOL",
                "prepare",
                "The wallet lacks SOL for the wrap amount, any refundable account rent, and network fee.",
                "Fund the wallet or choose a lower amount or rent cap.",
            ));
        }
        let native = AssetId {
            network: NETWORK.into(),
            reference: "native".into(),
            decimals: 9,
        };
        let wrapped = AssetId {
            network: NETWORK.into(),
            reference: mint.into(),
            decimals: 9,
        };
        let quote = self._prices.quote(&native).await?;
        ensure_fresh_quote(&quote)?;
        let price = price_micros_per_unit(&quote.usd_per_unit)?;
        let unit = fin_domain::Quantity::new("1000000000")?;
        let movement = amount.mul_div_ceil(&price, &unit)?;
        let funding_usd =
            fin_domain::Quantity::from_u64(account_funding_lamports).mul_div_ceil(&price, &unit)?;
        let fee_usd = max_fee.mul_div_ceil(&price, &unit)?;
        let resource = ResourceId(format!("solana:mainnet-beta:{}", self.address));
        let mut debits = vec![ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: native.clone(),
                atoms: amount.clone(),
            },
        }];
        let mut expected = vec![EffectConstraint::Conversion {
            resource: resource.clone(),
            input: native.clone(),
            maximum_input: amount.clone(),
            output: wrapped,
            minimum_output: amount.clone(),
        }];
        if account_funding_lamports > 0 {
            debits.push(ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: native.clone(),
                    atoms: fin_domain::Quantity::from_u64(account_funding_lamports),
                },
            });
            let position = format!("account-funding:{account}");
            let position_location = AccountingLocation::Position {
                resource: resource.clone(),
                position: position.clone(),
                position_kind: fin_domain::PositionKind::RefundableAccountFunding,
            };
            expected.push(EffectConstraint::OriginMovement {
                from: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                to: position_location,
                debit_asset: native.clone(),
                credit_asset: native.clone(),
                maximum_debit: fin_domain::Quantity::from_u64(account_funding_lamports),
                minimum_credit: fin_domain::Quantity::from_u64(account_funding_lamports),
            });
            expected.push(EffectConstraint::Debit {
                resource: resource.clone(),
                asset: native.clone(),
                maximum_debit: fin_domain::Quantity::from_u64(account_funding_lamports),
            });
            expected.push(EffectConstraint::PositionOfKind {
                resource: resource.clone(),
                position,
                asset: native.clone(),
                position_kind: fin_domain::PositionKind::RefundableAccountFunding,
                direction: fin_domain::Direction::Increase,
                maximum_change: fin_domain::Quantity::from_u64(account_funding_lamports),
            });
        }
        let mut prepared = PreparedOperation {
            operation: "wallet.wrap".into(),
            input: input.clone(),
            binding: json!({"message_sha256":frozen.message_sha256(),"instructions":instructions,"blockhash":blockhash,"last_valid_block_height":last_valid,"max_fee_lamports":max_fee_lamports,"amount_lamports":lamports,"token_account":account,"destination_account_created":destination_created,"account_funding_lamports":account_funding_lamports,"request_id":request_id}),
            summary: format!("Wrap {} SOL as wrapped SOL", amount.to_decimal(9)),
            checks: vec![
                Check {
                    name: "wrapped_account".into(),
                    status: CheckStatus::Passed,
                    detail: if destination_created {
                        format!(
                            "A wallet-owned wrapped-SOL associated account will be created with {account_funding_lamports} separately bounded refundable rent lamports."
                        )
                    } else {
                        "The existing account is wallet-owned, initialized, and has no active delegate.".into()
                    },
                },
                Check {
                    name: "fee_and_simulation".into(),
                    status: CheckStatus::Passed,
                    detail: format!(
                        "Estimated fee {fee} lamports is within limit and the frozen wrap transaction simulated successfully."
                    ),
                },
            ],
            warnings: vec![],
            observed_at: fin_domain::now(),
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339(),
            valuation: Some(fin_domain::Valuation {
                movement_usd_micros: movement.clone(),
                maximum_fee_usd_micros: fee_usd.clone(),
                additional_allocation_usd_micros: funding_usd,
                observed_at: quote.observed_at.clone(),
                source: quote.source.clone(),
                prices: vec![fin_domain::AssetPrice {
                    asset: native.clone(),
                    usd_micros_per_unit: price,
                    unit_atoms: unit,
                    observed_at: quote.observed_at,
                    source: quote.source,
                }],
            }),
            intent: FinancialIntent {
                debits,
                maximum_costs: vec![CostBound {
                    location: AccountingLocation::Cash {
                        resource: resource.clone(),
                    },
                    amount: Amount {
                        asset: native,
                        atoms: max_fee.clone(),
                    },
                    treatment: CostTreatment::AdditionalDebit,
                }],
                expected,
                locks: vec![resource],
            },
            action_deadline: None,
            operation_version: 1,
        };
        fin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }

    async fn prepare_unwrap(&self, input: &Value) -> Result<PreparedOperation> {
        let signer = self.signer.as_ref().ok_or_else(|| {
            failure(
                "SOLANA_WALLET_READ_ONLY",
                "prepare",
                "This enrolled wallet is read-only.",
                "Enroll a protected Solana signer before requesting an unwrap.",
            )
        })?;
        if input.get("wallet").and_then(Value::as_str) != Some(self.name.as_str())
            || input.get("network").and_then(Value::as_str) != Some("solana")
            || input.get("cluster").and_then(Value::as_str) != Some(CLUSTER)
        {
            return Err(FinError::validation(
                "Select this enrolled wallet and the Solana mainnet-beta cluster.",
            ));
        }
        let request_id = input
            .get("requestId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 128)
            .ok_or_else(|| FinError::validation("wallet.unwrap requires a stable requestId."))?;
        let source_account = match input.get("token_account").and_then(Value::as_str) {
            Some(value) => parse_address(value)?,
            None => crate::associated_token_address(
                &self.address,
                "So11111111111111111111111111111111111111112",
                TOKEN_PROGRAM_ID,
            )?,
        };
        let (source_info, source_program) = self.read_token_account(&source_account).await?;
        if source_program != TOKEN_PROGRAM_ID
            || source_info.token_owner != self.address
            || source_info.mint != "So11111111111111111111111111111111111111112"
            || !source_info.is_native
            || source_info.state != "initialized"
            || source_info.delegate.is_some()
        {
            return Err(failure(
                "SOLANA_WRAPPED_SOL_ACCOUNT_INVALID",
                "prepare",
                "The selected account is not an initialized wallet-owned native wrapped-SOL token account.",
                "Use an initialized wrapped-SOL account without an active delegate.",
            ));
        }
        let amount = fin_domain::Quantity::from_decimal(
            input.get("amount").and_then(Value::as_str).ok_or_else(|| {
                FinError::validation("wallet.unwrap requires an exact SOL amount.")
            })?,
            9,
        )?;
        let lamports = amount
            .as_str()
            .parse::<u64>()
            .map_err(|_| FinError::validation("SOL amount exceeds the exact u64 lamport range."))?;
        let source_amount = source_info.amount_atoms;
        if lamports == 0 || source_amount < lamports {
            return Err(FinError::validation(
                "The unwrap amount must be positive and no greater than the verified wrapped-SOL balance.",
            ));
        }
        let max_fee = fin_domain::Quantity::from_decimal(
            input
                .get("max_fee_sol")
                .and_then(Value::as_str)
                .ok_or_else(|| FinError::validation("wallet.unwrap requires max_fee_sol."))?,
            9,
        )?;
        let max_fee_lamports = max_fee.as_str().parse::<u64>().map_err(|_| {
            FinError::validation("Maximum fee exceeds the exact u64 lamport range.")
        })?;

        // The seed is stable for this approved request, so a restart cannot
        // create a second temporary account or redirect a retry to a new one.
        let digest = Sha256::digest(request_id.as_bytes());
        let seed = format!("finwsol{}", &hex::encode(digest)[..25]);
        let wallet_address = self
            .address
            .parse::<solana_address::Address>()
            .map_err(|_| crate::parse_error())?;
        let token_program = TOKEN_PROGRAM_ID
            .parse::<solana_address::Address>()
            .map_err(|_| crate::parse_error())?;
        let temporary =
            solana_address::Address::create_with_seed(&wallet_address, &seed, &token_program)
                .map_err(|_| {
                    FinError::validation("The request-bound temporary account seed is invalid.")
                })?
                .to_string();
        let prior = self
            .runtime
            .rpc()
            .request(
                "getAccountInfo",
                json!([temporary,{"encoding":"base64","commitment":"finalized"}]),
            )
            .await?;
        if !prior.get("value").is_some_and(|value| value.is_null()) {
            return Err(failure(
                "SOLANA_TEMP_ACCOUNT_ALREADY_EXISTS",
                "prepare",
                "The request-bound temporary unwrap account already exists.",
                "Reconcile the original request; do not create a new unwrap identity.",
            ));
        }
        let rent = self
            .runtime
            .rpc()
            .request(
                "getMinimumBalanceForRentExemption",
                json!([165,{"commitment":"finalized"}]),
            )
            .await?
            .as_u64()
            .ok_or_else(crate::parse_error)?;
        let rent_cap = fin_domain::Quantity::from_decimal(
            input
                .get("max_temporary_rent_sol")
                .and_then(Value::as_str)
                .ok_or_else(|| FinError::validation("wallet.unwrap requires max_temporary_rent_sol to bound temporary account funding."))?,
            9,
        )?
        .as_str()
        .parse::<u64>()
        .map_err(|_| FinError::validation("Temporary rent limit exceeds the exact u64 lamport range."))?;
        if rent > rent_cap {
            return Err(failure(
                "SOLANA_TEMP_ACCOUNT_FUNDING_LIMIT",
                "prepare",
                "Temporary wrapped-SOL account rent exceeds max_temporary_rent_sol.",
                "Approve a sufficient temporary liquidity limit or reduce the amount only if required by policy.",
            ));
        }

        let wallet3 = self
            .address
            .parse::<solana_pubkey3::Pubkey>()
            .map_err(|_| crate::parse_error())?;
        let temporary3 = temporary
            .parse::<solana_pubkey3::Pubkey>()
            .map_err(|_| crate::parse_error())?;
        let token_program3 = TOKEN_PROGRAM_ID
            .parse::<solana_pubkey3::Pubkey>()
            .map_err(|_| crate::parse_error())?;
        let old_create = solana_system_interface::instruction::create_account_with_seed(
            &wallet3,
            &temporary3,
            &wallet3,
            &seed,
            rent,
            165,
            &token_program3,
        );
        let mut instructions = vec![normalize_old_instruction(old_create)?];

        // Classic SPL Token wire formats: InitializeAccount3 (18 + owner),
        // TransferChecked (12 + amount + decimals), then CloseAccount (9).
        // A new account is closed in this same transaction; an existing ATA is
        // never closed and partial unwraps preserve its remaining balance.
        let temporary32 = temporary
            .parse::<solana_address::Address>()
            .map_err(|_| crate::parse_error())?;
        let mint32 = "So11111111111111111111111111111111111111112"
            .parse::<solana_address::Address>()
            .map_err(|_| crate::parse_error())?;
        let source32 = source_account
            .parse::<solana_address::Address>()
            .map_err(|_| crate::parse_error())?;
        let mut initialize_data = vec![18];
        initialize_data.extend_from_slice(wallet_address.as_ref());
        instructions.push(normalize_instruction(solana_instruction::Instruction {
            program_id: TOKEN_PROGRAM_ID.parse().map_err(|_| crate::parse_error())?,
            accounts: vec![
                solana_instruction::AccountMeta::new(temporary32, false),
                solana_instruction::AccountMeta::new_readonly(mint32, false),
            ],
            data: initialize_data,
        }));
        let mut transfer_data = vec![12];
        transfer_data.extend_from_slice(&lamports.to_le_bytes());
        transfer_data.push(9);
        instructions.push(normalize_instruction(solana_instruction::Instruction {
            program_id: TOKEN_PROGRAM_ID.parse().map_err(|_| crate::parse_error())?,
            accounts: vec![
                solana_instruction::AccountMeta::new(source32, false),
                solana_instruction::AccountMeta::new_readonly(mint32, false),
                solana_instruction::AccountMeta::new(temporary32, false),
                solana_instruction::AccountMeta::new(wallet_address, true),
            ],
            data: transfer_data,
        }));
        instructions.push(normalize_instruction(solana_instruction::Instruction {
            program_id: TOKEN_PROGRAM_ID.parse().map_err(|_| crate::parse_error())?,
            accounts: vec![
                solana_instruction::AccountMeta::new(temporary32, false),
                solana_instruction::AccountMeta::new(wallet_address, false),
                solana_instruction::AccountMeta::new_readonly(wallet_address, true),
            ],
            data: vec![9],
        }));
        let latest = self
            .runtime
            .rpc()
            .request("getLatestBlockhash", json!([{"commitment":"confirmed"}]))
            .await?;
        let value = latest.get("value").ok_or_else(crate::parse_error)?;
        let last_valid = value
            .get("lastValidBlockHeight")
            .and_then(Value::as_u64)
            .ok_or_else(crate::parse_error)?;
        let blockhash = value
            .get("blockhash")
            .and_then(Value::as_str)
            .ok_or_else(crate::parse_error)?;
        let frozen = self
            .runtime
            .freeze_instructions(
                &instructions,
                &[],
                blockhash,
                signer,
                last_valid,
                max_fee_lamports,
                Some(request_id.into()),
                &[],
            )
            .await?;
        let fee = self.runtime.validate_fee(&frozen).await?;
        self.runtime.simulate(&frozen).await?;
        let balance = self
            .runtime
            .rpc()
            .request(
                "getBalance",
                json!([self.address,{"commitment":"finalized"}]),
            )
            .await?
            .get("value")
            .and_then(Value::as_u64)
            .ok_or_else(crate::parse_error)?;
        let required_liquidity = rent.checked_add(fee).ok_or_else(crate::parse_error)?;
        if balance < required_liquidity {
            return Err(failure(
                "SOLANA_INSUFFICIENT_SOL",
                "prepare",
                "The wallet lacks SOL for temporary wrapped-SOL rent and the network fee.",
                "Fund the wallet or use a lower fee limit; temporary rent is returned when the new account closes.",
            ));
        }
        let wrapped = AssetId {
            network: NETWORK.into(),
            reference: "So11111111111111111111111111111111111111112".into(),
            decimals: 9,
        };
        let native = AssetId {
            network: NETWORK.into(),
            reference: "native".into(),
            decimals: 9,
        };
        let quote = self._prices.quote(&native).await?;
        ensure_fresh_quote(&quote)?;
        let price = price_micros_per_unit(&quote.usd_per_unit)?;
        let unit = fin_domain::Quantity::new("1000000000")?;
        let movement = amount.mul_div_ceil(&price, &unit)?;
        let fee_usd = max_fee.mul_div_ceil(&price, &unit)?;
        let resource = ResourceId(format!("solana:mainnet-beta:{}", self.address));
        let mut prepared = PreparedOperation {
            operation: "wallet.unwrap".into(),
            input: input.clone(),
            binding: json!({"message_sha256":frozen.message_sha256(),"instructions":instructions,"blockhash":blockhash,"last_valid_block_height":last_valid,"max_fee_lamports":max_fee_lamports,"amount_lamports":lamports,"source_token_account":source_account,"temporary_account":temporary,"temporary_seed":seed,"temporary_rent_lamports":rent,"request_id":request_id}),
            summary: format!("Unwrap {} wrapped SOL to SOL", amount.to_decimal(9)),
            checks: vec![
                Check { name:"wrapped_balance".into(), status:CheckStatus::Passed, detail:"The source is an initialized wallet-owned wrapped-SOL account with sufficient exact balance.".into() },
                Check { name:"temporary_rent_and_simulation".into(), status:CheckStatus::Passed, detail:format!("Temporary account rent {rent} lamports is within its explicit cap, is returned on close, and the frozen transaction simulated successfully; estimated fee is {fee} lamports.") },
            ],
            warnings: vec![],
            observed_at: fin_domain::now(),
            expires_at: (chrono::Utc::now()+chrono::Duration::seconds(60)).to_rfc3339(),
            valuation: Some(fin_domain::Valuation {
                movement_usd_micros: movement.clone(),
                maximum_fee_usd_micros: fee_usd.clone(),
                additional_allocation_usd_micros: fin_domain::Quantity::from_u64(0),
                observed_at: quote.observed_at.clone(),
                source: quote.source.clone(),
                prices: vec![fin_domain::AssetPrice { asset: native.clone(), usd_micros_per_unit: price, unit_atoms: unit, observed_at: quote.observed_at, source: quote.source }],
            }),
            intent: FinancialIntent {
                debits: vec![ResourceAmount { resource: resource.clone(), amount: Amount { asset: wrapped.clone(), atoms: amount.clone() } }],
                maximum_costs: vec![CostBound { location: AccountingLocation::Cash { resource: resource.clone() }, amount: Amount { asset: native.clone(), atoms: max_fee }, treatment: CostTreatment::AdditionalDebit }],
                expected: vec![EffectConstraint::Conversion { resource: resource.clone(), input: wrapped, maximum_input: amount.clone(), output: native, minimum_output: amount }],
                locks: vec![resource],
            },
            action_deadline: None,
            operation_version: 1,
        };
        fin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }

    async fn prepare_token_transfer(&self, input: &Value) -> Result<PreparedOperation> {
        let signer = self.signer.as_ref().ok_or_else(|| {
            failure(
                "SOLANA_WALLET_READ_ONLY",
                "prepare",
                "This enrolled wallet is read-only.",
                "Enroll a protected Solana signer before requesting a transfer.",
            )
        })?;
        if input.get("wallet").and_then(Value::as_str) != Some(self.name.as_str())
            || input.get("network").and_then(Value::as_str) != Some("solana")
            || input.get("cluster").and_then(Value::as_str) != Some(CLUSTER)
        {
            return Err(FinError::validation(
                "Select this enrolled wallet and the Solana mainnet-beta cluster.",
            ));
        }
        let request_id = input
            .get("requestId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 128)
            .ok_or_else(|| FinError::validation("wallet.transfer requires a stable requestId."))?;
        let recipient = parse_address(input.get("recipient").and_then(Value::as_str).ok_or_else(
            || FinError::validation("Token transfers require recipient owner address."),
        )?)?;
        if recipient == self.address {
            return Err(FinError::validation(
                "The token transfer recipient must differ from the source wallet.",
            ));
        }
        let mint = parse_address(input.get("mint").and_then(Value::as_str).ok_or_else(|| {
            FinError::validation("Token transfers require an exact mint address.")
        })?)?;
        let source = parse_address(
            input
                .get("source_token_account")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    FinError::validation("Token transfers require source_token_account.")
                })?,
        )?;
        let destination_override = input
            .get("destination_token_account")
            .and_then(Value::as_str)
            .map(parse_address)
            .transpose()?;
        let epoch = self
            .runtime
            .rpc()
            .request("getEpochInfo", json!([{"commitment":"finalized"}]))
            .await?
            .get("epoch")
            .and_then(Value::as_u64)
            .ok_or_else(crate::parse_error)?;
        let (source_info, source_program) = self.read_token_account(&source).await?;
        if source_info.token_owner != self.address
            || source_info.mint != mint
            || source_info.state != "initialized"
        {
            return Err(failure(
                "SOLANA_TOKEN_ACCOUNT_INVALID",
                "prepare",
                "The selected source token account does not match the enrolled wallet and exact mint.",
                "Choose an initialized token account for this mint owned by the enrolled wallet.",
            ));
        }
        let mint_response = self
            .runtime
            .rpc()
            .request(
                "getAccountInfo",
                json!([mint,{"encoding":"base64","commitment":"finalized"}]),
            )
            .await?;
        let mint_account = mint_response
            .get("value")
            .filter(|v| !v.is_null())
            .ok_or_else(crate::parse_error)?;
        let mint_program = mint_account
            .get("owner")
            .and_then(Value::as_str)
            .ok_or_else(crate::parse_error)?;
        if mint_program != source_program {
            return Err(crate::parse_error());
        }
        let mint_data =
            decode_rpc_account_data(mint_account.get("data").ok_or_else(crate::parse_error)?)?;
        let descriptor = parse_mint(mint_program, &mint_data, epoch)?;
        let allow_destination_creation = destination_override.is_none();
        let destination = match destination_override {
            Some(address) => address,
            None => crate::associated_token_address(&recipient, &mint, mint_program)?,
        };
        let destination_response = self
            .runtime
            .rpc()
            .request(
                "getAccountInfo",
                json!([destination,{"encoding":"base64","commitment":"finalized"}]),
            )
            .await?;
        let destination_value = destination_response
            .get("value")
            .filter(|value| !value.is_null());
        let destination_info = if destination_value.is_some() {
            Some(self.read_token_account(&destination).await?.0)
        } else {
            None
        };
        let create_destination = destination_info.is_none() && allow_destination_creation;
        if destination_info.is_none() && !create_destination {
            return Err(failure(
                "SOLANA_DESTINATION_TOKEN_ACCOUNT_MISSING",
                "prepare",
                "The selected destination token account does not exist.",
                "Omit destination_token_account to create the recipient's associated token account, or provide an existing account.",
            ));
        }
        if let Some(destination_info) = destination_info.as_ref()
            && (destination_info.token_owner != recipient
                || destination_info.mint != mint
                || destination_info.program_id != mint_program
                || destination_info.state != "initialized")
        {
            return Err(failure(
                "SOLANA_TOKEN_ACCOUNT_INVALID",
                "prepare",
                "The destination token account does not match the recipient, mint, token program, and initialized state.",
                "Choose an initialized destination account for this exact mint and recipient.",
            ));
        }
        if descriptor.default_account_state.as_deref() == Some("frozen") && create_destination {
            return Err(failure(
                "SOLANA_DESTINATION_DEFAULT_FROZEN",
                "prepare",
                "The mint initializes new token accounts in a frozen state.",
                "Use an already initialized, thawed destination account.",
            ));
        }
        if !descriptor.transfer_supported_by_runtime {
            return Err(FinError::new(
                "SOLANA_TOKEN_EXTENSION_UNSUPPORTED",
                "prepare",
                descriptor
                    .transfer_limitation
                    .clone()
                    .unwrap_or_else(|| "This token extension combination is unsupported.".into()),
                "Choose a token with a reviewed transfer path or wait for support for these extensions.",
            ));
        }
        if descriptor
            .extensions
            .iter()
            .any(|ext| ext == "ConfidentialTransferMint")
            && destination_info
                .as_ref()
                .is_some_and(|account| account.allow_non_confidential_credits == Some(false))
        {
            return Err(failure(
                "SOLANA_CONFIDENTIAL_TRANSFER_STATE_UNSUPPORTED",
                "prepare",
                "The destination token account disables ordinary non-confidential credits.",
                "Use a destination account that permits public token credits.",
            ));
        }
        let amount = fin_domain::Quantity::from_decimal(
            input.get("amount").and_then(Value::as_str).ok_or_else(|| {
                FinError::validation("wallet.transfer requires an exact decimal amount.")
            })?,
            descriptor.decimals,
        )?;
        let gross = amount.as_str().parse::<u64>().map_err(|_| {
            FinError::validation("SPL transfer amount exceeds the exact u64 atom range.")
        })?;
        if gross == 0 || gross > source_info.amount_atoms {
            return Err(FinError::validation(
                "The positive transfer amount must not exceed the source token balance.",
            ));
        }
        let fee = descriptor
            .transfer_fee
            .as_ref()
            .map(|schedule| schedule.fee_for_gross(gross).ok_or_else(crate::parse_error))
            .transpose()?
            .unwrap_or(0);
        let net = gross.checked_sub(fee).ok_or_else(crate::parse_error)?;
        let source_key = source
            .parse::<solana_address::Address>()
            .map_err(|_| crate::parse_error())?;
        let destination_key = destination
            .parse::<solana_address::Address>()
            .map_err(|_| crate::parse_error())?;
        let mint_key = mint
            .parse::<solana_address::Address>()
            .map_err(|_| crate::parse_error())?;
        let authority = self
            .address
            .parse::<solana_address::Address>()
            .map_err(|_| crate::parse_error())?;
        let token_program = mint_program
            .parse::<solana_address::Address>()
            .map_err(|_| crate::parse_error())?;
        let old_instruction = if descriptor.transfer_fee.is_some() {
            spl_token_2022_interface::extension::transfer_fee::instruction::transfer_checked_with_fee(&token_program,&source_key,&mint_key,&destination_key,&authority,&[],gross,descriptor.decimals,fee).map_err(|_|crate::parse_error())?
        } else {
            spl_token_2022_interface::instruction::transfer_checked(
                &token_program,
                &source_key,
                &mint_key,
                &destination_key,
                &authority,
                &[],
                gross,
                descriptor.decimals,
            )
            .map_err(|_| crate::parse_error())?
        };
        let instruction = solana_instruction::Instruction {
            program_id: old_instruction
                .program_id
                .to_string()
                .parse()
                .map_err(|_| crate::parse_error())?,
            accounts: old_instruction
                .accounts
                .into_iter()
                .map(|meta| {
                    Ok(solana_instruction::AccountMeta {
                        pubkey: meta
                            .pubkey
                            .to_string()
                            .parse()
                            .map_err(|_| crate::parse_error())?,
                        is_signer: meta.is_signer,
                        is_writable: meta.is_writable,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            data: old_instruction.data,
        };
        let mut transfer_instruction = normalize_instruction(instruction);
        if let Some(hook) = descriptor.transfer_hook_program.as_deref() {
            self.runtime
                .resolve_transfer_hook_instruction(&mut transfer_instruction, hook, gross)
                .await?;
        }
        let mut instructions = Vec::new();
        let mut account_funding_lamports = 0u64;
        if create_destination {
            let size = required_token_account_len(mint_program, &mint_data)?;
            let rent = self
                .runtime
                .rpc()
                .request(
                    "getMinimumBalanceForRentExemption",
                    json!([size,{"commitment":"finalized"}]),
                )
                .await?
                .as_u64()
                .ok_or_else(crate::parse_error)?;
            let funding_cap = fin_domain::Quantity::from_decimal(input.get("max_account_funding_sol").and_then(Value::as_str).ok_or_else(|| FinError::validation("Creating the recipient's associated token account requires max_account_funding_sol."))?,9)?.as_str().parse::<u64>().map_err(|_| FinError::validation("Account funding limit exceeds the exact u64 lamport range."))?;
            if rent > funding_cap {
                return Err(failure(
                    "SOLANA_ACCOUNT_FUNDING_LIMIT",
                    "prepare",
                    "The refundable recipient-account rent exceeds max_account_funding_sol.",
                    "Approve a sufficient explicit account-funding limit or provide an existing destination account.",
                ));
            }
            account_funding_lamports = rent;
            let old_create = spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent(
                &self.address.parse::<solana_pubkey3::Pubkey>().map_err(|_| crate::parse_error())?,
                &recipient.parse::<solana_pubkey3::Pubkey>().map_err(|_| crate::parse_error())?,
                &mint.parse::<solana_pubkey3::Pubkey>().map_err(|_| crate::parse_error())?,
                &mint_program.parse::<solana_pubkey3::Pubkey>().map_err(|_| crate::parse_error())?,
            );
            instructions.push(normalize_old_instruction(old_create)?);
        }
        if destination_info
            .as_ref()
            .is_some_and(|info| info.memo_required)
        {
            instructions.push(memo_instruction(request_id)?);
        }
        instructions.push(transfer_instruction);
        let max_fee = fin_domain::Quantity::from_decimal(
            input
                .get("max_fee_sol")
                .and_then(Value::as_str)
                .ok_or_else(|| FinError::validation("Token transfers require max_fee_sol."))?,
            9,
        )?;
        let max_fee_lamports = max_fee.as_str().parse::<u64>().map_err(|_| {
            FinError::validation("Maximum fee exceeds the exact u64 lamport range.")
        })?;
        let latest = self
            .runtime
            .rpc()
            .request("getLatestBlockhash", json!([{"commitment":"confirmed"}]))
            .await?;
        let value = latest.get("value").ok_or_else(crate::parse_error)?;
        let blockhash = value
            .get("blockhash")
            .and_then(Value::as_str)
            .ok_or_else(crate::parse_error)?;
        let last_valid = value
            .get("lastValidBlockHeight")
            .and_then(Value::as_u64)
            .ok_or_else(crate::parse_error)?;
        let frozen = self
            .runtime
            .freeze_instructions(
                &instructions,
                &[],
                blockhash,
                signer,
                last_valid,
                max_fee_lamports,
                Some(request_id.into()),
                &[],
            )
            .await?;
        let estimated_fee = self.runtime.validate_fee(&frozen).await?;
        self.runtime.simulate(&frozen).await?;
        let native_balance = self
            .runtime
            .rpc()
            .request(
                "getBalance",
                json!([self.address,{"commitment":"finalized"}]),
            )
            .await?
            .get("value")
            .and_then(Value::as_u64)
            .ok_or_else(crate::parse_error)?;
        let required_sol = estimated_fee
            .checked_add(account_funding_lamports)
            .ok_or_else(crate::parse_error)?;
        if native_balance < required_sol {
            return Err(failure(
                "SOLANA_INSUFFICIENT_SOL",
                "prepare",
                "The wallet does not have enough SOL for the estimated fee and refundable account rent.",
                "Fund the wallet with SOL, increase the bounded rent funding amount, or use an existing destination account.",
            ));
        }
        let asset = AssetId {
            network: NETWORK.into(),
            reference: mint.clone(),
            decimals: descriptor.decimals,
        };
        let sol = AssetId {
            network: NETWORK.into(),
            reference: "native".into(),
            decimals: 9,
        };
        let token_quote = self._prices.quote(&asset).await?;
        ensure_fresh_quote(&token_quote)?;
        let sol_quote = if asset == sol {
            token_quote.clone()
        } else {
            let quote = self._prices.quote(&sol).await?;
            ensure_fresh_quote(&quote)?;
            quote
        };
        let token_price = price_micros_per_unit(&token_quote.usd_per_unit)?;
        let sol_price = price_micros_per_unit(&sol_quote.usd_per_unit)?;
        let token_unit =
            fin_domain::Quantity::new(format!("1{}", "0".repeat(asset.decimals as usize)))?;
        let sol_unit = fin_domain::Quantity::new("1000000000")?;
        let mut movement = amount.mul_div_ceil(&token_price, &token_unit)?;
        if account_funding_lamports > 0 {
            let rent_usd = fin_domain::Quantity::from_u64(account_funding_lamports)
                .mul_div_ceil(&sol_price, &sol_unit)?;
            movement = movement.checked_add(&rent_usd)?;
        }
        let fee_usd = max_fee.mul_div_ceil(&sol_price, &sol_unit)?;
        let source_resource = ResourceId(format!("solana:mainnet-beta:{}", self.address));
        let destination_resource = ResourceId(format!("solana:mainnet-beta:{recipient}"));
        let mut expected = vec![EffectConstraint::Transfer {
            from: source_resource.clone(),
            to: destination_resource,
            debit_asset: asset.clone(),
            credit_asset: asset.clone(),
            maximum_debit: amount.clone(),
            minimum_credit: fin_domain::Quantity::from_u64(net),
        }];
        if account_funding_lamports > 0 {
            expected.push(EffectConstraint::Transfer {
                from: source_resource.clone(),
                to: ResourceId(format!("solana:mainnet-beta:account:{destination}")),
                debit_asset: sol.clone(),
                credit_asset: sol.clone(),
                maximum_debit: fin_domain::Quantity::from_u64(account_funding_lamports),
                minimum_credit: fin_domain::Quantity::from_u64(account_funding_lamports),
            });
        }
        let mut debits = vec![ResourceAmount {
            resource: source_resource.clone(),
            amount: Amount {
                asset: asset.clone(),
                atoms: amount.clone(),
            },
        }];
        if account_funding_lamports > 0 {
            debits.push(ResourceAmount {
                resource: source_resource.clone(),
                amount: Amount {
                    asset: sol.clone(),
                    atoms: fin_domain::Quantity::from_u64(account_funding_lamports),
                },
            });
        }
        let intent = FinancialIntent {
            debits,
            maximum_costs: vec![CostBound {
                location: AccountingLocation::Cash {
                    resource: source_resource.clone(),
                },
                amount: Amount {
                    asset: sol.clone(),
                    atoms: max_fee.clone(),
                },
                treatment: CostTreatment::AdditionalDebit,
            }],
            expected,
            locks: vec![source_resource],
        };
        let valuation = fin_domain::Valuation {
            movement_usd_micros: movement.clone(),
            maximum_fee_usd_micros: fee_usd.clone(),
            additional_allocation_usd_micros: fin_domain::Quantity::from_u64(0),
            observed_at: token_quote.observed_at.clone(),
            source: token_quote.source.clone(),
            prices: vec![
                fin_domain::AssetPrice {
                    asset: asset.clone(),
                    usd_micros_per_unit: token_price,
                    unit_atoms: token_unit,
                    observed_at: token_quote.observed_at,
                    source: token_quote.source,
                },
                fin_domain::AssetPrice {
                    asset: sol.clone(),
                    usd_micros_per_unit: sol_price,
                    unit_atoms: sol_unit,
                    observed_at: sol_quote.observed_at,
                    source: sol_quote.source,
                },
            ],
        };
        let mut prepared = PreparedOperation {
            operation: "wallet.transfer".into(),
            input: input.clone(),
            binding: json!({"message_sha256":frozen.message_sha256(),"instructions":frozen.instructions(),"blockhash":frozen.recent_blockhash(),"last_valid_block_height":last_valid,"max_fee_lamports":max_fee_lamports,"amount_atoms":gross,"expected_credit_atoms":net,"token_fee_atoms":fee,"mint":mint,"decimals":descriptor.decimals,"token_program":mint_program,"source_token_account":source,"destination_token_account":destination,"destination_account_created":create_destination,"account_funding_lamports":account_funding_lamports,"source_owner":self.address,"recipient":recipient,"request_id":request_id}),
            summary: if account_funding_lamports > 0 {
                format!(
                    "Transfer {gross} atoms of mint {mint} to {recipient}; send {account_funding_lamports} lamports for the recipient's token-account rent"
                )
            } else {
                format!("Transfer {} atoms of mint {} to {}", gross, mint, recipient)
            },
            checks: vec![
                Check {
                    name: "token_identity".into(),
                    status: CheckStatus::Passed,
                    detail: format!(
                        "Binary mint and source/destination accounts agree on mint, program, owner and exact decimals ({}).",
                        descriptor.decimals
                    ),
                },
                Check {
                    name: "token_fee".into(),
                    status: CheckStatus::Passed,
                    detail: format!(
                        "Estimated transfer fee {fee} atoms; recipient receives {net} atoms."
                    ),
                },
                Check {
                    name: "fee_and_simulation".into(),
                    status: CheckStatus::Passed,
                    detail: format!(
                        "Estimated network fee {estimated_fee} lamports is within the limit and the frozen transaction simulated successfully."
                    ),
                },
                Check {
                    name: "account_funding".into(),
                    status: CheckStatus::Passed,
                    detail: if account_funding_lamports > 0 {
                        format!(
                            "The newly created recipient account receives {account_funding_lamports} SOL rent lamports. Its owner ({recipient}) controls any later rent refund; this is separate from the network fee."
                        )
                    } else {
                        "No new token account funding is required.".into()
                    },
                },
            ],
            warnings: descriptor.transfer_limitation.clone().into_iter().collect(),
            observed_at: fin_domain::now(),
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339(),
            valuation: Some(valuation),
            intent,
            action_deadline: None,
            operation_version: 1,
        };
        fin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }

    async fn read_token_account(
        &self,
        address: &str,
    ) -> Result<(crate::token::TokenAccountInfo, String)> {
        let response = self
            .runtime
            .rpc()
            .request(
                "getAccountInfo",
                json!([address,{"encoding":"base64","commitment":"finalized"}]),
            )
            .await?;
        let account = response
            .get("value")
            .filter(|v| !v.is_null())
            .ok_or_else(crate::parse_error)?;
        let owner = account
            .get("owner")
            .and_then(Value::as_str)
            .ok_or_else(crate::parse_error)?
            .to_owned();
        let data = decode_rpc_account_data(account.get("data").ok_or_else(crate::parse_error)?)?;
        Ok((parse_token_account(&owner, address, &data)?, owner))
    }

    async fn freeze_prepared(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<crate::FrozenTransaction> {
        let signer = self.signer.as_ref().ok_or_else(|| {
            failure(
                "SOLANA_WALLET_READ_ONLY",
                "prepare",
                "This enrolled wallet is read-only.",
                "Enroll a protected Solana signer before preparing a transfer.",
            )
        })?;
        if !matches!(
            prepared.operation.as_str(),
            "wallet.transfer" | "wallet.wrap" | "wallet.unwrap"
        ) {
            return Err(unknown_operation());
        }
        let binding = &prepared.binding;
        let instructions: Vec<crate::NormalizedInstruction> = serde_json::from_value(
            binding
                .get("instructions")
                .cloned()
                .ok_or_else(crate::parse_error)?,
        )
        .map_err(|_| crate::parse_error())?;
        let blockhash = binding
            .get("blockhash")
            .and_then(Value::as_str)
            .ok_or_else(crate::parse_error)?;
        let last_valid = binding
            .get("last_valid_block_height")
            .and_then(Value::as_u64)
            .ok_or_else(crate::parse_error)?;
        let max_fee = binding
            .get("max_fee_lamports")
            .and_then(Value::as_u64)
            .ok_or_else(crate::parse_error)?;
        let request_id = binding
            .get("request_id")
            .and_then(Value::as_str)
            .ok_or_else(crate::parse_error)?;
        let height = self
            .runtime
            .rpc()
            .request("getBlockHeight", json!([{"commitment":"confirmed"}]))
            .await?
            .as_u64()
            .ok_or_else(crate::parse_error)?;
        if height > last_valid {
            return Err(failure(
                "SOLANA_BLOCKHASH_EXPIRED",
                "prepare",
                "The frozen transaction blockhash has expired; no replacement was created.",
                "Prepare a fresh candidate within active plan or boundary authority; otherwise request new approval.",
            ));
        }
        let frozen = self
            .runtime
            .freeze_instructions(
                &instructions,
                &[],
                blockhash,
                signer,
                last_valid,
                max_fee,
                Some(request_id.into()),
                &[],
            )
            .await?;
        if binding.get("message_sha256").and_then(Value::as_str) != Some(frozen.message_sha256()) {
            return Err(failure(
                "SOLANA_BINDING_MISMATCH",
                "prepare",
                "The regenerated message does not match the authorized frozen transaction.",
                "Keep the original operation unresolved and prepare a new reviewed transfer.",
            ));
        }
        Ok(frozen)
    }

    async fn receipt(&self, input: &Value) -> Result<Value> {
        let signature = input
            .get("signature")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                FinError::validation("wallet.receipt requires a transaction signature.")
            })?;
        let message_hash = input
            .get("message_sha256")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                FinError::validation("wallet.receipt requires the persisted message hash.")
            })?;
        let outcome = match self.runtime.reconcile(signature, message_hash).await {
            Ok(outcome) => outcome,
            Err(error) if error.code == "SOLANA_RPC_UNAVAILABLE" => crate::ReceiptOutcome::Pending { confirmed: false, reason: "The receipt RPC is temporarily unavailable; check this original signature again without submitting another transaction.".into() },
            Err(error) => return Err(error),
        };
        serde_json::to_value(outcome).map_err(|_| crate::parse_error())
    }

    async fn snapshot_for_scope(&self, scope: &SnapshotScope) -> Result<StateSnapshot> {
        let resource = ResourceId(format!("solana:mainnet-beta:{}", self.address));
        let mut state = FinancialState::default();
        if scope.matches_resource(&resource) && scope.matches_network(NETWORK) {
            let observation = self.observe_balances().await?;
            let observations = &observation.balances;
            let native = AssetId {
                network: NETWORK.into(),
                reference: "native".into(),
                decimals: 9,
            };
            if scope.matches_asset(&native) {
                let atoms = observations["native"]["amount_atoms"]
                    .as_str()
                    .ok_or_else(crate::parse_error)?;
                state.holdings.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: Amount {
                        asset: native,
                        atoms: fin_domain::Quantity::new(atoms)?,
                    },
                });
            }
            for account in observations["token_accounts"]
                .as_array()
                .ok_or_else(crate::parse_error)?
            {
                let asset: AssetId = serde_json::from_value(account["asset"].clone())
                    .map_err(|_| crate::parse_error())?;
                if !scope.matches_asset(&asset) {
                    continue;
                }
                let atoms = account["amount_atoms"]
                    .as_str()
                    .ok_or_else(crate::parse_error)?;
                let atoms = fin_domain::Quantity::new(atoms)?;
                if let Some(existing) = state
                    .holdings
                    .iter_mut()
                    .find(|holding| holding.resource == resource && holding.amount.asset == asset)
                {
                    existing.amount.atoms = existing.amount.atoms.checked_add(&atoms)?;
                } else {
                    state.holdings.push(ResourceAmount {
                        resource: resource.clone(),
                        amount: Amount {
                            asset: asset.clone(),
                            atoms,
                        },
                    });
                }
                let account_address = account
                    .get("address")
                    .and_then(Value::as_str)
                    .ok_or_else(crate::parse_error)?;
                let delegate = account.get("delegate").and_then(Value::as_str);
                let close_authority = account.get("close_authority").and_then(Value::as_str);
                if delegate.is_some() || close_authority.is_some() {
                    let delegated_atoms = account
                        .get("delegated_amount_atoms")
                        .and_then(Value::as_str)
                        .ok_or_else(crate::parse_error)?;
                    state.authorities.push(AuthorityState {
                        resource: resource.clone(),
                        authority: format!("token_account:{account_address}"),
                        value: serde_json::to_string(&json!({
                            "delegate":delegate,
                            "delegated_amount_atoms":delegated_atoms,
                            "close_authority":close_authority
                        }))
                        .map_err(|_| crate::parse_error())?,
                    });
                }

                // The finalized owner query already supplied the exact binary
                // account and lamports. Reuse that verified observation so rent,
                // token balances and close authority refer to the same account
                // state, without another RPC round trip per token account.
                let (lamports, verified) = observation
                    .accounts
                    .get(account_address)
                    .ok_or_else(crate::parse_error)?;
                let account_program = verified.program_id.as_str();
                let refund_authority = verified
                    .close_authority
                    .as_deref()
                    .unwrap_or(verified.token_owner.as_str());
                let refundable_lamports = if verified.is_native {
                    verified.rent_exempt_reserve_lamports.unwrap_or(0)
                } else {
                    *lamports
                };
                if refund_authority == self.address && refundable_lamports > 0 {
                    let native_asset = AssetId {
                        network: NETWORK.into(),
                        reference: "native".into(),
                        decimals: 9,
                    };
                    let principal = fin_domain::Quantity::from_u64(refundable_lamports);
                    let position = format!("account-funding:{account_address}");
                    state.positions.push(PositionState {
                        resource: resource.clone(),
                        position: position.clone(),
                        kind: fin_domain::PositionKind::RefundableAccountFunding,
                        amount: Amount {
                            asset: native_asset.clone(),
                            atoms: principal.clone(),
                        },
                        scaled_receipt: None,
                        anchor: Some(PositionAnchor {
                            resource: resource.clone(),
                            position,
                            position_kind: fin_domain::PositionKind::RefundableAccountFunding,
                            principal: Amount {
                                asset: native_asset,
                                atoms: principal,
                            },
                            scaled_receipt: None,
                            evidence_version: 1,
                            evidence: json!({
                                "cluster":CLUSTER,
                                "account":account_address,
                                "account_owner":account_program,
                                "token_owner":verified.token_owner,
                                "refund_authority":refund_authority,
                                "lamports":refundable_lamports,
                                "commitment":"finalized"
                            }),
                            observed_at: fin_domain::now(),
                        }),
                    });
                }
            }
            // A tracked mint with no account is still a verified zero holding;
            // confirm the mint owner and decimals from finalized binary data.
            for asset in scope
                .assets
                .iter()
                .filter(|asset| asset.network == NETWORK && asset.reference != "native")
            {
                if state
                    .holdings
                    .iter()
                    .any(|holding| holding.amount.asset == *asset)
                {
                    continue;
                }
                let Ok(mint) = parse_address(&asset.reference) else {
                    // Protocol position units can share Solana's network while
                    // using application-specific references. Their owning
                    // observer supplies coverage; this adapter only asserts
                    // SPL mints verified under one of the two token programs.
                    continue;
                };
                let response = self
                    .runtime
                    .rpc()
                    .request(
                        "getAccountInfo",
                        json!([mint,{"encoding":"base64","commitment":"finalized"}]),
                    )
                    .await?;
                let account = response.get("value");
                let account = account
                    .filter(|value| !value.is_null())
                    .ok_or_else(|| {
                        failure(
                            "SOLANA_MINT_NOT_FOUND",
                            "snapshot",
                            "A tracked base58 asset has no finalized account to verify as an SPL mint.",
                            "Refresh the tracked mint identity or let its application observer provide the asset coverage.",
                        )
                    })?;
                let owner = account
                    .get("owner")
                    .and_then(Value::as_str)
                    .ok_or_else(crate::parse_error)?;
                if owner != TOKEN_PROGRAM_ID && owner != TOKEN_2022_PROGRAM_ID {
                    continue;
                }
                let data =
                    decode_rpc_account_data(account.get("data").ok_or_else(crate::parse_error)?)?;
                let descriptor = parse_mint(owner, &data, 0)?;
                if descriptor.decimals != asset.decimals {
                    return Err(failure(
                        "SOLANA_ASSET_DECIMALS_MISMATCH",
                        "snapshot",
                        "The tracked asset decimals do not match the finalized mint account.",
                        "Refresh the asset identity from the verified mint metadata.",
                    ));
                }
                state.holdings.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: Amount {
                        asset: asset.clone(),
                        atoms: fin_domain::Quantity::from_u64(0),
                    },
                });
            }
        }
        Ok(StateSnapshot {
            state,
            observed_at: fin_domain::now(),
            complete: true,
            coverage: vec![
                resource.0.clone(),
                format!(
                    "Wallet-owned SOL and SPL token accounts for {} at finalized commitment; application positions are reported by their application observer.",
                    self.address
                ),
            ],
            pending: vec![],
        })
    }
}

#[derive(Clone)]
struct WalletAuthorizationIdentity {
    operation: String,
    resource: ResourceId,
    principal_asset: AssetId,
    debit_asset: AssetId,
    recipient: String,
    mint: Option<String>,
    source_token_account: Option<String>,
    destination_token_account: Option<String>,
    token_account: Option<String>,
}

impl SolanaWalletAdapter {
    async fn authorization_identity(
        &self,
        operation: &str,
        input: &Value,
        binding: Option<&Value>,
    ) -> Result<WalletAuthorizationIdentity> {
        if !matches!(
            operation,
            "wallet.transfer" | "wallet.wrap" | "wallet.unwrap"
        ) || input.get("wallet").and_then(Value::as_str) != Some(self.name.as_str())
            || input.get("network").and_then(Value::as_str) != Some("solana")
            || input.get("cluster").and_then(Value::as_str) != Some(CLUSTER)
        {
            return Err(FinError::validation(
                "Solana bounded authorization must select this wallet on mainnet-beta and a supported wallet write.",
            ));
        }
        let resource = ResourceId(format!("solana:mainnet-beta:{}", self.address));
        let native = AssetId {
            network: NETWORK.into(),
            reference: "native".into(),
            decimals: 9,
        };
        let wrapped = AssetId {
            network: NETWORK.into(),
            reference: "So11111111111111111111111111111111111111112".into(),
            decimals: 9,
        };
        match operation {
            "wallet.transfer" => {
                let recipient =
                    parse_address(input.get("recipient").and_then(Value::as_str).ok_or_else(
                        || FinError::validation("Bounded transfers require a fixed recipient."),
                    )?)?;
                if recipient == self.address {
                    return Err(FinError::validation(
                        "A bounded transfer recipient must differ from the enrolled wallet.",
                    ));
                }
                let mint = input
                    .get("mint")
                    .and_then(Value::as_str)
                    .map(parse_address)
                    .transpose()?;
                let (asset, source, destination) = if let Some(mint) = mint.as_ref() {
                    let source = binding
                        .and_then(|value| value.get("source_token_account"))
                        .and_then(Value::as_str)
                        .or_else(|| input.get("source_token_account").and_then(Value::as_str))
                        .ok_or_else(|| {
                            FinError::validation(
                                "Bounded token transfers require a fixed source token account.",
                            )
                        })?;
                    let source = parse_address(source)?;
                    if binding.is_some()
                        && input
                            .get("source_token_account")
                            .and_then(Value::as_str)
                            .is_some_and(|requested| requested != source)
                    {
                        return Err(FinError::validation(
                            "The prepared token source account differs from the requested account.",
                        ));
                    }
                    let (source_info, program) = self.read_token_account(&source).await?;
                    if source_info.token_owner != self.address || source_info.mint != *mint {
                        return Err(FinError::validation(
                            "The bounded token source account must belong to this wallet and exact mint.",
                        ));
                    }
                    let mint_response = self
                        .runtime
                        .rpc()
                        .request(
                            "getAccountInfo",
                            json!([mint,{"encoding":"base64","commitment":"finalized"}]),
                        )
                        .await?;
                    let account = mint_response
                        .get("value")
                        .filter(|value| !value.is_null())
                        .ok_or_else(crate::parse_error)?;
                    if account.get("owner").and_then(Value::as_str) != Some(program.as_str()) {
                        return Err(FinError::validation(
                            "The bounded token mint and source account use different token programs.",
                        ));
                    }
                    let data = decode_rpc_account_data(
                        account.get("data").ok_or_else(crate::parse_error)?,
                    )?;
                    let epoch = self
                        .runtime
                        .rpc()
                        .request("getEpochInfo", json!([{"commitment":"finalized"}]))
                        .await?
                        .get("epoch")
                        .and_then(Value::as_u64)
                        .ok_or_else(crate::parse_error)?;
                    let descriptor = parse_mint(&program, &data, epoch)?;
                    if let Some(binding) = binding
                        && (binding.get("mint").and_then(Value::as_str) != Some(mint.as_str())
                            || binding.get("token_program").and_then(Value::as_str)
                                != Some(program.as_str())
                            || binding.get("decimals").and_then(Value::as_u64)
                                != Some(u64::from(descriptor.decimals)))
                    {
                        return Err(FinError::validation(
                            "The prepared token identity differs from its mint account.",
                        ));
                    }
                    let destination = binding
                        .and_then(|value| value.get("destination_token_account"))
                        .and_then(Value::as_str)
                        .or_else(|| {
                            input
                                .get("destination_token_account")
                                .and_then(Value::as_str)
                        })
                        .map(parse_address)
                        .transpose()?
                        .unwrap_or(crate::associated_token_address(&recipient, mint, &program)?);
                    if binding.is_some()
                        && input
                            .get("destination_token_account")
                            .and_then(Value::as_str)
                            .is_some_and(|requested| requested != destination)
                    {
                        return Err(FinError::validation(
                            "The prepared token destination account differs from the requested account.",
                        ));
                    }
                    (
                        AssetId {
                            network: NETWORK.into(),
                            reference: mint.clone(),
                            decimals: descriptor.decimals,
                        },
                        Some(source),
                        Some(destination),
                    )
                } else {
                    (native.clone(), None, None)
                };
                Ok(WalletAuthorizationIdentity {
                    operation: operation.into(),
                    resource,
                    principal_asset: asset.clone(),
                    debit_asset: asset,
                    recipient,
                    mint,
                    source_token_account: source,
                    destination_token_account: destination,
                    token_account: None,
                })
            }
            "wallet.wrap" => {
                let account = binding
                    .and_then(|value| value.get("token_account"))
                    .and_then(Value::as_str)
                    .or_else(|| input.get("token_account").and_then(Value::as_str))
                    .map(parse_address)
                    .transpose()?
                    .unwrap_or(crate::associated_token_address(
                        &self.address,
                        &wrapped.reference,
                        TOKEN_PROGRAM_ID,
                    )?);
                if let Some(binding) = binding
                    && input
                        .get("token_account")
                        .and_then(Value::as_str)
                        .is_some_and(|requested| {
                            binding.get("token_account").and_then(Value::as_str) != Some(requested)
                        })
                {
                    return Err(FinError::validation(
                        "The prepared wrapped-SOL account differs from the requested account.",
                    ));
                }
                let account_response = self
                    .runtime
                    .rpc()
                    .request(
                        "getAccountInfo",
                        json!([account,{"encoding":"base64","commitment":"finalized"}]),
                    )
                    .await?;
                if account_response.get("value").is_some_and(Value::is_null) {
                    if account
                        != crate::associated_token_address(
                            &self.address,
                            &wrapped.reference,
                            TOKEN_PROGRAM_ID,
                        )?
                    {
                        return Err(FinError::validation(
                            "A missing wrapped-SOL account must use the wallet's associated token address.",
                        ));
                    }
                } else {
                    let (info, program) = self.read_token_account(&account).await?;
                    if program != TOKEN_PROGRAM_ID
                        || info.token_owner != self.address
                        || info.mint != wrapped.reference
                        || !info.is_native
                        || info.state != "initialized"
                        || info.delegate.is_some()
                    {
                        return Err(FinError::validation(
                            "The wrapped-SOL account identity or authority is not eligible for bounded authorization.",
                        ));
                    }
                }
                Ok(WalletAuthorizationIdentity {
                    operation: operation.into(),
                    resource,
                    principal_asset: wrapped.clone(),
                    debit_asset: native,
                    recipient: self.address.clone(),
                    mint: Some(wrapped.reference.clone()),
                    source_token_account: None,
                    destination_token_account: None,
                    token_account: Some(account),
                })
            }
            "wallet.unwrap" => {
                let account = binding
                    .and_then(|value| value.get("source_token_account"))
                    .and_then(Value::as_str)
                    .or_else(|| input.get("token_account").and_then(Value::as_str))
                    .map(parse_address)
                    .transpose()?
                    .unwrap_or(crate::associated_token_address(
                        &self.address,
                        &wrapped.reference,
                        TOKEN_PROGRAM_ID,
                    )?);
                if let Some(binding) = binding
                    && input
                        .get("token_account")
                        .and_then(Value::as_str)
                        .is_some_and(|requested| {
                            binding.get("source_token_account").and_then(Value::as_str)
                                != Some(requested)
                        })
                {
                    return Err(FinError::validation(
                        "The prepared unwrap source differs from the requested token account.",
                    ));
                }
                if binding.is_some() {
                    let (info, program) = self.read_token_account(&account).await?;
                    if program != TOKEN_PROGRAM_ID
                        || info.token_owner != self.address
                        || info.mint != wrapped.reference
                        || !info.is_native
                        || info.state != "initialized"
                        || info.delegate.is_some()
                    {
                        return Err(FinError::validation(
                            "The unwrap source account identity or authority is not eligible for bounded authorization.",
                        ));
                    }
                }
                Ok(WalletAuthorizationIdentity {
                    operation: operation.into(),
                    resource,
                    principal_asset: wrapped.clone(),
                    debit_asset: wrapped,
                    recipient: self.address.clone(),
                    mint: Some("So11111111111111111111111111111111111111112".into()),
                    source_token_account: Some(account),
                    destination_token_account: None,
                    token_account: None,
                })
            }
            _ => unreachable!(),
        }
    }

    fn amount_for_identity(
        &self,
        identity: &WalletAuthorizationIdentity,
        binding: &Value,
    ) -> Result<fin_domain::Quantity> {
        let field = if identity.operation == "wallet.transfer" && identity.mint.is_some() {
            "amount_atoms"
        } else {
            "amount_lamports"
        };
        binding
            .get(field)
            .and_then(Value::as_u64)
            .map(fin_domain::Quantity::from_u64)
            .ok_or_else(crate::parse_error)
    }

    fn wallet_boundary_spec(
        &self,
        identity: &WalletAuthorizationIdentity,
        input: &Value,
        bounds: &Value,
    ) -> Result<fin_domain::boundaries::BoundarySpec> {
        let maximum_amount = fin_domain::Quantity::from_decimal(
            bounds.get("max_amount").and_then(Value::as_str).ok_or_else(|| {
                FinError::validation("Solana wallet boundaries require max_amount in the selected asset's exact decimal units.")
            })?,
            identity.principal_asset.decimals,
        )?;
        if maximum_amount.is_zero() {
            return Err(FinError::validation(
                "Solana wallet max_amount must be positive.",
            ));
        }
        let initial_amount = fin_domain::Quantity::from_decimal(
            input.get("amount").and_then(Value::as_str).ok_or_else(|| {
                FinError::validation(
                    "Solana wallet authorization requires an exact initial amount.",
                )
            })?,
            identity.principal_asset.decimals,
        )?;
        if initial_amount.is_zero() || initial_amount > maximum_amount {
            return Err(FinError::validation(
                "The initial Solana wallet amount must be positive and within max_amount.",
            ));
        }
        let max_fee_lamports = fin_domain::Quantity::from_decimal(
            bounds
                .get("max_fee_sol")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    FinError::validation("Solana wallet boundaries require max_fee_sol.")
                })?,
            9,
        )?;
        if max_fee_lamports.is_zero() {
            return Err(FinError::validation(
                "Solana wallet max_fee_sol must be positive.",
            ));
        }
        let funding_input_key = if identity.operation == "wallet.unwrap" {
            "max_temporary_rent_sol"
        } else {
            "max_account_funding_sol"
        };
        let max_funding_lamports = match bounds.get(funding_input_key).and_then(Value::as_str) {
            Some(value) => fin_domain::Quantity::from_decimal(value, 9)?,
            None => fin_domain::Quantity::from_u64(0),
        };
        let initial_fee = input
            .get("max_fee_sol")
            .and_then(Value::as_str)
            .map(|value| fin_domain::Quantity::from_decimal(value, 9))
            .transpose()?
            .ok_or_else(|| FinError::validation("Solana wallet operation requires max_fee_sol."))?;
        let initial_funding = input
            .get(funding_input_key)
            .and_then(Value::as_str)
            .map(|value| fin_domain::Quantity::from_decimal(value, 9))
            .transpose()?
            .unwrap_or_default();
        if initial_fee.is_zero()
            || initial_fee > max_fee_lamports
            || initial_funding > max_funding_lamports
        {
            return Err(FinError::validation(
                "The initial Solana fee or account-funding cap exceeds the approved boundary.",
            ));
        }

        let fixed = json!({
            "operation":identity.operation,
            "wallet":self.name,
            "wallet_resource":identity.resource,
            "network":"solana",
            "cluster":CLUSTER,
            "asset":identity.principal_asset,
            "debit_asset":identity.debit_asset,
            "recipient":identity.recipient,
            "mint":identity.mint,
            "source_token_account":identity.source_token_account,
            "destination_token_account":identity.destination_token_account,
            "token_account":identity.token_account
        });
        let parameters = json!({
            "max_amount":maximum_amount.to_decimal(identity.principal_asset.decimals),
            "max_fee_sol":max_fee_lamports.to_decimal(9),
            (funding_input_key):max_funding_lamports.to_decimal(9),
            "amount_decimals":identity.principal_asset.decimals,
            "funding_semantics":if identity.operation == "wallet.unwrap" {"temporary_rent_refunded_in_same_transaction"} else {"explicit_separate_account_funding"}
        });
        let mut debits = vec![ResourceAmount {
            resource: identity.resource.clone(),
            amount: Amount {
                asset: identity.debit_asset.clone(),
                atoms: maximum_amount,
            },
        }];
        if !max_funding_lamports.is_zero() && identity.operation != "wallet.unwrap" {
            let native_asset = AssetId {
                network: NETWORK.into(),
                reference: "native".into(),
                decimals: 9,
            };
            if identity.debit_asset == native_asset {
                debits[0].amount.atoms =
                    debits[0].amount.atoms.checked_add(&max_funding_lamports)?;
            } else {
                debits.push(ResourceAmount {
                    resource: identity.resource.clone(),
                    amount: Amount {
                        asset: native_asset,
                        atoms: max_funding_lamports,
                    },
                });
            }
        }
        let fee_asset = AssetId {
            network: NETWORK.into(),
            reference: "native".into(),
            decimals: 9,
        };
        Ok(fin_domain::boundaries::BoundarySpec {
            fixed,
            parameters: parameters.clone(),
            maximum_debits: debits,
            maximum_costs: vec![CostBound {
                location: AccountingLocation::Cash {
                    resource: identity.resource.clone(),
                },
                amount: Amount {
                    asset: fee_asset,
                    atoms: max_fee_lamports,
                },
                treatment: CostTreatment::AdditionalDebit,
            }],
            summary: format!(
                "Allow {} from wallet {} to fixed recipient/account {} up to {} {}, with at most {} SOL network fees.",
                identity.operation,
                self.name,
                identity.recipient,
                parameters["max_amount"].as_str().unwrap_or(""),
                identity.principal_asset.reference,
                parameters["max_fee_sol"].as_str().unwrap_or("")
            ),
        })
    }

    fn validate_wallet_candidate(
        &self,
        spec: &fin_domain::boundaries::BoundarySpec,
        prepared: &PreparedOperation,
        identity: &WalletAuthorizationIdentity,
    ) -> Result<()> {
        let candidate = json!({
            "operation":identity.operation,
            "wallet":self.name,
            "wallet_resource":identity.resource,
            "network":"solana",
            "cluster":CLUSTER,
            "asset":identity.principal_asset,
            "debit_asset":identity.debit_asset,
            "recipient":identity.recipient,
            "mint":identity.mint,
            "source_token_account":identity.source_token_account,
            "destination_token_account":identity.destination_token_account,
            "token_account":identity.token_account
        });
        if candidate != spec.fixed {
            return Err(FinError::new(
                "BOUNDARY_VIOLATION", "authorization",
                "The Solana wallet candidate changes its approved wallet, network, asset, account, or recipient identity.",
                "Keep the fixed Solana identity or obtain fresh authorization.",
            ).with_details(json!({"boundary_violation":{"constraint":"fixed_identity","approved_bound":spec.fixed,"candidate_value":candidate,"reauthorization_required":true,"reconciliation_required":false}})));
        }
        if identity.operation == "wallet.transfer"
            && (prepared.binding.get("recipient").and_then(Value::as_str)
                != Some(identity.recipient.as_str())
                || prepared.binding.get("mint").and_then(Value::as_str) != identity.mint.as_deref())
        {
            return Err(FinError::validation(
                "The prepared transfer binding differs from its fixed recipient or mint.",
            ));
        }
        let parameters = &spec.parameters;
        let maximum_amount = fin_domain::Quantity::from_decimal(
            parameters
                .get("max_amount")
                .and_then(Value::as_str)
                .ok_or_else(crate::parse_error)?,
            identity.principal_asset.decimals,
        )?;
        let bound_amount = self.amount_for_identity(identity, &prepared.binding)?;
        let requested_amount = fin_domain::Quantity::from_decimal(
            prepared
                .input
                .get("amount")
                .and_then(Value::as_str)
                .ok_or_else(crate::parse_error)?,
            identity.principal_asset.decimals,
        )?;
        if bound_amount != requested_amount || bound_amount > maximum_amount {
            return Err(FinError::validation(
                "The prepared Solana amount must match the request exactly and stay within its approved maximum.",
            ));
        }
        let maximum_fee = fin_domain::Quantity::from_decimal(
            parameters
                .get("max_fee_sol")
                .and_then(Value::as_str)
                .ok_or_else(crate::parse_error)?,
            9,
        )?;
        let candidate_fee = prepared
            .input
            .get("max_fee_sol")
            .and_then(Value::as_str)
            .map(|value| fin_domain::Quantity::from_decimal(value, 9))
            .transpose()?
            .ok_or_else(crate::parse_error)?;
        if candidate_fee.is_zero() || candidate_fee > maximum_fee {
            return Err(FinError::validation(
                "The Solana wallet candidate fee cap exceeds its approved maximum.",
            ));
        }
        let bound_fee = prepared
            .binding
            .get("max_fee_lamports")
            .and_then(Value::as_u64)
            .ok_or_else(crate::parse_error)?;
        if candidate_fee
            .as_str()
            .parse::<u64>()
            .map_err(|_| crate::parse_error())?
            != bound_fee
        {
            return Err(FinError::validation(
                "The prepared Solana fee cap differs from the requested cap.",
            ));
        }
        let funding_key = if identity.operation == "wallet.unwrap" {
            "max_temporary_rent_sol"
        } else {
            "max_account_funding_sol"
        };
        let maximum_funding = parameters
            .get(funding_key)
            .and_then(Value::as_str)
            .map(|value| fin_domain::Quantity::from_decimal(value, 9))
            .transpose()?
            .unwrap_or_default();
        let candidate_funding = prepared
            .input
            .get(funding_key)
            .and_then(Value::as_str)
            .map(|value| fin_domain::Quantity::from_decimal(value, 9))
            .transpose()?
            .unwrap_or_default();
        if candidate_funding > maximum_funding {
            return Err(FinError::validation(
                "The Solana wallet candidate account-funding cap exceeds its approved maximum.",
            ));
        }
        let actual_funding = prepared
            .binding
            .get("account_funding_lamports")
            .or_else(|| prepared.binding.get("temporary_rent_lamports"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if fin_domain::Quantity::from_u64(actual_funding) > candidate_funding {
            return Err(FinError::validation(
                "Actual Solana account funding exceeds the candidate's explicit funding cap.",
            ));
        }
        if prepared.expires_at <= fin_domain::now() {
            return Err(FinError::new(
                "PREVIEW_EXPIRED",
                "authorization",
                "The Solana wallet preview has expired.",
                "Prepare a fresh candidate and recheck its approved bounds.",
            ));
        }
        Ok(())
    }
}

fn prepared_authorization_identity(
    adapter: &SolanaWalletAdapter,
    prepared: &PreparedOperation,
) -> Result<WalletAuthorizationIdentity> {
    let resource = ResourceId(format!("solana:mainnet-beta:{}", adapter.address));
    let native = AssetId {
        network: NETWORK.into(),
        reference: "native".into(),
        decimals: 9,
    };
    let wrapped = AssetId {
        network: NETWORK.into(),
        reference: "So11111111111111111111111111111111111111112".into(),
        decimals: 9,
    };
    match prepared.operation.as_str() {
        "wallet.transfer" => {
            let recipient = parse_address(
                prepared
                    .input
                    .get("recipient")
                    .and_then(Value::as_str)
                    .ok_or_else(crate::parse_error)?,
            )?;
            let mint = prepared
                .input
                .get("mint")
                .and_then(Value::as_str)
                .map(parse_address)
                .transpose()?;
            let asset = if let Some(mint) = mint.as_ref() {
                AssetId {
                    network: NETWORK.into(),
                    reference: mint.clone(),
                    decimals: prepared
                        .binding
                        .get("decimals")
                        .and_then(Value::as_u64)
                        .and_then(|value| u8::try_from(value).ok())
                        .ok_or_else(crate::parse_error)?,
                }
            } else {
                native.clone()
            };
            let source = prepared
                .binding
                .get("source_token_account")
                .and_then(Value::as_str)
                .map(parse_address)
                .transpose()?;
            let destination = prepared
                .binding
                .get("destination_token_account")
                .and_then(Value::as_str)
                .map(parse_address)
                .transpose()?;
            if prepared
                .input
                .get("source_token_account")
                .and_then(Value::as_str)
                .is_some_and(|requested| Some(requested) != source.as_deref())
                || prepared
                    .input
                    .get("destination_token_account")
                    .and_then(Value::as_str)
                    .is_some_and(|requested| Some(requested) != destination.as_deref())
            {
                return Err(FinError::validation(
                    "The prepared token account identity differs from the requested account.",
                ));
            }
            Ok(WalletAuthorizationIdentity {
                operation: prepared.operation.clone(),
                resource,
                principal_asset: asset.clone(),
                debit_asset: asset,
                recipient,
                mint,
                source_token_account: source,
                destination_token_account: destination,
                token_account: None,
            })
        }
        "wallet.wrap" => {
            let token_account = parse_address(
                prepared
                    .binding
                    .get("token_account")
                    .and_then(Value::as_str)
                    .ok_or_else(crate::parse_error)?,
            )?;
            if prepared
                .input
                .get("token_account")
                .and_then(Value::as_str)
                .is_some_and(|requested| requested != token_account)
            {
                return Err(FinError::validation(
                    "The prepared wrapped-SOL account differs from the requested account.",
                ));
            }
            Ok(WalletAuthorizationIdentity {
                operation: prepared.operation.clone(),
                resource,
                principal_asset: wrapped.clone(),
                debit_asset: native,
                recipient: adapter.address.clone(),
                mint: Some(wrapped.reference),
                source_token_account: None,
                destination_token_account: None,
                token_account: Some(token_account),
            })
        }
        "wallet.unwrap" => {
            let source_token_account = parse_address(
                prepared
                    .binding
                    .get("source_token_account")
                    .and_then(Value::as_str)
                    .ok_or_else(crate::parse_error)?,
            )?;
            if prepared
                .input
                .get("token_account")
                .and_then(Value::as_str)
                .is_some_and(|requested| requested != source_token_account)
            {
                return Err(FinError::validation(
                    "The prepared unwrap source differs from the requested token account.",
                ));
            }
            Ok(WalletAuthorizationIdentity {
                operation: prepared.operation.clone(),
                resource,
                principal_asset: wrapped.clone(),
                debit_asset: wrapped.clone(),
                recipient: adapter.address.clone(),
                mint: Some(wrapped.reference),
                source_token_account: Some(source_token_account),
                destination_token_account: None,
                token_account: None,
            })
        }
        _ => Err(FinError::new(
            "BOUNDARY_AUTHORIZATION_UNSUPPORTED",
            "authorization",
            "This Solana operation has no bounded wallet permission.",
            "Use exact-operation authorization.",
        )),
    }
}

#[async_trait]
impl Adapter for SolanaWalletAdapter {
    fn uses_rpc_reads(&self, operation: &str, input: &Value) -> bool {
        if operation == "fin.snapshot" {
            let Ok(scope) = serde_json::from_value::<SnapshotScope>(input.clone()) else {
                return false;
            };
            let connection = self.connections().into_iter().next();
            return connection.is_some_and(|connection| {
                scope.matches_network(NETWORK) && scope.matches_resource(&connection.resource)
            });
        }
        matches!(
            operation,
            "wallet.balances" | "wallet.token" | "wallet.receipt"
        ) && input.get("wallet").and_then(Value::as_str) == Some(self.name.as_str())
            && input.get("network").and_then(Value::as_str) == Some("solana")
            && input.get("cluster").and_then(Value::as_str) == Some(CLUSTER)
    }

    fn definitions(&self) -> Vec<Definition> {
        operation_definitions()
            .into_iter()
            .map(|mut definition| {
                for example in &mut definition.examples {
                    example["wallet"] = json!(self.name);
                }
                definition
            })
            .collect()
    }

    fn connections(&self) -> Vec<ConnectionMetadata> {
        vec![ConnectionMetadata {
            name: self.name.clone(),
            provider: "solana".into(),
            resource: ResourceId(format!("solana:mainnet-beta:{}", self.address)),
            role: Some("wallet".into()),
            parent: None,
            network: Some(NETWORK.into()),
            address: Some(self.address.clone()),
            credential_ref: None,
            signing_enabled: self.signer.is_some(),
            details: json!({"cluster":CLUSTER,"genesis_hash":crate::MAINNET_GENESIS_HASH,"signing_enabled":self.signer.is_some()}),
        }]
    }

    async fn snapshot(&self) -> Result<StateSnapshot> {
        self.snapshot_for_scope(&SnapshotScope::default()).await
    }
    async fn snapshot_scoped(&self, scope: &SnapshotScope) -> Result<StateSnapshot> {
        self.snapshot_for_scope(scope).await
    }

    async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
        if input.get("wallet").and_then(Value::as_str) != Some(self.name.as_str())
            || input.get("network").and_then(Value::as_str) != Some("solana")
            || input.get("cluster").and_then(Value::as_str) != Some(CLUSTER)
        {
            return Err(FinError::validation(
                "The operation must select this enrolled wallet and the Solana network.",
            ));
        }
        match operation {
            "wallet.balances" => self.balances().await,
            "wallet.token" => self.token(input).await,
            "wallet.receipt" => self.receipt(input).await,
            _ => Err(unknown_operation()),
        }
    }

    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        if operation == "wallet.wrap" {
            return self.prepare_wrap(input).await;
        }
        if operation == "wallet.unwrap" {
            return self.prepare_unwrap(input).await;
        }
        if operation == "wallet.transfer" {
            return if input.get("mint").is_some() {
                self.prepare_token_transfer(input).await
            } else {
                self.prepare_native_transfer(input).await
            };
        }
        Err(unknown_operation())
    }
    async fn prepare_boundaries(
        &self,
        prepared: &PreparedOperation,
        bounds: &Value,
    ) -> Result<fin_domain::boundaries::BoundarySpec> {
        if !matches!(
            prepared.operation.as_str(),
            "wallet.transfer" | "wallet.wrap" | "wallet.unwrap"
        ) {
            return Err(FinError::new(
                "BOUNDARY_AUTHORIZATION_UNSUPPORTED",
                "authorization",
                "Solana boundary authorization supports wallet transfer, wrap, and unwrap only.",
                "Use ordinary exact-operation approval for this operation.",
            ));
        }
        let identity = self
            .authorization_identity(
                &prepared.operation,
                &prepared.input,
                Some(&prepared.binding),
            )
            .await?;
        let spec = self.wallet_boundary_spec(&identity, &prepared.input, bounds)?;
        self.validate_wallet_candidate(&spec, prepared, &identity)?;
        Ok(spec)
    }
    async fn prepare_plan_permission(
        &self,
        operation: &str,
        input: &Value,
        bounds: &Value,
    ) -> Result<fin_domain::financial_plans::PlanPermission> {
        if !matches!(
            operation,
            "wallet.transfer" | "wallet.wrap" | "wallet.unwrap"
        ) {
            return Err(FinError::new(
                "PLAN_PERMISSION_UNSUPPORTED",
                "authorization",
                "Solana named-plan permission supports wallet transfer, wrap, and unwrap only.",
                "Choose one of the supported Solana wallet write operations.",
            ));
        }
        let identity = self.authorization_identity(operation, input, None).await?;
        let spec = self.wallet_boundary_spec(&identity, input, bounds)?;
        let possible_credits = match operation {
            "wallet.wrap" => vec![fin_domain::financial_plans::FinancialPlanFundingIdentity {
                resource: identity.resource.clone(),
                asset: identity.principal_asset.clone(),
            }],
            "wallet.unwrap" => vec![fin_domain::financial_plans::FinancialPlanFundingIdentity {
                resource: identity.resource.clone(),
                asset: AssetId {
                    network: NETWORK.into(),
                    reference: "native".into(),
                    decimals: 9,
                },
            }],
            _ => Vec::new(),
        };
        Ok(fin_domain::financial_plans::PlanPermission {
            recovery_parent: None,
            spec,
            possible_credits,
            fee_assets: vec![AssetId {
                network: NETWORK.into(),
                reference: "native".into(),
                decimals: 9,
            }],
            deferred_receipt: None,
            authorization_seconds: None,
            cumulative_scope: None,
        })
    }
    fn validate_boundary_candidate(
        &self,
        spec: &fin_domain::boundaries::BoundarySpec,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        let identity = prepared_authorization_identity(self, prepared)?;
        self.validate_wallet_candidate(spec, prepared, &identity)
    }
    fn preparation_reuse_window(
        &self,
        prepared: &PreparedOperation,
    ) -> Option<std::time::Duration> {
        matches!(
            prepared.operation.as_str(),
            "wallet.transfer" | "wallet.wrap" | "wallet.unwrap"
        )
        .then_some(std::time::Duration::from_secs(30))
    }
    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        let frozen = self.freeze_prepared(prepared).await?;
        self.runtime.validate_fee(&frozen).await?;
        self.runtime.simulate(&frozen).await?;
        Ok(prepared.clone())
    }
    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        let signer = self.signer.as_ref().ok_or_else(|| {
            failure(
                "SOLANA_WALLET_READ_ONLY",
                "dispatch",
                "This enrolled wallet is read-only.",
                "Enroll a protected Solana signer before dispatch.",
            )
        })?;
        let frozen = self.freeze_prepared(prepared).await?;
        self.runtime.validate_fee(&frozen).await?;
        self.runtime.simulate(&frozen).await?;
        self.runtime
            .dispatch(&frozen, signer, prepared, journal)
            .await
    }
    async fn submit_revalidated(
        &self,
        validated: &fin_domain::RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        validated.assert_fresh()?;
        let prepared = validated.prepared();
        let signer = self.signer.as_ref().ok_or_else(|| {
            failure(
                "SOLANA_WALLET_READ_ONLY",
                "dispatch",
                "This enrolled wallet is read-only.",
                "Enroll a protected Solana signer before dispatch.",
            )
        })?;
        let frozen = self.freeze_prepared(prepared).await?;
        self.runtime.validate_fee(&frozen).await?;
        validated.assert_fresh()?;
        self.runtime
            .dispatch(&frozen, signer, prepared, journal)
            .await
    }
    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
        let Some(submission) = record.submission.as_ref() else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"No durable Solana transaction was recorded."}),
                verified: None,
            });
        };
        if !matches!(
            record.operation.as_str(),
            "wallet.transfer" | "wallet.wrap" | "wallet.unwrap"
        ) {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"This Solana operation has no settlement verifier."}),
                verified: None,
            });
        }
        let outcome = self
            .runtime
            .reconcile_submission(submission, record.dispatch_evidence.as_ref())
            .await?;
        if let crate::ReceiptOutcome::NotDispatched {
            reason_code,
            reason,
        } = &outcome
        {
            let preview = record.preview.as_ref().ok_or_else(crate::parse_error)?;
            let expected_hash = preview
                .binding
                .get("message_sha256")
                .and_then(Value::as_str)
                .ok_or_else(crate::parse_error)?;
            if submission
                .data
                .get("message_sha256")
                .and_then(Value::as_str)
                != Some(expected_hash)
            {
                return Err(failure(
                    "SOLANA_BINDING_MISMATCH",
                    "reconciliation",
                    "The no-send evidence does not match the authorized immutable transaction.",
                    "Keep the operation unresolved and inspect its original journal record.",
                ));
            }
            let evidence = json!({"state":"not_dispatched","reason_code":reason_code,"reason":reason,"message_sha256":expected_hash,"submission_reference":submission.reference});
            let verified = VerifiedEffects {
                evidence_version: 1,
                submission: submission.clone(),
                effects: Vec::new(),
                proof_digest: String::new(),
                observed_at: fin_domain::now(),
                evidence: evidence.clone(),
            };
            let verified = VerifiedEffects {
                proof_digest: fin_domain::verified_effect_digest(&verified)?,
                ..verified
            };
            return Ok(Reconciliation {
                status: Status::Failed,
                evidence,
                verified: Some(verified),
            });
        }
        let crate::ReceiptOutcome::Finalized {
            signature,
            slot,
            fee_lamports,
            failed,
            proof,
            ..
        } = outcome
        else {
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence: serde_json::to_value(outcome).unwrap_or(Value::Null),
                verified: None,
            });
        };
        let preview = record.preview.as_ref().ok_or_else(crate::parse_error)?;
        if submission
            .data
            .get("message_sha256")
            .and_then(Value::as_str)
            != preview
                .binding
                .get("message_sha256")
                .and_then(Value::as_str)
        {
            return Err(failure(
                "SOLANA_BINDING_MISMATCH",
                "reconciliation",
                "The durable submission hash does not match the reviewed operation binding.",
                "Keep the submission unresolved and inspect its original journal record.",
            ));
        }
        let native = AssetId {
            network: NETWORK.into(),
            reference: "native".into(),
            decimals: 9,
        };
        let source = ResourceId(format!("solana:mainnet-beta:{}", self.address));
        let mut effects = Vec::new();
        if record.operation == "wallet.wrap" {
            let amount = preview
                .binding
                .get("amount_lamports")
                .and_then(Value::as_u64)
                .ok_or_else(crate::parse_error)?;
            let account = preview
                .binding
                .get("token_account")
                .and_then(Value::as_str)
                .ok_or_else(crate::parse_error)?;
            let wallet_delta = proof.lamport_delta(&self.address)?;
            let token_delta = proof.token_account_delta_verified(
                account,
                &self.address,
                "So11111111111111111111111111111111111111112",
                TOKEN_PROGRAM_ID,
                9,
            )?;
            let account_delta = proof.lamport_delta(account)?;
            let account_funding = preview
                .binding
                .get("account_funding_lamports")
                .and_then(Value::as_u64)
                .ok_or_else(crate::parse_error)?;
            if !failed {
                let required = amount
                    .checked_add(fee_lamports)
                    .and_then(|value| value.checked_add(account_funding))
                    .ok_or_else(crate::parse_error)?;
                let expected_account_credit = amount
                    .checked_add(account_funding)
                    .ok_or_else(crate::parse_error)?;
                if wallet_delta != fin_domain::SignedQuantity::new(format!("-{required}"))?
                    || token_delta != fin_domain::SignedQuantity::new(amount.to_string())?
                    || account_delta
                        != fin_domain::SignedQuantity::new(expected_account_credit.to_string())?
                {
                    return Err(failure(
                        "SOLANA_EFFECT_MISMATCH",
                        "reconciliation",
                        "The finalized balances do not prove the exact native-SOL to wrapped-SOL conversion.",
                        "Keep the submission unresolved and inspect the finalized receipt.",
                    ));
                }
                effects.push(FinancialEffect::Conversion {
                    resource: source.clone(),
                    spent: Amount {
                        asset: native.clone(),
                        atoms: fin_domain::Quantity::from_u64(amount),
                    },
                    received: Amount {
                        asset: AssetId {
                            network: NETWORK.into(),
                            reference: "So11111111111111111111111111111111111111112".into(),
                            decimals: 9,
                        },
                        atoms: fin_domain::Quantity::from_u64(amount),
                    },
                });
                if account_funding > 0 {
                    let principal = fin_domain::Quantity::from_u64(account_funding);
                    let position = format!("account-funding:{account}");
                    let anchor = PositionAnchor {
                        resource: source.clone(),
                        position: position.clone(),
                        position_kind: fin_domain::PositionKind::RefundableAccountFunding,
                        principal: Amount {
                            asset: native.clone(),
                            atoms: principal.clone(),
                        },
                        scaled_receipt: None,
                        evidence_version: 1,
                        evidence: json!({
                            "cluster":CLUSTER,
                            "account":account,
                            "account_owner":TOKEN_PROGRAM_ID,
                            "token_owner":self.address,
                            "refund_authority":self.address,
                            "rent_lamports":account_funding,
                            "signature":signature,
                            "message_sha256":proof.message_sha256
                        }),
                        observed_at: chrono::Utc::now().to_rfc3339(),
                    };
                    effects.push(FinancialEffect::OriginMovement {
                        from: AccountingLocation::Cash {
                            resource: source.clone(),
                        },
                        to: AccountingLocation::Position {
                            resource: source.clone(),
                            position: position.clone(),
                            position_kind: fin_domain::PositionKind::RefundableAccountFunding,
                        },
                        debit: Amount {
                            asset: native.clone(),
                            atoms: principal.clone(),
                        },
                        credit: Amount {
                            asset: native.clone(),
                            atoms: principal.clone(),
                        },
                    });
                    effects.push(FinancialEffect::CashDebit {
                        resource: source.clone(),
                        amount: Amount {
                            asset: native.clone(),
                            atoms: principal.clone(),
                        },
                    });
                    effects.push(FinancialEffect::PositionChange {
                        resource: source.clone(),
                        position,
                        asset: native.clone(),
                        delta: fin_domain::SignedQuantity::positive(&principal),
                        scaled_receipt: None,
                        anchor: Some(anchor),
                    });
                }
            } else if wallet_delta != fin_domain::SignedQuantity::new(format!("-{fee_lamports}"))?
                || !token_delta.is_zero()
                || !account_delta.is_zero()
            {
                return Err(failure(
                    "SOLANA_EFFECT_MISMATCH",
                    "reconciliation",
                    "The failed wrap transaction has unexpected balance effects.",
                    "Keep the submission unresolved and inspect the finalized receipt.",
                ));
            }
        } else if record.operation == "wallet.unwrap" {
            let amount = preview
                .binding
                .get("amount_lamports")
                .and_then(Value::as_u64)
                .ok_or_else(crate::parse_error)?;
            let token_account = preview
                .binding
                .get("source_token_account")
                .and_then(Value::as_str)
                .ok_or_else(crate::parse_error)?;
            let temporary = preview
                .binding
                .get("temporary_account")
                .and_then(Value::as_str)
                .ok_or_else(crate::parse_error)?;
            let wallet_delta = proof.lamport_delta(&self.address)?;
            let token_delta = proof.token_account_delta_verified(
                token_account,
                &self.address,
                "So11111111111111111111111111111111111111112",
                TOKEN_PROGRAM_ID,
                9,
            )?;
            let temporary_delta = proof.lamport_delta(temporary)?;
            if !failed {
                let expected_wallet_delta = i128::from(amount) - i128::from(fee_lamports);
                if wallet_delta
                    != fin_domain::SignedQuantity::new(expected_wallet_delta.to_string())?
                    || token_delta != fin_domain::SignedQuantity::new(format!("-{amount}"))?
                    || !temporary_delta.is_zero()
                {
                    return Err(failure(
                        "SOLANA_EFFECT_MISMATCH",
                        "reconciliation",
                        "The finalized balances do not prove the exact partial wrapped-SOL unwrap and temporary-account close.",
                        "Keep the submission unresolved and inspect the finalized receipt.",
                    ));
                }
                effects.push(FinancialEffect::Conversion {
                    resource: source.clone(),
                    spent: Amount {
                        asset: AssetId {
                            network: NETWORK.into(),
                            reference: "So11111111111111111111111111111111111111112".into(),
                            decimals: 9,
                        },
                        atoms: fin_domain::Quantity::from_u64(amount),
                    },
                    received: Amount {
                        asset: native.clone(),
                        atoms: fin_domain::Quantity::from_u64(amount),
                    },
                });
            } else if wallet_delta != fin_domain::SignedQuantity::new(format!("-{fee_lamports}"))?
                || !token_delta.is_zero()
                || !temporary_delta.is_zero()
            {
                return Err(failure(
                    "SOLANA_EFFECT_MISMATCH",
                    "reconciliation",
                    "The failed unwrap transaction has unexpected balance effects.",
                    "Keep the submission unresolved and inspect the finalized receipt.",
                ));
            }
        } else if let Some(mint) = preview.binding.get("mint").and_then(Value::as_str) {
            let gross = preview.binding["amount_atoms"]
                .as_u64()
                .ok_or_else(crate::parse_error)?;
            let net = preview.binding["expected_credit_atoms"]
                .as_u64()
                .ok_or_else(crate::parse_error)?;
            let recipient = preview
                .binding
                .get("recipient")
                .and_then(Value::as_str)
                .ok_or_else(crate::parse_error)?;
            let source_owner = preview.binding["source_owner"]
                .as_str()
                .ok_or_else(crate::parse_error)?;
            let token_source = preview.binding["source_token_account"]
                .as_str()
                .ok_or_else(crate::parse_error)?;
            let token_destination = preview.binding["destination_token_account"]
                .as_str()
                .ok_or_else(crate::parse_error)?;
            let created = preview
                .binding
                .get("destination_account_created")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let account_funding_cap = if created && !failed {
                preview
                    .binding
                    .get("account_funding_lamports")
                    .and_then(Value::as_u64)
                    .ok_or_else(crate::parse_error)?
            } else {
                0
            };
            let destination_rent_delta = proof.lamport_delta(token_destination)?;
            let actual_account_funding = if !created || failed {
                if !destination_rent_delta.is_zero() {
                    return Err(failure(
                        "SOLANA_EFFECT_MISMATCH",
                        "reconciliation",
                        "The finalized destination account has an unexpected rent balance change.",
                        "Keep the submission unresolved and inspect the finalized receipt.",
                    ));
                }
                0
            } else {
                let delta = destination_rent_delta
                    .as_str()
                    .parse::<u64>()
                    .map_err(|_| crate::parse_error())?;
                if delta != 0 && delta != account_funding_cap {
                    return Err(failure(
                        "SOLANA_EFFECT_MISMATCH",
                        "reconciliation",
                        "The finalized destination account rent differs from the reviewed funding bound.",
                        "Keep the submission unresolved and inspect the finalized receipt.",
                    ));
                }
                delta
            };
            let wallet_delta = proof.lamport_delta(&self.address)?;
            let zero = fin_domain::SignedQuantity::new("0")?;
            if source_owner != self.address
                || preview
                    .binding
                    .get("decimals")
                    .and_then(Value::as_u64)
                    .is_none()
                || wallet_delta
                    != fin_domain::SignedQuantity::new(format!(
                        "-{}",
                        fee_lamports
                            .checked_add(actual_account_funding)
                            .ok_or_else(crate::parse_error)?
                    ))?
                || proof.token_account_delta_verified(
                    token_source,
                    source_owner,
                    mint,
                    preview.binding["token_program"]
                        .as_str()
                        .ok_or_else(crate::parse_error)?,
                    preview.binding["decimals"]
                        .as_u64()
                        .ok_or_else(crate::parse_error)? as u8,
                )? != if failed {
                    zero.clone()
                } else {
                    fin_domain::SignedQuantity::new(format!("-{gross}"))?
                }
                || proof.token_account_delta_verified(
                    token_destination,
                    recipient,
                    mint,
                    preview.binding["token_program"]
                        .as_str()
                        .ok_or_else(crate::parse_error)?,
                    preview.binding["decimals"]
                        .as_u64()
                        .ok_or_else(crate::parse_error)? as u8,
                )? != if failed {
                    zero
                } else {
                    fin_domain::SignedQuantity::new(net.to_string())?
                }
            {
                return Err(failure(
                    "SOLANA_EFFECT_MISMATCH",
                    "reconciliation",
                    "The finalized token balances do not match the authorized owners, mint, amount, and fee.",
                    "Keep the submission unresolved and inspect the finalized receipt.",
                ));
            }
            if !failed {
                let asset = AssetId {
                    network: NETWORK.into(),
                    reference: mint.into(),
                    decimals: preview.binding["decimals"]
                        .as_u64()
                        .ok_or_else(crate::parse_error)? as u8,
                };
                effects.push(FinancialEffect::Transfer {
                    from: source.clone(),
                    to: ResourceId(format!("solana:mainnet-beta:{recipient}")),
                    debit: Amount {
                        asset: asset.clone(),
                        atoms: fin_domain::Quantity::from_u64(gross),
                    },
                    credit: Amount {
                        asset,
                        atoms: fin_domain::Quantity::from_u64(net),
                    },
                });
                if actual_account_funding > 0 {
                    effects.push(FinancialEffect::Transfer {
                        from: source.clone(),
                        to: ResourceId(format!("solana:mainnet-beta:account:{token_destination}")),
                        debit: Amount {
                            asset: native.clone(),
                            atoms: fin_domain::Quantity::from_u64(actual_account_funding),
                        },
                        credit: Amount {
                            asset: native.clone(),
                            atoms: fin_domain::Quantity::from_u64(actual_account_funding),
                        },
                    });
                }
            }
        } else {
            let amount_atoms = preview
                .binding
                .get("amount_lamports")
                .and_then(Value::as_u64)
                .ok_or_else(crate::parse_error)?;
            let recipient = preview
                .binding
                .get("recipient")
                .and_then(Value::as_str)
                .ok_or_else(crate::parse_error)?;
            let wallet_delta = proof.lamport_delta(&self.address)?;
            let recipient_delta = proof.lamport_delta(recipient)?;
            if !failed {
                let debit = amount_atoms
                    .checked_add(fee_lamports)
                    .ok_or_else(crate::parse_error)?;
                if wallet_delta != fin_domain::SignedQuantity::new(format!("-{debit}"))?
                    || recipient_delta != fin_domain::SignedQuantity::new(amount_atoms.to_string())?
                {
                    return Err(failure(
                        "SOLANA_EFFECT_MISMATCH",
                        "reconciliation",
                        "The finalized transaction balances do not match the authorized SOL transfer and fee.",
                        "Keep the submission unresolved and inspect the finalized receipt.",
                    ));
                }
                effects.push(FinancialEffect::Transfer {
                    from: source.clone(),
                    to: ResourceId(format!("solana:mainnet-beta:{recipient}")),
                    debit: Amount {
                        asset: native.clone(),
                        atoms: fin_domain::Quantity::from_u64(amount_atoms),
                    },
                    credit: Amount {
                        asset: native.clone(),
                        atoms: fin_domain::Quantity::from_u64(amount_atoms),
                    },
                });
            } else if wallet_delta != fin_domain::SignedQuantity::new(format!("-{fee_lamports}"))?
                || !recipient_delta.is_zero()
            {
                return Err(failure(
                    "SOLANA_EFFECT_MISMATCH",
                    "reconciliation",
                    "The failed finalized transaction has unexpected balance effects.",
                    "Keep the submission unresolved and inspect the finalized receipt.",
                ));
            }
        }
        if fee_lamports
            > preview
                .binding
                .get("max_fee_lamports")
                .and_then(Value::as_u64)
                .ok_or_else(crate::parse_error)?
        {
            return Err(failure(
                "SOLANA_FEE_LIMIT_EXCEEDED",
                "reconciliation",
                "The finalized fee exceeds the authorized maximum.",
                "Keep the submission unresolved and inspect the original fee limit.",
            ));
        }
        if fee_lamports > 0 {
            effects.push(FinancialEffect::Cost {
                location: AccountingLocation::Cash { resource: source },
                amount: Amount {
                    asset: native,
                    atoms: fin_domain::Quantity::from_u64(fee_lamports),
                },
                category: "solana_network_fee".into(),
                treatment: CostTreatment::AdditionalDebit,
            });
        }
        let evidence = json!({"signature":signature,"slot":slot,"fee_lamports":fee_lamports,"failed":failed,"proof":proof});
        let mut verified = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects,
            evidence: evidence.clone(),
            observed_at: fin_domain::now(),
            proof_digest: String::new(),
        };
        verified.proof_digest = fin_domain::verified_effect_digest(&verified)?;
        Ok(Reconciliation {
            status: if failed {
                Status::Failed
            } else {
                Status::Completed
            },
            evidence,
            verified: Some(verified),
        })
    }
}

/// Offline operation metadata used by CLI discovery and broker composition.
pub fn operation_definitions() -> Vec<Definition> {
    let mut definitions = ["wallet.balances", "wallet.token", "wallet.receipt", "wallet.transfer"].into_iter().map(|name| {
        let token = name == "wallet.token";
        let receipt = name == "wallet.receipt";
        let transfer = name == "wallet.transfer";
        Definition {
            name:name.into(),
            description:if token {"Inspect a finalized mainnet-beta SPL Token or Token-2022 mint account.".into()} else if receipt {"Reconcile one Solana transaction signature against its persisted message hash and finalized receipt.".into()} else if transfer {"Transfer an exact amount of native SOL to a base58 recipient on Solana mainnet-beta.".into()} else {"Read finalized native SOL and every wallet-owned SPL Token and Token-2022 account.".into()},
            kind:if transfer {OperationKind::Write} else {OperationKind::Read},
            execution_actor:if transfer {ExecutionActor::AgentInvocable} else {ExecutionActor::AgentReadable},
            input_schema:if token {json!({"type":"object","additionalProperties":false,"required":["wallet","network","cluster","mint"],"properties":{"wallet":{"type":"string","minLength":1},"network":{"const":"solana"},"cluster":{"const":"mainnet-beta"},"mint":{"type":"string","maxLength":44}}})} else if receipt {json!({"type":"object","additionalProperties":false,"required":["wallet","network","cluster","signature","message_sha256"],"properties":{"wallet":{"type":"string"},"network":{"const":"solana"},"cluster":{"const":"mainnet-beta"},"signature":{"type":"string","maxLength":88},"message_sha256":{"type":"string","pattern":"^[a-f0-9]{64}$"}}})} else if transfer {json!({"type":"object","additionalProperties":false,"required":["wallet","network","cluster","recipient","amount","max_fee_sol","requestId"],"properties":{"wallet":{"type":"string","minLength":1},"network":{"const":"solana"},"cluster":{"const":"mainnet-beta"},"recipient":{"type":"string","maxLength":44},"amount":{"type":"string","pattern":"^[0-9]+(\\.[0-9]{1,18})?$","description":"Native SOL uses 9 decimals; token amounts are checked against the finalized mint's decimals."},"max_fee_sol":{"type":"string","pattern":"^[0-9]+(\\.[0-9]{1,9})?$"},"max_account_funding_sol":{"type":"string","pattern":"^[0-9]+(\\.[0-9]{1,9})?$","description":"Required when Fin must create the recipient associated token account; this funds the recipient's refundable rent."},"requestId":{"type":"string","minLength":1,"maxLength":128},"mint":{"type":"string","maxLength":44},"source_token_account":{"type":"string","maxLength":44},"destination_token_account":{"type":"string","maxLength":44}}})} else {json!({"type":"object","additionalProperties":false,"required":["wallet","network","cluster"],"properties":{"wallet":{"type":"string","minLength":1},"network":{"const":"solana"},"cluster":{"const":"mainnet-beta"}}})},
            output_schema:if token {json!({"type":"object","required":["network","cluster","mint","observed_commitment","descriptor"],"properties":{"network":{"const":NETWORK},"cluster":{"const":CLUSTER},"mint":{"type":"string","maxLength":44},"observed_commitment":{"const":"finalized"},"descriptor":{"type":"object","required":["program_id","supply_atoms","decimals","extensions","transfer_supported_by_runtime"],"properties":{"program_id":{"enum":[TOKEN_PROGRAM_ID,TOKEN_2022_PROGRAM_ID]},"supply_atoms":{"type":"string","pattern":"^[0-9]+$"},"decimals":{"type":"integer","minimum":0,"maximum":255},"mint_authority":{"type":["string","null"]},"freeze_authority":{"type":["string","null"]},"extensions":{"type":"array","items":{"type":"string"}},"transfer_supported_by_runtime":{"type":"boolean"},"transfer_limitation":{"type":["string","null"]},"transfer_fee":{"type":["object","null"],"properties":{"epoch":{"type":"integer","minimum":0},"basis_points":{"type":"integer","minimum":0,"maximum":10000},"maximum_fee_atoms":{"type":"string","pattern":"^[0-9]+$"}}},"transfer_hook_program":{"type":["string","null"]},"paused":{"type":["boolean","null"]}}}}})} else if receipt {json!({"type":"object","required":["state"],"properties":{"state":{"enum":["pending","finalized","not_dispatched"]},"confirmed":{"type":"boolean"},"reason_code":{"type":"string"},"reason":{"type":"string"},"signature":{"type":"string","maxLength":88},"slot":{"type":"integer","minimum":0},"fee_lamports":{"type":"integer","minimum":0},"failed":{"type":"boolean"},"message_sha256":{"type":"string","pattern":"^[a-f0-9]{64}$"},"proof":{"type":"object","properties":{"account_keys":{"type":"array","items":{"type":"string"}},"message_instructions":{"type":"array","items":{"type":"object","required":["program_id","accounts","data_base64"]}},"pre_balances_lamports":{"type":"array","items":{"type":"integer","minimum":0}},"post_balances_lamports":{"type":"array","items":{"type":"integer","minimum":0}},"pre_token_balances":{"type":"array"},"post_token_balances":{"type":"array"},"inner_instructions":{"type":"array"},"log_messages":{"type":"array","items":{"type":"string"}}}}}})} else if transfer {json!({"type":"object","required":["operation","binding","intent"],"properties":{"operation":{"const":"wallet.transfer"},"binding":{"type":"object","required":["message_sha256"],"properties":{"message_sha256":{"type":"string","pattern":"^[a-f0-9]{64}$"},"amount_atoms":{"type":"integer","minimum":0},"amount_lamports":{"type":"integer","minimum":0},"mint":{"type":"string"},"decimals":{"type":"integer","minimum":0,"maximum":255},"max_fee_lamports":{"type":"integer","minimum":0},"destination_account_created":{"type":"boolean"},"account_funding_lamports":{"type":"integer","minimum":0}}},"intent":{"type":"object","required":["debits","expected","maximum_costs"]}}})} else {json!({"type":"object","required":["network","cluster","wallet","observed_commitment","native","token_accounts","coverage"],"properties":{"network":{"const":NETWORK},"cluster":{"const":CLUSTER},"wallet":{"type":"string","maxLength":44},"observed_commitment":{"const":"finalized"},"native":{"type":"object","required":["asset","amount_atoms"],"properties":{"asset":{"type":"object","required":["network","reference","decimals"]},"amount_atoms":{"type":"string","pattern":"^[0-9]+$"}}},"token_accounts":{"type":"array","items":{"type":"object","required":["address","asset","amount_atoms","state","token_owner","program_id","delegate","delegated_amount_atoms","close_authority","extensions"],"properties":{"address":{"type":"string","maxLength":44},"asset":{"type":"object","required":["network","reference","decimals"]},"amount_atoms":{"type":"string","pattern":"^[0-9]+$"},"state":{"enum":["initialized","frozen"]},"token_owner":{"type":"string","maxLength":44},"program_id":{"enum":[TOKEN_PROGRAM_ID,TOKEN_2022_PROGRAM_ID]},"delegate":{"type":["string","null"]},"delegated_amount_atoms":{"type":"string","pattern":"^[0-9]+$"},"close_authority":{"type":["string","null"]},"withheld_transfer_fee_atoms":{"type":["string","null"]},"memo_required":{"type":"boolean"},"allow_non_confidential_credits":{"type":["boolean","null"]},"extensions":{"type":"array","items":{"type":"string"}}}}},"coverage":{"type":"object","required":["kind","programs","address_associated_only","portfolio_complete","scope_note"],"properties":{"kind":{"const":"wallet_owned_spl_accounts"},"programs":{"type":"array","items":{"type":"string"}},"address_associated_only":{"const":false},"complete_for_program_account_queries":{"type":"boolean"},"portfolio_complete":{"const":false},"scope_note":{"type":"string"}}}}})},
            error_schema:json!({"type":"object","required":["code","stage","message","next_action"],"properties":{"code":{"type":"string"},"stage":{"type":"string"},"message":{"type":"string"},"next_action":{"type":"string"}}}),
            examples:if token {vec![json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","mint":"So11111111111111111111111111111111111111112"})]} else if receipt {vec![json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","signature":"5Z...","message_sha256":"0000000000000000000000000000000000000000000000000000000000000000"})]} else if transfer {vec![json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","recipient":"11111111111111111111111111111111","amount":"0.01","max_fee_sol":"0.00001","requestId":"stable-attempt-id"})]} else {vec![json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta"})]},
            coverage:if token {"One binary-verified mint account at finalized commitment, including extension metadata and transfer limitations.".into()} else if receipt {"Finalized transaction status plus cryptographically verified message/signatures and balance/token effect evidence.".into()} else if transfer {"Native SOL and SPL Token/Token-2022 transfers. Exact amounts, stable request identity, explicit network fee cap, simulation, extension-aware fee/memo/hook handling and finalized balance-delta verification.".into()} else {"Wallet-owned native SOL and token accounts selected by both SPL Token program owners, including non-associated token accounts. External application positions are separate.".into()},
            completion:if transfer {"A successful RPC response is not completion; settle only after the original signature is finalized and exact SOL debits, recipient credit and network fee are verified.".into()} else {"Read-only finalized mainnet-beta observation. Amounts are exact atom strings.".into()},
            semantics:json!({"network":NETWORK,"cluster":CLUSTER,"commitment":"finalized","financial_movement":transfer,"account_discovery":"All owned accounts for both supported token programs; not ATA-only.","portfolio_scope":"Wallet-owned tokens only; application positions are reported by their application observer.","implemented_transfer_assets":["native SOL","SPL Token","Token-2022"],"bounded_authorization":if transfer {json!({"supported":true,"scope":"fixed wallet, mainnet-beta cluster, recipient, mint and selected token accounts","bounds":{"max_amount":"exact decimal in the selected asset's units","max_fee_sol":"maximum SOL network fee","max_account_funding_sol":"optional separate account funding cap"},"candidate_request_id":"may vary per attempt","plan_permission_balance_independent":true,"plan_authorization_duration":"finite parent plan duration; candidate freshness and blockhash expiry remain independent"})} else {Value::Null}}),
        }
    }).collect::<Vec<_>>();
    definitions.push(Definition {
        name:"wallet.wrap".into(), description:"Convert native SOL to wrapped SOL, creating the wallet's associated wrapped-SOL account when needed under an explicit rent cap.".into(), kind:OperationKind::Write, execution_actor:ExecutionActor::AgentInvocable,
        input_schema:json!({"type":"object","additionalProperties":false,"required":["wallet","network","cluster","amount","max_fee_sol","requestId"],"properties":{"wallet":{"type":"string","minLength":1},"network":{"const":"solana"},"cluster":{"const":"mainnet-beta"},"amount":{"type":"string","pattern":"^[0-9]+(\\.[0-9]{1,9})?$"},"max_fee_sol":{"type":"string","pattern":"^[0-9]+(\\.[0-9]{1,9})?$"},"max_account_funding_sol":{"type":"string","pattern":"^[0-9]+(\\.[0-9]{1,9})?$","description":"Required only when Fin must create the wallet's wrapped-SOL associated account; refundable rent remains wallet-owned."},"token_account":{"type":"string","maxLength":44},"requestId":{"type":"string","minLength":1,"maxLength":128}}}),
        output_schema:json!({"type":"object","required":["operation","binding","intent"],"properties":{"operation":{"const":"wallet.wrap"},"binding":{"type":"object"},"intent":{"type":"object"}}}),
        error_schema:json!({"type":"object","required":["code","stage","message","next_action"],"properties":{"code":{"type":"string"},"stage":{"type":"string"},"message":{"type":"string"},"next_action":{"type":"string"}}}),
        examples:vec![json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","amount":"0.1","max_fee_sol":"0.00001","requestId":"stable-wrap-id"})], coverage:"Exact conversion to a wallet-owned wrapped-SOL token account, idempotent associated-account creation when absent under an explicit rent cap, simulation, durable identity, and finalized native/token proof.".into(), completion:"Settles only after finalized SOL lamport and wrapped-token deltas match the conversion, any new wallet-owned rent funding, and network fee.".into(), semantics:json!({"network":NETWORK,"cluster":CLUSTER,"commitment":"finalized","token_mint":"So11111111111111111111111111111111111111112","account_creation":"Creates only the wallet's associated token address, with bounded rent funding; existing accounts are preserved.","bounded_authorization":{"supported":true,"scope":"fixed wallet, cluster, wrapped-SOL mint and wallet token account","bounds":{"max_amount":"exact SOL quantity","max_fee_sol":"maximum SOL network fee","max_account_funding_sol":"separate wallet-owned rent funding cap"},"plan_permission_balance_independent":true,"plan_authorization_duration":"finite parent plan duration; candidate freshness and blockhash expiry remain independent"}}),
    });
    definitions.push(Definition {
        name: "wallet.unwrap".into(),
        description: "Convert an exact amount of wallet-owned wrapped SOL back to native SOL without closing the existing token account.".into(),
        kind: OperationKind::Write,
        execution_actor: ExecutionActor::AgentInvocable,
        input_schema: json!({"type":"object","additionalProperties":false,"required":["wallet","network","cluster","amount","max_fee_sol","max_temporary_rent_sol","requestId"],"properties":{"wallet":{"type":"string","minLength":1},"network":{"const":"solana"},"cluster":{"const":"mainnet-beta"},"amount":{"type":"string","pattern":"^[0-9]+(\\.[0-9]{1,9})?$","description":"Exact wrapped-SOL amount, up to 9 decimals."},"max_fee_sol":{"type":"string","pattern":"^[0-9]+(\\.[0-9]{1,9})?$"},"max_temporary_rent_sol":{"type":"string","pattern":"^[0-9]+(\\.[0-9]{1,9})?$","description":"Bounds temporary SOL liquidity; the request-bound account closes in the same transaction and returns this rent."},"token_account":{"type":"string","maxLength":44},"requestId":{"type":"string","minLength":1,"maxLength":128}}}),
        output_schema: json!({"type":"object","required":["operation","binding","intent"],"properties":{"operation":{"const":"wallet.unwrap"},"binding":{"type":"object","required":["message_sha256","amount_lamports","source_token_account","temporary_account","temporary_rent_lamports"]},"intent":{"type":"object"}}}),
        error_schema: json!({"type":"object","required":["code","stage","message","next_action"],"properties":{"code":{"type":"string"},"stage":{"type":"string"},"message":{"type":"string"},"next_action":{"type":"string"}}}),
        examples: vec![json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","amount":"0.05","max_fee_sol":"0.00001","max_temporary_rent_sol":"0.003","requestId":"stable-unwrap-id"})],
        coverage: "Partial unwrap from an existing initialized wallet-owned classic wrapped-SOL account. Uses a request-bound create-with-seed temporary token account, returns its rent by closing that newly created account in the same transaction, and proves exact finalized token/native deltas.".into(),
        completion: "Settles only after the exact original request is finalized, the existing wrapped-SOL balance falls by the approved amount, native SOL rises by amount less the actual fee, and the temporary account is closed.".into(),
        semantics: json!({"network":NETWORK,"cluster":CLUSTER,"commitment":"finalized","token_mint":"So11111111111111111111111111111111111111112","existing_account":"Preserved; partial unwraps are supported.","temporary_account":"Deterministic for requestId, created and closed in the same transaction.","temporary_rent":"Explicitly capped as transient liquidity and returned on successful close.","bounded_authorization":{"supported":true,"scope":"fixed wallet, cluster, wrapped-SOL mint and source token account","bounds":{"max_amount":"exact wrapped-SOL quantity","max_fee_sol":"maximum SOL network fee","max_temporary_rent_sol":"separate transient liquidity cap"},"plan_permission_balance_independent":true,"plan_authorization_duration":"finite parent plan duration; candidate freshness and blockhash expiry remain independent"}}),
    });
    for definition in &mut definitions {
        if definition.kind == OperationKind::Write {
            definition.semantics["plan_allowance_seconds"] =
                json!(fin_domain::plan_timing::ONCHAIN_ALLOWANCE_SECONDS);
        }
    }
    definitions
}

fn unknown_operation() -> FinError {
    failure(
        "SOLANA_OPERATION_UNAVAILABLE",
        "operation",
        "This Solana wallet operation is not available in the current adapter.",
        "Inspect fin capabilities and select an implemented Solana operation.",
    )
}

fn normalize_instruction(instruction: solana_instruction::Instruction) -> NormalizedInstruction {
    NormalizedInstruction {
        program_id: instruction.program_id.to_string(),
        accounts: instruction
            .accounts
            .into_iter()
            .map(|meta| crate::transaction::NormalizedAccountMeta {
                address: meta.pubkey.to_string(),
                signer: meta.is_signer,
                writable: meta.is_writable,
            })
            .collect(),
        data_base64: STANDARD.encode(instruction.data),
    }
}

fn normalize_old_instruction(
    instruction: solana_instruction3::Instruction,
) -> Result<NormalizedInstruction> {
    let program_id = instruction
        .program_id
        .to_string()
        .parse()
        .map_err(|_| crate::parse_error())?;
    let accounts = instruction
        .accounts
        .into_iter()
        .map(|meta| {
            Ok(solana_instruction::AccountMeta {
                pubkey: meta
                    .pubkey
                    .to_string()
                    .parse()
                    .map_err(|_| crate::parse_error())?,
                is_signer: meta.is_signer,
                is_writable: meta.is_writable,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(normalize_instruction(solana_instruction::Instruction {
        program_id,
        accounts,
        data: instruction.data,
    }))
}

fn memo_instruction(memo: &str) -> Result<NormalizedInstruction> {
    let program_id = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr"
        .parse()
        .map_err(|_| crate::parse_error())?;
    Ok(normalize_instruction(solana_instruction::Instruction {
        program_id,
        accounts: vec![],
        data: memo.as_bytes().to_vec(),
    }))
}

fn price_micros_per_unit(usd_per_unit: &str) -> Result<fin_domain::Quantity> {
    fin_domain::Quantity::from_decimal(usd_per_unit, 18)?.mul_div_ceil(
        &fin_domain::Quantity::from_u64(1),
        &fin_domain::Quantity::new("1000000000000")?,
    )
}

fn ensure_fresh_quote(quote: &fin_pricing::UsdQuote) -> Result<()> {
    let updated = chrono::DateTime::parse_from_rfc3339(&quote.updated_at).map_err(|_| {
        failure(
            "SOLANA_PRICE_STALE",
            "prepare",
            "The asset price source returned an invalid source update time.",
            "Refresh the source quote before preparing this operation.",
        )
    })?;
    let age = chrono::Utc::now().signed_duration_since(updated);
    if age < chrono::Duration::seconds(-30) || age > chrono::Duration::seconds(300) {
        return Err(failure(
            "SOLANA_PRICE_STALE",
            "prepare",
            "The asset price source quote is outside its freshness window.",
            "Refresh the source quote before preparing this operation.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transaction::codec;
    use async_trait::async_trait;
    use bincode::Options;
    use fin_domain::{OperationRecord, SubmissionJournal};
    use fin_pricing::UsdQuote;
    use serde_json::Value;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Journal {
        submission: Mutex<Option<Submission>>,
        evidence: Mutex<Option<Value>>,
    }
    impl SubmissionJournal for Journal {
        fn assert_authorized(&self) -> Result<()> {
            Ok(())
        }
        fn persist_submission(&self, submission: &Submission) -> Result<()> {
            *self.submission.lock().unwrap() = Some(submission.clone());
            Ok(())
        }
        fn persist_dispatch_evidence(
            &self,
            _submission: &Submission,
            evidence: &Value,
        ) -> Result<()> {
            *self.evidence.lock().unwrap() = Some(evidence.clone());
            Ok(())
        }
    }

    struct Prices;
    #[async_trait]
    impl UsdPriceSource for Prices {
        async fn quote(&self, _asset: &AssetId) -> Result<UsdQuote> {
            Ok(UsdQuote {
                usd_per_unit: "100.0".into(),
                updated_at: fin_domain::now(),
                observed_at: fin_domain::now(),
                source: "synthetic-test-quote".into(),
            })
        }
    }

    #[tokio::test]
    async fn read_only_wallet_rejects_transfer_before_rpc_or_signing() {
        let owner = test_signer().address().to_owned();
        let adapter = SolanaWalletAdapter::new(
            "readonly",
            &owner,
            None,
            Arc::new(Rpc::default()),
            Arc::new(Prices),
        )
        .unwrap();
        let input = json!({
            "wallet":"readonly",
            "network":"solana",
            "cluster":"mainnet-beta",
            "recipient":bs58::encode([79u8; 32]).into_string(),
            "amount":"0.01",
            "max_fee_sol":"0.00001",
            "requestId":"readonly-cannot-sign"
        });
        let error = adapter
            .prepare("wallet.transfer", &input)
            .await
            .unwrap_err();
        assert_eq!(error.code, "SOLANA_WALLET_READ_ONLY");
    }

    #[tokio::test]
    async fn native_transfer_boundary_fixes_recipient_and_caps_amount_and_fee() {
        let signer = test_signer();
        let owner = signer.address().to_owned();
        let adapter = SolanaWalletAdapter::new(
            "bounded",
            &owner,
            Some(signer),
            Arc::new(Rpc::default()),
            Arc::new(Prices),
        )
        .unwrap();
        let recipient = bs58::encode([91u8; 32]).into_string();
        let input = json!({"wallet":"bounded","network":"solana","cluster":"mainnet-beta","recipient":recipient,"amount":"0.0001","max_fee_sol":"0.00001","requestId":"boundary-first"});
        let prepared = adapter.prepare("wallet.transfer", &input).await.unwrap();
        let bounds = json!({"max_amount":"0.0002","max_fee_sol":"0.00002"});
        let spec = adapter
            .prepare_boundaries(&prepared, &bounds)
            .await
            .unwrap();
        assert_eq!(
            spec.maximum_debits[0].amount.atoms,
            fin_domain::Quantity::from_u64(200_000)
        );
        assert_eq!(
            spec.maximum_costs[0].amount.atoms,
            fin_domain::Quantity::from_u64(20_000)
        );

        let within = json!({"wallet":"bounded","network":"solana","cluster":"mainnet-beta","recipient":recipient,"amount":"0.00019","max_fee_sol":"0.00002","requestId":"boundary-second"});
        let candidate = adapter.prepare("wallet.transfer", &within).await.unwrap();
        adapter
            .validate_boundary_candidate(&spec, &candidate)
            .unwrap();

        let changed = json!({"wallet":"bounded","network":"solana","cluster":"mainnet-beta","recipient":bs58::encode([92u8; 32]).into_string(),"amount":"0.0001","max_fee_sol":"0.00001","requestId":"boundary-third"});
        let candidate = adapter.prepare("wallet.transfer", &changed).await.unwrap();
        assert!(
            adapter
                .validate_boundary_candidate(&spec, &candidate)
                .is_err()
        );
    }

    #[tokio::test]
    async fn revalidated_transfer_skips_duplicate_simulation_and_checks_expiry() {
        struct MovingClock(Arc<Mutex<chrono::DateTime<chrono::Utc>>>);
        impl fin_domain::clock::Clock for MovingClock {
            fn now(&self) -> chrono::DateTime<chrono::Utc> {
                *self.0.lock().unwrap()
            }
        }

        let signer = test_signer();
        let owner = signer.address().to_owned();
        let rpc = Arc::new(Rpc::default());
        let adapter = SolanaWalletAdapter::new(
            "revalidated",
            &owner,
            Some(signer),
            rpc.clone(),
            Arc::new(Prices),
        )
        .unwrap();
        let input = json!({
            "wallet":"revalidated",
            "network":"solana",
            "cluster":"mainnet-beta",
            "recipient":bs58::encode([92u8; 32]).into_string(),
            "amount":"0.0001",
            "max_fee_sol":"0.00001",
            "requestId":"reuse-transfer"
        });
        let prepared = adapter.prepare("wallet.transfer", &input).await.unwrap();
        assert_eq!(*rpc.simulations.lock().unwrap(), 1);
        assert_eq!(
            adapter.preparation_reuse_window(&prepared),
            Some(std::time::Duration::from_secs(30))
        );
        let validated = fin_domain::RevalidatedOperation::after_revalidation(
            prepared.clone(),
            Arc::new(fin_domain::clock::SystemClock),
        )
        .unwrap();
        adapter
            .submit_revalidated(&validated, Arc::new(Journal::default()))
            .await
            .unwrap();
        assert_eq!(*rpc.simulations.lock().unwrap(), 1);
        assert!(rpc.signed.lock().unwrap().is_some());

        let delayed_rpc = Arc::new(Rpc::default());
        let delayed_adapter = SolanaWalletAdapter::new(
            "delayed",
            &owner,
            Some(test_signer()),
            delayed_rpc.clone(),
            Arc::new(Prices),
        )
        .unwrap();
        let delayed_input = json!({
            "wallet":"delayed",
            "network":"solana",
            "cluster":"mainnet-beta",
            "recipient":bs58::encode([93u8; 32]).into_string(),
            "amount":"0.0001",
            "max_fee_sol":"0.00001",
            "requestId":"delayed-transfer"
        });
        let delayed_prepared = delayed_adapter
            .prepare("wallet.transfer", &delayed_input)
            .await
            .unwrap();
        let clock = Arc::new(Mutex::new(chrono::Utc::now()));
        *delayed_rpc.advance_clock_after_fee.lock().unwrap() = Some(clock.clone());
        let delayed_validated = fin_domain::RevalidatedOperation::after_revalidation(
            delayed_prepared,
            Arc::new(MovingClock(clock)),
        )
        .unwrap();
        let error = delayed_adapter
            .submit_revalidated(&delayed_validated, Arc::new(Journal::default()))
            .await
            .unwrap_err();
        assert_eq!(error.code, "PREVIEW_EXPIRED");
        assert!(delayed_rpc.signed.lock().unwrap().is_none());

        let expired_rpc = Arc::new(Rpc::default());
        let expired_adapter = SolanaWalletAdapter::new(
            "expired",
            &owner,
            Some(test_signer()),
            expired_rpc.clone(),
            Arc::new(Prices),
        )
        .unwrap();
        let mut expired_input = input;
        expired_input["wallet"] = json!("expired");
        expired_input["requestId"] = json!("expired-transfer");
        let expired = expired_adapter
            .prepare("wallet.transfer", &expired_input)
            .await
            .unwrap();
        *expired_rpc.block_height.lock().unwrap() = 101;
        let expired = fin_domain::RevalidatedOperation::after_revalidation(
            expired,
            Arc::new(fin_domain::clock::SystemClock),
        )
        .unwrap();
        let error = expired_adapter
            .submit_revalidated(&expired, Arc::new(Journal::default()))
            .await
            .unwrap_err();
        assert_eq!(error.code, "SOLANA_BLOCKHASH_EXPIRED");
        assert!(expired_rpc.signed.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn wallet_plan_permissions_cover_transfer_wrap_unwrap_without_balance_enrollment() {
        let signer = test_signer();
        let owner = signer.address().to_owned();
        let rpc = Arc::new(Rpc::default());
        let adapter = SolanaWalletAdapter::new(
            "planned",
            &owner,
            Some(signer),
            rpc.clone(),
            Arc::new(Prices),
        )
        .unwrap();
        let recipient = bs58::encode([93u8; 32]).into_string();
        let transfer = json!({"wallet":"planned","network":"solana","cluster":"mainnet-beta","recipient":recipient,"amount":"0.0001","max_fee_sol":"0.00001","requestId":"plan-transfer"});
        let bounds = json!({"max_amount":"0.0003","max_fee_sol":"0.00002"});
        let permission = adapter
            .prepare_plan_permission("wallet.transfer", &transfer, &bounds)
            .await
            .unwrap();
        assert_eq!(permission.authorization_seconds, None);
        assert!(permission.possible_credits.is_empty());
        assert_eq!(
            permission.spec.maximum_debits[0].amount.atoms,
            fin_domain::Quantity::from_u64(300_000)
        );

        let wrap = json!({"wallet":"planned","network":"solana","cluster":"mainnet-beta","amount":"0.0001","max_fee_sol":"0.00001","max_account_funding_sol":"0.003","requestId":"plan-wrap"});
        let wrap_permission = adapter.prepare_plan_permission(
            "wallet.wrap", &wrap,
            &json!({"max_amount":"0.0002","max_fee_sol":"0.00002","max_account_funding_sol":"0.004"}),
        ).await.unwrap();
        assert_eq!(
            wrap_permission.possible_credits[0].asset.reference,
            "So11111111111111111111111111111111111111112"
        );
        assert_eq!(wrap_permission.authorization_seconds, None);

        let unwrap = json!({"wallet":"planned","network":"solana","cluster":"mainnet-beta","amount":"0.0001","max_fee_sol":"0.00001","max_temporary_rent_sol":"0.003","requestId":"plan-unwrap"});
        let unwrap_permission = adapter.prepare_plan_permission(
            "wallet.unwrap", &unwrap,
            &json!({"max_amount":"0.0002","max_fee_sol":"0.00002","max_temporary_rent_sol":"0.004"}),
        ).await.unwrap();
        assert_eq!(
            unwrap_permission.possible_credits[0].asset.reference,
            "native"
        );

        let mint = bs58::encode([94u8; 32]).into_string();
        let source = bs58::encode([95u8; 32]).into_string();
        rpc.accounts
            .lock()
            .unwrap()
            .insert(mint.clone(), (TOKEN_PROGRAM_ID.into(), legacy_mint(6)));
        rpc.accounts.lock().unwrap().insert(
            source.clone(),
            (
                TOKEN_PROGRAM_ID.into(),
                legacy_token_account(&mint, &owner, 0),
            ),
        );
        let token_transfer = json!({"wallet":"planned","network":"solana","cluster":"mainnet-beta","recipient":recipient,"mint":mint,"source_token_account":source,"amount":"0.25","max_fee_sol":"0.00001","max_account_funding_sol":"0.003","requestId":"plan-token-transfer"});
        let token_permission = adapter.prepare_plan_permission(
            "wallet.transfer", &token_transfer,
            &json!({"max_amount":"0.5","max_fee_sol":"0.00002","max_account_funding_sol":"0.004"}),
        ).await.unwrap();
        assert_eq!(token_permission.spec.fixed["asset"]["decimals"], 6);
        assert_eq!(token_permission.spec.maximum_debits.len(), 2);
        assert_eq!(
            *rpc.balance_reads.lock().unwrap(),
            0,
            "plan permission must not depend on present wallet balances"
        );
    }

    #[derive(Clone)]
    struct TokenReceiptFixture {
        mint: String,
        token_program: String,
        owner: String,
        recipient: String,
        source: String,
        destination: String,
        source_before: u64,
        source_after: u64,
        destination_before: u64,
        destination_after: u64,
        account_funding_lamports: u64,
    }

    #[derive(Clone)]
    struct WrapReceiptFixture {
        wallet: String,
        token_account: String,
        token_before: u64,
        amount_lamports: u64,
        rent_lamports: u64,
    }

    #[derive(Default)]
    struct Rpc {
        signed: Mutex<Option<String>>,
        accounts: Mutex<std::collections::HashMap<String, (String, Vec<u8>)>>,
        token_receipt: Mutex<Option<TokenReceiptFixture>>,
        wrap_receipt: Mutex<Option<WrapReceiptFixture>>,
        requested_rent_lengths: Mutex<Vec<usize>>,
        failed_transaction: Mutex<bool>,
        balance_reads: Mutex<u32>,
        account_info_reads: Mutex<Vec<String>>,
        account_info_delay: Mutex<Option<std::time::Duration>>,
        multiple_accounts_reads: Mutex<Vec<Vec<String>>>,
        multiple_accounts_delay: Mutex<Option<std::time::Duration>>,
        simulations: Mutex<u32>,
        block_height: Mutex<u64>,
        advance_clock_after_fee: Mutex<Option<Arc<Mutex<chrono::DateTime<chrono::Utc>>>>>,
    }
    #[async_trait]
    impl SolanaRpc for Rpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            Ok(match method {
                "getGenesisHash" => json!(crate::MAINNET_GENESIS_HASH),
                "getLatestBlockhash" => {
                    json!({"context":{"slot":10},"value":{"blockhash":solana_hash::Hash::default().to_string(),"lastValidBlockHeight":100}})
                }
                "getSlot" => json!(10),
                "getFeeForMessage" => {
                    if let Some(clock) = self.advance_clock_after_fee.lock().unwrap().as_ref() {
                        *clock.lock().unwrap() += chrono::Duration::seconds(61);
                    }
                    json!({"context":{"slot":10},"value":5000})
                }
                "simulateTransaction" => {
                    *self.simulations.lock().unwrap() += 1;
                    json!({"context":{"slot":10},"value":{"err":null,"unitsConsumed":10,"logs":[]}})
                }
                "getBalance" => {
                    *self.balance_reads.lock().unwrap() += 1;
                    json!({"context":{"slot":10},"value":100_000_000})
                }
                "getBlockHeight" => json!(*self.block_height.lock().unwrap()),
                "getEpochInfo" => json!({"epoch":2}),
                "getMinimumBalanceForRentExemption" => {
                    let size = params
                        .as_array()
                        .and_then(|values| values.first())
                        .and_then(Value::as_u64)
                        .and_then(|value| usize::try_from(value).ok())
                        .ok_or_else(crate::parse_error)?;
                    self.requested_rent_lengths.lock().unwrap().push(size);
                    json!(2_039_280)
                }
                "getTokenAccountsByOwner" => {
                    let program = params
                        .as_array()
                        .and_then(|values| values.get(1))
                        .and_then(|value| value.get("programId"))
                        .and_then(Value::as_str)
                        .ok_or_else(crate::parse_error)?;
                    let entries = self.accounts.lock().unwrap().iter()
                        .filter(|(_, (owner, data))| owner == program && data.len() == 165)
                        .map(|(address, (owner, data))| json!({"pubkey":address,"account":{"owner":owner,"lamports":2_039_280,"data":[STANDARD.encode(data),"base64"]}}))
                        .collect::<Vec<_>>();
                    json!({"value":entries})
                }
                "getAccountInfo" => {
                    let address = params
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(Value::as_str)
                        .ok_or_else(crate::parse_error)?;
                    self.account_info_reads
                        .lock()
                        .unwrap()
                        .push(address.to_owned());
                    let delay = *self.account_info_delay.lock().unwrap();
                    if let Some(delay) = delay {
                        tokio::time::sleep(delay).await;
                    }
                    match self.accounts.lock().unwrap().get(address) {
                        Some((owner, data)) => {
                            json!({"value":{"owner":owner,"lamports":2_039_280,"data":[STANDARD.encode(data),"base64"]}})
                        }
                        None => json!({"value":null}),
                    }
                }
                "getMultipleAccounts" => {
                    if params
                        .as_array()
                        .and_then(|values| values.get(1))
                        .is_none_or(|config| {
                            config.get("commitment").and_then(Value::as_str) != Some("finalized")
                                || config.get("encoding").and_then(Value::as_str) != Some("base64")
                        })
                    {
                        return Err(crate::parse_error());
                    }
                    let addresses = params
                        .as_array()
                        .and_then(|values| values.first())
                        .and_then(Value::as_array)
                        .ok_or_else(crate::parse_error)?
                        .iter()
                        .map(|address| {
                            address
                                .as_str()
                                .map(str::to_owned)
                                .ok_or_else(crate::parse_error)
                        })
                        .collect::<Result<Vec<_>>>()?;
                    self.multiple_accounts_reads
                        .lock()
                        .unwrap()
                        .push(addresses.clone());
                    let delay = *self.multiple_accounts_delay.lock().unwrap();
                    if let Some(delay) = delay {
                        tokio::time::sleep(delay).await;
                    }
                    let accounts = self.accounts.lock().unwrap();
                    let values = addresses
                        .iter()
                        .map(|address| match accounts.get(address) {
                            Some((owner, data)) => {
                                json!({"owner":owner,"lamports":2_039_280,"data":[STANDARD.encode(data),"base64"]})
                            }
                            None => Value::Null,
                        })
                        .collect::<Vec<_>>();
                    json!({"value":values})
                }
                "sendTransaction" => {
                    let encoded = params
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(Value::as_str)
                        .ok_or_else(crate::parse_error)?;
                    *self.signed.lock().unwrap() = Some(encoded.to_owned());
                    let wire = STANDARD.decode(encoded).map_err(|_| crate::parse_error())?;
                    let tx: solana_transaction::versioned::VersionedTransaction = codec()
                        .deserialize(&wire)
                        .map_err(|_| crate::parse_error())?;
                    json!(tx.signatures[0].to_string())
                }
                "getSignatureStatuses" => {
                    let error = if *self.failed_transaction.lock().unwrap() {
                        json!({"InstructionError":[0,{"Custom":1}]})
                    } else {
                        Value::Null
                    };
                    json!({"value":[{"confirmationStatus":"finalized","err":error}]})
                }
                "getTransaction" => {
                    let wire = self
                        .signed
                        .lock()
                        .unwrap()
                        .clone()
                        .ok_or_else(crate::parse_error)?;
                    let tx: solana_transaction::versioned::VersionedTransaction = codec()
                        .deserialize(&STANDARD.decode(&wire).map_err(|_| crate::parse_error())?)
                        .map_err(|_| crate::parse_error())?;
                    let account_keys = tx.message.static_account_keys();
                    let token_fixture = self.token_receipt.lock().unwrap().clone();
                    let wrap_fixture = self.wrap_receipt.lock().unwrap().clone();
                    let failed = *self.failed_transaction.lock().unwrap();
                    let mut pre_lamports = vec![0u64; account_keys.len()];
                    let mut post_lamports = vec![0u64; account_keys.len()];
                    if let Some(fixture) = wrap_fixture.as_ref() {
                        let wallet_index = account_keys
                            .iter()
                            .position(|key| key.to_string() == fixture.wallet)
                            .ok_or_else(crate::parse_error)?;
                        let account_index = account_keys
                            .iter()
                            .position(|key| key.to_string() == fixture.token_account)
                            .ok_or_else(crate::parse_error)?;
                        pre_lamports[wallet_index] = 100_000_000;
                        post_lamports[wallet_index] = 100_000_000u64
                            .checked_sub(fixture.amount_lamports)
                            .and_then(|value| value.checked_sub(5_000))
                            .ok_or_else(crate::parse_error)?;
                        pre_lamports[account_index] = fixture
                            .rent_lamports
                            .checked_add(fixture.token_before)
                            .ok_or_else(crate::parse_error)?;
                        post_lamports[account_index] = fixture
                            .rent_lamports
                            .checked_add(fixture.token_before)
                            .and_then(|value| value.checked_add(fixture.amount_lamports))
                            .ok_or_else(crate::parse_error)?;
                    } else if !account_keys.is_empty() {
                        pre_lamports[0] = if token_fixture.is_some() {
                            10_000_000
                        } else {
                            1_000_000
                        };
                        post_lamports[0] = if let Some(fixture) = token_fixture.as_ref() {
                            10_000_000u64
                                .saturating_sub(5_000)
                                .saturating_sub(fixture.account_funding_lamports)
                        } else if failed {
                            995_000
                        } else {
                            895_000
                        };
                    }
                    if let Some(fixture) = token_fixture.as_ref()
                        && fixture.account_funding_lamports > 0
                    {
                        let destination_index = account_keys
                            .iter()
                            .position(|key| key.to_string() == fixture.destination)
                            .ok_or_else(crate::parse_error)?;
                        post_lamports[destination_index] = fixture.account_funding_lamports;
                    }
                    if token_fixture.is_none()
                        && wrap_fixture.is_none()
                        && !failed
                        && account_keys.len() > 1
                    {
                        post_lamports[1] = 100_000;
                    }
                    let (pre_token_balances, post_token_balances) = if let Some(fixture) =
                        token_fixture
                    {
                        let source_index = account_keys
                            .iter()
                            .position(|key| key.to_string() == fixture.source)
                            .ok_or_else(crate::parse_error)?;
                        let destination_index = account_keys
                            .iter()
                            .position(|key| key.to_string() == fixture.destination)
                            .ok_or_else(crate::parse_error)?;
                        let balance = |index: usize, amount: u64| json!({"accountIndex":index,"mint":fixture.mint,"owner":if index==source_index {fixture.owner.as_str()} else {fixture.recipient.as_str()},"programId":fixture.token_program,"uiTokenAmount":{"amount":amount.to_string(),"decimals":6}});
                        (
                            vec![
                                balance(source_index, fixture.source_before),
                                balance(destination_index, fixture.destination_before),
                            ],
                            vec![
                                balance(source_index, fixture.source_after),
                                balance(destination_index, fixture.destination_after),
                            ],
                        )
                    } else if let Some(fixture) = wrap_fixture {
                        let account_index = account_keys
                            .iter()
                            .position(|key| key.to_string() == fixture.token_account)
                            .ok_or_else(crate::parse_error)?;
                        let balance = |amount: u64| json!({"accountIndex":account_index,"mint":"So11111111111111111111111111111111111111112","owner":fixture.wallet,"programId":TOKEN_PROGRAM_ID,"uiTokenAmount":{"amount":amount.to_string(),"decimals":9}});
                        (
                            vec![balance(fixture.token_before)],
                            vec![balance(
                                fixture
                                    .token_before
                                    .checked_add(fixture.amount_lamports)
                                    .ok_or_else(crate::parse_error)?,
                            )],
                        )
                    } else {
                        (Vec::new(), Vec::new())
                    };
                    let error = if failed {
                        json!({"InstructionError":[0,{"Custom":1}]})
                    } else {
                        Value::Null
                    };
                    json!({"slot":99,"transaction":[wire,"base64"],"meta":{"fee":5000,"err":error,"preBalances":pre_lamports,"postBalances":post_lamports,"preTokenBalances":pre_token_balances,"postTokenBalances":post_token_balances,"loadedAddresses":{"writable":[],"readonly":[]},"innerInstructions":[],"logMessages":[]}})
                }
                _ => return Err(crate::parse_error()),
            })
        }
    }

    fn test_signer() -> Arc<SolanaSigner> {
        let seed = [42u8; 32];
        let key = ed25519_dalek::SigningKey::from_bytes(&seed);
        let mut bytes = [0u8; 64];
        bytes[..32].copy_from_slice(&seed);
        bytes[32..].copy_from_slice(key.verifying_key().as_bytes());
        Arc::new(SolanaSigner::parse(&bs58::encode(bytes).into_string()).unwrap())
    }

    fn legacy_mint(decimals: u8) -> Vec<u8> {
        let mut data = vec![0u8; 82];
        data[44] = decimals;
        data[45] = 1;
        data
    }

    fn legacy_token_account(mint: &str, owner: &str, amount: u64) -> Vec<u8> {
        let mut data = vec![0u8; 165];
        data[..32].copy_from_slice(&mint.parse::<solana_address::Address>().unwrap().to_bytes());
        data[32..64].copy_from_slice(&owner.parse::<solana_address::Address>().unwrap().to_bytes());
        data[64..72].copy_from_slice(&amount.to_le_bytes());
        data[108] = 1;
        data
    }

    fn token_account_with_delegate_and_close_authority(
        mint: &str,
        owner: &str,
        amount: u64,
        delegate: &str,
        close_authority: &str,
    ) -> Vec<u8> {
        let mut data = legacy_token_account(mint, owner, amount);
        data[72..76].copy_from_slice(&1u32.to_le_bytes());
        data[76..108].copy_from_slice(
            &delegate
                .parse::<solana_address::Address>()
                .unwrap()
                .to_bytes(),
        );
        data[121..129].copy_from_slice(&7u64.to_le_bytes());
        data[129..133].copy_from_slice(&1u32.to_le_bytes());
        data[133..165].copy_from_slice(
            &close_authority
                .parse::<solana_address::Address>()
                .unwrap()
                .to_bytes(),
        );
        data
    }

    fn confidential_token_2022_mint(decimals: u8) -> Vec<u8> {
        use spl_token_2022_interface::extension::{
            AccountType, ExtensionType, confidential_transfer::ConfidentialTransferMint,
        };
        let mut data = vec![0u8; 166];
        data[44] = decimals;
        data[45] = 1;
        data[165] = AccountType::Mint.into();
        data.extend_from_slice(&u16::from(ExtensionType::ConfidentialTransferMint).to_le_bytes());
        data.extend_from_slice(
            &(std::mem::size_of::<ConfidentialTransferMint>() as u16).to_le_bytes(),
        );
        data.resize(166 + 4 + std::mem::size_of::<ConfidentialTransferMint>(), 0);
        data
    }

    #[tokio::test]
    async fn native_transfer_completes_only_after_durable_finalized_balance_proof() {
        let signer = test_signer();
        let address = signer.address().to_owned();
        let rpc = Arc::new(Rpc::default());
        let adapter =
            SolanaWalletAdapter::new("synthetic", &address, Some(signer), rpc, Arc::new(Prices))
                .unwrap();
        let recipient = bs58::encode([8u8; 32]).into_string();
        let input = json!({"wallet":"synthetic","network":"solana","cluster":"mainnet-beta","recipient":recipient,"amount":"0.0001","max_fee_sol":"0.00001","requestId":"synthetic-transfer-1"});
        let prepared = adapter.prepare("wallet.transfer", &input).await.unwrap();
        assert!(prepared.valuation.is_some());
        let prepared = adapter.revalidate(&prepared).await.unwrap();
        let journal = Arc::new(Journal::default());
        let submission = adapter.submit(&prepared, journal.clone()).await.unwrap();
        assert_eq!(
            submission.reference,
            format!(
                "solana:message:{}",
                prepared.binding["message_sha256"].as_str().unwrap()
            )
        );
        assert_eq!(
            journal.submission.lock().unwrap().as_ref(),
            Some(&submission)
        );
        assert!(journal.evidence.lock().unwrap().as_ref().is_some_and(
            |evidence| evidence["chain_signature"] == submission.data["chain_signature"]
        ));
        let record = OperationRecord {
            id: "synthetic-operation".into(),
            request_id: "synthetic-transfer-1".into(),
            fingerprint: "synthetic".into(),
            fingerprint_version: 1,
            operation: "wallet.transfer".into(),
            input,
            status: Status::Submitted,
            created_at: fin_domain::now(),
            updated_at: fin_domain::now(),
            preview: Some(prepared),
            approval: None,
            submission: Some(submission),
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let settlement = adapter.reconcile(&record).await.unwrap();
        assert_eq!(settlement.status, Status::Completed);
        let verified = settlement.verified.unwrap();
        assert_eq!(
            verified.proof_digest,
            fin_domain::verified_effect_digest(&verified).unwrap()
        );
        assert_eq!(verified.effects.len(), 2);
        assert_eq!(
            verified.effects[0],
            FinancialEffect::Transfer {
                from: ResourceId(format!("solana:mainnet-beta:{address}")),
                to: ResourceId(format!("solana:mainnet-beta:{recipient}")),
                debit: Amount {
                    asset: AssetId {
                        network: NETWORK.into(),
                        reference: "native".into(),
                        decimals: 9
                    },
                    atoms: fin_domain::Quantity::from_u64(100000)
                },
                credit: Amount {
                    asset: AssetId {
                        network: NETWORK.into(),
                        reference: "native".into(),
                        decimals: 9
                    },
                    atoms: fin_domain::Quantity::from_u64(100000)
                }
            }
        );
    }

    #[tokio::test]
    async fn failed_finalized_transfer_records_network_fee_without_principal_movement() {
        let signer = test_signer();
        let address = signer.address().to_owned();
        let rpc = Arc::new(Rpc::default());
        *rpc.failed_transaction.lock().unwrap() = true;
        let adapter =
            SolanaWalletAdapter::new("synthetic", &address, Some(signer), rpc, Arc::new(Prices))
                .unwrap();
        let recipient = bs58::encode([18u8; 32]).into_string();
        let input = json!({"wallet":"synthetic","network":"solana","cluster":"mainnet-beta","recipient":recipient,"amount":"0.0001","max_fee_sol":"0.00001","requestId":"synthetic-failed-transfer"});
        let prepared = adapter.prepare("wallet.transfer", &input).await.unwrap();
        let prepared = adapter.revalidate(&prepared).await.unwrap();
        let submission = adapter
            .submit(&prepared, Arc::new(Journal::default()))
            .await
            .unwrap();
        let record = OperationRecord {
            id: "synthetic-failed-operation".into(),
            request_id: "synthetic-failed-transfer".into(),
            fingerprint: "synthetic-failed-transfer".into(),
            fingerprint_version: 1,
            operation: "wallet.transfer".into(),
            input,
            status: Status::Submitted,
            created_at: fin_domain::now(),
            updated_at: fin_domain::now(),
            preview: Some(prepared),
            approval: None,
            submission: Some(submission),
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };

        let settlement = adapter.reconcile(&record).await.unwrap();
        assert_eq!(settlement.status, Status::Failed);
        let verified = settlement.verified.unwrap();
        assert!(verified.effects.iter().any(|effect| matches!(
            effect,
            FinancialEffect::Cost {
                amount,
                category,
                ..
            } if category == "solana_network_fee"
                && amount.atoms == fin_domain::Quantity::from_u64(5_000)
        )));
        assert!(
            !verified
                .effects
                .iter()
                .any(|effect| matches!(effect, FinancialEffect::Transfer { .. }))
        );
    }

    #[tokio::test]
    async fn token_transfer_prepares_exact_atoms_and_verified_accounts() {
        let signer = test_signer();
        let owner = signer.address().to_owned();
        let mint = bs58::encode([11u8; 32]).into_string();
        let source = bs58::encode([12u8; 32]).into_string();
        let recipient = bs58::encode([13u8; 32]).into_string();
        let destination = bs58::encode([14u8; 32]).into_string();
        let rpc = Arc::new(Rpc::default());
        let token_program = TOKEN_PROGRAM_ID.to_owned();
        {
            let mut accounts = rpc.accounts.lock().unwrap();
            accounts.insert(mint.clone(), (token_program.clone(), legacy_mint(6)));
            accounts.insert(
                source.clone(),
                (
                    token_program.clone(),
                    legacy_token_account(&mint, &owner, 2_000_000),
                ),
            );
            accounts.insert(
                destination.clone(),
                (token_program, legacy_token_account(&mint, &recipient, 0)),
            );
        }
        let adapter = SolanaWalletAdapter::new(
            "synthetic",
            &owner,
            Some(signer),
            rpc.clone(),
            Arc::new(Prices),
        )
        .unwrap();
        let input = json!({"wallet":"synthetic","network":"solana","cluster":"mainnet-beta","recipient":recipient,"mint":mint,"source_token_account":source,"destination_token_account":destination,"amount":"1.25","max_fee_sol":"0.00001","requestId":"synthetic-token-1"});
        let prepared = adapter.prepare("wallet.transfer", &input).await.unwrap();
        assert_eq!(prepared.binding["amount_atoms"], 1_250_000);
        assert_eq!(prepared.binding["expected_credit_atoms"], 1_250_000);
        assert_eq!(prepared.binding["decimals"], 6);
        assert_eq!(
            prepared.intent.expected[0],
            EffectConstraint::Transfer {
                from: ResourceId(format!("solana:mainnet-beta:{owner}")),
                to: ResourceId(format!("solana:mainnet-beta:{recipient}")),
                debit_asset: AssetId {
                    network: NETWORK.into(),
                    reference: mint.clone(),
                    decimals: 6
                },
                credit_asset: AssetId {
                    network: NETWORK.into(),
                    reference: mint,
                    decimals: 6
                },
                maximum_debit: fin_domain::Quantity::from_u64(1_250_000),
                minimum_credit: fin_domain::Quantity::from_u64(1_250_000),
            }
        );
    }

    #[tokio::test]
    async fn token_transfer_settles_only_from_exact_finalized_account_deltas() {
        let signer = test_signer();
        let owner = signer.address().to_owned();
        let mint = bs58::encode([31u8; 32]).into_string();
        let source = bs58::encode([32u8; 32]).into_string();
        let recipient = bs58::encode([33u8; 32]).into_string();
        let destination = bs58::encode([34u8; 32]).into_string();
        let rpc = Arc::new(Rpc::default());
        {
            let mut accounts = rpc.accounts.lock().unwrap();
            accounts.insert(mint.clone(), (TOKEN_PROGRAM_ID.into(), legacy_mint(6)));
            accounts.insert(
                source.clone(),
                (
                    TOKEN_PROGRAM_ID.into(),
                    legacy_token_account(&mint, &owner, 2_000_000),
                ),
            );
            accounts.insert(
                destination.clone(),
                (
                    TOKEN_PROGRAM_ID.into(),
                    legacy_token_account(&mint, &recipient, 0),
                ),
            );
        }
        *rpc.token_receipt.lock().unwrap() = Some(TokenReceiptFixture {
            mint: mint.clone(),
            token_program: TOKEN_PROGRAM_ID.into(),
            owner: owner.clone(),
            recipient: recipient.clone(),
            source: source.clone(),
            destination: destination.clone(),
            source_before: 2_000_000,
            source_after: 750_000,
            destination_before: 0,
            destination_after: 1_250_000,
            account_funding_lamports: 0,
        });
        let adapter = SolanaWalletAdapter::new(
            "synthetic",
            &owner,
            Some(signer),
            rpc.clone(),
            Arc::new(Prices),
        )
        .unwrap();
        let input = json!({"wallet":"synthetic","network":"solana","cluster":"mainnet-beta","recipient":recipient,"mint":mint,"source_token_account":source,"destination_token_account":destination,"amount":"1.25","max_fee_sol":"0.00001","requestId":"synthetic-token-finalized"});
        let prepared = adapter.prepare("wallet.transfer", &input).await.unwrap();
        let prepared = adapter.revalidate(&prepared).await.unwrap();
        let journal = Arc::new(Journal::default());
        let submission = adapter.submit(&prepared, journal).await.unwrap();
        let record = OperationRecord {
            id: "synthetic-token-operation".into(),
            request_id: "synthetic-token-finalized".into(),
            fingerprint: "synthetic-token".into(),
            fingerprint_version: 1,
            operation: "wallet.transfer".into(),
            input,
            status: Status::Submitted,
            created_at: fin_domain::now(),
            updated_at: fin_domain::now(),
            preview: Some(prepared),
            approval: None,
            submission: Some(submission),
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let settlement = adapter.reconcile(&record).await.unwrap();
        assert_eq!(settlement.status, Status::Completed);
        assert_eq!(settlement.verified.unwrap().effects.len(), 2);
    }

    #[tokio::test]
    async fn confidential_transfer_mint_allows_public_transfer_to_new_ata() {
        let signer = test_signer();
        let owner = signer.address().to_owned();
        let mint = bs58::encode([41u8; 32]).into_string();
        let source = bs58::encode([42u8; 32]).into_string();
        let recipient = bs58::encode([43u8; 32]).into_string();
        let rpc = Arc::new(Rpc::default());
        rpc.accounts.lock().unwrap().insert(
            mint.clone(),
            (
                TOKEN_2022_PROGRAM_ID.into(),
                confidential_token_2022_mint(6),
            ),
        );
        rpc.accounts.lock().unwrap().insert(
            source.clone(),
            (
                TOKEN_2022_PROGRAM_ID.into(),
                legacy_token_account(&mint, &owner, 2_000_000),
            ),
        );
        let adapter = SolanaWalletAdapter::new(
            "synthetic",
            &owner,
            Some(signer),
            rpc.clone(),
            Arc::new(Prices),
        )
        .unwrap();
        let input = json!({"wallet":"synthetic","network":"solana","cluster":"mainnet-beta","recipient":recipient,"mint":mint,"source_token_account":source,"amount":"1.25","max_fee_sol":"0.00001","max_account_funding_sol":"0.003","requestId":"synthetic-public-confidential"});
        let prepared = adapter.prepare("wallet.transfer", &input).await.unwrap();
        assert_eq!(prepared.binding["destination_account_created"], true);
        assert_eq!(
            rpc.requested_rent_lengths.lock().unwrap().last(),
            Some(&170)
        );
        assert!(
            prepared.binding["instructions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|ix| {
                    ix["program_id"]
                        == spl_associated_token_account_interface::program::id().to_string()
                })
        );
    }

    #[tokio::test]
    async fn token_2022_public_transfer_settles_for_unconfigured_accounts() {
        let signer = test_signer();
        let owner = signer.address().to_owned();
        let mint = bs58::encode([51u8; 32]).into_string();
        let source = bs58::encode([52u8; 32]).into_string();
        let recipient = bs58::encode([53u8; 32]).into_string();
        let destination = bs58::encode([54u8; 32]).into_string();
        let rpc = Arc::new(Rpc::default());
        {
            let mut accounts = rpc.accounts.lock().unwrap();
            accounts.insert(
                mint.clone(),
                (
                    TOKEN_2022_PROGRAM_ID.into(),
                    confidential_token_2022_mint(6),
                ),
            );
            accounts.insert(
                source.clone(),
                (
                    TOKEN_2022_PROGRAM_ID.into(),
                    legacy_token_account(&mint, &owner, 2_000_000),
                ),
            );
            accounts.insert(
                destination.clone(),
                (
                    TOKEN_2022_PROGRAM_ID.into(),
                    legacy_token_account(&mint, &recipient, 0),
                ),
            );
        }
        *rpc.token_receipt.lock().unwrap() = Some(TokenReceiptFixture {
            mint: mint.clone(),
            token_program: TOKEN_2022_PROGRAM_ID.into(),
            owner: owner.clone(),
            recipient: recipient.clone(),
            source: source.clone(),
            destination: destination.clone(),
            source_before: 2_000_000,
            source_after: 750_000,
            destination_before: 0,
            destination_after: 1_250_000,
            account_funding_lamports: 0,
        });
        let adapter =
            SolanaWalletAdapter::new("synthetic", &owner, Some(signer), rpc, Arc::new(Prices))
                .unwrap();
        let input = json!({"wallet":"synthetic","network":"solana","cluster":"mainnet-beta","recipient":recipient,"mint":mint,"source_token_account":source,"destination_token_account":destination,"amount":"1.25","max_fee_sol":"0.00001","requestId":"synthetic-token-2022-finalized"});
        let prepared = adapter.prepare("wallet.transfer", &input).await.unwrap();
        let prepared = adapter.revalidate(&prepared).await.unwrap();
        let submission = adapter
            .submit(&prepared, Arc::new(Journal::default()))
            .await
            .unwrap();
        let record = OperationRecord {
            id: "synthetic-token-2022-operation".into(),
            request_id: "synthetic-token-2022-finalized".into(),
            fingerprint: "synthetic-token-2022".into(),
            fingerprint_version: 1,
            operation: "wallet.transfer".into(),
            input,
            status: Status::Submitted,
            created_at: fin_domain::now(),
            updated_at: fin_domain::now(),
            preview: Some(prepared),
            approval: None,
            submission: Some(submission),
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let settlement = adapter.reconcile(&record).await.unwrap();
        assert_eq!(settlement.status, Status::Completed);
        assert_eq!(settlement.verified.unwrap().effects.len(), 2);
    }

    #[tokio::test]
    async fn scoped_snapshot_aggregates_wallet_accounts_and_keeps_app_units_separate() {
        let signer = test_signer();
        let owner = signer.address().to_owned();
        let mint = bs58::encode([61u8; 32]).into_string();
        let first = bs58::encode([62u8; 32]).into_string();
        let second = bs58::encode([63u8; 32]).into_string();
        let delegate = bs58::encode([64u8; 32]).into_string();
        let close_authority = bs58::encode([65u8; 32]).into_string();
        let rpc = Arc::new(Rpc::default());
        {
            let mut accounts = rpc.accounts.lock().unwrap();
            accounts.insert(mint.clone(), (TOKEN_PROGRAM_ID.into(), legacy_mint(6)));
            accounts.insert(
                first.clone(),
                (
                    TOKEN_PROGRAM_ID.into(),
                    token_account_with_delegate_and_close_authority(
                        &mint,
                        &owner,
                        100,
                        &delegate,
                        &close_authority,
                    ),
                ),
            );
            accounts.insert(
                second,
                (
                    TOKEN_PROGRAM_ID.into(),
                    legacy_token_account(&mint, &owner, 200),
                ),
            );
        }
        let adapter =
            SolanaWalletAdapter::new("synthetic", &owner, Some(signer), rpc, Arc::new(Prices))
                .unwrap();
        let app_unit = AssetId {
            network: NETWORK.into(),
            reference: "jupiter-perps:position-value".into(),
            decimals: 6,
        };
        let scope = SnapshotScope {
            resources: vec![ResourceId(format!("solana:mainnet-beta:{owner}"))],
            networks: vec![],
            assets: vec![
                AssetId {
                    network: NETWORK.into(),
                    reference: mint,
                    decimals: 6,
                },
                app_unit,
            ],
        };
        let snapshot = adapter.snapshot_scoped(&scope).await.unwrap();
        assert!(snapshot.complete);
        assert!(snapshot.coverage[0].starts_with("solana:mainnet-beta:"));
        assert_eq!(snapshot.state.holdings.len(), 1);
        assert_eq!(snapshot.state.holdings[0].amount.atoms.as_str(), "300");
        assert_eq!(snapshot.state.authorities.len(), 1);
        assert!(snapshot.state.authorities[0].value.contains(&delegate));
        assert!(
            snapshot.state.authorities[0]
                .value
                .contains(&close_authority)
        );
        assert_eq!(snapshot.state.positions.len(), 1);
        assert_eq!(snapshot.state.positions[0].amount.atoms.as_str(), "2039280");
    }

    #[tokio::test(start_paused = true)]
    async fn many_account_snapshot_reuses_finalized_data_and_finishes_before_observer_deadline() {
        let signer = test_signer();
        let owner = signer.address().to_owned();
        let rpc = Arc::new(Rpc::default());
        *rpc.multiple_accounts_delay.lock().unwrap() = Some(std::time::Duration::from_millis(500));
        let mut mints = Vec::new();
        for index in 1..=33u8 {
            let mint = bs58::encode([index; 32]).into_string();
            let account = bs58::encode([index + 100; 32]).into_string();
            let mut accounts = rpc.accounts.lock().unwrap();
            accounts.insert(mint.clone(), (TOKEN_PROGRAM_ID.into(), legacy_mint(6)));
            accounts.insert(
                account,
                (
                    TOKEN_PROGRAM_ID.into(),
                    legacy_token_account(&mint, &owner, 100),
                ),
            );
            mints.push(mint);
        }
        let adapter = SolanaWalletAdapter::new(
            "synthetic",
            &owner,
            Some(signer),
            rpc.clone(),
            Arc::new(Prices),
        )
        .unwrap();
        let started = tokio::time::Instant::now();
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(10), adapter.snapshot())
            .await
            .expect("33 accounts must fit the broker observer deadline")
            .unwrap();
        assert_eq!(started.elapsed(), std::time::Duration::from_millis(500));
        assert!(snapshot.complete);
        assert!(snapshot.pending.is_empty());
        assert_eq!(snapshot.state.holdings.len(), 34);
        assert_eq!(snapshot.state.positions.len(), 33);
        assert!(snapshot.state.positions.iter().all(|position| {
            position.amount.atoms.as_str() == "2039280"
                && position.anchor.as_ref().unwrap().evidence["commitment"] == "finalized"
        }));
        mints.sort();
        assert_eq!(
            rpc.multiple_accounts_reads.lock().unwrap().clone(),
            vec![mints.clone()],
            "all unique mints should be read in one finalized batch"
        );
        assert!(rpc.account_info_reads.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn wallet_mint_reads_split_at_the_rpc_batch_limit() {
        let signer = test_signer();
        let owner = signer.address().to_owned();
        let rpc = Arc::new(Rpc::default());
        for index in 1..=101u16 {
            let mint = bs58::encode([index as u8, (index >> 8) as u8].repeat(16)).into_string();
            let account =
                bs58::encode([(index as u8).wrapping_add(101), (index >> 8) as u8].repeat(16))
                    .into_string();
            let mut accounts = rpc.accounts.lock().unwrap();
            accounts.insert(mint.clone(), (TOKEN_PROGRAM_ID.into(), legacy_mint(6)));
            accounts.insert(
                account,
                (
                    TOKEN_PROGRAM_ID.into(),
                    legacy_token_account(&mint, &owner, 100),
                ),
            );
        }
        let adapter = SolanaWalletAdapter::new(
            "synthetic",
            &owner,
            Some(signer),
            rpc.clone(),
            Arc::new(Prices),
        )
        .unwrap();
        adapter.balances().await.unwrap();
        let batches = rpc.multiple_accounts_reads.lock().unwrap().clone();
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), MAX_MULTIPLE_ACCOUNTS_ADDRESSES);
        assert_eq!(batches[1].len(), 1);
        assert!(
            batches
                .iter()
                .flatten()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == 101
        );
    }

    #[tokio::test]
    async fn snapshot_rejects_incomplete_duplicate_or_foreign_finalized_accounts() {
        struct CorruptOwnerQuery {
            rpc: Arc<Rpc>,
            corruption: &'static str,
        }
        #[async_trait]
        impl SolanaRpc for CorruptOwnerQuery {
            async fn request(&self, method: &str, params: Value) -> Result<Value> {
                let mut response = self.rpc.request(method, params).await?;
                if method == "getTokenAccountsByOwner" {
                    let rows = response["value"].as_array_mut().unwrap();
                    if let Some(first) = rows.first_mut() {
                        match self.corruption {
                            "missing_lamports" => {
                                first["account"].as_object_mut().unwrap().remove("lamports");
                            }
                            "duplicate" => {
                                let duplicate = first.clone();
                                rows.push(duplicate);
                            }
                            "program" => {
                                first["account"]["owner"] = json!(TOKEN_2022_PROGRAM_ID);
                            }
                            "token_owner" => {
                                let mut data = STANDARD
                                    .decode(first["account"]["data"][0].as_str().unwrap())
                                    .unwrap();
                                data[32..64].copy_from_slice(&[255u8; 32]);
                                first["account"]["data"][0] = json!(STANDARD.encode(data));
                            }
                            _ => {}
                        }
                    }
                } else if method == "getMultipleAccounts" {
                    let values = response["value"].as_array_mut().unwrap();
                    match self.corruption {
                        "batch_cardinality" => {
                            values.pop();
                        }
                        "batch_null" => {
                            values[0] = Value::Null;
                        }
                        "batch_owner" => {
                            values[0]["owner"] = json!(TOKEN_2022_PROGRAM_ID);
                        }
                        _ => {}
                    }
                }
                Ok(response)
            }
        }
        for corruption in [
            "missing_lamports",
            "duplicate",
            "program",
            "token_owner",
            "batch_cardinality",
            "batch_null",
            "batch_owner",
        ] {
            let signer = test_signer();
            let owner = signer.address().to_owned();
            let rpc = Arc::new(Rpc::default());
            let mint = bs58::encode([71u8; 32]).into_string();
            let account = bs58::encode([72u8; 32]).into_string();
            rpc.accounts
                .lock()
                .unwrap()
                .insert(mint.clone(), (TOKEN_PROGRAM_ID.into(), legacy_mint(6)));
            rpc.accounts.lock().unwrap().insert(
                account,
                (
                    TOKEN_PROGRAM_ID.into(),
                    legacy_token_account(&mint, &owner, 100),
                ),
            );
            let adapter = SolanaWalletAdapter::new(
                "synthetic",
                &owner,
                Some(signer),
                Arc::new(CorruptOwnerQuery {
                    rpc: rpc.clone(),
                    corruption,
                }),
                Arc::new(Prices),
            )
            .unwrap();
            assert!(adapter.snapshot().await.is_err(), "{corruption}");
            assert!(rpc.signed.lock().unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn token_read_emits_public_supply_as_exact_string_quantity() {
        let signer = test_signer();
        let owner = signer.address().to_owned();
        let mint = bs58::encode([71u8; 32]).into_string();
        let mut mint_data = legacy_mint(6);
        mint_data[36..44].copy_from_slice(&u64::MAX.to_le_bytes());
        let rpc = Arc::new(Rpc::default());
        rpc.accounts
            .lock()
            .unwrap()
            .insert(mint.clone(), (TOKEN_PROGRAM_ID.into(), mint_data));
        let adapter =
            SolanaWalletAdapter::new("synthetic", &owner, Some(signer), rpc, Arc::new(Prices))
                .unwrap();
        let value = adapter
            .token(&json!({"wallet":"synthetic","network":"solana","cluster":"mainnet-beta","mint":mint}))
            .await
            .unwrap();
        assert_eq!(value["descriptor"]["supply_atoms"], u64::MAX.to_string());
        assert!(value["descriptor"]["supply_atoms"].is_string());
        let definition = operation_definitions()
            .into_iter()
            .find(|definition| definition.name == "wallet.token")
            .unwrap();
        assert_eq!(
            definition.output_schema["properties"]["descriptor"]["properties"]["supply_atoms"]["type"],
            "string"
        );
        let definitions = operation_definitions();
        for operation in ["wallet.transfer", "wallet.wrap", "wallet.unwrap"] {
            let definition = definitions
                .iter()
                .find(|definition| definition.name == operation)
                .unwrap();
            assert_eq!(
                definition.semantics["plan_allowance_seconds"],
                fin_domain::plan_timing::ONCHAIN_ALLOWANCE_SECONDS
            );
        }
    }

    #[tokio::test]
    async fn token_transfer_creates_missing_recipient_ata_with_separately_bounded_rent() {
        let signer = test_signer();
        let owner = signer.address().to_owned();
        let mint = bs58::encode([21u8; 32]).into_string();
        let source = bs58::encode([22u8; 32]).into_string();
        let recipient = bs58::encode([23u8; 32]).into_string();
        let rpc = Arc::new(Rpc::default());
        rpc.accounts
            .lock()
            .unwrap()
            .insert(mint.clone(), (TOKEN_PROGRAM_ID.into(), legacy_mint(6)));
        rpc.accounts.lock().unwrap().insert(
            source.clone(),
            (
                TOKEN_PROGRAM_ID.into(),
                legacy_token_account(&mint, &owner, 2_000_000),
            ),
        );
        let adapter = SolanaWalletAdapter::new(
            "synthetic",
            &owner,
            Some(signer),
            rpc.clone(),
            Arc::new(Prices),
        )
        .unwrap();
        let input = json!({"wallet":"synthetic","network":"solana","cluster":"mainnet-beta","recipient":recipient,"mint":mint,"source_token_account":source,"amount":"1.25","max_fee_sol":"0.00001","max_account_funding_sol":"0.003","requestId":"synthetic-token-ata"});
        let prepared = adapter.prepare("wallet.transfer", &input).await.unwrap();
        assert_eq!(prepared.binding["destination_account_created"], true);
        assert_eq!(prepared.binding["account_funding_lamports"], 2_039_280);
        assert!(
            prepared
                .intent
                .debits
                .iter()
                .any(|debit| debit.amount.asset.reference == "native"
                    && debit.amount.atoms == fin_domain::Quantity::from_u64(2_039_280))
        );
        assert!(prepared.intent.expected.iter().any(|expected| matches!(expected,EffectConstraint::Transfer { credit_asset,maximum_debit,.. } if credit_asset.reference=="native" && *maximum_debit==fin_domain::Quantity::from_u64(2_039_280))));
        assert!(prepared.checks.iter().any(|check| {
            check.name == "account_funding"
                && check.detail.contains(&recipient)
                && check.detail.contains("controls any later rent refund")
                && !check.detail.contains("wallet-owned funding")
        }));

        let destination_account = prepared.binding["destination_token_account"]
            .as_str()
            .unwrap()
            .to_owned();
        *rpc.token_receipt.lock().unwrap() = Some(TokenReceiptFixture {
            mint: mint.clone(),
            token_program: TOKEN_PROGRAM_ID.into(),
            owner: owner.clone(),
            recipient: recipient.clone(),
            source: source.clone(),
            destination: destination_account.clone(),
            source_before: 2_000_000,
            source_after: 750_000,
            destination_before: 0,
            destination_after: 1_250_000,
            account_funding_lamports: 2_039_280,
        });
        let prepared = adapter.revalidate(&prepared).await.unwrap();
        let submission = adapter
            .submit(&prepared, Arc::new(Journal::default()))
            .await
            .unwrap();
        let record = OperationRecord {
            id: "synthetic-token-ata-operation".into(),
            request_id: "synthetic-token-ata".into(),
            fingerprint: "synthetic-token-ata".into(),
            fingerprint_version: 1,
            operation: "wallet.transfer".into(),
            input,
            status: Status::Submitted,
            created_at: fin_domain::now(),
            updated_at: fin_domain::now(),
            preview: Some(prepared),
            approval: None,
            submission: Some(submission),
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let settlement = adapter.reconcile(&record).await.unwrap();
        assert_eq!(settlement.status, Status::Completed);
        let effects = settlement.verified.unwrap().effects;
        assert!(effects.iter().any(|effect| matches!(effect, FinancialEffect::Cost { category, .. } if category == "solana_network_fee")));
        assert!(effects.iter().any(|effect| matches!(effect, FinancialEffect::Transfer { from, to, debit, credit } if from.0 == format!("solana:mainnet-beta:{owner}") && to.0 == format!("solana:mainnet-beta:account:{destination_account}") && debit.atoms == fin_domain::Quantity::from_u64(2_039_280) && credit.atoms == fin_domain::Quantity::from_u64(2_039_280))));
    }

    #[tokio::test]
    async fn idempotent_ata_created_after_preview_does_not_charge_unspent_rent_bound() {
        let signer = test_signer();
        let owner = signer.address().to_owned();
        let mint = bs58::encode([24u8; 32]).into_string();
        let source = bs58::encode([25u8; 32]).into_string();
        let recipient = bs58::encode([26u8; 32]).into_string();
        let rpc = Arc::new(Rpc::default());
        rpc.accounts
            .lock()
            .unwrap()
            .insert(mint.clone(), (TOKEN_PROGRAM_ID.into(), legacy_mint(6)));
        rpc.accounts.lock().unwrap().insert(
            source.clone(),
            (
                TOKEN_PROGRAM_ID.into(),
                legacy_token_account(&mint, &owner, 2_000_000),
            ),
        );
        let adapter = SolanaWalletAdapter::new(
            "synthetic",
            &owner,
            Some(signer),
            rpc.clone(),
            Arc::new(Prices),
        )
        .unwrap();
        let input = json!({"wallet":"synthetic","network":"solana","cluster":"mainnet-beta","recipient":recipient,"mint":mint,"source_token_account":source,"amount":"1.25","max_fee_sol":"0.00001","max_account_funding_sol":"0.003","requestId":"synthetic-token-ata-race"});
        let prepared = adapter.prepare("wallet.transfer", &input).await.unwrap();
        assert_eq!(prepared.binding["destination_account_created"], true);
        assert_eq!(prepared.binding["account_funding_lamports"], 2_039_280);
        let destination = prepared.binding["destination_token_account"]
            .as_str()
            .unwrap()
            .to_owned();
        // Idempotent ATA creation observes that another transaction created
        // the empty account after preview, so this write funds no rent.
        *rpc.token_receipt.lock().unwrap() = Some(TokenReceiptFixture {
            mint: mint.clone(),
            token_program: TOKEN_PROGRAM_ID.into(),
            owner: owner.clone(),
            recipient,
            source,
            destination: destination.clone(),
            source_before: 2_000_000,
            source_after: 750_000,
            destination_before: 0,
            destination_after: 1_250_000,
            account_funding_lamports: 0,
        });
        let prepared = adapter.revalidate(&prepared).await.unwrap();
        let submission = adapter
            .submit(&prepared, Arc::new(Journal::default()))
            .await
            .unwrap();
        let record = OperationRecord {
            id: "synthetic-token-ata-race-operation".into(),
            request_id: "synthetic-token-ata-race".into(),
            fingerprint: "synthetic-token-ata-race".into(),
            fingerprint_version: 1,
            operation: "wallet.transfer".into(),
            input,
            status: Status::Submitted,
            created_at: fin_domain::now(),
            updated_at: fin_domain::now(),
            preview: Some(prepared),
            approval: None,
            submission: Some(submission),
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let settlement = adapter.reconcile(&record).await.unwrap();
        assert_eq!(settlement.status, Status::Completed);
        let effects = settlement.verified.unwrap().effects;
        assert!(effects.iter().any(|effect| matches!(
            effect,
            FinancialEffect::Transfer { to, debit, .. }
                if to.0 == format!("solana:mainnet-beta:{}", record.input["recipient"].as_str().unwrap())
                    && debit.asset.reference == mint
        )));
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            FinancialEffect::Transfer { to, debit, .. }
                if to.0 == format!("solana:mainnet-beta:account:{destination}")
                    && debit.asset.reference == "native"
        )));
    }

    #[tokio::test]
    async fn partial_unwrap_uses_request_bound_temp_account_and_preserves_source_account() {
        let signer = test_signer();
        let address = signer.address().to_owned();
        let rpc = Arc::new(Rpc::default());
        let source = crate::associated_token_address(
            &address,
            "So11111111111111111111111111111111111111112",
            TOKEN_PROGRAM_ID,
        )
        .unwrap();
        let mut source_data = legacy_token_account(
            "So11111111111111111111111111111111111111112",
            &address,
            50_000_000,
        );
        source_data[109..113].copy_from_slice(&1u32.to_le_bytes());
        source_data[113..121].copy_from_slice(&2_039_280u64.to_le_bytes());
        rpc.accounts
            .lock()
            .unwrap()
            .insert(source.clone(), (TOKEN_PROGRAM_ID.into(), source_data));
        let adapter =
            SolanaWalletAdapter::new("synthetic", &address, Some(signer), rpc, Arc::new(Prices))
                .unwrap();
        let input = json!({
            "wallet":"synthetic",
            "network":"solana",
            "cluster":"mainnet-beta",
            "amount":"0.02",
            "max_fee_sol":"0.00001",
            "max_temporary_rent_sol":"0.003",
            "token_account":source,
            "requestId":"stable-partial-unwrap"
        });

        let first = adapter.prepare("wallet.unwrap", &input).await.unwrap();
        let second = adapter.prepare("wallet.unwrap", &input).await.unwrap();
        assert_eq!(first.operation, "wallet.unwrap");
        assert_eq!(
            first.binding["message_sha256"],
            second.binding["message_sha256"]
        );
        assert_eq!(
            first.binding["temporary_account"],
            second.binding["temporary_account"]
        );
        assert_eq!(first.binding["source_token_account"], source);
        assert_eq!(first.binding["amount_lamports"], 20_000_000);
        assert_eq!(first.binding["temporary_rent_lamports"], 2_039_280);

        let instructions = first.binding["instructions"].as_array().unwrap();
        assert_eq!(instructions.len(), 4);
        let token_transfer = STANDARD
            .decode(instructions[2]["data_base64"].as_str().unwrap())
            .unwrap();
        assert_eq!(token_transfer[0], 12);
        assert_eq!(
            u64::from_le_bytes(token_transfer[1..9].try_into().unwrap()),
            20_000_000
        );
        assert_eq!(token_transfer[9], 9);
        let close_account = STANDARD
            .decode(instructions[3]["data_base64"].as_str().unwrap())
            .unwrap();
        assert_eq!(close_account, vec![9]);
        assert_eq!(
            instructions[3]["accounts"][0]["address"],
            first.binding["temporary_account"]
        );

        let mut too_small = input.clone();
        too_small["max_temporary_rent_sol"] = json!("0.001");
        let error = adapter
            .prepare("wallet.unwrap", &too_small)
            .await
            .unwrap_err();
        assert_eq!(error.code, "SOLANA_TEMP_ACCOUNT_FUNDING_LIMIT");
    }

    #[tokio::test]
    async fn wrap_creates_only_wallet_ata_with_separate_rent_funding() {
        let signer = test_signer();
        let address = signer.address().to_owned();
        let rpc = Arc::new(Rpc::default());
        let adapter =
            SolanaWalletAdapter::new("synthetic", &address, Some(signer), rpc, Arc::new(Prices))
                .unwrap();
        let input = json!({
            "wallet":"synthetic",
            "network":"solana",
            "cluster":"mainnet-beta",
            "amount":"0.01",
            "max_fee_sol":"0.00001",
            "max_account_funding_sol":"0.003",
            "requestId":"synthetic-wrap-ata"
        });
        let prepared = adapter.prepare("wallet.wrap", &input).await.unwrap();
        let ata = crate::associated_token_address(
            &address,
            "So11111111111111111111111111111111111111112",
            TOKEN_PROGRAM_ID,
        )
        .unwrap();
        assert_eq!(prepared.binding["token_account"], ata);
        assert_eq!(prepared.binding["destination_account_created"], true);
        assert_eq!(prepared.binding["account_funding_lamports"], 2_039_280);
        assert!(prepared.intent.debits.iter().any(|debit| {
            debit.amount.asset.reference == "native"
                && debit.amount.atoms == fin_domain::Quantity::from_u64(2_039_280)
        }));
        assert!(prepared.intent.expected.iter().any(|effect| matches!(
            effect,
            EffectConstraint::PositionOfKind {
                position,
                maximum_change,
                position_kind,
                ..
            }
                if position == &format!("account-funding:{ata}")
                    && *position_kind == fin_domain::PositionKind::RefundableAccountFunding
                    && *maximum_change == fin_domain::Quantity::from_u64(2_039_280)
        )));
        assert_eq!(
            prepared
                .valuation
                .as_ref()
                .unwrap()
                .additional_allocation_usd_micros,
            // The shared bound rounds USD reservations conservatively to the
            // next cent: $0.203928 becomes $0.21.
            fin_domain::Quantity::from_u64(210_000)
        );
        assert_eq!(
            prepared.binding["instructions"].as_array().unwrap().len(),
            3
        );
    }

    #[tokio::test]
    async fn existing_wsol_wrap_settles_account_lamports_and_token_amount_exactly() {
        let signer = test_signer();
        let address = signer.address().to_owned();
        let token_account = crate::associated_token_address(
            &address,
            "So11111111111111111111111111111111111111112",
            TOKEN_PROGRAM_ID,
        )
        .unwrap();
        let rpc = Arc::new(Rpc::default());
        let mut account =
            legacy_token_account("So11111111111111111111111111111111111111112", &address, 0);
        account[109..113].copy_from_slice(&1u32.to_le_bytes());
        account[113..121].copy_from_slice(&2_039_280u64.to_le_bytes());
        rpc.accounts
            .lock()
            .unwrap()
            .insert(token_account.clone(), (TOKEN_PROGRAM_ID.into(), account));
        let adapter = SolanaWalletAdapter::new(
            "synthetic",
            &address,
            Some(signer),
            rpc.clone(),
            Arc::new(Prices),
        )
        .unwrap();
        let input = json!({
            "wallet":"synthetic",
            "network":"solana",
            "cluster":"mainnet-beta",
            "amount":"0.02",
            "max_fee_sol":"0.00001",
            "token_account":token_account,
            "requestId":"synthetic-wrap-existing-account"
        });
        let prepared = adapter.prepare("wallet.wrap", &input).await.unwrap();
        assert_eq!(prepared.binding["destination_account_created"], false);
        assert_eq!(prepared.binding["account_funding_lamports"], 0);
        let prepared = adapter.revalidate(&prepared).await.unwrap();
        let submission = adapter
            .submit(&prepared, Arc::new(Journal::default()))
            .await
            .unwrap();
        *rpc.wrap_receipt.lock().unwrap() = Some(WrapReceiptFixture {
            wallet: address.clone(),
            token_account: token_account.clone(),
            token_before: 0,
            amount_lamports: 20_000_000,
            rent_lamports: 2_039_280,
        });
        let record = OperationRecord {
            id: "synthetic-wrap-existing-account-operation".into(),
            request_id: "synthetic-wrap-existing-account".into(),
            fingerprint: "synthetic-wrap-existing-account".into(),
            fingerprint_version: 1,
            operation: "wallet.wrap".into(),
            input,
            status: Status::Submitted,
            created_at: fin_domain::now(),
            updated_at: fin_domain::now(),
            preview: Some(prepared),
            approval: None,
            submission: Some(submission),
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let settlement = adapter.reconcile(&record).await.unwrap();
        assert_eq!(settlement.status, Status::Completed);
        let effects = settlement.verified.unwrap().effects;
        assert!(effects.iter().any(|effect| matches!(
            effect,
            FinancialEffect::Conversion { spent, received, .. }
                if spent.asset.reference == "native"
                    && spent.atoms == fin_domain::Quantity::from_u64(20_000_000)
                    && received.asset.reference == "So11111111111111111111111111111111111111112"
                    && received.atoms == fin_domain::Quantity::from_u64(20_000_000)
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            FinancialEffect::Cost { amount, category, .. }
                if category == "solana_network_fee"
                    && amount.atoms == fin_domain::Quantity::from_u64(5_000)
        )));

        // A second operation must observe and extend the existing wrapped-SOL
        // principal instead of treating it as fresh rent or replacing it.
        let mut account = rpc
            .accounts
            .lock()
            .unwrap()
            .get(&token_account)
            .unwrap()
            .1
            .clone();
        account[64..72].copy_from_slice(&20_000_000u64.to_le_bytes());
        rpc.accounts
            .lock()
            .unwrap()
            .insert(token_account.clone(), (TOKEN_PROGRAM_ID.into(), account));
        let second_input = json!({
            "wallet":"synthetic",
            "network":"solana",
            "cluster":"mainnet-beta",
            "amount":"0.01",
            "max_fee_sol":"0.00001",
            "token_account":token_account,
            "requestId":"synthetic-wrap-existing-account-second"
        });
        let second_prepared = adapter.prepare("wallet.wrap", &second_input).await.unwrap();
        let second_prepared = adapter.revalidate(&second_prepared).await.unwrap();
        let second_submission = adapter
            .submit(&second_prepared, Arc::new(Journal::default()))
            .await
            .unwrap();
        *rpc.wrap_receipt.lock().unwrap() = Some(WrapReceiptFixture {
            wallet: address.clone(),
            token_account: token_account.clone(),
            token_before: 20_000_000,
            amount_lamports: 10_000_000,
            rent_lamports: 2_039_280,
        });
        let second_record = OperationRecord {
            id: "synthetic-wrap-existing-account-second-operation".into(),
            request_id: "synthetic-wrap-existing-account-second".into(),
            fingerprint: "synthetic-wrap-existing-account-second".into(),
            fingerprint_version: 1,
            operation: "wallet.wrap".into(),
            input: second_input,
            status: Status::Submitted,
            created_at: fin_domain::now(),
            updated_at: fin_domain::now(),
            preview: Some(second_prepared),
            approval: None,
            submission: Some(second_submission),
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let second_settlement = adapter.reconcile(&second_record).await.unwrap();
        assert_eq!(second_settlement.status, Status::Completed);
        assert!(
            second_settlement
                .verified
                .unwrap()
                .effects
                .iter()
                .any(|effect| matches!(
                    effect,
                    FinancialEffect::Conversion { spent, received, .. }
                        if spent.atoms == fin_domain::Quantity::from_u64(10_000_000)
                            && received.atoms == fin_domain::Quantity::from_u64(10_000_000)
                ))
        );
    }
}
