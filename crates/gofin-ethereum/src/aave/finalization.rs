//! Pure final assembly for an Aave candidate and its exact simulation evidence.
//! This module does not access a clock, provider, signer, journal or adapter.
use super::super::*;
use crate::abi::Pool;
use gofin_evm::alloy::{primitives::Bytes, sol_types::SolCall};

pub(crate) struct AaveFinalFacts<'a> {
    pub operation: &'a str,
    pub input: &'a Value,
    pub request: &'a request::Request,
    pub token: &'a reads::Token,
    pub chain_id: u64,
    pub pool: &'a str,
    pub amount: U256,
    pub native_asset: &'a AssetId,
    pub binding: Binding,
    pub intent: FinancialIntent,
    pub warnings: Vec<String>,
    pub simulation: &'a Bytes,
    pub original: Option<&'a Binding>,
    pub observed_at: String,
    pub expires_at: String,
}

fn check_simulation(
    operation: &str,
    simulation: &Bytes,
    amount: U256,
    withdraw_all: bool,
    minimum_output: Option<&Quantity>,
) -> Result<()> {
    if operation == "aave.repay" {
        let paid = Pool::repayCall::abi_decode_returns(simulation).map_err(|_| {
            error(
                "SIMULATION_RETURN_INVALID",
                "preview",
                "Repay simulation lacks exact underlying amount.",
            )
        })?;
        require(
            paid > U256::ZERO && paid <= amount,
            "REPAY_SIMULATION_LIMIT",
            "Simulated repayment exceeds the requested preapproval debit limit or clears no debt.",
        )?;
    }
    if operation == "aave.withdraw" {
        let received = Pool::withdrawCall::abi_decode_returns(simulation).map_err(|_| {
            error(
                "SIMULATION_RETURN_INVALID",
                "preview",
                "Withdraw simulation lacks exact underlying amount.",
            )
        })?;
        let valid = if withdraw_all {
            let minimum = minimum_output
                .ok_or_else(|| GoFinError::validation("Withdraw minimum is missing."))?;
            received <= amount && received >= u256(minimum)?
        } else {
            received == amount
        };
        require(
            valid,
            "WITHDRAW_SIMULATION_LIMIT",
            "Simulated withdrawal differs from the requested exact amount or preapproval withdrawal range.",
        )?;
    }
    Ok(())
}

