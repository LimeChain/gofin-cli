//! Jupiter Perpetuals JLP mint/redeem integration.
//!
//! The Perps API supplies a transaction candidate. This module pins the
//! mainnet program and JLP mint, decodes the IDL instruction arguments and
//! account order, simulates the frozen message, and settles only from a
//! finalized runtime proof.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{Duration as ChronoDuration, Utc};
use gofin_domain::{
    AccountingLocation, Adapter, Amount, AssetId, Check, CheckStatus, ConnectionMetadata,
    CostBound, CostTreatment, Definition, EffectConstraint, ExecutionActor, FinancialEffect,
    FinancialIntent, FinancialState, GoFinError, OperationKind, OperationRecord, PreparedOperation,
    Reconciliation, ResourceAmount, ResourceId, Result, SnapshotScope, StateSnapshot, Status,
    Submission, SubmissionJournal,
    boundaries::BoundarySpec,
    financial_plans::{FinancialPlanFundingIdentity, PlanPermission},
};
use gofin_pricing::UsdPriceSource;
use gofin_solana::{
    CLUSTER, NETWORK, SolanaWalletAdapter,
    transaction::{FinalizedReceiptProof, FrozenTransaction, ReceiptOutcome},
};
use reqwest::Method;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::api::{JupiterApi, JupiterHost, RateBucket, RequestPriority};

pub const PERPS_PROGRAM_ID: &str = "PERPHjGBqRHArX4DySjwM6UJHiR3sWAatqfdBS2qQJu";
pub const JLP_MINT: &str = "27G8MtK7VtTcCHkpASjSDdkWWYfoqT6ggEuKidVJidD4";
const JLP_DECIMALS: u8 = 6;
const OP_INFO: &str = "jupiter.jlp.info";
const OP_MINT: &str = "jupiter.jlp.mint";
const OP_BURN: &str = "jupiter.jlp.burn";
const DEADLINE: Duration = Duration::from_secs(20);
const COMPUTE_BUDGET: &str = "ComputeBudget111111111111111111111111111111";
const ASSOCIATED_TOKEN: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const SYSTEM: &str = "11111111111111111111111111111111";
const WRAPPED_SOL_MINT: &str = "So11111111111111111111111111111111111111112";
const MEMO: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

pub struct JlpAdapter {
    wallets: BTreeMap<String, Arc<SolanaWalletAdapter>>,
    api: Arc<dyn JupiterApi>,
    prices: Arc<dyn UsdPriceSource>,
}

impl JlpAdapter {
    pub fn new(
        wallets: BTreeMap<String, Arc<SolanaWalletAdapter>>,
        api: Arc<dyn JupiterApi>,
    ) -> Result<Self> {
        let rpc = wallets
            .values()
            .next()
            .ok_or_else(|| GoFinError::validation("JLP needs an enrolled Solana wallet."))?
            .runtime()
            .rpc()
            .clone();
        let prices: Arc<dyn UsdPriceSource> =
            Arc::new(crate::JupiterUsdPrices::new(api.clone(), rpc));
        Self::with_price_source(wallets, api, prices)
    }

    pub fn with_price_source(
        wallets: BTreeMap<String, Arc<SolanaWalletAdapter>>,
        api: Arc<dyn JupiterApi>,
        prices: Arc<dyn UsdPriceSource>,
    ) -> Result<Self> {
        if wallets.is_empty()
            || wallets
                .iter()
                .any(|(name, wallet)| name.trim().is_empty() || wallet.address().is_empty())
        {
            return Err(GoFinError::validation(
                "JLP needs at least one enrolled Solana wallet.",
            ));
        }
        Ok(Self {
            wallets,
            api,
            prices,
        })
    }

    fn wallet(&self, input: &Value) -> Result<Arc<SolanaWalletAdapter>> {
        let name = input
            .get("wallet")
            .and_then(Value::as_str)
            .ok_or_else(|| GoFinError::validation("Select an enrolled Solana wallet."))?;
        self.wallets.get(name).cloned().ok_or_else(|| {
            GoFinError::new(
                "JUPITER_WALLET_UNKNOWN",
                "connection",
                "The selected Solana wallet is not enrolled for JLP.",
                "Choose an enrolled wallet from connection discovery.",
            )
        })
    }

    fn validate_network(input: &Value) -> Result<()> {
        if input.get("network").and_then(Value::as_str) != Some("solana")
            || input
                .get("cluster")
                .and_then(Value::as_str)
                .unwrap_or(CLUSTER)
                != CLUSTER
        {
            return Err(GoFinError::validation(
                "JLP operations require Solana mainnet-beta.",
            ));
        }
        Ok(())
    }

    async fn info(&self, input: &Value) -> Result<Value> {
        Self::validate_network(input)?;
        self.api
            .request(
                JupiterHost::Perps,
                Method::GET,
                "/jlp-info",
                None,
                RateBucket::Main,
                RequestPriority::Normal,
                DEADLINE,
            )
            .await
    }

