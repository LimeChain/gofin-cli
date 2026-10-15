//! Permit2 allowance inspection and exact approval lifecycle.

use super::*;
use abi::Permit2;
use gofin_evm::alloy::{
    primitives::{
        B256, U256,
        aliases::{U48, U160},
    },
    sol,
    sol_types::{SolCall, SolEvent},
};
use gofin_evm::{CanonicalReceipt, quantity, rpc_bytes, u256};
use serde::{Deserialize, Serialize};

sol! {
    interface Permit2ApprovalCall {
        function approve(address token,address spender,uint160 amount,uint48 expiration) external;
        event Approval(address indexed owner,address indexed token,address indexed spender,uint160 amount,uint48 expiration);
    }
}

const PERMIT2_ADDRESS: &str = "0x000000000022d473030f116ddee9f6b43ac78ba3";
const MAX_UINT48: u64 = (1u64 << 48) - 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct AllowanceState {
    amount: Quantity,
    expiration: u64,
    nonce: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Permit2Binding {
    transaction: FrozenTransaction,
    resource: ResourceId,
    owner: String,
    token: AssetId,
    token_address: String,
    permit2: String,
    spender: String,
    requested_expiration: u64,
    before: AllowanceState,
    after: AllowanceState,
    // The engine adds these broker-owned markers after adapter preparation.
    // Keep them in the parsed form and preserve the original binding by
    // refreshing a clone of the full PreparedOperation below.
    #[serde(
        default,
        rename = "_gofin_usd_ceiling_v1",
        alias = "_fin_usd_ceiling_v1",
        skip_serializing_if = "Option::is_none"
    )]
    value_bounds: Option<Value>,
    #[serde(
        default,
        rename = "__gofin_boundary_authorization",
        alias = "__fin_boundary_authorization",
        skip_serializing_if = "Option::is_none"
    )]
    boundary_authorization: Option<Value>,
    #[serde(
        default,
        rename = "__gofin_financial_plan",
        alias = "__fin_financial_plan",
        skip_serializing_if = "Option::is_none"
    )]
    financial_plan: Option<Value>,
}

#[derive(Clone, Copy)]
struct AllowanceRaw {
    amount: U160,
    expiration: U48,
    nonce: U48,
}

fn permit2_for(adapter: &EthereumAdapter) -> Result<(Address, Address)> {
    let v4 = adapter.uniswap()?.v4.as_ref().ok_or_else(|| {
        error(
            "OPERATION_UNAVAILABLE",
            "validation",
            "Permit2 operations require a configured Uniswap v4 deployment.",
        )
    })?;
    Ok((address(PERMIT2_ADDRESS)?, address(&v4.router)?))
}

fn actual_expiration(amount: U256, requested: u64, observed_block_timestamp: u64) -> Result<u64> {
    require(
        requested <= MAX_UINT48,
        "PERMIT2_EXPIRATION_INVALID",
        "Permit2 expiration exceeds uint48.",
    )?;
    if requested == 0 {
        require(
            amount.is_zero(),
            "PERMIT2_EXPIRATION_INVALID",
            "Zero Permit2 expiration is valid only when revoking the allowance with amount zero.",
        )?;
        require(
            observed_block_timestamp > 0 && observed_block_timestamp <= MAX_UINT48,
            "PERMIT2_EXPIRATION_INVALID",
            "Pinned block timestamp cannot be represented as a Permit2 expiration.",
        )?;
        Ok(observed_block_timestamp)
    } else {
        Ok(requested)
    }
}

async fn block_timestamp(adapter: &EthereumAdapter, block: u64) -> Result<u64> {
    let header = adapter
        .runtime
        .rpc
        .request(
            "eth_getBlockByNumber",
            json!([format!("{block:#x}"), false]),
        )
        .await?;
    let timestamp = gofin_evm::rpc_u64(&header["timestamp"])?;
    require(
        timestamp > 0 && timestamp <= MAX_UINT48,
        "PERMIT2_EXPIRATION_INVALID",
        "Pinned block timestamp is outside Permit2's uint48 range.",
    )?;
    Ok(timestamp)
}

