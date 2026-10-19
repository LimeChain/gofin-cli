//! Broker resolution of exact Solana identities for Relay. Provider labels are
//! never allowed to define a mint, its precision, or a recipient.
use super::*;
use gofin_bridge::relay::solana::{ExpectedSourceTransaction, ValidatedSourceTransaction};
use gofin_bridge::{SolanaAsset, SolanaAssetInput, SolanaDestination, SolanaDestinationInput};
use gofin_solana::{FrozenTransaction as SolanaFrozenTransaction, ReceiptOutcome};

// Reviewed Relay Chain mainnet deployments from relay-settlement's
// smart-contracts/deployments/contracts/prod.json (chain 537713).
const RELAY_EXECUTOR: &str = "0x6747c7e64b90dccc0d54f865cbd9513156adbc0c";
const RELAY_ALLOCATOR: &str = "0x613d3c588f6b8f89302b463f8f19f7241b2857e2";
const RELAY_HUB: &str = "0xddd361727c22a01eb137880678a20b0beae69318";
const RELAY_ORACLE: &str = "0xd180dc3b8cb71b185c563b6e4857592ca93dacaf";
// Relay's current Solana solver signer, reviewed against the destination fill
// and Relay's public relay-kit transaction examples. A rotation fails closed.
const RELAY_SOLANA_SOLVER: &str = "F7p3dFrjRTbtRp8FRF6qHLomXbKRBzpvBLjtQcfcgmNe";

enum RelaySolanaWitness {
    Depository(gofin_bridge::relay::solana::VerifiedHubPayoutWitness),
    Solver(gofin_bridge::relay::solana::VerifiedHubOrderSettlement),
}

fn relay_solana_refund_targets(
    paths: &[gofin_bridge::RefundPath],
    refund_to: &str,
    scope: &gofin_domain::BridgeSettlementExpectation,
) -> Vec<(String, gofin_domain::AssetId, Quantity)> {
    paths
        .iter()
        .filter_map(|path| {
            if path.chain_id != 792703809 || path.recipient != refund_to {
                return None;
            }
            let target = scope.refund.iter().find(|target| {
                target.asset == path.asset
                    && target.resource
                        == gofin_domain::ResourceId(format!(
                            "{}:{}",
                            gofin_solana::NETWORK,
                            path.recipient
                        ))
            })?;
            Some((
                path.recipient.clone(),
                target.asset.clone(),
                target.minimum_atoms.clone(),
            ))
        })
        .collect()
}

fn verify_failed_relay_source_receipt(
    proof: &gofin_solana::FinalizedReceiptProof,
    expected: &ExpectedSourceTransaction,
    validated: &ValidatedSourceTransaction,
) -> gofin_domain::Result<()> {
    if proof.lamport_delta(&expected.wallet)?
        != gofin_domain::SignedQuantity::negative(&Quantity::from_u64(proof.fee_lamports))
        || !proof.lamport_delta(&validated.vault)?.is_zero()
        || proof.pre_token_balances != proof.post_token_balances
    {
        return Err(GoFinError::validation(
            "Failed Relay Solana deposit changed principal, vault balance, or account funding.",
        ));
    }
    Ok(())
}

impl BridgeAdapter {
    pub(super) async fn relay_solana_asset(
        &self,
        input: &SolanaAssetInput,
    ) -> gofin_domain::Result<SolanaAsset> {
        if input.network != gofin_solana::NETWORK {
            return Err(GoFinError::validation(
                "Relay Solana requires mainnet-beta.",
            ));
        }
        let asset = match input.mint.as_deref() {
            None => {
                gofin_solana::funding::resolve_external_asset(&self.solana_observer, "SOL", None)
                    .await?
            }
            Some(mint) => {
                if mint == "native" || mint.is_empty() {
                    return Err(GoFinError::validation(
                        "Use mint: null for native SOL and an exact base58 address for tokens.",
                    ));
                }
                let asset = gofin_solana::funding::resolve_external_asset(
                    &self.solana_observer,
                    "TOKEN",
                    Some(mint),
                )
                .await?;
                let verified =
                    gofin_bridge::relay::solana::resolve_relay_mint(&self.solana_observer, mint)
                        .await?;
                if verified.decimals != asset.decimals {
                    return Err(GoFinError::validation(
                        "Solana mint precision changed during bridge resolution.",
                    ));
                }
                asset
            }
        };
        Ok(SolanaAsset {
            network: asset.network,
            mint: (asset.reference != "native").then_some(asset.reference),
            decimals: asset.decimals,
        })
    }

    pub(super) async fn resolve_relay_solana_destination(
        &self,
        input: &SolanaDestinationInput,
        recipient: &str,
    ) -> gofin_domain::Result<SolanaDestination> {
        gofin_solana::parse_address(recipient)?;
        let asset = self
            .relay_solana_asset(&SolanaAssetInput {
                network: input.network.clone(),
                mint: input.mint.clone(),
            })
            .await?
            .asset_id();
        if let Some(alias) = input.wallet.as_deref() {
            let wallet = self.solana_wallets.get(alias).ok_or_else(|| {
                GoFinError::validation("The selected Solana destination wallet is not enrolled.")
            })?;
            if wallet.address() != recipient {
                return Err(GoFinError::validation(
                    "The selected destination wallet does not match the exact Solana recipient.",
                ));
            }
        }
        Ok(SolanaDestination {
            network: input.network.clone(),
            wallet: input.wallet.clone(),
            wallet_resource: gofin_domain::ResourceId(format!("{}:{}", input.network, recipient)),
            wallet_address: recipient.into(),
            recipient: recipient.into(),
            asset,
            recipient_token_account: String::new(),
            destination_caller: String::new(),
        })
    }