    async fn prepare_trade(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        Self::validate_network(input)?;
        let wallet = self.wallet(input)?;
        let signer = wallet.signer().ok_or_else(|| {
            GoFinError::new(
                "SOLANA_SIGNING_UNAVAILABLE",
                "prepare",
                "The selected Solana wallet has no protected signing capability.",
                "Enroll a protected signer before preparing a JLP mint or redemption.",
            )
        })?;
        let runtime = wallet.runtime();
        let (
            symbol,
            input_mint,
            input_decimals,
            input_atoms,
            output_mint,
            output_decimals,
            minimum_output,
            path,
            body,
        ) = if operation == OP_MINT {
            let symbol = required_text(input, "deposit_token")?;
            let custody = self.custody_by_symbol(symbol).await?;
            let mint = address_field(&custody, "mint")?;
            let decimals = crate::adapter::token_decimals(&runtime, mint).await?;
            let amount = required_atoms(input, "amount_atoms")?;
            let minimum = required_atoms(input, "minimum_jlp_atoms")?;
            if amount == 0 || minimum == 0 {
                return Err(GoFinError::validation(
                    "JLP amounts must be positive atomic quantities.",
                ));
            }
            (
                symbol.to_owned(),
                mint.to_owned(),
                decimals,
                amount,
                JLP_MINT.to_owned(),
                JLP_DECIMALS,
                minimum,
                "/jlp/mint",
                json!({"depositToken":symbol,"depositTokenAmount":gofin_domain::Quantity::from_u64(amount).to_decimal(decimals),"walletAddress":wallet.address()}),
            )
        } else {
            let symbol = required_text(input, "receive_token")?;
            let custody = self.custody_by_symbol(symbol).await?;
            let mint = address_field(&custody, "mint")?;
            let decimals = crate::adapter::token_decimals(&runtime, mint).await?;
            let amount = required_atoms(input, "jlp_atoms")?;
            let minimum = required_atoms(input, "minimum_receive_atoms")?;
            if amount == 0 || minimum == 0 {
                return Err(GoFinError::validation(
                    "JLP amounts must be positive atomic quantities.",
                ));
            }
            (
                symbol.to_owned(),
                JLP_MINT.to_owned(),
                JLP_DECIMALS,
                amount,
                mint.to_owned(),
                decimals,
                minimum,
                "/jlp/burn",
                json!({"burnTokenAmount":amount.to_string(),"receiveToken":symbol,"walletAddress":wallet.address()}),
            )
        };
        let response = self
            .api
            .request(
                JupiterHost::Perps,
                Method::POST,
                path,
                Some(body),
                RateBucket::Main,
                RequestPriority::Financial,
                DEADLINE,
            )
            .await?;
        let candidate = candidate_from_response(&response, output_decimals)?;
        let fee_cap = required_fee(input)?;
        let rent_cap = optional_atoms(input, "maximum_account_funding_lamports")?;
        let (
            frozen,
            arg_input,
            arg_min,
            auxiliary,
            source_account,
            destination_account,
            token_program,
        ) = inspect_and_validate(JlpInspection {
            encoded: &candidate.encoded,
            runtime: &runtime,
            signer: &signer,
            wallet: &wallet,
            operation,
            input_mint: &input_mint,
            output_mint: &output_mint,
            amount: input_atoms,
            minimum: minimum_output,
            last_valid: candidate.last_valid_block_height,
            max_fee: fee_cap,
            max_account_funding: rent_cap,
        })
        .await?;
        let _actual_fee = runtime.validate_fee(&frozen).await?;
        runtime.simulate(&frozen).await?;
        // The tx instruction's IDL-encoded minimum is the enforceable on-chain
        // bound. The API's displayed output is retained as advisory metadata.
        if arg_input != input_atoms || arg_min < minimum_output {
            return Err(unreviewed());
        }
        let input_asset = asset(&input_mint, input_decimals);
        let output_asset = asset(&output_mint, output_decimals);
        let input_program = crate::adapter::token_program_for(&runtime, &input_mint).await?;
        let output_program = crate::adapter::token_program_for(&runtime, &output_mint).await?;
        if input_program != token_program || output_program != token_program {
            return Err(unreviewed());
        }
        let (mut valuation, fee_bound) = crate::adapter::valuation(
            self.prices.clone(),
            input_asset.clone(),
            input_atoms,
            output_asset.clone(),
            candidate.expected_output,
            fee_cap,
            wallet.address(),
        )
        .await?;
        let resource = ResourceId(format!("solana:mainnet-beta:{}", wallet.address()));
        let maximum_costs = vec![fee_bound];
        // Rent-bearing accounts remain owned refundable value; the funding
        // cap is reserved separately from spend and network fee.
        valuation.additional_allocation_usd_micros = gofin_domain::Quantity::zero();
        let mut expected = vec![EffectConstraint::Conversion {
            resource: resource.clone(),
            input: input_asset.clone(),
            maximum_input: gofin_domain::Quantity::from_u64(input_atoms),
            output: output_asset.clone(),
            minimum_output: gofin_domain::Quantity::from_u64(minimum_output),
        }];
        let wallet_funding = auxiliary
            .funding_accounts
            .iter()
            .filter(|funding| funding.payer == wallet.address())
            .try_fold(0u64, |sum, funding| {
                sum.checked_add(funding.lamports).ok_or_else(bad_response)
            })?;
        let closed = &auxiliary.closed_wrapped_native_accounts;
        for funding in auxiliary.funding_accounts.iter().filter(|funding| {
            funding.payer == wallet.address() && !closed.contains(&funding.address)
        }) {
            let position = format!("account-funding:{}", funding.address);
            let amount = gofin_domain::Quantity::from_u64(funding.lamports);
            expected.push(EffectConstraint::PositionOfKind {
                resource: resource.clone(),
                position: position.clone(),
                asset: asset("native", 9),
                position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                direction: gofin_domain::Direction::Increase,
                maximum_change: amount.clone(),
            });
            expected.push(EffectConstraint::OriginMovement {
                from: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                to: AccountingLocation::Position {
                    resource: resource.clone(),
                    position,
                    position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                },
                debit_asset: asset("native", 9),
                credit_asset: asset("native", 9),
                maximum_debit: amount.clone(),
                minimum_credit: amount,
            });
        }
        if wallet_funding > 0 {
            expected.push(EffectConstraint::Debit {
                resource: resource.clone(),
                asset: asset("native", 9),
                maximum_debit: gofin_domain::Quantity::from_u64(wallet_funding),
            });
        }
        for refund in auxiliary
            .closed_wrapped_native_refunds
            .iter()
            .filter(|refund| {
                refund.refund_authority == wallet.address()
                    && !auxiliary
                        .funding_accounts
                        .iter()
                        .any(|new| new.address == refund.address)
            })
        {
            let position = format!("account-funding:{}", refund.address);
            let amount = gofin_domain::Quantity::from_u64(refund.lamports);
            expected.push(EffectConstraint::PositionOfKind {
                resource: resource.clone(),
                position: position.clone(),
                asset: asset("native", 9),
                position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                direction: gofin_domain::Direction::Decrease,
                maximum_change: amount.clone(),
            });
            expected.push(EffectConstraint::OriginMovement {
                from: AccountingLocation::Position {
                    resource: resource.clone(),
                    position,
                    position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                },
                to: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                debit_asset: asset("native", 9),
                credit_asset: asset("native", 9),
                maximum_debit: amount.clone(),
                minimum_credit: amount,
            });
            expected.push(EffectConstraint::Credit {
                to: resource.clone(),
                asset: asset("native", 9),
                minimum_credit: gofin_domain::Quantity::from_u64(refund.lamports),
            });
        }
        let debit_asset = input_asset;
        let binding = json!({"wallet_address":wallet.address(),"fee_payer":frozen.fee_payer(),"operation":operation,"resource":resource,"source_mint":input_mint,"destination_mint":output_mint,"source_decimals":input_decimals,"destination_decimals":output_decimals,"input_token_program":input_program,"output_token_program":output_program,"source_token_account":source_account,"destination_token_account":destination_account,"funding_accounts":auxiliary.funding_accounts,"closed_wrapped_native_refunds":auxiliary.closed_wrapped_native_refunds,"message_sha256":frozen.message_sha256(),"last_valid_block_height":candidate.last_valid_block_height,"blockhash":frozen.recent_blockhash(),"transaction_base64":candidate.encoded,"symbol":symbol,"input_atoms":input_atoms.to_string(),"minimum_output_atoms":minimum_output.to_string(),"expected_output_atoms":candidate.expected_output.to_string(),"maximum_network_fee_lamports":fee_cap.to_string(),"maximum_account_funding_lamports":rent_cap.to_string(),"account_funding_lamports":wallet_funding,"wrapped_native_lamports":auxiliary.wrapped_native_lamports,"closed_wrapped_native_accounts":auxiliary.closed_wrapped_native_accounts});
        let action = if operation == OP_MINT {
            "Mint"
        } else {
            "Redeem"
        };
        let (input_symbol, output_symbol) = if operation == OP_MINT {
            (symbol.as_str(), "JLP")
        } else {
            ("JLP", symbol.as_str())
        };
        let mut debits = vec![ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: debit_asset.clone(),
                atoms: gofin_domain::Quantity::from_u64(input_atoms),
            },
        }];
        if wallet_funding > 0 {
            debits.push(ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: asset("native", 9),
                    atoms: gofin_domain::Quantity::from_u64(wallet_funding),
                },
            });
        }
        let summary = format!(
            "{action} {} {input_symbol} for at least {} {output_symbol} through Jupiter Perps JLP; network fees up to {}, refundable account funding up to {}.",
            format_token_quantity(input_atoms, input_decimals),
            format_token_quantity(minimum_output, output_decimals),
            format_sol_lamports(fee_cap),
            format_sol_lamports(rent_cap),
        );
        Ok(PreparedOperation { operation: operation.into(), input: input.clone(), binding, summary, checks:vec![check("Pinned JLP mint and Perps program"),check("IDL instruction arguments and token accounts"),check("Network fee bound"),check("Account funding cap"),check("Simulation"),check("Blockhash expiry")], warnings:vec!["Settlement is reported only after finalized chain evidence proves exact token balance changes and costs.".into()], observed_at:Utc::now().to_rfc3339(), expires_at:(Utc::now()+ChronoDuration::seconds(20)).to_rfc3339(), valuation:Some(valuation), intent:FinancialIntent { debits, maximum_costs, expected, locks:vec![resource] }, action_deadline:None, operation_version:1 })
    }

    async fn custody_by_symbol(&self, symbol: &str) -> Result<Value> {
        if !matches!(symbol, "WBTC" | "ETH" | "SOL" | "USDC") {
            return Err(GoFinError::validation(
                "JLP supports only the reviewed WBTC, ETH, SOL and USDC deposit/receive assets.",
            ));
        }
        let info = self
            .api
            .request(
                JupiterHost::Perps,
                Method::GET,
                "/jlp-info",
                None,
                RateBucket::Main,
                RequestPriority::Normal,
                DEADLINE,
            )
            .await?;
        let list = info
            .get("custodies")
            .and_then(Value::as_array)
            .ok_or_else(bad_response)?;
        let matches = list
            .iter()
            .filter(|entry| entry.get("symbol").and_then(Value::as_str) == Some(symbol))
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(bad_response());
        }
        Ok(matches[0].clone())
    }

    async fn permission_scope(
        &self,
        input: &Value,
        operation: &str,
        value: &Value,
    ) -> Result<BoundarySpec> {
        let bounds: JlpBounds = serde_json::from_value(value.clone())
            .map_err(|_| GoFinError::validation("Invalid JLP authorization bounds."))?;
        validate_bounds(&bounds)?;
        let wallet = self.wallet(input)?;
        let symbol = required_text(
            input,
            if operation == OP_MINT {
                "deposit_token"
            } else {
                "receive_token"
            },
        )?;
        let wallet_name = required_text(input, "wallet")?;
        let custody = self.custody_by_symbol(symbol).await?;
        let mint = address_field(&custody, "mint")?;
        let decimals = crate::adapter::token_decimals(&wallet.runtime(), mint).await?;
        let (input_asset, output_asset) = if operation == OP_MINT {
            (asset(mint, decimals), asset(JLP_MINT, JLP_DECIMALS))
        } else {
            (asset(JLP_MINT, JLP_DECIMALS), asset(mint, decimals))
        };
        let (input_symbol, output_symbol) = if operation == OP_MINT {
            (symbol, "JLP")
        } else {
            ("JLP", symbol)
        };
        let (input_mint, output_mint) = if operation == OP_MINT {
            (mint, JLP_MINT)
        } else {
            (JLP_MINT, mint)
        };
        let resource = ResourceId(format!("solana:mainnet-beta:{}", wallet.address()));
        let mut maximum_debits = vec![ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: input_asset.clone(),
                atoms: gofin_domain::Quantity::new(&bounds.maximum_input_atoms)?,
            },
        }];
        if bounds.maximum_account_funding_lamports > 0 {
            maximum_debits.push(ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: asset("native", 9),
                    atoms: gofin_domain::Quantity::from_u64(
                        bounds.maximum_account_funding_lamports,
                    ),
                },
            });
        }
        Ok(BoundarySpec {
            fixed: json!({"operation":operation,"wallet_name":input["wallet"],"wallet_address":wallet.address(),"network":"solana","cluster":CLUSTER,"program":PERPS_PROGRAM_ID,"jlp_mint":JLP_MINT,"symbol":symbol,"input_mint":input_mint,"output_mint":output_mint,"input_asset":input_asset,"output_asset":output_asset}),
            parameters: serde_json::to_value(&bounds).map_err(|_| bad_response())?,
            maximum_debits,
            maximum_costs: vec![CostBound {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: Amount {
                    asset: asset("native", 9),
                    atoms: gofin_domain::Quantity::from_u64(bounds.maximum_network_fee_lamports),
                },
                treatment: CostTreatment::AdditionalDebit,
            }],
            summary: format!(
                "Authorize {operation} for wallet {wallet_name}: at most {} {input_symbol}, minimum output rate {} {output_symbol} per {input_symbol}, network fees up to {}, refundable account funding up to {}.",
                format_atomic_quantity(&bounds.maximum_input_atoms, input_asset.decimals,),
                format_output_rate(
                    &bounds.minimum_output_per_input_numerator,
                    &bounds.minimum_output_per_input_denominator,
                    input_asset.decimals,
                    output_asset.decimals,
                )?,
                format_sol_lamports(bounds.maximum_network_fee_lamports),
                format_sol_lamports(bounds.maximum_account_funding_lamports),
            ),
        })
    }

    fn validate_jlp_candidate(
        &self,
        spec: &BoundarySpec,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        let bounds: JlpBounds =
            serde_json::from_value(spec.parameters.clone()).map_err(|_| bad_response())?;
        validate_bounds(&bounds)?;
        let wallet_name = prepared
            .input
            .get("wallet")
            .and_then(Value::as_str)
            .ok_or_else(bad_response)?;
        let wallet = self.wallet(&prepared.input)?;
        let binding = &prepared.binding;
        let input_atoms = binding["input_atoms"]
            .as_str()
            .ok_or_else(bad_response)?
            .parse::<u64>()
            .map_err(|_| bad_response())?;
        let minimum = binding["minimum_output_atoms"]
            .as_str()
            .ok_or_else(bad_response)?
            .parse::<u64>()
            .map_err(|_| bad_response())?;
        let expected = binding["expected_output_atoms"]
            .as_str()
            .ok_or_else(bad_response)?
            .parse::<u64>()
            .map_err(|_| bad_response())?;
        let fee = binding["maximum_network_fee_lamports"]
            .as_str()
            .ok_or_else(bad_response)?
            .parse::<u64>()
            .map_err(|_| bad_response())?;
        let funding = binding["account_funding_lamports"]
            .as_u64()
            .ok_or_else(bad_response)?;
        let ratio_floor = gofin_domain::Quantity::from_u64(input_atoms).mul_div_ceil(
            &gofin_domain::Quantity::new(&bounds.minimum_output_per_input_numerator)?,
            &gofin_domain::Quantity::new(&bounds.minimum_output_per_input_denominator)?,
        )?;
        let fixed_input: AssetId = serde_json::from_value(spec.fixed["input_asset"].clone())
            .map_err(|_| bad_response())?;
        let fixed_output: AssetId = serde_json::from_value(spec.fixed["output_asset"].clone())
            .map_err(|_| bad_response())?;
        let lifetime = chrono::DateTime::parse_from_rfc3339(&prepared.expires_at)
            .map_err(|_| bad_response())?
            - chrono::DateTime::parse_from_rfc3339(&prepared.observed_at)
                .map_err(|_| bad_response())?;
        if spec.fixed["operation"] != prepared.operation
            || spec.fixed["wallet_name"].as_str() != Some(wallet_name)
            || spec.fixed["wallet_address"].as_str() != Some(wallet.address())
            || spec.fixed["network"] != "solana"
            || spec.fixed["cluster"] != CLUSTER
            || spec.fixed["program"] != PERPS_PROGRAM_ID
            || spec.fixed["jlp_mint"] != JLP_MINT
            || spec.fixed["symbol"].as_str()
                != prepared
                    .input
                    .get(if prepared.operation == OP_MINT {
                        "deposit_token"
                    } else {
                        "receive_token"
                    })
                    .and_then(Value::as_str)
            || binding["source_mint"] != spec.fixed["input_mint"]
            || binding["destination_mint"] != spec.fixed["output_mint"]
            || binding["source_decimals"].as_u64() != Some(u64::from(fixed_input.decimals))
            || binding["destination_decimals"].as_u64() != Some(u64::from(fixed_output.decimals))
            || input_atoms
                > bounds
                    .maximum_input_atoms
                    .parse::<u64>()
                    .map_err(|_| bad_response())?
            || gofin_domain::Quantity::from_u64(minimum) < ratio_floor
            || gofin_domain::Quantity::from_u64(expected) < ratio_floor
            || fee > bounds.maximum_network_fee_lamports
            || funding > bounds.maximum_account_funding_lamports
            || lifetime.num_seconds() <= 0
            || lifetime.num_seconds() > i64::from(bounds.maximum_quote_lifetime_seconds)
        {
            return Err(GoFinError::new(
                "BOUNDARY_VIOLATION",
                "authorization",
                "The JLP candidate exceeds the approved identity, amount, output, fee, funding or expiry bounds.",
                "Review the changed candidate and obtain fresh authorization.",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl Adapter for JlpAdapter {
    fn definitions(&self) -> Vec<Definition> {
        operation_definitions()
    }
    fn connections(&self) -> Vec<ConnectionMetadata> {
        self.wallets
            .iter()
            .map(|(name, wallet)| ConnectionMetadata {
                name: name.clone(),
                provider: "jupiter".into(),
                resource: ResourceId(format!("solana:mainnet-beta:{}", wallet.address())),
                role: Some("jlp".into()),
                parent: None,
                network: Some(NETWORK.into()),
                address: Some(wallet.address().into()),
                credential_ref: None,
                signing_enabled: wallet.signer().is_some(),
                details: json!({"cluster":CLUSTER,"program":PERPS_PROGRAM_ID,"jlp_mint":JLP_MINT}),
            })
            .collect()
    }
    fn snapshot_identities(&self) -> Vec<ConnectionMetadata> {
        // JLP is a pool conversion over wallet-held tokens. The wallet-family
        // adapter owns cash/token snapshots; duplicating this identity would
        // query the protocol adapter for a portfolio snapshot it does not own.
        Vec::new()
    }
    async fn snapshot(&self) -> Result<StateSnapshot> {
        Ok(StateSnapshot {
            state: FinancialState::default(),
            observed_at: Utc::now().to_rfc3339(),
            complete: true,
            coverage: vec!["JLP adapter contributes no separate wallet cash, token holdings, positions or authority state; wallet balances are observed by the Solana wallet adapter.".into()],
            pending: Vec::new(),
        })
    }
    async fn snapshot_scoped(&self, _scope: &SnapshotScope) -> Result<StateSnapshot> {
        self.snapshot().await
    }
    async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
        if operation != OP_INFO {
            return Err(unknown());
        }
        self.info(input).await
    }
    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        if operation != OP_MINT && operation != OP_BURN {
            return Err(unknown());
        }
        self.prepare_trade(operation, input).await
    }
    async fn prepare_boundaries(
        &self,
        prepared: &PreparedOperation,
        bounds: &Value,
    ) -> Result<BoundarySpec> {
        if prepared.operation != OP_MINT && prepared.operation != OP_BURN {
            return Err(unsupported_permission());
        }
        let spec = self
            .permission_scope(&prepared.input, &prepared.operation, bounds)
            .await?;
        self.validate_jlp_candidate(&spec, prepared)?;
        Ok(spec)
    }
    async fn prepare_plan_permission(
        &self,
        operation: &str,
        input: &Value,
        bounds: &Value,
    ) -> Result<PlanPermission> {
        if operation != OP_MINT && operation != OP_BURN {
            return Err(unsupported_permission());
        }
        Self::validate_network(input)?;
        let spec = self.permission_scope(input, operation, bounds).await?;
        let output_asset: AssetId = serde_json::from_value(spec.fixed["output_asset"].clone())
            .map_err(|_| bad_response())?;
        let wallet_address = spec.fixed["wallet_address"]
            .as_str()
            .ok_or_else(bad_response)?
            .to_owned();
        Ok(PlanPermission {
            recovery_parent: None,
            spec,
            possible_credits: vec![FinancialPlanFundingIdentity {
                resource: ResourceId(format!("solana:mainnet-beta:{wallet_address}")),
                asset: output_asset,
            }],
            fee_assets: vec![asset("native", 9)],
            deferred_receipt: None,
            authorization_seconds: None,
            cumulative_scope: None,
        })
    }
    fn validate_boundary_candidate(
        &self,
        spec: &BoundarySpec,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        self.validate_jlp_candidate(spec, prepared)
    }
    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        if prepared.operation != OP_MINT && prepared.operation != OP_BURN {
            return Err(unknown());
        }
        let wallet = self.wallet(&prepared.input)?;
        let signer = wallet.signer().ok_or_else(|| {
            GoFinError::new(
                "SOLANA_SIGNING_UNAVAILABLE",
                "revalidation",
                "The wallet signer is unavailable.",
                "Keep the operation unresolved and restore the enrolled signer.",
            )
        })?;
        let tx = freeze_from_binding(&prepared.binding, &wallet, &signer).await?;
        if prepared.binding["message_sha256"].as_str() != Some(tx.message_sha256()) {
            return Err(GoFinError::new(
                "JLP_MESSAGE_CHANGED",
                "revalidation",
                "The frozen JLP transaction no longer matches the approved message.",
                "Keep the operation unresolved and prepare a new exact request.",
            ));
        }
        let height = wallet
            .runtime()
            .rpc()
            .request("getBlockHeight", json!([{"commitment":"confirmed"}]))
            .await?
            .as_u64()
            .ok_or_else(bad_response)?;
        if tx
            .last_valid_block_height()
            .is_some_and(|last_valid| height > last_valid)
        {
            return Err(GoFinError::new(
                "SOLANA_BLOCKHASH_EXPIRED",
                "revalidation",
                "The approved JLP transaction has expired and was not sent.",
                "Prepare and approve a new operation; do not replay the expired transaction.",
            ));
        }
        wallet.runtime().validate_fee(&tx).await?;
        wallet.runtime().simulate(&tx).await?;
        Ok(prepared.clone())
    }
    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        if prepared.operation != OP_MINT && prepared.operation != OP_BURN {
            return Err(unknown());
        }
        let wallet = self.wallet(&prepared.input)?;
        let signer = wallet.signer().ok_or_else(unknown)?;
        let tx = freeze_from_binding(&prepared.binding, &wallet, &signer).await?;
        if prepared.binding["message_sha256"].as_str() != Some(tx.message_sha256()) {
            return Err(GoFinError::new(
                "JLP_MESSAGE_CHANGED",
                "dispatch",
                "The frozen JLP transaction no longer matches the approved message.",
                "Keep the operation unresolved and prepare a new exact request.",
            ));
        }
        wallet
            .runtime()
            .dispatch(&tx, &signer, prepared, journal)
            .await
    }
    async fn submit_revalidated(
        &self,
        validated: &gofin_domain::RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        validated.assert_fresh()?;
        let prepared = validated.prepared();
        if prepared.operation != OP_MINT && prepared.operation != OP_BURN {
            return Err(unknown());
        }
        let wallet = self.wallet(&prepared.input)?;
        let signer = wallet.signer().ok_or_else(unknown)?;
        let tx = freeze_from_binding(&prepared.binding, &wallet, &signer).await?;
        validated.assert_fresh()?;
        if prepared.binding["message_sha256"].as_str() != Some(tx.message_sha256()) {
            return Err(GoFinError::new(
                "JLP_MESSAGE_CHANGED",
                "dispatch",
                "The frozen JLP transaction no longer matches the approved message.",
                "Keep the operation unresolved and prepare a new exact request.",
            ));
        }
        wallet
            .runtime()
            .dispatch(&tx, &signer, prepared, journal)
            .await
    }
    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
        let Some(submission) = record.submission.as_ref() else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"No durable JLP submission exists."}),
                verified: None,
            });
        };
        let wallet = self.wallet(&record.input)?;
        match wallet
            .runtime()
            .reconcile_submission(submission, record.dispatch_evidence.as_ref())
            .await?
        {
            ReceiptOutcome::Pending { confirmed, reason } => Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"confirmed":confirmed,"reason":reason,"submission":submission}),
                verified: None,
            }),
            ReceiptOutcome::NotDispatched {
                reason_code,
                reason,
            } => {
                let verified =
                    crate::adapter::verify_not_dispatched(submission, &reason_code, &reason)?;
                Ok(Reconciliation {
                    status: Status::Failed,
                    evidence: verified.evidence.clone(),
                    verified: Some(verified),
                })
            }
            ReceiptOutcome::Finalized {
                signature,
                slot,
                fee_lamports,
                failed,
                message_sha256,
                proof,
            } => {
                if failed {
                    let verified = crate::adapter::verify_failed_cost(
                        record,
                        submission,
                        &signature,
                        slot,
                        fee_lamports,
                        &proof,
                    )?;
                    return Ok(Reconciliation {
                        status: Status::Failed,
                        evidence: json!({"signature":signature,"slot":slot,"fee_lamports":fee_lamports,"message_sha256":message_sha256,"proof":proof}),
                        verified: Some(verified),
                    });
                }
                let verified =
                    verify_finalized(record, submission, &signature, slot, fee_lamports, &proof)?;
                Ok(Reconciliation {
                    status: Status::Completed,
                    evidence: json!({"signature":signature,"slot":slot,"fee_lamports":fee_lamports,"message_sha256":message_sha256,"proof":proof}),
                    verified: Some(verified),
                })
            }
        }
    }
}

