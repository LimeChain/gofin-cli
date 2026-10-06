use super::*;
use abi::*;
use fin_evm::alloy::{primitives::B256, sol_types::SolEvent};
use fin_evm::{CanonicalReceipt, quantity, rpc_bytes, u256};

fn events<E: SolEvent>(receipt: &Value, emitter: Address) -> Result<Vec<E>> {
    let logs = receipt["logs"].as_array().ok_or_else(|| {
        error(
            "RECEIPT_LOGS_INVALID",
            "reconciliation",
            "Canonical receipt lacks a log array.",
        )
    })?;
    let mut found = Vec::new();
    for log in logs {
        if address(log["address"].as_str().unwrap_or(""))? != emitter {
            continue;
        }
        let topics = log["topics"]
            .as_array()
            .ok_or_else(|| {
                error(
                    "RECEIPT_LOGS_INVALID",
                    "reconciliation",
                    "Receipt event topics are invalid.",
                )
            })?
            .iter()
            .map(|v| fin_evm::hash(v.as_str().unwrap_or("")))
            .collect::<Result<Vec<B256>>>()?;
        if topics.first() != Some(&E::SIGNATURE_HASH) {
            continue;
        }
        require(
            log["removed"].as_bool() != Some(true)
                && log["transactionHash"] == receipt["transactionHash"]
                && log["blockHash"] == receipt["blockHash"]
                && log["blockNumber"] == receipt["blockNumber"],
            "RECEIPT_LOG_IDENTITY",
            "Event inclusion differs from the original canonical transaction.",
        )?;
        let data = rpc_bytes(&log["data"])?;
        let event = E::decode_raw_log_validate(topics.iter().copied(), &data).map_err(|_| {
            error(
                "RECEIPT_EVENT_INVALID",
                "reconciliation",
                "Event does not decode against the reviewed ABI.",
            )
        })?;
        found.push(event);
    }
    Ok(found)
}
fn transfers(receipt: &Value, token: Address, from: Address, to: Address) -> Result<U256> {
    let mut total = U256::ZERO;
    for event in events::<Erc20::Transfer>(receipt, token)? {
        if event.from == from && event.to == to {
            total = add(total, event.value)?;
        }
    }
    Ok(total)
}
fn exact_one<T>(events: Vec<T>) -> Result<T> {
    require(
        events.len() == 1,
        "RECEIPT_EVENT_MISMATCH",
        "Expected exactly one matching protocol event.",
    )?;
    Ok(events.into_iter().next().expect("one"))
}
/// Accrued expenses are accounted before principal; carry unpaid amounts across partial repayments.
pub(crate) fn settle_debt_costs(
    new_interest: U256,
    deferred_interest: U256,
    deferred_rounding: U256,
    paid: U256,
    cleared: U256,
) -> Result<(U256, U256, U256)> {
    require(
        cleared > U256::ZERO && cleared <= paid,
        "REPAY_BACKING_MISMATCH",
        "Exact debt cleared must be positive and no greater than cash paid.",
    )?;
    let unpaid = add(new_interest, deferred_interest)?;
    let paid_interest = unpaid.min(cleared);
    let paid_deferred = deferred_rounding.min(sub(cleared, paid_interest)?);
    Ok((
        add(add(paid_interest, paid_deferred)?, sub(paid, cleared)?)?,
        sub(unpaid, paid_interest)?,
        sub(deferred_rounding, paid_deferred)?,
    ))
}
fn debt_location(b: &Binding) -> AccountingLocation {
    AccountingLocation::Position {
        resource: b.resource.clone(),
        position: position(
            b.pool.as_deref().expect("pool"),
            &b.token.reference,
            "variable-debt",
        ),
        position_kind: PositionKind::VariableDebt,
    }
}
fn supply_location(b: &Binding) -> AccountingLocation {
    AccountingLocation::Position {
        resource: b.resource.clone(),
        position: position(
            b.pool.as_deref().expect("pool"),
            &b.token.reference,
            "supply",
        ),
        position_kind: PositionKind::Supply,
    }
}
fn origin(
    from: AccountingLocation,
    to: AccountingLocation,
    asset: &AssetId,
    debit: U256,
    credit: U256,
) -> FinancialEffect {
    FinancialEffect::OriginMovement {
        from,
        to,
        debit: amount(asset, debit),
        credit: amount(asset, credit),
    }
}

/// Revision-5 mint backing is floor-scaled, never a generic dust tolerance.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupplyBacking {
    pub scaled_minted: Quantity,
    pub backed_principal: Quantity,
    pub rounding_loss: Quantity,
    pub mint_balance_increase: Quantity,
    pub mint_value: Quantity,
    pub supplied_after: Quantity,
    pub yield_after_mint: Quantity,
}
pub fn revision_five_backing(
    input: Quantity,
    scaled_before: Quantity,
    scaled_after: Quantity,
    previous_index: Quantity,
    mint_index: Quantity,
    income_index: Quantity,
) -> Result<SupplyBacking> {
    let q = u256(&input)?;
    let before = u256(&scaled_before)?;
    let after = u256(&scaled_after)?;
    let previous = u256(&previous_index)?;
    let index = u256(&mint_index)?;
    let income = u256(&income_index)?;
    let ray = pow10(27)?;
    require(
        index >= ray
            && index < (U256::from(1) << 128)
            && income >= index
            && previous <= index
            && (previous >= ray || (previous.is_zero() && before.is_zero()))
            && after < (U256::from(1) << 120),
        "SUPPLY_INDEX_INVALID",
        "Scaled mint/index fields exceed reviewed revision-5 invariants.",
    )?;
    let minted = mul(q, ray)? / index;
    require(
        minted > U256::ZERO && after == add(before, minted)?,
        "SUPPLY_SCALED_MISMATCH",
        "Scaled mint does not match exact revision-5 floor conversion.",
    )?;
    let before_at_mint = mul(before, index)? / ray;
    let prior_balance = mul(before, previous)? / ray;
    let total_at_mint = mul(after, index)? / ray;
    let supplied_after = mul(after, income)? / ray;
    let principal = sub(total_at_mint, before_at_mint)?;
    require(
        principal <= q,
        "SUPPLY_BACKING_MISMATCH",
        "Minted backing exceeds exact input.",
    )?;
    Ok(SupplyBacking {
        scaled_minted: quantity(minted),
        backed_principal: quantity(principal),
        rounding_loss: quantity(sub(q, principal)?),
        mint_balance_increase: quantity(sub(before_at_mint, prior_balance)?),
        mint_value: quantity(sub(total_at_mint, prior_balance)?),
        supplied_after: quantity(supplied_after),
        yield_after_mint: quantity(sub(supplied_after, total_at_mint)?),
    })
}