    pub(super) async fn relay_solana_routes(
        &self,
        input: &gofin_bridge::BridgeRoutesInput,
    ) -> gofin_domain::Result<Value> {
        if input.source.is_some() == input.solana_source.is_some() {
            return Err(GoFinError::validation(
                "Select exactly one EVM or Solana bridge source.",
            ));
        }
        if input.solana_source.is_some() == input.solana_destination.is_some() {
            return Err(GoFinError::validation(
                "A Relay Solana route must connect Solana and a distinct reviewed EVM network.",
            ));
        }
        let source = if let Some(source) = &input.source {
            Some(self.asset(source, source.chain_id).await?)
        } else {
            None
        };
        let solana_source = if let Some(source) = &input.solana_source {
            Some(self.relay_solana_asset(source).await?)
        } else {
            None
        };
        let destination_chain_id = match input.destination_chain_id {
            Some(chain) if matches!(chain, 1 | 42161 | 4663 | 8453) => chain,
            None if input.solana_destination.is_some() => {
                gofin_bridge::relay::RELAY_SOLANA_CHAIN_ID
            }
            _ => {
                return Err(GoFinError::validation(
                    "Select one reviewed EVM destination or Solana mainnet-beta.",
                ));
            }
        };
        if source.is_some() && destination_chain_id != gofin_bridge::relay::RELAY_SOLANA_CHAIN_ID
            || solana_source.is_some()
                && destination_chain_id == gofin_bridge::relay::RELAY_SOLANA_CHAIN_ID
            || input.solana_destination.is_some() && input.destination_asset.is_some()
        {
            return Err(GoFinError::validation(
                "Relay route source and destination identities conflict.",
            ));
        }
        let solana_destination = if let Some(destination) = &input.solana_destination {
            self.relay_solana_asset(&SolanaAssetInput {
                network: destination.network.clone(),
                mint: destination.mint.clone(),
            })
            .await?;
            Some(destination.clone())
        } else {
            None
        };
        let destination_asset = if let Some(asset) = &input.destination_asset {
            Some(self.asset(asset, destination_chain_id).await?)
        } else {
            None
        };
        let request = gofin_bridge::BridgeRouteRequest {
            source: source.clone(),
            solana_source: solana_source.clone(),
            destination_chain_id,
            solana_destination: solana_destination.clone(),
            destination_asset: destination_asset.clone(),
        };
        let provider = self.provider("relay")?;
        let batch = provider.discover_routes(&request).await?;
        let mut routes = Vec::new();
        for candidate in batch.candidates {
            if let Some(destination) = candidate.solana_destination {
                if solana_destination.as_ref().is_some_and(|wanted| {
                    wanted.network == destination.network && wanted.mint == destination.mint
                }) {
                    routes.push(json!({"provider":"relay","availability":"candidate","indicative":true,"solana_destination":destination,"identity_verified":true}));
                }
            } else if let Some(destination) = candidate.destination {
                let asset = self.asset(&destination, destination_chain_id).await?;
                if destination_asset
                    .as_ref()
                    .is_none_or(|wanted| wanted == &asset)
                    && candidate
                        .reported_decimals
                        .is_none_or(|decimals| decimals == asset.decimals)
                {
                    routes.push(json!({"provider":"relay","availability":"candidate","indicative":true,"destination_asset":asset,"identity_verified":true}));
                }
            }
        }
        let mut providers = vec!["relay"];
        if input.source.is_some()
            && solana_destination
                .as_ref()
                .and_then(|asset| asset.mint.as_deref())
                == Some(gofin_solana::funding::MAINNET_USDC_MINT)
            && let Ok(cctp) = self.solana_routes(input).await
            && let Some(candidates) = cctp["routes"].as_array()
        {
            routes.extend(candidates.iter().cloned());
            providers.push("cctp");
        }
        Ok(
            json!({"indicative":true,"source":source,"solana_source":solana_source,"destination_chain_id":input.destination_chain_id,"solana_destination":solana_destination,"coverage":{"providers":providers,"market_wide_comparison":false,"complete":batch.complete},"routes":routes,"limitations":batch.limitations,"error":batch.error,"next_action":"Request bridge.quotes with exact wallet, amount, recipient, refund and finite bounds."}),
        )
    }
}

impl BridgeAdapter {
    pub(super) async fn prevalidate_relay_solana_quote(
        &self,
        quote: &BridgeQuote,
        bounds: &BridgeBounds,
        wallet_alias: &str,
    ) -> gofin_domain::Result<()> {
        self.relay_solana_quote_lifetime(quote)?;
        let wallet = self
            .solana_wallets
            .get(wallet_alias)
            .ok_or_else(|| GoFinError::validation("Relay Solana source wallet is not enrolled."))?;
        if wallet.address() != quote.intent.source_address || wallet.signer().is_none() {
            return Err(GoFinError::validation(
                "Relay Solana quote source differs from the selected signer.",
            ));
        }
        if quote.protocol_steps.len() != 1 {
            return Err(GoFinError::validation(
                "Relay Solana quote requires one direct source deposit.",
            ));
        }
        let step = &quote.protocol_steps[0];
        let tx = step
            .solana_transaction
            .as_ref()
            .ok_or_else(internal_error)?;
        let expected = self.relay_source_expectation(quote, step, bounds).await?;
        let runtime = wallet.runtime();
        let latest = runtime
            .rpc()
            .request("getLatestBlockhash", json!([{"commitment":"confirmed"}]))
            .await?;
        let blockhash = latest["value"]["blockhash"]
            .as_str()
            .ok_or_else(internal_error)?;
        let height = latest["value"]["lastValidBlockHeight"]
            .as_u64()
            .ok_or_else(internal_error)?;
        let signer = wallet.signer().ok_or_else(internal_error)?;
        let frozen = runtime
            .freeze_instructions_with_expiry(
                &tx.instructions,
                &tx.lookup_tables,
                blockhash,
                &signer,
                Some(height),
                tx.expire_at.as_deref(),
                tx.max_network_fee_lamports,
                quote.provider_request_id.clone(),
                &[],
            )
            .await?;
        gofin_bridge::relay::solana::validate_source_transaction(&frozen, &expected)?;
        let fee = runtime.validate_fee(&frozen).await?;
        if fee > tx.max_network_fee_lamports {
            return Err(GoFinError::validation(
                "Relay Solana source fee exceeds the quote cap.",
            ));
        }
        Ok(())
    }

    fn relay_solana_quote_lifetime(&self, quote: &BridgeQuote) -> gofin_domain::Result<()> {
        let expires = chrono::DateTime::parse_from_rfc3339(&quote.valid_until)
            .map_err(|_| GoFinError::validation("Relay Solana quote expiration is invalid."))?;
        if expires <= self.clock.now() {
            return Err(GoFinError::validation(
                "Relay Solana quote expired before source preparation.",
            ));
        }
        Ok(())
    }

    fn relay_source_wallet(
        &self,
        record: &BridgeTransferRecord,
        quote: &BridgeQuote,
    ) -> gofin_domain::Result<&Arc<gofin_solana::SolanaWalletAdapter>> {
        let alias = record.metadata["wallet_alias"]
            .as_str()
            .ok_or_else(internal_error)?;
        let wallet = self.solana_wallets.get(alias).ok_or_else(|| {
            GoFinError::validation("The saved Relay Solana source wallet is unavailable.")
        })?;
        if wallet.address() != quote.intent.source_address
            || quote.intent.wallet
                != gofin_domain::ResourceId(format!(
                    "{}:{}",
                    gofin_solana::NETWORK,
                    wallet.address()
                ))
            || wallet.signer().is_none()
        {
            return Err(GoFinError::validation(
                "The Relay Solana source wallet differs from the saved transfer.",
            ));
        }
        Ok(wallet)
    }