struct Candidate {
    encoded: String,
    last_valid_block_height: u64,
    expected_output: u64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct JlpBounds {
    maximum_input_atoms: String,
    minimum_output_per_input_numerator: String,
    minimum_output_per_input_denominator: String,
    maximum_network_fee_lamports: u64,
    maximum_account_funding_lamports: u64,
    maximum_quote_lifetime_seconds: u16,
}

fn format_atomic_quantity(atoms: &str, decimals: u8) -> String {
    let digits = atoms.trim_start_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let decimals = usize::from(decimals);
    if decimals == 0 {
        return digits.to_owned();
    }
    let padded;
    let digits = if digits.len() <= decimals {
        padded = format!("{}{}", "0".repeat(decimals + 1 - digits.len()), digits);
        padded.as_str()
    } else {
        digits
    };
    let split = digits.len() - decimals;
    let whole = &digits[..split];
    let fraction = digits[split..].trim_end_matches('0');
    if fraction.is_empty() {
        whole.to_owned()
    } else {
        format!("{whole}.{fraction}")
    }
}

fn format_token_quantity(atoms: u64, decimals: u8) -> String {
    format_atomic_quantity(&atoms.to_string(), decimals)
}

fn format_sol_lamports(lamports: u64) -> String {
    format!("{} SOL", format_token_quantity(lamports, 9))
}

fn format_output_rate(
    numerator_atoms: &str,
    denominator_atoms: &str,
    input_decimals: u8,
    output_decimals: u8,
) -> Result<String> {
    const DISPLAY_SCALE: u128 = 100_000_000;
    let numerator = numerator_atoms.parse::<u64>().map_err(|_| bad_response())?;
    let denominator = denominator_atoms
        .parse::<u64>()
        .map_err(|_| bad_response())?;
    if denominator == 0 {
        return Err(bad_response());
    }
    let input_unit = 10u128
        .checked_pow(u32::from(input_decimals))
        .ok_or_else(bad_response)?;
    let output_unit = 10u128
        .checked_pow(u32::from(output_decimals))
        .ok_or_else(bad_response)?;
    let scaled_numerator = u128::from(numerator)
        .checked_mul(input_unit)
        .and_then(|value| value.checked_mul(DISPLAY_SCALE))
        .ok_or_else(bad_response)?;
    let scaled_denominator = u128::from(denominator)
        .checked_mul(output_unit)
        .ok_or_else(bad_response)?;
    let scaled_rate = scaled_numerator / scaled_denominator;
    let whole = scaled_rate / DISPLAY_SCALE;
    let fraction = format!("{:08}", scaled_rate % DISPLAY_SCALE);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        Ok(whole.to_string())
    } else {
        Ok(format!("{whole}.{fraction}"))
    }
}

fn validate_bounds(bounds: &JlpBounds) -> Result<()> {
    let maximum = gofin_domain::Quantity::new(&bounds.maximum_input_atoms)?;
    let numerator = gofin_domain::Quantity::new(&bounds.minimum_output_per_input_numerator)?;
    let denominator = gofin_domain::Quantity::new(&bounds.minimum_output_per_input_denominator)?;
    if maximum.is_zero()
        || numerator.is_zero()
        || denominator.is_zero()
        || bounds.maximum_network_fee_lamports == 0
        || bounds.maximum_quote_lifetime_seconds == 0
        || bounds.maximum_quote_lifetime_seconds > 600
    {
        return Err(GoFinError::validation(
            "JLP bounds require positive amount, output rate and fee, with a finite quote lifetime no longer than ten minutes.",
        ));
    }
    Ok(())
}

fn unsupported_permission() -> GoFinError {
    GoFinError::new(
        "PLAN_PERMISSION_UNSUPPORTED",
        "authorization",
        "JLP permissions support only reviewed mint and redemption actions.",
        "Use an exact operation approval for this action.",
    )
}

fn candidate_from_response(response: &Value, output_decimals: u8) -> Result<Candidate> {
    if response.get("transactionType").and_then(Value::as_str) != Some("legacy") {
        return Err(bad_response());
    }
    let encoded = response
        .get("serializedTxBase64")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(bad_response)?
        .to_owned();
    let metadata = response
        .get("txMetadata")
        .filter(|v| !v.is_null())
        .ok_or_else(bad_response)?;
    let last_valid = metadata
        .get("lastValidBlockHeight")
        .and_then(Value::as_str)
        .ok_or_else(bad_response)?
        .parse::<u64>()
        .map_err(|_| bad_response())?;
    let amount = response
        .get("mintTokenAmount")
        .or_else(|| response.get("receiveTokenAmount"))
        .and_then(Value::as_str)
        .ok_or_else(bad_response)?;
    let expected = gofin_domain::Quantity::from_decimal(amount, output_decimals)
        .map_err(|_| bad_response())?;
    Ok(Candidate {
        encoded,
        last_valid_block_height: last_valid,
        expected_output: expected.as_str().parse().map_err(|_| bad_response())?,
    })
}

