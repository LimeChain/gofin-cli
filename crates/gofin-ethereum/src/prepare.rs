use super::*;
use abi::*;
use gofin_evm::{
    AuthorizedPermit, Eip2612Permit, Permit2PermitSingle, SignatureLayout,
    UnsignedPermitTransaction,
    alloy::{primitives::Bytes, sol_types::SolCall},
    quantity, rpc_bytes, u256,
};

/// Translate the shared financial bound failure using typed request values.
/// The shared validator still decides whether the request may proceed.
fn swap_validation_error(err: GoFinError, request: &gofin_amm::AmmSwapRequest) -> GoFinError {
    if err.code != "AMM_BOUND_EXCEEDED" {
        return err;
    }
    let observed = request.quote.price_impact_bps;
    let approved = request.maximum_price_impact_bps;
    if observed <= 10_000 && approved <= 10_000 && observed > approved {
        return error("PRICE_IMPACT_LIMIT", "validation", &format!(
            "Quoted price impact is {observed} bps, above the approved maximum of {approved} bps."
        )).with_details(json!({"observed_price_impact_bps":observed,"maximum_price_impact_bps":approved}));
    }
    let mut translated = error(
        "SLIPPAGE_LIMIT",
        "validation",
        "Minimum output is below the approved fresh quote floor or above its attainable output.",
    );
    translated.details = err.details;
    translated
}

fn quote_native_output(binding: &Binding) -> bool {
    binding
        .output
        .as_ref()
        .is_some_and(|asset| asset.reference == "native")
}

fn require_eoa_permit_signer(code: &[u8]) -> Result<()> {
    require(
        code.is_empty(),
        "ALLOWANCE_INSUFFICIENT",
        "Atomic EIP-2612 and Permit2 signatures require an EOA wallet; approve a finite allowance for this wallet first.",
    )
}

impl EthereumAdapter {
    pub(crate) async fn prepare_operation(
        &self,
        operation: &str,
        input: &Value,
        original: Option<&Binding>,
        context: &PreparationContext,
    ) -> Result<PreparedOperation> {
        if operation == "swap.execute" {
            return self.prepare_swap_execute(input, original, context).await;
        }
        self.require_operation(operation)?;
        if operation == "wallet.permit2.approve" {
            return self.prepare_permit2_approval(input, None, context).await;
        }
        // The protocol preparation path already validates the shared AMM
        // request against its pinned quote. Do not obtain a second quote in a
        // wrapper before repeating the same checks here.
        self.prepare_operation_inner(operation, input, original, context)
            .await
    }

    /// Execute one agent-nominated concrete swap through a stable intent
    /// operation. The original DEX implementation remains responsible for
    /// quoting, simulation, calldata, and the persisted concrete binding.
    async fn prepare_swap_execute(
        &self,
        input: &Value,
        original: Option<&Binding>,
        context: &PreparationContext,
    ) -> Result<PreparedOperation> {
        let request = request::parse("swap.execute", input)?;
        let dex = request
            .dex
            .as_deref()
            .ok_or_else(|| GoFinError::validation("Execution must nominate one supported DEX."))?;
        let concrete = super::amm_permissions::swap_operation_for_dex(dex)?;
        let concrete_input = request::concrete_swap_execute_input(dex, input)?;
        let mut concrete_original = original.cloned();
        if dex == "uniswap" {
            // The stable wrapper persists the nominated DEX in `Binding.dex`
            // so restart recovery can verify the selected operation. Ordinary
            // Uniswap bindings identify their venue by operation and leave
            // this field empty, so remove only the wrapper marker while the
            // concrete preparation compares its immutable envelope.
            if let Some(binding) = concrete_original.as_mut() {
                binding.dex = None;
            }
        }
        let mut prepared = self
            .prepare_operation_inner(
                concrete,
                &concrete_input,
                concrete_original.as_ref(),
                context,
            )
            .await?;
        let mut binding: Binding = serde_json::from_value(prepared.binding.clone())
            .map_err(|_| GoFinError::validation("Prepared swap binding is invalid."))?;
        let binding_dex = binding.dex.as_deref().unwrap_or_else(|| {
            if concrete == "uniswap.swap" {
                "uniswap"
            } else {
                ""
            }
        });
        if binding_dex != dex {
            return Err(GoFinError::validation(
                "Prepared swap binding differs from the nominated DEX.",
            ));
        }
        // The stable operation and full candidate input are fingerprinted by
        // the engine; the binding keeps the concrete DEX route for recovery.
        binding.dex = Some(dex.to_owned());
        prepared.operation = "swap.execute".into();
        prepared.input = input.clone();
        prepared.binding = serde_json::to_value(binding)
            .map_err(|_| GoFinError::validation("Prepared swap binding could not be encoded."))?;
        Ok(prepared)
    }

