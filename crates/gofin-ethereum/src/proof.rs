use super::*;
use abi::*;
use gofin_evm::alloy::{primitives::B256, sol_types::SolEvent};
use gofin_evm::{CanonicalReceipt, quantity, rpc_bytes, u256};

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
            .map(|v| gofin_evm::hash(v.as_str().unwrap_or("")))
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
fn verify_native_input_weth_settlement(
    receipt: &Value,
    weth: Address,
    router: Address,
    pool: Address,
    amount: U256,
) -> Result<()> {
    let deposit = events::<Weth::Deposit>(receipt, weth)?
        .iter()
        .any(|event| event.dst == router && event.wad == amount);
    let mint = transfers(receipt, weth, Address::ZERO, router)? == amount;
    require(
        (deposit || mint) && transfers(receipt, weth, router, pool)? == amount,
        "SWAP_NATIVE_INPUT_MISMATCH",
        "Native input lacks exact router WETH mint/deposit and pool transfer proof.",
    )
}
fn verify_native_output_weth_settlement(
    receipt: &Value,
    weth: Address,
    pool: Address,
    router: Address,
    amount: U256,
) -> Result<()> {
    let withdrawal = events::<Weth::Withdrawal>(receipt, weth)?
        .iter()
        .any(|event| event.src == router && event.wad == amount);
    let burn = transfers(receipt, weth, router, Address::ZERO)? == amount;
    require(
        transfers(receipt, weth, pool, router)? == amount && (withdrawal || burn),
        "SWAP_NATIVE_OUTPUT_MISMATCH",
        "Native output lacks exact pool WETH transfer and router WETH burn/unwrap proof.",
    )
}
fn exact_one<T>(events: Vec<T>) -> Result<T> {
    require(
        events.len() == 1,
        "RECEIPT_EVENT_MISMATCH",
        "Expected exactly one matching protocol event.",
    )?;
    Ok(events.into_iter().next().expect("one"))
}
fn external_credit_rpc_error(mut error: GoFinError, method: &str) -> GoFinError {
    // Surface the reviewed RPC method in sanitized reconciliation evidence so
    // an operator can distinguish a transient read failure from an unsupported
    // proof layout. Never include the configured endpoint or request payload.
    error.stage = format!("external-credit:{method}");
    error
}