    async fn relay_source_expectation(
        &self,
        quote: &BridgeQuote,
        step: &BridgeStep,
        bounds: &BridgeBounds,
    ) -> gofin_domain::Result<ExpectedSourceTransaction> {
        let tx = step
            .solana_transaction
            .as_ref()
            .ok_or_else(internal_error)?;
        let source = quote
            .intent
            .solana_source
            .as_ref()
            .ok_or_else(internal_error)?;
        if quote.provider != "relay"
            || quote.class != gofin_bridge::QuoteClass::Executable
            || step.kind != ProviderStepKind::Deposit
            || step.call.is_some()
            || step.solana_mint.is_some()
            || step.expected_sender != quote.intent.source_address
            || tx.source_address != quote.intent.source_address
            || quote.protocol_order_id.as_deref() != Some(tx.order_id.as_str())
            || quote.provider_request_id.as_deref() != Some(tx.request_id.as_str())
            || tx.mint != source.mint
            || tx.amount_atoms != quote.gross_debit.atoms
            || quote.gross_debit.asset != source.asset_id()
            || tx.transaction_base64.is_some()
        {
            return Err(GoFinError::validation(
                "Saved Relay Solana source step differs from the exact quote identity.",
            ));
        }
        let gas_cap = cctp_solana::sol_cap(bounds, gofin_bridge::FeeKind::Gas)?
            .as_str()
            .parse::<u64>()
            .map_err(|_| internal_error())?;
        let rent_cap = cctp_solana::sol_cap(bounds, gofin_bridge::FeeKind::AdditionalDebit)?
            .as_str()
            .parse::<u64>()
            .map_err(|_| internal_error())?;
        if tx.max_network_fee_lamports > gas_cap || tx.max_rent_lamports > rent_cap {
            return Err(GoFinError::validation(
                "Relay Solana fee or rent exceeds the approved cap.",
            ));
        }
        let (token_program, minimum_vault_credit_atoms) = if let Some(mint) = &source.mint {
            let info = gofin_bridge::relay::solana::resolve_relay_mint(&self.solana_observer, mint)
                .await?;
            if info.decimals != source.decimals {
                return Err(GoFinError::validation(
                    "Relay Solana mint precision changed before dispatch.",
                ));
            }
            let gross = tx
                .amount_atoms
                .as_str()
                .parse::<u64>()
                .map_err(|_| internal_error())?;
            let net = info
                .transfer_fee
                .as_ref()
                .map(|fee| fee.net_for_gross(gross))
                .unwrap_or(Some(gross))
                .ok_or_else(internal_error)?;
            (Some(info.program_id), Quantity::from_u64(net))
        } else {
            if source.decimals != 9 {
                return Err(GoFinError::validation("Native SOL precision must be nine."));
            }
            (None, tx.amount_atoms.clone())
        };
        Ok(ExpectedSourceTransaction {
            wallet: quote.intent.source_address.clone(),
            depositor: quote.intent.source_address.clone(),
            order_id: tx.order_id.clone(),
            mint: source.mint.clone(),
            token_program,
            decimals: source.decimals,
            amount_atoms: tx.amount_atoms.clone(),
            minimum_vault_credit_atoms,
            max_network_fee_lamports: tx.max_network_fee_lamports,
            max_rent_lamports: tx.max_rent_lamports,
        })
    }

    async fn freeze_relay_source(
        &self,
        prepared: &PreparedOperation,
    ) -> gofin_domain::Result<SolanaFrozenTransaction> {
        let quote: BridgeQuote = serde_json::from_value(prepared.binding["quote"].clone())
            .map_err(|_| internal_error())?;
        let record = self
            .state()?
            .transfer(
                prepared.binding["transfer_id"]
                    .as_str()
                    .ok_or_else(internal_error)?,
            )
            .map_err(store_error)?
            .ok_or_else(internal_error)?;
        let wallet = self.relay_source_wallet(&record, &quote)?;
        let signer = wallet.signer().ok_or_else(internal_error)?;
        let instructions: Vec<gofin_solana::NormalizedInstruction> =
            serde_json::from_value(prepared.binding["relay_solana_source_instructions"].clone())
                .map_err(|_| internal_error())?;
        let lookup_tables: Vec<String> =
            serde_json::from_value(prepared.binding["relay_solana_source_lookup_tables"].clone())
                .map_err(|_| internal_error())?;
        let frozen = wallet
            .runtime()
            .freeze_instructions_with_expiry(
                &instructions,
                &lookup_tables,
                prepared.binding["relay_solana_source_blockhash"]
                    .as_str()
                    .ok_or_else(internal_error)?,
                &signer,
                prepared.binding["relay_solana_source_last_valid_block_height"].as_u64(),
                prepared.binding["relay_solana_source_expire_at"].as_str(),
                prepared.binding["relay_solana_source_max_network_fee_lamports"]
                    .as_u64()
                    .ok_or_else(internal_error)?,
                quote.provider_request_id.clone(),
                &[],
            )
            .await?;
        if prepared.binding["relay_solana_source_message_sha256"].as_str()
            != Some(frozen.message_sha256())
        {
            return Err(GoFinError::validation(
                "Relay Solana message changed after approval.",
            ));
        }
        Ok(frozen)
    }