impl EthereumAdapter {
    /// Read-only proof of venue-controlled Ethereum funding. Bounds use exact decimal asset units.
    /// Native funding supports direct externally-owned recipients; ERC-20 funding proves net receipt logs.
    pub async fn verify_external_credit(
        &self,
        tx_hash: &str,
        recipient: &str,
        asset: &AssetId,
        minimum_amount: &str,
        maximum_amount: &str,
        confirmations: u32,
    ) -> Result<ExternalCredit> {
        require(
            u64::from(confirmations) >= self.config.confirmations && confirmations > 0,
            "FINALITY_LIMIT",
            "External funding proof cannot weaken configured confirmations.",
        )?;
        require(
            asset.network == format!("ethereum:{}", self.config.chain_id),
            "WRONG_CHAIN",
            "Funding asset belongs to another immutable network.",
        )?;
        let recipient = address(recipient)?;
        require(
            recipient != Address::ZERO,
            "ZERO_ADDRESS",
            "Funding recipient must be nonzero.",
        )?;
        let expected_hash = fin_evm::hash(tx_hash)?;
        let token = self.token(&asset.reference, None).await?;
        require(
            token.asset(self) == *asset,
            "ASSET_IDENTITY_MISMATCH",
            "Funding asset precision or canonical reference differs from deployed token.",
        )?;
        let minimum = exact(minimum_amount, asset.decimals)?;
        let maximum = exact(maximum_amount, asset.decimals)?;
        require(
            minimum > U256::ZERO && minimum <= maximum,
            "FUNDING_BOUNDS_INVALID",
            "Funding requires positive finite ordered amount bounds.",
        )?;
        self.runtime.assert_chain().await?;
        let receipt = self
            .runtime
            .rpc
            .request("eth_getTransactionReceipt", json!([tx_hash]))
            .await?;
        let pending = || ExternalCredit {
            status: Status::Pending,
            amount_atoms: None,
            evidence: json!({"original_hash":tx_hash,"canonical_confirmed":false}),
        };
        if receipt.is_null() {
            return Ok(pending());
        }
        let transaction = self
            .runtime
            .rpc
            .request("eth_getTransactionByHash", json!([tx_hash]))
            .await?;
        if transaction.is_null() {
            return Ok(pending());
        }
        let h = |v: &Value| fin_evm::hash(v.as_str().unwrap_or(""));
        require(
            h(&receipt["transactionHash"])? == expected_hash
                && h(&transaction["hash"])? == expected_hash
                && fin_evm::rpc_u64(&transaction["chainId"])? == self.config.chain_id,
            "FUNDING_IDENTITY_MISMATCH",
            "Funding transaction does not match its original hash or configured chain.",
        )?;
        let block = fin_evm::rpc_u64(&receipt["blockNumber"])?;
        let block_hash = h(&receipt["blockHash"])?;
        require(
            h(&transaction["blockHash"])? == block_hash
                && fin_evm::rpc_u64(&transaction["blockNumber"])? == block,
            "FUNDING_INCLUSION_MISMATCH",
            "Funding transaction and receipt inclusion disagree.",
        )?;
        let header = self
            .runtime
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([fin_evm::block_tag(Some(block)), false]),
            )
            .await?;
        if header.is_null()
            || h(&header["hash"])? != block_hash
            || fin_evm::rpc_u64(&header["number"])? != block
        {
            return Ok(pending());
        }
        let latest = fin_evm::rpc_u64(
            &self
                .runtime
                .rpc
                .request("eth_blockNumber", json!([]))
                .await?,
        )?;
        let observed_confirmations = if latest >= block {
            latest - block + 1
        } else {
            0
        };
        if observed_confirmations < u64::from(confirmations) {
            return Ok(pending());
        }
        let status = fin_evm::rpc_u64(&receipt["status"])?;
        require(
            status <= 1,
            "FUNDING_STATUS_INVALID",
            "Funding receipt status is not success or revert.",
        )?;
        let evidence = json!({"original_hash":tx_hash,"chain_id":self.config.chain_id,"block":block,"block_hash":format!("{block_hash:#x}"),"confirmations":observed_confirmations,"recipient":lower_address(recipient),"asset":asset,"canonical_confirmed":true});
        if status == 0 {
            return Ok(ExternalCredit {
                status: Status::Failed,
                amount_atoms: None,
                evidence,
            });
        }
        let credited = if asset.reference == "native" {
            require(
                address(transaction["to"].as_str().unwrap_or(""))? == recipient,
                "FUNDING_RECIPIENT_MISMATCH",
                "Native payout transaction has another direct recipient.",
            )?;
            let code = rpc_bytes(
                &self
                    .runtime
                    .rpc
                    .request(
                        "eth_getCode",
                        json!([lower_address(recipient), fin_evm::block_tag(Some(block))]),
                    )
                    .await?,
            )?;
            require(
                code.is_empty(),
                "NATIVE_FUNDING_UNSUPPORTED",
                "Native funding proof supports direct externally-owned recipients.",
            )?;
            fin_evm::rpc_u256(&transaction["value"])?
        } else {
            let mut incoming = U256::ZERO;
            let mut outgoing = U256::ZERO;
            for transfer in events::<Erc20::Transfer>(&receipt, address(&asset.reference)?)? {
                if transfer.to == recipient {
                    incoming = add(incoming, transfer.value)?
                }
                if transfer.from == recipient {
                    outgoing = add(outgoing, transfer.value)?
                }
            }
            sub(incoming, outgoing)?
        };
        require(
            credited >= minimum && credited <= maximum,
            "FUNDING_AMOUNT_MISMATCH",
            "Canonical net funding credit violates the exact venue payout bounds.",
        )?;
        require(
            self.balance_of(&token, recipient, Some(block)).await? >= credited,
            "FUNDING_HOLDING_MISMATCH",
            "Recipient inclusion-block holdings do not cover its proved net credit.",
        )?;
        let mut evidence = evidence;
        evidence["received_atoms"] = json!(quantity(credited));
        evidence["proof_method"] = json!(if asset.reference == "native" {
            "canonical-direct-native-value"
        } else {
            "canonical-erc20-net-recipient-transfers"
        });
        Ok(ExternalCredit {
            status: Status::Completed,
            amount_atoms: Some(quantity(credited)),
            evidence,
        })
    }
    pub(crate) async fn reconcile_operation(
        &self,
        record: &OperationRecord,
    ) -> Result<Reconciliation> {
        let submission = record.submission.as_ref().ok_or_else(|| {
            error(
                "SUBMISSION_REQUIRED",
                "reconciliation",
                "Original durable submission identity is missing.",
            )
        })?;
        let preview = record.preview.as_ref().ok_or_else(|| {
            error(
                "BINDING_REQUIRED",
                "reconciliation",
                "Original preview is missing.",
            )
        })?;
        require(
            preview.operation == record.operation && preview.input == record.input,
            "BINDING_IDENTITY_MISMATCH",
            "Record operation/input differs from original preview.",
        )?;
        let b: Binding = serde_json::from_value(preview.binding.clone()).map_err(|_| {
            error(
                "BINDING_INVALID",
                "reconciliation",
                "Original transaction binding is malformed.",
            )
        })?;
        require(
            submission.data["transaction"]
                == serde_json::to_value(&b.transaction).expect("serializable")
                && submission.data["metadata"]["operation"] == json!(record.operation)
                && submission.data["metadata"]["resource"] == json!(b.resource),
            "SUBMISSION_IDENTITY_MISMATCH",
            "Durable submission differs from the original financial binding.",
        )?;
        let receipt = match self
            .runtime
            .receipt(submission, &b.transaction, self.config.confirmations)
            .await?
        {
            Some(r) => r,
            None => {
                return Ok(Reconciliation {
                    status: Status::Pending,
                    evidence: json!({"original_hash":submission.reference,"canonical_confirmed":false}),
                    verified: None,
                });
            }
        };
        if self.config.require_finalized {
            let finalized = self
                .runtime
                .rpc
                .request("eth_getBlockByNumber", json!(["finalized", false]))
                .await;
            let finalized_height = finalized
                .as_ref()
                .ok()
                .filter(|header| !header.is_null())
                .and_then(|header| {
                    fin_evm::rpc_u64(&header["number"])
                        .ok()
                        .filter(|_| fin_evm::hash(header["hash"].as_str().unwrap_or("")).is_ok())
                });
            if finalized_height.is_none_or(|height| height < receipt.block_number) {
                return Ok(Reconciliation {
                    status: Status::Pending,
                    evidence: json!({"original_hash":submission.reference,"canonical_confirmed":true,"finality":"awaiting-rpc-finalized","finalized_height":finalized_height}),
                    verified: None,
                });
            }
            // Finalized height alone does not bind the original transaction's
            // inclusion. Observe that block again after the finalized marker.
            let canonical = self
                .runtime
                .rpc
                .request(
                    "eth_getBlockByNumber",
                    json!([format!("{:#x}", receipt.block_number), false]),
                )
                .await?;
            if canonical["hash"] != json!(format!("{:#x}", receipt.block_hash))
                || fin_evm::rpc_u64(&canonical["number"])? != receipt.block_number
            {
                return Ok(Reconciliation {
                    status: Status::Pending,
                    evidence: json!({"original_hash":submission.reference,"canonical_confirmed":false,"finality":"inclusion-changed"}),
                    verified: None,
                });
            }
        }
        let mut effects = vec![FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: b.resource.clone(),
            },
            amount: amount(&self.native_asset(), receipt.fee),
            category: "ethereum_gas".into(),
            treatment: CostTreatment::AdditionalDebit,
        }];
        let mut details = json!({"block_number":receipt.block_number,"block_hash":format!("{:#x}",receipt.block_hash),"confirmations":receipt.confirmations,"original_hash":submission.reference,"original_envelope_verified":true,"success":receipt.success,"fee_wei":quantity(receipt.fee),"finality":if self.config.require_finalized {"rpc-finalized"} else {"confirmation-count"}});
        if receipt.success {
            self.success_effects(&record.operation, &b, &receipt, &mut effects, &mut details)
                .await?;
        }
        let mut bundle = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects,
            evidence: details.clone(),
            observed_at: now(),
            proof_digest: String::new(),
        };
        bundle.proof_digest = verified_effect_digest(&bundle)?;
        Ok(Reconciliation {
            status: if receipt.success {
                Status::Completed
            } else {
                Status::Failed
            },
            evidence: details,
            verified: Some(bundle),
        })
    }
    async fn success_effects(
        &self,
        operation: &str,
        b: &Binding,
        r: &CanonicalReceipt,
        effects: &mut Vec<FinancialEffect>,
        details: &mut Value,
    ) -> Result<()> {
        let owner = address(&b.transaction.sender)?;
        let qty = u256(&b.units)?;
        let receipt = &r.receipt;
        let token_addr = address(&b.token.reference).unwrap_or(Address::ZERO);
        let cash = AccountingLocation::Cash {
            resource: b.resource.clone(),
        };
        match operation {
            "wallet.transfer" => {
                let recipient = address(b.recipient.as_deref().expect("recipient"))?;
                if b.token.reference != "native" {
                    require(
                        transfers(receipt, token_addr, owner, recipient)? == qty,
                        "TRANSFER_PROOF_MISMATCH",
                        "Exact ERC-20 transfer event is missing or differs from input.",
                    )?;
                }
                effects.push(FinancialEffect::Transfer {
                    from: b.resource.clone(),
                    to: resource(self.config.chain_id, recipient),
                    debit: amount(&b.token, qty),
                    credit: amount(&b.token, qty),
                });
            }
            "wallet.wrap" | "wallet.unwrap" => {
                let weth = address(self.weth()?)?;
                if operation == "wallet.wrap" {
                    let e = exact_one(
                        events::<Weth::Deposit>(receipt, weth)?
                            .into_iter()
                            .filter(|e| e.dst == owner)
                            .collect(),
                    )?;
                    require(
                        e.wad == qty,
                        "WRAP_PROOF_MISMATCH",
                        "WETH deposit event differs from original native value.",
                    )?;
                } else {
                    let e = exact_one(
                        events::<Weth::Withdrawal>(receipt, weth)?
                            .into_iter()
                            .filter(|e| e.src == owner)
                            .collect(),
                    )?;
                    require(
                        e.wad == qty,
                        "WRAP_PROOF_MISMATCH",
                        "WETH withdrawal event differs from exact input.",
                    )?;
                }
                effects.push(FinancialEffect::Conversion {
                    resource: b.resource.clone(),
                    spent: amount(&b.token, qty),
                    received: amount(b.output.as_ref().expect("output"), qty),
                });
            }
            "wallet.approve" => {
                let spender = address(b.spender.as_deref().expect("spender"))?;
                let e = exact_one(
                    events::<Erc20::Approval>(receipt, token_addr)?
                        .into_iter()
                        .filter(|e| e.owner == owner && e.spender == spender)
                        .collect(),
                )?;
                require(
                    e.value == qty
                        && self
                            .runtime
                            .call(
                                token_addr,
                                &Erc20::allowanceCall { owner, spender },
                                Some(r.block_number),
                            )
                            .await?
                            == qty,
                    "ALLOWANCE_PROOF_MISMATCH",
                    "Approval event or inclusion-block authority differs from exact allowance.",
                )?;
                effects.push(FinancialEffect::AuthorityChange {
                    resource: b.resource.clone(),
                    authority: authority(
                        &b.token.reference,
                        b.spender.as_deref().expect("spender"),
                    ),
                    before: b.allowance_before.as_ref().expect("before").to_string(),
                    after: qty.to_string(),
                });
            }
            "uniswap.swap" => {
                let pool = address(b.pool.as_deref().expect("pool"))?;
                let recipient = address(b.recipient.as_deref().expect("recipient"))?;
                let router = address(&b.transaction.to)?;
                let swap = exact_one(
                    events::<UniswapPool::Swap>(receipt, pool)?
                        .into_iter()
                        .filter(|e| e.sender == router && e.recipient == recipient)
                        .collect(),
                )?;
                require(
                    (swap.amount0.is_positive() && swap.amount1.is_negative())
                        || (swap.amount1.is_positive() && swap.amount0.is_negative()),
                    "SWAP_PROOF_MISMATCH",
                    "Pool swap event does not contain one input and one output.",
                )?;
                let output = b.output.as_ref().expect("output");
                let credited = transfers(receipt, address(&output.reference)?, pool, recipient)?;
                require(
                    transfers(receipt, token_addr, owner, pool)? == qty
                        && credited >= u256(b.minimum_output.as_ref().expect("minimum"))?,
                    "SWAP_PROOF_MISMATCH",
                    "Pool cash legs differ from exact input or minimum output.",
                )?;
                effects.push(FinancialEffect::Conversion {
                    resource: b.resource.clone(),
                    spent: amount(&b.token, qty),
                    received: amount(output, credited),
                });
                details["actual_output"] = json!(quantity(credited));
            }
            _ => {
                let pool = address(b.pool.as_deref().expect("pool"))?;
                let token = reads::Token {
                    reference: b.token.reference.clone(),
                    decimals: b.token.decimals,
                    symbol: String::new(),
                };
                let post = self
                    .user_reserve(&token, owner, Some(r.block_number))
                    .await?;
                let a_token = address(b.a_token.as_deref().expect("aToken"))?;
                let supply_position = position(
                    b.pool.as_deref().expect("pool"),
                    &b.token.reference,
                    "supply",
                );
                let debt_position = position(
                    b.pool.as_deref().expect("pool"),
                    &b.token.reference,
                    "variable-debt",
                );
                match operation {
                    "aave.supply" => {
                        let supply = exact_one(
                            events::<Pool::Supply>(receipt, pool)?
                                .into_iter()
                                .filter(|e| {
                                    e.reserve == token_addr
                                        && e.user == owner
                                        && e.onBehalfOf == owner
                                })
                                .collect(),
                        )?;
                        require(
                            supply.amount == qty
                                && transfers(receipt, token_addr, owner, a_token)? == qty,
                            "SUPPLY_PROOF_MISMATCH",
                            "Supply event or exact underlying cash transfer differs.",
                        )?;
                        require(
                            r.block_number > 0,
                            "SUPPLY_PROOF_MISMATCH",
                            "Supply proof requires a previous inclusion block.",
                        )?;
                        let mint = exact_one(
                            events::<AToken::Mint>(receipt, a_token)?
                                .into_iter()
                                .filter(|e| e.caller == owner && e.onBehalfOf == owner)
                                .collect(),
                        )?;
                        let before = self
                            .runtime
                            .call(
                                a_token,
                                &AToken::scaledBalanceOfCall { user: owner },
                                Some(r.block_number - 1),
                            )
                            .await?;
                        let after = self
                            .runtime
                            .call(
                                a_token,
                                &AToken::scaledBalanceOfCall { user: owner },
                                Some(r.block_number),
                            )
                            .await?;
                        let previous = self
                            .runtime
                            .call(
                                a_token,
                                &AToken::getPreviousIndexCall { user: owner },
                                Some(r.block_number - 1),
                            )
                            .await?;
                        let post_index = self
                            .runtime
                            .call(
                                a_token,
                                &AToken::getPreviousIndexCall { user: owner },
                                Some(r.block_number),
                            )
                            .await?;
                        let income = self
                            .runtime
                            .call(
                                pool,
                                &Pool::getReserveNormalizedIncomeCall { asset: token_addr },
                                Some(r.block_number),
                            )
                            .await?;
                        require(
                            self.runtime
                                .call(
                                    a_token,
                                    &AToken::ATOKEN_REVISIONCall {},
                                    Some(r.block_number),
                                )
                                .await?
                                == U256::from(5)
                                && post_index == mint.index
                                && quantity(before) == *b.scaled_before.as_ref().expect("scaled"),
                            "SUPPLY_PROOF_MISMATCH",
                            "aToken revision, original scaled balance or minted index changed.",
                        )?;
                        let backing = revision_five_backing(
                            b.units.clone(),
                            quantity(before),
                            quantity(after),
                            quantity(previous),
                            quantity(mint.index),
                            quantity(income),
                        )?;
                        require(
                            quantity(mint.balanceIncrease) == backing.mint_balance_increase
                                && quantity(mint.value) == backing.mint_value
                                && transfers(receipt, a_token, Address::ZERO, owner)?
                                    == u256(&backing.mint_value)?
                                && post.supplied == backing.supplied_after,
                            "SUPPLY_PROOF_MISMATCH",
                            "Mint event/backing or inclusion claim violates floor-scaled accounting.",
                        )?;
                        let backed = u256(&backing.backed_principal)?;
                        effects.push(FinancialEffect::CashDebit {
                            resource: b.resource.clone(),
                            amount: amount(&b.token, qty),
                        });
                        effects.push(FinancialEffect::PositionChange {
                            resource: b.resource.clone(),
                            position: supply_position.clone(),
                            asset: b.token.clone(),
                            delta: SignedQuantity::positive(&backing.backed_principal),
                            scaled_receipt: Some(quantity(after)),
                            anchor: Some(
                                self.make_anchor(
                                    &token,
                                    owner,
                                    a_token,
                                    PositionKind::Supply,
                                    r.block_number,
                                    U256::ZERO,
                                )
                                .await?,
                            ),
                        });
                        effects.push(origin(
                            cash.clone(),
                            supply_location(b),
                            &b.token,
                            qty,
                            backed,
                        ));
                        if !backing.rounding_loss.is_zero() {
                            effects.push(FinancialEffect::Cost {
                                location: AccountingLocation::Cash {
                                    resource: b.resource.clone(),
                                },
                                amount: Amount {
                                    asset: b.token.clone(),
                                    atoms: backing.rounding_loss.clone(),
                                },
                                category: "aave_revision_5_backing_rounding".into(),
                                treatment: CostTreatment::IncludedInDebit,
                            });
                        }
                        let accrued = self
                            .accrued_before(
                                b.supply_anchor.as_ref().expect("anchor"),
                                &token,
                                owner,
                                a_token,
                                mint.index,
                                r.block_number - 1,
                            )
                            .await?;
                        let total_yield = add(accrued, u256(&backing.yield_after_mint)?)?;
                        effects.insert(
                            effects
                                .iter()
                                .position(|effect| {
                                    matches!(effect, FinancialEffect::PositionChange { .. })
                                })
                                .expect("position effect"),
                            FinancialEffect::Accrual {
                                resource: b.resource.clone(),
                                position: supply_position.clone(),
                                asset: b.token.clone(),
                                position_kind: PositionKind::Supply,
                                delta: SignedQuantity::positive(&quantity(total_yield)),
                            },
                        );
                        details["supply_backing"] = json!(backing);
                    }
                    "aave.borrow" => {
                        let e = exact_one(
                            events::<Pool::Borrow>(receipt, pool)?
                                .into_iter()
                                .filter(|e| {
                                    e.reserve == token_addr
                                        && e.user == owner
                                        && e.onBehalfOf == owner
                                })
                                .collect(),
                        )?;
                        require(
                            e.amount == qty
                                && e.interestRateMode == 2
                                && transfers(receipt, token_addr, a_token, owner)? == qty,
                            "BORROW_PROOF_MISMATCH",
                            "Variable borrow event/cash credit differs from exact amount.",
                        )?;
                        let debt_token = address(b.debt_token.as_deref().expect("debt"))?;
                        require(
                            r.block_number > 0,
                            "DEBT_PROOF_MISMATCH",
                            "Debt proof requires a prior block.",
                        )?;
                        let prior = b.debt_anchor.as_ref().expect("anchor");
                        let minted = exact_one(
                            events::<AToken::Mint>(receipt, debt_token)?
                                .into_iter()
                                .filter(|e| e.caller == owner && e.onBehalfOf == owner)
                                .collect(),
                        )?;
                        let index = self
                            .runtime
                            .call(
                                pool,
                                &Pool::getReserveNormalizedVariableDebtCall { asset: token_addr },
                                Some(r.block_number),
                            )
                            .await?;
                        let before = self
                            .runtime
                            .call(
                                debt_token,
                                &AToken::scaledBalanceOfCall { user: owner },
                                Some(r.block_number - 1),
                            )
                            .await?;
                        let after = self
                            .runtime
                            .call(
                                debt_token,
                                &AToken::scaledBalanceOfCall { user: owner },
                                Some(r.block_number),
                            )
                            .await?;
                        let ray = pow10(27)?;
                        require(
                            minted.index >= ray && index >= minted.index,
                            "DEBT_INDEX_INVALID",
                            "Debt mint index must be a valid positive normalized ray.",
                        )?;
                        let scaled_minted =
                            add(mul(qty, ray)?, minted.index - U256::from(1))? / minted.index;
                        require(
                            minted.index >= ray
                                && index >= minted.index
                                && after == add(before, scaled_minted)?,
                            "DEBT_SCALED_MISMATCH",
                            "Borrow scaled debt mint differs from exact ceiling conversion.",
                        )?;
                        let pre_claim =
                            anchors::backing(before, minted.index, &PositionKind::VariableDebt)?;
                        let post_claim_at_mint =
                            anchors::backing(after, minted.index, &PositionKind::VariableDebt)?;
                        let added = sub(post_claim_at_mint, pre_claim)?;
                        require(
                            added >= qty,
                            "DEBT_BACKING_MISMATCH",
                            "Debt backing is below borrowed cash.",
                        )?;
                        let rounding = sub(added, qty)?;
                        require(
                            rounding
                                <= u256(
                                    b.debt_rounding_maximum
                                        .as_ref()
                                        .expect("borrow rounding bound"),
                                )?,
                            "DEBT_ROUNDING_LIMIT",
                            "Proved debt ceiling exceeds its finite approved exposure bound.",
                        )?;
                        let previous = self
                            .runtime
                            .call(
                                debt_token,
                                &AToken::getPreviousIndexCall { user: owner },
                                Some(r.block_number - 1),
                            )
                            .await?;
                        let prior_claim =
                            anchors::backing(before, previous, &PositionKind::VariableDebt)?;
                        require(
                            minted.value == sub(post_claim_at_mint, prior_claim)?
                                && minted.balanceIncrease == sub(pre_claim, prior_claim)?
                                && transfers(receipt, debt_token, Address::ZERO, owner)?
                                    == minted.value,
                            "DEBT_MINT_PROOF_MISMATCH",
                            "Debt mint event does not prove exact principal and index accrual.",
                        )?;
                        let pre_accrual = self
                            .accrued_before(
                                prior,
                                &token,
                                owner,
                                debt_token,
                                minted.index,
                                r.block_number - 1,
                            )
                            .await?;
                        let extra_after = sub(
                            anchors::backing(after, index, &PositionKind::VariableDebt)?,
                            post_claim_at_mint,
                        )?;
                        let total_accrual = add(add(pre_accrual, rounding)?, extra_after)?;
                        let deferred: Quantity = serde_json::from_value(
                            prior.evidence["deferred_unpaid_rounding"].clone(),
                        )
                        .map_err(|_| FinError::validation("Invalid proved rounding."))?;
                        let deferred_interest: Quantity = serde_json::from_value(
                            prior.evidence["deferred_unpaid_interest"].clone(),
                        )
                        .map_err(|_| FinError::validation("Invalid proved unpaid interest."))?;
                        let anchor = self
                            .make_anchor_with_costs(
                                &token,
                                owner,
                                debt_token,
                                PositionKind::VariableDebt,
                                r.block_number,
                                anchors::DeferredExpenses {
                                    rounding: add(u256(&deferred)?, rounding)?,
                                    interest: add(
                                        add(u256(&deferred_interest)?, pre_accrual)?,
                                        extra_after,
                                    )?,
                                },
                            )
                            .await?;
                        require(
                            anchor.principal.atoms == post.variable_debt,
                            "DEBT_POST_STATE_MISMATCH",
                            "Debt receipt backing disagrees with inclusion-block user position.",
                        )?;
                        effects.push(FinancialEffect::Accrual {
                            resource: b.resource.clone(),
                            position: debt_position.clone(),
                            asset: b.token.clone(),
                            position_kind: PositionKind::VariableDebt,
                            delta: SignedQuantity::positive(&quantity(total_accrual)),
                        });
                        effects.push(FinancialEffect::DebtChange {
                            resource: b.resource.clone(),
                            debt: debt_position,
                            asset: b.token.clone(),
                            delta: SignedQuantity::positive(&b.units),
                            anchor: Some(anchor),
                        });
                        effects.push(FinancialEffect::UsableCredit {
                            to: b.resource.clone(),
                            amount: amount(&b.token, qty),
                        });
                        effects.push(origin(debt_location(b), cash.clone(), &b.token, qty, qty));
                        details["debt_rounding_deferred"] = json!(quantity(rounding));
                    }
                    "aave.repay" => {
                        let e = exact_one(
                            events::<Pool::Repay>(receipt, pool)?
                                .into_iter()
                                .filter(|e| {
                                    e.reserve == token_addr && e.user == owner && e.repayer == owner
                                })
                                .collect(),
                        )?;
                        let actual = e.amount;
                        require(
                            !e.useATokens
                                && actual > U256::ZERO
                                && actual <= qty
                                && transfers(receipt, token_addr, owner, a_token)? == actual
                                && (!b.repay_all || post.variable_debt.is_zero()),
                            "REPAY_PROOF_MISMATCH",
                            "Repayment event/cash debit or remaining debt violates bounded repayment.",
                        )?;
                        require(
                            r.block_number > 0,
                            "DEBT_PROOF_MISMATCH",
                            "Debt proof requires a prior block.",
                        )?;
                        let debt_token = address(b.debt_token.as_deref().expect("debt"))?;
                        let index = self
                            .runtime
                            .call(
                                pool,
                                &Pool::getReserveNormalizedVariableDebtCall { asset: token_addr },
                                Some(r.block_number),
                            )
                            .await?;
                        let before = self
                            .runtime
                            .call(
                                debt_token,
                                &AToken::scaledBalanceOfCall { user: owner },
                                Some(r.block_number - 1),
                            )
                            .await?;
                        let after = self
                            .runtime
                            .call(
                                debt_token,
                                &AToken::scaledBalanceOfCall { user: owner },
                                Some(r.block_number),
                            )
                            .await?;
                        let ray = pow10(27)?;
                        let before_claim =
                            anchors::backing(before, index, &PositionKind::VariableDebt)?;
                        let after_claim =
                            anchors::backing(after, index, &PositionKind::VariableDebt)?;
                        let burned = if actual == before_claim {
                            before
                        } else {
                            mul(actual, ray)? / index
                        };
                        require(
                            after == sub(before, burned)?
                                && quantity(after_claim) == post.variable_debt,
                            "DEBT_BURN_PROOF_MISMATCH",
                            "Repayment scaled burn or post debt differs from exact floor rule.",
                        )?;
                        let cleared = sub(before_claim, after_claim)?;
                        require(
                            cleared > U256::ZERO && cleared <= actual,
                            "REPAY_BACKING_MISMATCH",
                            "Exact debt extinguishment exceeds actual repayment.",
                        )?;
                        let prior = b.debt_anchor.as_ref().expect("anchor");
                        let interest = self
                            .accrued_before(
                                prior,
                                &token,
                                owner,
                                debt_token,
                                index,
                                r.block_number - 1,
                            )
                            .await?;
                        let deferred: Quantity = serde_json::from_value(
                            prior.evidence["deferred_unpaid_rounding"].clone(),
                        )
                        .map_err(|_| FinError::validation("Invalid deferred rounding."))?;
                        let deferred_interest: Quantity = serde_json::from_value(
                            prior.evidence["deferred_unpaid_interest"].clone(),
                        )
                        .map_err(|_| FinError::validation("Invalid proved unpaid interest."))?;
                        let (total_cost, remaining_interest, remaining_deferred) =
                            settle_debt_costs(
                                interest,
                                u256(&deferred_interest)?,
                                u256(&deferred)?,
                                actual,
                                cleared,
                            )?;
                        let anchor = self
                            .make_anchor_with_costs(
                                &token,
                                owner,
                                debt_token,
                                PositionKind::VariableDebt,
                                r.block_number,
                                anchors::DeferredExpenses {
                                    rounding: remaining_deferred,
                                    interest: remaining_interest,
                                },
                            )
                            .await?;
                        effects.push(FinancialEffect::Accrual {
                            resource: b.resource.clone(),
                            position: debt_position.clone(),
                            asset: b.token.clone(),
                            position_kind: PositionKind::VariableDebt,
                            delta: SignedQuantity::positive(&quantity(interest)),
                        });
                        effects.push(FinancialEffect::CashDebit {
                            resource: b.resource.clone(),
                            amount: amount(&b.token, actual),
                        });
                        effects.push(FinancialEffect::DebtChange {
                            resource: b.resource.clone(),
                            debt: debt_position,
                            asset: b.token.clone(),
                            delta: SignedQuantity::negative(&quantity(cleared)),
                            anchor: Some(anchor),
                        });
                        effects.push(origin(
                            cash.clone(),
                            debt_location(b),
                            &b.token,
                            actual,
                            cleared,
                        ));
                        if total_cost > U256::ZERO {
                            effects.push(FinancialEffect::Cost {
                                location: cash.clone(),
                                amount: amount(&b.token, total_cost),
                                category: "aave_variable_interest_and_proved_rounding".into(),
                                treatment: CostTreatment::IncludedInDebit,
                            });
                        }
                        details["actual_repayment"] = json!({"paid":quantity(actual),"debt_cleared":quantity(cleared),"interest_and_rounding_cost":quantity(total_cost)});
                    }
                    "aave.withdraw" => {
                        let recipient = address(b.recipient.as_deref().expect("recipient"))?;
                        let e = exact_one(
                            events::<Pool::Withdraw>(receipt, pool)?
                                .into_iter()
                                .filter(|e| {
                                    e.reserve == token_addr && e.user == owner && e.to == recipient
                                })
                                .collect(),
                        )?;
                        let qty = if b.all_amount {
                            require(
                                e.amount > U256::ZERO
                                    && e.amount <= qty
                                    && e.amount
                                        >= u256(b.minimum_output.as_ref().expect("minimum"))?,
                                "WITHDRAW_MAXIMUM",
                                "Full withdrawal credit lies outside the frozen finite return bound.",
                            )?;
                            e.amount
                        } else {
                            qty
                        };
                        require(
                            e.amount == qty
                                && transfers(receipt, token_addr, a_token, recipient)? == qty,
                            "WITHDRAW_PROOF_MISMATCH",
                            "Withdraw event or underlying cash credit differs from exact input.",
                        )?;
                        require(
                            r.block_number > 0,
                            "WITHDRAW_PROOF_MISMATCH",
                            "Withdraw proof requires a prior block.",
                        )?;
                        let before = self
                            .runtime
                            .call(
                                a_token,
                                &AToken::scaledBalanceOfCall { user: owner },
                                Some(r.block_number - 1),
                            )
                            .await?;
                        let after = self
                            .runtime
                            .call(
                                a_token,
                                &AToken::scaledBalanceOfCall { user: owner },
                                Some(r.block_number),
                            )
                            .await?;
                        let index = self
                            .runtime
                            .call(
                                pool,
                                &Pool::getReserveNormalizedIncomeCall { asset: token_addr },
                                Some(r.block_number),
                            )
                            .await?;
                        let ray = pow10(27)?;
                        require(
                            index >= ray,
                            "WITHDRAW_INDEX_INVALID",
                            "Withdraw index must be a positive normalized ray.",
                        )?;
                        let burned = add(mul(qty, ray)?, index - U256::from(1))? / index;
                        require(
                            after == sub(before, burned)?,
                            "WITHDRAW_SCALED_MISMATCH",
                            "Withdraw scaled burn does not match exact ceiling rule.",
                        )?;
                        let before_claim = anchors::backing(before, index, &PositionKind::Supply)?;
                        let after_claim = anchors::backing(after, index, &PositionKind::Supply)?;
                        if b.all_amount {
                            require(
                                after.is_zero()
                                    && after_claim.is_zero()
                                    && post.supplied.is_zero()
                                    && before_claim == qty,
                                "WITHDRAW_NOT_CLOSED",
                                "Full withdrawal must close the complete scaled claim and credit its exact physical backing.",
                            )?;
                        }
                        require(
                            quantity(after_claim) == post.supplied,
                            "WITHDRAW_BACKING_MISMATCH",
                            "Post supplied claim disagrees with exact scaled backing.",
                        )?;
                        let decreased = sub(before_claim, after_claim)?;
                        require(
                            decreased >= qty,
                            "WITHDRAW_BACKING_MISMATCH",
                            "Claim decrease is below cash withdrawal.",
                        )?;
                        let rounding = sub(decreased, qty)?;
                        let accrued = self
                            .accrued_before(
                                b.supply_anchor.as_ref().expect("anchor"),
                                &token,
                                owner,
                                a_token,
                                index,
                                r.block_number - 1,
                            )
                            .await?;
                        let anchor = self
                            .make_anchor(
                                &token,
                                owner,
                                a_token,
                                PositionKind::Supply,
                                r.block_number,
                                U256::ZERO,
                            )
                            .await?;
                        effects.push(FinancialEffect::Accrual {
                            resource: b.resource.clone(),
                            position: supply_position.clone(),
                            asset: b.token.clone(),
                            position_kind: PositionKind::Supply,
                            delta: SignedQuantity::positive(&quantity(accrued)),
                        });
                        effects.push(FinancialEffect::PositionChange {
                            resource: b.resource.clone(),
                            position: supply_position,
                            asset: b.token.clone(),
                            delta: SignedQuantity::negative(&quantity(decreased)),
                            scaled_receipt: Some(quantity(after)),
                            anchor: Some(anchor),
                        });
                        let recipient_resource = resource(self.config.chain_id, recipient);
                        effects.push(FinancialEffect::UsableCredit {
                            to: recipient_resource.clone(),
                            amount: amount(&b.token, qty),
                        });
                        effects.push(origin(
                            supply_location(b),
                            AccountingLocation::Cash {
                                resource: recipient_resource,
                            },
                            &b.token,
                            decreased,
                            qty,
                        ));
                        if rounding > U256::ZERO {
                            effects.push(FinancialEffect::Cost {
                                location: supply_location(b),
                                amount: amount(&b.token, rounding),
                                category: "aave_revision_5_withdraw_rounding".into(),
                                treatment: CostTreatment::IncludedInDebit,
                            });
                        }
                        details["withdraw_backing"] = json!({"scaled_burned":quantity(burned),"claim_decreased":quantity(decreased),"cash_received":quantity(qty),"rounding_loss":quantity(rounding)});
                    }
                    "aave.collateral" => {
                        let enabled = b.enabled.expect("enabled");
                        let matched = if enabled {
                            events::<Pool::ReserveUsedAsCollateralEnabled>(receipt, pool)?
                                .into_iter()
                                .filter(|e| e.reserve == token_addr && e.user == owner)
                                .count()
                        } else {
                            events::<Pool::ReserveUsedAsCollateralDisabled>(receipt, pool)?
                                .into_iter()
                                .filter(|e| e.reserve == token_addr && e.user == owner)
                                .count()
                        };
                        require(
                            matched == 1 && post.collateral_enabled == enabled,
                            "COLLATERAL_PROOF_MISMATCH",
                            "Collateral event or inclusion-block state differs from approved value.",
                        )?;
                        effects.push(FinancialEffect::AuthorityChange {
                            resource: b.resource.clone(),
                            authority: collateral(
                                b.pool.as_deref().expect("pool"),
                                &b.token.reference,
                            ),
                            before: b.collateral_before.expect("before").to_string(),
                            after: enabled.to_string(),
                        });
                    }
                    _ => return Err(FinError::validation("Unsupported receipt proof.")),
                }
                // A supply/withdraw may also change collateral. The preview explicitly binds its prior value.
                if matches!(operation, "aave.supply" | "aave.withdraw") {
                    effects.push(FinancialEffect::AuthorityChange {
                        resource: b.resource.clone(),
                        authority: collateral(b.pool.as_deref().expect("pool"), &b.token.reference),
                        before: b.collateral_before.expect("before").to_string(),
                        after: post.collateral_enabled.to_string(),
                    });
                }
                details["position_after"] = json!(post);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExternalCredit {
    pub status: Status,
    pub amount_atoms: Option<Quantity>,
    pub evidence: Value,
}
