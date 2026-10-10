use super::*;
use abi::*;
use fin_evm::{
    AuthorizedPermit, Eip2612Permit, Permit2PermitSingle, SignatureLayout,
    UnsignedPermitTransaction,
    alloy::{primitives::Bytes, sol_types::SolCall},
    quantity, rpc_bytes, u256,
};

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

/// Adapts the reviewed Uniswap protocol kernel to Fin's shared AMM contract.
/// `prepare_swap` validates the shared route/floor bounds before delegating to
/// the normal protected transaction preparation path.
struct UniswapAmm<'a> {
    adapter: &'a EthereumAdapter,
    owner: Address,
    input: Value,
    request: request::Request,
    original: Option<Binding>,
    context: PreparationContext,
    resource: ResourceId,
    token_in: reads::Token,
    input_units: U256,
    maximum_fee: Quantity,
}

#[async_trait::async_trait]
impl fin_amm::AmmDex for UniswapAmm<'_> {
    async fn quote(&self, request: fin_amm::AmmQuoteRequest) -> Result<fin_amm::AmmQuote> {
        require(
            request.resource == self.resource
                && request.input.asset == self.token_in.asset(self.adapter)
                && request.input.atoms == quantity(self.input_units)
                && request.output_asset
                    == self
                        .adapter
                        .token(self.request.token_out.as_deref().expect("schema"), None)
                        .await?
                        .asset(self.adapter),
            "AMM_RESOURCE_MISMATCH",
            "Generic quote request differs from the selected wallet or exact route assets.",
        )?;
        let mut quote = self
            .adapter
            .quote_protocol(reads::ProtocolQuoteRequest {
                input: &self.token_in.reference,
                output: self.request.token_out.as_deref().expect("schema"),
                amount: &quantity(self.input_units).to_decimal(self.token_in.decimals),
                fee_tier: self.request.fee_tier,
                protocol: self.request.protocol.as_deref(),
                v4_key: self.request.v4_pool_key.as_ref(),
                block: None,
            })
            .await?;
        let permission = self
            .adapter
            .swap_permission_requirement(self.owner, &quote)
            .await?;
        quote.permission_requirement = Some(permission.clone());
        let generic = super::uniswap::as_amm_quote(
            self.adapter,
            self.resource.clone(),
            &quote,
            self.maximum_fee.clone(),
            permission,
        )?;
        if let Some(route) = request.route {
            require(
                route == generic.route,
                "AMM_ROUTE_MISMATCH",
                "Requested generic route differs from the reviewed Uniswap pool.",
            )?;
        }
        Ok(generic)
    }

    async fn prepare_swap(&self, request: fin_amm::AmmSwapRequest) -> Result<PreparedOperation> {
        fin_amm::validate_swap(&request, chrono::Utc::now()).map_err(|err| {
            if err.code == "AMM_BOUND_EXCEEDED" {
                error("SLIPPAGE_LIMIT", "validation", "Minimum output is below the approved fresh quote floor or above its attainable output.")
            } else { err }
        })?;
        require(
            request.resource == self.resource
                && request.approved_resource == self.resource
                && request.quote.input.atoms == quantity(self.input_units)
                && request.quote.route.version == self.request.protocol.as_deref().unwrap_or("v3"),
            "AMM_BOUND_EXCEEDED",
            "Shared swap request differs from the exact protected Uniswap request.",
        )?;
        let prepared = self
            .adapter
            .prepare_operation_inner(
                "uniswap.swap",
                &self.input,
                self.original.as_ref(),
                &self.context,
            )
            .await?;
        require(
            prepared.binding["units"] == json!(quantity(self.input_units)),
            "BINDING_CHANGED",
            "Exact input balance changed between generic AMM validation and protected preparation.",
        )?;
        Ok(prepared)
    }
}