async fn allowance_raw(
    adapter: &EthereumAdapter,
    owner: Address,
    token: Address,
    spender: Address,
    block: Option<u64>,
) -> Result<AllowanceRaw> {
    let value = adapter
        .runtime
        .call(
            address(PERMIT2_ADDRESS)?,
            &Permit2::allowanceCall {
                owner,
                token,
                spender,
            },
            block,
        )
        .await?;
    Ok(AllowanceRaw {
        amount: value.amount,
        expiration: value.expiration,
        nonce: value.nonce,
    })
}

fn u160(value: U256) -> Result<U160> {
    require(
        value < U256::from(U160::MAX),
        "PERMIT2_UNLIMITED_ALLOWANCE_DISALLOWED",
        "Unlimited Permit2 allowance is not supported; choose a finite amount or zero to revoke.",
    )?;
    let limbs = value.into_limbs();
    Ok(U160::from_limbs([limbs[0], limbs[1], limbs[2]]))
}
fn allowance_state(value: AllowanceRaw) -> AllowanceState {
    AllowanceState {
        amount: quantity(U256::from(value.amount)),
        expiration: value.expiration.to::<u64>(),
        nonce: value.nonce.to::<u64>(),
    }
}
pub(crate) fn approval_authority(token: &AssetId, spender: &str) -> String {
    format!("permit2:{}:spender:{spender}", token.reference)
}
pub(crate) fn allowance_state_text(amount: &Quantity, expiration: u64, nonce: u64) -> String {
    state_text(&AllowanceState {
        amount: amount.clone(),
        expiration,
        nonce,
    })
}
fn permit2_warnings(requested_expiration: u64) -> Vec<String> {
    let mut warnings = vec!["This changes token-spending authority for the configured Uniswap v4 router. It does not move tokens; native gas is charged.".into()];
    if requested_expiration == 0 {
        warnings.push("Permit2 treats zero expiration as the execution block timestamp. GoFin freezes the pinned timestamp for this zero-amount revocation; the stored expiry field cannot be restored to the literal zero value, while the approval call preserves the Permit2 nonce.".into());
    }
    warnings
}
fn state_text(state: &AllowanceState) -> String {
    serde_json::to_string(state).expect("allowance state is serializable")
}

impl EthereumAdapter {
    /// Inspect the exact Permit2 allowance for the configured v4 router.
    pub(crate) async fn permit2_allowance(&self, request: &request::Request) -> Result<Value> {
        self.runtime.assert_chain().await?;
        let owner = address(&self.resolve(request, false)?.address)?;
        let (permit2, router) = permit2_for(self)?;
        let block = gofin_evm::rpc_u64(
            &self
                .runtime
                .rpc
                .request("eth_blockNumber", json!([]))
                .await?,
        )?;
        let token = self
            .token(
                request.token.as_deref().expect("validated token"),
                Some(block),
            )
            .await?;
        require(
            token.reference != "native",
            "NATIVE_ALLOWANCE_INVALID",
            "Permit2 allowances require an ERC-20 token.",
        )?;
        let spender = address(request.spender.as_deref().expect("validated spender"))?;
        require(
            spender == router,
            "PERMIT2_SPENDER_INVALID",
            "Permit2 spender must be the configured Uniswap v4 router.",
        )?;
        self.runtime.code_at(permit2, Some(block)).await?;
        self.runtime.code_at(router, Some(block)).await?;
        self.runtime
            .code_at(address(&token.reference)?, Some(block))
            .await?;
        let state = allowance_state(
            allowance_raw(
                self,
                owner,
                address(&token.reference)?,
                spender,
                Some(block),
            )
            .await?,
        );
        Ok(json!({
            "wallet":request.wallet, "network":format!("ethereum:{}", self.config.chain_id),
            "chain_id":self.config.chain_id, "resource":resource(self.config.chain_id,owner),
            "token":token.asset(self), "spender":lower_address(spender),
            "amount":state.amount, "expiration":state.expiration, "nonce":state.nonce,
            "block_number":block,
        }))
    }