struct JlpInspection<'a> {
    encoded: &'a str,
    runtime: &'a Arc<gofin_solana::transaction::SolanaRuntime>,
    signer: &'a Arc<gofin_solana::SolanaSigner>,
    wallet: &'a SolanaWalletAdapter,
    operation: &'a str,
    input_mint: &'a str,
    output_mint: &'a str,
    amount: u64,
    minimum: u64,
    last_valid: u64,
    max_fee: u64,
    max_account_funding: u64,
}

async fn inspect_and_validate(
    input: JlpInspection<'_>,
) -> Result<(
    FrozenTransaction,
    u64,
    u64,
    crate::adapter::AuxiliaryEffects,
    String,
    String,
    String,
)> {
    let JlpInspection {
        encoded,
        runtime,
        signer,
        wallet,
        operation,
        input_mint,
        output_mint,
        amount,
        minimum,
        last_valid,
        max_fee,
        max_account_funding,
    } = input;
    let frozen = runtime
        .freeze_transaction(encoded, signer, last_valid, max_fee, None, &[])
        .await?;
    let auxiliary = crate::adapter::validate_auxiliary_instructions(
        wallet,
        &frozen,
        &[
            input_mint.to_owned(),
            output_mint.to_owned(),
            JLP_MINT.to_owned(),
        ],
        (input_mint == WRAPPED_SOL_MINT).then_some(amount),
        max_account_funding,
    )
    .await?;
    let auxiliary_writable = frozen
        .instructions()
        .iter()
        .filter(|instruction| instruction.program_id != PERPS_PROGRAM_ID)
        .flat_map(|instruction| instruction.accounts.iter())
        .filter(|account| account.writable)
        .map(|account| account.address.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let expected_name = if operation == OP_MINT {
        "add_liquidity2"
    } else {
        "remove_liquidity2"
    };
    let expected_discriminator = anchor_discriminator(expected_name);
    let mut primary = None;
    for ix in frozen.instructions() {
        if ix.program_id == PERPS_PROGRAM_ID {
            if primary.is_some() {
                return Err(unreviewed_reason("single primary liquidity instruction"));
            }
            let data = STANDARD
                .decode(&ix.data_base64)
                .map_err(|_| bad_response())?;
            if data.len() < 8
                || data[..8] != expected_discriminator
                || ix.accounts.len() != 14
                || ix.accounts[0].address != wallet.address()
                || !ix.accounts[0].signer
                || ix.accounts[10].address != JLP_MINT
                || ix.accounts[3].address != pda(&[b"transfer_authority"])?
                || ix.accounts[4].address != pda(&[b"perpetuals"])?
                || ix.accounts[12].address != pda(&[b"__event_authority"])?
                || ix.accounts[13].address != PERPS_PROGRAM_ID
            {
                return Err(unreviewed_reason(
                    "pinned instruction identity or fixed account graph",
                ));
            }
            for (index, account) in ix.accounts.iter().enumerate() {
                let auxiliary_write = auxiliary_writable.contains(account.address.as_str());
                if !valid_global_privilege(index, account.signer, account.writable, auxiliary_write)
                {
                    return Err(unreviewed_reason(
                        "pinned account signer or writable privileges",
                    ));
                }
            }
            let token_program = ix.accounts[11].address.as_str();
            if token_program != gofin_solana::token::TOKEN_PROGRAM_ID
                && token_program != gofin_solana::token::TOKEN_2022_PROGRAM_ID
            {
                return Err(unreviewed_reason("token program account"));
            }
            if crate::adapter::token_program_for(runtime, input_mint).await? != token_program
                || crate::adapter::token_program_for(runtime, output_mint).await? != token_program
            {
                return Err(unreviewed_reason("finalized mint program identity"));
            }
            let (in_amount, min_out) = decode_args(expected_name, &data[8..])?;
            if operation == OP_MINT && ix.accounts[1].address == ix.accounts[2].address
                || operation == OP_BURN && ix.accounts[1].address == ix.accounts[2].address
            {
                return Err(unreviewed_reason(
                    "exact liquidity amount or minimum output",
                ));
            }
            if operation == OP_MINT && in_amount != amount
                || operation == OP_BURN && in_amount != amount
                || min_out < minimum
            {
                return Err(GoFinError::new(
                    "JLP_INSTRUCTION_UNREVIEWED",
                    "prepare",
                    format!(
                        "The JLP instruction amount/minimum do not match approval (input {in_amount}, approved {amount}; minimum {min_out}, required {minimum})."
                    ),
                    "Do not submit this transaction; wait for a reviewed Jupiter Perpetuals deployment profile.",
                ));
            }
            verify_token_account(
                wallet,
                &ix.accounts[1].address,
                if operation == OP_MINT {
                    input_mint
                } else {
                    output_mint
                },
                token_program,
                &frozen,
                operation == OP_BURN,
            )
            .await?;
            verify_token_account(
                wallet,
                &ix.accounts[2].address,
                JLP_MINT,
                token_program,
                &frozen,
                operation == OP_MINT,
            )
            .await?;
            validate_perps_pool_custody(
                runtime,
                &ix.accounts[4].address,
                &ix.accounts[5].address,
                &ix.accounts[6].address,
                &ix.accounts[9].address,
                &ix.accounts[10].address,
                if operation == OP_MINT {
                    input_mint
                } else {
                    output_mint
                },
            )
            .await?;
            validate_jlp_mint(runtime, token_program)
                .await
                .map_err(|_| unreviewed_reason("finalized JLP mint identity or decimals"))?;
            let (source_account, destination_account) = if operation == OP_MINT {
                (
                    ix.accounts[1].address.clone(),
                    ix.accounts[2].address.clone(),
                )
            } else {
                (
                    ix.accounts[2].address.clone(),
                    ix.accounts[1].address.clone(),
                )
            };
            primary = Some((
                in_amount,
                min_out,
                source_account,
                destination_account,
                token_program.to_owned(),
            ));
        } else {
            if !matches!(
                ix.program_id.as_str(),
                COMPUTE_BUDGET
                    | ASSOCIATED_TOKEN
                    | SYSTEM
                    | gofin_solana::token::TOKEN_PROGRAM_ID
                    | gofin_solana::token::TOKEN_2022_PROGRAM_ID
                    | MEMO
            ) {
                return Err(unreviewed_reason("supported auxiliary instruction program"));
            }
        }
    }
    let (input, minimum, source_account, destination_account, token_program) =
        primary.ok_or_else(unreviewed)?;
    Ok((
        frozen,
        input,
        minimum,
        auxiliary,
        source_account,
        destination_account,
        token_program,
    ))
}

fn valid_global_privilege(
    index: usize,
    signer: bool,
    writable: bool,
    auxiliary_write: bool,
) -> bool {
    let signer_required = index == 0;
    let writable_required = matches!(index, 0 | 1 | 2 | 5 | 6 | 9 | 10);
    let writable_allowed = writable_required || auxiliary_write;
    signer == signer_required && (!writable_required || writable) && (!writable || writable_allowed)
}

async fn verify_token_account(
    wallet: &SolanaWalletAdapter,
    account: &str,
    mint: &str,
    token_program: &str,
    frozen: &FrozenTransaction,
    allow_create: bool,
) -> Result<()> {
    let response = wallet
        .runtime()
        .rpc()
        .request(
            "getAccountInfo",
            json!([account,{"encoding":"base64","commitment":"finalized"}]),
        )
        .await
        .map_err(|_| unreviewed_reason("finalized wallet token-account identity or permission"))?;
    let Some(value) = response.get("value").filter(|v| !v.is_null()) else {
        if allow_create
            && associated_address(wallet.address(), token_program, mint)? == account
            && frozen.instructions().iter().any(|ix| {
                ix.program_id == ASSOCIATED_TOKEN
                    && ix
                        .accounts
                        .get(1)
                        .is_some_and(|meta| meta.address == account)
            })
        {
            return Ok(());
        }
        return Err(unreviewed());
    };
    if value.get("owner").and_then(Value::as_str) != Some(token_program) {
        return Err(unreviewed());
    }
    let bytes =
        gofin_solana::token::decode_rpc_account_data(value.get("data").ok_or_else(unreviewed)?)?;
    let parsed = gofin_solana::token::parse_token_account(token_program, account, &bytes)?;
    if parsed.mint != mint
        || parsed.token_owner != wallet.address()
        || parsed.state != "initialized"
        || parsed.delegate.is_some() && parsed.delegated_amount_atoms > 0
    {
        return Err(unreviewed());
    }
    Ok(())
}

fn associated_address(owner: &str, token_program: &str, mint: &str) -> Result<String> {
    let owner = owner
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| unreviewed())?;
    let token_program = token_program
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| unreviewed())?;
    let mint = mint
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| unreviewed())?;
    let associated_program = ASSOCIATED_TOKEN
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| unreviewed())?;
    Ok(solana_pubkey3::Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &associated_program,
    )
    .0
    .to_string())
}

async fn validate_perps_pool_custody(
    runtime: &Arc<gofin_solana::transaction::SolanaRuntime>,
    perpetuals_address: &str,
    pool_address: &str,
    custody_address: &str,
    custody_token_account: &str,
    lp_mint: &str,
    expected_mint: &str,
) -> Result<()> {
    let pool = account_data(runtime, pool_address, PERPS_PROGRAM_ID)
        .await
        .map_err(|_| unreviewed_reason("finalized Pool account owner or data"))?;
    let custody = account_data(runtime, custody_address, PERPS_PROGRAM_ID)
        .await
        .map_err(|_| unreviewed_reason("finalized Custody account owner or data"))?;
    let pool_disc = account_discriminator("Pool");
    let custody_disc = account_discriminator("Custody");
    if pool.get(..8) != Some(pool_disc.as_slice())
        || custody.get(..8) != Some(custody_disc.as_slice())
    {
        return Err(unreviewed_reason("Pool/Custody account discriminator"));
    }

    // The pinned IDL orders Custody's first fields as pool, mint, tokenAccount,
    // decimals. These offsets are stable Borsh encodings of public keys/u8s.
    if custody.len() < 105
        || bs58::encode(&custody[8..40]).into_string() != pool_address
        || bs58::encode(&custody[40..72]).into_string() != expected_mint
        || bs58::encode(&custody[72..104]).into_string() != custody_token_account
    {
        return Err(unreviewed_reason(
            "Custody pool, mint, or token-account fields",
        ));
    }
    let mint_decimals = crate::adapter::token_decimals(runtime, expected_mint).await?;
    if custody[104] != mint_decimals {
        return Err(unreviewed_reason("Custody decimals"));
    }

    // Pool's pinned IDL prefix is UTF-8 name then vec<Pubkey> custodies. Check
    // the vector contains this exact program-owned custody account rather than
    // accepting API symbol metadata as an identity.
    let name_len = read_u32(&pool, 8)? as usize;
    let name_end = 12usize.checked_add(name_len).ok_or_else(unreviewed)?;
    if name_len == 0
        || name_len > 64
        || name_end.checked_add(4).is_none_or(|end| end > pool.len())
        || std::str::from_utf8(&pool[12..name_end]).is_err()
    {
        return Err(unreviewed_reason("Pool Borsh name and custody vector"));
    }
    let pool_name = std::str::from_utf8(&pool[12..name_end]).map_err(|_| unreviewed())?;
    let count = read_u32(&pool, name_end)? as usize;
    if count == 0 || count > 32 {
        return Err(unreviewed_reason("Pool custody membership"));
    }
    let keys_start = name_end + 4;
    let keys_end = keys_start
        .checked_add(count.checked_mul(32).ok_or_else(unreviewed)?)
        .ok_or_else(unreviewed)?;
    if keys_end > pool.len()
        || !pool[keys_start..keys_end]
            .chunks_exact(32)
            .any(|key| bs58::encode(key).into_string() == custody_address)
    {
        return Err(unreviewed_reason("Pool custody membership"));
    }
    let program = PERPS_PROGRAM_ID
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| unreviewed())?;
    let perpetuals = solana_pubkey3::Pubkey::find_program_address(&[b"perpetuals"], &program).0;
    let pool_pda =
        solana_pubkey3::Pubkey::find_program_address(&[b"pool", pool_name.as_bytes()], &program).0;
    let pool_key = pool_address
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| unreviewed())?;
    let mint_key = expected_mint
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| unreviewed())?;
    let custody_pda = solana_pubkey3::Pubkey::find_program_address(
        &[b"custody", pool_key.as_ref(), mint_key.as_ref()],
        &program,
    )
    .0;
    let lp_mint_pda = solana_pubkey3::Pubkey::find_program_address(
        &[b"lp_token_mint", pool_key.as_ref()],
        &program,
    )
    .0;
    if perpetuals.to_string() != perpetuals_address {
        return Err(unreviewed_reason("Perps singleton PDA"));
    }
    if pool_pda.to_string() != pool_address {
        return Err(unreviewed_reason("Pool PDA"));
    }
    if custody_pda.to_string() != custody_address {
        return Err(unreviewed_reason("Custody PDA"));
    }
    if lp_mint_pda.to_string() != lp_mint || lp_mint != JLP_MINT {
        return Err(unreviewed_reason("JLP mint PDA"));
    }
    Ok(())
}