impl EthereumAdapter {
    pub(crate) async fn prepare_operation(
        &self,
        operation: &str,
        input: &Value,
        original: Option<&Binding>,
        context: &PreparationContext,
    ) -> Result<PreparedOperation> {
        self.require_operation(operation)?;
        if operation == "wallet.permit2.approve" {
            return self.prepare_permit2_approval(input, None, context).await;
        }
        if operation != "uniswap.swap" {
            return self
                .prepare_operation_inner(operation, input, original, context)
                .await;
        }
        let request = request::parse(operation, input)?;
        let wallet = self.resolve(&request, true)?;
        let wallet_address = address(&wallet.address)?;
        let token_in = self
            .token(request.token_in.as_deref().expect("schema"), None)
            .await?;
        let token_out = self
            .token(request.token_out.as_deref().expect("schema"), None)
            .await?;
        let input_cap = if request.amount.as_deref() == Some("all") {
            exact(
                request.max_amount.as_deref().expect("schema"),
                token_in.decimals,
            )?
        } else {
            exact(
                request.amount.as_deref().expect("schema"),
                token_in.decimals,
            )?
        };
        let amount = if request.amount.as_deref() == Some("all") {
            self.balance_of(&token_in, wallet_address, None)
                .await?
                .min(input_cap)
        } else {
            input_cap
        };
        let maximum_fee = exact(request.max_fee_eth.as_deref().expect("schema"), 18)?;
        let resource = resource(self.config.chain_id, wallet_address);
        let quote_backend = UniswapAmm {
            adapter: self,
            owner: wallet_address,
            input: input.clone(),
            request: request.clone(),
            original: original.cloned(),
            context: context.clone(),
            resource: resource.clone(),
            token_in: token_in.clone(),
            input_units: amount,
            maximum_fee: quantity(maximum_fee),
        };
        let quote = fin_amm::AmmDex::quote(
            &quote_backend,
            fin_amm::AmmQuoteRequest {
                resource: resource.clone(),
                input: Amount {
                    asset: token_in.asset(self),
                    atoms: quantity(amount),
                },
                output_asset: token_out.asset(self),
                route: None,
            },
        )
        .await?;
        let amm_request = fin_amm::AmmSwapRequest {
            resource: resource.clone(),
            approved_resource: resource,
            approved_route: quote.route.clone(),
            quote,
            selected_slippage_bps: request.slippage_bps.expect("schema"),
            approved_slippage_bps: request.slippage_bps.expect("schema"),
            rate_floor: None,
            absolute_minimum_output: Some(quantity(exact(
                request.min_output.as_deref().expect("schema"),
                token_out.decimals,
            )?)),
            input_cap: Amount {
                asset: token_in.asset(self),
                atoms: quantity(input_cap),
            },
            fee_cap: Amount {
                asset: self.native_asset(),
                atoms: quantity(maximum_fee),
            },
            maximum_price_impact_bps: request.max_price_impact_bps.unwrap_or(10_000),
        };
        fin_amm::AmmDex::prepare_swap(&quote_backend, amm_request).await
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
        } else if operation == "uniswap.swap" {
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
        if all_amount && matches!(operation, "wallet.unwrap" | "uniswap.swap") {
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
            supply_anchor: None,
            debt_anchor: None,
            debt_rounding_maximum: None,
            value_bounds: original.and_then(|binding| binding.value_bounds.clone()),
            boundary_authorization: original
                .and_then(|binding| binding.boundary_authorization.clone()),
            financial_plan: original.and_then(|binding| binding.financial_plan.clone()),
        };
        let mut warnings=vec!["Every financial write requires a fresh human authorization. An uncertain hash is reconciled without resubmission.".into()];
        let mut shared_swap: Option<fin_amm::AmmSwapRequest> = None;
        let mut swap_output_symbol: Option<String> = None;
        let mut swap_permission: Option<String> = None;
        let mut to = address(&token.reference).unwrap_or(owner);
        let mut value = U256::ZERO;
        let mut permit2_authorization: Option<AuthorizedPermit> = None;
        let mut eip2612_authorization: Option<AuthorizedPermit> = None;
        let calldata: Bytes = match operation {
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
            "uniswap.swap" => {
                let quote = self
                    .quote_protocol(reads::ProtocolQuoteRequest {
                        input: &token.reference,
                        output: r.token_out.as_deref().expect("schema"),
                        amount: &quantity(qty).to_decimal(token.decimals),
                        fee_tier: original.and_then(|b| b.fee_tier).or(r.fee_tier),
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
                let amm_quote = super::uniswap::as_amm_quote(
                    self,
                    resource.clone(),
                    &quote,
                    quantity(maximum_fee),
                    if token.reference == "native" {
                        fin_amm::PermissionRequirement::None
                    } else {
                        fin_amm::PermissionRequirement::TokenApproval
                    },
                )?;
                let selected_slippage = r.slippage_bps.expect("schema");
                let absolute_minimum = exact(
                    r.min_output.as_deref().expect("schema"),
                    quote.token_out.decimals,
                )?;
                let amm_request = fin_amm::AmmSwapRequest {
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
                let validated = fin_amm::validate_swap(&amm_request, chrono::Utc::now()).map_err(|err| {
                    if err.code == "AMM_BOUND_EXCEEDED" {
                        error("SLIPPAGE_LIMIT", "validation", "Minimum output is below the approved fresh quote floor or above its attainable output.")
                    } else { err }
                })?;
                let fresh_minimum = u256(&validated)?;
                shared_swap = Some(amm_request);
                let quote_output = u256(&quote.output)?;
                let minimum = if let Some(original) = original {
                    let approved_minimum =
                        u256(original.minimum_output.as_ref().ok_or_else(|| {
                            error(
                                "BINDING_INVALID",
                                "validation",
                                "Approved swap minimum output is missing.",
                            )
                        })?)?;
                    require(
                        approved_minimum >= fresh_minimum && approved_minimum <= quote_output,
                        "SLIPPAGE_LIMIT",
                        "The fresh quote no longer supports the approved minimum output; create a fresh preview.",
                    )?;
                    approved_minimum
                } else {
                    fresh_minimum
                };
                require(
                    quote.price_impact_bps <= r.max_price_impact_bps.unwrap_or(10_000),
                    "PRICE_IMPACT_LIMIT",
                    "Quoted spot price impact exceeds the explicit maximum.",
                )?;
                let floor = mul(
                    quote_output,
                    U256::from(10000 - r.slippage_bps.expect("schema")),
                )? / U256::from(10000);
                require(
                    minimum > U256::ZERO && minimum >= floor && minimum <= quote_output,
                    "SLIPPAGE_LIMIT",
                    "Minimum output must respect the quoted slippage floor and remain achievable.",
                )?;
                let deadline = r.deadline.expect("schema");
                let current = chrono::Utc::now().timestamp() as u64;
                require(
                    deadline > current && deadline <= current + 600,
                    "DEADLINE_INVALID",
                    "Swap deadline must be in the next ten minutes.",
                )?;
                let protocol = super::uniswap::Protocol::parse(Some(&quote.protocol))?;
                let router = if protocol == super::uniswap::Protocol::V4 {
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
                    address(&self.uniswap()?.router)?
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
                        let result = self.runtime.rpc.request("eth_call", json!([{"to":lower_address(permit2),"data":fin_evm::hex_bytes(&allowance)},quote_block_tag])).await?;
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
                        let timestamp = chrono::Utc::now().timestamp().max(0) as u64;
                        if U256::from(amount) >= qty
                            && amount < fin_evm::alloy::primitives::aliases::U160::MAX
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
                                nonce < fin_evm::alloy::primitives::aliases::U48::MAX,
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
                b.fee_tier = Some(quote.fee_tier);
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
                if protocol == super::uniswap::Protocol::V3 {
                    let router_cfg = self.uniswap()?;
                    let token_in = address(&quote.pool_token_in)?;
                    let token_out = address(&quote.pool_token_out)?;
                    let mut call = super::uniswap::v3::exact_input_single(
                        super::uniswap::v3::ExactInputSingle {
                            kind: router_cfg.router_kind,
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
                    if let Some(AuthorizedPermit::Eip2612(permit)) = &eip2612_authorization {
                        let (with_permit, layout) = super::uniswap::v3::with_eip2612(
                            router_cfg.router_kind,
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
                            == U256::from(5),
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
                            == U256::from(5),
                        "DEBT_REVISION_UNSUPPORTED",
                        "Selected debt writes require reviewed variable debt token revision 5 backing math.",
                    )?;
                }
                b.supply_anchor = Some(
                    self.context_anchor(
                        context,
                        &token,
                        owner,
                        address(&reserve.a_token)?,
                        PositionKind::Supply,
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
                    )
                    .await?,
                );
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
                                == U256::from(5),
                            "ATOKEN_REVISION_UNSUPPORTED",
                            "Exact supply backing proof supports reviewed aToken revision 5 only.",
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
                        Pool::supplyCall {
                            asset,
                            amount: qty,
                            onBehalfOf: owner,
                            referralCode: 0,
                        }
                        .abi_encode()
                        .into()
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
                        let (usd, _) = self
                            .quote_protocol_usd(
                                &token.reference,
                                quantity(add(qty, rounding_limit)?),
                            )
                            .await?;
                        let added = mul(u256(&usd)?, U256::from(100))?;
                        let debt = add(account.totalDebtBase, added)?;
                        let projected = if debt.is_zero() {
                            U256::MAX
                        } else {
                            mul(
                                mul(
                                    account.totalCollateralBase,
                                    account.currentLiquidationThreshold,
                                )?,
                                pow10(18)?,
                            )? / mul(U256::from(10000), debt)?
                        };
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
                            maximum_change: quantity(qty),
                        });
                        intent.expected.push(EffectConstraint::Credit {
                            to: resource.clone(),
                            asset: principal_asset.clone(),
                            minimum_credit: quantity(qty),
                        });
                        warnings.push("The minimum health factor is a fresh preflight constraint; variable interest continues after execution.".into());
                        Pool::borrowCall {
                            asset,
                            amount: qty,
                            interestRateMode: U256::from(2),
                            referralCode: 0,
                            onBehalfOf: owner,
                        }
                        .abi_encode()
                        .into()
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
                        Pool::repayCall {
                            asset,
                            amount: if repay_all { U256::MAX } else { qty },
                            interestRateMode: U256::from(2),
                            onBehalfOf: owner,
                        }
                        .abi_encode()
                        .into()
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
                        let recipient = address(r.recipient.as_deref().expect("schema"))?;
                        require(
                            recipient != Address::ZERO,
                            "ZERO_ADDRESS",
                            "Withdraw recipient must be nonzero.",
                        )?;
                        b.recipient = Some(lower_address(recipient));
                        intent.expected.push(EffectConstraint::Position {
                            resource: resource.clone(),
                            position: supply_position.clone(),
                            asset: principal_asset.clone(),
                            direction: Direction::Decrease,
                            maximum_change: quantity(qty),
                        });
                        intent.expected.push(EffectConstraint::Credit {
                            to: super::resource(self.config.chain_id, recipient),
                            asset: principal_asset.clone(),
                            minimum_credit: quantity(minimum),
                        });
                        Pool::withdrawCall {
                            asset,
                            amount: if withdraw_all { U256::MAX } else { qty },
                            to: recipient,
                        }
                        .abi_encode()
                        .into()
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
                        b.enabled = Some(enabled);
                        intent.expected.push(EffectConstraint::Authority {
                            resource: resource.clone(),
                            authority: collateral(&self.aave()?.pool, &token.reference),
                            before: user.collateral_enabled.to_string(),
                            after: enabled.to_string(),
                        });
                        Pool::setUserUseReserveAsCollateralCall {
                            asset,
                            useAsCollateral: enabled,
                        }
                        .abi_encode()
                        .into()
                    }
                    _ => return Err(FinError::validation("Unsupported write.")),
                }
            }
        };
        if matches!(
            operation,
            "wallet.unwrap" | "wallet.transfer" | "uniswap.swap" | "aave.supply" | "aave.repay"
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
                        && fin_evm::rpc_bytes(&json!(tx.calldata))? == calldata,
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
        if operation == "uniswap.swap" {
            let protocol = b.protocol.as_deref().unwrap_or("v3");
            if b.permit_template.is_some() || protocol == "v4" {
                // Universal Router v4 `execute` intentionally returns no amount;
                // typed permit templates are intentionally unsigned in preview.
                // Their exact signature-dependent call is simulated only after
                // approval and the frozen minimum remains enforced by calldata.
            } else {
                let received = if self.uniswap()?.router_kind == RouterKind::SwapRouter02
                    || quote_native_output(&b)
                {
                    let outputs = if self.uniswap()?.router_kind == RouterKind::SwapRouter02 {
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
                    Router::exactInputSingleCall::abi_decode_returns(&bytes).map_err(|_| {
                        error(
                            "SIMULATION_RETURN_INVALID",
                            "preview",
                            "Swap simulation lacks exact output amount.",
                        )
                    })?
                } else {
                    Router::exactInputSingleCall::abi_decode_returns(&simulation).map_err(|_| {
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
            if operation == "uniswap.swap" {
                // Keep the original quote evidence immutable after proving the
                // fresh quote still meets its minimum and impact ceiling.
                b.quote_output = old.quote_output.clone();
                b.price_impact_bps = old.price_impact_bps;
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
            .map_err(|_| FinError::validation("Invalid trusted index."))?;
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
                    allowed_after: if operation == "aave.supply" {
                        vec![before, "true".into()]
                    } else {
                        vec![before, "false".into()]
                    },
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
        let (movement_usd, source, movement_observed) = self
            .quote_usd_observed(&principal_asset.reference, movement)
            .await?;
        let (fee_usd, _, fee_observed) = self.quote_usd_observed("native", quantity(gas)).await?;
        let mut price_observations = vec![movement_observed, fee_observed];
        let mut prices = Vec::new();
        for asset in [&principal_asset, &self.native_asset()]
            .into_iter()
            .chain(b.output.as_ref())
        {
            if prices.iter().any(|p: &AssetPrice| p.asset == *asset) {
                continue;
            }
            let unit = quantity(pow10(asset.decimals)?);
            let (usd, source, observed_at) = self
                .quote_usd_observed(&asset.reference, unit.clone())
                .await?;
            price_observations.push(observed_at.clone());
            prices.push(AssetPrice {
                asset: asset.clone(),
                usd_micros_per_unit: usd,
                unit_atoms: unit,
                observed_at,
                source,
            });
        }
        let observed = now();
        let additional_allocation = if operation == "aave.borrow" {
            let (value, _, observed_at) = self
                .quote_usd_observed(
                    &principal_asset.reference,
                    quantity(add(
                        qty,
                        u256(b.debt_rounding_maximum.as_ref().expect("borrow rounding"))?,
                    )?),
                )
                .await?;
            price_observations.push(observed_at);
            value
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
        let expires = (chrono::Utc::now() + chrono::Duration::seconds(120))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        if operation == "uniswap.swap" {
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
                fin_amm::PermissionRequirement::None
            } else if b.permit_template.is_some() {
                fin_amm::PermissionRequirement::Permit
            } else {
                fin_amm::PermissionRequirement::None
            };
            let mut shared = shared.clone();
            shared.quote.permission = actual_permission;
            // Keep Fin's shared route and exact minimum-output contract as the
            // source for the typed conversion effect; protocol cash proof and
            // the finite gas reservation remain adapter-owned.
            let generic = fin_amm::prepared_operation(&shared, chrono::Utc::now())?;
            intent.expected = generic.intent.expected;
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
                detail: "The unsigned permit template is not simulated as if it were signed. After authorization, Fin signs the typed permit, materializes the frozen transaction and simulates that exact transaction before submission.".into(),
            });
        } else {
            checks.push(Check {
                name: "exact transaction simulation".into(),
                status: CheckStatus::Passed,
                detail: "The exact frozen transaction was simulated during preparation.".into(),
            });
        }
        let summary = if operation == "uniswap.swap" {
            let output = b.output.as_ref().expect("swap output");
            let output_symbol = swap_output_symbol.as_deref().unwrap_or(&output.reference);
            let protocol = b.protocol.as_deref().unwrap_or("v3");
            let route = if protocol == "v4" {
                let key = b.v4_pool_key.as_ref().expect("v4 pool key");
                format!(
                    "Uniswap v4 fee {} ppm, tick spacing {}",
                    key.fee, key.tick_spacing
                )
            } else {
                format!("Uniswap v3 fee {} ppm", b.fee_tier.expect("v3 fee tier"))
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
