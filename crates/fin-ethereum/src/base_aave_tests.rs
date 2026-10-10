//! Synthetic Base profile evidence only: no claim about deployed revisions.
use super::*;

pub(super) struct BaseAaveRpc {
    pub(super) inner: Arc<Fixture>,
}
#[async_trait]
impl EvmRpc for BaseAaveRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "eth_call" {
            let to = params[0]["to"].as_str().unwrap_or_default();
            if to == "0x420000000000000000000000000000000000000f" {
                let data = params[0]["data"].as_str().unwrap();
                let selector = |sig: &str| {
                    format!(
                        "0x{}",
                        hex::encode(&fin_evm::alloy::primitives::keccak256(sig)[..4])
                    )
                };
                let value = if data.starts_with(&selector("isFjord()"))
                    || data.starts_with(&selector("isIsthmus()"))
                {
                    1
                } else if data.starts_with(&selector("isJovian()"))
                    || data.starts_with(&selector("getOperatorFee(uint256)"))
                {
                    0
                } else {
                    1000
                };
                return Ok(abi_value(U256::from(value)));
            }
            if to == "0x4200000000000000000000000000000000000015" {
                return Ok(abi_value(U256::ZERO));
            }
        }
        let mut value = self.inner.request(method, params).await?;
        if method == "eth_getBlockByNumber" {
            value["extraData"] = json!(format!("0x00{}", "00".repeat(8)));
        }
        if method == "eth_getTransactionReceipt" && value.is_object() {
            value["l1Fee"] = json!("0x3e8");
        }
        Ok(value)
    }
}

fn base_fixture() -> (Arc<Fixture>, EthereumAdapter) {
    let rpc = Arc::new(Fixture::new());
    rpc.chain.store(8453, Ordering::SeqCst);
    let config = EthereumConfig::base_aave(vec![lower_address(addr(101))]);
    let profile = config.aave.as_ref().unwrap();
    rpc.call_overrides.lock().unwrap().insert(
        AddressesProvider::getPoolCall::SELECTOR,
        abi_value(address(&profile.pool).unwrap()),
    );
    rpc.call_overrides.lock().unwrap().insert(
        AddressesProvider::getPriceOracleCall::SELECTOR,
        abi_value(address(&profile.oracle).unwrap()),
    );
    rpc.call_overrides.lock().unwrap().insert(
        Pool::getReservesListCall::SELECTOR,
        abi_value(vec![addr(101)]),
    );
    let adapter = EthereumAdapter::new(
        config,
        vec![WalletConnection {
            name: "synthetic".into(),
            address: lower_address(addr(1)),
            signing_enabled: true,
        }],
        Arc::new(EvmRuntime::new(
            Arc::new(BaseAaveRpc { inner: rpc.clone() }),
            8453,
            None,
        )),
    )
    .unwrap();
    (rpc, adapter)
}
fn base_input(operation: &str) -> Value {
    let mut input = aave_input(operation);
    input["chain_id"] = json!(8453);
    input["network"] = json!("evm");
    input
}

#[tokio::test]
async fn base_aave_reads_and_six_guarded_actions_use_exact_reviewed_profile() {
    let (rpc, adapter) = base_fixture();
    rpc.states.lock().unwrap().insert(
        20,
        AtBlock {
            scaled_supply: U256::from(10_000_000),
            scaled_debt: U256::from(2_000_000),
            income: ray(),
            debt_index: ray(),
            supply_previous: ray(),
            debt_previous: ray(),
            collateral: true,
            ..Default::default()
        },
    );
    let read = json!({"wallet":"synthetic","network":"evm","chain_id":8453,"tokens":[lower_address(addr(101))]});
    for operation in ["aave.reserves", "aave.positions"] {
        let result = adapter.read(operation, &read).await.unwrap();
        assert_eq!(result["chain_id"], 8453);
        assert_eq!(result["block"], 20);
        assert!(
            result["resource"]
                .as_str()
                .unwrap()
                .starts_with("ethereum:8453:")
        );
    }
    for (operation, enabled) in [
        ("aave.supply", false),
        ("aave.collateral", false),
        ("aave.collateral", true),
        ("aave.borrow", false),
        ("aave.repay", false),
        ("aave.withdraw", false),
    ] {
        let mut input = base_input(operation);
        if operation == "aave.collateral" {
            input["enabled"] = json!(enabled);
        }
        let preview = adapter
            .prepare(operation, &input)
            .await
            .unwrap_or_else(|error| panic!("{operation}: {error:?}"));
        let binding: Binding = serde_json::from_value(preview.binding).unwrap();
        assert_eq!(binding.transaction.chain_id, 8453);
        assert_eq!(
            binding.transaction.to,
            adapter.config.aave.as_ref().unwrap().pool
        );
        assert!(binding.transaction.rollup_fee.is_some());
        assert_eq!(binding.token.network, "ethereum:8453");
        assert_eq!(
            binding.pool.as_deref(),
            Some(adapter.config.aave.as_ref().unwrap().pool.as_str())
        );
    }
    assert!(adapter.config.uniswap.is_none());
    let definitions = adapter.definitions();
    assert!(!definitions.iter().any(|d| d.name.starts_with("uniswap.")));
    for definition in definitions.iter().filter(|d| d.name.starts_with("aave.")) {
        assert_eq!(
            definition.semantics["deployment_compatibility"]["reviewed_token_revision"],
            5
        );
    }
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "eth_sendRawTransaction")
    );
}

