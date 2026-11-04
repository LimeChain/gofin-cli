use super::*;
use abi::*;
use gofin_evm::{block_tag, quantity, rpc_u64, u256};
pub(crate) struct DeferredExpenses {
    pub rounding: U256,
    pub interest: U256,
}

pub(crate) fn backing(scaled: U256, index: U256, kind: &PositionKind) -> Result<U256> {
    let product = mul(scaled, index)?;
    let ray = pow10(27)?;
    if *kind == PositionKind::VariableDebt {
        Ok(if product.is_zero() {
            U256::ZERO
        } else {
            add(product, ray - U256::from(1))? / ray
        })
    } else {
        Ok(product / ray)
    }
}
impl EthereumAdapter {
    /// An empty historical position can be observed without applying a token's
    /// revision-specific rounding math. This is a baseline proof, never a write
    /// eligibility claim; selected protocol writes check the deployed revision.
    pub(crate) async fn snapshot_anchor(
        &self,
        token: &reads::Token,
        owner: Address,
        receipt_token: Address,
        kind: PositionKind,
        block: u64,
    ) -> Result<PositionAnchor> {
        let scaled = self
            .runtime
            .call(
                receipt_token,
                &AToken::scaledBalanceOfCall { user: owner },
                Some(block),
            )
            .await?;
        let observed = self
            .runtime
            .call(receipt_token, &Erc20::balanceOfCall { owner }, Some(block))
            .await?;
        if scaled.is_zero() && observed.is_zero() {
            self.zero_anchor(token, owner, receipt_token, kind, block)
                .await
        } else {
            self.make_anchor(token, owner, receipt_token, kind, block, U256::ZERO)
                .await
        }
    }
    async fn zero_anchor(
        &self,
        token: &reads::Token,
        owner: Address,
        receipt_token: Address,
        kind: PositionKind,
        block: u64,
    ) -> Result<PositionAnchor> {
        require(
            matches!(kind, PositionKind::Supply | PositionKind::VariableDebt),
            "ANCHOR_KIND_UNSUPPORTED",
            "Only scaled supply and variable debt have empty backing anchors.",
        )?;
        let scaled = self
            .runtime
            .call(
                receipt_token,
                &AToken::scaledBalanceOfCall { user: owner },
                Some(block),
            )
            .await?;
        let observed = self
            .runtime
            .call(receipt_token, &Erc20::balanceOfCall { owner }, Some(block))
            .await?;
        require(
            scaled.is_zero() && observed.is_zero(),
            "ANCHOR_BACKING_MISMATCH",
            "An empty backing anchor requires exact zero token and scaled balances at its historical block.",
        )?;
        let index = if kind == PositionKind::Supply {
            self.runtime
                .call(
                    address(&self.aave()?.pool)?,
                    &Pool::getReserveNormalizedIncomeCall {
                        asset: address(&token.reference)?,
                    },
                    Some(block),
                )
                .await?
        } else {
            self.runtime
                .call(
                    address(&self.aave()?.pool)?,
                    &Pool::getReserveNormalizedVariableDebtCall {
                        asset: address(&token.reference)?,
                    },
                    Some(block),
                )
                .await?
        };
        require(
            index >= pow10(27)? && index < (U256::from(1) << 128),
            "ANCHOR_INDEX_INVALID",
            "Observed normalized index exceeds reviewed bounds.",
        )?;
        let header = self
            .runtime
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([block_tag(Some(block)), false]),
            )
            .await?;
        gofin_evm::hash(header["hash"].as_str().unwrap_or(""))?;
        require(
            rpc_u64(&header["number"])? == block,
            "ANCHOR_BLOCK_INVALID",
            "Backing observation block identity differs.",
        )?;
        Ok(PositionAnchor {
            resource: resource(self.config.chain_id, owner),
            position: position(
                &self.aave()?.pool,
                &token.reference,
                if kind == PositionKind::Supply {
                    "supply"
                } else {
                    "variable-debt"
                },
            ),
            position_kind: kind,
            principal: amount(&token.asset(self), U256::ZERO),
            scaled_receipt: Some(Quantity::zero()),
            evidence_version: 1,
            evidence: json!({"method":"aave-zero-observed-backing","chain_id":self.config.chain_id,"pool":self.aave()?.pool,"underlying":token.reference,"receipt_token":lower_address(receipt_token),"block":block,"block_hash":header["hash"],"normalized_index":quantity(index),"scaled_balance":Quantity::zero(),"deferred_unpaid_rounding":Quantity::zero(),"deferred_unpaid_interest":Quantity::zero()}),
            observed_at: now(),
        })
    }
    pub(crate) async fn make_anchor(
        &self,
        token: &reads::Token,
        owner: Address,
        receipt_token: Address,
        kind: PositionKind,
        block: u64,
        deferred_rounding: U256,
    ) -> Result<PositionAnchor> {
        self.make_anchor_with_costs(
            token,
            owner,
            receipt_token,
            kind,
            block,
            DeferredExpenses {
                rounding: deferred_rounding,
                interest: U256::ZERO,
            },
        )
        .await
    }
    pub(crate) async fn make_anchor_with_costs(
        &self,
        token: &reads::Token,
        owner: Address,
        receipt_token: Address,
        kind: PositionKind,
        block: u64,
        deferred: DeferredExpenses,
    ) -> Result<PositionAnchor> {
        let DeferredExpenses {
            rounding: deferred_rounding,
            interest: deferred_interest,
        } = deferred;
        require(
            kind != PositionKind::StableDebt,
            "STABLE_DEBT_WRITE_UNSUPPORTED",
            "Stable debt is fully observed but this profile executes variable debt only.",
        )?;
        let revision = if kind == PositionKind::Supply {
            self.runtime
                .call(receipt_token, &AToken::ATOKEN_REVISIONCall {}, Some(block))
                .await?
        } else {
            self.runtime
                .call(
                    receipt_token,
                    &AToken::DEBT_TOKEN_REVISIONCall {},
                    Some(block),
                )
                .await?
        };
        require(
            revision
                == if kind == PositionKind::Supply {
                    self.supply_revision()
                } else {
                    self.debt_revision()
                },
            "TOKEN_REVISION_UNSUPPORTED",
            "Exact scaled backing anchors require the selected reviewed token revision.",
        )?;
        let scaled = self
            .runtime
            .call(
                receipt_token,
                &AToken::scaledBalanceOfCall { user: owner },
                Some(block),
            )
            .await?;
        let index = if kind == PositionKind::Supply {
            self.runtime
                .call(
                    address(&self.aave()?.pool)?,
                    &Pool::getReserveNormalizedIncomeCall {
                        asset: address(&token.reference)?,
                    },
                    Some(block),
                )
                .await?
        } else {
            self.runtime
                .call(
                    address(&self.aave()?.pool)?,
                    &Pool::getReserveNormalizedVariableDebtCall {
                        asset: address(&token.reference)?,
                    },
                    Some(block),
                )
                .await?
        };
        require(
            index >= pow10(27)?
                && index < (U256::from(1) << 128)
                && scaled < (U256::from(1) << 120),
            "ANCHOR_INDEX_INVALID",
            "Scaled backing or normalized index exceeds reviewed bounds.",
        )?;
        let claim = self.position_backing(scaled, index, &kind)?;
        let observed = self
            .runtime
            .call(receipt_token, &Erc20::balanceOfCall { owner }, Some(block))
            .await?;
        require(
            claim == observed,
            "ANCHOR_BACKING_MISMATCH",
            "Exact scaled backing differs from token balance.",
        )?;
        let header = self
            .runtime
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([block_tag(Some(block)), false]),
            )
            .await?;
        gofin_evm::hash(header["hash"].as_str().unwrap_or(""))?;
        require(
            rpc_u64(&header["number"])? == block,
            "ANCHOR_BLOCK_INVALID",
            "Backing observation block identity differs.",
        )?;
        Ok(PositionAnchor {
            resource: resource(self.config.chain_id, owner),
            position: position(
                &self.aave()?.pool,
                &token.reference,
                if kind == PositionKind::Supply {
                    "supply"
                } else {
                    "variable-debt"
                },
            ),
            position_kind: kind,
            principal: amount(&token.asset(self), claim),
            scaled_receipt: Some(quantity(scaled)),
            evidence_version: 1,
            evidence: json!({"method":self.backing_method(),"chain_id":self.config.chain_id,"pool":self.aave()?.pool,"underlying":token.reference,"receipt_token":lower_address(receipt_token),"block":block,"block_hash":header["hash"],"normalized_index":quantity(index),"scaled_balance":quantity(scaled),"deferred_unpaid_rounding":quantity(deferred_rounding),"deferred_unpaid_interest":quantity(deferred_interest)}),
            observed_at: now(),
        })
    }
    fn validate_anchor_identity(
        &self,
        anchor: &PositionAnchor,
        token: &reads::Token,
        owner: Address,
        receipt_token: Address,
    ) -> Result<()> {
        let kind = &anchor.position_kind;
        require(
            anchor.evidence_version == 1
                && anchor.resource == resource(self.config.chain_id, owner)
                && anchor.principal.asset == token.asset(self)
                && (anchor.evidence["method"] == json!(self.backing_method())
                    || anchor.evidence["method"] == json!("aave-zero-observed-backing"))
                && anchor.evidence["chain_id"] == json!(self.config.chain_id)
                && anchor.evidence["pool"] == json!(self.aave()?.pool)
                && anchor.evidence["receipt_token"] == json!(lower_address(receipt_token))
                && anchor.position
                    == position(
                        &self.aave()?.pool,
                        &token.reference,
                        if *kind == PositionKind::Supply {
                            "supply"
                        } else {
                            "variable-debt"
                        },
                    ),
            "ANCHOR_IDENTITY_INVALID",
            "Trusted anchor does not bind the same immutable position representation.",
        )?;
        Ok(())
    }
    pub(crate) async fn validate_anchor(
        &self,
        anchor: &PositionAnchor,
        token: &reads::Token,
        owner: Address,
        receipt_token: Address,
    ) -> Result<()> {
        self.validate_anchor_identity(anchor, token, owner, receipt_token)?;
        let kind = &anchor.position_kind;
        let block = anchor.evidence["block"].as_u64().ok_or_else(|| {
            error(
                "ANCHOR_BLOCK_INVALID",
                "prepare",
                "Anchor inclusion block is missing.",
            )
        })?;
        let deferred: Quantity = serde_json::from_value(
            anchor.evidence["deferred_unpaid_rounding"].clone(),
        )
        .map_err(|_| {
            error(
                "ANCHOR_INVALID",
                "prepare",
                "Anchor deferred rounding is invalid.",
            )
        })?;
        let interest: Quantity = serde_json::from_value(
            anchor.evidence["deferred_unpaid_interest"].clone(),
        )
        .map_err(|_| {
            error(
                "ANCHOR_INVALID",
                "prepare",
                "Anchor unpaid interest is invalid.",
            )
        })?;
        require(
            add(u256(&interest)?, u256(&deferred)?)? <= u256(&anchor.principal.atoms)?,
            "ANCHOR_COST_INVALID",
            "Deferred debt expenses exceed exact physical backing.",
        )?;
        let observed = if anchor.evidence["method"] == json!("aave-zero-observed-backing") {
            self.zero_anchor(token, owner, receipt_token, kind.clone(), block)
                .await?
        } else {
            self.make_anchor_with_costs(
                token,
                owner,
                receipt_token,
                kind.clone(),
                block,
                DeferredExpenses {
                    rounding: u256(&deferred)?,
                    interest: u256(&interest)?,
                },
            )
            .await?
        };
        require(
            observed.principal == anchor.principal
                && observed.scaled_receipt == anchor.scaled_receipt
                && observed.evidence == anchor.evidence,
            "ANCHOR_PROOF_MISMATCH",
            "Historical anchor backing or canonical inclusion no longer matches.",
        )?;
        Ok(())
    }
}
