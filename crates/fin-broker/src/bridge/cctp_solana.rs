//! Cross-VM destination preparation uses the existing protected Solana runtime.
use super::*;
use fin_bridge::{SolanaDestination, SolanaDestinationInput, cctp};
use fin_solana::{NormalizedInstruction, SolanaWalletAdapter};

pub(super) fn sol_asset() -> fin_domain::AssetId {
    fin_domain::AssetId {
        network: fin_solana::NETWORK.into(),
        reference: "native".into(),
        decimals: 9,
    }
}
pub(super) fn sol_cap(
    bounds: &BridgeBounds,
    kind: fin_bridge::FeeKind,
) -> fin_domain::Result<Quantity> {
    bounds
        .fee_caps
        .iter()
        .find(|cap| cap.kind == kind && cap.asset == sol_asset())
        .map(|cap| cap.maximum.clone())
        .ok_or_else(|| {
            FinError::validation(
                "Solana mint requires separate finite SOL gas and nonce-account funding caps.",
            )
        })
}
fn require_ready_state(
    message: &cctp::CctpMessage,
    state: &cctp::solana::SolanaCctpState,
) -> fin_domain::Result<()> {
    if state.used_nonce {
        return Err(error(
            "CCTP_ALREADY_PROCESSED",
            "preview",
            "The source-linked CCTP nonce is already processed on Solana.",
            "Reconcile bridge.status; do not submit another mint.",
        ));
    }
    if !message.expiration_block.is_zero()
        && message.expiration_block <= Quantity::from_u64(state.slot)
    {
        return Err(error(
            "CCTP_ATTESTATION_EXPIRED",
            "preview",
            "The CCTP attestation has expired at the Solana destination slot.",
            "Explicitly request bridge.recovery for this original burn, then poll bridge.status before minting.",
        ));
    }
    if !state.recipient_exists {
        return Err(error(
            "CCTP_SOLANA_ATA_REQUIRED",
            "preview",
            "The approved Solana recipient needs an existing native-USDC associated token account.",
            "Create that exact account under separate authority, then resume the original transfer.",
        ));
    }
    Ok(())
}

