//! Synchronous accounting helpers for Aave observation evidence.
//!
//! Chain reads and timestamps are supplied as facts; no RPC or clock is used here.
use super::super::*;
use crate::collection::{ReadCall, ReadCollector};
use gofin_evm::alloy::sol_types::SolCall;
use gofin_evm::{hash, quantity, u256};

fn decode_call<C: SolCall>(value: Option<Result<Vec<u8>>>) -> Result<C::Return> {
    let bytes = value.ok_or_else(|| {
        error(
            "MULTICALL_INCOMPLETE",
            "read",
            "Required observation result is missing.",
        )
    })??;
    C::abi_decode_returns(&bytes).map_err(|_| {
        error(
            "CONTRACT_DATA_INVALID",
            "read",
            "Contract return does not match the reviewed ABI.",
        )
    })
}

/// Independently observed values for one receipt token at one exact block.
#[derive(Clone, Debug)]
pub(crate) struct AnchorFacts {
    pub(crate) block: u64,
    pub(crate) block_hash: String,
    pub(crate) revision: Option<U256>,
    pub(crate) scaled: U256,
    pub(crate) index: U256,
    pub(crate) token_balance: U256,
}

/// Immutable context selected by the frozen operation and reviewed market.
#[derive(Clone, Debug)]
pub(crate) struct AnchorContext {
    pub(crate) chain_id: u64,
    pub(crate) pool: String,
    pub(crate) owner: Address,
    pub(crate) asset: AssetId,
    pub(crate) receipt_token: Address,
    pub(crate) kind: PositionKind,
    pub(crate) horizon: bool,
}
impl AnchorContext {
    fn method(&self) -> &'static str {
        if self.horizon {
            "aave-horizon-revision-3-1-half-up-backing"
        } else {
            "aave-v3-revision-5-exact-scaled-backing"
        }
    }
    fn expected_revision(&self) -> U256 {
        U256::from(if self.horizon {
            if self.kind == PositionKind::Supply {
                3
            } else {
                1
            }
        } else {
            5
        })
    }
    fn backing(&self, scaled: U256, index: U256) -> Result<U256> {
        if self.horizon {
            Ok(add(mul(scaled, index)?, pow10(27)? / U256::from(2))? / pow10(27)?)
        } else {
            anchors::backing(scaled, index, &self.kind)
        }
    }
    fn position(&self) -> String {
        position(
            &self.pool,
            &self.asset.reference,
            if self.kind == PositionKind::Supply {
                "supply"
            } else {
                "variable-debt"
            },
        )
    }
    /// Reconstruct an anchor only after exact evidence has been supplied.
    pub(crate) fn anchor(
        &self,
        facts: &AnchorFacts,
        zero: bool,
        rounding: U256,
        interest: U256,
        observed_at: String,
    ) -> Result<PositionAnchor> {
        require(
            matches!(self.kind, PositionKind::Supply | PositionKind::VariableDebt),
            "ANCHOR_KIND_UNSUPPORTED",
            "Only scaled supply and variable debt have backing anchors.",
        )?;
        hash(&facts.block_hash)?;
        require(
            facts.index >= pow10(27)?
                && facts.index < (U256::from(1) << 128)
                && (zero || facts.scaled < (U256::from(1) << 120)),
            "ANCHOR_INDEX_INVALID",
            "Scaled backing or normalized index exceeds reviewed bounds.",
        )?;
        let claim = if zero {
            require(
                facts.scaled.is_zero() && facts.token_balance.is_zero(),
                "ANCHOR_BACKING_MISMATCH",
                "An empty backing anchor requires exact zero balances.",
            )?;
            require(
                rounding.is_zero() && interest.is_zero(),
                "ANCHOR_COST_INVALID",
                "Empty backing cannot carry deferred expenses.",
            )?;
            U256::ZERO
        } else {
            require(
                facts.revision == Some(self.expected_revision()),
                "TOKEN_REVISION_UNSUPPORTED",
                "Exact backing requires the reviewed token revision.",
            )?;
            let claim = self.backing(facts.scaled, facts.index)?;
            require(
                claim == facts.token_balance,
                "ANCHOR_BACKING_MISMATCH",
                "Exact scaled backing differs from token balance.",
            )?;
            claim
        };
        Ok(PositionAnchor {
            resource: resource(self.chain_id, self.owner),
            position: self.position(),
            position_kind: self.kind.clone(),
            principal: amount(&self.asset, claim),
            scaled_receipt: Some(quantity(facts.scaled)),
            evidence_version: 1,
            evidence: json!({"method":if zero { "aave-zero-observed-backing" } else { self.method() },"chain_id":self.chain_id,"pool":self.pool,"underlying":self.asset.reference,"receipt_token":lower_address(self.receipt_token),"block":facts.block,"block_hash":facts.block_hash,"normalized_index":quantity(facts.index),"scaled_balance":quantity(facts.scaled),"deferred_unpaid_rounding":quantity(rounding),"deferred_unpaid_interest":quantity(interest)}),
            observed_at,
        })
    }
    /// Validate a frozen anchor against independently gathered historical facts.
    pub(crate) fn validate_prior(&self, prior: &PositionAnchor, facts: &AnchorFacts) -> Result<()> {
        require(
            prior.evidence_version == 1
                && prior.resource == resource(self.chain_id, self.owner)
                && prior.principal.asset == self.asset
                && prior.position_kind == self.kind
                && prior.position == self.position()
                && prior.evidence["chain_id"] == json!(self.chain_id)
                && prior.evidence["pool"] == json!(self.pool)
                && prior.evidence["receipt_token"] == json!(lower_address(self.receipt_token)),
            "ANCHOR_IDENTITY_INVALID",
            "Trusted anchor does not bind the same immutable position representation.",
        )?;
        let method = prior.evidence["method"].as_str().unwrap_or("");
        require(
            method == self.method() || method == "aave-zero-observed-backing",
            "ANCHOR_IDENTITY_INVALID",
            "Trusted anchor uses an unsupported backing method.",
        )?;
        let rounding: Quantity = serde_json::from_value(
            prior.evidence["deferred_unpaid_rounding"].clone(),
        )
        .map_err(|_| {
            error(
                "ANCHOR_INVALID",
                "prepare",
                "Anchor deferred rounding is invalid.",
            )
        })?;
        let interest: Quantity = serde_json::from_value(
            prior.evidence["deferred_unpaid_interest"].clone(),
        )
        .map_err(|_| {
            error(
                "ANCHOR_INVALID",
                "prepare",
                "Anchor unpaid interest is invalid.",
            )
        })?;
        let rounding = u256(&rounding)?;
        let interest = u256(&interest)?;
        require(
            add(rounding, interest)? <= u256(&prior.principal.atoms)?,
            "ANCHOR_COST_INVALID",
            "Deferred debt expenses exceed exact physical backing.",
        )?;
        let observed = self.anchor(
            facts,
            method == "aave-zero-observed-backing",
            rounding,
            interest,
            prior.observed_at.clone(),
        )?;
        require(
            observed.principal == prior.principal
                && observed.scaled_receipt == prior.scaled_receipt
                && observed.evidence == prior.evidence,
            "ANCHOR_PROOF_MISMATCH",
            "Historical anchor backing or canonical inclusion no longer matches.",
        )
    }
    pub(crate) fn accrued_before(
        &self,
        prior: &PositionAnchor,
        historical: &AnchorFacts,
        before_scaled: U256,
        current_index: U256,
    ) -> Result<U256> {
        self.validate_prior(prior, historical)?;
        require(
            prior.scaled_receipt.as_ref() == Some(&quantity(before_scaled)),
            "POSITION_PRINCIPAL_CHANGED",
            "Scaled principal changed outside its broker-proved lineage.",
        )?;
        let previous: Quantity = serde_json::from_value(prior.evidence["normalized_index"].clone())
            .map_err(|_| {
                error(
                    "ANCHOR_INVALID",
                    "reconciliation",
                    "Anchor normalized index is missing.",
                )
            })?;
        require(
            current_index >= u256(&previous)?,
            "INDEX_REGRESSED",
            "Current normalized index regressed below proved anchor.",
        )?;
        sub(
            self.backing(before_scaled, current_index)?,
            u256(&prior.principal.atoms)?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn anchor_evaluation_rejects_changed_historical_facts_without_io() {
        let asset = AssetId {
            network: "ethereum:1".into(),
            reference: lower_address(Address::from([1; 20])),
            decimals: 6,
        };
        let ctx = AnchorContext {
            chain_id: 1,
            pool: lower_address(Address::from([2; 20])),
            owner: Address::from([3; 20]),
            asset,
            receipt_token: Address::from([4; 20]),
            kind: PositionKind::Supply,
            horizon: false,
        };
        let mut facts = AnchorFacts {
            block: 10,
            block_hash: format!("0x{}", "12".repeat(32)),
            revision: Some(U256::from(5)),
            scaled: U256::from(100),
            index: pow10(27).unwrap(),
            token_balance: U256::from(100),
        };
        let prior = ctx
            .anchor(
                &facts,
                false,
                U256::ZERO,
                U256::ZERO,
                "2026-01-01T00:00:00Z".into(),
            )
            .unwrap();
        assert!(ctx.validate_prior(&prior, &facts).is_ok());
        let again = ctx
            .anchor(
                &facts,
                false,
                U256::ZERO,
                U256::ZERO,
                prior.observed_at.clone(),
            )
            .unwrap();
        assert_eq!(again, prior);
        let mut reorg = facts.clone();
        reorg.block_hash = format!("0x{}", "ab".repeat(32));
        assert_eq!(
            ctx.validate_prior(&prior, &reorg).unwrap_err().code,
            "ANCHOR_PROOF_MISMATCH"
        );
        assert_eq!(
            ctx.accrued_before(&prior, &facts, U256::from(100), facts.index)
                .unwrap(),
            U256::ZERO
        );
        facts.token_balance += U256::from(1);
        assert_eq!(
            ctx.validate_prior(&prior, &facts).unwrap_err().code,
            "ANCHOR_BACKING_MISMATCH"
        );
    }
    #[tokio::test]
    async fn repeated_collateral_evaluation_uses_no_rpc_and_checks_receipt() {
        use gofin_evm::alloy::sol_types::SolEvent;
        let rpc = Arc::new(crate::tests::Fixture::new());
        let a = crate::tests::adapter(rpc.clone());
        let mut input = crate::tests::input();
        input["token"] = json!(lower_address(crate::tests::addr(101)));
        input["amount"] = json!("1");
        let prepared = a.prepare("aave.supply", &input).await.unwrap();
        let mut b: Binding = serde_json::from_value(prepared.binding).unwrap();
        b.enabled = Some(true);
        b.collateral_before = Some(false);
        let owner = address(&b.transaction.sender).unwrap();
        let event = abi::Pool::ReserveUsedAsCollateralEnabled {
            reserve: address(&b.token.reference).unwrap(),
            user: owner,
        };
        let data = event.encode_log_data();
        let tx = format!("0x{}", "34".repeat(32));
        let block_hash = format!("0x{}", "56".repeat(32));
        let receipt = json!({"transactionHash":tx,"blockHash":block_hash,"blockNumber":"0x15","logs":[{"address":b.pool,"topics":data.topics().iter().map(|t|format!("{t:#x}")).collect::<Vec<_>>(),"data":gofin_evm::hex_bytes(&data.data),"transactionHash":tx,"blockHash":block_hash,"blockNumber":"0x15","removed":false}]});
        let observation = AaveObservation {
            receipt: gofin_evm::CanonicalReceipt {
                hash: hash(&tx).unwrap(),
                transaction_sender: b.transaction.sender.clone(),
                block_hash: hash(&block_hash).unwrap(),
                block_number: 21,
                confirmations: 1,
                success: true,
                fee: None,
                receipt,
            },
            post: reads::UserReserve {
                token: reads::Token {
                    reference: b.token.reference.clone(),
                    decimals: b.token.decimals,
                    symbol: String::new(),
                },
                supplied: Quantity::zero(),
                stable_debt: Quantity::zero(),
                variable_debt: Quantity::zero(),
                scaled_variable_debt: Quantity::zero(),
                collateral_enabled: true,
            },
            context: None,
            historical: None,
            inclusion: None,
            before_scaled: None,
            before_index: None,
            after_index: None,
            observed_at: "2026-01-01T00:00:00Z".into(),
        };
        rpc.calls.lock().unwrap().clear();
        let mut first = Vec::new();
        let mut first_details = json!({});
        observation
            .evaluate_success("aave.collateral", &b, &mut first, &mut first_details)
            .unwrap();
        let mut second = Vec::new();
        let mut second_details = json!({});
        observation
            .evaluate_success("aave.collateral", &b, &mut second, &mut second_details)
            .unwrap();
        assert_eq!(
            serde_json::to_value(first).unwrap(),
            serde_json::to_value(second).unwrap()
        );
        assert_eq!(first_details, second_details);
        assert!(rpc.calls.lock().unwrap().is_empty());
        let mut forged = observation.clone();
        forged.receipt.receipt["logs"] = json!([]);
        assert_eq!(
            forged
                .evaluate_success("aave.collateral", &b, &mut Vec::new(), &mut json!({}))
                .unwrap_err()
                .code,
            "COLLATERAL_PROOF_MISMATCH"
        );
        assert!(rpc.calls.lock().unwrap().is_empty());
    }
}

/// State facts for one successful, canonical Aave receipt. Economic interpretation
/// is deliberately deferred until after the collector has returned.
#[derive(Clone, Debug)]
pub(crate) struct AaveObservation {
    pub(crate) receipt: gofin_evm::CanonicalReceipt,
    pub(crate) post: reads::UserReserve,
    pub(crate) context: Option<AnchorContext>,
    pub(crate) historical: Option<AnchorFacts>,
    pub(crate) inclusion: Option<AnchorFacts>,
    pub(crate) before_scaled: Option<U256>,
    pub(crate) before_index: Option<U256>,
    pub(crate) after_index: Option<U256>,
    pub(crate) observed_at: String,
}

impl EthereumAdapter {
    pub(crate) async fn observe_anchor_facts(
        &self,
        ctx: &AnchorContext,
        block: u64,
        zero: bool,
    ) -> Result<AnchorFacts> {
        let mut collector = ReadCollector::new(self, block).await;
        self.observe_anchor_facts_with_collector(ctx, block, zero, &mut collector)
            .await
    }
    pub(crate) async fn observe_anchor_facts_with_collector(
        &self,
        ctx: &AnchorContext,
        block: u64,
        zero: bool,
        collector: &mut ReadCollector,
    ) -> Result<AnchorFacts> {
        let (facts, _) = self
            .observe_anchor_facts_with_extra(ctx, block, zero, collector, Vec::new(), None)
            .await?;
        Ok(facts)
    }
    /// Reuse the already-observed capture header; callers still perform the
    /// uncached canonicality check at the end of the pass.
    pub(crate) async fn observe_anchor_facts_with_collector_and_header(
        &self,
        ctx: &AnchorContext,
        block: u64,
        zero: bool,
        collector: &mut ReadCollector,
        header: &Value,
    ) -> Result<AnchorFacts> {
        let (facts, _) = self
            .observe_anchor_facts_with_extra(ctx, block, zero, collector, Vec::new(), Some(header))
            .await?;
        Ok(facts)
    }
    async fn observe_anchor_facts_with_extra(
        &self,
        ctx: &AnchorContext,
        block: u64,
        zero: bool,
        collector: &mut ReadCollector,
        extra: Vec<ReadCall>,
        observed_header: Option<&Value>,
    ) -> Result<(AnchorFacts, Vec<Result<Vec<u8>>>)> {
        require(
            collector.block() == block,
            "AAVE_READ_CHANGED",
            "Anchor collector block differs from the requested block.",
        )?;
        let pool = address(&ctx.pool)?;
        let asset = address(&ctx.asset.reference)?;
        let mut calls = Vec::new();
        if !zero {
            calls.push(if ctx.kind == PositionKind::Supply {
                ReadCall::new(ctx.receipt_token, &abi::AToken::ATOKEN_REVISIONCall {})
            } else {
                ReadCall::new(ctx.receipt_token, &abi::AToken::DEBT_TOKEN_REVISIONCall {})
            });
        }
        calls.push(ReadCall::new(
            ctx.receipt_token,
            &abi::AToken::scaledBalanceOfCall { user: ctx.owner },
        ));
        calls.push(ReadCall::new(
            ctx.receipt_token,
            &abi::Erc20::balanceOfCall { owner: ctx.owner },
        ));
        calls.push(if ctx.kind == PositionKind::Supply {
            ReadCall::new(pool, &abi::Pool::getReserveNormalizedIncomeCall { asset })
        } else {
            ReadCall::new(
                pool,
                &abi::Pool::getReserveNormalizedVariableDebtCall { asset },
            )
        });
        calls.extend(extra);
        let mut values = collector.collect(calls).await.into_iter();
        let revision = if zero {
            None
        } else if ctx.kind == PositionKind::Supply {
            Some(decode_call::<abi::AToken::ATOKEN_REVISIONCall>(
                values.next(),
            )?)
        } else {
            Some(decode_call::<abi::AToken::DEBT_TOKEN_REVISIONCall>(
                values.next(),
            )?)
        };
        let scaled = decode_call::<abi::AToken::scaledBalanceOfCall>(values.next())?;
        let token_balance = decode_call::<abi::Erc20::balanceOfCall>(values.next())?;
        let index = if ctx.kind == PositionKind::Supply {
            decode_call::<abi::Pool::getReserveNormalizedIncomeCall>(values.next())?
        } else {
            decode_call::<abi::Pool::getReserveNormalizedVariableDebtCall>(values.next())?
        };
        let header = match observed_header {
            Some(header) => header.clone(),
            None => {
                self.runtime
                    .rpc
                    .request(
                        "eth_getBlockByNumber",
                        json!([gofin_evm::block_tag(Some(block)), false]),
                    )
                    .await?
            }
        };
        let block_hash = header["hash"].as_str().unwrap_or("").to_owned();
        hash(&block_hash)?;
        require(
            gofin_evm::rpc_u64(&header["number"])? == block,
            "ANCHOR_BLOCK_INVALID",
            "Backing observation block identity differs.",
        )?;
        Ok((
            AnchorFacts {
                block,
                block_hash,
                revision,
                scaled,
                index,
                token_balance,
            },
            values.collect(),
        ))
    }
    pub(crate) async fn collect_aave_receipt_facts(
        &self,
        operation: &str,
        b: &Binding,
        receipt: &gofin_evm::CanonicalReceipt,
    ) -> Result<AaveObservation> {
        let owner = address(&b.transaction.sender)?;
        let token = reads::Token {
            reference: b.token.reference.clone(),
            decimals: b.token.decimals,
            symbol: String::new(),
        };
        let provider = self.data_provider(Some(receipt.block_number)).await?;
        let asset = address(&token.reference)?;
        if operation == "aave.collateral" {
            // A single call gains nothing from deploying a batch envelope and code check.
            let user = self
                .runtime
                .call(
                    provider,
                    &abi::DataProvider::getUserReserveDataCall { asset, user: owner },
                    Some(receipt.block_number),
                )
                .await?;
            let post = reads::UserReserve {
                token,
                supplied: quantity(user.currentATokenBalance),
                stable_debt: quantity(user.currentStableDebt),
                variable_debt: quantity(user.currentVariableDebt),
                scaled_variable_debt: quantity(user.scaledVariableDebt),
                collateral_enabled: user.usageAsCollateralEnabled,
            };
            return Ok(AaveObservation {
                receipt: receipt.clone(),
                post,
                context: None,
                historical: None,
                inclusion: None,
                before_scaled: None,
                before_index: None,
                after_index: None,
                observed_at: now(),
            });
        }
        let mut inclusion_collector = ReadCollector::new(self, receipt.block_number).await;
        require(
            receipt.block_number > 0,
            "ANCHOR_BLOCK_INVALID",
            "Aave proof requires a previous inclusion block.",
        )?;
        let kind = if matches!(operation, "aave.supply" | "aave.withdraw") {
            PositionKind::Supply
        } else {
            PositionKind::VariableDebt
        };
        let prior = if kind == PositionKind::Supply {
            b.supply_anchor.as_ref()
        } else {
            b.debt_anchor.as_ref()
        }
        .ok_or_else(|| {
            error(
                "ANCHOR_INVALID",
                "reconciliation",
                "Frozen position anchor is missing.",
            )
        })?;
        let receipt_token = address(
            if kind == PositionKind::Supply {
                b.a_token.as_deref()
            } else {
                b.debt_token.as_deref()
            }
            .ok_or_else(|| {
                error(
                    "BINDING_INVALID",
                    "reconciliation",
                    "Receipt token identity is missing.",
                )
            })?,
        )?;
        let ctx = AnchorContext {
            chain_id: self.config.chain_id,
            pool: b.pool.clone().ok_or_else(|| {
                error(
                    "BINDING_INVALID",
                    "reconciliation",
                    "Pool identity is missing.",
                )
            })?,
            owner,
            asset: b.token.clone(),
            receipt_token,
            kind,
            horizon: self.horizon_math(),
        };
        let historical_block = prior.evidence["block"].as_u64().ok_or_else(|| {
            error(
                "ANCHOR_BLOCK_INVALID",
                "reconciliation",
                "Anchor inclusion block is missing.",
            )
        })?;
        let zero = prior.evidence["method"] == json!("aave-zero-observed-backing");
        let before_block = receipt.block_number - 1;
        let mut before_collector = ReadCollector::new(self, before_block).await;
        let historical = if historical_block == before_block {
            self.observe_anchor_facts_with_collector(
                &ctx,
                historical_block,
                zero,
                &mut before_collector,
            )
            .await?
        } else {
            self.observe_anchor_facts(&ctx, historical_block, zero)
                .await?
        };
        let mut previous_calls = vec![ReadCall::new(
            receipt_token,
            &abi::AToken::scaledBalanceOfCall { user: owner },
        )];
        if matches!(operation, "aave.supply" | "aave.borrow") {
            previous_calls.push(ReadCall::new(
                receipt_token,
                &abi::AToken::getPreviousIndexCall { user: owner },
            ));
        }
        let mut previous = before_collector.collect(previous_calls).await.into_iter();
        let before_scaled = decode_call::<abi::AToken::scaledBalanceOfCall>(previous.next())?;
        let before_index = if matches!(operation, "aave.supply" | "aave.borrow") {
            Some(decode_call::<abi::AToken::getPreviousIndexCall>(
                previous.next(),
            )?)
        } else {
            None
        };
        let mut extra = vec![ReadCall::new(
            provider,
            &abi::DataProvider::getUserReserveDataCall { asset, user: owner },
        )];
        if operation == "aave.supply" {
            extra.push(ReadCall::new(
                receipt_token,
                &abi::AToken::getPreviousIndexCall { user: owner },
            ));
        }
        let (inclusion, extra) = self
            .observe_anchor_facts_with_extra(
                &ctx,
                receipt.block_number,
                false,
                &mut inclusion_collector,
                extra,
                None,
            )
            .await?;
        let mut extra = extra.into_iter();
        let user = decode_call::<abi::DataProvider::getUserReserveDataCall>(extra.next())?;
        let post = reads::UserReserve {
            token,
            supplied: quantity(user.currentATokenBalance),
            stable_debt: quantity(user.currentStableDebt),
            variable_debt: quantity(user.currentVariableDebt),
            scaled_variable_debt: quantity(user.scaledVariableDebt),
            collateral_enabled: user.usageAsCollateralEnabled,
        };
        let after_index = if operation == "aave.supply" {
            Some(decode_call::<abi::AToken::getPreviousIndexCall>(
                extra.next(),
            )?)
        } else {
            None
        };
        let mut observation = AaveObservation {
            receipt: receipt.clone(),
            post,
            context: None,
            historical: None,
            inclusion: None,
            before_scaled: None,
            before_index: None,
            after_index: None,
            observed_at: now(),
        };
        observation.context = Some(ctx);
        observation.historical = Some(historical);
        observation.inclusion = Some(inclusion);
        observation.before_scaled = Some(before_scaled);
        observation.before_index = before_index;
        observation.after_index = after_index;
        Ok(observation)
    }
}

impl AaveObservation {
    fn horizon_math(&self) -> bool {
        self.context.as_ref().is_some_and(|ctx| ctx.horizon)
    }
    fn supply_revision(&self) -> U256 {
        U256::from(if self.horizon_math() { 3 } else { 5 })
    }
    fn position_backing(&self, scaled: U256, index: U256, kind: &PositionKind) -> Result<U256> {
        if self.horizon_math() {
            Ok(add(mul(scaled, index)?, pow10(27)? / U256::from(2))? / pow10(27)?)
        } else {
            anchors::backing(scaled, index, kind)
        }
    }
    fn scaled_change(
        &self,
        amount: U256,
        index: U256,
        kind: &PositionKind,
        mint: bool,
    ) -> Result<U256> {
        require(
            index >= pow10(27)?,
            "AAVE_INDEX_INVALID",
            "Normalized index must be at least one ray.",
        )?;
        let rounding = if self.horizon_math() {
            index / U256::from(2)
        } else if (*kind == PositionKind::VariableDebt) == mint {
            index - U256::from(1)
        } else {
            U256::ZERO
        };
        Ok(add(mul(amount, pow10(27)?)?, rounding)? / index)
    }
    fn accrued_before(
        &self,
        prior: &PositionAnchor,
        _token: &reads::Token,
        _owner: Address,
        _receipt_token: Address,
        index: U256,
        _block: u64,
    ) -> Result<U256> {
        self.context.as_ref().expect("context").accrued_before(
            prior,
            self.historical.as_ref().expect("historical facts"),
            self.before_scaled.expect("previous scaled balance"),
            index,
        )
    }
    fn make_anchor(
        &self,
        _token: &reads::Token,
        _owner: Address,
        _receipt_token: Address,
        _kind: PositionKind,
        _block: u64,
        deferred_rounding: U256,
    ) -> Result<PositionAnchor> {
        self.context.as_ref().expect("context").anchor(
            self.inclusion.as_ref().expect("inclusion facts"),
            false,
            deferred_rounding,
            U256::ZERO,
            self.observed_at.clone(),
        )
    }
    fn make_anchor_with_costs(
        &self,
        _token: &reads::Token,
        _owner: Address,
        _receipt_token: Address,
        _kind: PositionKind,
        _block: u64,
        deferred: anchors::DeferredExpenses,
    ) -> Result<PositionAnchor> {
        self.context.as_ref().expect("context").anchor(
            self.inclusion.as_ref().expect("inclusion facts"),
            false,
            deferred.rounding,
            deferred.interest,
            self.observed_at.clone(),
        )
    }
    pub(crate) fn evaluate_success(
        &self,
        operation: &str,
        b: &Binding,
        effects: &mut Vec<FinancialEffect>,
        details: &mut Value,
    ) -> Result<()> {
        use super::{
            debt_location, events, exact_one, origin, revision_five_backing, settle_debt_costs,
            supply_backing, supply_location, transfers,
        };
        use crate::abi::{AToken, Pool};
        let owner = address(&b.transaction.sender)?;
        let qty = u256(&b.units)?;
        let r = &self.receipt;
        if let Some(inclusion) = &self.inclusion {
            require(
                inclusion.block == r.block_number
                    && inclusion.block_hash == format!("{:#x}", r.block_hash),
                "AAVE_INCLUSION_CHANGED",
                "Inclusion anchor block does not match the canonical receipt.",
            )?;
        }
        let receipt = &r.receipt;
        let token_addr = address(&b.token.reference).unwrap_or(Address::ZERO);
        let cash = AccountingLocation::Cash {
            resource: b.resource.clone(),
        };
        let pool = address(b.pool.as_deref().expect("pool"))?;
        let token = reads::Token {
            reference: b.token.reference.clone(),
            decimals: b.token.decimals,
            symbol: String::new(),
        };
        let post = &self.post;
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
                            e.reserve == token_addr && e.user == owner && e.onBehalfOf == owner
                        })
                        .collect(),
                )?;
                require(
                    supply.amount == qty && transfers(receipt, token_addr, owner, a_token)? == qty,
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
                    .before_scaled
                    .expect("collected previous scaled balance");
                let after = self.inclusion.as_ref().expect("collected inclusion").scaled;
                let previous = self.before_index.expect("collected previous index");
                let post_index = self.after_index.expect("collected current index");
                let income = self.inclusion.as_ref().expect("collected inclusion").index;
                require(
                    self.inclusion
                        .as_ref()
                        .expect("collected inclusion")
                        .revision
                        .expect("collected revision")
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
                    anchor: Some(self.make_anchor(
                        &token,
                        owner,
                        a_token,
                        PositionKind::Supply,
                        r.block_number,
                        U256::ZERO,
                    )?),
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
                let accrued = self.accrued_before(
                    b.supply_anchor.as_ref().expect("anchor"),
                    &token,
                    owner,
                    a_token,
                    mint.index,
                    r.block_number - 1,
                )?;
                let total_yield = add(
                    add(accrued, u256(&backing.yield_after_mint)?)?,
                    u256(&backing.rounding_gain)?,
                )?;
                effects.insert(
                    effects
                        .iter()
                        .position(|effect| matches!(effect, FinancialEffect::PositionChange { .. }))
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
                            e.reserve == token_addr && e.user == owner && e.onBehalfOf == owner
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
                let index = self.inclusion.as_ref().expect("collected inclusion").index;
                let before = self
                    .before_scaled
                    .expect("collected previous scaled balance");
                let after = self.inclusion.as_ref().expect("collected inclusion").scaled;
                let ray = pow10(27)?;
                require(
                    minted.index >= ray && index >= minted.index,
                    "DEBT_INDEX_INVALID",
                    "Debt mint index must be a valid positive normalized ray.",
                )?;
                let scaled_minted =
                    self.scaled_change(qty, minted.index, &PositionKind::VariableDebt, true)?;
                require(
                    minted.index >= ray
                        && index >= minted.index
                        && scaled_minted > U256::ZERO
                        && after == add(before, scaled_minted)?,
                    "DEBT_SCALED_MISMATCH",
                    "Borrow scaled debt mint differs from exact ceiling conversion.",
                )?;
                let pre_claim =
                    self.position_backing(before, minted.index, &PositionKind::VariableDebt)?;
                let post_claim_at_mint =
                    self.position_backing(after, minted.index, &PositionKind::VariableDebt)?;
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
                let previous = self.before_index.expect("collected previous index");
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
                        && transfers(receipt, debt_token, Address::ZERO, owner)? == minted.value,
                    "DEBT_MINT_PROOF_MISMATCH",
                    "Debt mint event does not prove exact principal and index accrual.",
                )?;
                let pre_accrual = self.accrued_before(
                    prior,
                    &token,
                    owner,
                    debt_token,
                    minted.index,
                    r.block_number - 1,
                )?;
                let extra_after = sub(
                    self.position_backing(after, index, &PositionKind::VariableDebt)?,
                    post_claim_at_mint,
                )?;
                let total_accrual = add(add(pre_accrual, rounding)?, extra_after)?;
                let deferred: Quantity =
                    serde_json::from_value(prior.evidence["deferred_unpaid_rounding"].clone())
                        .map_err(|_| GoFinError::validation("Invalid proved rounding."))?;
                let deferred_interest: Quantity =
                    serde_json::from_value(prior.evidence["deferred_unpaid_interest"].clone())
                        .map_err(|_| GoFinError::validation("Invalid proved unpaid interest."))?;
                let anchor = self.make_anchor_with_costs(
                    &token,
                    owner,
                    debt_token,
                    PositionKind::VariableDebt,
                    r.block_number,
                    anchors::DeferredExpenses {
                        rounding: add(u256(&deferred)?, rounding)?,
                        interest: add(add(u256(&deferred_interest)?, pre_accrual)?, extra_after)?,
                    },
                )?;
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
                let (principal_change, accrued_change) = if opening && !self.horizon_math() {
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
                let index = self.inclusion.as_ref().expect("collected inclusion").index;
                let before = self
                    .before_scaled
                    .expect("collected previous scaled balance");
                let after = self.inclusion.as_ref().expect("collected inclusion").scaled;
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
                    after == sub(before, burned)? && quantity(after_claim) == post.variable_debt,
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
                let interest = self.accrued_before(
                    prior,
                    &token,
                    owner,
                    debt_token,
                    index,
                    r.block_number - 1,
                )?;
                let deferred: Quantity =
                    serde_json::from_value(prior.evidence["deferred_unpaid_rounding"].clone())
                        .map_err(|_| GoFinError::validation("Invalid deferred rounding."))?;
                let deferred_interest: Quantity =
                    serde_json::from_value(prior.evidence["deferred_unpaid_interest"].clone())
                        .map_err(|_| GoFinError::validation("Invalid proved unpaid interest."))?;
                let (total_cost, remaining_interest, remaining_deferred) = settle_debt_costs(
                    interest,
                    u256(&deferred_interest)?,
                    u256(&deferred)?,
                    actual,
                    cleared.min(actual),
                )?;
                let rounding_gain = cleared.saturating_sub(actual);
                let principal_leg = cleared.min(actual);
                let anchor = self.make_anchor_with_costs(
                    &token,
                    owner,
                    debt_token,
                    PositionKind::VariableDebt,
                    r.block_number,
                    anchors::DeferredExpenses {
                        rounding: remaining_deferred,
                        interest: remaining_interest,
                    },
                )?;
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
                        .filter(|e| e.reserve == token_addr && e.user == owner && e.to == recipient)
                        .collect(),
                )?;
                let qty = if b.all_amount {
                    require(
                        e.amount > U256::ZERO
                            && e.amount <= qty
                            && e.amount >= u256(b.minimum_output.as_ref().expect("minimum"))?,
                        "WITHDRAW_MAXIMUM",
                        "Full withdrawal credit lies outside the frozen finite return bound.",
                    )?;
                    e.amount
                } else {
                    qty
                };
                require(
                    e.amount == qty && transfers(receipt, token_addr, a_token, recipient)? == qty,
                    "WITHDRAW_PROOF_MISMATCH",
                    "Withdraw event or underlying cash credit differs from exact input.",
                )?;
                require(
                    r.block_number > 0,
                    "WITHDRAW_PROOF_MISMATCH",
                    "Withdraw proof requires a prior block.",
                )?;
                let before = self
                    .before_scaled
                    .expect("collected previous scaled balance");
                let after = self.inclusion.as_ref().expect("collected inclusion").scaled;
                let index = self.inclusion.as_ref().expect("collected inclusion").index;
                let ray = pow10(27)?;
                require(
                    index >= ray,
                    "WITHDRAW_INDEX_INVALID",
                    "Withdraw index must be a positive normalized ray.",
                )?;
                let burned = self.scaled_change(qty, index, &PositionKind::Supply, false)?;
                require(
                    after == sub(before, burned)?,
                    "WITHDRAW_SCALED_MISMATCH",
                    "Withdraw scaled burn does not match exact ceiling rule.",
                )?;
                let before_claim = self.position_backing(before, index, &PositionKind::Supply)?;
                let after_claim = self.position_backing(after, index, &PositionKind::Supply)?;
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
                let accrued = self.accrued_before(
                    b.supply_anchor.as_ref().expect("anchor"),
                    &token,
                    owner,
                    a_token,
                    index,
                    r.block_number - 1,
                )?;
                let anchor = self.make_anchor(
                    &token,
                    owner,
                    a_token,
                    PositionKind::Supply,
                    r.block_number,
                    U256::ZERO,
                )?;
                effects.push(FinancialEffect::Accrual {
                    resource: b.resource.clone(),
                    position: supply_position.clone(),
                    asset: b.token.clone(),
                    position_kind: PositionKind::Supply,
                    delta: SignedQuantity::positive(&quantity(add(accrued, rounding_gain)?)),
                });
                effects.push(FinancialEffect::PositionChange {
                    resource: b.resource.clone(),
                    position: supply_position,
                    asset: b.token.clone(),
                    delta: SignedQuantity::negative(&quantity(principal_leg)),
                    scaled_receipt: Some(quantity(after)),
                    anchor: Some(anchor),
                });
                let recipient_resource =
                    resource(self.context.as_ref().expect("context").chain_id, recipient);
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
                    authority: collateral(b.pool.as_deref().expect("pool"), &b.token.reference),
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
        Ok(())
    }
}