pub(crate) fn finalize_aave(mut facts: AaveFinalFacts<'_>) -> Result<PreparedOperation> {
    if !matches!(
        facts.operation,
        "aave.supply" | "aave.borrow" | "aave.repay" | "aave.withdraw" | "aave.collateral"
    ) {
        return Err(GoFinError::validation(
            "Unsupported Aave finalization operation.",
        ));
    }
    facts.warnings.push("Financial bounds and protocol feasibility are assessed before approval only. GoFin checks execution requirements after approval; market conditions may change before inclusion.".into());
    if facts.request.amount.as_deref() == Some("all") {
        facts.warnings.push("The full-position Pool call uses the protocol maximum sentinel. Its finite amount limit is checked in preapproval observations and simulation, not enforced by calldata; the settled amount can change before inclusion.".into());
    }
    let all = facts.request.amount.as_deref() == Some("all");
    let repay_all = facts.operation == "aave.repay" && all;
    let withdraw_all = facts.operation == "aave.withdraw" && all;
    check_simulation(
        facts.operation,
        facts.simulation,
        facts.amount,
        withdraw_all,
        facts.binding.minimum_output.as_ref(),
    )?;
    if let Some(old) = facts.original {
        require(
            old.scaled_before == facts.binding.scaled_before
                && old.allowance_before == facts.binding.allowance_before
                && old.collateral_before == facts.binding.collateral_before,
            "STATE_CHANGED",
            "Bound scaled principal or authority changed since approval.",
        )?;
        facts.binding.supplied_before = old.supplied_before.clone();
        facts.binding.debt_before = old.debt_before.clone();
        require(
            &facts.binding == old,
            "BINDING_CHANGED",
            "Financial binding changed since approval.",
        )?;
    }
    let resource = &facts.binding.resource;
    let principal = &facts.binding.token;
    super::write_lifecycle::evaluate_accounting(
        &super::write_lifecycle::AccountingFacts {
            operation: facts.operation,
            pool: facts.pool,
            token_reference: &facts.token.reference,
            chain_id: facts.chain_id,
            resource,
            asset: principal,
            binding: &facts.binding,
            amount: facts.amount,
            withdraw_all,
            repay_all,
        },
        &mut facts.intent,
    )?;
    let gas = facts.binding.transaction.maximum_cost()?;
    facts.intent.maximum_costs.push(CostBound {
        location: AccountingLocation::Cash {
            resource: resource.clone(),
        },
        amount: amount(facts.native_asset, gas),
        treatment: CostTreatment::AdditionalDebit,
    });
    // USD is intentionally not fetched for preparation or execution. Protocol
    // risk prices are separate mandatory observations in the initial capture.
    let mut checks = vec![Check {
        name: "chain, deployment, balance and protocol prerequisites".into(), status: CheckStatus::Passed,
        detail: "Live reads validated exact wallet, deployment, token, route and allowance prerequisites.".into(),
    }];
    if facts.binding.permit_template.is_some() {
        checks.push(Check { name: "materialized permit transaction simulation".into(), status: CheckStatus::Unchecked, detail: "The unsigned permit template is not simulated as if it were signed. After authorization, GoFin signs the typed permit, materializes the frozen transaction and simulates that exact transaction before submission.".into() });
    } else {
        checks.push(Check {
            name: "exact transaction simulation".into(),
            status: CheckStatus::Passed,
            detail: "The exact frozen transaction was simulated during preparation.".into(),
        });
    }
    let quantity = quantity(facts.amount).to_decimal(facts.token.decimals);
    let label = crate::amm_permissions::safe_display_label(Some(&facts.token.symbol));
    let recipient = if facts.operation == "aave.withdraw" {
        format!(
            "; recipient {}",
            facts.request.recipient.as_deref().unwrap_or("")
        )
    } else {
        String::new()
    };
    let risk = facts
        .request
        .min_health_factor
        .as_deref()
        .map(|value| format!("; preapproval minimum health factor {value}"))
        .unwrap_or_default();
    let debt_limit = facts
        .request
        .max_debt_usd
        .as_deref()
        .map(|value| format!("; preapproval maximum outstanding debt ${value}"))
        .unwrap_or_default();
    let ratio = facts
        .request
        .max_debt_to_collateral_bps
        .map(|value| format!("; preapproval maximum debt/collateral {value} bps"))
        .unwrap_or_default();
    let action = if facts.operation == "aave.collateral" {
        format!(
            "{} {} collateral",
            if facts.request.enabled == Some(true) {
                "Enable"
            } else {
                "Disable"
            },
            label
        )
    } else {
        format!(
            "{} {} {}",
            facts
                .operation
                .strip_prefix("aave.")
                .unwrap_or(facts.operation),
            if all {
                format!("all, preapproval assessed cap {quantity}")
            } else {
                quantity
            },
            label
        )
    };
    let summary = format!(
        "{action} on Aave V3, chain {}; maximum fee {} ETH{recipient}{risk}{debt_limit}{ratio}. Supplied assets and borrowed debt remain separate from wallet cash.",
        facts.chain_id,
        facts.request.max_fee_eth.as_deref().unwrap_or("")
    );
    Ok(PreparedOperation {
        operation: facts.operation.into(),
        input: facts.input.clone(),
        binding: serde_json::to_value(facts.binding)
            .map_err(|_| GoFinError::validation("Aave binding cannot be serialized."))?,
        summary,
        checks,
        warnings: facts.warnings,
        observed_at: facts.observed_at,
        expires_at: facts.expires_at,
        valuation: None,
        intent: facts.intent,
        operation_version: 2,
        action_deadline: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn encoded(value: u64) -> Bytes {
        Bytes::copy_from_slice(&U256::from(value).to_be_bytes::<32>())
    }
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct CountPrices(Arc<AtomicUsize>);
    #[async_trait]
    impl WalletValuation for CountPrices {
        async fn quote_usd(&self, _: &AssetId, _: Quantity) -> Result<(Quantity, String)> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(GoFinError::validation("unexpected optional USD request"))
        }
    }

    struct ReorgRpc {
        inner: Arc<crate::tests::Fixture>,
        reorg: Arc<AtomicBool>,
        trigger_on_estimate: bool,
    }
    #[async_trait]
    impl gofin_evm::EvmRpc for ReorgRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            let mut result = self.inner.request(method, params.clone()).await?;
            if method == "eth_estimateGas" && self.trigger_on_estimate {
                self.reorg.store(true, Ordering::SeqCst);
            }
            if method == "eth_getBlockByNumber"
                && params[0].as_str().is_some_and(|tag| tag.starts_with("0x"))
                && self.reorg.load(Ordering::SeqCst)
            {
                result["hash"] = json!(format!("0x{}", "ab".repeat(32)));
            }
            Ok(result)
        }
    }

    struct FeeRpc {
        inner: Arc<crate::tests::Fixture>,
        changed: Arc<AtomicBool>,
    }
    #[async_trait]
    impl gofin_evm::EvmRpc for FeeRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            let mut result = self.inner.request(method, params.clone()).await?;
            if method == "eth_getBlockByNumber"
                && params[0] == "latest"
                && self.changed.load(Ordering::SeqCst)
            {
                result["baseFeePerGas"] = json!("0xffffffffffffffff");
            }
            Ok(result)
        }
    }

    struct ReorgMcp {
        inner: crate::tests::SyntheticAaveMcp,
        reorg: Arc<AtomicBool>,
        trigger: bool,
    }
    #[async_trait]
    impl crate::aave_mcp::AaveMcpTransport for ReorgMcp {
        async fn post(
            &self,
            request: crate::aave_mcp::McpHttpRequest,
        ) -> std::result::Result<crate::aave_mcp::McpHttpResponse, crate::aave_mcp::AaveMcpError>
        {
            let is_call = serde_json::from_slice::<Value>(&request.body)
                .ok()
                .is_some_and(|request| request["method"] == "tools/call");
            let response = self.inner.post(request).await?;
            if self.trigger && is_call {
                self.reorg.store(true, Ordering::SeqCst);
            }
            Ok(response)
        }
    }

    fn supply_input() -> Value {
        let mut input = crate::tests::input();
        input["token"] = json!(lower_address(crate::tests::addr(101)));
        input["amount"] = json!("1");
        input
    }

    #[tokio::test]
    async fn preparation_skips_usd_quotes_and_execution_compatibility_reuses_calldata() {
        let rpc = Arc::new(crate::tests::Fixture::new());
        let quotes = Arc::new(AtomicUsize::new(0));
        let mcp_calls = Arc::new(AtomicUsize::new(0));
        let adapter = crate::tests::adapter(rpc)
            .with_wallet_valuation(Arc::new(CountPrices(quotes.clone())))
            .with_aave_mcp(crate::aave_mcp::AaveMcpClient::with_transport(Arc::new(
                crate::tests::SyntheticAaveMcp {
                    calls: mcp_calls.clone(),
                },
            )));
        let prepared = adapter
            .prepare("aave.supply", &supply_input())
            .await
            .unwrap();
        assert!(prepared.valuation.is_none());
        assert!(
            !prepared.intent.maximum_costs.is_empty(),
            "native fee remains bounded"
        );
        assert_eq!(mcp_calls.load(Ordering::SeqCst), 1);
        let refreshed = adapter.revalidate(&prepared).await.unwrap();
        assert_eq!(refreshed.binding, prepared.binding);
        assert_eq!(refreshed.intent, prepared.intent);
        assert!(refreshed.valuation.is_none());
        assert_eq!(quotes.load(Ordering::SeqCst), 0);
        assert_eq!(
            mcp_calls.load(Ordering::SeqCst),
            1,
            "execution must reuse the approved calldata"
        );
    }

    struct TestJournal;
    impl SubmissionJournal for TestJournal {
        fn assert_authorized(&self) -> Result<()> {
            Ok(())
        }
        fn persist_submission(&self, _: &Submission) -> Result<()> {
            panic!("execution requirement failure must precede admission")
        }
    }

    #[tokio::test]
    async fn execution_checks_frozen_nonce_and_fee_without_new_mcp_call() {
        let rpc = Arc::new(crate::tests::Fixture::new());
        let fee_changed = Arc::new(AtomicBool::new(false));
        let mcp_calls = Arc::new(AtomicUsize::new(0));
        let mut adapter = crate::tests::adapter(rpc.clone());
        adapter.runtime = Arc::new(gofin_evm::EvmRuntime::new(
            Arc::new(FeeRpc {
                inner: rpc.clone(),
                changed: fee_changed.clone(),
            }),
            1,
            None,
        ));
        adapter = adapter.with_aave_mcp(crate::aave_mcp::AaveMcpClient::with_transport(Arc::new(
            crate::tests::SyntheticAaveMcp {
                calls: mcp_calls.clone(),
            },
        )));
        let prepared = adapter
            .prepare("aave.supply", &supply_input())
            .await
            .unwrap();
        rpc.nonce.store(1, Ordering::SeqCst);
        assert_eq!(
            adapter
                .submit(&prepared, Arc::new(TestJournal))
                .await
                .unwrap_err()
                .code,
            "NONCE_CHANGED"
        );
        rpc.nonce.store(0, Ordering::SeqCst);
        fee_changed.store(true, Ordering::SeqCst);
        assert_eq!(
            adapter
                .submit(&prepared, Arc::new(TestJournal))
                .await
                .unwrap_err()
                .code,
            "FEE_BOUND_CHANGED"
        );
        assert_eq!(mcp_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn reorg_during_mcp_or_simulation_rejects_candidate() {
        for trigger_on_mcp in [true, false] {
            let rpc = Arc::new(crate::tests::Fixture::new());
            let reorg = Arc::new(AtomicBool::new(false));
            let mcp_calls = Arc::new(AtomicUsize::new(0));
            let mut adapter = crate::tests::adapter(rpc.clone());
            adapter.runtime = Arc::new(gofin_evm::EvmRuntime::new(
                Arc::new(ReorgRpc {
                    inner: rpc,
                    reorg: reorg.clone(),
                    trigger_on_estimate: !trigger_on_mcp,
                }),
                1,
                None,
            ));
            adapter = adapter.with_aave_mcp(crate::aave_mcp::AaveMcpClient::with_transport(
                Arc::new(ReorgMcp {
                    inner: crate::tests::SyntheticAaveMcp {
                        calls: mcp_calls.clone(),
                    },
                    reorg,
                    trigger: trigger_on_mcp,
                }),
            ));
            let failure = adapter
                .prepare("aave.supply", &supply_input())
                .await
                .unwrap_err();
            assert_eq!(
                failure.code, "AAVE_READ_CHANGED",
                "MCP trigger: {trigger_on_mcp}"
            );
            assert_eq!(mcp_calls.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn exact_simulation_amounts_enforce_repayment_and_withdrawal_bounds() {
        assert!(check_simulation("aave.repay", &encoded(10), U256::from(10), false, None).is_ok());
        for value in [0, 11] {
            assert_eq!(
                check_simulation("aave.repay", &encoded(value), U256::from(10), false, None)
                    .unwrap_err()
                    .code,
                "REPAY_SIMULATION_LIMIT"
            );
        }
        assert_eq!(
            check_simulation("aave.repay", &Bytes::new(), U256::from(10), false, None)
                .unwrap_err()
                .code,
            "SIMULATION_RETURN_INVALID"
        );
        assert!(
            check_simulation("aave.withdraw", &encoded(10), U256::from(10), false, None).is_ok()
        );
        assert_eq!(
            check_simulation("aave.withdraw", &encoded(9), U256::from(10), false, None)
                .unwrap_err()
                .code,
            "WITHDRAW_SIMULATION_LIMIT"
        );
        let minimum = Quantity::from_u64(8);
        for value in [8, 10] {
            assert!(
                check_simulation(
                    "aave.withdraw",
                    &encoded(value),
                    U256::from(10),
                    true,
                    Some(&minimum)
                )
                .is_ok()
            );
        }
        for value in [7, 11] {
            assert_eq!(
                check_simulation(
                    "aave.withdraw",
                    &encoded(value),
                    U256::from(10),
                    true,
                    Some(&minimum)
                )
                .unwrap_err()
                .code,
                "WITHDRAW_SIMULATION_LIMIT"
            );
        }
    }
}