    async fn prepare_operation_inner(
        &self,
        operation: &str,
        input: &Value,
        original: Option<&Binding>,
        context: &PreparationContext,
    ) -> Result<PreparedOperation> {
        require(
            !request::is_read(operation),
            "WRITE_OPERATION_REQUIRED",
            "Read operations have no financial execution preview.",
        )?;
        self.require_operation(operation)?;
        let r = request::parse(operation, input)?;
        let w = self.resolve(&r, true)?;
        let owner = address(&w.address)?;
        self.runtime.assert_chain().await?;
        let token_ref = if matches!(operation, "wallet.wrap" | "wallet.unwrap") {
            self.resolved_weth(None).await?
        } else if matches!(
            operation,
            "uniswap.swap" | "ramses.swap" | "sushi.swap" | "pancakeswap.swap"
        ) {
            r.token_in.as_deref().expect("schema").to_owned()
        } else {
            r.token.as_deref().expect("schema").to_owned()
        };
        let token = self.token(&token_ref, None).await?;
        let all_amount = r.amount.as_deref() == Some("all");
        let repay_all = operation == "aave.repay" && all_amount;
        let withdraw_all = operation == "aave.withdraw" && all_amount;
        let mut qty = if operation == "aave.collateral" {
            U256::ZERO
        } else {
            exact(
                if all_amount {
                    r.max_amount.as_deref().expect("schema")
                } else {
                    r.amount.as_deref().expect("schema")
                },
                token.decimals,
            )?
        };
        if matches!(operation, "aave.repay" | "aave.withdraw") && all_amount {
            require(
                qty < U256::MAX,
                "ALL_AMOUNT_CAP_INVALID",
                "Aave full repayment and withdrawal require a finite max_amount below uint256 maximum.",
            )?;
        }
        if all_amount
            && matches!(
                operation,
                "wallet.unwrap"
                    | "uniswap.swap"
                    | "ramses.swap"
                    | "sushi.swap"
                    | "pancakeswap.swap"
            )
        {
            let observed = self.balance_of(&token, owner, None).await?;
            require(
                observed > U256::ZERO && observed <= qty,
                "SWEEP_MAXIMUM",
                "Complete observed token balance must fit finite max_amount.",
            )?;
            qty = observed;
        }
        require(
            qty > U256::ZERO || matches!(operation, "wallet.approve" | "aave.collateral"),
            "ZERO_AMOUNT",
            "Movement amount must be positive.",
        )?;
        let principal_asset = if operation == "wallet.wrap" {
            self.native_asset()
        } else {
            token.asset(self)
        };
        let resource = resource(self.config.chain_id, owner);
        let mut intent = FinancialIntent::default();
        intent.locks.push(resource.clone());
        let placeholder = FrozenTransaction {
            rollup_fee: None,
            chain_id: self.config.chain_id,
            sender: w.address.clone(),
            to: w.address.clone(),
            value: Quantity::zero(),
            calldata: "0x".into(),
            nonce: 0,
            gas: 1,
            max_fee_per_gas: Quantity::zero(),
            max_priority_fee_per_gas: Quantity::zero(),
        };
        let mut b = Binding {
            transaction: placeholder,
            resource: resource.clone(),
            token: principal_asset.clone(),
            units: quantity(qty),
            recipient: None,
            spender: None,
            output: None,
            minimum_output: None,
            deadline: None,
            pool: None,
            fee_tier: None,
            dex: None,
            tick_spacing: None,
            pool_deployer: None,
            factory: None,
            swap_fee_ppm: None,
            max_pool_fee_ppm: None,
            protocol: None,
            v4_pool_key: None,
            route_token_in: None,
            route_token_out: None,
            quote_output: None,
            price_impact_bps: None,
            permit_template: None,
            permit2_allowance_before: None,
            allowance_before: None,
            supplied_before: None,
            debt_before: None,
            collateral_before: None,
            enabled: None,
            a_token: None,
            debt_token: None,
            scaled_before: None,
            repay_all,
            all_amount,
            aave_emode: None,
            supply_anchor: None,
            debt_anchor: None,
            debt_rounding_maximum: None,
            aave_proxy_identity: None,
            verified_baseline_anchors: original
                .map(|old| old.verified_baseline_anchors.clone())
                .unwrap_or_default(),
            value_bounds: original.and_then(|binding| binding.value_bounds.clone()),
            boundary_authorization: original
                .and_then(|binding| binding.boundary_authorization.clone()),
            financial_plan: original.and_then(|binding| binding.financial_plan.clone()),
        };
        let mut warnings =
            vec!["An uncertain transaction must be reconciled without resubmission.".into()];
        let mut shared_swap: Option<gofin_amm::AmmSwapRequest> = None;
        let mut swap_output_symbol: Option<String> = None;
        let mut swap_permission: Option<String> = None;
        let mut to = address(&token.reference).unwrap_or(owner);
        let mut value = U256::ZERO;
        let mut permit2_authorization: Option<AuthorizedPermit> = None;
        let mut eip2612_authorization: Option<AuthorizedPermit> = None;
        let mut calldata: Bytes = match operation {
            "wallet.wrap" => {
                require(
                    token.decimals == 18,
                    "WETH_PRECISION_INVALID",
                    "Configured WETH must have 18 decimals.",
                )?;
                value = qty;
                intent.debits.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: amount(&principal_asset, qty),
                });
                let output = token.asset(self);
                b.output = Some(output.clone());
                intent.expected.push(EffectConstraint::Conversion {
                    resource: resource.clone(),
                    input: principal_asset.clone(),
                    maximum_input: quantity(qty),
                    output,
                    minimum_output: quantity(qty),
                });
                Weth::depositCall {}.abi_encode().into()
            }
            "wallet.unwrap" => {
                require(
                    token.decimals == 18,
                    "WETH_PRECISION_INVALID",
                    "Configured WETH must have 18 decimals.",
                )?;
                b.output = Some(self.native_asset());
                intent.debits.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: amount(&principal_asset, qty),
                });
                intent.expected.push(EffectConstraint::Conversion {
                    resource: resource.clone(),
                    input: principal_asset.clone(),
                    maximum_input: quantity(qty),
                    output: self.native_asset(),
                    minimum_output: quantity(qty),
                });
                Weth::withdrawCall { amount: qty }.abi_encode().into()
            }
            "wallet.transfer" => {
                let recipient = address(r.recipient.as_deref().expect("schema"))?;
                require(
                    recipient != Address::ZERO && recipient != owner,
                    "RECIPIENT_INVALID",
                    "Recipient must be a different explicit nonzero address.",
                )?;
                b.recipient = Some(lower_address(recipient));
                intent.debits.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: amount(&principal_asset, qty),
                });
                let target = super::resource(self.config.chain_id, recipient);
                intent.locks.push(target.clone());
                intent.expected.push(EffectConstraint::Transfer {
                    from: resource.clone(),
                    to: target,
                    debit_asset: principal_asset.clone(),
                    credit_asset: principal_asset.clone(),
                    maximum_debit: quantity(qty),
                    minimum_credit: quantity(qty),
                });
                if token.reference == "native" {
                    let code = rpc_bytes(
                        &self
                            .runtime
                            .rpc
                            .request("eth_getCode", json!([lower_address(recipient), "latest"]))
                            .await?,
                    )?;
                    require(
                        code.is_empty(),
                        "NATIVE_CONTRACT_RECIPIENT_UNSUPPORTED",
                        "Native transfers support explicitly verified externally-owned recipients in this profile.",
                    )?;
                    to = recipient;
                    value = qty;
                    Bytes::new()
                } else {
                    Erc20::transferCall {
                        to: recipient,
                        amount: qty,
                    }
                    .abi_encode()
                    .into()
                }
            }
            "wallet.approve" => {
                require(
                    token.reference != "native" && qty < U256::MAX,
                    "UNBOUNDED_ALLOWANCE",
                    "Use an explicit finite ERC-20 allowance; maximum uint256 approval is unsupported.",
                )?;
                let spender = address(r.spender.as_deref().expect("schema"))?;
                require(
                    spender != Address::ZERO,
                    "ZERO_ADDRESS",
                    "Spender must be nonzero.",
                )?;
                self.runtime.code(spender).await?;
                let before = self.allowance(&token, owner, spender, None).await?;
                b.spender = Some(lower_address(spender));
                b.allowance_before = Some(quantity(before));
                intent.expected.push(EffectConstraint::Authority {
                    resource: resource.clone(),
                    authority: authority(&token.reference, b.spender.as_deref().expect("spender")),
                    before: before.to_string(),
                    after: qty.to_string(),
                });
                warnings.push(
                    "Allowance persists until another approved write restores or revokes it."
                        .into(),
                );
                Erc20::approveCall {
                    spender,
                    amount: qty,
                }
                .abi_encode()
                .into()
            }
            "uniswap.swap" | "ramses.swap" | "sushi.swap" | "pancakeswap.swap" => {
                let is_ramses = operation == "ramses.swap";
                let quote = self
                    .quote_protocol(reads::ProtocolQuoteRequest {
                        dex: super::uniswap::profile::operation_dex(operation)?,
                        input: &token.reference,
                        output: r.token_out.as_deref().expect("schema"),
                        amount: &quantity(qty).to_decimal(token.decimals),
                        fee_tier: original.and_then(|b| b.fee_tier).or(r.fee_tier),
                        tick_spacing: r.tick_spacing,
                        protocol: original
                            .and_then(|b| b.protocol.as_deref())
                            .or(r.protocol.as_deref()),
                        v4_key: original
                            .and_then(|b| b.v4_pool_key.as_ref())
                            .or(r.v4_pool_key.as_ref()),
                        block: None,
                    })
                    .await?;
                swap_output_symbol = Some(quote.token_out.symbol.clone());
                let quote_block = quote.block_number.expect("pinned quote block");
                let quote_block_tag = format!("{quote_block:#x}");
                let recipient = address(r.recipient.as_deref().expect("schema"))?;
                require(
                    recipient != Address::ZERO,
                    "ZERO_ADDRESS",
                    "Swap recipient must be nonzero.",
                )?;
                // Cross-custody swaps require two independent effects and are deliberately outside this beta profile.
                require(
                    recipient == owner,
                    "SWAP_RECIPIENT_UNSUPPORTED",
                    "This bounded swap profile returns output to the signing wallet.",
                )?;
                let maximum_fee = exact(r.max_fee_eth.as_deref().expect("schema"), 18)?;
                let permission = self.swap_permission_requirement(owner, &quote).await?;
                let amm_quote = super::uniswap::as_amm_quote(
                    self,
                    resource.clone(),
                    &quote,
                    quantity(maximum_fee),
                    permission,
                )?;
                let selected_slippage = r.slippage_bps.expect("schema");
                let absolute_minimum = exact(
                    r.min_output.as_deref().expect("schema"),
                    quote.token_out.decimals,
                )?;
                let amm_request = gofin_amm::AmmSwapRequest {
                    resource: resource.clone(),
                    approved_resource: resource.clone(),
                    approved_route: amm_quote.route.clone(),
                    quote: amm_quote.clone(),
                    selected_slippage_bps: selected_slippage,
                    approved_slippage_bps: selected_slippage,
                    rate_floor: None,
                    absolute_minimum_output: Some(quantity(absolute_minimum)),
                    input_cap: amount(&principal_asset, qty),
                    fee_cap: amm_quote.maximum_fee.clone(),
                    maximum_price_impact_bps: r.max_price_impact_bps.unwrap_or(10_000),
                };
                let validated = gofin_amm::validate_swap(&amm_request, self.clock.now())
                    .map_err(|err| swap_validation_error(err, &amm_request))?;
                let fresh_minimum = u256(&validated)?;
                shared_swap = Some(amm_request);
                let quote_output = u256(&quote.output)?;
                let max_pool_fee = if is_ramses {
                    let observed = quote.swap_fee_ppm.ok_or_else(|| {
                        error(
                            "POOL_FEE_UNAVAILABLE",
                            "preview",
                            "Ramses dynamic pool fee was not observed.",
                        )
                    })?;
                    let bound = original
                        .and_then(|binding| binding.max_pool_fee_ppm)
                        .or(r.max_pool_fee_ppm)
                        .or_else(|| original.and_then(|binding| binding.swap_fee_ppm))
                        .unwrap_or(observed);
                    require(
                        observed <= bound,
                        "POOL_FEE_LIMIT",
                        "Pinned Ramses pool fee exceeds the approved quote ceiling; create a fresh bounded preview.",
                    )?;
                    Some(bound)
                } else {
                    None
                };
                // Slippage is measured from the quote that fixed this exact
                // transaction. A better live quote must not raise its encoded
                // minimum; a worse quote must still cover that same minimum.
                let reference_output = original
                    .map(|binding| {
                        binding.quote_output.as_ref().ok_or_else(|| {
                            error(
                                "BINDING_INVALID",
                                "validation",
                                "Approved swap quote output is missing.",
                            )
                        })
                    })
                    .transpose()?
                    .map(u256)
                    .transpose()?
                    .unwrap_or(quote_output);
                let floor = mul(reference_output, U256::from(10000 - selected_slippage))?
                    / U256::from(10000);
                let minimum = if let Some(original) = original {
                    u256(original.minimum_output.as_ref().ok_or_else(|| {
                        error(
                            "BINDING_INVALID",
                            "validation",
                            "Approved swap minimum output is missing.",
                        )
                    })?)?
                } else {
                    fresh_minimum
                };
                require(
                    quote.price_impact_bps <= r.max_price_impact_bps.unwrap_or(10_000),
                    "PRICE_IMPACT_LIMIT",
                    "Quoted spot price impact exceeds the explicit maximum.",
                )?;
                if minimum == U256::ZERO
                    || minimum < floor
                    || minimum < absolute_minimum
                    || minimum > quote_output
                {
                    return Err(error("SLIPPAGE_LIMIT", "validation",
                        "The quote must cover the fixed minimum output, including the original slippage and explicit receipt floors.")
                        .with_details(json!({"reason":if minimum > quote_output { "minimum_output_unattainable" } else { "approved_floor_violated" },
                            "minimum_output_atoms":minimum.to_string(), "quote_output_atoms":quote_output.to_string(),
                            "slippage_reference_output_atoms":reference_output.to_string(), "slippage_floor_atoms":floor.to_string(),
                            "explicit_minimum_output_atoms":absolute_minimum.to_string()})));
                }
                let deadline = r.deadline.expect("schema");
                let current = self.clock.now().timestamp() as u64;
                require(
                    deadline > current && deadline <= current + 600,
                    "DEADLINE_INVALID",
                    "Swap deadline must be in the next ten minutes.",
                )?;
                let protocol = super::uniswap::Protocol::parse(Some(&quote.protocol))?;
                let router = if is_ramses {
                    address(
                        super::uniswap::profile::V3DeploymentProfile::ramses(self.ramses()?).router,
                    )?
                } else if protocol == super::uniswap::Protocol::V4 {
                    address(
                        &self
                            .uniswap()?
                            .v4
                            .as_ref()
                            .ok_or_else(|| {
                                error(
                                    "OPERATION_UNAVAILABLE",
                                    "validation",
                                    "Uniswap v4 is not configured on this network.",
                                )
                            })?
                            .router,
                    )?
                } else {
                    let profile =
                        self.v3_profile(super::uniswap::profile::operation_dex(operation)?)?;
                    address(profile.router)?
                };
                self.runtime.code_at(router, Some(quote_block)).await?;
                if token.reference != "native" {
                    if protocol == super::uniswap::Protocol::V4 {
                        // Universal Router v4 pulls ERC-20 input through the
                        // canonical Permit2 contract. The token's underlying
                        // allowance and Permit2's router allowance are
                        // independently checked before selecting a typed permit.
                        let permit2 = address("0x000000000022d473030f116ddee9f6b43ac78ba3")?;
                        self.runtime.code_at(permit2, Some(quote_block)).await?;
                        let underlying = self
                            .allowance(&token, owner, permit2, Some(quote_block))
                            .await?;
                        require(
                            underlying >= qty && underlying < U256::MAX,
                            "ALLOWANCE_INSUFFICIENT",
                            "Uniswap v4 requires a finite token allowance to canonical Permit2.",
                        )?;
                        let allowance = Permit2::allowanceCall {
                            owner,
                            token: address(&token.reference)?,
                            spender: router,
                        }
                        .abi_encode();
                        let result = self.runtime.rpc.request("eth_call", json!([{"to":lower_address(permit2),"data":gofin_evm::hex_bytes(&allowance)},quote_block_tag])).await?;
                        let allowance =
                            Permit2::allowanceCall::abi_decode_returns(&rpc_bytes(&result)?)
                                .map_err(|_| {
                                    error(
                                        "PERMIT2_ALLOWANCE_INVALID",
                                        "preview",
                                        "Permit2 returned malformed allowance state.",
                                    )
                                })?;
                        let amount = allowance.amount;
                        let expiration = allowance.expiration;
                        let nonce = allowance.nonce;
                        b.permit2_allowance_before = Some(Permit2AllowanceState {
                            amount: quantity(U256::from(amount)),
                            expiration: expiration.to::<u64>(),
                            nonce: nonce.to::<u64>(),
                        });
                        let timestamp = self.clock.now().timestamp().max(0) as u64;
                        if U256::from(amount) >= qty
                            && amount < gofin_evm::alloy::primitives::aliases::U160::MAX
                            && expiration.to::<u64>() > timestamp
                        {
                            // A live finite Permit2 allowance needs no new signature.
                        } else {
                            let owner_code = rpc_bytes(
                                &self
                                    .runtime
                                    .rpc
                                    .request(
                                        "eth_getCode",
                                        json!([lower_address(owner), quote_block_tag]),
                                    )
                                    .await?,
                            )?;
                            require_eoa_permit_signer(&owner_code)?;
                            require(
                                nonce < gofin_evm::alloy::primitives::aliases::U48::MAX,
                                "PERMIT2_NONCE_EXHAUSTED",
                                "Permit2 nonce cannot be incremented safely.",
                            )?;
                            let authorization = if let Some(existing) =
                                original.and_then(|b| b.permit_template.as_ref())
                            {
                                existing.permit().clone()
                            } else {
                                AuthorizedPermit::Permit2Single(Permit2PermitSingle {
                                    chain_id: self.config.chain_id,
                                    verifying_contract: lower_address(permit2),
                                    owner: lower_address(owner),
                                    token: lower_address(address(&token.reference)?),
                                    amount: quantity(qty),
                                    expiration: deadline,
                                    nonce: nonce.to::<u64>(),
                                    spender: lower_address(router),
                                    sig_deadline: deadline,
                                })
                            };
                            match &authorization {
                                AuthorizedPermit::Permit2Single(p) => require(
                                    p.chain_id == self.config.chain_id
                                        && p.verifying_contract == lower_address(permit2)
                                        && p.owner == lower_address(owner)
                                        && p.token == lower_address(address(&token.reference)?)
                                        && p.amount == quantity(qty)
                                        && p.expiration == deadline
                                        && p.nonce == nonce.to::<u64>()
                                        && p.spender == lower_address(router)
                                        && p.sig_deadline == deadline,
                                    "PERMIT_STATE_CHANGED",
                                    "Typed Permit2 authorization differs from the exact swap scope.",
                                )?,
                                _ => {
                                    return Err(error(
                                        "PERMIT_STATE_CHANGED",
                                        "preview",
                                        "The stored permit type does not match the v4 Permit2 route.",
                                    ));
                                }
                            }
                            permit2_authorization = Some(authorization);
                        }
                    } else {
                        let allowance = self
                            .allowance(&token, owner, router, Some(quote_block))
                            .await?;
                        b.allowance_before = Some(quantity(allowance));
                        if !(allowance >= qty && allowance < U256::MAX) {
                            let token_address = address(&token.reference)?;
                            let owner_code = rpc_bytes(
                                &self
                                    .runtime
                                    .rpc
                                    .request(
                                        "eth_getCode",
                                        json!([lower_address(owner), quote_block_tag]),
                                    )
                                    .await?,
                            )?;
                            require_eoa_permit_signer(&owner_code)?;
                            let permit_unavailable = || {
                                error(
                                    "ALLOWANCE_INSUFFICIENT",
                                    "preview",
                                    "No finite router allowance exists and the token does not expose a standard EIP-2612 domain bound to this chain and token; approve a finite allowance first.",
                                )
                            };
                            let (nonce, domain) = self
                                .eip2612_nonce_at(token_address, owner, quote_block)
                                .await
                                .ok_or_else(permit_unavailable)?;
                            require(
                                nonce < U256::MAX,
                                "PERMIT_NONCE_EXHAUSTED",
                                "EIP-2612 nonce cannot be incremented safely.",
                            )?;
                            let authorization = if let Some(existing) =
                                original.and_then(|b| b.permit_template.as_ref())
                            {
                                existing.permit().clone()
                            } else {
                                AuthorizedPermit::Eip2612(Eip2612Permit {
                                    chain_id: self.config.chain_id,
                                    token: lower_address(token_address),
                                    owner: lower_address(owner),
                                    spender: lower_address(router),
                                    value: quantity(qty),
                                    nonce: quantity(nonce),
                                    deadline,
                                    domain_separator: format!("{domain:#x}"),
                                })
                            };
                            match &authorization {
                                AuthorizedPermit::Eip2612(p) => require(
                                    p.chain_id == self.config.chain_id
                                        && p.token == lower_address(token_address)
                                        && p.owner == lower_address(owner)
                                        && p.spender == lower_address(router)
                                        && p.value == quantity(qty)
                                        && p.nonce == quantity(nonce)
                                        && p.deadline == deadline
                                        && p.domain_separator == format!("{domain:#x}"),
                                    "PERMIT_STATE_CHANGED",
                                    "Typed EIP-2612 authorization differs from the exact swap scope.",
                                )?,
                                _ => {
                                    return Err(error(
                                        "PERMIT_STATE_CHANGED",
                                        "preview",
                                        "The stored permit type does not match the v3 EIP-2612 route.",
                                    ));
                                }
                            }
                            eip2612_authorization = Some(authorization);
                            intent.expected.push(EffectConstraint::Authority {
                                resource: resource.clone(),
                                authority: authority(&token.reference, &lower_address(router)),
                                before: quantity(allowance).to_string(),
                                after: Quantity::zero().to_string(),
                            });
                        }
                    }
                }
                to = router;
                b.spender = Some(lower_address(router));
                b.recipient = Some(lower_address(recipient));
                b.output = Some(quote.token_out.asset(self));
                b.minimum_output = Some(quantity(minimum));
                b.deadline = Some(deadline);
                b.pool = Some(quote.pool);
                // Ramses identifies pools by tick spacing; `fee()` is a
                // mutable quote observation and must not enter the fixed
                // fee-tier binding field.
                b.fee_tier = (!is_ramses).then_some(quote.fee_tier);
                if super::uniswap::profile::additional_dex(operation).is_some() {
                    let profile =
                        self.v3_profile(super::uniswap::profile::operation_dex(operation)?)?;
                    b.dex = Some(profile.dex_id.into());
                    b.factory = Some(profile.factory.to_owned());
                    b.pool_deployer = profile.pool_deployer.map(str::to_owned);
                }
                if is_ramses {
                    b.dex = Some("ramses".into());
                    b.tick_spacing = quote.tick_spacing;
                    b.pool_deployer = Some(self.ramses()?.pool_deployer.clone());
                    b.factory = Some(self.ramses()?.factory.clone());
                    b.swap_fee_ppm = quote.swap_fee_ppm;
                    b.max_pool_fee_ppm = max_pool_fee;
                }
                b.protocol = Some(quote.protocol.clone());
                b.v4_pool_key = quote.pool_key.clone();
                b.route_token_in = Some(quote.pool_token_in.clone());
                b.route_token_out = Some(quote.pool_token_out.clone());
                b.quote_output = Some(quantity(quote_output));
                b.price_impact_bps = Some(quote.price_impact_bps);
                intent.debits.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: amount(&principal_asset, qty),
                });
                intent.expected.push(EffectConstraint::Conversion {
                    resource: resource.clone(),
                    input: principal_asset.clone(),
                    maximum_input: quantity(qty),
                    output: b.output.clone().expect("output"),
                    minimum_output: quantity(minimum),
                });
                if is_ramses {
                    let mut call = super::uniswap::ramses::exact_input_single(
                        super::uniswap::ramses::ExactInputSingle {
                            router,
                            token_in: address(&quote.pool_token_in)?,
                            token_out: address(&quote.pool_token_out)?,
                            tick_spacing: quote.tick_spacing.expect("Ramses quote spacing"),
                            wallet: recipient,
                            deadline,
                            input: qty,
                            minimum,
                            native_in: token.reference == "native",
                            native_out: quote.token_out.reference == "native",
                        },
                    )?;
                    if let Some(AuthorizedPermit::Eip2612(permit)) = &eip2612_authorization {
                        let (with_permit, layout) = super::uniswap::ramses::with_eip2612(
                            call.calldata,
                            address(&permit.token)?,
                            qty,
                            deadline,
                        )?;
                        call.calldata = with_permit;
                        let request = UnsignedPermitTransaction {
                            sender: owner,
                            to,
                            value: call.value,
                            calldata: call.calldata.clone(),
                            gas_limit: 500_000,
                            maximum_total_fee: exact(
                                r.max_fee_eth.as_deref().expect("schema"),
                                18,
                            )?,
                        };
                        let authorization = AuthorizedPermit::Eip2612(permit.clone());
                        let template = if let Some(existing) =
                            original.and_then(|binding| binding.permit_template.as_ref())
                        {
                            self.runtime
                                .revalidate_permit_template(
                                    existing,
                                    request,
                                    layout,
                                    authorization,
                                )
                                .await?
                        } else {
                            self.runtime
                                .prepare_permit_template_with_layout(request, layout, authorization)
                                .await?
                        };
                        b.permit_template = Some(template);
                    }
                    value = call.value;
                    call.calldata
                } else if protocol == super::uniswap::Protocol::V3 {
                    let profile =
                        self.v3_profile(super::uniswap::profile::operation_dex(operation)?)?;
                    let token_in = address(&quote.pool_token_in)?;
                    let token_out = address(&quote.pool_token_out)?;
                    let mut call = super::uniswap::v3::exact_input_single(
                        super::uniswap::v3::ExactInputSingle {
                            kind: profile.router_kind,
                            router,
                            token_in,
                            token_out,
                            fee: quote.fee_tier,
                            wallet: recipient,
                            deadline,
                            input: qty,
                            minimum,
                            native_in: token.reference == "native",
                            native_out: quote.token_out.reference == "native",
                        },
                    )?;
                    if profile.router_metadata_abi == V3RouterMetadataAbi::PancakeV3
                        && profile.router_kind == RouterKind::SwapRouter02
                    {
                        call.calldata = super::uniswap::v3::with_refund(call.calldata)?;
                    }
                    if let Some(AuthorizedPermit::Eip2612(permit)) = &eip2612_authorization {
                        let (with_permit, layout) = super::uniswap::v3::with_eip2612(
                            profile.router_kind,
                            call.calldata,
                            address(&permit.token)?,
                            qty,
                            deadline,
                        )?;
                        call.calldata = with_permit;
                        let request = UnsignedPermitTransaction {
                            sender: owner,
                            to,
                            value: call.value,
                            calldata: call.calldata.clone(),
                            gas_limit: 500_000,
                            maximum_total_fee: exact(
                                r.max_fee_eth.as_deref().expect("schema"),
                                18,
                            )?,
                        };
                        let authorization = AuthorizedPermit::Eip2612(permit.clone());
                        let template = if let Some(existing) =
                            original.and_then(|binding| binding.permit_template.as_ref())
                        {
                            self.runtime
                                .revalidate_permit_template(
                                    existing,
                                    request,
                                    layout,
                                    authorization,
                                )
                                .await?
                        } else {
                            self.runtime
                                .prepare_permit_template_with_layout(request, layout, authorization)
                                .await?
                        };
                        b.permit_template = Some(template);
                    }
                    value = call.value;
                    call.calldata
                } else {
                    let key = quote.pool_key.as_ref().expect("validated v4 key");
                    let (mut calldata, native_value) = super::uniswap::v4::exact_input_single(
                        super::uniswap::v4::ExactInputSingle {
                            key,
                            input_asset: address(&quote.pool_token_in)?,
                            output_asset: address(&quote.pool_token_out)?,
                            wallet: owner,
                            amount_in: qty,
                            minimum,
                            deadline,
                            has_hop_price: self
                                .uniswap()?
                                .v4
                                .as_ref()
                                .expect("v4 config")
                                .router_has_hop_price,
                            native_in: token.reference == "native",
                        },
                    )?;
                    if let Some(AuthorizedPermit::Permit2Single(permit)) = &permit2_authorization {
                        let (with_permit, offset) =
                            super::uniswap::v4::with_permit2(calldata, permit)?;
                        calldata = with_permit;
                        // The runtime replaces this exact signature slot only
                        // after durable authorization.
                        let request = UnsignedPermitTransaction {
                            sender: owner,
                            to,
                            value: native_value,
                            calldata: calldata.clone(),
                            gas_limit: 650_000,
                            maximum_total_fee: exact(
                                r.max_fee_eth.as_deref().expect("schema"),
                                18,
                            )?,
                        };
                        let layout = SignatureLayout::Packed65 { offset };
                        let authorization = AuthorizedPermit::Permit2Single(permit.clone());
                        let template = if let Some(existing) =
                            original.and_then(|binding| binding.permit_template.as_ref())
                        {
                            self.runtime
                                .revalidate_permit_template(
                                    existing,
                                    request,
                                    layout,
                                    authorization,
                                )
                                .await?
                        } else {
                            self.runtime
                                .prepare_permit_template_with_layout(request, layout, authorization)
                                .await?
                        };
                        b.permit_template = Some(template);
                    }
                    value = native_value;
                    calldata
                }
            }
            _ => {
                let reserve = self.reserve(&token.reference, None).await?;
                require(
                    reserve.active && !reserve.paused,
                    "RESERVE_UNAVAILABLE",
                    "Aave reserve must be active and unpaused.",
                )?;
                let user = self.user_reserve(&token, owner, None).await?;
                let pool = address(&self.aave()?.pool)?;
                to = pool;
                b.aave_emode = Some(quantity(self.aave_mode(owner, None).await?));
                b.pool = Some(self.aave()?.pool.clone());
                b.spender = Some(self.aave()?.pool.clone());
                b.a_token = Some(reserve.a_token.clone());
                b.debt_token = Some(reserve.variable_debt_token.clone());
                b.supplied_before = Some(user.supplied.clone());
                b.debt_before = Some(user.variable_debt.clone());
                b.collateral_before = Some(user.collateral_enabled);
                let asset = address(&token.reference)?;
                if matches!(operation, "aave.supply" | "aave.withdraw") {
                    require(
                        self.runtime
                            .call(
                                address(&reserve.a_token)?,
                                &AToken::ATOKEN_REVISIONCall {},
                                None,
                            )
                            .await?
                            == self.supply_revision(),
                        "ATOKEN_REVISION_UNSUPPORTED",
                        "Selected supply writes require reviewed aToken revision 5 backing math.",
                    )?;
                }
                if matches!(operation, "aave.borrow" | "aave.repay") {
                    require(
                        self.runtime
                            .call(
                                address(&reserve.variable_debt_token)?,
                                &AToken::DEBT_TOKEN_REVISIONCall {},
                                None,
                            )
                            .await?
                            == self.debt_revision(),
                        "DEBT_REVISION_UNSUPPORTED",
                        "Selected debt writes require the selected reviewed debt token backing math.",
                    )?;
                }
                self.verify_receipt_identity(&token, &reserve, None).await?;
                b.supply_anchor = Some(
                    self.context_anchor(
                        context,
                        &token,
                        owner,
                        address(&reserve.a_token)?,
                        PositionKind::Supply,
                        original.is_none(),
                    )
                    .await?,
                );
                b.debt_anchor = Some(
                    self.context_anchor(
                        context,
                        &token,
                        owner,
                        address(&reserve.variable_debt_token)?,
                        PositionKind::VariableDebt,
                        original.is_none(),
                    )
                    .await?,
                );
                if original.is_none() {
                    let relevant = match operation {
                        "aave.borrow" | "aave.repay" => b.debt_anchor.as_ref(),
                        "aave.supply" | "aave.withdraw" => b.supply_anchor.as_ref(),
                        _ => None,
                    };
                    if let Some(anchor) = relevant
                        && !context.positions.iter().any(|old| {
                            old.resource == anchor.resource
                                && old.position == anchor.position
                                && old.position_kind == anchor.position_kind
                                && old.principal.asset == anchor.principal.asset
                        })
                    {
                        b.verified_baseline_anchors.push(anchor.clone());
                    }
                }
                require(
                    b.supply_anchor
                        .as_ref()
                        .expect("anchor")
                        .scaled_receipt
                        .as_ref()
                        == Some(&quantity(
                            self.runtime
                                .call(
                                    address(&reserve.a_token)?,
                                    &AToken::scaledBalanceOfCall { user: owner },
                                    None,
                                )
                                .await?,
                        ))
                        && b.debt_anchor
                            .as_ref()
                            .expect("anchor")
                            .scaled_receipt
                            .as_ref()
                            == Some(&user.scaled_variable_debt),
                    "POSITION_PRINCIPAL_CHANGED",
                    "Current scaled principal differs from broker-proved prior anchor.",
                )?;
                let supply_position = position(&self.aave()?.pool, &token.reference, "supply");
                let debt_position = position(&self.aave()?.pool, &token.reference, "variable-debt");
                match operation {
                    "aave.supply" => {
                        require(
                            !reserve.frozen,
                            "RESERVE_FROZEN",
                            "Frozen reserves cannot accept supply.",
                        )?;
                        let cap = mul(u256(&reserve.supply_cap)?, pow10(token.decimals)?)?;
                        require(
                            cap.is_zero() || add(u256(&reserve.total_supply)?, qty)? <= cap,
                            "SUPPLY_CAP",
                            "Supply exceeds the observed reserve cap.",
                        )?;
                        require(
                            {
                                let allowance = self.allowance(&token, owner, pool, None).await?;
                                allowance >= qty && allowance < U256::MAX
                            },
                            "ALLOWANCE_INSUFFICIENT",
                            "Finite Pool allowance must cover exact supply.",
                        )?;
                        let a_token = address(&reserve.a_token)?;
                        require(
                            self.runtime
                                .call(a_token, &AToken::ATOKEN_REVISIONCall {}, None)
                                .await?
                                == self.supply_revision(),
                            "ATOKEN_REVISION_UNSUPPORTED",
                            "Exact supply backing proof supports the selected reviewed aToken revision.",
                        )?;
                        b.scaled_before = Some(quantity(
                            self.runtime
                                .call(a_token, &AToken::scaledBalanceOfCall { user: owner }, None)
                                .await?,
                        ));
                        intent.debits.push(ResourceAmount {
                            resource: resource.clone(),
                            amount: amount(&principal_asset, qty),
                        });
                        intent.expected.push(EffectConstraint::Debit {
                            resource: resource.clone(),
                            asset: principal_asset.clone(),
                            maximum_debit: quantity(qty),
                        });
                        intent.expected.push(EffectConstraint::Position {
                            resource: resource.clone(),
                            position: supply_position.clone(),
                            asset: principal_asset.clone(),
                            direction: Direction::Increase,
                            maximum_change: quantity(qty),
                        });
                        warnings.push("Supply may enable collateral automatically. Collateral settings and allowances persist until explicitly changed by another approved write.".into());
                        Bytes::new()
                    }
                    "aave.borrow" => {
                        require(
                            !reserve.frozen && reserve.borrowing,
                            "BORROW_UNAVAILABLE",
                            "Reserve must permit variable borrowing and be unfrozen.",
                        )?;
                        let cap = mul(u256(&reserve.borrow_cap)?, pow10(token.decimals)?)?;
                        require(
                            cap.is_zero() || add(u256(&reserve.total_debt)?, qty)? <= cap,
                            "BORROW_CAP",
                            "Borrow exceeds the observed reserve cap.",
                        )?;
                        require(
                            u256(&reserve.liquidity)? >= qty,
                            "LIQUIDITY_INSUFFICIENT",
                            "Reserve lacks exact underlying liquidity.",
                        )?;
                        let account = self
                            .runtime
                            .call(pool, &Pool::getUserAccountDataCall { user: owner }, None)
                            .await?;
                        let debt_index = self
                            .runtime
                            .call(
                                pool,
                                &Pool::getReserveNormalizedVariableDebtCall { asset },
                                None,
                            )
                            .await?;
                        require(
                            debt_index >= pow10(27)? && debt_index < (U256::from(1) << 128),
                            "DEBT_INDEX_INVALID",
                            "Borrow normalized index exceeds reviewed finite bounds.",
                        )?;
                        let rounding_limit = add(
                            add(debt_index, pow10(27)? - U256::from(1))? / pow10(27)?,
                            U256::from(2),
                        )?;
                        b.debt_rounding_maximum = Some(quantity(rounding_limit));
                        self.aave_borrow_restrictions(owner, &token, add(qty, rounding_limit)?)
                            .await?;
                        let (collateral_base, debt, projected) = self
                            .aave_health_projection(
                                owner,
                                &token,
                                None,
                                false,
                                add(qty, rounding_limit)?,
                            )
                            .await?;
                        self.aave_debt_limits(&r, collateral_base, debt)?;
                        let added = debt.saturating_sub(account.totalDebtBase);
                        let minimum = exact(r.min_health_factor.as_deref().expect("schema"), 18)?;
                        require(
                            minimum > pow10(18)?
                                && projected >= minimum
                                && added <= account.availableBorrowsBase,
                            "HEALTH_FACTOR_LIMIT",
                            "Borrow exceeds collateral capacity or explicit minimum health factor.",
                        )?;
                        intent.expected.push(EffectConstraint::Debt {
                            resource: resource.clone(),
                            debt: debt_position.clone(),
                            asset: principal_asset.clone(),
                            direction: Direction::Increase,
                            // Opening debt includes its already-priced finite
                            // rounding exposure. With an existing verified debt
                            // baseline, principal and accrual stay separate.
                            maximum_change: quantity(
                                if b.debt_anchor
                                    .as_ref()
                                    .expect("anchor")
                                    .principal
                                    .atoms
                                    .is_zero()
                                    && user.scaled_variable_debt.is_zero()
                                {
                                    add(qty, rounding_limit)?
                                } else {
                                    qty
                                },
                            ),
                        });
                        intent.expected.push(EffectConstraint::Credit {
                            to: resource.clone(),
                            asset: principal_asset.clone(),
                            minimum_credit: quantity(qty),
                        });
                        warnings.push("The minimum health factor is a fresh preflight constraint; variable interest continues after execution.".into());
                        Bytes::new()
                    }
                    "aave.repay" => {
                        let debt = u256(&user.variable_debt)?;
                        require(
                            debt > U256::ZERO,
                            "NO_VARIABLE_DEBT",
                            "No variable debt exists for this reserve.",
                        )?;
                        let allowance = self.allowance(&token, owner, pool, None).await?;
                        require(
                            allowance < U256::MAX
                                && allowance >= if repay_all { debt } else { qty },
                            "ALLOWANCE_INSUFFICIENT",
                            "Repayment requires finite sufficient Pool allowance.",
                        )?;
                        if repay_all {
                            require(
                                debt <= qty && allowance <= qty,
                                "REPAY_MAXIMUM",
                                "Repay all debt/allowance exceeds explicit max_amount.",
                            )?;
                        }
                        intent.debits.push(ResourceAmount {
                            resource: resource.clone(),
                            amount: amount(&principal_asset, qty),
                        });
                        intent.expected.push(EffectConstraint::Debit {
                            resource: resource.clone(),
                            asset: principal_asset.clone(),
                            maximum_debit: quantity(qty),
                        });
                        intent.expected.push(EffectConstraint::Debt {
                            resource: resource.clone(),
                            debt: debt_position.clone(),
                            asset: principal_asset.clone(),
                            direction: Direction::Decrease,
                            maximum_change: quantity(qty),
                        });
                        Bytes::new()
                    }
                    "aave.withdraw" => {
                        let claim = u256(&user.supplied)?;
                        require(
                            if withdraw_all {
                                claim > U256::ZERO
                                    && claim <= qty
                                    && claim <= u256(&reserve.liquidity)?
                            } else {
                                qty <= claim && qty <= u256(&reserve.liquidity)?
                            },
                            "WITHDRAW_LIMIT",
                            "Withdrawal must fit its exact claim, finite cap and available liquidity.",
                        )?;
                        let minimum = if withdraw_all {
                            original
                                .and_then(|old| old.minimum_output.as_ref())
                                .map(u256)
                                .transpose()?
                                .unwrap_or(claim)
                        } else {
                            qty
                        };
                        require(
                            claim >= minimum,
                            "WITHDRAW_MINIMUM",
                            "Current claim decreased below the frozen return minimum.",
                        )?;
                        b.minimum_output = if withdraw_all {
                            Some(quantity(minimum))
                        } else {
                            None
                        };
                        if user.collateral_enabled {
                            self.aave_removal_risk(
                                owner,
                                &token,
                                Some(if withdraw_all { claim } else { qty }),
                                r.min_health_factor.as_deref(),
                            )
                            .await?;
                        }
                        let recipient = address(r.recipient.as_deref().expect("schema"))?;
                        require(
                            recipient != Address::ZERO,
                            "ZERO_ADDRESS",
                            "Withdraw recipient must be nonzero.",
                        )?;
                        require(
                            recipient == owner,
                            "AAVE_MCP_RECIPIENT_UNSUPPORTED",
                            "The official Aave MCP withdrawal action targets the signing wallet; choose that wallet as recipient.",
                        )?;
                        b.recipient = Some(lower_address(recipient));
                        intent.expected.push(EffectConstraint::Position {
                            resource: resource.clone(),
                            position: supply_position.clone(),
                            asset: principal_asset.clone(),
                            direction: Direction::Decrease,
                            maximum_change: quantity(if withdraw_all {
                                qty
                            } else {
                                add(
                                    qty,
                                    (u256(&reserve.normalized_income)? + pow10(27)?
                                        - U256::from(1))
                                        / pow10(27)?
                                        + U256::from(2),
                                )?
                            }),
                        });
                        intent.expected.push(EffectConstraint::Credit {
                            to: super::resource(self.config.chain_id, recipient),
                            asset: principal_asset.clone(),
                            minimum_credit: quantity(minimum),
                        });
                        Bytes::new()
                    }
                    "aave.collateral" => {
                        require(
                            !user.supplied.is_zero(),
                            "NO_SUPPLIED_POSITION",
                            "Collateral toggles require an existing supplied position.",
                        )?;
                        let enabled = r.enabled.expect("schema");
                        require(
                            !enabled || (reserve.collateral_eligible && !reserve.ltv.is_zero()),
                            "COLLATERAL_UNAVAILABLE",
                            "Reserve is ineligible for collateral use.",
                        )?;
                        if !enabled && user.collateral_enabled {
                            self.aave_removal_risk(
                                owner,
                                &token,
                                None,
                                r.min_health_factor.as_deref(),
                            )
                            .await?;
                        }
                        b.enabled = Some(enabled);
                        intent.expected.push(EffectConstraint::Authority {
                            resource: resource.clone(),
                            authority: collateral(&self.aave()?.pool, &token.reference),
                            before: user.collateral_enabled.to_string(),
                            after: enabled.to_string(),
                        });
                        Bytes::new()
                    }
                    _ => return Err(GoFinError::validation("Unsupported write.")),
                }
            }
        };
        if operation.starts_with("aave.") {
            use crate::aave_mcp::{
                AaveMcpAction, AaveMcpAmount, AaveMcpIntent, validate_transaction_fields,
            };
            let action = match operation {
                "aave.supply" => AaveMcpAction::Supply,
                "aave.borrow" => AaveMcpAction::Borrow,
                "aave.repay" => AaveMcpAction::Repay,
                "aave.withdraw" => AaveMcpAction::Withdraw,
                "aave.collateral" => AaveMcpAction::Collateral,
                _ => unreachable!("registered Aave write"),
            };
            let amount = if action == AaveMcpAction::Collateral {
                None
            } else if repay_all || withdraw_all {
                Some(AaveMcpAmount::Max { cap: qty })
            } else {
                Some(AaveMcpAmount::Exact(qty))
            };
            let mcp_intent = AaveMcpIntent {
                chain_id: self.config.chain_id,
                pool: address(&self.aave()?.pool)?,
                owner,
                token: address(&token.reference)?,
                action,
                amount,
                recipient: b.recipient.as_deref().map(address).transpose()?,
                enabled: b.enabled,
                decimals: token.decimals,
            };
            calldata = if let Some(old) = original {
                // Revalidation keeps the exact approved bytes and never asks
                // the MCP for a replacement action.
                rpc_bytes(&json!(old.transaction.calldata))?
            } else {
                self.aave_mcp
                    .prepare(&mcp_intent)
                    .await
                    .map_err(|e| error("AAVE_MCP_REJECTED", "preview", &e.to_string()))?
                    .calldata
            };
            validate_transaction_fields(
                &mcp_intent,
                self.config.chain_id,
                to,
                owner,
                value,
                &calldata,
            )
            .map_err(|e| error("AAVE_CALLDATA_INVALID", "preview", &e.to_string()))?;
            if let Some(old) = original {
                let identity = old.aave_proxy_identity.as_ref().ok_or_else(|| {
                    error(
                        "AAVE_PREVIEW_REFRESH_REQUIRED",
                        "revalidation",
                        "This legacy Aave preview did not bind the selected Pool proxy and implementation. Create a fresh preview before execution.",
                    )
                })?;
                let current = self.aave_proxy_identity().await?;
                require(
                    &current == identity,
                    "AAVE_IMPLEMENTATION_CHANGED",
                    "The selected Pool proxy or implementation changed after preview; create a fresh preview.",
                )?;
                b.aave_proxy_identity = Some(identity.clone());
            } else {
                b.aave_proxy_identity = Some(self.aave_proxy_identity().await?);
            }
            warnings.push("The official Aave MCP supplies candidate unsigned transaction data; GoFin validates the selected Pool call locally before approval and signing.".into());
        }
        if matches!(
            operation,
            "wallet.unwrap"
                | "wallet.transfer"
                | "uniswap.swap"
                | "ramses.swap"
                | "sushi.swap"
                | "pancakeswap.swap"
                | "aave.supply"
                | "aave.repay"
        ) {
            require(
                self.balance_of(&token, owner, None).await? >= qty,
                "INSUFFICIENT_BALANCE",
                "Exact reserved principal exceeds spendable holdings.",
            )?;
        }
        let maximum_fee = exact(r.max_fee_eth.as_deref().expect("schema"), 18)?;
        require(
            maximum_fee > U256::ZERO,
            "FEE_LIMIT",
            "Explicit max_fee_eth must be positive.",
        )?;
        let simulation = if let Some(old) = original {
            if let Some(template) = &b.permit_template {
                require(
                    old.transaction == *template.transaction()
                        && old.permit_template.as_ref() == Some(template),
                    "BINDING_CHANGED",
                    "Current unsigned Permit2 transaction differs from the approved template.",
                )?;
                b.transaction = template.transaction().clone();
                Bytes::new()
            } else {
                let tx = &old.transaction;
                require(
                    address(&tx.sender)? == owner
                        && address(&tx.to)? == to
                        && u256(&tx.value)? == value
                        && gofin_evm::rpc_bytes(&json!(tx.calldata))? == calldata,
                    "BINDING_CHANGED",
                    "Current protocol route/calldata differs from approved exact envelope.",
                )?;
                b.transaction = tx.clone();
                self.runtime.revalidate(tx).await?
            }
        } else {
            if let Some(template) = &b.permit_template {
                b.transaction = template.transaction().clone();
                Bytes::new()
            } else {
                let prepared = self
                    .runtime
                    .prepare(owner, to, value, calldata, maximum_fee)
                    .await?;
                b.transaction = prepared.transaction;
                prepared.simulation
            }
        };
        if matches!(operation, "wallet.approve" | "wallet.transfer") && token.reference != "native"
        {
            let accepted = if operation == "wallet.approve" {
                Erc20::approveCall::abi_decode_returns(&simulation)
            } else {
                Erc20::transferCall::abi_decode_returns(&simulation)
            }
            .map_err(|_| {
                error(
                    "ERC20_RETURN_INVALID",
                    "preview",
                    "Standard ERC-20 write must return a decoded boolean.",
                )
            })?;
            require(
                accepted,
                "ERC20_RETURN_FALSE",
                "Token simulation returned false.",
            )?;
        }
        if operation == "aave.repay" {
            let paid = Pool::repayCall::abi_decode_returns(&simulation).map_err(|_| {
                error(
                    "SIMULATION_RETURN_INVALID",
                    "preview",
                    "Repay simulation lacks exact underlying amount.",
                )
            })?;
            require(
                paid > U256::ZERO && paid <= qty,
                "REPAY_SIMULATION_LIMIT",
                "Simulated repayment exceeds the approved finite debit or clears no debt.",
            )?;
        }
        if operation == "aave.withdraw" {
            let received = Pool::withdrawCall::abi_decode_returns(&simulation).map_err(|_| {
                error(
                    "SIMULATION_RETURN_INVALID",
                    "preview",
                    "Withdraw simulation lacks exact underlying amount.",
                )
            })?;
            require(
                if withdraw_all {
                    received <= qty
                        && received >= u256(b.minimum_output.as_ref().expect("minimum"))?
                } else {
                    received == qty
                },
                "WITHDRAW_SIMULATION_LIMIT",
                "Simulated withdrawal differs from requested exact cash amount.",
            )?;
        }
        if matches!(
            operation,
            "uniswap.swap" | "ramses.swap" | "sushi.swap" | "pancakeswap.swap"
        ) {
            let protocol = b.protocol.as_deref().unwrap_or("v3");
            if b.permit_template.is_some() || protocol == "v4" {
                // Universal Router v4 `execute` intentionally returns no amount;
                // typed permit templates are intentionally unsigned in preview.
                // Their exact signature-dependent call is simulated only after
                // approval and the frozen minimum remains enforced by calldata.
            } else {
                let ramses = b.dex.as_deref() == Some("ramses");
                let router_kind = if ramses {
                    RouterKind::LegacyV3
                } else {
                    self.v3_profile(super::uniswap::profile::operation_dex(operation)?)?
                        .router_kind
                };
                let received = if router_kind == RouterKind::SwapRouter02 || quote_native_output(&b)
                {
                    let outputs = if ramses {
                        RamsesRouter::multicallCall::abi_decode_returns(&simulation)
                    } else if router_kind == RouterKind::SwapRouter02 {
                        SwapRouter02::multicallCall::abi_decode_returns(&simulation)
                    } else {
                        LegacyRouterPayments::multicallCall::abi_decode_returns(&simulation)
                    }
                    .map_err(|_| {
                        error(
                            "SIMULATION_RETURN_INVALID",
                            "preview",
                            "Router multicall simulation lacks bounded output data.",
                        )
                    })?;
                    let bytes = outputs.into_iter().next().ok_or_else(|| {
                        error(
                            "SIMULATION_RETURN_INVALID",
                            "preview",
                            "Router multicall returned no swap result.",
                        )
                    })?;
                    if ramses {
                        RamsesRouter::exactInputSingleCall::abi_decode_returns(&bytes)
                    } else {
                        Router::exactInputSingleCall::abi_decode_returns(&bytes)
                    }
                    .map_err(|_| {
                        error(
                            "SIMULATION_RETURN_INVALID",
                            "preview",
                            "Swap simulation lacks exact output amount.",
                        )
                    })?
                } else {
                    if ramses {
                        RamsesRouter::exactInputSingleCall::abi_decode_returns(&simulation)
                    } else {
                        Router::exactInputSingleCall::abi_decode_returns(&simulation)
                    }
                    .map_err(|_| {
                        error(
                            "SIMULATION_RETURN_INVALID",
                            "preview",
                            "Swap simulation lacks exact output amount.",
                        )
                    })?
                };
                require(
                    received >= u256(b.minimum_output.as_ref().expect("minimum"))?,
                    "SWAP_SIMULATION_LIMIT",
                    "Simulated output is below the approved minimum.",
                )?;
            }
        }
        if let Some(old) = original {
            require(
                old.scaled_before == b.scaled_before
                    && old.allowance_before == b.allowance_before
                    && old.collateral_before == b.collateral_before,
                "STATE_CHANGED",
                "Bound scaled principal or authority changed since approval.",
            )?;
            // Observed claims may accrue but immutable execution fields and approved origin remain frozen.
            b.supplied_before = old.supplied_before.clone();
            b.debt_before = old.debt_before.clone();
            if matches!(
                operation,
                "uniswap.swap" | "ramses.swap" | "sushi.swap" | "pancakeswap.swap"
            ) {
                // Keep the original quote evidence immutable after proving the
                // fresh quote still meets its minimum and impact ceiling.
                b.quote_output = old.quote_output.clone();
                b.price_impact_bps = old.price_impact_bps;
                if old.dex.as_deref() == Some("ramses") {
                    // Fee is dynamic and quote-observed, not a pool identity.
                    // Validate the refreshed observation against the immutable
                    // cap above, then retain the initially reviewed evidence.
                    b.swap_fee_ppm = old.swap_fee_ppm;
                }
            }
            require(
                &b == old,
                "BINDING_CHANGED",
                "Financial binding changed since approval.",
            )?;
        }
        if operation.starts_with("aave.") {
            let supply_pos = position(&self.aave()?.pool, &token.reference, "supply");
            let debt_pos = position(&self.aave()?.pool, &token.reference, "variable-debt");
            let cash = AccountingLocation::Cash {
                resource: resource.clone(),
            };
            let supply = AccountingLocation::Position {
                resource: resource.clone(),
                position: supply_pos.clone(),
                position_kind: PositionKind::Supply,
            };
            let debt = AccountingLocation::Position {
                resource: resource.clone(),
                position: debt_pos.clone(),
                position_kind: PositionKind::VariableDebt,
            };
            let index: Quantity = serde_json::from_value(
                b.supply_anchor.as_ref().expect("anchor").evidence["normalized_index"].clone(),
            )
            .map_err(|_| GoFinError::validation("Invalid trusted index."))?;
            let quantum = (u256(&index)? + pow10(27)? - U256::from(1)) / pow10(27)? + U256::from(2);
            let ceiling = add(qty, quantum)?;
            if matches!(operation, "aave.supply" | "aave.withdraw") {
                // A complete withdrawal's observed claim, including yield,
                // must fit its finite max_amount. Partial withdrawals keep
                // their prior claim-plus-request envelope.
                let maximum_change = if operation == "aave.withdraw" && withdraw_all {
                    qty
                } else {
                    add(qty, u256(b.supplied_before.as_ref().expect("claim"))?)?
                };
                intent.expected.push(EffectConstraint::Accrual {
                    resource: resource.clone(),
                    position: supply_pos.clone(),
                    asset: principal_asset.clone(),
                    position_kind: PositionKind::Supply,
                    maximum_change: quantity(maximum_change),
                });
                let before = b.collateral_before.expect("collateral").to_string();
                intent.expected.push(EffectConstraint::AuthorityTransition {
                    resource: resource.clone(),
                    authority: collateral(&self.aave()?.pool, &token.reference),
                    before: before.clone(),
                    allowed_after: vec![
                        before,
                        if operation == "aave.supply" {
                            "true".into()
                        } else {
                            "false".into()
                        },
                    ]
                    .into_iter()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
                });
            }
            if matches!(operation, "aave.borrow" | "aave.repay") {
                // For repay-all the complete paid debt, including accrued
                // interest, must fit this fresh preview's finite max_amount.
                // Partial repayment keeps its distinct debt-plus-installment
                // envelope.
                let finite_full_close = operation == "aave.repay" && repay_all;
                let maximum_change = if finite_full_close {
                    qty
                } else {
                    add(qty, u256(b.debt_before.as_ref().expect("claim"))?)?
                };
                intent.expected.push(EffectConstraint::Accrual {
                    resource: resource.clone(),
                    position: debt_pos.clone(),
                    asset: principal_asset.clone(),
                    position_kind: PositionKind::VariableDebt,
                    maximum_change: quantity(maximum_change),
                });
            }
            match operation {
                "aave.supply" => {
                    intent.expected.push(EffectConstraint::OriginMovement {
                        from: cash,
                        to: supply,
                        debit_asset: principal_asset.clone(),
                        credit_asset: principal_asset.clone(),
                        maximum_debit: quantity(qty),
                        minimum_credit: quantity(qty.saturating_sub(quantum).max(U256::from(1))),
                    });
                    intent.maximum_costs.push(CostBound {
                        location: AccountingLocation::Cash {
                            resource: resource.clone(),
                        },
                        amount: amount(&principal_asset, quantum),
                        treatment: CostTreatment::IncludedInDebit,
                    });
                }
                "aave.borrow" => intent.expected.push(EffectConstraint::OriginMovement {
                    from: debt,
                    to: cash,
                    debit_asset: principal_asset.clone(),
                    credit_asset: principal_asset.clone(),
                    maximum_debit: quantity(qty),
                    minimum_credit: quantity(qty),
                }),
                "aave.repay" => {
                    intent.expected.push(EffectConstraint::OriginMovement {
                        from: cash,
                        to: debt,
                        debit_asset: principal_asset.clone(),
                        credit_asset: principal_asset.clone(),
                        maximum_debit: quantity(qty),
                        minimum_credit: Quantity::from_u64(1),
                    });
                    intent.maximum_costs.push(CostBound {
                        location: AccountingLocation::Cash {
                            resource: resource.clone(),
                        },
                        amount: amount(&principal_asset, qty),
                        treatment: CostTreatment::IncludedInDebit,
                    });
                }
                "aave.withdraw" => {
                    for e in &mut intent.expected {
                        if let EffectConstraint::Position { maximum_change, .. } = e {
                            *maximum_change = quantity(if withdraw_all { qty } else { ceiling });
                        }
                    }
                    intent.expected.push(EffectConstraint::OriginMovement {
                        from: supply,
                        to: AccountingLocation::Cash {
                            resource: super::resource(
                                self.config.chain_id,
                                address(b.recipient.as_deref().expect("recipient"))?,
                            ),
                        },
                        debit_asset: principal_asset.clone(),
                        credit_asset: principal_asset.clone(),
                        maximum_debit: quantity(if withdraw_all { qty } else { ceiling }),
                        minimum_credit: if withdraw_all {
                            b.minimum_output.clone().expect("minimum")
                        } else {
                            quantity(qty)
                        },
                    });
                    intent.maximum_costs.push(CostBound {
                        location: AccountingLocation::Position {
                            resource: resource.clone(),
                            position: supply_pos.clone(),
                            position_kind: PositionKind::Supply,
                        },
                        amount: amount(&principal_asset, quantum),
                        treatment: CostTreatment::IncludedInDebit,
                    });
                }
                _ => {}
            }
        }
        let gas = b.transaction.maximum_cost()?;
        intent.maximum_costs.push(CostBound {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: amount(&self.native_asset(), gas),
            treatment: CostTreatment::AdditionalDebit,
        });
        let movement = if operation == "aave.collateral" {
            b.supplied_before.clone().unwrap_or_default()
        } else {
            quantity(qty)
        };
        // Derive all USD scalars from the same attributable final unit rows.
        // Separate amount quotes can expire or change during preparation;
        // old quote timestamps are never restamped or silently discarded.
        let native_asset = self.native_asset();
        let price = |asset: Option<AssetId>| async move {
            let Some(asset) = asset else {
                return Ok::<_, GoFinError>(None);
            };
            let unit = quantity(pow10(asset.decimals)?);
            let (usd, source, observed_at) = self
                .quote_verified_asset_usd_observed(&asset, unit.clone())
                .await?;
            Ok(Some(AssetPrice {
                asset: asset.clone(),
                usd_micros_per_unit: usd,
                unit_atoms: unit,
                observed_at,
                source,
            }))
        };
        // At most three independent assets; no unbounded fan-out or cross-phase cache.
        let (principal_price, gas_price, output_price) = tokio::try_join!(
            price(Some(principal_asset.clone())),
            price((native_asset != principal_asset).then_some(native_asset.clone())),
            price(
                b.output
                    .as_ref()
                    .filter(|asset| **asset != principal_asset && **asset != native_asset)
                    .cloned()
            ),
        )?;
        let prices: Vec<_> = [principal_price, gas_price, output_price]
            .into_iter()
            .flatten()
            .collect();
        let price_observations: Vec<_> = prices
            .iter()
            .map(|price| price.observed_at.clone())
            .collect();
        let observed = self.clock.now().to_rfc3339();
        let usd_from_row = |asset: &AssetId, units: Quantity| -> Result<Quantity> {
            let row = prices
                .iter()
                .find(|p| p.asset == *asset)
                .ok_or_else(|| GoFinError::validation("Exact valuation row missing."))?;
            units.mul_div_ceil(&row.usd_micros_per_unit, &row.unit_atoms)
        };
        let movement_usd = usd_from_row(&principal_asset, movement)?;
        let fee_usd = usd_from_row(&self.native_asset(), quantity(gas))?;
        let source = prices
            .iter()
            .find(|p| p.asset == principal_asset)
            .expect("principal row")
            .source
            .clone();
        let additional_allocation = if operation == "aave.borrow" {
            usd_from_row(
                &principal_asset,
                quantity(add(
                    qty,
                    u256(b.debt_rounding_maximum.as_ref().expect("borrow rounding"))?,
                )?),
            )?
        } else {
            Quantity::zero()
        };
        let valuation_observed = price_observations
            .into_iter()
            .min_by_key(|timestamp| {
                chrono::DateTime::parse_from_rfc3339(timestamp)
                    .expect("validated observation timestamp")
            })
            .expect("principal and fee observations");
        let expires = (self.clock.now() + chrono::Duration::seconds(300))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        if matches!(
            operation,
            "uniswap.swap" | "ramses.swap" | "sushi.swap" | "pancakeswap.swap"
        ) {
            let router = b.spender.as_deref().expect("swap router spender");
            let strategy = if b.token.reference == "native" {
                "native input requires no token allowance; the reviewed router performs the native settlement"
                    .to_owned()
            } else if b.protocol.as_deref() == Some("v4") {
                if b.permit_template.is_some() {
                    format!(
                        "finite token allowance to canonical Permit2 0x000000000022d473030f116ddee9f6b43ac78ba3 was checked; a typed Permit2 signature will authorize router {router} only after approval"
                    )
                } else {
                    format!(
                        "finite token allowance to canonical Permit2 0x000000000022d473030f116ddee9f6b43ac78ba3 and finite Permit2 allowance to router {router} were checked and reused"
                    )
                }
            } else if b.permit_template.is_some() {
                format!("typed EIP-2612 permit authorizes router {router} only after approval")
            } else {
                format!(
                    "existing finite ERC-20 allowance to router {router} was checked and reused"
                )
            };
            swap_permission = Some(strategy.clone());
            warnings.push(format!("Swap permission: {strategy}. If a required finite allowance is missing and no usable atomic permit is available, approve that exact finite spender separately."));
        }
        if let Some(shared) = &shared_swap {
            let actual_permission = if b.token.reference == "native" {
                gofin_amm::PermissionRequirement::None
            } else if b.permit_template.is_some() {
                gofin_amm::PermissionRequirement::Permit
            } else {
                gofin_amm::PermissionRequirement::None
            };
            let mut shared = shared.clone();
            shared.quote.permission = actual_permission;
            // Keep GoFin's shared route and exact minimum-output contract as the
            // source for the typed conversion effect; protocol cash proof and
            // the finite gas reservation remain adapter-owned.
            let generic = gofin_amm::prepared_operation(&shared, self.clock.now())?;
            intent.expected = generic.intent.expected;
            // The fresh shared quote validates route/impact/receipt feasibility;
            // the immutable transaction still owns its original encoded floor.
            for effect in &mut intent.expected {
                if let EffectConstraint::Conversion { minimum_output, .. } = effect {
                    *minimum_output = b.minimum_output.clone().expect("swap minimum");
                }
            }
            // Generic conversion intent does not own protocol authority
            // transitions. Preserve the exact finite permit consumed by this
            // swap as a separate expected effect.
            if let Some(template) = &b.permit_template {
                match template.permit() {
                    AuthorizedPermit::Eip2612(permit) => {
                        intent.expected.push(EffectConstraint::Authority {
                            resource: resource.clone(),
                            authority: authority(&permit.token, &permit.spender),
                            before: b
                                .allowance_before
                                .clone()
                                .unwrap_or_else(Quantity::zero)
                                .to_string(),
                            after: Quantity::zero().to_string(),
                        });
                    }
                    AuthorizedPermit::Permit2Single(permit) => {
                        let before = b.permit2_allowance_before.as_ref().ok_or_else(|| {
                            error(
                                "BINDING_INVALID",
                                "preview",
                                "Permit2 swap lacks its original allowance state.",
                            )
                        })?;
                        intent.expected.push(EffectConstraint::Authority {
                            resource: resource.clone(),
                            authority: super::permit2::approval_authority(
                                &token.asset(self),
                                &permit.spender,
                            ),
                            before: super::permit2::allowance_state_text(
                                &before.amount,
                                before.expiration,
                                before.nonce,
                            ),
                            after: super::permit2::allowance_state_text(
                                &Quantity::zero(),
                                permit.expiration,
                                permit.nonce + 1,
                            ),
                        });
                    }
                }
            }
        }
        let mut checks = vec![Check {
            name: "chain, deployment, balance and protocol prerequisites".into(),
            status: CheckStatus::Passed,
            detail: "Live reads validated exact wallet, deployment, token, route and allowance prerequisites.".into(),
        }];
        if b.permit_template.is_some() {
            checks.push(Check {
                name: "materialized permit transaction simulation".into(),
                status: CheckStatus::Unchecked,
                detail: "The unsigned permit template is not simulated as if it were signed. After authorization, GoFin signs the typed permit, materializes the frozen transaction and simulates that exact transaction before submission.".into(),
            });
        } else {
            checks.push(Check {
                name: "exact transaction simulation".into(),
                status: CheckStatus::Passed,
                detail: "The exact frozen transaction was simulated during preparation.".into(),
            });
        }
        let summary = if matches!(
            operation,
            "uniswap.swap" | "ramses.swap" | "sushi.swap" | "pancakeswap.swap"
        ) {
            let output = b.output.as_ref().expect("swap output");
            let output_symbol = swap_output_symbol.as_deref().unwrap_or(&output.reference);
            let protocol = b.protocol.as_deref().unwrap_or("v3");
            let route = if b.dex.as_deref() == Some("ramses") {
                format!(
                    "Ramses tick spacing {}, quoted pool fee {} ppm (quote-time ceiling {} ppm only)",
                    b.tick_spacing.expect("Ramses tick spacing"),
                    b.swap_fee_ppm.expect("quoted dynamic fee"),
                    b.max_pool_fee_ppm.expect("quote fee ceiling")
                )
            } else if protocol == "v4" {
                let key = b.v4_pool_key.as_ref().expect("v4 pool key");
                format!(
                    "Uniswap v4 fee {} ppm, tick spacing {}",
                    key.fee, key.tick_spacing
                )
            } else {
                format!(
                    "{} v3 fee {} ppm",
                    super::uniswap::profile::additional_dex(operation)
                        .map(V3Dex::display_name)
                        .unwrap_or("Uniswap"),
                    b.fee_tier.expect("v3 fee tier")
                )
            };
            let permission = swap_permission
                .as_deref()
                .unwrap_or("reviewed finite allowance or typed permit");
            format!(
                "Swap {} {} for at least {} {}; recipient {}; {route}; router {}; slippage {} bps, impact max {} bps; deadline {}; max network fee {} ETH; permission: {permission}.",
                quantity(qty).to_decimal(token.decimals),
                token.symbol,
                b.minimum_output
                    .as_ref()
                    .expect("minimum output")
                    .to_decimal(output.decimals),
                output_symbol,
                b.recipient.as_deref().expect("swap recipient"),
                b.spender.as_deref().expect("swap router"),
                r.slippage_bps.expect("selected slippage"),
                r.max_price_impact_bps.unwrap_or(10_000),
                b.deadline.expect("swap deadline"),
                quantity(gas).to_decimal(18),
            )
        } else if operation.starts_with("aave.") {
            let quantity = quantity(qty).to_decimal(token.decimals);
            let label = super::amm_permissions::safe_display_label(Some(&token.symbol));
            let recipient = if operation == "aave.withdraw" {
                format!("; recipient {}", r.recipient.as_deref().unwrap_or(""))
            } else {
                String::new()
            };
            let risk = r
                .min_health_factor
                .as_deref()
                .map(|hf| format!("; minimum health factor {hf}"))
                .unwrap_or_default();
            let debt_limit = r
                .max_debt_usd
                .as_deref()
                .map(|v| format!("; maximum outstanding debt ${v}"))
                .unwrap_or_default();
            let ratio = r
                .max_debt_to_collateral_bps
                .map(|v| format!("; maximum debt/collateral {v} bps"))
                .unwrap_or_default();
            let action = if operation == "aave.collateral" {
                format!(
                    "{} {} collateral",
                    if r.enabled == Some(true) {
                        "Enable"
                    } else {
                        "Disable"
                    },
                    label
                )
            } else {
                format!(
                    "{} {} {}",
                    operation.strip_prefix("aave.").unwrap_or(operation),
                    if all_amount {
                        format!("all, capped at {quantity}")
                    } else {
                        quantity
                    },
                    label
                )
            };
            format!(
                "{action} on Aave V3, chain {}; maximum fee {} ETH{recipient}{risk}{debt_limit}{ratio}. Supplied assets and borrowed debt remain separate from wallet cash.",
                self.config.chain_id,
                r.max_fee_eth.as_deref().unwrap_or("")
            )
        } else {
            format!(
                "{} from {} on EVM chain {} with exact principal {} and maximum native gas {}",
                request::description(operation),
                w.name,
                self.config.chain_id,
                qty,
                gas
            )
        };
        Ok(PreparedOperation {
            operation: operation.into(),
            input: input.clone(),
            binding: serde_json::to_value(b).expect("binding serializable"),
            summary,
            checks,
            warnings,
            observed_at: observed.clone(),
            expires_at: expires,
            valuation: Some(Valuation {
                movement_usd_micros: movement_usd,
                maximum_fee_usd_micros: fee_usd,
                additional_allocation_usd_micros: additional_allocation,
                observed_at: valuation_observed,
                source,
                prices,
            }),
            intent,
            operation_version: 1,
            action_deadline: None,
        })
    }
}