    /// Prepare a finite uint160 Permit2 allowance mutation. This changes only
    /// Permit2's token/spender authority; native gas is the only cash cost.
    pub(crate) async fn prepare_permit2_approval(
        &self,
        input: &Value,
        original: Option<&PreparedOperation>,
        _context: &PreparationContext,
    ) -> Result<PreparedOperation> {
        let r = request::parse("wallet.permit2.approve", input)?;
        if let Some(original) = original {
            return self.permit2_revalidate(original).await;
        }
        self.runtime.assert_chain().await?;
        let wallet = self.resolve(&r, true)?;
        let owner = address(&wallet.address)?;
        let (permit2, router) = permit2_for(self)?;
        let block = gofin_evm::rpc_u64(
            &self
                .runtime
                .rpc
                .request("eth_blockNumber", json!([]))
                .await?,
        )?;
        let token = self
            .token(r.token.as_deref().expect("validated token"), Some(block))
            .await?;
        require(
            token.reference != "native",
            "NATIVE_ALLOWANCE_INVALID",
            "Permit2 approvals require an ERC-20 token.",
        )?;
        let token_address = address(&token.reference)?;
        let spender = address(r.spender.as_deref().expect("validated spender"))?;
        require(
            spender == router,
            "PERMIT2_SPENDER_INVALID",
            "Permit2 spender must be the configured Uniswap v4 router.",
        )?;
        let units = exact(
            r.amount.as_deref().expect("validated amount"),
            token.decimals,
        )?;
        let target_amount = u160(units)?;
        let requested_expiration = input_expiration(input)?;
        self.runtime.code_at(permit2, Some(block)).await?;
        self.runtime.code_at(router, Some(block)).await?;
        self.runtime.code_at(token_address, Some(block)).await?;
        let expiration = actual_expiration(
            units,
            requested_expiration,
            block_timestamp(self, block).await?,
        )?;
        let target_expiration = U48::from(expiration);
        let before =
            allowance_state(allowance_raw(self, owner, token_address, spender, Some(block)).await?);
        let after = AllowanceState {
            amount: quantity(U256::from(target_amount)),
            expiration,
            nonce: before.nonce,
        };
        let calldata = Permit2ApprovalCall::approveCall {
            token: token_address,
            spender,
            amount: target_amount,
            expiration: target_expiration,
        }
        .abi_encode();
        let max_fee = exact(
            r.max_fee_eth
                .as_deref()
                .expect("write requires max_fee_eth"),
            18,
        )?;
        require(
            max_fee > U256::ZERO,
            "FEE_LIMIT",
            "Explicit max_fee_eth must be positive.",
        )?;
        let prepared_call = self
            .runtime
            .prepare(owner, permit2, U256::ZERO, calldata.into(), max_fee)
            .await?;
        let resource = resource(self.config.chain_id, owner);
        let b = Permit2Binding {
            transaction: prepared_call.transaction.clone(),
            resource: resource.clone(),
            owner: lower_address(owner),
            token: token.asset(self),
            token_address: lower_address(token_address),
            permit2: lower_address(permit2),
            spender: lower_address(spender),
            requested_expiration,
            before: before.clone(),
            after: after.clone(),
            value_bounds: None,
            boundary_authorization: None,
            financial_plan: None,
        };
        let gas = prepared_call.transaction.maximum_cost()?;
        let (fee_usd, source, fee_observed) =
            self.quote_usd_observed("native", quantity(gas)).await?;
        let unit = quantity(pow10(18)?);
        let (unit_usd, unit_source, unit_observed) =
            self.quote_usd_observed("native", unit.clone()).await?;
        let valuation = Valuation {
            movement_usd_micros: Quantity::zero(),
            maximum_fee_usd_micros: fee_usd,
            additional_allocation_usd_micros: Quantity::zero(),
            observed_at: fee_observed.clone(),
            source: source.clone(),
            prices: vec![AssetPrice {
                asset: self.native_asset(),
                usd_micros_per_unit: unit_usd,
                unit_atoms: unit,
                observed_at: unit_observed,
                source: unit_source,
            }],
        };
        let intent = FinancialIntent {
            debits: vec![],
            maximum_costs: vec![CostBound {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: amount(&self.native_asset(), gas),
                treatment: CostTreatment::AdditionalDebit,
            }],
            expected: vec![EffectConstraint::AuthorityTransition {
                resource: resource.clone(),
                authority: approval_authority(&b.token, &b.spender),
                before: state_text(&before),
                allowed_after: vec![state_text(&after)],
            }],
            locks: vec![resource.clone()],
        };
        let observed = now();
        Ok(PreparedOperation {
            action_deadline: None,
            operation:"wallet.permit2.approve".into(), input:input.clone(),
            binding:serde_json::to_value(&b).expect("Permit2 binding serializes"),
            summary:format!("Set Permit2 allowance for {} to {} atoms through {} (expiration {}).", b.token.reference, after.amount, b.spender, after.expiration),
            checks:vec![Check { name:"chain, token, Permit2 deployment, router and allowance state".into(),status:CheckStatus::Passed,detail:"Pinned reads verified the current exact Permit2 allowance and configured v4 router.".into() }],
            warnings:permit2_warnings(requested_expiration),
            observed_at:observed.clone(),expires_at:(chrono::Utc::now()+chrono::Duration::seconds(120)).to_rfc3339(),valuation:Some(valuation),intent,operation_version:1,
        })
    }