fn pda(seeds: &[&[u8]]) -> Result<String> {
    let program = PERPS_PROGRAM_ID
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| unreviewed())?;
    Ok(
        solana_pubkey3::Pubkey::find_program_address(seeds, &program)
            .0
            .to_string(),
    )
}

async fn validate_jlp_mint(
    runtime: &Arc<gofin_solana::transaction::SolanaRuntime>,
    token_program: &str,
) -> Result<()> {
    let response = runtime
        .rpc()
        .request(
            "getAccountInfo",
            json!([JLP_MINT,{"encoding":"base64","commitment":"finalized"}]),
        )
        .await
        .map_err(|_| unreviewed_reason("finalized JLP token-account identity or permission"))?;
    let value = response
        .get("value")
        .filter(|v| !v.is_null())
        .ok_or_else(unreviewed)?;
    if value.get("owner").and_then(Value::as_str) != Some(token_program) {
        return Err(unreviewed());
    }
    let bytes =
        gofin_solana::token::decode_rpc_account_data(value.get("data").ok_or_else(unreviewed)?)?;
    let epoch = runtime
        .rpc()
        .request("getEpochInfo", json!([{"commitment":"finalized"}]))
        .await?
        .get("epoch")
        .and_then(Value::as_u64)
        .ok_or_else(unreviewed)?;
    let parsed = gofin_solana::token::parse_mint(token_program, &bytes, epoch)?;
    if !parsed.initialized || parsed.decimals != JLP_DECIMALS || parsed.program_id != token_program
    {
        return Err(unreviewed());
    }
    Ok(())
}

async fn account_data(
    runtime: &Arc<gofin_solana::transaction::SolanaRuntime>,
    address: &str,
    expected_owner: &str,
) -> Result<Vec<u8>> {
    let response = runtime
        .rpc()
        .request(
            "getAccountInfo",
            json!([address,{"encoding":"base64","commitment":"finalized"}]),
        )
        .await?;
    let account = response
        .get("value")
        .filter(|v| !v.is_null())
        .ok_or_else(unreviewed)?;
    if account.get("owner").and_then(Value::as_str) != Some(expected_owner) {
        return Err(unreviewed());
    }
    gofin_solana::token::decode_rpc_account_data(account.get("data").ok_or_else(unreviewed)?)
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    let bytes = data.get(offset..offset + 4).ok_or_else(unreviewed)?;
    Ok(u32::from_le_bytes(
        bytes.try_into().map_err(|_| unreviewed())?,
    ))
}

fn account_discriminator(name: &str) -> [u8; 8] {
    let digest = Sha256::digest(format!("account:{name}").as_bytes());
    digest[..8].try_into().expect("eight byte digest prefix")
}

async fn freeze_from_binding(
    binding: &Value,
    wallet: &SolanaWalletAdapter,
    signer: &Arc<gofin_solana::SolanaSigner>,
) -> Result<FrozenTransaction> {
    let encoded = binding
        .get("transaction_base64")
        .and_then(Value::as_str)
        .ok_or_else(bad_response)?;
    let last = binding
        .get("last_valid_block_height")
        .and_then(Value::as_u64)
        .ok_or_else(bad_response)?;
    let max = binding
        .get("maximum_network_fee_lamports")
        .and_then(Value::as_str)
        .ok_or_else(bad_response)?
        .parse::<u64>()
        .map_err(|_| bad_response())?;
    wallet
        .runtime()
        .freeze_transaction(encoded, signer, last, max, None, &[])
        .await
}

fn decode_args(name: &str, data: &[u8]) -> Result<(u64, u64)> {
    if name == "add_liquidity2" {
        // Borsh `Option<u64>::None` is exactly its one-byte tag; accepting
        // `Some` would allow the IDL's pre-swap amount to escape our exact
        // single-asset debit bound.
        if data.len() != 17 || data[16] != 0 {
            return Err(unreviewed());
        }
        Ok((
            u64::from_le_bytes(data[0..8].try_into().map_err(|_| unreviewed())?),
            u64::from_le_bytes(data[8..16].try_into().map_err(|_| unreviewed())?),
        ))
    } else {
        if data.len() != 16 {
            return Err(unreviewed());
        }
        Ok((
            u64::from_le_bytes(data[0..8].try_into().map_err(|_| unreviewed())?),
            u64::from_le_bytes(data[8..16].try_into().map_err(|_| unreviewed())?),
        ))
    }
}

fn anchor_discriminator(name: &str) -> [u8; 8] {
    let digest = Sha256::digest(format!("global:{name}").as_bytes());
    digest[..8].try_into().expect("eight byte digest prefix")
}