impl BridgeAdapter {
    pub(super) fn cctp_solana_wallet(
        &self,
        destination: &SolanaDestination,
    ) -> fin_domain::Result<&Arc<SolanaWalletAdapter>> {
        let wallet = self
            .solana_wallets
            .get(&destination.wallet)
            .ok_or_else(|| FinError::validation("The approved Solana wallet is not enrolled."))?;
        if wallet.address() != destination.wallet_address
            || wallet.signer().is_none()
            || destination.wallet_address != destination.recipient
            || destination.destination_caller != destination.wallet_address
            || destination.wallet_resource
                != fin_domain::ResourceId(format!("{}:{}", fin_solana::NETWORK, wallet.address()))
        {
            return Err(FinError::validation(
                "Solana mint requires the exact approved recipient and protected signing wallet.",
            ));
        }
        Ok(wallet)
    }
    pub(super) async fn resolve_solana_destination(
        &self,
        input: &SolanaDestinationInput,
        recipient: &str,
    ) -> fin_domain::Result<SolanaDestination> {
        if input.network != fin_solana::NETWORK
            || input.mint != fin_solana::funding::MAINNET_USDC_MINT
        {
            return Err(FinError::validation(
                "CCTP Solana requires mainnet-beta native USDC.",
            ));
        }
        fin_solana::parse_address(recipient)?;
        let wallet = self.solana_wallets.get(&input.wallet).ok_or_else(|| {
            FinError::validation("Enroll the exact Solana destination wallet before quoting CCTP.")
        })?;
        let asset = fin_solana::funding::resolve_external_asset(
            &wallet.runtime(),
            "USDC",
            Some(&input.mint),
        )
        .await?;
        let destination = SolanaDestination {
            network: input.network.clone(),
            wallet: input.wallet.clone(),
            wallet_resource: fin_domain::ResourceId(format!(
                "{}:{}",
                fin_solana::NETWORK,
                wallet.address()
            )),
            wallet_address: wallet.address().into(),
            recipient: recipient.into(),
            asset,
            recipient_token_account: fin_solana::associated_token_address(
                recipient,
                &input.mint,
                fin_solana::token::TOKEN_PROGRAM_ID,
            )?,
            destination_caller: wallet.address().into(),
        };
        self.cctp_solana_wallet(&destination)?;
        Ok(destination)
    }
    pub(super) async fn solana_routes(
        &self,
        input: &fin_bridge::BridgeRoutesInput,
    ) -> fin_domain::Result<Value> {
        let destination = input
            .solana_destination
            .as_ref()
            .ok_or_else(internal_error)?;
        if input.destination_chain_id.is_some()
            || input.destination_asset.is_some()
            || destination.network != fin_solana::NETWORK
            || destination.mint != fin_solana::funding::MAINNET_USDC_MINT
        {
            return Err(FinError::validation(
                "Use only an explicit Solana mainnet-beta native-USDC destination.",
            ));
        }
        let source = self.asset(&input.source, input.source.chain_id).await?;
        if source != cctp::usdc_asset(source.chain_id)? {
            return Err(FinError::validation(
                "CCTP requires exact native USDC on its EVM source.",
            ));
        }
        Ok(
            json!({"indicative":true,"source":source,"solana_destination":destination,"coverage":{"providers":["cctp"],"complete":true,"market_wide_comparison":false},"routes":[{"provider":"cctp","availability":"candidate","destination":destination,"limitations":["Requires an enrolled Solana signer and existing native-USDC associated token account; quote verifies deployments and accounts."]}],"next_action":"Request bridge.quotes with provider cctp, solanaDestination wallet, exact owner and finite ETH/SOL fee caps."}),
        )
    }
    fn solana_cost_usage(
        &self,
        transfer: &BridgeTransferRecord,
        current_step: Option<&str>,
        rent: bool,
    ) -> fin_domain::Result<(Quantity, Quantity)> {
        let engine = self
            .engine
            .lock()
            .map_err(|_| internal_error())?
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        let operations = match engine {
            Some(engine) => engine.operations()?,
            None if transfer
                .steps
                .iter()
                .all(|step| step.operation_id.is_none()) =>
            {
                vec![]
            }
            None => return Err(internal_error()),
        };
        let mut spent = Quantity::zero();
        let mut reserved = Quantity::zero();
        for operation in operations.iter().filter(|operation| {
            operation.preview.as_ref().is_some_and(|preview| {
                preview.binding["transfer_id"] == transfer.id
                    && current_step
                        .is_none_or(|step| preview.binding["step_id"].as_str() != Some(step))
            })
        }) {
            if let Some(effects) = &operation.effects {
                for effect in &effects.effects {
                    if let FinancialEffect::Cost {
                        amount,
                        category,
                        treatment: CostTreatment::AdditionalDebit,
                        ..
                    } = effect
                        && amount.asset == sol_asset()
                        && (category == "cctp_nonce_rent") == rent
                    {
                        spent = spent.checked_add(&amount.atoms)?;
                    }
                }
            } else if !operation.status.terminal()
                && let Some(preview) = &operation.preview
            {
                let field = if rent {
                    "solana_account_funding_lamports"
                } else {
                    "solana_maximum_network_fee_lamports"
                };
                if let Some(value) = preview.binding[field].as_u64() {
                    reserved = reserved.checked_add(&Quantity::from_u64(value))?;
                }
            }
        }
        Ok((spent, reserved))
    }
    pub(super) async fn validate_cctp_solana_gas(
        &self,
        quote: &BridgeQuote,
        bounds: &BridgeBounds,
        transfer: &BridgeTransferRecord,
    ) -> fin_domain::Result<()> {
        let destination = quote
            .intent
            .solana_destination
            .as_ref()
            .ok_or_else(internal_error)?;
        let wallet = self.cctp_solana_wallet(destination)?;
        let mut needed = Quantity::zero();
        for (kind, rent) in [
            (fin_bridge::FeeKind::Gas, false),
            (fin_bridge::FeeKind::AdditionalDebit, true),
        ] {
            let cap = sol_cap(bounds, kind)?;
            let (spent, reserved) = self.solana_cost_usage(transfer, None, rent)?;
            if spent.checked_add(&reserved)? > cap {
                return Err(FinError::validation(
                    "Prior Solana mint costs or reservations exceed the approved cap.",
                ));
            }
            needed = needed.checked_add(&cap.checked_sub(&spent)?)?;
        }
        let balance = wallet
            .runtime()
            .rpc()
            .request(
                "getBalance",
                json!([wallet.address(),{"commitment":"finalized"}]),
            )
            .await?;
        let balance = balance["value"].as_u64().ok_or_else(internal_error)?;
        if Quantity::from_u64(balance) < needed {
            return Err(error(
                "CCTP_GAS_REQUIRED",
                "preview",
                "The Solana destination lacks SOL for its remaining gas and nonce-account funding caps.",
                "Fund the approved Solana wallet, then resume the same transfer.",
            ));
        }
        Ok(())
    }
    fn validate_solana_stage_caps(
        &self,
        transfer: &BridgeTransferRecord,
        step: &str,
        bounds: &BridgeBounds,
        gas: u64,
        rent: u64,
    ) -> fin_domain::Result<()> {
        for (kind, is_rent, candidate) in [
            (fin_bridge::FeeKind::Gas, false, gas),
            (fin_bridge::FeeKind::AdditionalDebit, true, rent),
        ] {
            let (spent, reserved) = self.solana_cost_usage(transfer, Some(step), is_rent)?;
            if spent
                .checked_add(&reserved)?
                .checked_add(&Quantity::from_u64(candidate))?
                > sol_cap(bounds, kind)?
            {
                return Err(error(
                    "BRIDGE_CUMULATIVE_FEE_LIMIT",
                    "preview",
                    "Earlier mint costs and reservations leave insufficient approved SOL budget.",
                    "Preserve the original burn; review a revised finite plan if necessary.",
                ));
            }
        }
        Ok(())
    }
    async fn validate_solana_usd_caps(
        &self,
        quote: &BridgeQuote,
        bounds: &BridgeBounds,
        gas: u64,
        rent: u64,
    ) -> fin_domain::Result<()> {
        self.validate_frozen_gas_cap(bounds, &sol_asset(), &Quantity::from_u64(gas))
            .await?;
        let cap = bounds
            .fee_caps
            .iter()
            .find(|cap| {
                cap.kind == fin_bridge::FeeKind::AdditionalDebit && cap.asset == sol_asset()
            })
            .ok_or_else(internal_error)?;
        if let Some(maximum) = cap.maximum_usd_micros {
            let price = self.fresh_price(&sol_asset()).await?;
            let actual = Quantity::from_u64(rent)
                .mul_div_ceil(&price.usd_micros_per_unit, &price.unit_atoms)?;
            if actual > Quantity::from_u64(maximum) {
                return Err(error(
                    "BRIDGE_FEE_USD_CAP_EXCEEDED",
                    "pricing",
                    "Solana nonce-account funding exceeds the approved USD cap at fresh prices.",
                    "Preserve this burn and review a finite updated authority before minting.",
                ));
            }
        }
        self.validate_selected_usd_caps(bounds, &quote.intent.source, &quote.intent.wallet)
            .await
    }