#[tokio::test]
async fn base_aave_unreviewed_revisions_and_provider_changes_refuse_before_dispatch() {
    let (rpc, adapter) = base_fixture();
    for revision in [4, 6] {
        rpc.selected_revision.store(revision, Ordering::SeqCst);
        for operation in ["aave.supply", "aave.withdraw", "aave.borrow", "aave.repay"] {
            let expected = if matches!(operation, "aave.supply" | "aave.withdraw") {
                "ATOKEN_REVISION_UNSUPPORTED"
            } else {
                "DEBT_REVISION_UNSUPPORTED"
            };
            assert_eq!(
                adapter
                    .prepare(operation, &base_input(operation))
                    .await
                    .unwrap_err()
                    .code,
                expected
            );
        }
    }
    rpc.selected_revision.store(5, Ordering::SeqCst);
    rpc.call_overrides.lock().unwrap().insert(
        AddressesProvider::getPoolCall::SELECTOR,
        abi_value(addr(300)),
    );
    assert_eq!(
        adapter
            .prepare("aave.supply", &base_input("aave.supply"))
            .await
            .unwrap_err()
            .code,
        "DEPLOYMENT_IDENTITY_MISMATCH"
    );
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "eth_sendRawTransaction")
    );
}

#[tokio::test]
async fn base_aave_supply_requires_canonical_exact_events_and_emits_base_accounting() {
    let (rpc, adapter) = base_fixture();
    rpc.states.lock().unwrap().insert(
        20,
        AtBlock {
            income: ray(),
            debt_index: ray(),
            ..Default::default()
        },
    );
    let preview = adapter
        .prepare("aave.supply", &base_input("aave.supply"))
        .await
        .unwrap();
    let binding: Binding = serde_json::from_value(preview.binding.clone()).unwrap();
    let units = U256::from(1_000_000);
    rpc.post(
        21,
        AtBlock {
            scaled_supply: units,
            income: ray(),
            debt_index: ray(),
            supply_previous: ray(),
            collateral: true,
            ..Default::default()
        },
    );
    let pool = address(&adapter.config.aave.as_ref().unwrap().pool).unwrap();
    let logs = vec![
        event(
            addr(101),
            Erc20::Transfer {
                from: addr(1),
                to: addr(110),
                value: units,
            },
            21,
        ),
        event(
            pool,
            Pool::Supply {
                reserve: addr(101),
                user: addr(1),
                onBehalfOf: addr(1),
                amount: units,
                referralCode: 0,
            },
            21,
        ),
        event(
            addr(110),
            AToken::Mint {
                caller: addr(1),
                onBehalfOf: addr(1),
                value: units,
                balanceIncrease: U256::ZERO,
                index: ray(),
            },
            21,
        ),
        event(
            addr(110),
            Erc20::Transfer {
                from: Address::ZERO,
                to: addr(1),
                value: units,
            },
            21,
        ),
    ];
    rpc.set_receipt(&binding, 21, logs.clone());
    let record = record(preview);
    let reconciled = adapter.reconcile(&record).await.unwrap();
    assert_eq!(reconciled.status, Status::Completed);
    let proof = reconciled.verified.unwrap();
    assert!(proof.effects.iter().any(|effect| matches!(effect,
        FinancialEffect::PositionChange { anchor:Some(anchor),.. } if anchor.principal.asset.network=="ethereum:8453" && anchor.principal.atoms==Quantity::from_u64(1_000_000))));
    assert_eq!(proof.submission.reference, txhash());
    let mut forged = logs;
    forged[1]["address"] = json!(lower_address(addr(300)));
    rpc.set_receipt(&binding, 21, forged);
    assert_eq!(
        adapter.reconcile(&record).await.unwrap_err().code,
        "RECEIPT_EVENT_MISMATCH"
    );
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "eth_sendRawTransaction")
    );
}