fn verify_finalized(
    record: &OperationRecord,
    submission: &Submission,
    signature: &str,
    slot: u64,
    fee: u64,
    proof: &FinalizedReceiptProof,
) -> Result<gofin_domain::VerifiedEffects> {
    let binding = record
        .preview
        .as_ref()
        .map(|p| &p.binding)
        .ok_or_else(bad_response)?;
    let wallet = binding
        .get("wallet_address")
        .and_then(Value::as_str)
        .ok_or_else(bad_response)?;
    let source = binding
        .get("source_mint")
        .and_then(Value::as_str)
        .ok_or_else(bad_response)?;
    let destination = binding
        .get("destination_mint")
        .and_then(Value::as_str)
        .ok_or_else(bad_response)?;
    let source_account = binding["source_token_account"]
        .as_str()
        .ok_or_else(bad_response)?;
    let destination_account = binding["destination_token_account"]
        .as_str()
        .ok_or_else(bad_response)?;
    let source_program = binding["input_token_program"]
        .as_str()
        .ok_or_else(bad_response)?;
    let destination_program = binding["output_token_program"]
        .as_str()
        .ok_or_else(bad_response)?;
    let source_decimals = binding["source_decimals"]
        .as_u64()
        .and_then(|v| u8::try_from(v).ok())
        .ok_or_else(bad_response)?;
    let destination_decimals = binding["destination_decimals"]
        .as_u64()
        .and_then(|v| u8::try_from(v).ok())
        .ok_or_else(bad_response)?;
    let expected_input = binding["input_atoms"]
        .as_str()
        .ok_or_else(bad_response)?
        .parse::<u128>()
        .map_err(|_| bad_response())?;
    let minimum = binding["minimum_output_atoms"]
        .as_str()
        .ok_or_else(bad_response)?
        .parse::<u128>()
        .map_err(|_| bad_response())?;
    let closed = binding
        .get("closed_wrapped_native_accounts")
        .and_then(Value::as_array)
        .ok_or_else(bad_response)?;
    let destination_closed = destination == WRAPPED_SOL_MINT
        && closed
            .iter()
            .any(|key| key.as_str() == Some(destination_account));
    let wrapped_native = binding
        .get("wrapped_native_lamports")
        .and_then(Value::as_u64)
        .ok_or_else(bad_response)?;
    let funding: Vec<crate::adapter::AccountFundingRequirement> = serde_json::from_value(
        binding
            .get("funding_accounts")
            .cloned()
            .ok_or_else(bad_response)?,
    )
    .map_err(|_| bad_response())?;
    let refunds: Vec<crate::adapter::AccountFundingRequirement> = serde_json::from_value(
        binding
            .get("closed_wrapped_native_refunds")
            .cloned()
            .ok_or_else(bad_response)?,
    )
    .map_err(|_| bad_response())?;
    let fee_payer = submission
        .data
        .get("fee_payer")
        .and_then(Value::as_str)
        .ok_or_else(bad_response)?;
    if proof.account_keys.first().map(String::as_str) != Some(fee_payer) {
        return Err(bad_response());
    }
    let wallet_fee = if fee_payer == wallet { fee } else { 0 };
    let wallet_delta = proof
        .lamport_delta(wallet)?
        .as_str()
        .parse::<i128>()
        .map_err(|_| bad_response())?;
    let native_input = source == WRAPPED_SOL_MINT && u128::from(wrapped_native) == expected_input;
    let source_delta = if native_input {
        gofin_domain::SignedQuantity::positive(&gofin_domain::Quantity::zero())
    } else {
        proof.token_account_delta_verified(
            source_account,
            wallet,
            source,
            source_program,
            source_decimals,
        )?
    };
    let destination_delta = if destination_closed {
        gofin_domain::SignedQuantity::positive(&gofin_domain::Quantity::zero())
    } else {
        proof.token_account_delta_verified(
            destination_account,
            wallet,
            destination,
            destination_program,
            destination_decimals,
        )?
    };
    reject_unapproved_wallet_token_changes(
        proof,
        wallet,
        &[source_account.to_owned(), destination_account.to_owned()],
    )?;
    let rent_cap = binding["maximum_account_funding_lamports"]
        .as_str()
        .ok_or_else(bad_response)?
        .parse::<u64>()
        .map_err(|_| bad_response())?;
    let debit = if native_input {
        expected_input
    } else if source_delta.is_negative() {
        source_delta
            .magnitude()
            .as_str()
            .parse::<u128>()
            .map_err(|_| bad_response())?
    } else {
        0
    };
    let credit = if destination_closed {
        if refunds
            .iter()
            .find(|refund| refund.address == destination_account)
            .is_none_or(|refund| refund.refund_authority != wallet)
        {
            return Err(bad_response());
        }
        let wallet_funding = funding
            .iter()
            .filter(|account| account.payer == wallet)
            .try_fold(0i128, |total, account| {
                total
                    .checked_add(i128::from(account.lamports))
                    .ok_or_else(bad_response)
            })?;
        let wallet_refunds = refunds
            .iter()
            .filter(|account| account.refund_authority == wallet)
            .try_fold(0i128, |total, account| {
                total
                    .checked_add(i128::from(account.lamports))
                    .ok_or_else(bad_response)
            })?;
        u128::try_from(
            wallet_delta
                .checked_add(i128::from(wallet_fee))
                .and_then(|value| value.checked_add(wallet_funding))
                .and_then(|value| value.checked_sub(wallet_refunds))
                .ok_or_else(bad_response)?,
        )
        .map_err(|_| bad_response())?
    } else if destination_delta.is_negative() {
        0
    } else {
        destination_delta
            .as_str()
            .parse::<u128>()
            .map_err(|_| bad_response())?
    };
    if debit != expected_input || credit < minimum {
        return Err(GoFinError::new(
            "JLP_SETTLEMENT_EFFECT_MISMATCH",
            "reconciliation",
            "The finalized token balances do not prove the approved JLP input debit and minimum output credit.",
            "Keep the operation unresolved and inspect the finalized transaction effects.",
        ));
    }
    let debit_asset = record
        .preview
        .as_ref()
        .and_then(|p| p.intent.debits.first())
        .map(|d| d.amount.asset.clone())
        .ok_or_else(bad_response)?;
    let credit_asset = record
        .preview
        .as_ref()
        .and_then(|p| p.intent.expected.first())
        .and_then(|e| match e {
            EffectConstraint::Conversion { output, .. } => Some(output.clone()),
            _ => None,
        })
        .ok_or_else(bad_response)?;
    let resource = ResourceId(format!("solana:mainnet-beta:{wallet}"));
    let mut effects = vec![FinancialEffect::Conversion {
        resource: resource.clone(),
        spent: Amount {
            asset: debit_asset,
            atoms: gofin_domain::Quantity::new(expected_input.to_string())?,
        },
        received: Amount {
            asset: credit_asset,
            atoms: gofin_domain::Quantity::new(credit.to_string())?,
        },
    }];
    if wallet_fee > 0 {
        effects.push(FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: asset("native", 9),
                atoms: gofin_domain::Quantity::from_u64(wallet_fee),
            },
            category: "solana_network_fee".into(),
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    let principal_delta = -i128::from(if native_input { wrapped_native } else { 0 })
        + if destination_closed {
            i128::try_from(credit).map_err(|_| bad_response())?
        } else {
            0
        };
    let native = asset("native", 9);
    let mut rent_cap_used = 0u64;
    let mut expected_wallet_lamports = principal_delta
        .checked_sub(wallet_fee as i128)
        .ok_or_else(bad_response)?;
    for account in &funding {
        if account.refund_authority != wallet || account.lamports == 0 {
            return Err(bad_response());
        }
        let delta = proof
            .lamport_delta(&account.address)?
            .as_str()
            .parse::<i128>()
            .map_err(|_| bad_response())?;
        let created_and_closed = closed
            .iter()
            .any(|key| key.as_str() == Some(&account.address));
        if (created_and_closed && delta != 0)
            || (!created_and_closed && delta != i128::from(account.lamports))
        {
            return Err(bad_response());
        }
        if created_and_closed || account.payer != wallet {
            continue;
        }
        rent_cap_used = rent_cap_used
            .checked_add(account.lamports)
            .ok_or_else(bad_response)?;
        expected_wallet_lamports = expected_wallet_lamports
            .checked_sub(i128::from(account.lamports))
            .ok_or_else(bad_response)?;
        let qty = gofin_domain::Quantity::from_u64(account.lamports);
        let position = format!("account-funding:{}", account.address);
        let anchor = gofin_domain::PositionAnchor {
            resource: resource.clone(),
            position: position.clone(),
            position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
            principal: Amount {
                asset: native.clone(),
                atoms: qty.clone(),
            },
            scaled_receipt: None,
            evidence_version: 1,
            evidence: json!({"cluster":CLUSTER,"account":account.address,"account_owner":account.account_owner,"refund_authority":account.refund_authority,"rent_lamports":account.lamports,"signature":signature,"message_sha256":proof.message_sha256}),
            observed_at: Utc::now().to_rfc3339(),
        };
        effects.push(FinancialEffect::PositionChange {
            resource: resource.clone(),
            position: position.clone(),
            asset: native.clone(),
            delta: gofin_domain::SignedQuantity::positive(&qty),
            scaled_receipt: None,
            anchor: Some(anchor),
        });
        effects.push(FinancialEffect::CashDebit {
            resource: resource.clone(),
            amount: Amount {
                asset: native.clone(),
                atoms: qty.clone(),
            },
        });
        effects.push(FinancialEffect::OriginMovement {
            from: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            to: AccountingLocation::Position {
                resource: resource.clone(),
                position,
                position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
            },
            debit: Amount {
                asset: native.clone(),
                atoms: qty.clone(),
            },
            credit: Amount {
                asset: native.clone(),
                atoms: qty,
            },
        });
    }
    for refund in &refunds {
        if refund.refund_authority != wallet
            || refund.lamports == 0
            || !closed
                .iter()
                .any(|key| key.as_str() == Some(&refund.address))
        {
            return Err(bad_response());
        }
        let delta = proof
            .lamport_delta(&refund.address)?
            .as_str()
            .parse::<i128>()
            .map_err(|_| bad_response())?;
        let created_here = funding
            .iter()
            .any(|new| new.address == refund.address && new.lamports == refund.lamports);
        if (created_here && delta != 0) || (!created_here && delta != -i128::from(refund.lamports))
        {
            return Err(bad_response());
        }
        if created_here {
            continue;
        }
        expected_wallet_lamports = expected_wallet_lamports
            .checked_add(i128::from(refund.lamports))
            .ok_or_else(bad_response)?;
        let qty = gofin_domain::Quantity::from_u64(refund.lamports);
        let position = format!("account-funding:{}", refund.address);
        let anchor = gofin_domain::PositionAnchor {
            resource: resource.clone(),
            position: position.clone(),
            position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
            principal: Amount {
                asset: native.clone(),
                // The finalized account is closed. Anchor the resulting
                // position (zero), while the signed delta records returned
                // rent separately as spendable cash.
                atoms: gofin_domain::Quantity::zero(),
            },
            scaled_receipt: None,
            evidence_version: 1,
            evidence: json!({"cluster":CLUSTER,"account":refund.address,"account_owner":refund.account_owner,"refund_authority":refund.refund_authority,"returned_rent_lamports":refund.lamports,"signature":signature,"message_sha256":proof.message_sha256}),
            observed_at: Utc::now().to_rfc3339(),
        };
        effects.push(FinancialEffect::PositionChange {
            resource: resource.clone(),
            position: position.clone(),
            asset: native.clone(),
            delta: gofin_domain::SignedQuantity::negative(&qty),
            scaled_receipt: None,
            anchor: Some(anchor),
        });
        effects.push(FinancialEffect::UsableCredit {
            to: resource.clone(),
            amount: Amount {
                asset: native.clone(),
                atoms: qty.clone(),
            },
        });
        effects.push(FinancialEffect::OriginMovement {
            from: AccountingLocation::Position {
                resource: resource.clone(),
                position,
                position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
            },
            to: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            debit: Amount {
                asset: native.clone(),
                atoms: qty.clone(),
            },
            credit: Amount {
                asset: native.clone(),
                atoms: qty,
            },
        });
    }
    if rent_cap_used > rent_cap || wallet_delta != expected_wallet_lamports {
        return Err(GoFinError::new(
            "JLP_NATIVE_EFFECT_MISMATCH",
            "reconciliation",
            "Finalized SOL changes do not match the approved fee, rent funding and native-token effects.",
            "Keep the operation unresolved and inspect the finalized account and token balance proof.",
        ));
    }
    let evidence = json!({"cluster":CLUSTER,"program":PERPS_PROGRAM_ID,"jlp_mint":JLP_MINT,"signature":signature,"slot":slot,"network_fee_lamports":fee,"proof":proof});
    let mut verified = gofin_domain::VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects,
        evidence,
        observed_at: Utc::now().to_rfc3339(),
        proof_digest: String::new(),
    };
    verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
    Ok(verified)
}