    /// Revalidate the exact original envelope and allowance pre-state before
    /// the journal-authorized signer is reached.
    pub(crate) async fn permit2_revalidate(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<PreparedOperation> {
        require(
            prepared.operation == "wallet.permit2.approve",
            "OPERATION_MISMATCH",
            "Permit2 revalidation received another operation.",
        )?;
        let b: Permit2Binding = serde_json::from_value(prepared.binding.clone())
            .map_err(|_| GoFinError::validation("Permit2 binding is invalid."))?;
        require(
            serde_json::to_value(&b).ok().as_ref() == Some(&prepared.binding),
            "BINDING_INVALID",
            "Permit2 binding contains unsupported or noncanonical fields.",
        )?;
        let r = request::parse("wallet.permit2.approve", &prepared.input)?;
        self.runtime.assert_chain().await?;
        let wallet = self.resolve(&r, true)?;
        let owner = address(&wallet.address)?;
        let (permit2, router) = permit2_for(self)?;
        let token = self
            .token(r.token.as_deref().expect("validated token"), None)
            .await?;
        let spender = address(r.spender.as_deref().expect("validated spender"))?;
        let current = allowance_state(
            allowance_raw(self, owner, address(&token.reference)?, spender, None).await?,
        );
        require(
            b.owner == lower_address(owner)
                && b.resource == resource(self.config.chain_id, owner)
                && b.token == token.asset(self)
                && b.token_address == token.reference
                && b.permit2 == lower_address(permit2)
                && b.spender == lower_address(router)
                && spender == router,
            "BINDING_CHANGED",
            "Permit2 token, spender, signer or network differs from approved identity.",
        )?;
        require(
            current == b.before,
            "PERMIT2_STATE_CHANGED",
            "Permit2 allowance changed since preview; refresh before approval.",
        )?;
        let units = exact(
            r.amount.as_deref().expect("validated amount"),
            token.decimals,
        )?;
        let target = u160(units)?;
        let expiration = input_expiration(&prepared.input)?;
        let expected_expiration = if expiration == 0 {
            require(
                target.is_zero() && b.after.expiration > 0,
                "BINDING_CHANGED",
                "Zero expiry is only valid for a zero-amount revocation with a frozen positive block timestamp.",
            )?;
            b.after.expiration
        } else {
            expiration
        };
        require(
            b.requested_expiration == expiration
                && b.after
                    == AllowanceState {
                        amount: quantity(U256::from(target)),
                        expiration: expected_expiration,
                        nonce: b.before.nonce,
                    },
            "BINDING_CHANGED",
            "Permit2 desired allowance differs from approved input.",
        )?;
        let expected = Permit2ApprovalCall::approveCall {
            token: address(&b.token_address)?,
            spender,
            amount: target,
            expiration: U48::from(expected_expiration),
        }
        .abi_encode();
        require(
            b.transaction.to == lower_address(permit2)
                && b.transaction.sender == lower_address(owner)
                && b.transaction.chain_id == self.config.chain_id
                && b.transaction.value == Quantity::zero()
                && rpc_bytes(&json!(b.transaction.calldata))? == expected,
            "BINDING_CHANGED",
            "Permit2 frozen transaction differs from the exact approved allowance.",
        )?;
        self.runtime.revalidate(&b.transaction).await?;
        let gas = b.transaction.maximum_cost()?;
        let (fee_usd, source, fee_observed) =
            self.quote_usd_observed("native", quantity(gas)).await?;
        let unit = quantity(pow10(18)?);
        let (unit_usd, unit_source, unit_observed) =
            self.quote_usd_observed("native", unit.clone()).await?;
        let mut refreshed = prepared.clone();
        refreshed.observed_at = now();
        refreshed.expires_at = (chrono::Utc::now() + chrono::Duration::seconds(120)).to_rfc3339();
        refreshed.valuation = Some(Valuation {
            movement_usd_micros: Quantity::zero(),
            maximum_fee_usd_micros: fee_usd,
            additional_allocation_usd_micros: Quantity::zero(),
            observed_at: fee_observed,
            source,
            prices: vec![AssetPrice {
                asset: self.native_asset(),
                usd_micros_per_unit: unit_usd,
                unit_atoms: unit,
                observed_at: unit_observed,
                source: unit_source,
            }],
        });
        carry_attempt_valuation(prepared, &mut refreshed, self.clock.now())?;
        gofin_domain::value_bounds::validate(&refreshed)?;
        Ok(refreshed)
    }

    pub(crate) async fn permit2_submit_revalidated(
        &self,
        validated: &RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        let prepared = validated.prepared();
        let b: Permit2Binding = serde_json::from_value(prepared.binding.clone())
            .map_err(|_| GoFinError::validation("Permit2 binding is invalid."))?;
        self.runtime
            .submit_revalidated(
                &b.transaction,
                json!({"operation":prepared.operation,"resource":b.resource}),
                journal,
                validated,
            )
            .await
    }

    pub(crate) async fn permit2_reconcile(
        &self,
        record: &OperationRecord,
    ) -> Result<Reconciliation> {
        let submission = record.submission.as_ref().ok_or_else(|| {
            error(
                "SUBMISSION_REQUIRED",
                "reconciliation",
                "Original Permit2 submission identity is missing.",
            )
        })?;
        let preview = record.preview.as_ref().ok_or_else(|| {
            error(
                "BINDING_REQUIRED",
                "reconciliation",
                "Original Permit2 preview is missing.",
            )
        })?;
        require(
            record.operation == "wallet.permit2.approve"
                && preview.operation == record.operation
                && preview.input == record.input,
            "BINDING_IDENTITY_MISMATCH",
            "Permit2 record differs from its original preview.",
        )?;
        let b: Permit2Binding = serde_json::from_value(preview.binding.clone()).map_err(|_| {
            error(
                "BINDING_INVALID",
                "reconciliation",
                "Permit2 binding is malformed.",
            )
        })?;
        let submitted: FrozenTransaction =
            serde_json::from_value(submission.data["transaction"].clone()).map_err(|_| {
                error(
                    "SUBMISSION_IDENTITY_MISMATCH",
                    "reconciliation",
                    "Permit2 submitted envelope is malformed.",
                )
            })?;
        require(
            submitted == b.transaction
                && submission.data["metadata"]["operation"] == json!(record.operation)
                && submission.data["metadata"]["resource"] == json!(b.resource),
            "SUBMISSION_IDENTITY_MISMATCH",
            "Permit2 submission differs from the original frozen envelope.",
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
        if self.config.requires_rpc_finalized()
            && !self.finalized_permit2_inclusion(&receipt).await?
        {
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"original_hash":submission.reference,"canonical_confirmed":true,"finality":"awaiting-rpc-finalized"}),
                verified: None,
            });
        }
        let mut effects = vec![FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: b.resource.clone(),
            },
            amount: amount(&self.native_asset(), receipt.fee),
            category: "ethereum_gas".into(),
            treatment: CostTreatment::AdditionalDebit,
        }];
        let success = receipt.success;
        let state = allowance_state(
            allowance_raw(
                self,
                address(&b.owner)?,
                address(&b.token_address)?,
                address(&b.spender)?,
                Some(receipt.block_number),
            )
            .await?,
        );
        if success {
            let owner = address(&b.owner)?;
            let token = address(&b.token_address)?;
            let spender = address(&b.spender)?;
            let matching = p2_approval_events(&receipt.receipt, address(&b.permit2)?)?
                .into_iter()
                .filter(|event| {
                    event.owner == owner && event.token == token && event.spender == spender
                })
                .collect::<Vec<_>>();
            let [event] = matching.as_slice() else {
                return Err(error(
                    "PERMIT2_PROOF_MISMATCH",
                    "reconciliation",
                    "Canonical Permit2 approval event is missing or ambiguous.",
                ));
            };
            require(
                event.amount == u160(u256(&b.after.amount)?)?
                    && event.expiration == U48::from(b.after.expiration)
                    && state.amount == b.after.amount
                    && state.expiration == b.after.expiration
                    && state.nonce == b.before.nonce,
                "PERMIT2_PROOF_MISMATCH",
                "Permit2 approval event or inclusion-block allowance differs from the original scope.",
            )?;
            effects.push(FinancialEffect::AuthorityChange {
                resource: b.resource.clone(),
                authority: approval_authority(&b.token, &b.spender),
                before: state_text(&b.before),
                after: state_text(&state),
            });
        } else {
            require(
                state == b.before,
                "PERMIT2_PROOF_MISMATCH",
                "Reverted Permit2 approval changed its inclusion-block allowance.",
            )?;
        }
        if !self.inclusion_still_canonical(&receipt).await? {
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"original_hash":submission.reference,"canonical_confirmed":false,"finality":"inclusion-changed"}),
                verified: None,
            });
        }
        let evidence = json!({"original_hash":submission.reference,"block_number":receipt.block_number,"block_hash":format!("{:#x}",receipt.block_hash),"confirmations":receipt.confirmations,"success":success,"fee_wei":quantity(receipt.fee),"allowance":state,"finality":self.config.finality_label()});
        let mut verified = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects,
            evidence: evidence.clone(),
            observed_at: now(),
            proof_digest: String::new(),
        };
        verified.proof_digest = verified_effect_digest(&verified)?;
        Ok(Reconciliation {
            status: if success {
                Status::Completed
            } else {
                Status::Failed
            },
            evidence,
            verified: Some(verified),
        })
    }

    async fn finalized_permit2_inclusion(&self, receipt: &CanonicalReceipt) -> Result<bool> {
        let latest = self
            .runtime
            .rpc
            .request("eth_getBlockByNumber", json!(["finalized", false]))
            .await;
        let Some(height) = latest
            .as_ref()
            .ok()
            .filter(|value| !value.is_null())
            .and_then(|value| gofin_evm::rpc_u64(&value["number"]).ok())
        else {
            return Ok(false);
        };
        if height < receipt.block_number {
            return Ok(false);
        }
        let block = self
            .runtime
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([format!("{:#x}", receipt.block_number), false]),
            )
            .await?;
        Ok(block["hash"] == json!(format!("{:#x}", receipt.block_hash))
            && gofin_evm::rpc_u64(&block["number"])? == receipt.block_number)
    }
}