    pub(super) async fn freeze_cctp_solana(
        &self,
        prepared: &PreparedOperation,
    ) -> fin_domain::Result<fin_solana::FrozenTransaction> {
        let quote: BridgeQuote = serde_json::from_value(prepared.binding["quote"].clone())
            .map_err(|_| internal_error())?;
        let destination = quote
            .intent
            .solana_destination
            .as_ref()
            .ok_or_else(internal_error)?;
        let wallet = self.cctp_solana_wallet(destination)?;
        let signer = wallet.signer().ok_or_else(internal_error)?;
        let instructions: Vec<NormalizedInstruction> =
            serde_json::from_value(prepared.binding["solana_instructions"].clone())
                .map_err(|_| internal_error())?;
        let frozen = wallet
            .runtime()
            .freeze_instructions(
                &instructions,
                &[],
                prepared.binding["solana_blockhash"]
                    .as_str()
                    .ok_or_else(internal_error)?,
                &signer,
                prepared.binding["solana_last_valid_block_height"]
                    .as_u64()
                    .ok_or_else(internal_error)?,
                prepared.binding["solana_maximum_network_fee_lamports"]
                    .as_u64()
                    .ok_or_else(internal_error)?,
                None,
                &[],
            )
            .await?;
        if prepared.binding["solana_message_sha256"] != frozen.message_sha256() {
            return Err(FinError::validation(
                "The Solana mint no longer matches its frozen message identity.",
            ));
        }
        Ok(frozen)
    }
    pub(super) async fn prepare_cctp_solana_stage(
        &self,
        input: &Value,
        record: &BridgeTransferRecord,
        saved: &SavedQuote,
        step: BridgeStep,
        link: &BridgeStepLink,
        requested: Option<BridgeBounds>,
    ) -> fin_domain::Result<PreparedOperation> {
        if saved.quote.provider != "cctp"
            || link.kind != BridgeStepKind::Mint
            || record.grant_id.is_some()
        {
            return Err(FinError::validation(
                "Solana CCTP mint requires its original exact or named-plan authority.",
            ));
        }
        let locator = cctp_lifecycle::cctp_locator(record)?;
        let bounds: BridgeBounds =
            serde_json::from_value(record.approved_bounds.clone().ok_or_else(internal_error)?)
                .map_err(|_| internal_error())?;
        if requested.is_some() && normalize_authorization_bounds(requested, saved)? != bounds {
            return Err(FinError::validation(
                "Solana mint must preserve the original approved bounds.",
            ));
        }
        let destination = saved
            .quote
            .intent
            .solana_destination
            .as_ref()
            .ok_or_else(internal_error)?;
        let wallet = self.cctp_solana_wallet(destination)?;
        if step.call.is_some()
            || step
                .solana_mint
                .as_ref()
                .is_none_or(|spec| &spec.destination != destination)
            || step.expected_sender != destination.wallet_address
        {
            return Err(FinError::validation(
                "The saved Solana mint step has changed destination scope.",
            ));
        }
        let attestation = self
            .cctp_attestation(&saved.quote, locator)
            .await?
            .ok_or_else(|| {
                error(
                    "CCTP_ATTESTATION_PENDING",
                    "preview",
                    "Circle attestation is pending.",
                    "Poll bridge.status and retain the original burn.",
                )
            })?;
        let message = cctp_lifecycle::validate_attestation(&saved.quote, locator, &attestation)?;
        let provider = self.cctp_provider()?;
        let state = provider
            .solana_state(
                saved.quote.intent.source.chain_id,
                destination,
                Some(&message.nonce),
            )
            .await?;
        require_ready_state(&message, &state)?;
        let instructions =
            cctp::solana::solana_mint_instructions(destination, &attestation, &state)?;
        let runtime = wallet.runtime();
        let signer = wallet.signer().ok_or_else(internal_error)?;
        let latest = runtime
            .rpc()
            .request("getLatestBlockhash", json!([{"commitment":"finalized"}]))
            .await?;
        let blockhash = latest["value"]["blockhash"]
            .as_str()
            .ok_or_else(internal_error)?;
        let height = latest["value"]["lastValidBlockHeight"]
            .as_u64()
            .ok_or_else(internal_error)?;
        let gas = sol_cap(&bounds, fin_bridge::FeeKind::Gas)?
            .as_str()
            .parse::<u64>()
            .map_err(|_| internal_error())?;
        let frozen = runtime
            .freeze_instructions(
                &instructions,
                &[],
                blockhash,
                &signer,
                height,
                gas,
                None,
                &[],
            )
            .await?;
        let actual_fee = runtime.validate_fee(&frozen).await?;
        runtime.simulate(&frozen).await?;
        self.validate_cctp_solana_gas(&saved.quote, &bounds, record)
            .await?;
        self.validate_solana_stage_caps(
            record,
            &link.step_id,
            &bounds,
            actual_fee,
            state.account_funding_lamports,
        )?;
        self.validate_solana_usd_caps(
            &saved.quote,
            &bounds,
            actual_fee,
            state.account_funding_lamports,
        )
        .await?;
        let resource = destination.wallet_resource.clone();
        let cost = |atoms| CostBound {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: FinAmount {
                asset: sol_asset(),
                atoms: Quantity::from_u64(atoms),
            },
            treatment: CostTreatment::AdditionalDebit,
        };
        let intent = FinancialIntent {
            debits: vec![],
            maximum_costs: vec![cost(
                actual_fee
                    .checked_add(state.account_funding_lamports)
                    .ok_or_else(internal_error)?,
            )],
            expected: vec![],
            locks: vec![resource.clone()],
        };
        let valuation = self.value_intent(&intent).await?;
        let mut plan = saved.plan.clone();
        plan.bounds = bounds.clone();
        plan.fixed_scope_digest =
            cctp::scope_digest(&saved.quote, &bounds, &saved.quote.intent.source_address)?;
        let mut prepared=PreparedOperation{action_deadline:None,operation:"bridge.transfer".into(),input:input.clone(),binding:json!({"resource":resource,"transfer_id":record.id,"step_id":link.step_id,"step_kind":link.kind,"step_dependencies":link.depends_on,"grant_id":null,"quote_id":saved.quote.quote_id,"route_id":saved.quote.route_id,"quote":saved.quote,"intent":saved.quote.intent,"plan_digest":plan.fixed_scope_digest,"plan":plan,"step_ids":saved.step_ids,"step":step,"bounds":bounds,"source_locator":locator,"cctp_attestation":attestation,"solana_instructions":instructions,"solana_blockhash":blockhash,"solana_last_valid_block_height":height,"solana_maximum_network_fee_lamports":actual_fee,"solana_account_funding_lamports":state.account_funding_lamports,"solana_message_sha256":frozen.message_sha256(),(fin_domain::BRIDGE_STEP_METADATA_KEY):{"transfer_id":record.id,"grant_id":null,"step_id":link.step_id,"kind":"mint","bounds":bounds,"settlement_expectation":record.settlement_expectation}}),summary:self.summary(saved,&link.kind,&destination.wallet),checks:vec![Check{name:"CCTP Solana mint".into(),status:CheckStatus::Passed,detail:"Verified source-linked attestation, destination program accounts, existing native-USDC ATA, nonce and bounded SOL fees.".into()}],warnings:vec!["Mint completion requires finalized Solana native-USDC credit.".into()],observed_at:self.clock.now().to_rfc3339(),expires_at:(self.clock.now()+chrono::Duration::seconds(90)).to_rfc3339(),valuation:Some(valuation),intent,operation_version:1};
        fin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }
    pub(super) async fn revalidate_cctp_solana_mint(
        &self,
        prepared: &PreparedOperation,
    ) -> fin_domain::Result<PreparedOperation> {
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
        let locator = cctp_lifecycle::cctp_locator(&record)?;
        let saved = self.saved_quote(&record)?;
        if quote != saved.quote
            || prepared.binding["step_kind"] != "mint"
            || prepared.binding["resource"] != json!(quote.intent.destination_resource()?)
        {
            return Err(FinError::validation(
                "Solana mint differs from its persisted transfer or destination resource.",
            ));
        }
        if quote.provider != "cctp"
            || record.grant_id.is_some()
            || json!(locator) != prepared.binding["source_locator"]
        {
            return Err(FinError::validation("Solana mint source binding changed."));
        }
        let attestation: cctp::CctpAttestation =
            serde_json::from_value(prepared.binding["cctp_attestation"].clone())
                .map_err(|_| internal_error())?;
        let message = cctp_lifecycle::validate_attestation(&quote, locator, &attestation)?;
        let destination = quote
            .intent
            .solana_destination
            .as_ref()
            .ok_or_else(internal_error)?;
        let state = self
            .cctp_provider()?
            .solana_state(
                quote.intent.source.chain_id,
                destination,
                Some(&message.nonce),
            )
            .await?;
        require_ready_state(&message, &state)?;
        if prepared.binding["solana_instructions"]
            != json!(cctp::solana::solana_mint_instructions(
                destination,
                &attestation,
                &state
            )?)
            || prepared.binding["solana_account_funding_lamports"] != state.account_funding_lamports
        {
            return Err(FinError::validation(
                "Solana mint account state or exact instructions changed before dispatch.",
            ));
        }
        let frozen = self.freeze_cctp_solana(prepared).await?;
        let runtime = self.cctp_solana_wallet(destination)?.runtime();
        runtime.assert_lifetime_valid(&frozen).await?;
        let fee = runtime.validate_fee(&frozen).await?;
        runtime.simulate(&frozen).await?;
        let bounds: BridgeBounds = serde_json::from_value(prepared.binding["bounds"].clone())
            .map_err(|_| internal_error())?;
        if prepared.binding["plan_digest"]
            != cctp::scope_digest(&quote, &bounds, &quote.intent.source_address)?
        {
            return Err(FinError::validation(
                "Solana mint's fixed protocol scope changed.",
            ));
        }
        let maximum_fee = prepared.binding["solana_maximum_network_fee_lamports"]
            .as_u64()
            .ok_or_else(internal_error)?;
        let total = maximum_fee
            .checked_add(state.account_funding_lamports)
            .ok_or_else(internal_error)?;
        let expected_intent = FinancialIntent {
            debits: vec![],
            maximum_costs: vec![CostBound {
                location: AccountingLocation::Cash {
                    resource: destination.wallet_resource.clone(),
                },
                amount: FinAmount {
                    asset: sol_asset(),
                    atoms: Quantity::from_u64(total),
                },
                treatment: CostTreatment::AdditionalDebit,
            }],
            expected: vec![],
            locks: vec![destination.wallet_resource.clone()],
        };
        if prepared.intent != expected_intent {
            return Err(FinError::validation(
                "Solana mint's gas and account-funding intent changed.",
            ));
        }
        if record.approved_bounds.as_ref() != Some(&prepared.binding["bounds"]) {
            return Err(FinError::validation(
                "Solana mint authority bounds changed.",
            ));
        }
        self.validate_solana_stage_caps(
            &record,
            prepared.binding["step_id"]
                .as_str()
                .ok_or_else(internal_error)?,
            &bounds,
            fee,
            state.account_funding_lamports,
        )?;
        self.validate_solana_usd_caps(&quote, &bounds, fee, state.account_funding_lamports)
            .await?;
        let mut fresh = prepared.clone();
        fresh.valuation = Some(self.value_intent(&fresh.intent).await?);
        Ok(fresh)
    }
    pub(super) async fn submit_cctp_solana_mint(
        &self,
        validated: &fin_domain::RevalidatedOperation,
        journal: Arc<dyn fin_domain::SubmissionJournal>,
    ) -> fin_domain::Result<fin_domain::Submission> {
        validated.assert_fresh()?;
        let prepared = validated.prepared();
        let frozen = self.freeze_cctp_solana(prepared).await?;
        let quote: BridgeQuote = serde_json::from_value(prepared.binding["quote"].clone())
            .map_err(|_| internal_error())?;
        let wallet = self.cctp_solana_wallet(
            quote
                .intent
                .solana_destination
                .as_ref()
                .ok_or_else(internal_error)?,
        )?;
        let signer = wallet.signer().ok_or_else(internal_error)?;
        validated.assert_fresh()?;
        wallet
            .runtime()
            .dispatch(&frozen, &signer, prepared, journal)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plan_mint_binding_preserves_case_sensitive_solana_scope() {
        let scope = json!({"destination":{"network":fin_solana::NETWORK,"wallet_address":"OwnerABC","recipient_token_account":"AtaXYZ"},"message_transmitter":"ProgramABC","token_messenger_minter":"ProgramXYZ"});
        let approved = json!({"network":fin_solana::NETWORK,"solana_mint":scope});
        let mut candidate = json!({"solana_mint":scope});
        assert!(bridge_step_execution_matches(&approved, &candidate));
        candidate["solana_mint"]["destination"]["wallet_address"] = json!("ownerabc");
        assert!(!bridge_step_execution_matches(&approved, &candidate));
        candidate["solana_mint"] = scope;
        candidate["call"] = json!({"chain_id":8453,"to":"0x1"});
        assert!(!bridge_step_execution_matches(&approved, &candidate));
        let evm = json!({"chain_id":8453,"to":"0xabc"});
        assert!(bridge_step_execution_matches(
            &evm,
            &json!({"call":{"chain_id":8453,"to":"0xABC"}})
        ));
        assert!(!bridge_step_execution_matches(&evm, &candidate));
    }
    #[test]
    fn solana_gas_and_nonce_funding_have_distinct_exact_caps() {
        let mut bounds = BridgeBounds::default();
        bounds.fee_caps.push(fin_bridge::FeeCap {
            asset: sol_asset(),
            kind: fin_bridge::FeeKind::Gas,
            maximum: Quantity::from_u64(5000),
            maximum_usd_micros: None,
        });
        assert_eq!(
            sol_cap(&bounds, fin_bridge::FeeKind::Gas).unwrap(),
            Quantity::from_u64(5000)
        );
        assert!(sol_cap(&bounds, fin_bridge::FeeKind::AdditionalDebit).is_err());
        bounds.fee_caps.push(fin_bridge::FeeCap {
            asset: EvmAsset {
                chain_id: 8453,
                token: None,
                decimals: 18,
            }
            .asset_id(),
            kind: fin_bridge::FeeKind::AdditionalDebit,
            maximum: Quantity::from_u64(1000000),
            maximum_usd_micros: None,
        });
        assert!(sol_cap(&bounds, fin_bridge::FeeKind::AdditionalDebit).is_err());
        bounds.fee_caps.push(fin_bridge::FeeCap {
            asset: sol_asset(),
            kind: fin_bridge::FeeKind::AdditionalDebit,
            maximum: Quantity::from_u64(953520),
            maximum_usd_micros: None,
        });
        assert_eq!(
            sol_cap(&bounds, fin_bridge::FeeKind::AdditionalDebit).unwrap(),
            Quantity::from_u64(953520)
        );
    }
    #[test]
    fn solana_mint_expiry_uses_destination_slot_and_requires_unused_nonce() {
        let message:cctp::CctpMessage=serde_json::from_value(json!({"source_domain":6,"destination_domain":5,"nonce":"unused","sender":"source","recipient":"messenger","destination_caller":"owner","min_finality_threshold":2000,"finality_threshold_executed":2000,"burn_token":"USDC","mint_recipient":"ATA","amount":"10000000","message_sender":"source","max_fee":"0","fee_executed":"0","expiration_block":"100"})).unwrap();
        let mut state = cctp::solana::SolanaCctpState {
            slot: 99,
            source_chain: 8453,
            source_domain: 6,
            used_nonce: false,
            nonce: Some("unused".into()),
            used_nonce_address: None,
            account_funding_lamports: 953520,
            recipient_exists: true,
            signature_threshold: 1,
            enabled_attesters: vec![],
            fee_recipient: "fee".into(),
            fee_recipient_token_account: "feeATA".into(),
            custody_balance: 10000000,
        };
        require_ready_state(&message, &state).unwrap();
        state.slot = 100;
        assert_eq!(
            require_ready_state(&message, &state).unwrap_err().code,
            "CCTP_ATTESTATION_EXPIRED"
        );
        state.slot = 99;
        state.used_nonce = true;
        assert_eq!(
            require_ready_state(&message, &state).unwrap_err().code,
            "CCTP_ALREADY_PROCESSED"
        );
        state.used_nonce = false;
        state.recipient_exists = false;
        assert_eq!(
            require_ready_state(&message, &state).unwrap_err().code,
            "CCTP_SOLANA_ATA_REQUIRED"
        );
    }
}