    pub(super) async fn prepare_relay_solana_source(
        &self,
        input: &Value,
        record: &BridgeTransferRecord,
        saved: &SavedQuote,
        step: BridgeStep,
        link: &BridgeStepLink,
        requested: Option<BridgeBounds>,
    ) -> gofin_domain::Result<PreparedOperation> {
        self.relay_solana_quote_lifetime(&saved.quote)?;
        let bounds = if let Some(approved) = &record.approved_bounds {
            let approved: BridgeBounds =
                serde_json::from_value(approved.clone()).map_err(|_| internal_error())?;
            if requested.is_some() && normalize_authorization_bounds(requested, saved)? != approved
            {
                return Err(GoFinError::validation(
                    "Relay Solana source must preserve its approved bounds.",
                ));
            }
            approved
        } else {
            normalize_authorization_bounds(requested, saved)?
        };
        let quote = &saved.quote;
        let tx = step
            .solana_transaction
            .as_ref()
            .ok_or_else(internal_error)?;
        let wallet = self.relay_source_wallet(record, quote)?;
        let expected = self.relay_source_expectation(quote, &step, &bounds).await?;
        if link.kind != BridgeStepKind::Deposit
            || tx.instructions.is_empty()
            || tx.lookup_tables.len() > 8
        {
            return Err(GoFinError::validation(
                "Relay Solana source has no supported direct deposit step.",
            ));
        }
        self.validate_quote_prices(&bounds, quote).await?;
        self.validate_selected_usd_asset_caps(
            &bounds,
            &quote.gross_debit.asset,
            &quote.intent.wallet,
        )
        .await?;
        self.validate_solana_stage_caps(
            record,
            &link.step_id,
            &bounds,
            tx.max_network_fee_lamports,
            tx.max_rent_lamports,
        )?;
        self.validate_frozen_gas_cap(
            &bounds,
            &cctp_solana::sol_asset(),
            &Quantity::from_u64(tx.max_network_fee_lamports),
        )
        .await?;
        let cost = CostBound {
            location: AccountingLocation::Cash {
                resource: quote.intent.wallet.clone(),
            },
            amount: GoFinAmount {
                asset: cctp_solana::sol_asset(),
                atoms: Quantity::from_u64(
                    tx.max_network_fee_lamports
                        .checked_add(tx.max_rent_lamports)
                        .ok_or_else(internal_error)?,
                ),
            },
            treatment: CostTreatment::AdditionalDebit,
        };
        let intent = FinancialIntent {
            debits: vec![ResourceAmount {
                resource: quote.intent.wallet.clone(),
                amount: quote.gross_debit.clone(),
            }],
            maximum_costs: vec![cost],
            expected: vec![EffectConstraint::Debit {
                resource: quote.intent.wallet.clone(),
                asset: quote.gross_debit.asset.clone(),
                maximum_debit: quote.gross_debit.atoms.clone(),
            }],
            locks: vec![quote.intent.wallet.clone()],
        };
        let valuation = self.value_intent(&intent).await?;
        let runtime = wallet.runtime();
        let latest = runtime
            .rpc()
            .request("getLatestBlockhash", json!([{"commitment":"confirmed"}]))
            .await?;
        let blockhash = latest["value"]["blockhash"]
            .as_str()
            .ok_or_else(internal_error)?;
        let height = latest["value"]["lastValidBlockHeight"]
            .as_u64()
            .ok_or_else(internal_error)?;
        let signer = wallet.signer().ok_or_else(internal_error)?;
        let frozen = runtime
            .freeze_instructions_with_expiry(
                &tx.instructions,
                &tx.lookup_tables,
                blockhash,
                &signer,
                Some(height),
                tx.expire_at.as_deref(),
                tx.max_network_fee_lamports,
                quote.provider_request_id.clone(),
                &[],
            )
            .await?;
        let validated =
            gofin_bridge::relay::solana::validate_source_transaction(&frozen, &expected)?;
        let actual_fee = runtime.validate_fee(&frozen).await?;
        if actual_fee > tx.max_network_fee_lamports {
            return Err(GoFinError::validation(
                "Relay Solana network fee exceeds the saved cap.",
            ));
        }
        runtime.simulate(&frozen).await?;
        let mut plan = saved.plan.clone();
        plan.bounds = bounds.clone();
        let alias = record.metadata["wallet_alias"]
            .as_str()
            .ok_or_else(internal_error)?;
        let mut prepared = PreparedOperation {
            action_deadline: None, operation: "bridge.transfer".into(), input: input.clone(),
            binding: json!({"resource":quote.intent.wallet,"transfer_id":record.id,"step_id":link.step_id,"step_kind":link.kind,"step_dependencies":link.depends_on,"grant_id":record.grant_id,"quote_id":quote.quote_id,"route_id":quote.route_id,"provider_request_id":quote.provider_request_id,"protocol_order_id":quote.protocol_order_id,"quote":quote,"intent":quote.intent,"plan_digest":plan.fixed_scope_digest,"plan":plan,"step_ids":saved.step_ids,"step":step,"bounds":bounds,"relay_solana_source_instructions":tx.instructions,"relay_solana_source_lookup_tables":tx.lookup_tables,"relay_solana_source_blockhash":blockhash,"relay_solana_source_last_valid_block_height":height,"relay_solana_source_expire_at":tx.expire_at,"relay_solana_source_max_network_fee_lamports":tx.max_network_fee_lamports,"relay_solana_source_message_sha256":validated.message_sha256,"relay_solana_source_expected":expected,"relay_solana_source_validated":validated,(gofin_domain::BRIDGE_STEP_METADATA_KEY):{"transfer_id":record.id,"grant_id":record.grant_id,"step_id":link.step_id,"kind":"deposit","bounds":bounds,"settlement_expectation":record.settlement_expectation,"prospective_debit":record.prospective_debit}}),
            summary: self.summary(saved, &link.kind, alias),
            checks: vec![Check { name: "Relay Solana direct deposit".into(), status: CheckStatus::Passed, detail: "Exact order, source wallet, mint, instructions and fee limits verified against the frozen message.".into() }],
            warnings: vec!["Destination settlement requires independently finalized recipient credit.".into()],
            observed_at: self.clock.now().to_rfc3339(), expires_at: quote.valid_until.clone(),
            valuation: Some(valuation), intent, operation_version: 1,
        };
        if record.grant_id.is_none() {
            gofin_domain::value_bounds::bind(&mut prepared);
        }
        Ok(prepared)
    }