#[tokio::test]
async fn opening_borrow_accounts_complete_new_debt_and_preserves_existing_accrual() {
    let units = U256::from(1_000_000);
    for (name, before, mint_index, post_index, expected_debt, expected_accrual, rejected) in [
        (
            "opening-rounding",
            U256::ZERO,
            ray() + U256::from(1),
            ray() + U256::from(1),
            1_000_001,
            0,
            false,
        ),
        (
            "opening-post-mint",
            U256::ZERO,
            ray() + U256::from(1),
            ray() + ray() / U256::from(500_000),
            1_000_002,
            0,
            false,
        ),
        (
            "opening-over-envelope",
            U256::ZERO,
            ray() + U256::from(1),
            ray() + ray() / U256::from(100_000),
            0,
            0,
            true,
        ),
        (
            "existing-debt",
            units,
            ray() + ray() / U256::from(1_000_000),
            ray() + ray() / U256::from(1_000_000),
            1_000_000,
            1,
            false,
        ),
    ] {
        let (rpc, adapter) = base_fixture();
        let previous = if before.is_zero() {
            U256::ZERO
        } else {
            ray() + U256::from(1)
        };
        rpc.states.lock().unwrap().insert(
            20,
            AtBlock {
                scaled_debt: before,
                income: ray() + U256::from(1),
                debt_index: ray() + U256::from(1),
                debt_previous: previous,
                ..Default::default()
            },
        );
        let preview = adapter
            .prepare("aave.borrow", &base_input("aave.borrow"))
            .await
            .unwrap();
        let bound = preview
            .intent
            .expected
            .iter()
            .find_map(|constraint| {
                if let EffectConstraint::Debt { maximum_change, .. } = constraint {
                    Some(maximum_change)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(
            bound,
            &Quantity::from_u64(if before.is_zero() {
                1_000_004
            } else {
                1_000_000
            }),
            "{name}"
        );
        let binding: Binding = serde_json::from_value(preview.binding.clone()).unwrap();
        let scaled_mint = (units * ray() + mint_index - U256::from(1)) / mint_index;
        let after = before + scaled_mint;
        let before_at_mint =
            anchors::backing(before, mint_index, &PositionKind::VariableDebt).unwrap();
        let prior_claim = anchors::backing(before, previous, &PositionKind::VariableDebt).unwrap();
        let minted =
            anchors::backing(after, mint_index, &PositionKind::VariableDebt).unwrap() - prior_claim;
        rpc.post(
            21,
            AtBlock {
                scaled_debt: after,
                income: ray() + U256::from(1),
                debt_index: post_index,
                debt_previous: mint_index,
                ..Default::default()
            },
        );
        let pool = address(&adapter.config.aave.as_ref().unwrap().pool).unwrap();
        rpc.set_receipt(
            &binding,
            21,
            vec![
                event(
                    pool,
                    Pool::Borrow {
                        reserve: addr(101),
                        user: addr(1),
                        onBehalfOf: addr(1),
                        amount: units,
                        interestRateMode: 2,
                        borrowRate: U256::from(1),
                        referralCode: 0,
                    },
                    21,
                ),
                event(
                    addr(101),
                    Erc20::Transfer {
                        from: addr(110),
                        to: addr(1),
                        value: units,
                    },
                    21,
                ),
                event(
                    addr(120),
                    AToken::Mint {
                        caller: addr(1),
                        onBehalfOf: addr(1),
                        value: minted,
                        balanceIncrease: before_at_mint - prior_claim,
                        index: mint_index,
                    },
                    21,
                ),
                event(
                    addr(120),
                    Erc20::Transfer {
                        from: Address::ZERO,
                        to: addr(1),
                        value: minted,
                    },
                    21,
                ),
            ],
        );
        let record = record(preview);
        if rejected {
            assert_eq!(
                adapter.reconcile(&record).await.unwrap_err().code,
                "DEBT_ROUNDING_LIMIT",
                "{name}"
            );
            continue;
        }
        let proof = adapter.reconcile(&record).await.unwrap().verified.unwrap();
        let debt = proof
            .effects
            .iter()
            .find_map(|effect| {
                if let FinancialEffect::DebtChange { delta, anchor, .. } = effect {
                    Some((delta, anchor.as_ref().unwrap()))
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(
            debt.0.magnitude(),
            Quantity::from_u64(expected_debt),
            "{name}"
        );
        assert!(proof.effects.iter().any(|effect| matches!(effect,FinancialEffect::Accrual { position_kind:PositionKind::VariableDebt,delta,.. } if delta.magnitude()==Quantity::from_u64(expected_accrual))),"{name}");
        assert_eq!(
            debt.1.principal.atoms,
            quantity(anchors::backing(after, post_index, &PositionKind::VariableDebt).unwrap()),
            "{name}"
        );
        assert!(proof.effects.iter().any(|effect| matches!(effect,FinancialEffect::UsableCredit { amount,.. } if amount.atoms==quantity(units))),"cash principal remains exact: {name}");
        assert_eq!(
            debt.1.evidence["deferred_unpaid_rounding"],
            json!("1"),
            "{name}"
        );
        if name == "opening-post-mint" {
            assert_eq!(debt.1.evidence["deferred_unpaid_interest"], json!("1"));
        }
    }
}