fn qty_if(condition: bool, quantity: U256) -> U256 {
    if condition { quantity } else { U256::ZERO }
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
    #[serde(default, skip_serializing_if = "Quantity::is_zero")]
    pub rounding_gain: Quantity,
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
    supply_backing(
        input,
        scaled_before,
        scaled_after,
        previous_index,
        mint_index,
        income_index,
        false,
    )
}
fn supply_backing(
    input: Quantity,
    scaled_before: Quantity,
    scaled_after: Quantity,
    previous_index: Quantity,
    mint_index: Quantity,
    income_index: Quantity,
    legacy: bool,
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
    let minted = add(
        mul(q, ray)?,
        if legacy {
            index / U256::from(2)
        } else {
            U256::ZERO
        },
    )? / index;
    require(
        minted > U256::ZERO && after == add(before, minted)?,
        "SUPPLY_SCALED_MISMATCH",
        "Scaled mint does not match exact revision-5 floor conversion.",
    )?;
    let round = if legacy {
        ray / U256::from(2)
    } else {
        U256::ZERO
    };
    let before_at_mint = add(mul(before, index)?, round)? / ray;
    let prior_balance = add(mul(before, previous)?, round)? / ray;
    let total_at_mint = add(mul(after, index)?, round)? / ray;
    let supplied_after = add(mul(after, income)?, round)? / ray;
    let backed = sub(total_at_mint, before_at_mint)?;
    let principal = backed.min(q);
    require(
        legacy || backed <= q,
        "SUPPLY_BACKING_MISMATCH",
        "Minted backing exceeds exact input.",
    )?;
    Ok(SupplyBacking {
        rounding_gain: quantity(backed.saturating_sub(q)),
        scaled_minted: quantity(minted),
        backed_principal: quantity(principal),
        rounding_loss: quantity(sub(q, principal)?),
        mint_balance_increase: quantity(sub(before_at_mint, prior_balance)?),
        mint_value: quantity(if legacy {
            add(q, sub(before_at_mint, prior_balance)?)?
        } else {
            sub(total_at_mint, prior_balance)?
        }),
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
        // External and GoFin-originated writes share the reviewed rollup
        // completion policy: canonical sequencer inclusion plus configured
        // confirmations, without an Ethereum L1 finalized-tag wait.
        let sequencer_inclusion =
            EthereumConfig::is_reviewed_sequencer_rollup(self.config.chain_id);
        require(
            confirmations > 0
                && (sequencer_inclusion || u64::from(confirmations) >= self.config.confirmations),
            "FINALITY_LIMIT",
            "External funding proof cannot weaken the configured confirmation floor on this network.",
        )?;
        // Older durable payout requests can retain a higher confirmation
        // count. For reviewed rollups, current protected configuration is the
        // floor and is expected to be one sequencer-included block.
        let required_confirmations = if sequencer_inclusion {
            self.config.confirmations
        } else {
            u64::from(confirmations)
        };
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
        let expected_hash = gofin_evm::hash(tx_hash)?;
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
        self.runtime
            .assert_chain()
            .await
            .map_err(|error| external_credit_rpc_error(error, "eth_chainId"))?;
        let receipt = self
            .runtime
            .rpc
            .request("eth_getTransactionReceipt", json!([tx_hash]))
            .await
            .map_err(|error| external_credit_rpc_error(error, "eth_getTransactionReceipt"))?;
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
            .await
            .map_err(|error| external_credit_rpc_error(error, "eth_getTransactionByHash"))?;
        if transaction.is_null() {
            return Ok(pending());
        }
        let h = |v: &Value| gofin_evm::hash(v.as_str().unwrap_or(""));
        require(
            h(&receipt["transactionHash"])? == expected_hash
                && h(&transaction["hash"])? == expected_hash
                && gofin_evm::rpc_u64(&transaction["chainId"])? == self.config.chain_id,
            "FUNDING_IDENTITY_MISMATCH",
            "Funding transaction does not match its original hash or configured chain.",
        )?;
        let block = gofin_evm::rpc_u64(&receipt["blockNumber"])?;
        let block_hash = h(&receipt["blockHash"])?;
        require(
            h(&transaction["blockHash"])? == block_hash
                && gofin_evm::rpc_u64(&transaction["blockNumber"])? == block,
            "FUNDING_INCLUSION_MISMATCH",
            "Funding transaction and receipt inclusion disagree.",
        )?;
        let header = self
            .runtime
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([gofin_evm::block_tag(Some(block)), false]),
            )
            .await
            .map_err(|error| external_credit_rpc_error(error, "eth_getBlockByNumber"))?;
        if header.is_null()
            || h(&header["hash"])? != block_hash
            || gofin_evm::rpc_u64(&header["number"])? != block
        {
            return Ok(pending());
        }
        let minimum_block = block
            .checked_add(required_confirmations - 1)
            .ok_or_else(|| {
                error(
                    "RPC_DATA_INVALID",
                    "reconciliation",
                    "Required confirmation height overflows.",
                )
            })?;
        let latest = match self
            .runtime
            .rpc
            .request_checked(
                "eth_blockNumber",
                json!([]),
                gofin_evm::RpcReadRequirements {
                    minimum_block: Some(minimum_block),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(value) => gofin_evm::rpc_u64(&value)?,
            Err(error)
                if matches!(
                    error.code.as_str(),
                    "RPC_EVIDENCE_UNAVAILABLE" | "RPC_STALE_READ" | "RPC_UNSUPPORTED_CAPABILITY"
                ) =>
            {
                return Ok(pending());
            }
            Err(error) => return Err(external_credit_rpc_error(error, "eth_blockNumber")),
        };
        let observed_confirmations = if latest >= block {
            latest - block + 1
        } else {
            0
        };
        if observed_confirmations < required_confirmations {
            return Ok(pending());
        }
        if self.config.requires_rpc_finalized() {
            let finalized = self
                .runtime
                .rpc
                .request_checked(
                    "eth_getBlockByNumber",
                    json!(["finalized", false]),
                    gofin_evm::RpcReadRequirements {
                        minimum_block: Some(block),
                        ..Default::default()
                    },
                )
                .await;
            let finalized_height = finalized
                .as_ref()
                .ok()
                .filter(|header| !header.is_null())
                .and_then(|header| {
                    gofin_evm::rpc_u64(&header["number"])
                        .ok()
                        .filter(|_| gofin_evm::hash(header["hash"].as_str().unwrap_or("")).is_ok())
                });
            if finalized_height.is_none_or(|height| height < block) {
                return Ok(ExternalCredit {
                    status: Status::Pending,
                    amount_atoms: None,
                    evidence: json!({"original_hash":tx_hash,"canonical_confirmed":true,"finality":"awaiting-rpc-finalized","finalized_height":finalized_height}),
                });
            }
            // The finalized height alone cannot bind this payout's inclusion.
            // Recheck the original block after reading the finality marker, as
            // for GoFin-originated transactions, before any terminal outcome.
            let canonical = self
                .runtime
                .rpc
                .request(
                    "eth_getBlockByNumber",
                    json!([gofin_evm::block_tag(Some(block)), false]),
                )
                .await
                .map_err(|error| external_credit_rpc_error(error, "eth_getBlockByNumber"))?;
            if canonical["hash"] != json!(format!("{block_hash:#x}"))
                || gofin_evm::rpc_u64(&canonical["number"])? != block
            {
                return Ok(ExternalCredit {
                    status: Status::Pending,
                    amount_atoms: None,
                    evidence: json!({"original_hash":tx_hash,"canonical_confirmed":false,"finality":"inclusion-changed"}),
                });
            }
        }
        let status = gofin_evm::rpc_u64(&receipt["status"])?;
        require(
            status <= 1,
            "FUNDING_STATUS_INVALID",
            "Funding receipt status is not success or revert.",
        )?;
        let evidence = json!({"original_hash":tx_hash,"chain_id":self.config.chain_id,"block":block,"block_hash":format!("{block_hash:#x}"),"confirmations":observed_confirmations,"recipient":lower_address(recipient),"asset":asset,"canonical_confirmed":true,"finality":self.config.finality_label()});
        if status == 0 {
            if sequencer_inclusion
                && !self
                    .external_inclusion_is_canonical(block, block_hash)
                    .await?
            {
                return Ok(ExternalCredit {
                    status: Status::Pending,
                    amount_atoms: None,
                    evidence: json!({"original_hash":tx_hash,"canonical_confirmed":false,"finality":"inclusion-changed"}),
                });
            }
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
                        json!([lower_address(recipient), gofin_evm::block_tag(Some(block))]),
                    )
                    .await
                    .map_err(|error| external_credit_rpc_error(error, "eth_getCode"))?,
            )?;
            require(
                code.is_empty(),
                "NATIVE_FUNDING_UNSUPPORTED",
                "Native funding proof supports direct externally-owned recipients.",
            )?;
            gofin_evm::rpc_u256(&transaction["value"])?
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
            self.balance_of(&token, recipient, Some(block))
                .await
                .map_err(|error| {
                    external_credit_rpc_error(
                        error,
                        if token.reference == "native" {
                            "eth_getBalance"
                        } else {
                            "eth_call"
                        },
                    )
                })?
                >= credited,
            "FUNDING_HOLDING_MISMATCH",
            "Recipient inclusion-block holdings do not cover its proved net credit.",
        )?;
        // Historical code and token-balance reads can take long enough for an
        // L2 inclusion to be reorganized. Recheck the exact block after those
        // reads before allowing the external payout to become terminal.
        if sequencer_inclusion
            && !self
                .external_inclusion_is_canonical(block, block_hash)
                .await?
        {
            return Ok(ExternalCredit {
                status: Status::Pending,
                amount_atoms: None,
                evidence: json!({"original_hash":tx_hash,"canonical_confirmed":false,"finality":"inclusion-changed"}),
            });
        }
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

    async fn external_inclusion_is_canonical(&self, block: u64, block_hash: B256) -> Result<bool> {
        let header = self
            .runtime
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([gofin_evm::block_tag(Some(block)), false]),
            )
            .await
            .map_err(|error| external_credit_rpc_error(error, "eth_getBlockByNumber"))?;
        if header.is_null() {
            return Ok(false);
        }
        Ok(
            gofin_evm::hash(header["hash"].as_str().unwrap_or(""))? == block_hash
                && gofin_evm::rpc_u64(&header["number"])? == block,
        )
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
        let mut b: Binding = serde_json::from_value(preview.binding.clone()).map_err(|_| {
            error(
                "BINDING_INVALID",
                "reconciliation",
                "Original transaction binding is malformed.",
            )
        })?;
        let submitted: FrozenTransaction =
            serde_json::from_value(submission.data["transaction"].clone()).map_err(|_| {
                error(
                    "SUBMISSION_IDENTITY_MISMATCH",
                    "reconciliation",
                    "Durable transaction envelope is malformed.",
                )
            })?;
        if let Some(template) = &b.permit_template {
            gofin_evm::validate_materialized_permit_template(template, &submitted)?;
            b.transaction = submitted;
        }
        require(
            submission.data["transaction"]
                == serde_json::to_value(&b.transaction).expect("serializable")
                && submission.data["metadata"]["operation"] == json!(record.operation)
                && submission.data["metadata"]["resource"] == json!(b.resource),
            "SUBMISSION_IDENTITY_MISMATCH",
            "Durable submission differs from the original financial binding.",
        )?;
        let sequencer_inclusion =
            EthereumConfig::external_credit_uses_sequencer(self.config.chain_id);
        let confirmations = if sequencer_inclusion {
            1
        } else {
            self.config.confirmations
        };
        let receipt = match self
            .runtime
            .receipt(submission, &b.transaction, confirmations)
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
        if self.config.requires_rpc_finalized() {
            let finalized = self
                .runtime
                .rpc
                .request_checked(
                    "eth_getBlockByNumber",
                    json!(["finalized", false]),
                    gofin_evm::RpcReadRequirements {
                        minimum_block: Some(receipt.block_number),
                        ..Default::default()
                    },
                )
                .await;
            let finalized_height = finalized
                .as_ref()
                .ok()
                .filter(|header| !header.is_null())
                .and_then(|header| {
                    gofin_evm::rpc_u64(&header["number"])
                        .ok()
                        .filter(|_| gofin_evm::hash(header["hash"].as_str().unwrap_or("")).is_ok())
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
                .await
                .map_err(|error| external_credit_rpc_error(error, "eth_getBlockByNumber"))?;
            if canonical["hash"] != json!(format!("{:#x}", receipt.block_hash))
                || gofin_evm::rpc_u64(&canonical["number"])? != receipt.block_number
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
        let mut details = json!({"block_number":receipt.block_number,"block_hash":format!("{:#x}",receipt.block_hash),"confirmations":receipt.confirmations,"original_hash":submission.reference,"original_envelope_verified":true,"success":receipt.success,"fee_wei":quantity(receipt.fee),"finality":self.config.finality_label()});
        if receipt.success {
            self.success_effects(&record.operation, &b, &receipt, &mut effects, &mut details)
                .await?;
        }
        if !self.inclusion_still_canonical(&receipt).await? {
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"original_hash":submission.reference,"canonical_confirmed":false,"finality":"inclusion-changed"}),
                verified: None,
            });
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
    // Pinned post-state reads must belong to the same canonical inclusion even
    // if a provider observes a reorg while assembling protocol evidence.
    pub(crate) async fn inclusion_still_canonical(
        &self,
        receipt: &CanonicalReceipt,
    ) -> Result<bool> {
        let block = self
            .runtime
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([gofin_evm::block_tag(Some(receipt.block_number)), false]),
            )
            .await?;
        Ok(!block.is_null()
            && block["hash"] == json!(format!("{:#x}", receipt.block_hash))
            && gofin_evm::rpc_u64(&block["number"])? == receipt.block_number)
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
                let weth = address(&self.resolved_weth(Some(r.block_number)).await?)?;
                let wrapping = operation == "wallet.wrap";
                let classic: Vec<U256> = if wrapping {
                    events::<Weth::Deposit>(receipt, weth)?
                        .into_iter()
                        .filter(|e| e.dst == owner)
                        .map(|e| e.wad)
                        .collect()
                } else {
                    events::<Weth::Withdrawal>(receipt, weth)?
                        .into_iter()
                        .filter(|e| e.src == owner)
                        .map(|e| e.wad)
                        .collect()
                };
                if classic.is_empty() {
                    // Reviewed Arbitrum WETH uses exact ERC-20 mint/burn logs.
                    // Envelope, actual cash legs and inclusion are still proved.
                    require(
                        self.config.chain_id == 42161
                            && lower_address(weth) == "0x82af49447d8a07e3bd95bd0d56f35241523fbab1",
                        "WRAP_PROOF_MISMATCH",
                        "This WETH deployment requires its exact deposit/withdrawal event.",
                    )?;
                    let (from, to) = if wrapping {
                        (Address::ZERO, owner)
                    } else {
                        (owner, Address::ZERO)
                    };
                    let event = exact_one(
                        events::<Erc20::Transfer>(receipt, weth)?
                            .into_iter()
                            .filter(|e| e.from == from && e.to == to)
                            .collect(),
                    )?;
                    require(
                        event.value == qty,
                        "WRAP_PROOF_MISMATCH",
                        "WETH mint/burn event differs from exact input.",
                    )?;
                    self.verify_native_swap_balance(
                        owner,
                        r,
                        if wrapping { qty } else { U256::ZERO },
                        if wrapping { U256::ZERO } else { qty },
                    )
                    .await?;
                } else {
                    require(
                        exact_one(classic)? == qty,
                        "WRAP_PROOF_MISMATCH",
                        "WETH deposit/withdrawal event differs from exact input.",
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
            "uniswap.swap" | "ramses.swap" | "sushi.swap" | "pancakeswap.swap" | "swap.execute" => {
                let recipient = address(b.recipient.as_deref().expect("recipient"))?;
                let router = address(&b.transaction.to)?;
                let route_in = address(b.route_token_in.as_deref().expect("route input"))?;
                let route_out = address(b.route_token_out.as_deref().expect("route output"))?;
                let v4 = b.protocol.as_deref() == Some("v4");
                let ramses = b.dex.as_deref() == Some("ramses") || operation == "ramses.swap";
                let (pool, token0, token1, amount0_positive, amount0, amount1_positive, amount1) =
                    if v4 {
                        let key = b.v4_pool_key.as_ref().ok_or_else(|| {
                            error(
                                "BINDING_INVALID",
                                "reconciliation",
                                "V4 swap lacks its immutable pool key.",
                            )
                        })?;
                        let pool = address(
                            &self
                                .uniswap()?
                                .v4
                                .as_ref()
                                .ok_or_else(|| {
                                    error(
                                        "OPERATION_UNAVAILABLE",
                                        "reconciliation",
                                        "V4 deployment is no longer configured.",
                                    )
                                })?
                                .pool_manager,
                        )?;
                        let id = gofin_evm::hash(&super::uniswap::v4::pool_id(key)?)?;
                        let event = exact_one(
                            events::<V4PoolManager::Swap>(receipt, pool)?
                                .into_iter()
                                .filter(|e| e.id == id && e.sender == router)
                                .collect(),
                        )?;
                        (
                            pool,
                            address(&key.currency0)?,
                            address(&key.currency1)?,
                            event.amount0.is_positive(),
                            U256::from(event.amount0.unsigned_abs()),
                            event.amount1.is_positive(),
                            U256::from(event.amount1.unsigned_abs()),
                        )
                    } else {
                        let pool = address(b.pool.as_deref().expect("pool"))?;
                        if ramses {
                            let config = self.ramses()?;
                            let configured_router = address(&config.router)?;
                            require(
                                router == configured_router,
                                "BINDING_INVALID",
                                "Ramses swap router differs from the reviewed deployment.",
                            )?;
                            let expected_deployer = address(&config.pool_deployer)?;
                            let deployer = self
                                .runtime
                                .call(
                                    address(&config.factory)?,
                                    &RamsesFactory::ramsesV3PoolDeployerCall {},
                                    Some(r.block_number),
                                )
                                .await?;
                            require(
                                deployer == expected_deployer,
                                "DEPLOYMENT_MISMATCH",
                                "Ramses factory pool deployer changed before receipt verification.",
                            )?;
                            let router_deployer = self
                                .runtime
                                .call(router, &RamsesRouter::deployerCall {}, Some(r.block_number))
                                .await?;
                            let quoter_deployer = self
                                .runtime
                                .call(
                                    address(&config.quoter)?,
                                    &RamsesQuoter::deployerCall {},
                                    Some(r.block_number),
                                )
                                .await?;
                            require(
                                router_deployer == expected_deployer
                                    && quoter_deployer == expected_deployer,
                                "DEPLOYMENT_MISMATCH",
                                "Ramses router or quoter deployer differs at receipt inclusion.",
                            )?;
                            let token0 = self
                                .runtime
                                .call(pool, &RamsesPool::token0Call {}, Some(r.block_number))
                                .await?;
                            let token1 = self
                                .runtime
                                .call(pool, &RamsesPool::token1Call {}, Some(r.block_number))
                                .await?;
                            let observed_spacing = self
                                .runtime
                                .call(pool, &RamsesPool::tickSpacingCall {}, Some(r.block_number))
                                .await?;
                            require(
                                b.tick_spacing.is_some_and(|spacing| {
                                    observed_spacing
                                        == gofin_evm::alloy::primitives::aliases::I24::try_from(
                                            i64::from(spacing),
                                        )
                                        .expect("reviewed positive spacing")
                                }),
                                "BINDING_INVALID",
                                "Ramses pool tick spacing differs from the immutable binding.",
                            )?;
                            let factory_pool = self
                                .runtime
                                .call(
                                    address(&config.factory)?,
                                    &RamsesFactory::getPoolCall {
                                        tokenA: route_in,
                                        tokenB: route_out,
                                        tickSpacing: observed_spacing,
                                    },
                                    Some(r.block_number),
                                )
                                .await?;
                            require(
                                factory_pool == pool,
                                "POOL_IDENTITY_MISMATCH",
                                "Ramses factory no longer resolves the exact bound tick-spacing pool.",
                            )?;
                            let event = exact_one(
                                events::<RamsesPool::Swap>(receipt, pool)?
                                    .into_iter()
                                    .filter(|e| {
                                        e.sender == router
                                            && (e.recipient == recipient
                                                || (b
                                                    .output
                                                    .as_ref()
                                                    .is_some_and(|a| a.reference == "native")
                                                    && e.recipient == router))
                                    })
                                    .collect(),
                            )?;
                            (
                                pool,
                                token0,
                                token1,
                                event.amount0.is_positive(),
                                event.amount0.unsigned_abs(),
                                event.amount1.is_positive(),
                                event.amount1.unsigned_abs(),
                            )
                        } else {
                            let dex = if let Some(dex) = b.dex.as_deref() {
                                dex
                            } else {
                                super::uniswap::profile::operation_dex(operation)?
                            };
                            let profile = self.v3_profile(dex)?;
                            if super::uniswap::profile::additional_dex(dex).is_some() {
                                require(
                                    b.dex.as_deref() == Some(profile.dex_id)
                                        && b.factory.as_deref() == Some(profile.factory)
                                        && b.pool_deployer.as_deref() == profile.pool_deployer
                                        && router == address(profile.router)?,
                                    "BINDING_INVALID",
                                    "V3 receipt deployment differs from the immutable binding.",
                                )?;
                                self.validate_v3_metadata(profile, Some(r.block_number))
                                    .await?;
                                let tier = b.fee_tier.ok_or_else(|| {
                                    GoFinError::validation("V3 fee tier is missing.")
                                })?;
                                require(
                                    profile.fee_tiers.contains(&tier),
                                    "BINDING_INVALID",
                                    "Bound fee tier is outside the venue profile.",
                                )?;
                                let factory_pool = self
                                    .runtime
                                    .call(
                                        address(profile.factory)?,
                                        &Factory::getPoolCall {
                                            tokenA: route_in,
                                            tokenB: route_out,
                                            fee: gofin_evm::alloy::primitives::aliases::U24::from(
                                                tier,
                                            ),
                                        },
                                        Some(r.block_number),
                                    )
                                    .await?;
                                require(
                                    factory_pool == pool,
                                    "POOL_IDENTITY_MISMATCH",
                                    "Factory does not resolve the bound pool at receipt inclusion.",
                                )?;
                                let pool_factory = self
                                    .runtime
                                    .call(pool, &UniswapPool::factoryCall {}, Some(r.block_number))
                                    .await?;
                                require(
                                    pool_factory == address(profile.factory)?,
                                    "POOL_IDENTITY_MISMATCH",
                                    "Included pool belongs to a different factory.",
                                )?;
                                let actual_fee = self
                                    .runtime
                                    .call(pool, &UniswapPool::feeCall {}, Some(r.block_number))
                                    .await?;
                                require(
                                    actual_fee.to::<u32>() == tier,
                                    "POOL_IDENTITY_MISMATCH",
                                    "Pool fee differs from the bound factory key.",
                                )?;
                            }
                            let token0 = self
                                .runtime
                                .call(pool, &UniswapPool::token0Call {}, Some(r.block_number))
                                .await?;
                            let token1 = self
                                .runtime
                                .call(pool, &UniswapPool::token1Call {}, Some(r.block_number))
                                .await?;
                            if profile.pool_abi == V3PoolAbi::PancakeV3 {
                                let event =
                                    exact_one(
                                        events::<PancakePool::Swap>(receipt, pool)?
                                            .into_iter()
                                            .filter(|e| {
                                                e.sender == router
                                                    && (e.recipient == recipient
                                                        || (b.output.as_ref().is_some_and(|a| {
                                                            a.reference == "native"
                                                        }) && e.recipient == router))
                                            })
                                            .collect(),
                                    )?;
                                (
                                    pool,
                                    token0,
                                    token1,
                                    event.amount0.is_positive(),
                                    event.amount0.unsigned_abs(),
                                    event.amount1.is_positive(),
                                    event.amount1.unsigned_abs(),
                                )
                            } else {
                                let event =
                                    exact_one(
                                        events::<UniswapPool::Swap>(receipt, pool)?
                                            .into_iter()
                                            .filter(|e| {
                                                e.sender == router
                                                    && (e.recipient == recipient
                                                        || (b.output.as_ref().is_some_and(|a| {
                                                            a.reference == "native"
                                                        }) && e.recipient == router))
                                            })
                                            .collect(),
                                    )?;
                                (
                                    pool,
                                    token0,
                                    token1,
                                    event.amount0.is_positive(),
                                    event.amount0.unsigned_abs(),
                                    event.amount1.is_positive(),
                                    event.amount1.unsigned_abs(),
                                )
                            }
                        }
                    };
                require(
                    (route_in == token0 && route_out == token1)
                        || (route_in == token1 && route_out == token0),
                    "SWAP_POOL_ASSETS_MISMATCH",
                    "Receipt pool currencies differ from the reviewed swap route.",
                )?;
                let (in_positive, input_atoms, out_positive, output_atoms) = if route_in == token0 {
                    (amount0_positive, amount0, amount1_positive, amount1)
                } else {
                    (amount1_positive, amount1, amount0_positive, amount0)
                };
                // V3 Swap events report pool balance movement (input positive,
                // output negative). V4 PoolManager reports the caller's signed
                // BalanceDelta, so an exact-input swap has input negative and
                // output positive.
                let input_direction_matches = if v4 {
                    !in_positive && out_positive
                } else {
                    in_positive && !out_positive
                };
                require(
                    input_direction_matches && input_atoms == qty,
                    "SWAP_POOL_LEG_MISMATCH",
                    "Pool receipt event does not prove the exact input and opposite output direction.",
                )?;
                let output = b.output.as_ref().expect("output");
                let native_input = b.token.reference == "native";
                let native_output = output.reference == "native";
                if native_input {
                    require(
                        u256(&b.transaction.value)? == qty,
                        "SWAP_NATIVE_INPUT_MISMATCH",
                        "Frozen transaction value differs from exact native swap input.",
                    )?;
                    if !v4 {
                        let weth = address(b.route_token_in.as_deref().expect("wrapped input"))?;
                        verify_native_input_weth_settlement(receipt, weth, router, pool, qty)?;
                    }
                } else {
                    require(
                        u256(&b.transaction.value)?.is_zero()
                            && transfers(receipt, token_addr, owner, pool)? == qty,
                        "SWAP_INPUT_CASH_MISMATCH",
                        "Receipt lacks exact ERC-20 input cash transfer to the pool.",
                    )?;
                }
                let credited = if native_output {
                    if !v4 {
                        let weth = address(b.route_token_out.as_deref().expect("wrapped output"))?;
                        verify_native_output_weth_settlement(
                            receipt,
                            weth,
                            pool,
                            router,
                            output_atoms,
                        )?;
                    }
                    output_atoms
                } else {
                    let amount_out =
                        transfers(receipt, address(&output.reference)?, pool, recipient)?;
                    require(
                        amount_out == output_atoms,
                        "SWAP_OUTPUT_CASH_MISMATCH",
                        "Output token cash transfer differs from its pool event amount.",
                    )?;
                    amount_out
                };
                require(
                    credited >= u256(b.minimum_output.as_ref().expect("minimum"))?,
                    "SWAP_PROOF_MISMATCH",
                    "Pool cash legs are below the approved minimum output.",
                )?;
                if native_input || native_output {
                    self.verify_native_swap_balance(
                        owner,
                        r,
                        qty_if(native_input, qty),
                        qty_if(native_output, credited),
                    )
                    .await?;
                }
                if let Some(template) = &b.permit_template {
                    match template.permit() {
                        gofin_evm::AuthorizedPermit::Eip2612(permit) => {
                            let permit_token = address(&permit.token)?;
                            let permit_owner = address(&permit.owner)?;
                            let spender = address(&permit.spender)?;
                            let permit_approvals =
                                events::<Erc20::Approval>(receipt, permit_token)?
                                    .into_iter()
                                    .filter(|event| {
                                        event.owner == permit_owner
                                            && event.spender == spender
                                            && event.value == qty
                                    })
                                    .count();
                            let post = self
                                .runtime
                                .call(
                                    permit_token,
                                    &Erc20::allowanceCall {
                                        owner: permit_owner,
                                        spender,
                                    },
                                    Some(r.block_number),
                                )
                                .await?;
                            let nonce_after = self
                                .runtime
                                .call(
                                    permit_token,
                                    &Erc20::noncesCall {
                                        owner: permit_owner,
                                    },
                                    Some(r.block_number),
                                )
                                .await?;
                            let nonce_before = u256(&permit.nonce)?;
                            require(
                                nonce_before < U256::MAX
                                    && permit_approvals > 0
                                    && post == U256::ZERO
                                    && nonce_after == nonce_before + U256::from(1),
                                "PERMIT_AUTHORITY_PROOF_MISMATCH",
                                "EIP-2612 event, nonce and inclusion-block allowance differ from the typed finite permit.",
                            )?;
                            effects.push(FinancialEffect::AuthorityChange {
                                resource: b.resource.clone(),
                                authority: authority(&permit.token, &permit.spender),
                                before: b
                                    .allowance_before
                                    .clone()
                                    .unwrap_or_else(Quantity::zero)
                                    .to_string(),
                                after: Quantity::zero().to_string(),
                            });
                        }
                        gofin_evm::AuthorizedPermit::Permit2Single(permit) => {
                            let permit2 = address(&permit.verifying_contract)?;
                            let permit_owner = address(&permit.owner)?;
                            let permit_token = address(&permit.token)?;
                            let spender = address(&permit.spender)?;
                            let nonce =
                                gofin_evm::alloy::primitives::aliases::U48::from(permit.nonce);
                            let expiration =
                                gofin_evm::alloy::primitives::aliases::U48::from(permit.expiration);
                            let amount_u256 = u256(&permit.amount)?;
                            let amount_max =
                                U256::from(gofin_evm::alloy::primitives::aliases::U160::MAX);
                            require(
                                amount_u256 <= amount_max,
                                "PERMIT_AUTHORITY_PROOF_MISMATCH",
                                "Permit2 amount exceeds uint160.",
                            )?;
                            let limbs = amount_u256.into_limbs();
                            let amount = gofin_evm::alloy::primitives::aliases::U160::from_limbs([
                                limbs[0], limbs[1], limbs[2],
                            ]);
                            exact_one(
                                events::<Permit2::Permit>(receipt, permit2)?
                                    .into_iter()
                                    .filter(|event| {
                                        event.owner == permit_owner
                                            && event.token == permit_token
                                            && event.spender == spender
                                            && event.amount == amount
                                            && event.expiration == expiration
                                            && event.nonce == nonce
                                    })
                                    .collect(),
                            )?;
                            let post = self
                                .runtime
                                .call(
                                    permit2,
                                    &Permit2::allowanceCall {
                                        owner: permit_owner,
                                        token: permit_token,
                                        spender,
                                    },
                                    Some(r.block_number),
                                )
                                .await?;
                            let before = b.permit2_allowance_before.as_ref().ok_or_else(|| {
                                error(
                                    "BINDING_INVALID",
                                    "reconciliation",
                                    "Permit2 swap lacks its original allowance state.",
                                )
                            })?;
                            require(
                                U256::from(post.amount).is_zero()
                                    && post.expiration == expiration
                                    && post.nonce.to::<u64>() == permit.nonce + 1,
                                "PERMIT_AUTHORITY_PROOF_MISMATCH",
                                "Permit2 event and inclusion-block allowance differ from the typed finite permit.",
                            )?;
                            effects.push(FinancialEffect::AuthorityChange {
                                resource: b.resource.clone(),
                                authority: super::permit2::approval_authority(
                                    &b.token,
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
                effects.push(FinancialEffect::Conversion {
                    resource: b.resource.clone(),
                    spent: amount(&b.token, qty),
                    received: amount(output, credited),
                });
                details["actual_output"] = json!(quantity(credited));
                if ramses {
                    details["ramses_pool_identity"] = json!({
                        "factory": b.factory,
                        "pool_deployer": b.pool_deployer,
                        "pool": b.pool,
                        "tick_spacing": b.tick_spacing,
                        "quoted_pool_fee_ppm": b.swap_fee_ppm,
                        "quote_fee_ceiling_ppm": b.max_pool_fee_ppm,
                        "fee_evidence": "pinned quote observation only; actual conversion is proven by receipt pool/cash legs and the approved minimum output"
                    });
                }
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
                                == self.supply_revision()
                                && post_index == mint.index
                                && quantity(before) == *b.scaled_before.as_ref().expect("scaled"),
                            "SUPPLY_PROOF_MISMATCH",
                            "aToken revision, original scaled balance or minted index changed.",
                        )?;
                        let backing = if self.horizon_math() {
                            supply_backing(
                                b.units.clone(),
                                quantity(before),
                                quantity(after),
                                quantity(previous),
                                quantity(mint.index),
                                quantity(income),
                                true,
                            )
                        } else {
                            revision_five_backing(
                                b.units.clone(),
                                quantity(before),
                                quantity(after),
                                quantity(previous),
                                quantity(mint.index),
                                quantity(income),
                            )
                        }?;
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
                        let total_yield = add(
                            add(accrued, u256(&backing.yield_after_mint)?)?,
                            u256(&backing.rounding_gain)?,
                        )?;
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
                        let scaled_minted = self.scaled_change(
                            qty,
                            minted.index,
                            &PositionKind::VariableDebt,
                            true,
                        )?;
                        require(
                            minted.index >= ray
                                && index >= minted.index
                                && scaled_minted > U256::ZERO
                                && after == add(before, scaled_minted)?,
                            "DEBT_SCALED_MISMATCH",
                            "Borrow scaled debt mint differs from exact ceiling conversion.",
                        )?;
                        let pre_claim = self.position_backing(
                            before,
                            minted.index,
                            &PositionKind::VariableDebt,
                        )?;
                        let post_claim_at_mint = self.position_backing(
                            after,
                            minted.index,
                            &PositionKind::VariableDebt,
                        )?;
                        let added = sub(post_claim_at_mint, pre_claim)?;
                        require(
                            self.horizon_math() || added >= qty,
                            "DEBT_BACKING_MISMATCH",
                            "Debt backing is below borrowed cash.",
                        )?;
                        let rounding = added.saturating_sub(qty);
                        let rounding_gain = qty.saturating_sub(added);
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
                            self.position_backing(before, previous, &PositionKind::VariableDebt)?;
                        require(
                            minted.value
                                == if self.horizon_math() {
                                    add(qty, sub(pre_claim, prior_claim)?)?
                                } else {
                                    sub(post_claim_at_mint, prior_claim)?
                                }
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
                            self.position_backing(after, index, &PositionKind::VariableDebt)?,
                            post_claim_at_mint,
                        )?;
                        let total_accrual = add(add(pre_accrual, rounding)?, extra_after)?;
                        let deferred: Quantity = serde_json::from_value(
                            prior.evidence["deferred_unpaid_rounding"].clone(),
                        )
                        .map_err(|_| GoFinError::validation("Invalid proved rounding."))?;
                        let deferred_interest: Quantity = serde_json::from_value(
                            prior.evidence["deferred_unpaid_interest"].clone(),
                        )
                        .map_err(|_| GoFinError::validation("Invalid proved unpaid interest."))?;
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
                        // A proved zero pre-position has no historical claim
                        // accruing interest. Its rounding and post-mint index
                        // increment create new debt within the approved finite
                        // exposure, rather than inventing an unanchored accrual.
                        // Existing debt retains its prior principal/accrual split.
                        let opening = prior.principal.atoms.is_zero() && before.is_zero();
                        let (principal_change, accrued_change) = if opening && !self.horizon_math()
                        {
                            require(
                                total_accrual
                                    <= u256(
                                        b.debt_rounding_maximum
                                            .as_ref()
                                            .expect("borrow rounding bound"),
                                    )?,
                                "DEBT_ROUNDING_LIMIT",
                                "Opening debt's complete proved increment exceeds approved finite rounding exposure.",
                            )?;
                            (add(qty, total_accrual)?, U256::ZERO)
                        } else {
                            (qty, total_accrual)
                        };
                        effects.push(FinancialEffect::Accrual {
                            resource: b.resource.clone(),
                            position: debt_position.clone(),
                            asset: b.token.clone(),
                            position_kind: PositionKind::VariableDebt,
                            delta: aave::math::signed_difference(accrued_change, rounding_gain),
                        });
                        effects.push(FinancialEffect::DebtChange {
                            resource: b.resource.clone(),
                            debt: debt_position,
                            asset: b.token.clone(),
                            delta: SignedQuantity::positive(&quantity(principal_change)),
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
                        let before_claim =
                            self.position_backing(before, index, &PositionKind::VariableDebt)?;
                        let after_claim =
                            self.position_backing(after, index, &PositionKind::VariableDebt)?;
                        let burned = if actual == before_claim {
                            before
                        } else {
                            self.scaled_change(actual, index, &PositionKind::VariableDebt, false)?
                        };
                        require(
                            after == sub(before, burned)?
                                && quantity(after_claim) == post.variable_debt,
                            "DEBT_BURN_PROOF_MISMATCH",
                            "Repayment scaled burn or post debt differs from exact floor rule.",
                        )?;
                        let cleared = sub(before_claim, after_claim)?;
                        require(
                            cleared > U256::ZERO && (self.horizon_math() || cleared <= actual),
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
                        .map_err(|_| GoFinError::validation("Invalid deferred rounding."))?;
                        let deferred_interest: Quantity = serde_json::from_value(
                            prior.evidence["deferred_unpaid_interest"].clone(),
                        )
                        .map_err(|_| GoFinError::validation("Invalid proved unpaid interest."))?;
                        let (total_cost, remaining_interest, remaining_deferred) =
                            settle_debt_costs(
                                interest,
                                u256(&deferred_interest)?,
                                u256(&deferred)?,
                                actual,
                                cleared.min(actual),
                            )?;
                        let rounding_gain = cleared.saturating_sub(actual);
                        let principal_leg = cleared.min(actual);
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
                            delta: aave::math::signed_difference(interest, rounding_gain),
                        });
                        effects.push(FinancialEffect::CashDebit {
                            resource: b.resource.clone(),
                            amount: amount(&b.token, actual),
                        });
                        effects.push(FinancialEffect::DebtChange {
                            resource: b.resource.clone(),
                            debt: debt_position,
                            asset: b.token.clone(),
                            delta: SignedQuantity::negative(&quantity(principal_leg)),
                            anchor: Some(anchor),
                        });
                        effects.push(origin(
                            cash.clone(),
                            debt_location(b),
                            &b.token,
                            actual,
                            principal_leg,
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
                        let burned =
                            self.scaled_change(qty, index, &PositionKind::Supply, false)?;
                        require(
                            after == sub(before, burned)?,
                            "WITHDRAW_SCALED_MISMATCH",
                            "Withdraw scaled burn does not match exact ceiling rule.",
                        )?;
                        let before_claim =
                            self.position_backing(before, index, &PositionKind::Supply)?;
                        let after_claim =
                            self.position_backing(after, index, &PositionKind::Supply)?;
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
                            self.horizon_math() || decreased >= qty,
                            "WITHDRAW_BACKING_MISMATCH",
                            "Claim decrease is below cash withdrawal.",
                        )?;
                        let rounding = decreased.saturating_sub(qty);
                        let rounding_gain = qty.saturating_sub(decreased);
                        let principal_leg = decreased.max(qty);
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
                            delta: SignedQuantity::positive(&quantity(add(
                                accrued,
                                rounding_gain,
                            )?)),
                        });
                        effects.push(FinancialEffect::PositionChange {
                            resource: b.resource.clone(),
                            position: supply_position,
                            asset: b.token.clone(),
                            delta: SignedQuantity::negative(&quantity(principal_leg)),
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
                            principal_leg,
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
                    _ => return Err(GoFinError::validation("Unsupported receipt proof.")),
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

    async fn verify_native_swap_balance(
        &self,
        owner: Address,
        receipt: &CanonicalReceipt,
        native_input: U256,
        native_output: U256,
    ) -> Result<()> {
        require(
            receipt.block_number > 0,
            "NATIVE_SWAP_POSTSTATE_UNAVAILABLE",
            "Native swap proof needs a parent inclusion block.",
        )?;
        let block = self
            .runtime
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([format!("{:#x}", receipt.block_number), true]),
            )
            .await?;
        let transactions = block["transactions"].as_array().ok_or_else(|| {
            error(
                "NATIVE_SWAP_POSTSTATE_UNAVAILABLE",
                "reconciliation",
                "Inclusion block lacks transaction details for native balance attribution.",
            )
        })?;
        let sender_txs = transactions
            .iter()
            .filter(|tx| address(tx["from"].as_str().unwrap_or("")).is_ok_and(|from| from == owner))
            .collect::<Vec<_>>();
        require(
            sender_txs.len() == 1 && sender_txs[0]["hash"] == json!(format!("{:#x}", receipt.hash)),
            "NATIVE_SWAP_POSTSTATE_AMBIGUOUS",
            "Wallet has another transaction in the inclusion block, so native balance change cannot be attributed safely.",
        )?;
        let before = self
            .runtime
            .balance(owner, Some(receipt.block_number - 1))
            .await?;
        let after = self
            .runtime
            .balance(owner, Some(receipt.block_number))
            .await?;
        require(
            add(before, native_output)? == add(add(after, receipt.fee)?, native_input)?,
            "NATIVE_SWAP_POSTSTATE_MISMATCH",
            "Inclusion-block native balance change does not match the verified swap leg, exact native input and receipt gas.",
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExternalCredit {
    pub status: Status,
    pub amount_atoms: Option<Quantity>,
    pub evidence: Value,
}

#[cfg(test)]
mod native_weth_settlement_tests {
    use super::*;
    use serde_json::json;

    fn log<E: SolEvent>(emitter: Address, event: E) -> Value {
        let encoded = event.encode_log_data();
        json!({
            "address": format!("{emitter:#x}"),
            "topics": encoded.topics().iter().map(|topic| format!("{topic:#x}")).collect::<Vec<_>>(),
            "data": format!("0x{}", hex::encode(&encoded.data)),
            "removed": false,
            "transactionHash": format!("0x{}", "11".repeat(32)),
            "blockHash": format!("0x{}", "22".repeat(32)),
            "blockNumber": "0x1",
        })
    }

    fn receipt(logs: Vec<Value>) -> Value {
        json!({
            "transactionHash": format!("0x{}", "11".repeat(32)),
            "blockHash": format!("0x{}", "22".repeat(32)),
            "blockNumber": "0x1",
            "logs": logs,
        })
    }

    fn a(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    #[test]
    fn native_input_accepts_deposit_or_exact_router_mint() {
        let (weth, router, pool) = (a(10), a(11), a(12));
        let amount = U256::from(100);
        let pool_transfer = log(
            weth,
            Erc20::Transfer {
                from: router,
                to: pool,
                value: amount,
            },
        );
        let deposit = log(
            weth,
            Weth::Deposit {
                dst: router,
                wad: amount,
            },
        );
        assert!(
            verify_native_input_weth_settlement(
                &receipt(vec![deposit, pool_transfer.clone()]),
                weth,
                router,
                pool,
                amount,
            )
            .is_ok()
        );

        let mint = log(
            weth,
            Erc20::Transfer {
                from: Address::ZERO,
                to: router,
                value: amount,
            },
        );
        assert!(
            verify_native_input_weth_settlement(
                &receipt(vec![mint.clone(), pool_transfer.clone()]),
                weth,
                router,
                pool,
                amount,
            )
            .is_ok()
        );

        let wrong_mint = log(
            weth,
            Erc20::Transfer {
                from: Address::ZERO,
                to: router,
                value: amount + U256::from(1),
            },
        );
        assert!(
            verify_native_input_weth_settlement(
                &receipt(vec![wrong_mint, pool_transfer.clone()]),
                weth,
                router,
                pool,
                amount,
            )
            .is_err()
        );
        let wrong_pool_amount = log(
            weth,
            Erc20::Transfer {
                from: router,
                to: pool,
                value: amount - U256::from(1),
            },
        );
        assert!(
            verify_native_input_weth_settlement(
                &receipt(vec![mint, wrong_pool_amount]),
                weth,
                router,
                pool,
                amount,
            )
            .is_err()
        );
    }

    #[test]
    fn native_output_accepts_withdrawal_or_exact_router_burn() {
        let (weth, pool, router) = (a(20), a(21), a(22));
        let amount = U256::from(200);
        let pool_transfer = log(
            weth,
            Erc20::Transfer {
                from: pool,
                to: router,
                value: amount,
            },
        );
        let withdrawal = log(
            weth,
            Weth::Withdrawal {
                src: router,
                wad: amount,
            },
        );
        assert!(
            verify_native_output_weth_settlement(
                &receipt(vec![pool_transfer.clone(), withdrawal]),
                weth,
                pool,
                router,
                amount,
            )
            .is_ok()
        );
        let burn = log(
            weth,
            Erc20::Transfer {
                from: router,
                to: Address::ZERO,
                value: amount,
            },
        );
        assert!(
            verify_native_output_weth_settlement(
                &receipt(vec![pool_transfer.clone(), burn.clone()]),
                weth,
                pool,
                router,
                amount,
            )
            .is_ok()
        );

        let wrong_burn = log(
            weth,
            Erc20::Transfer {
                from: router,
                to: Address::ZERO,
                value: amount - U256::from(1),
            },
        );
        assert!(
            verify_native_output_weth_settlement(
                &receipt(vec![pool_transfer.clone(), wrong_burn]),
                weth,
                pool,
                router,
                amount,
            )
            .is_err()
        );
        let wrong_pool_amount = log(
            weth,
            Erc20::Transfer {
                from: pool,
                to: router,
                value: amount - U256::from(1),
            },
        );
        assert!(
            verify_native_output_weth_settlement(
                &receipt(vec![wrong_pool_amount, burn]),
                weth,
                pool,
                router,
                amount,
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod legacy_math_tests {
    use super::*;
    #[test]
    fn half_up_mint_exposes_exact_gain_and_loss_without_tolerance() {
        let ray = pow10(27).unwrap();
        for (input, index, backed, gain, loss) in [
            (1, ray * U256::from(3) / U256::from(2), 1, 1, 0),
            (2, ray * U256::from(3) / U256::from(2), 2, 0, 0),
            (3, ray * U256::from(11) / U256::from(10), 3, 0, 0),
        ] {
            let scaled = (U256::from(input) * ray + index / U256::from(2)) / index;
            let output = supply_backing(
                quantity(U256::from(input)),
                Quantity::zero(),
                quantity(scaled),
                Quantity::zero(),
                quantity(index),
                quantity(index),
                true,
            )
            .unwrap();
            assert_eq!(output.backed_principal, Quantity::from_u64(backed));
            assert_eq!(output.rounding_gain, Quantity::from_u64(gain));
            assert_eq!(output.rounding_loss, Quantity::from_u64(loss));
            assert_eq!(output.mint_value, Quantity::from_u64(input));
        }
        assert!(
            supply_backing(
                Quantity::from_u64(1),
                Quantity::zero(),
                Quantity::zero(),
                Quantity::zero(),
                quantity(ray * U256::from(3)),
                quantity(ray * U256::from(3)),
                true
            )
            .is_err()
        );
    }
}