#[cfg(test)]
mod permit_signer_tests {
    use super::require_eoa_permit_signer;

    #[test]
    fn permits_require_an_eoa_but_existing_allowance_routes_are_unaffected() {
        assert!(require_eoa_permit_signer(&[]).is_ok());
        let delegated = hex::decode("ef01008a67b5020ee254ef48e3b6a04927f39baf7e408a").unwrap();
        assert_eq!(
            require_eoa_permit_signer(&delegated).unwrap_err().code,
            "ALLOWANCE_INSUFFICIENT"
        );
    }
}

#[cfg(test)]
mod validation_error_tests {
    use super::*;
    use crate::tests::{Fixture, adapter, swap_input};
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn excessive_price_impact_has_its_own_error_and_never_reaches_submission() {
        let rpc = Arc::new(Fixture::new());
        let adapter = adapter(rpc.clone());
        let mut input = swap_input();
        input["max_price_impact_bps"] = json!(100);
        for inner in [false, true] {
            let result = if inner {
                adapter
                    .prepare_operation_inner(
                        "uniswap.swap",
                        &input,
                        None,
                        &PreparationContext::default(),
                    )
                    .await
            } else {
                adapter.prepare("uniswap.swap", &input).await
            };
            let err = result.unwrap_err();
            assert_eq!(err.code, "PRICE_IMPACT_LIMIT");
            let details = err.details.unwrap();
            assert!(details["observed_price_impact_bps"].as_u64().unwrap() > 100);
            assert_eq!(details["maximum_price_impact_bps"], 100);
        }
        // A minimum output above the quote retains the existing classification.
        input["max_price_impact_bps"] = json!(10_000);
        input["min_output"] = json!("3.01");
        for inner in [false, true] {
            let result = if inner {
                adapter
                    .prepare_operation_inner(
                        "uniswap.swap",
                        &input,
                        None,
                        &PreparationContext::default(),
                    )
                    .await
            } else {
                adapter.prepare("uniswap.swap", &input).await
            };
            let error = result.unwrap_err();
            assert_eq!(error.code, "SLIPPAGE_LIMIT");
            let details = error.details.unwrap();
            assert_eq!(details["reason"], "minimum_output_unattainable");
            assert_eq!(details["minimum_output_atoms"], "3010000");
            assert_eq!(details["quote_output_atoms"], "3000000");
        }
        assert_eq!(rpc.nonce.load(Ordering::SeqCst), 0);
        assert!(
            !rpc.calls
                .lock()
                .unwrap()
                .iter()
                .any(|method| method == "eth_sendRawTransaction" || method == "eth_estimateGas")
        );
    }
}
