//! Canonical settlement of opaque API swaps and externally filled orders.
//! No provider status creates spendable credit without independent chain proof.
use super::*;
use gofin_evm::alloy::{
    primitives::{B256, keccak256},
    sol_types::SolEvent,
};
use gofin_evm::{CanonicalReceipt, hash, rpc_bytes};

fn pending(submission: &Submission, details: Value) -> Reconciliation {
    Reconciliation {
        status: Status::Pending,
        evidence: json!({"original_submission":submission.reference,"details":details}),
        verified: None,
    }
}
fn checked_logs(receipt: &CanonicalReceipt) -> Result<&Vec<Value>> {
    let logs = receipt.receipt["logs"]
        .as_array()
        .ok_or_else(|| GoFinError::validation("Canonical receipt lacks logs."))?;
    for log in logs {
        require(
            log["removed"].as_bool() != Some(true)
                && log["transactionHash"] == json!(format!("{:#x}", receipt.hash))
                && log["blockHash"] == json!(format!("{:#x}", receipt.block_hash))
                && rpc_u64(&log["blockNumber"])? == receipt.block_number,
            "RECEIPT_LOG_IDENTITY",
            "Receipt log inclusion differs from its transaction.",
        )?;
    }
    Ok(logs)
}
fn transfer_totals(
    receipt: &CanonicalReceipt,
    token: &AssetId,
    owner: Address,
) -> Result<(U256, U256)> {
    let mut incoming = U256::ZERO;
    let mut outgoing = U256::ZERO;
    for log in checked_logs(receipt)? {
        if address(log["address"].as_str().unwrap_or(""))? != address(&token.reference)? {
            continue;
        }
        let topics = log["topics"]
            .as_array()
            .ok_or_else(|| GoFinError::validation("Invalid transfer topics."))?
            .iter()
            .map(|v| hash(v.as_str().unwrap_or("")))
            .collect::<Result<Vec<B256>>>()?;
        if topics.first() != Some(&abi::Erc20::Transfer::SIGNATURE_HASH) {
            continue;
        }
        let event = abi::Erc20::Transfer::decode_raw_log_validate(
            topics.iter().copied(),
            &rpc_bytes(&log["data"])?,
        )
        .map_err(|_| GoFinError::validation("Invalid token transfer event."))?;
        if event.from == owner {
            outgoing = add(outgoing, event.value)?;
        }
        if event.to == owner {
            incoming = add(incoming, event.value)?;
        }
    }
    Ok((incoming, outgoing))
}
fn fill_topic() -> String {
    format!("{:#x}", keccak256("Fill(bytes32,address,address,uint256)"))
}
fn verify_fill(receipt: &CanonicalReceipt, b: &UniswapBinding, reference: &str) -> Result<()> {
    let info = &b.quote.raw["quote"]["orderInfo"];
    let reactor = address(info["reactor"].as_str().unwrap_or(""))?;
    let order = hash(reference)?;
    let owner = address(&b.owner)?;
    let nonce = provider_uint(&info["nonce"])?;
    let mut count = 0;
    for log in checked_logs(receipt)? {
        if address(log["address"].as_str().unwrap_or(""))? != reactor {
            continue;
        }
        let topics = log["topics"]
            .as_array()
            .ok_or_else(|| GoFinError::validation("Invalid order fill topics."))?;
        if topics.first() != Some(&json!(fill_topic()))
            || topics.get(1) != Some(&json!(format!("{order:#x}")))
        {
            continue;
        }
        require(
            topics.len() == 4
                && hash(topics[3].as_str().unwrap_or(""))? == B256::from(owner.into_word()),
            "UNISWAPX_FILL_IDENTITY",
            "Order fill names another swapper.",
        )?;
        let data = rpc_bytes(&log["data"])?;
        require(
            data.len() == 32 && U256::from_be_slice(&data) == nonce,
            "UNISWAPX_FILL_IDENTITY",
            "Order fill nonce differs from the original order.",
        )?;
        count += 1;
    }
    require(
        count == 1,
        "UNISWAPX_FILL_IDENTITY",
        "Canonical receipt must contain exactly one fill of the original order.",
    )
}
fn trace_transfers(frame: &Value, owner: Address, depth: u32) -> Result<(U256, U256)> {
    require(
        depth <= 128,
        "UNISWAP_TRACE_INVALID",
        "Native transfer trace exceeds bounded depth.",
    )?;
    if frame.get("error").is_some_and(|v| !v.is_null()) {
        return Ok((U256::ZERO, U256::ZERO));
    }
    let mut incoming = U256::ZERO;
    let mut outgoing = U256::ZERO;
    if matches!(
        frame["type"].as_str(),
        Some("CALL" | "CREATE" | "CREATE2" | "SELFDESTRUCT")
    ) {
        let value = frame
            .get("value")
            .map(rpc_u256)
            .transpose()?
            .unwrap_or(U256::ZERO);
        if !value.is_zero() {
            if address(frame["from"].as_str().unwrap_or(""))? == owner {
                outgoing = value;
            }
            if address(frame["to"].as_str().unwrap_or(""))? == owner {
                incoming = value;
            }
        }
    }
    if let Some(calls) = frame.get("calls") {
        for child in calls
            .as_array()
            .ok_or_else(|| GoFinError::validation("Invalid native call trace."))?
        {
            let (i, o) = trace_transfers(child, owner, depth + 1)?;
            incoming = add(incoming, i)?;
            outgoing = add(outgoing, o)?;
        }
    }
    Ok((incoming, outgoing))
}
impl EthereumAdapter {
    pub(crate) async fn uniswap_api_reconcile(
        &self,
        record: &OperationRecord,
    ) -> Result<Reconciliation> {
        let p = record
            .preview
            .as_ref()
            .ok_or_else(|| GoFinError::validation("Original API preview missing."))?;
        let b = parse_binding(p)?;
        let s = record
            .submission
            .as_ref()
            .ok_or_else(|| GoFinError::validation("Original API submission missing."))?;
        require(
            p.operation == record.operation
                && p.input == record.input
                && b.chain_id == self.config.chain_id,
            "BINDING_IDENTITY_MISMATCH",
            "Recovery record differs from the original API preview.",
        )?;
        self.runtime.assert_chain().await?;
        let is_x = b.quote.routing != "CLASSIC";
        let (receipt, fee) = if is_x {
            require(
                s.kind == "uniswapx"
                    && Some(s.reference.as_str()) == b.quote.order_hash.as_deref()
                    && s.data["binding_hash"] == binding_hash(&p.binding)?,
                "SUBMISSION_IDENTITY_MISMATCH",
                "Durable order identity differs from its approved quote.",
            )?;
            // Provider tracking is a candidate source only. In particular an
            // HTTP timeout, 404 or 'cancelled' status cannot release a hold.
            let status = self.uniswap_api.orders(&s.reference).await.ok();
            let candidate = status
                .as_ref()
                .and_then(|v| v["orders"].as_array())
                .and_then(|rows| {
                    rows.iter().find(|row| {
                        row["orderId"]
                            .as_str()
                            .is_some_and(|id| id.eq_ignore_ascii_case(&s.reference))
                    })
                })
                .and_then(|row| row["txHash"].as_str())
                .filter(|hash| !hash.is_empty())
                .map(str::to_owned);
            let tx_hash = if let Some(candidate) = candidate {
                Some(candidate)
            } else {
                let head = self
                    .runtime
                    .rpc
                    .request("eth_getBlockByNumber", json!(["finalized", false]))
                    .await?;
                if head.is_null() {
                    return Ok(pending(
                        s,
                        json!({"reason":"awaiting-finalized-order-evidence"}),
                    ));
                }
                let height = rpc_u64(&head["number"])?;
                let head_hash = hash(head["hash"].as_str().unwrap_or(""))?;
                let head_time = rpc_u64(&head["timestamp"])?;
                if height < b.prepared_block {
                    return Ok(pending(
                        s,
                        json!({"reason":"awaiting-preparation-block-finality"}),
                    ));
                }
                let search_from = b.prepared_block.saturating_sub(1);
                let search_to = if head_time > b.deadline {
                    // An order cannot fill after its deadline. Locate the first
                    // finalized block strictly beyond it, so recovery months
                    // later still scans only the order's execution window.
                    let mut lower = search_from;
                    let mut upper = height;
                    while lower < upper {
                        let middle = lower + (upper - lower) / 2;
                        let header = self
                            .runtime
                            .rpc
                            .request(
                                "eth_getBlockByNumber",
                                json!([format!("{middle:#x}"), false]),
                            )
                            .await?;
                        hash(header["hash"].as_str().unwrap_or(""))?;
                        let timestamp = rpc_u64(&header["timestamp"])?;
                        require(
                            rpc_u64(&header["number"])? == middle && timestamp <= head_time,
                            "UNISWAPX_EXPIRY_HEADER_INVALID",
                            "Historical expiry search header differs from its finalized range.",
                        )?;
                        if timestamp > b.deadline {
                            upper = middle;
                        } else {
                            lower = middle + 1;
                        }
                    }
                    lower
                } else {
                    height
                };
                let mut found = None;
                let mut from = search_from;
                loop {
                    // Keep every request within common hosted-RPC range limits.
                    let to = from.saturating_add(999).min(search_to);
                    let logs = self
                        .runtime
                        .rpc
                        .request(
                            "eth_getLogs",
                            json!([{
                                "address":b.quote.raw["quote"]["orderInfo"]["reactor"],
                                "fromBlock":format!("{from:#x}"), "toBlock":format!("{to:#x}"),
                                "topics":[fill_topic(),s.reference]
                            }]),
                        )
                        .await?;
                    let logs = logs.as_array().ok_or_else(|| {
                        GoFinError::validation("Invalid order fill search result.")
                    })?;
                    if logs.len() > 1 {
                        return Err(GoFinError::validation(
                            "Conflicting UniswapX fill identities.",
                        ));
                    }
                    if let Some(log) = logs.first() {
                        found = Some(
                            log["transactionHash"]
                                .as_str()
                                .ok_or_else(|| {
                                    GoFinError::validation("Order fill lacks transaction identity.")
                                })?
                                .to_owned(),
                        );
                        // A positive result is independently checked below;
                        // only no-fill expiry requires complete range coverage.
                        break;
                    }
                    if to == search_to {
                        break;
                    }
                    from = to + 1;
                }
                if let Some(candidate) = found {
                    Some(candidate)
                } else if head_time > b.deadline {
                    // Recheck the exact finalized header after the complete log
                    // observation. Expiry alone does not prove no earlier fill.
                    let canonical = self
                        .runtime
                        .rpc
                        .request(
                            "eth_getBlockByNumber",
                            json!([format!("{height:#x}"), false]),
                        )
                        .await?;
                    if hash(canonical["hash"].as_str().unwrap_or(""))? != head_hash
                        || rpc_u64(&canonical["number"])? != height
                        || rpc_u64(&canonical["timestamp"])? != head_time
                    {
                        return Ok(pending(s, json!({"reason":"finalized-view-changed"})));
                    }
                    return verified(
                        s,
                        Status::Expired,
                        vec![],
                        json!({"provider":PROVIDER,"order_id":s.reference,"expired_unfilled":true,"finalized_block":height,"finalized_hash":head["hash"],"searched_from":search_from,"searched_to":search_to}),
                    );
                } else {
                    None
                }
            };
            let Some(tx_hash) = tx_hash else {
                return Ok(pending(s, json!({"reason":"order-open"})));
            };
            let receipt = self
                .runtime
                .observe_receipt(
                    &tx_hash,
                    self.config.confirmations,
                    self.config.requires_rpc_finalized(),
                )
                .await?;
            let Some(receipt) = receipt else {
                return Ok(pending(
                    s,
                    json!({"reason":"fill-awaiting-finality","transaction_hash":tx_hash}),
                ));
            };
            require(
                receipt.success,
                "UNISWAPX_FILL_INVALID",
                "A reverted filler transaction cannot settle an order.",
            )?;
            verify_fill(&receipt, &b, &s.reference)?;
            (receipt, U256::ZERO)
        } else {
            require(
                s.kind == "ethereum"
                    && s.data["metadata"]["binding_hash"] == binding_hash(&p.binding)?
                    && s.data["metadata"]["provider"] == PROVIDER,
                "SUBMISSION_IDENTITY_MISMATCH",
                "Durable swap does not match its provider quote binding.",
            )?;
            let tx: FrozenTransaction = serde_json::from_value(s.data["transaction"].clone())
                .map_err(|_| GoFinError::validation("Invalid durable API transaction."))?;
            require(
                tx.chain_id == b.chain_id
                    && address(&tx.sender)? == address(&b.owner)?
                    && tx.maximum_cost()? <= u256(&b.maximum_fee)?,
                "SUBMISSION_IDENTITY_MISMATCH",
                "Durable swap envelope exceeds its approved scope.",
            )?;
            let Some(receipt) = self
                .runtime
                .receipt(s, &tx, self.config.confirmations)
                .await?
            else {
                return Ok(pending(s, json!({"reason":"transaction-pending"})));
            };
            if self.config.requires_rpc_finalized()
                && self
                    .runtime
                    .observe_receipt(&s.reference, self.config.confirmations, true)
                    .await?
                    .is_none()
            {
                return Ok(pending(s, json!({"reason":"awaiting-finality"})));
            }
            let fee = receipt.fee;
            require(
                fee <= u256(&b.maximum_fee)?,
                "FEE_LIMIT",
                "Settled swap fee exceeds the approved bound.",
            )?;
            (receipt, fee)
        };
        let mut effects = vec![];
        if !fee.is_zero() {
            effects.push(FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: b.resource.clone(),
                },
                amount: amount(&self.native_asset(), fee),
                category: "ethereum_gas".into(),
                treatment: CostTreatment::AdditionalDebit,
            });
        }
        if !receipt.success {
            return verified(
                s,
                Status::Failed,
                effects,
                json!({"transaction_hash":format!("{:#x}",receipt.hash),"reverted":true,"fee_wei":quantity(fee)}),
            );
        }
        let owner = address(&b.owner)?;
        let native = if b.token.reference == "native" || b.output.reference == "native" {
            let tx = self
                .runtime
                .rpc
                .request(
                    "eth_getTransactionByHash",
                    json!([format!("{:#x}", receipt.hash)]),
                )
                .await?;
            let trace = self
                .runtime
                .rpc
                .request(
                    "debug_traceTransaction",
                    json!([format!("{:#x}",receipt.hash),{"tracer":"callTracer"}]),
                )
                .await?;
            require(
                trace["from"]
                    .as_str()
                    .is_some_and(|a| a.eq_ignore_ascii_case(tx["from"].as_str().unwrap_or("")))
                    && trace["to"]
                        .as_str()
                        .is_some_and(|a| a.eq_ignore_ascii_case(tx["to"].as_str().unwrap_or("")))
                    && trace["input"] == tx["input"]
                    && trace.get("error").is_none(),
                "UNISWAP_TRACE_INVALID",
                "Native swap trace does not identify the canonical transaction.",
            )?;
            trace_transfers(&trace, owner, 0)?
        } else {
            (U256::ZERO, U256::ZERO)
        };
        let (input_in, input_out) = if b.token.reference == "native" {
            native
        } else {
            transfer_totals(&receipt, &b.token, owner)?
        };
        let (output_in, output_out) = if b.output.reference == "native" {
            native
        } else {
            transfer_totals(&receipt, &b.output, owner)?
        };
        let spent = sub(input_out, input_in)?;
        let received = sub(output_in, output_out)?;
        require(
            spent == u256(&b.units)? && received >= u256(&b.minimum_output)?,
            "SWAP_SETTLEMENT_MISMATCH",
            "Verified wallet transfers violate the approved exact input or output floor.",
        )?;
        effects.push(FinancialEffect::Conversion {
            resource: b.resource.clone(),
            spent: amount(&b.token, spent),
            received: amount(&b.output, received),
        });
        if !self.inclusion_still_canonical(&receipt).await? {
            return Ok(pending(s, json!({"reason":"inclusion-changed"})));
        }
        verified(
            s,
            Status::Completed,
            effects,
            json!({"provider":PROVIDER,"routing":b.quote.routing,"order_id":b.quote.order_hash,"transaction_hash":format!("{:#x}",receipt.hash),"block_number":receipt.block_number,"block_hash":format!("{:#x}",receipt.block_hash),"input_atoms":quantity(spent),"output_atoms":quantity(received),"fee_wei":quantity(fee),"finality":self.config.finality_label()}),
        )
    }
}
fn verified(
    submission: &Submission,
    status: Status,
    effects: Vec<FinancialEffect>,
    evidence: Value,
) -> Result<Reconciliation> {
    let mut bundle = VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects,
        evidence: evidence.clone(),
        observed_at: now(),
        proof_digest: String::new(),
    };
    bundle.proof_digest = verified_effect_digest(&bundle)?;
    Ok(Reconciliation {
        status,
        evidence,
        verified: Some(bundle),
    })
}