fn reject_unapproved_wallet_token_changes(
    proof: &FinalizedReceiptProof,
    wallet: &str,
    permitted_accounts: &[String],
) -> Result<()> {
    use std::collections::BTreeMap;
    type TokenIdentity = (String, String, u64, String);
    fn rows(
        values: &[Value],
        keys: &[String],
        wallet: &str,
    ) -> Result<BTreeMap<usize, TokenIdentity>> {
        let mut result = BTreeMap::new();
        for row in values {
            if row.get("owner").and_then(Value::as_str) != Some(wallet) {
                continue;
            }
            let index = row
                .get("accountIndex")
                .and_then(Value::as_u64)
                .and_then(|v| usize::try_from(v).ok())
                .ok_or_else(bad_response)?;
            if keys.get(index).is_none() {
                return Err(bad_response());
            }
            let mint = row
                .get("mint")
                .and_then(Value::as_str)
                .ok_or_else(bad_response)?
                .to_owned();
            let program = row
                .get("programId")
                .and_then(Value::as_str)
                .ok_or_else(bad_response)?
                .to_owned();
            let amount = row.get("uiTokenAmount").ok_or_else(bad_response)?;
            let decimals = amount
                .get("decimals")
                .and_then(Value::as_u64)
                .ok_or_else(bad_response)?;
            let atoms = amount
                .get("amount")
                .and_then(Value::as_str)
                .ok_or_else(bad_response)?
                .to_owned();
            if result
                .insert(index, (mint, program, decimals, atoms))
                .is_some()
            {
                return Err(bad_response());
            }
        }
        Ok(result)
    }
    let allowed = permitted_accounts
        .iter()
        .collect::<std::collections::BTreeSet<_>>();
    let before = rows(&proof.pre_token_balances, &proof.account_keys, wallet)?;
    let after = rows(&proof.post_token_balances, &proof.account_keys, wallet)?;
    for index in before
        .keys()
        .chain(after.keys())
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
    {
        let address = proof.account_keys.get(index).ok_or_else(bad_response)?;
        if allowed.contains(address) {
            continue;
        }
        if before.get(&index) != after.get(&index) {
            return Err(GoFinError::new(
                "JLP_UNAPPROVED_TOKEN_EFFECT",
                "reconciliation",
                "The finalized transaction changed another wallet token account outside the approved JLP accounts.",
                "Keep the operation unresolved and inspect all token-account balance changes.",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
fn net_account_funding(
    principal_delta: i128,
    fee: i128,
    wallet_delta: i128,
    cap: u64,
) -> Result<u64> {
    let rent = principal_delta
        .checked_sub(fee)
        .and_then(|value| value.checked_sub(wallet_delta))
        .ok_or_else(bad_response)?;
    if rent < 0 {
        return Err(GoFinError::new(
            "JLP_RENT_REFUND_UNMODELED",
            "reconciliation",
            "The finalized transaction includes a native-account rent refund that is not represented by the approved effects.",
            "Keep the operation unresolved until the rent refund is explicitly accounted for.",
        ));
    }
    let rent = u64::try_from(rent).map_err(|_| bad_response())?;
    if rent > cap {
        return Err(GoFinError::new(
            "JLP_ACCOUNT_FUNDING_LIMIT",
            "reconciliation",
            "Finalized SOL account funding exceeded the approved cap.",
            "Keep the operation unresolved and inspect finalized native-balance effects.",
        ));
    }
    Ok(rent)
}

pub fn operation_definitions() -> Vec<Definition> {
    let mut definitions: Vec<_> = [OP_INFO, OP_MINT, OP_BURN]
        .into_iter()
        .map(|operation| Definition {
            name: operation.into(),
            description: match operation {
                OP_INFO => "Read Jupiter Perps JLP pool and custody information.".into(),
                OP_MINT => "Mint JLP from one supported custody asset using Jupiter Perps.".into(),
                _ => "Redeem JLP for one supported custody asset using Jupiter Perps.".into(),
            },
            kind: if operation == OP_INFO { OperationKind::Read } else { OperationKind::Write },
            execution_actor: if operation == OP_INFO { ExecutionActor::AgentReadable } else { ExecutionActor::AgentInvocable },
            input_schema: input_schema(operation),
            output_schema: output_schema(operation),
            error_schema: json!({"type":"object","required":["code","stage","message","next_action"]}),
            examples: vec![if operation == OP_INFO {
                json!({"network":"solana","cluster":CLUSTER})
            } else if operation == OP_MINT {
                json!({"network":"solana","cluster":CLUSTER,"wallet":"wallet","deposit_token":"USDC","amount_atoms":"1000000","minimum_jlp_atoms":"100000","maximum_network_fee_lamports":"10000","maximum_account_funding_lamports":"0"})
            } else {
                json!({"network":"solana","cluster":CLUSTER,"wallet":"wallet","receive_token":"USDC","jlp_atoms":"1000000","minimum_receive_atoms":"1000000","maximum_network_fee_lamports":"10000","maximum_account_funding_lamports":"0"})
            }],
            coverage: "Jupiter Perps API transaction proposal, pinned Anchor IDL instruction validation, Solana simulation, exact account/mint/program/decimal receipt checks, and finalized settlement. Mint/redeem support finite amount, output-rate, fee, funding and expiry boundaries.".into(),
            completion: if operation == OP_INFO { "Provider pool metadata only; it is not a wallet balance or complete exposure snapshot.".into() } else { "Completed only when finalized chain proof verifies the exact JLP conversion and bounded fee.".into() },
            semantics: json!({"network":NETWORK,"cluster":CLUSTER,"program":PERPS_PROGRAM_ID,"jlp_mint":JLP_MINT,"accepted_custodies":["WBTC","ETH","SOL","USDC"],"provider_success_is_settlement":false,"ambiguous_submission":"reconcile original durable transaction; never replay","fee_units":"maximum_network_fee_lamports is exact lamports","rent_units":"maximum_account_funding_lamports is refundable owned account funding, not a network fee","plan_permissions":[OP_MINT,OP_BURN]}),
        })
        .collect();
    for definition in &mut definitions {
        if definition.kind == OperationKind::Write {
            definition.semantics["plan_allowance_seconds"] =
                json!(gofin_domain::plan_timing::ONCHAIN_ALLOWANCE_SECONDS);
        }
    }
    definitions
}

fn output_schema(operation: &str) -> Value {
    if operation == OP_INFO {
        return json!({"type":"object","required":["custodies"],"properties":{"custodies":{"type":"array","items":{"type":"object","required":["symbol","mint"],"properties":{"symbol":{"type":"string"},"mint":{"type":"string","description":"Case-sensitive Solana mint address."},"decimals":{"type":"integer","minimum":0,"maximum":18}},"additionalProperties":true}},"slot":{"type":"integer","minimum":0},"cluster":{"const":"mainnet-beta"}},"additionalProperties":true});
    }
    json!({"type":"object","required":["status"],"properties":{"status":{"enum":["prepared","submitted","pending","completed","failed","unknown"]},"id":{"type":"string"},"request_id":{"type":"string"},"operation":{"const":operation},"submission":{"type":["object","null"],"properties":{"kind":{"type":"string"},"reference":{"type":"string"}},"additionalProperties":true},"effects":{"type":["object","null"],"properties":{"proof_digest":{"type":"string"},"effects":{"type":"array","items":{"type":"object"}},"evidence":{"type":"object"}},"additionalProperties":true},"error":{"type":["object","null"],"required":["code","stage","message","next_action"],"properties":{"code":{"type":"string"},"stage":{"type":"string"},"message":{"type":"string"},"next_action":{"type":"string"}}}},"additionalProperties":true})
}

fn input_schema(operation: &str) -> Value {
    let mut properties = json!({"network":{"const":"solana"},"cluster":{"const":CLUSTER}});
    let mut required = vec!["network", "cluster"];
    if operation != OP_INFO {
        properties["wallet"] = json!({"type":"string","minLength":1});
        properties["maximum_network_fee_lamports"] =
            json!({"type":"string","pattern":"^[0-9]{1,20}$"});
        properties["maximum_account_funding_lamports"] =
            json!({"type":"string","pattern":"^[0-9]{1,20}$"});
        required.extend([
            "wallet",
            "maximum_network_fee_lamports",
            "maximum_account_funding_lamports",
        ]);
    }
    if operation == OP_MINT {
        properties["deposit_token"] = json!({"enum":["WBTC","ETH","SOL","USDC"]});
        properties["amount_atoms"] = json!({"type":"string","pattern":"^[0-9]{1,20}$"});
        properties["minimum_jlp_atoms"] = json!({"type":"string","pattern":"^[0-9]{1,20}$"});
        required.extend(["deposit_token", "amount_atoms", "minimum_jlp_atoms"]);
    }
    if operation == OP_BURN {
        properties["receive_token"] = json!({"enum":["WBTC","ETH","SOL","USDC"]});
        properties["jlp_atoms"] = json!({"type":"string","pattern":"^[0-9]{1,20}$"});
        properties["minimum_receive_atoms"] = json!({"type":"string","pattern":"^[0-9]{1,20}$"});
        required.extend(["receive_token", "jlp_atoms", "minimum_receive_atoms"]);
    }
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

fn asset(mint: &str, decimals: u8) -> AssetId {
    AssetId {
        network: NETWORK.into(),
        reference: if mint == WRAPPED_SOL_MINT {
            "native".into()
        } else {
            mint.into()
        },
        decimals,
    }
}
fn required_text<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| GoFinError::validation(format!("{key} is required.")))
}
fn required_atoms(input: &Value, key: &str) -> Result<u64> {
    let text = required_text(input, key)?;
    if text.len() > 20 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(GoFinError::validation(format!(
            "{key} must be an exact unsigned atomic quantity."
        )));
    }
    text.parse()
        .map_err(|_| GoFinError::validation(format!("{key} exceeds the supported amount range.")))
}
fn required_fee(input: &Value) -> Result<u64> {
    required_atoms(input, "maximum_network_fee_lamports")
}
fn optional_atoms(input: &Value, key: &str) -> Result<u64> {
    if input.get(key).is_none() {
        return Ok(0);
    }
    required_atoms(input, key)
}
fn address_field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    let text = value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(bad_response)?;
    gofin_solana::parse_address(text)?;
    Ok(text)
}
fn check(name: &str) -> Check {
    Check {
        name: name.into(),
        status: CheckStatus::Passed,
        detail: "Verified against the frozen transaction and mainnet-beta runtime.".into(),
    }
}
fn unknown() -> GoFinError {
    GoFinError::new(
        "JUPITER_OPERATION_UNKNOWN",
        "operation",
        "This JLP operation is not implemented by the selected adapter.",
        "Inspect GoFin discovery for supported Jupiter operations.",
    )
}
fn bad_response() -> GoFinError {
    GoFinError::new(
        "JLP_RESPONSE_INVALID",
        "prepare",
        "Jupiter returned incomplete or invalid JLP transaction data.",
        "Request a fresh JLP operation and review its exact transaction before approval.",
    )
}
fn unreviewed() -> GoFinError {
    GoFinError::new(
        "JLP_INSTRUCTION_UNREVIEWED",
        "prepare",
        "The JLP transaction contains an instruction or account effect outside the pinned IDL envelope.",
        "Do not submit this transaction; wait for a reviewed Jupiter Perps deployment profile.",
    )
}
fn unreviewed_reason(reason: &'static str) -> GoFinError {
    GoFinError::new(
        "JLP_INSTRUCTION_UNREVIEWED",
        "prepare",
        format!("The JLP transaction failed the reviewed {reason} check."),
        "Do not submit this transaction; wait for a reviewed Jupiter Perpetuals deployment profile.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_quantity_formatting_is_decimal_exact_for_assets_and_sol() {
        assert_eq!(format_token_quantity(11_000_000, 6), "11");
        assert_eq!(format_token_quantity(110_000_000, 9), "0.11");
        assert_eq!(format_sol_lamports(10_000), "0.00001 SOL");
        assert_eq!(
            format_output_rate("900000", "1000000", 6, 6).unwrap(),
            "0.9"
        );
        assert_eq!(
            format_output_rate("1000000", "1000000000", 9, 6).unwrap(),
            "1"
        );
    }
    use gofin_domain::{PositionKind, Quantity};

    fn balance_row(
        index: usize,
        mint: &str,
        owner: &str,
        program: &str,
        decimals: u8,
        atoms: &str,
    ) -> Value {
        json!({
            "accountIndex":index,
            "mint":mint,
            "owner":owner,
            "programId":program,
            "uiTokenAmount":{"amount":atoms,"decimals":decimals}
        })
    }

    // Keep proof fields explicit so each synthetic receipt fixture documents
    // its independent wire evidence without hiding values in a generic builder.
    #[allow(clippy::too_many_arguments)]
    fn receipt_proof(
        wallet: &str,
        source_account: &str,
        destination_account: &str,
        pre_lamports: [u64; 3],
        post_lamports: [u64; 3],
        pre_tokens: Vec<Value>,
        post_tokens: Vec<Value>,
        fee: u64,
    ) -> FinalizedReceiptProof {
        FinalizedReceiptProof {
            signature: "SYNTHETIC-JLP-SIGNATURE".into(),
            slot: 77,
            fee_lamports: fee,
            failed: false,
            message_sha256: "a".repeat(64),
            account_keys: vec![
                wallet.into(),
                source_account.into(),
                destination_account.into(),
            ],
            message_instructions: vec![],
            required_signatures: vec!["SYNTHETIC-ENCODED-SIGNATURE".into()],
            pre_balances_lamports: pre_lamports.to_vec(),
            post_balances_lamports: post_lamports.to_vec(),
            pre_token_balances: pre_tokens,
            post_token_balances: post_tokens,
            loaded_addresses: json!({"writable":[],"readonly":[]}),
            inner_instructions: vec![],
            log_messages: vec![],
            error: None,
        }
    }

    // This test constructor mirrors the complete adapter record shape under
    // test; a context object would make the fixture's proof inputs less clear.
    #[allow(clippy::too_many_arguments)]
    fn finalized_record(
        operation: &str,
        wallet: &str,
        source_mint: &str,
        source_decimals: u8,
        source_account: &str,
        input_atoms: u64,
        destination_mint: &str,
        destination_decimals: u8,
        destination_account: &str,
        minimum_output: u64,
        wrapped_native_lamports: u64,
        closed_accounts: Vec<String>,
        closed_refunds: Vec<crate::adapter::AccountFundingRequirement>,
    ) -> OperationRecord {
        let resource = ResourceId(format!("solana:mainnet-beta:{wallet}"));
        let source_asset = asset(source_mint, source_decimals);
        let destination_asset = asset(destination_mint, destination_decimals);
        let mut expected = vec![EffectConstraint::Conversion {
            resource: resource.clone(),
            input: source_asset.clone(),
            maximum_input: Quantity::from_u64(input_atoms),
            output: destination_asset.clone(),
            minimum_output: Quantity::from_u64(minimum_output),
        }];
        for refund in &closed_refunds {
            let position = format!("account-funding:{}", refund.address);
            let amount = Quantity::from_u64(refund.lamports);
            expected.extend([
                EffectConstraint::PositionOfKind {
                    resource: resource.clone(),
                    position: position.clone(),
                    asset: asset("native", 9),
                    position_kind: PositionKind::RefundableAccountFunding,
                    direction: gofin_domain::Direction::Decrease,
                    maximum_change: amount.clone(),
                },
                EffectConstraint::OriginMovement {
                    from: AccountingLocation::Position {
                        resource: resource.clone(),
                        position,
                        position_kind: PositionKind::RefundableAccountFunding,
                    },
                    to: AccountingLocation::Cash {
                        resource: resource.clone(),
                    },
                    debit_asset: asset("native", 9),
                    credit_asset: asset("native", 9),
                    maximum_debit: amount.clone(),
                    minimum_credit: amount.clone(),
                },
                EffectConstraint::Credit {
                    to: resource.clone(),
                    asset: asset("native", 9),
                    minimum_credit: amount,
                },
            ]);
        }
        let binding = json!({
            "wallet_address":wallet,
            "fee_payer":wallet,
            "source_mint":source_mint,
            "destination_mint":destination_mint,
            "source_token_account":source_account,
            "destination_token_account":destination_account,
            "input_token_program":gofin_solana::token::TOKEN_PROGRAM_ID,
            "output_token_program":gofin_solana::token::TOKEN_PROGRAM_ID,
            "source_decimals":source_decimals,
            "destination_decimals":destination_decimals,
            "input_atoms":input_atoms.to_string(),
            "minimum_output_atoms":minimum_output.to_string(),
            "maximum_account_funding_lamports":"0",
            "maximum_network_fee_lamports":"10000",
            "wrapped_native_lamports":wrapped_native_lamports,
            "closed_wrapped_native_accounts":closed_accounts,
            "closed_wrapped_native_refunds":closed_refunds,
            "funding_accounts":[],
        });
        OperationRecord {
            id: "synthetic-jlp-operation".into(),
            request_id: "synthetic-jlp-request".into(),
            fingerprint: "synthetic-jlp-fingerprint".into(),
            fingerprint_version: 1,
            operation: operation.into(),
            input: json!({"wallet":"synthetic"}),
            status: Status::Submitted,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
            preview: Some(PreparedOperation {
                operation: operation.into(),
                input: json!({"wallet":"synthetic"}),
                binding,
                summary: "synthetic finalized JLP conversion".into(),
                checks: vec![],
                warnings: vec![],
                observed_at: Utc::now().to_rfc3339(),
                expires_at: (Utc::now() + ChronoDuration::seconds(20)).to_rfc3339(),
                valuation: None,
                intent: FinancialIntent {
                    debits: vec![ResourceAmount {
                        resource: resource.clone(),
                        amount: Amount {
                            asset: source_asset,
                            atoms: Quantity::from_u64(input_atoms),
                        },
                    }],
                    maximum_costs: vec![CostBound {
                        location: AccountingLocation::Cash {
                            resource: resource.clone(),
                        },
                        amount: Amount {
                            asset: asset("native", 9),
                            atoms: Quantity::from_u64(10_000),
                        },
                        treatment: CostTreatment::AdditionalDebit,
                    }],
                    expected,
                    locks: vec![resource],
                },
                action_deadline: None,
                operation_version: 1,
            }),
            approval: None,
            authorization_source: None,
            submission: Some(Submission {
                kind: "solana".into(),
                reference: "SYNTHETIC-JLP-SIGNATURE".into(),
                data: json!({"fee_payer":wallet}),
            }),
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        }
    }

    fn token_balance_pair(
        index: usize,
        mint: &str,
        decimals: u8,
        before: &str,
        after: &str,
    ) -> (Vec<Value>, Vec<Value>) {
        let program = gofin_solana::token::TOKEN_PROGRAM_ID;
        (
            vec![balance_row(
                index,
                mint,
                "SYNTHETIC-WALLET",
                program,
                decimals,
                before,
            )],
            vec![balance_row(
                index,
                mint,
                "SYNTHETIC-WALLET",
                program,
                decimals,
                after,
            )],
        )
    }

    #[test]
    fn finalized_usdc_mint_and_redeem_prove_exact_token_deltas() {
        let wallet = "SYNTHETIC-WALLET";
        let usdc = "SYNTHETIC-USDC-MINT";
        let source_account = "SYNTHETIC-USDC-ATA";
        let destination_account = "SYNTHETIC-JLP-ATA";
        let fee = 5_000;

        let (mut pre, mut post) = token_balance_pair(1, usdc, 6, "1000000", "0");
        let (jlp_pre, jlp_post) = token_balance_pair(2, JLP_MINT, 6, "0", "1100000");
        pre.extend(jlp_pre);
        post.extend(jlp_post);
        let proof = receipt_proof(
            wallet,
            source_account,
            destination_account,
            [10_000_000, 1, 1],
            [10_000_000 - fee, 1, 1],
            pre,
            post,
            fee,
        );
        let mint_record = finalized_record(
            OP_MINT,
            wallet,
            usdc,
            6,
            source_account,
            1_000_000,
            JLP_MINT,
            6,
            destination_account,
            1_000_000,
            0,
            vec![],
            vec![],
        );
        let mint = verify_finalized(
            &mint_record,
            mint_record.submission.as_ref().unwrap(),
            "SYNTHETIC-JLP-SIGNATURE",
            77,
            fee,
            &proof,
        )
        .unwrap();
        assert!(matches!(
            &mint.effects[0],
            FinancialEffect::Conversion { spent, received, .. }
                if spent.atoms.to_string() == "1000000"
                    && received.atoms.to_string() == "1100000"
                    && received.asset.reference == JLP_MINT
        ));

        let (mut pre, mut post) = token_balance_pair(1, JLP_MINT, 6, "1100000", "0");
        let (usdc_pre, usdc_post) = token_balance_pair(2, usdc, 6, "0", "1000000");
        pre.extend(usdc_pre);
        post.extend(usdc_post);
        let proof = receipt_proof(
            wallet,
            destination_account,
            source_account,
            [10_000_000, 1, 1],
            [10_000_000 - fee, 1, 1],
            pre,
            post,
            fee,
        );
        let burn_record = finalized_record(
            OP_BURN,
            wallet,
            JLP_MINT,
            6,
            destination_account,
            1_100_000,
            usdc,
            6,
            source_account,
            1_000_000,
            0,
            vec![],
            vec![],
        );
        let burn = verify_finalized(
            &burn_record,
            burn_record.submission.as_ref().unwrap(),
            "SYNTHETIC-JLP-SIGNATURE",
            78,
            fee,
            &proof,
        )
        .unwrap();
        assert!(matches!(
            &burn.effects[0],
            FinancialEffect::Conversion { spent, received, .. }
                if spent.atoms.to_string() == "1100000"
                    && received.atoms.to_string() == "1000000"
                    && received.asset.reference == usdc
        ));

        let mut tampered = proof.clone();
        tampered.post_token_balances[1]["uiTokenAmount"]["decimals"] = json!(9);
        assert!(
            verify_finalized(
                &burn_record,
                burn_record.submission.as_ref().unwrap(),
                "SYNTHETIC-JLP-SIGNATURE",
                78,
                fee,
                &tampered,
            )
            .is_err()
        );
    }

    #[test]
    fn finalized_native_mint_and_redeem_normalize_sol_and_close_rent_position() {
        let wallet = "SYNTHETIC-WALLET";
        let wsol_account = "SYNTHETIC-WSOL-ATA";
        let jlp_account = "SYNTHETIC-JLP-ATA";
        let fee = 5_000;
        let sol_atoms = 2_000_000_000u64;
        let (mut pre, mut post) = token_balance_pair(1, WRAPPED_SOL_MINT, 9, "0", "0");
        let (jlp_pre, jlp_post) = token_balance_pair(2, JLP_MINT, 6, "0", "1000000");
        pre.extend(jlp_pre);
        post.extend(jlp_post);
        let proof = receipt_proof(
            wallet,
            wsol_account,
            jlp_account,
            [10_000_000_000, 1, 1],
            [10_000_000_000 - sol_atoms - fee, 1, 1],
            pre,
            post,
            fee,
        );
        let mint_record = finalized_record(
            OP_MINT,
            wallet,
            WRAPPED_SOL_MINT,
            9,
            wsol_account,
            sol_atoms,
            JLP_MINT,
            6,
            jlp_account,
            900_000,
            sol_atoms,
            vec![],
            vec![],
        );
        let mint = verify_finalized(
            &mint_record,
            mint_record.submission.as_ref().unwrap(),
            "SYNTHETIC-JLP-SIGNATURE",
            79,
            fee,
            &proof,
        )
        .unwrap();
        assert!(matches!(
            &mint.effects[0],
            FinancialEffect::Conversion { spent, received, .. }
                if spent.asset.reference == "native"
                    && spent.atoms.to_string() == sol_atoms.to_string()
                    && received.asset.reference == JLP_MINT
        ));

        let rent = 2_039_280u64;
        let (mut pre, mut post) = token_balance_pair(1, JLP_MINT, 6, "1000000", "0");
        let proof = receipt_proof(
            wallet,
            jlp_account,
            wsol_account,
            [10_000_000_000, 1, rent],
            [10_000_000_000 + sol_atoms + rent - fee, 1, 0],
            std::mem::take(&mut pre),
            std::mem::take(&mut post),
            fee,
        );
        let closed_refund = crate::adapter::AccountFundingRequirement {
            address: wsol_account.into(),
            lamports: rent,
            account_owner: gofin_solana::token::TOKEN_PROGRAM_ID.into(),
            refund_authority: wallet.into(),
            payer: wallet.into(),
        };
        let burn_record = finalized_record(
            OP_BURN,
            wallet,
            JLP_MINT,
            6,
            jlp_account,
            1_000_000,
            WRAPPED_SOL_MINT,
            9,
            wsol_account,
            1_900_000_000,
            0,
            vec![wsol_account.into()],
            vec![closed_refund],
        );
        let burn = verify_finalized(
            &burn_record,
            burn_record.submission.as_ref().unwrap(),
            "SYNTHETIC-JLP-SIGNATURE",
            80,
            fee,
            &proof,
        )
        .unwrap();
        assert!(matches!(
            &burn.effects[0],
            FinancialEffect::Conversion { spent, received, .. }
                if spent.atoms.to_string() == "1000000"
                    && received.asset.reference == "native"
                    && received.atoms.to_string() == sol_atoms.to_string()
        ));
        let rent_change = burn
            .effects
            .iter()
            .find_map(|effect| match effect {
                FinancialEffect::PositionChange {
                    delta,
                    anchor: Some(anchor),
                    ..
                } if delta.is_negative() => Some((delta, anchor)),
                _ => None,
            })
            .expect("closed account rent decrease");
        assert_eq!(rent_change.0.magnitude().to_string(), rent.to_string());
        assert_eq!(rent_change.1.principal.atoms.to_string(), "0");
        assert!(burn.effects.iter().any(|effect| matches!(
            effect,
            FinancialEffect::OriginMovement {
                from: AccountingLocation::Position { position_kind: PositionKind::RefundableAccountFunding, .. },
                to: AccountingLocation::Cash { .. },
                credit,
                ..
            } if credit.atoms.to_string() == rent.to_string()
        )));
    }
    #[test]
    fn global_privileges_accept_a_writable_fee_payer_without_overgranting_core_accounts() {
        assert!(valid_global_privilege(0, true, true, false));
        assert!(valid_global_privilege(1, false, true, false));
        assert!(valid_global_privilege(3, false, true, true));
        assert!(!valid_global_privilege(3, false, true, false));
        assert!(!valid_global_privilege(0, true, false, false));
        assert!(!valid_global_privilege(2, true, true, false));
    }

    #[test]
    fn decodes_pinned_idl_liquidity_arguments_without_rounding() {
        let mut add = Vec::new();
        add.extend(10u64.to_le_bytes());
        add.extend(9u64.to_le_bytes());
        add.push(0);
        assert_eq!(decode_args("add_liquidity2", &add).unwrap(), (10, 9));
        let mut burn = Vec::new();
        burn.extend(10u64.to_le_bytes());
        burn.extend(8u64.to_le_bytes());
        assert_eq!(decode_args("remove_liquidity2", &burn).unwrap(), (10, 8));
        assert!(decode_args("add_liquidity2", &[0; 24]).is_err());
        add[16] = 1;
        assert!(decode_args("add_liquidity2", &add).is_err());
    }
    #[test]
    fn pins_protocol_and_rejects_unreviewed_custodies() {
        assert_eq!(
            gofin_solana::parse_address(PERPS_PROGRAM_ID).unwrap(),
            PERPS_PROGRAM_ID
        );
        assert_eq!(gofin_solana::parse_address(JLP_MINT).unwrap(), JLP_MINT);
        assert!(
            input_schema(OP_MINT)["required"]
                .as_array()
                .unwrap()
                .contains(&json!("minimum_jlp_atoms"))
        );
        assert_eq!(
            &anchor_discriminator("add_liquidity2")[..],
            &[228, 162, 78, 28, 70, 219, 116, 115]
        );
    }

    #[test]
    fn derives_exact_net_rent_from_final_wallet_balance_for_wrapped_sol_lifecycle() {
        // Deposit: 2 SOL principal, fee and temporary ATA rent are all debits.
        assert_eq!(
            net_account_funding(-2_000_000_000, 5_000, -2_000_203_928, 250_000).unwrap(),
            198_928
        );
        // Redemption: SOL credit minus fee and net ATA rent.
        assert_eq!(
            net_account_funding(1_500_000_000, 5_000, 1_499_796_072, 250_000).unwrap(),
            198_928
        );
        // An unexpected rent refund or over-cap creation cannot settle.
        assert_eq!(
            net_account_funding(0, 5_000, -4_999, 250_000)
                .unwrap_err()
                .code,
            "JLP_RENT_REFUND_UNMODELED"
        );
        assert_eq!(
            net_account_funding(0, 5_000, -255_001, 250_000)
                .unwrap_err()
                .code,
            "JLP_ACCOUNT_FUNDING_LIMIT"
        );
    }
}