    pub(super) async fn revalidate_relay_solana_source(
        &self,
        prepared: &PreparedOperation,
    ) -> gofin_domain::Result<PreparedOperation> {
        let quote: BridgeQuote = serde_json::from_value(prepared.binding["quote"].clone())
            .map_err(|_| internal_error())?;
        self.relay_solana_quote_lifetime(&quote)?;
        let record = self
            .state()?
            .transfer(
                prepared.binding["transfer_id"]
                    .as_str()
                    .ok_or_else(internal_error)?,
            )
            .map_err(store_error)?
            .ok_or_else(internal_error)?;
        let saved = self.saved_quote(&record)?;
        let step: BridgeStep = serde_json::from_value(prepared.binding["step"].clone())
            .map_err(|_| internal_error())?;
        let bounds: BridgeBounds = serde_json::from_value(prepared.binding["bounds"].clone())
            .map_err(|_| internal_error())?;
        if quote != saved.quote
            || !saved.plan.steps.contains(&step)
            || prepared.binding["resource"] != json!(quote.intent.wallet)
            || prepared.binding["plan_digest"] != saved.plan.fixed_scope_digest
            || record
                .approved_bounds
                .as_ref()
                .is_some_and(|approved| approved != &prepared.binding["bounds"])
        {
            return Err(GoFinError::validation(
                "Relay Solana approval differs from its saved transfer.",
            ));
        }
        let expected = self
            .relay_source_expectation(&quote, &step, &bounds)
            .await?;
        if prepared.binding["relay_solana_source_expected"] != json!(expected) {
            return Err(GoFinError::validation(
                "Relay Solana mint or transfer fee changed before dispatch.",
            ));
        }
        let frozen = self.freeze_relay_source(prepared).await?;
        let validated =
            gofin_bridge::relay::solana::validate_source_transaction(&frozen, &expected)?;
        if prepared.binding["relay_solana_source_validated"] != json!(validated) {
            return Err(GoFinError::validation(
                "Relay Solana source instruction proof changed.",
            ));
        }
        let wallet = self.relay_source_wallet(&record, &quote)?;
        wallet.runtime().assert_lifetime_valid(&frozen).await?;
        let fee = wallet.runtime().validate_fee(&frozen).await?;
        wallet.runtime().simulate(&frozen).await?;
        self.validate_solana_stage_caps(
            &record,
            prepared.binding["step_id"]
                .as_str()
                .ok_or_else(internal_error)?,
            &bounds,
            fee,
            expected.max_rent_lamports,
        )?;
        self.validate_frozen_gas_cap(&bounds, &cctp_solana::sol_asset(), &Quantity::from_u64(fee))
            .await?;
        self.validate_selected_usd_asset_caps(
            &bounds,
            &quote.gross_debit.asset,
            &quote.intent.wallet,
        )
        .await?;
        let mut fresh = prepared.clone();
        fresh.valuation = Some(self.value_intent(&fresh.intent).await?);
        gofin_domain::value_bounds::carry_at(prepared, &mut fresh, self.clock.now())?;
        Ok(fresh)
    }