fn input_expiration(input: &Value) -> Result<u64> {
    let expiration = input
        .get("expiration")
        .and_then(Value::as_u64)
        .ok_or_else(|| GoFinError::validation("Permit2 approval requires a uint48 expiration."))?;
    if expiration > MAX_UINT48 {
        return Err(GoFinError::validation("Permit2 expiration exceeds uint48."));
    }
    Ok(expiration)
}

fn p2_approval_events(
    receipt: &Value,
    emitter: Address,
) -> Result<Vec<Permit2ApprovalCall::Approval>> {
    let logs = receipt["logs"].as_array().ok_or_else(|| {
        error(
            "RECEIPT_LOGS_INVALID",
            "reconciliation",
            "Canonical receipt lacks a log array.",
        )
    })?;
    let mut events = Vec::new();
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
                    "Permit2 event topics are invalid.",
                )
            })?
            .iter()
            .map(|topic| gofin_evm::hash(topic.as_str().unwrap_or("")))
            .collect::<Result<Vec<B256>>>()?;
        if topics.first() != Some(&Permit2ApprovalCall::Approval::SIGNATURE_HASH) {
            continue;
        }
        require(
            log["removed"].as_bool() != Some(true)
                && log["transactionHash"] == receipt["transactionHash"]
                && log["blockHash"] == receipt["blockHash"]
                && log["blockNumber"] == receipt["blockNumber"],
            "RECEIPT_LOG_IDENTITY",
            "Permit2 Approval event inclusion differs from its canonical receipt.",
        )?;
        events.push(
            Permit2ApprovalCall::Approval::decode_raw_log_validate(
                topics.iter().copied(),
                &rpc_bytes(&log["data"])?,
            )
            .map_err(|_| {
                error(
                    "RECEIPT_EVENT_INVALID",
                    "reconciliation",
                    "Permit2 Approval event does not match the reviewed ABI.",
                )
            })?,
        );
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permit2_uint_width_and_expiration_boundaries_are_exact() {
        assert_eq!(
            u160(U256::from(U160::MAX) - U256::from(1)).unwrap(),
            U160::MAX - U160::from(1)
        );
        assert_eq!(
            u160(U256::from(U160::MAX)).unwrap_err().code,
            "PERMIT2_UNLIMITED_ALLOWANCE_DISALLOWED"
        );
        assert!(u160(U256::from(U160::MAX) + U256::from(1)).is_err());
        assert_eq!(input_expiration(&json!({"expiration":0})).unwrap(), 0);
        assert_eq!(
            input_expiration(&json!({"expiration":MAX_UINT48})).unwrap(),
            MAX_UINT48
        );
        assert!(input_expiration(&json!({"expiration":MAX_UINT48 + 1})).is_err());
        assert!(input_expiration(&json!({"expiration":"0"})).is_err());
        assert_eq!(
            actual_expiration(U256::ZERO, 0, 1_700_000_000).unwrap(),
            1_700_000_000
        );
        assert!(actual_expiration(U256::from(1), 0, 1_700_000_000).is_err());
        assert!(actual_expiration(U256::ZERO, 0, 0).is_err());
        assert_eq!(actual_expiration(U256::from(1), 123, 999).unwrap(), 123);
    }

    #[test]
    fn approval_event_decoder_requires_the_permit2_emitter_and_exact_topics() {
        let owner = address("0x0000000000000000000000000000000000000001").unwrap();
        let token = address("0x0000000000000000000000000000000000000002").unwrap();
        let spender = address("0x0000000000000000000000000000000000000003").unwrap();
        let event = Permit2ApprovalCall::Approval {
            owner,
            token,
            spender,
            amount: U160::from(7),
            expiration: U48::from(9),
        };
        let topics = event.encode_topics();
        let data = event.encode_log_data();
        let receipt = json!({"transactionHash":"0x01", "blockHash":"0x02", "blockNumber":"0x3", "logs":[{
            "address":PERMIT2_ADDRESS,
            "topics":topics.iter().map(|topic|format!("{:#x}", topic.0)).collect::<Vec<_>>(),
            "data":format!("0x{}", hex::encode(data.data)),
            "transactionHash":"0x01", "blockHash":"0x02", "blockNumber":"0x3", "removed":false
        }]});
        let decoded = p2_approval_events(&receipt, address(PERMIT2_ADDRESS).unwrap()).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].owner, owner);
        assert_eq!(decoded[0].amount, U160::from(7));
        assert!(
            p2_approval_events(
                &receipt,
                address("0x0000000000000000000000000000000000000004").unwrap()
            )
            .unwrap()
            .is_empty()
        );
    }
}