    pub(super) async fn submit_relay_solana_source(
        &self,
        validated: &gofin_domain::RevalidatedOperation,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> gofin_domain::Result<gofin_domain::Submission> {
        validated.assert_fresh()?;
        let prepared = validated.prepared();
        let frozen = self.freeze_relay_source(prepared).await?;
        let quote: BridgeQuote = serde_json::from_value(prepared.binding["quote"].clone())
            .map_err(|_| internal_error())?;
        let record = self
            .state()?
            .transfer(
                prepared.binding["transfer_id"]
                    .as_str()
                    .ok_or_else(internal_error)?,
            )
            .map_err(store_error)?
            .ok_or_else(internal_error)?;
        let wallet = self.relay_source_wallet(&record, &quote)?;
        let signer = wallet.signer().ok_or_else(internal_error)?;
        validated.assert_fresh()?;
        wallet
            .runtime()
            .dispatch(&frozen, &signer, prepared, journal)
            .await
    }

    pub(super) async fn reconcile_relay_solana_source(
        &self,
        record: &gofin_domain::OperationRecord,
    ) -> gofin_domain::Result<gofin_domain::Reconciliation> {
        let preview = record.preview.as_ref().ok_or_else(internal_error)?;
        let submission = record.submission.as_ref().ok_or_else(internal_error)?;
        let quote: BridgeQuote = serde_json::from_value(preview.binding["quote"].clone())
            .map_err(|_| internal_error())?;
        let transfer = self
            .state()?
            .transfer(
                preview.binding["transfer_id"]
                    .as_str()
                    .ok_or_else(internal_error)?,
            )
            .map_err(store_error)?
            .ok_or_else(internal_error)?;
        let wallet = self.relay_source_wallet(&transfer, &quote)?;
        if record.operation != "bridge.transfer"
            || preview.operation != record.operation
            || preview.input != record.input
            || submission.kind != "solana.transaction"
            || submission.data["message_sha256"]
                != preview.binding["relay_solana_source_message_sha256"]
        {
            return Err(GoFinError::validation(
                "Relay Solana submission differs from the original frozen operation.",
            ));
        }
        match wallet
            .runtime()
            .reconcile_submission(submission, record.dispatch_evidence.as_ref())
            .await?
        {
            ReceiptOutcome::ExpiredWithoutInclusion { reason, .. } => {
                Ok(gofin_domain::Reconciliation {
                    status: Status::Pending,
                    evidence: json!({"reason":reason,"settlement_verifier":"expired_without_inclusion_not_supported"}),
                    verified: None,
                })
            }
            ReceiptOutcome::Pending { confirmed, reason } => Ok(gofin_domain::Reconciliation {
                status: Status::Pending,
                evidence: json!({"confirmed":confirmed,"reason":reason,"submission":submission}),
                verified: None,
            }),
            ReceiptOutcome::NotDispatched {
                reason_code,
                reason,
            } => {
                let evidence = json!({"state":"not_dispatched","reason_code":reason_code,"reason":reason,"financial_effects":[]});
                Ok(gofin_domain::Reconciliation {
                    status: Status::Failed,
                    verified: Some(verified_bundle(
                        record,
                        submission.clone(),
                        vec![],
                        evidence.clone(),
                    )?),
                    evidence,
                })
            }
            ReceiptOutcome::Finalized { proof, .. } => {
                let expected: ExpectedSourceTransaction =
                    serde_json::from_value(preview.binding["relay_solana_source_expected"].clone())
                        .map_err(|_| internal_error())?;
                let validated: ValidatedSourceTransaction = serde_json::from_value(
                    preview.binding["relay_solana_source_validated"].clone(),
                )
                .map_err(|_| internal_error())?;
                // Solana compilation can upgrade a repeated account's flags
                // across instructions. The finalized message hash binds the
                // exact frozen bytes; the receipt verifier checks their effects.
                if proof.account_keys.first() != Some(&quote.intent.source_address)
                    || proof.message_sha256 != validated.message_sha256
                {
                    return Err(GoFinError::validation(
                        "Finalized Relay Solana transaction differs from the frozen message.",
                    ));
                }
                let mut effects = vec![];
                if proof.fee_lamports > 0 {
                    effects.push(FinancialEffect::Cost {
                        location: AccountingLocation::Cash {
                            resource: quote.intent.wallet.clone(),
                        },
                        amount: GoFinAmount {
                            asset: cctp_solana::sol_asset(),
                            atoms: Quantity::from_u64(proof.fee_lamports),
                        },
                        category: "gas".into(),
                        treatment: CostTreatment::AdditionalDebit,
                    });
                }
                if proof.failed {
                    verify_failed_relay_source_receipt(&proof, &expected, &validated)?;
                    let evidence = json!({"network":gofin_solana::NETWORK,"transaction_hash":proof.signature,"slot":proof.slot,"canonical":true,"success":false,"proof":proof});
                    return Ok(gofin_domain::Reconciliation {
                        status: Status::Failed,
                        verified: Some(verified_bundle(
                            record,
                            submission.clone(),
                            effects,
                            evidence.clone(),
                        )?),
                        evidence,
                    });
                }
                let payment = gofin_bridge::relay::solana::verify_source_receipt(
                    &proof, &validated, &expected,
                )?;
                if payment.wallet_rent_lamports > 0 {
                    effects.push(FinancialEffect::Cost {
                        location: AccountingLocation::Cash {
                            resource: quote.intent.wallet.clone(),
                        },
                        amount: GoFinAmount {
                            asset: cctp_solana::sol_asset(),
                            atoms: Quantity::from_u64(payment.wallet_rent_lamports),
                        },
                        category: "account_funding".into(),
                        treatment: CostTreatment::AdditionalDebit,
                    });
                }
                effects.push(FinancialEffect::CashDebit {
                    resource: quote.intent.wallet.clone(),
                    amount: quote.gross_debit.clone(),
                });
                effects.push(FinancialEffect::TransitPrincipal {
                    transfer_id: transfer.id.clone(),
                    amount: quote.gross_debit.clone(),
                });
                let evidence_id = format!(
                    "{}:{}:{}",
                    gofin_solana::NETWORK,
                    proof.signature,
                    validated.instruction_index
                );
                let evidence = json!({"network":gofin_solana::NETWORK,"source_chain_id":792703809,"transaction_hash":proof.signature,"slot":proof.slot,"canonical":true,"success":true,"transfer_id":transfer.id,"step_id":preview.binding["step_id"],"evidence_id":evidence_id,"receipt_scope":"instruction","instruction_index":validated.instruction_index,"correlation":expected.order_id,"recipient":quote.intent.source_address,"recipient_resource":quote.intent.wallet.0,"resource":quote.intent.wallet.0,"asset":quote.gross_debit.asset,"amount_atoms":quote.gross_debit.atoms,"proof":proof,"source_payment":payment});
                Ok(gofin_domain::Reconciliation {
                    status: Status::Completed,
                    verified: Some(verified_bundle(
                        record,
                        submission.clone(),
                        effects,
                        evidence.clone(),
                    )?),
                    evidence,
                })
            }
        }
    }

    async fn relay_solana_hub_witnesses(
        &self,
        candidates: &[BridgeEvidenceCandidate],
        order_id: &str,
        source_protocol_chain_id: &str,
    ) -> gofin_domain::Result<
        Vec<(
            String,
            gofin_bridge::relay::solana::VerifiedHubPayoutWitness,
        )>,
    > {
        let observer = self.observers.get(&537713).ok_or_else(internal_error)?;
        let (confirmations, finalized) = self
            .confirmation_rules
            .get(&537713)
            .copied()
            .unwrap_or((2, true));
        let expected = gofin_bridge::relay::solana::HubPayoutExpectation {
            order_id: order_id.into(),
            source_protocol_chain_id: source_protocol_chain_id.into(),
            executor: RELAY_EXECUTOR.into(),
            allocator: RELAY_ALLOCATOR.into(),
        };
        let mut witnesses = vec![];
        for candidate in candidates
            .iter()
            .filter(|candidate| {
                candidate.kind == EvidenceKind::SettlementHub
                    && candidate.chain_id == 537713
                    && candidate.order_id.as_deref() == Some(order_id)
            })
            .take(16)
        {
            let hash = match gofin_evm::hash(&candidate.transaction_hash) {
                Ok(hash) => format!("{hash:#x}"),
                Err(_) => continue,
            };
            let Some(receipt) = observer
                .observe_receipt(&hash, confirmations, finalized)
                .await?
            else {
                continue;
            };
            if !receipt.success || !receipt_logs_canonical(&receipt.receipt, &receipt, &hash)? {
                continue;
            }
            let logs = receipt.receipt["logs"]
                .as_array()
                .ok_or_else(internal_error)?;
            if logs.len() > 256 {
                continue;
            }
            let raw = logs
                .iter()
                .map(raw_evm_log)
                .collect::<gofin_domain::Result<Vec<_>>>()?;
            let mut exact = vec![];
            for executed in raw
                .iter()
                .filter(|log| log.address.eq_ignore_ascii_case(RELAY_EXECUTOR))
            {
                for payload in raw
                    .iter()
                    .filter(|log| log.address.eq_ignore_ascii_case(RELAY_ALLOCATOR))
                {
                    if let Ok(witness) = gofin_bridge::relay::solana::verify_hub_payout_witness(
                        executed, payload, &expected,
                    ) {
                        exact.push(witness);
                    }
                }
            }
            if exact.len() == 1 {
                witnesses.push((hash, exact.remove(0)));
            }
        }
        Ok(witnesses)
    }

    async fn relay_solana_solver_witnesses(
        &self,
        candidates: &[BridgeEvidenceCandidate],
        order_id: &str,
        source_protocol_chain_id: &str,
    ) -> gofin_domain::Result<
        Vec<(
            String,
            gofin_bridge::relay::solana::VerifiedHubOrderSettlement,
        )>,
    > {
        let observer = self.observers.get(&537713).ok_or_else(internal_error)?;
        let (confirmations, finalized) = self
            .confirmation_rules
            .get(&537713)
            .copied()
            .unwrap_or((2, true));
        let mut witnesses = vec![];
        for candidate in candidates
            .iter()
            .filter(|candidate| {
                candidate.kind == EvidenceKind::SettlementHub
                    && candidate.chain_id == 537713
                    && candidate.order_id.as_deref() == Some(order_id)
            })
            .take(16)
        {
            let hash = match gofin_evm::hash(&candidate.transaction_hash) {
                Ok(hash) => format!("{hash:#x}"),
                Err(_) => continue,
            };
            let Some(receipt) = observer
                .observe_receipt(&hash, confirmations, finalized)
                .await?
            else {
                continue;
            };
            if !receipt.success || !receipt_logs_canonical(&receipt.receipt, &receipt, &hash)? {
                continue;
            }
            let logs = receipt.receipt["logs"]
                .as_array()
                .ok_or_else(internal_error)?;
            if logs.len() > 256 {
                continue;
            }
            let raw = logs
                .iter()
                .map(raw_evm_log)
                .collect::<gofin_domain::Result<Vec<_>>>()?;
            let exact = raw
                .iter()
                .filter_map(|log| {
                    gofin_bridge::relay::solana::verify_hub_order_settlement(
                        log,
                        RELAY_HUB,
                        RELAY_ORACLE,
                        source_protocol_chain_id,
                        order_id,
                    )
                    .ok()
                })
                .collect::<Vec<_>>();
            if exact.len() == 1 {
                witnesses.push((hash, exact[0].clone()));
            }
        }
        Ok(witnesses)
    }

    async fn relay_solana_payout(
        &self,
        engine: &Engine,
        transfer: &BridgeTransferRecord,
        saved: &SavedQuote,
        outcome: gofin_domain::BridgeSettlementOutcome,
    ) -> gofin_domain::Result<Option<Value>> {
        let quote = &saved.quote;
        let order_id = quote
            .protocol_order_id
            .as_deref()
            .ok_or_else(internal_error)?;
        let request_id = quote
            .provider_request_id
            .as_deref()
            .ok_or_else(internal_error)?;
        let source_chain = bridge_source_chain_id(&quote.intent);
        let source_protocol = if source_chain == 792703809 {
            gofin_bridge::relay::RELAY_SOLANA_PROTOCOL_CHAIN_ID.to_owned()
        } else {
            reviewed_relay_config()
                .chains
                .get(&source_chain)
                .ok_or_else(internal_error)?
                .protocol_chain_id
                .clone()
        };
        let candidates = self
            .provider("relay")?
            .reconcile(&BridgeTransferRef {
                order_id: Some(order_id.into()),
                provider_request_id: Some(request_id.into()),
                source_chain_id: source_chain,
                destination_chain_id: quote.intent.destination_chain_id,
                destination_network: quote
                    .intent
                    .solana_destination
                    .as_ref()
                    .map(|destination| destination.network.clone()),
                destination_locator: transfer
                    .transit
                    .as_ref()
                    .and_then(|transit| transit.destination_locator.clone()),
            })
            .await?
            .candidates;
        let mut witnesses = self
            .relay_solana_hub_witnesses(&candidates, order_id, &source_protocol)
            .await?
            .into_iter()
            .map(|(hash, witness)| (hash, RelaySolanaWitness::Depository(witness)))
            .collect::<Vec<_>>();
        witnesses.extend(
            self.relay_solana_solver_witnesses(&candidates, order_id, &source_protocol)
                .await?
                .into_iter()
                .map(|(hash, witness)| (hash, RelaySolanaWitness::Solver(witness))),
        );
        if witnesses.is_empty() {
            return Ok(None);
        }
        let targets = match outcome {
            gofin_domain::BridgeSettlementOutcome::Destination => {
                let destination = quote
                    .intent
                    .solana_destination
                    .as_ref()
                    .ok_or_else(internal_error)?;
                let target = transfer
                    .settlement_expectation
                    .as_ref()
                    .and_then(|scope| {
                        scope.destination.iter().find(|target| {
                            target.resource == destination.wallet_resource
                                && target.asset == destination.asset
                        })
                    })
                    .ok_or_else(internal_error)?;
                vec![(
                    destination.recipient.clone(),
                    target.asset.clone(),
                    target.minimum_atoms.clone(),
                )]
            }
            gofin_domain::BridgeSettlementOutcome::Refund => {
                let scope = transfer
                    .settlement_expectation
                    .as_ref()
                    .ok_or_else(internal_error)?;
                relay_solana_refund_targets(&quote.refund_paths, &quote.intent.refund_to, scope)
            }
        };
        let kind = match outcome {
            gofin_domain::BridgeSettlementOutcome::Destination => EvidenceKind::DestinationFill,
            gofin_domain::BridgeSettlementOutcome::Refund => EvidenceKind::Refund,
        };
        for (recipient, asset, floor) in targets {
            gofin_solana::parse_address(&recipient)?;
            let mint = (asset.reference != "native").then_some(asset.reference.clone());
            let token_program = if let Some(mint) = &mint {
                let info =
                    gofin_bridge::relay::solana::resolve_relay_mint(&self.solana_observer, mint)
                        .await?;
                if info.decimals != asset.decimals {
                    return Err(GoFinError::validation(
                        "Relay Solana payout mint precision changed.",
                    ));
                }
                Some(info.program_id)
            } else {
                None
            };
            let expected = gofin_bridge::relay::solana::ExpectedPayout {
                recipient: recipient.clone(),
                mint,
                token_program,
                decimals: asset.decimals,
                minimum_amount_atoms: if floor.is_zero() {
                    Quantity::from_u64(1)
                } else {
                    floor
                },
                order_id: order_id.into(),
            };
            for (hub_hash, witness) in &witnesses {
                for candidate in candidates
                    .iter()
                    .filter(|candidate| {
                        candidate.kind == kind
                            && candidate.chain_id == 792703809
                            && candidate.order_id.as_deref() == Some(order_id)
                    })
                    .take(32)
                {
                    let proof = match self
                        .solana_observer
                        .observe_finalized_transaction(&candidate.transaction_hash)
                        .await?
                    {
                        ReceiptOutcome::Finalized { proof, .. } => proof,
                        ReceiptOutcome::Pending { .. }
                        | ReceiptOutcome::NotDispatched { .. }
                        | ReceiptOutcome::ExpiredWithoutInclusion { .. } => {
                            continue;
                        }
                    };
                    let (payout, witness_json) = match witness {
                        RelaySolanaWitness::Depository(witness) => (
                            gofin_bridge::relay::solana::verify_payout_receipt(
                                &proof, witness, &expected,
                            ),
                            json!(witness),
                        ),
                        RelaySolanaWitness::Solver(witness) => (
                            gofin_bridge::relay::solana::verify_solver_payout_receipt(
                                &proof,
                                witness,
                                &expected,
                                request_id,
                                RELAY_SOLANA_SOLVER,
                            ),
                            json!(witness),
                        ),
                    };
                    let payout = match payout {
                        Ok(payout) => payout,
                        Err(_) => continue,
                    };
                    let resource = gofin_domain::ResourceId(format!(
                        "{}:{}",
                        gofin_solana::NETWORK,
                        recipient
                    ));
                    let amount = GoFinAmount {
                        asset: asset.clone(),
                        atoms: payout.net_amount_atoms.clone(),
                    };
                    let evidence_id = format!(
                        "{}:{}:{}",
                        gofin_solana::NETWORK,
                        proof.signature,
                        payout.instruction_index
                    );
                    let evidence = json!({"transfer_id":transfer.id,"provider":"relay","network":gofin_solana::NETWORK,"transaction_hash":proof.signature,"slot":proof.slot,"canonical":true,"commitment":"finalized","success":true,"hub_chain_id":537713,"hub_transaction_hash":hub_hash,"hub_witness":witness_json,"correlation":order_id,"recipient":recipient,"recipient_resource":resource,"asset":asset,"amount_atoms":payout.net_amount_atoms,"net_amount_atoms":payout.net_amount_atoms,"evidence_id":evidence_id,"receipt_scope":"instruction","instruction_index":payout.instruction_index,"proof_method":payout.proof_method,"payout":payout,"proof":proof});
                    let mut settlement = gofin_domain::BridgeSettlementProof {
                        transfer_id: transfer.id.clone(),
                        outcome: outcome.clone(),
                        evidence_id: evidence_id.clone(),
                        effects: vec![FinancialEffect::UsableCredit {
                            to: resource,
                            amount,
                        }],
                        evidence,
                        observed_at: self.clock.now().to_rfc3339(),
                        proof_digest: String::new(),
                    };
                    settlement.proof_digest = gofin_domain::bridge_settlement_digest(&settlement)?;
                    engine.commit_bridge_external_settlement(settlement)?;
                    return Ok(Some(
                        json!({"provider":"relay","settled":true,"status":if matches!(outcome,gofin_domain::BridgeSettlementOutcome::Destination){"destination_settled"}else{"refunded"},"evidence_id":evidence_id,"transaction_hash":proof.signature,"net_amount_atoms":payout.net_amount_atoms}),
                    ));
                }
            }
        }
        Ok(None)
    }

    pub(super) async fn reconcile_relay_solana_external(
        &self,
        engine: &Engine,
        transfer: &BridgeTransferRecord,
        saved: &SavedQuote,
    ) -> gofin_domain::Result<Value> {
        if let Some(settled) = self
            .relay_solana_payout(
                engine,
                transfer,
                saved,
                gofin_domain::BridgeSettlementOutcome::Destination,
            )
            .await?
        {
            return Ok(settled);
        }
        Ok(
            json!({"provider":"relay","transfer_id":transfer.id,"settled":false,"status":"canonical_solana_payout_pending","next_action":"Retain the original transfer and poll bridge.status; Relay status hashes are locators only."}),
        )
    }

    pub(super) async fn try_reconcile_relay_solana_refund(
        &self,
        engine: &Engine,
        transfer: &BridgeTransferRecord,
        saved: &SavedQuote,
    ) -> gofin_domain::Result<Option<Value>> {
        if saved.quote.intent.solana_source.is_none()
            || !saved
                .quote
                .refund_paths
                .iter()
                .any(|path| path.chain_id == 792703809)
        {
            return Ok(None);
        }
        self.relay_solana_payout(
            engine,
            transfer,
            saved,
            gofin_domain::BridgeSettlementOutcome::Refund,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refund_targets_preserve_each_approved_solana_mint_and_floor() {
        let recipient = "GTtzwxqy67xx9DVESJjx28TgXqpc8xTqtiytgNMaQBTE";
        let assets = [
            gofin_domain::AssetId {
                network: gofin_solana::NETWORK.into(),
                reference: "native".into(),
                decimals: 9,
            },
            gofin_domain::AssetId {
                network: gofin_solana::NETWORK.into(),
                reference: gofin_solana::funding::MAINNET_USDC_MINT.into(),
                decimals: 6,
            },
        ];
        let paths = assets
            .iter()
            .enumerate()
            .map(|(index, asset)| gofin_bridge::RefundPath {
                chain_id: 792703809,
                network: Some(gofin_solana::NETWORK.into()),
                asset: asset.clone(),
                recipient: recipient.into(),
                minimum: Quantity::from_u64((index + 1) as u64),
                maximum: Quantity::from_u64(100),
            })
            .collect::<Vec<_>>();
        let scope = gofin_domain::BridgeSettlementExpectation {
            destination: vec![],
            refund: assets
                .iter()
                .enumerate()
                .map(|(index, asset)| gofin_domain::BridgeSettlementTarget {
                    resource: gofin_domain::ResourceId(format!(
                        "{}:{}",
                        gofin_solana::NETWORK,
                        recipient
                    )),
                    asset: asset.clone(),
                    minimum_atoms: Quantity::from_u64((index + 1) as u64),
                })
                .collect(),
        };
        let targets = relay_solana_refund_targets(&paths, recipient, &scope);
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[1].1, assets[1]);
        assert_eq!(targets[1].2, Quantity::from_u64(2));
    }

    #[test]
    fn failed_source_receipt_allows_only_network_fee() {
        let expected = ExpectedSourceTransaction {
            wallet: "wallet".into(),
            depositor: "wallet".into(),
            order_id: "order".into(),
            mint: None,
            token_program: None,
            decimals: 9,
            amount_atoms: Quantity::from_u64(100),
            minimum_vault_credit_atoms: Quantity::from_u64(100),
            max_network_fee_lamports: 5,
            max_rent_lamports: 0,
        };
        let validated = ValidatedSourceTransaction {
            message_sha256: "message".into(),
            order_id: "order".into(),
            instruction_index: 0,
            vault: "vault".into(),
            vault_token_account: None,
            sender_token_account: None,
        };
        let mut proof = gofin_solana::FinalizedReceiptProof {
            signature: "signature".into(),
            slot: 1,
            fee_lamports: 5,
            failed: true,
            message_sha256: "message".into(),
            account_keys: vec!["wallet".into(), "vault".into()],
            message_instructions: vec![],
            required_signatures: vec![],
            pre_balances_lamports: vec![1000, 100],
            post_balances_lamports: vec![995, 100],
            pre_token_balances: vec![],
            post_token_balances: vec![],
            loaded_addresses: json!({}),
            inner_instructions: vec![],
            log_messages: vec![],
            error: Some(json!({"InstructionError":[0,"Custom"]})),
        };
        verify_failed_relay_source_receipt(&proof, &expected, &validated).unwrap();
        proof.post_balances_lamports[1] = 101;
        assert!(verify_failed_relay_source_receipt(&proof, &expected, &validated).is_err());
        proof.post_balances_lamports[1] = 100;
        proof.post_token_balances = vec![json!({"accountIndex":1,"uiTokenAmount":{"amount":"1"}})];
        assert!(verify_failed_relay_source_receipt(&proof, &expected, &validated).is_err());
    }
}
