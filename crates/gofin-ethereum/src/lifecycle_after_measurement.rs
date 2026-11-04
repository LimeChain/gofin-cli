//! Lifecycle loopback measurements, no external calls.
// Synthetic key and signed transactions are confined to the localhost fixture.
const MEASUREMENT_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";
fn measurement_owner() -> Address {
    gofin_evm::LocalEnvelopeSigner::from_private_key(MEASUREMENT_KEY)
        .unwrap()
        .address()
}

struct MeasurementJournal {
    expected: PreparedOperation,
    saved: Mutex<Option<Submission>>,
}
impl gofin_domain::SubmissionJournal for MeasurementJournal {
    fn assert_authorized(&self) -> Result<()> {
        Ok(())
    }
    fn assert_candidate(&self, candidate: &PreparedOperation) -> Result<()> {
        if candidate == &self.expected {
            Ok(())
        } else {
            Err(GoFinError::validation("Measurement candidate changed."))
        }
    }
    fn persist_submission(&self, submission: &Submission) -> Result<()> {
        let mut saved = self.saved.lock().unwrap();
        if saved.is_some() {
            return Err(GoFinError::validation("Duplicate fixture admission."));
        }
        *saved = Some(submission.clone());
        Ok(())
    }
}
use super::*;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Counted {
    inner: gofin_evm::HttpRpc,
    logical: AtomicUsize,
}
#[async_trait]
impl EvmRpc for Counted {
    fn enforces_chain_identity(&self, expected_chain_id: u64) -> bool {
        self.inner.enforces_chain_identity(expected_chain_id)
    }
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.logical.fetch_add(1, Ordering::SeqCst);
        self.inner.request(method, params).await
    }
    async fn request_checked(
        &self,
        method: &str,
        params: Value,
        requirements: gofin_evm::RpcReadRequirements,
    ) -> Result<Value> {
        self.logical.fetch_add(1, Ordering::SeqCst);
        self.inner
            .request_checked(method, params, requirements)
            .await
    }
}
/// Count all MCP transport posts separately from EVM RPCs.
struct CountedMcp {
    inner: SyntheticAaveMcp,
    posts: Arc<AtomicUsize>,
    tool_calls: Arc<AtomicUsize>,
}
#[async_trait]
impl aave_mcp::AaveMcpTransport for CountedMcp {
    async fn post(
        &self,
        request: aave_mcp::McpHttpRequest,
    ) -> std::result::Result<aave_mcp::McpHttpResponse, aave_mcp::AaveMcpError> {
        self.posts.fetch_add(1, Ordering::SeqCst);
        if serde_json::from_slice::<Value>(&request.body)
            .ok()
            .is_some_and(|v| v["method"] == "tools/call")
        {
            self.tool_calls.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.post(request).await
    }
}
struct LocalCatalog {
    lookups: Arc<AtomicUsize>,
    remote_calls: Arc<AtomicUsize>,
}
#[async_trait]
impl TokenDiscovery for LocalCatalog {
    async fn tokens(&self, _: u64, _: &str) -> Result<TokenDiscoveryResult> {
        self.remote_calls.fetch_add(1, Ordering::SeqCst);
        Err(GoFinError::validation(
            "Catalog-first fixture cannot call a metadata service.",
        ))
    }
    fn token_metadata(&self, chain_id: u64, reference: &str) -> Option<TokenMetadataHint> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        (chain_id == 1 && reference == lower_address(addr(101))).then(|| TokenMetadataHint {
            chain_id,
            address: reference.into(),
            decimals: 6,
            symbol: "USDC".into(),
        })
    }
}
fn selected_metadata(params: &Value) -> bool {
    if params[0]["to"] != json!(lower_address(addr(101))) {
        return false;
    }
    let Ok(bytes) = gofin_evm::rpc_bytes(&params[0]["data"]) else {
        return false;
    };
    bytes.starts_with(&Erc20::decimalsCall::SELECTOR)
        || bytes.starts_with(&Erc20::symbolCall::SELECTOR)
}

/// Exact synthetic fact identity. Block-by-hash and numeric tags normalize to
/// the same canonical test block; mutable sender-bearing simulations are separate.
fn fact_key(method: &str, params: &Value) -> Option<String> {
    let (target, data, tag) = match method {
        "eth_call" if params[0].get("from").is_none() => (
            params[0]["to"].as_str()?,
            params[0]["data"].as_str()?,
            &params[1],
        ),
        "eth_getCode" => (params[0].as_str()?, "code", &params[1]),
        "eth_getStorageAt" => (params[0].as_str()?, params[1].as_str()?, &params[2]),
        _ => return None,
    };
    let block = if let Some(hash) = tag["blockHash"].as_str() {
        hash.to_ascii_lowercase()
    } else {
        let value = tag.as_str()?;
        if let Some(hex) = value.strip_prefix("0x") {
            match u64::from_str_radix(hex, 16) {
                Ok(number) => blockhash(number),
                Err(_) => value.to_owned(),
            }
        } else {
            value.to_owned()
        }
    };
    Some(format!(
        "{method}|{}|{}|{block}",
        target.to_ascii_lowercase(),
        data.to_ascii_lowercase()
    ))
}
#[test]
fn exact_fact_identity_normalizes_block_tags_and_keeps_owner_and_target_distinct() {
    let target = lower_address(addr(300));
    let first = DataProvider::getUserReserveDataCall {
        asset: addr(101),
        user: addr(1),
    }
    .abi_encode();
    let second = DataProvider::getUserReserveDataCall {
        asset: addr(101),
        user: addr(2),
    }
    .abi_encode();
    let params =
        |target: &str, data: &[u8], tag: Value| json!([{"to":target,"data":hex_bytes(data)},tag]);
    let numeric = params(&target, &first, json!("0x14"));
    let by_hash = params(
        &target,
        &first,
        json!({"blockHash":blockhash(20),"requireCanonical":true}),
    );
    assert_eq!(
        fact_key("eth_call", &numeric),
        fact_key("eth_call", &by_hash)
    );
    assert_ne!(
        fact_key("eth_call", &numeric),
        fact_key("eth_call", &params(&target, &second, json!("0x14")))
    );
    assert_ne!(
        fact_key("eth_call", &numeric),
        fact_key("eth_call", &params(&target, &first, json!("0x15")))
    );
    assert_ne!(
        fact_key("eth_call", &numeric),
        fact_key(
            "eth_call",
            &params(&lower_address(addr(301)), &first, json!("0x14"))
        )
    );
    assert!(
        fact_key(
            "eth_call",
            &json!([{"to":target,"from":lower_address(addr(1)),"data":hex_bytes(&first)},"latest"])
        )
        .is_none()
    );
    assert!(fact_key("eth_getBlockByNumber", &json!(["0x14", false])).is_none());
}

#[derive(Clone, Copy)]
struct Case {
    label: &'static str,
    op: &'static str,
    supply: u64,
    debt: u64,
    collateral: bool,
    all: bool,
    enabled: bool,
    scoped: bool,
}
async fn synthetic_reply(
    fixture: &Fixture,
    case: Case,
    method: &str,
    mut params: Value,
) -> Result<Value> {
    if method == "eth_getCode" && params[0] == json!(lower_address(measurement_owner())) {
        return Ok(json!("0x"));
    }
    if method == "eth_sendRawTransaction" {
        let raw = gofin_evm::rpc_bytes(&params[0])?;
        return Ok(json!(format!(
            "{:#x}",
            gofin_evm::alloy::primitives::keccak256(raw)
        )));
    }
    // The synthetic Fixture accepts numeric tags; emulate EIP-1898 faithfully.
    let index = if method == "eth_getStorageAt" { 2 } else { 1 };
    if let Some(hash_value) = params[index]["blockHash"].as_str() {
        require(
            params[index]["requireCanonical"] == json!(true),
            "FIXTURE_PIN",
            "Expected canonical block hash.",
        )?;
        let block = U256::from_str_radix(hash_value.trim_start_matches("0x"), 16)
            .map_err(|_| GoFinError::validation("Invalid synthetic block hash."))?
            .to::<u64>();
        require(
            blockhash(block) == hash_value,
            "FIXTURE_PIN",
            "Unknown synthetic block hash.",
        )?;
        params[index] = json!(format!("{block:#x}"));
    }
    let data = gofin_evm::rpc_bytes(&params[0]["data"]).unwrap_or_default();
    let is_call = method == "eth_call" && data.len() >= 4;
    let supports_other_collateral =
        case.debt > 0 && matches!(case.op, "aave.withdraw" | "aave.collateral");
    if is_call && data[..4] == Pool::getUserConfigurationCall::SELECTOR {
        Ok(abi_value(
            U256::from(if case.collateral { 2 } else { 0 })
                | U256::from(if case.debt > 0 { 1 } else { 0 })
                | U256::from(if supports_other_collateral { 8 } else { 0 }),
        ))
    } else if is_call
        && data[..4] == DataProvider::getUserReserveDataCall::SELECTOR
        && DataProvider::getUserReserveDataCall::abi_decode_validate(&data)
            .unwrap()
            .asset
            != addr(101)
    {
        let asset = DataProvider::getUserReserveDataCall::abi_decode_validate(&data)
            .unwrap()
            .asset;
        let active = asset == addr(102) && supports_other_collateral;
        Ok(abi_value((
            U256::from(if active { case.supply } else { 0 }),
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U40::ZERO,
            active,
        )))
    } else {
        fixture.request(method, params).await
    }
}
fn install_success_fixture(
    case: Case,
    fixture: &Fixture,
    adapter: &EthereumAdapter,
    prepared: &PreparedOperation,
) -> Result<()> {
    let binding: Binding = serde_json::from_value(prepared.binding.clone()).unwrap();
    let height = 22;
    let mut state = fixture.states.lock().unwrap().get(&21).unwrap().clone();
    let index = state.income;
    let debt_index = state.debt_index;
    let owner = address(&binding.transaction.sender)?;
    let token = addr(101);
    let pool = addr(300);
    let a_token = addr(110);
    let debt_token = addr(120);
    let requested = U256::from(1_000_000);
    let logs = match case.op {
        "aave.supply" => {
            let minted = adapter.scaled_change(requested, index, &PositionKind::Supply, true)?;
            let after = add(state.scaled_supply, minted)?;
            let backing = proof::revision_five_backing(
                quantity(requested),
                quantity(state.scaled_supply),
                quantity(after),
                quantity(state.supply_previous),
                quantity(index),
                quantity(index),
            )?;
            state.scaled_supply = after;
            state.supply_previous = index;
            state.collateral = true;
            let mint_value = u256(&backing.mint_value)?;
            vec![
                event(
                    token,
                    Erc20::Transfer {
                        from: owner,
                        to: a_token,
                        value: requested,
                    },
                    height,
                ),
                event(
                    pool,
                    Pool::Supply {
                        reserve: token,
                        user: owner,
                        onBehalfOf: owner,
                        amount: requested,
                        referralCode: 0,
                    },
                    height,
                ),
                event(
                    a_token,
                    AToken::Mint {
                        caller: owner,
                        onBehalfOf: owner,
                        value: mint_value,
                        balanceIncrease: u256(&backing.mint_balance_increase)?,
                        index,
                    },
                    height,
                ),
                event(
                    a_token,
                    Erc20::Transfer {
                        from: Address::ZERO,
                        to: owner,
                        value: mint_value,
                    },
                    height,
                ),
            ]
        }
        "aave.borrow" => {
            let before = state.scaled_debt;
            let minted =
                adapter.scaled_change(requested, debt_index, &PositionKind::VariableDebt, true)?;
            let after = add(before, minted)?;
            let pre = adapter.position_backing(before, debt_index, &PositionKind::VariableDebt)?;
            let old = adapter.position_backing(
                before,
                state.debt_previous,
                &PositionKind::VariableDebt,
            )?;
            let post = adapter.position_backing(after, debt_index, &PositionKind::VariableDebt)?;
            let mint_value = sub(post, old)?;
            let increase = sub(pre, old)?;
            state.scaled_debt = after;
            state.debt_previous = debt_index;
            vec![
                event(
                    pool,
                    Pool::Borrow {
                        reserve: token,
                        user: owner,
                        onBehalfOf: owner,
                        amount: requested,
                        interestRateMode: 2,
                        borrowRate: U256::from(1),
                        referralCode: 0,
                    },
                    height,
                ),
                event(
                    token,
                    Erc20::Transfer {
                        from: a_token,
                        to: owner,
                        value: requested,
                    },
                    height,
                ),
                event(
                    debt_token,
                    AToken::Mint {
                        caller: owner,
                        onBehalfOf: owner,
                        value: mint_value,
                        balanceIncrease: increase,
                        index: debt_index,
                    },
                    height,
                ),
                event(
                    debt_token,
                    Erc20::Transfer {
                        from: Address::ZERO,
                        to: owner,
                        value: mint_value,
                    },
                    height,
                ),
            ]
        }
        "aave.repay" => {
            let before = state.scaled_debt;
            let claim =
                adapter.position_backing(before, debt_index, &PositionKind::VariableDebt)?;
            let paid = if case.all { claim } else { requested };
            let burned = if paid == claim {
                before
            } else {
                adapter.scaled_change(paid, debt_index, &PositionKind::VariableDebt, false)?
            };
            state.scaled_debt = sub(before, burned)?;
            vec![
                event(
                    pool,
                    Pool::Repay {
                        reserve: token,
                        user: owner,
                        repayer: owner,
                        amount: paid,
                        useATokens: false,
                    },
                    height,
                ),
                event(
                    token,
                    Erc20::Transfer {
                        from: owner,
                        to: a_token,
                        value: paid,
                    },
                    height,
                ),
            ]
        }
        "aave.withdraw" => {
            let before = state.scaled_supply;
            let claim = adapter.position_backing(before, index, &PositionKind::Supply)?;
            let paid = if case.all { claim } else { requested };
            let burned = adapter.scaled_change(paid, index, &PositionKind::Supply, false)?;
            state.scaled_supply = sub(before, burned)?;
            if state.scaled_supply.is_zero() {
                state.collateral = false;
            }
            vec![
                event(
                    pool,
                    Pool::Withdraw {
                        reserve: token,
                        user: owner,
                        to: owner,
                        amount: paid,
                    },
                    height,
                ),
                event(
                    token,
                    Erc20::Transfer {
                        from: a_token,
                        to: owner,
                        value: paid,
                    },
                    height,
                ),
            ]
        }
        "aave.collateral" => {
            state.collateral = case.enabled;
            if case.enabled {
                vec![event(
                    pool,
                    Pool::ReserveUsedAsCollateralEnabled {
                        reserve: token,
                        user: owner,
                    },
                    height,
                )]
            } else {
                vec![event(
                    pool,
                    Pool::ReserveUsedAsCollateralDisabled {
                        reserve: token,
                        user: owner,
                    },
                    height,
                )]
            }
        }
        _ => return Err(GoFinError::validation("Unsupported synthetic observation.")),
    };
    fixture.post(height, state);
    fixture.set_receipt(&binding, height, logs);
    Ok(())
}
type Counts = (usize, usize, usize, usize, usize);
async fn preview(
    session: &mut dyn gofin_domain::operation_lifecycle::LifecycleSession,
    measure: &impl Fn() -> Counts,
) -> (
    Result<gofin_domain::operation_lifecycle::CandidateResult<Value>>,
    Vec<Counts>,
) {
    let mut phases = Vec::new();
    let result = session.collect().await;
    phases.push(measure());
    if let Err(e) = result {
        return (Err(e), phases);
    }
    let result = session.evaluate();
    phases.push(measure());
    if let Err(e) = result {
        return (Err(e), phases);
    }
    let result = session.prepare().await;
    phases.push(measure());
    if let Err(e) = result {
        return (Err(e), phases);
    }
    let result = session.simulate().await;
    phases.push(measure());
    (result, phases)
}
async fn execute_session(
    session: &mut dyn gofin_domain::operation_lifecycle::LifecycleSession,
    original: &PreparedOperation,
    reservation: Option<gofin_domain::operation_lifecycle::NonceReservation>,
    measure: &impl Fn() -> Counts,
) -> (Result<Submission>, Vec<Counts>) {
    let journal = Arc::new(MeasurementJournal {
        expected: original.clone(),
        saved: Mutex::new(None),
    });
    let authorization =
        gofin_domain::operation_lifecycle::AuthorizedOperation::with_nonce_reservation(
            original.clone(),
            journal.clone(),
            reservation,
        )
        .and_then(|authorized| session.authorize(authorized));
    let authorized_counts = measure();
    assert_eq!(authorized_counts, (0, 0, 0, 0, 0));
    if let Err(error) = authorization {
        return (Err(error), vec![authorized_counts]);
    }
    let submitted = session.execute().await;
    let execution_counts = measure();
    if let Ok(submission) = &submitted {
        assert_eq!(journal.saved.lock().unwrap().as_ref(), Some(submission));
    }
    (submitted, vec![authorized_counts, execution_counts])
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_action_and_read_http_counts() {
    let cases = [
        Case {
            label: "supply_new",
            op: "aave.supply",
            supply: 0,
            debt: 0,
            collateral: false,
            all: false,
            enabled: false,
            scoped: false,
        },
        Case {
            label: "supply_catalog_first",
            op: "aave.supply",
            supply: 0,
            debt: 0,
            collateral: false,
            all: false,
            enabled: false,
            scoped: false,
        },
        Case {
            label: "borrow_open",
            op: "aave.borrow",
            supply: 10_000_000,
            debt: 0,
            collateral: true,
            all: false,
            enabled: false,
            scoped: false,
        },
        Case {
            label: "borrow_catalog_first",
            op: "aave.borrow",
            supply: 10_000_000,
            debt: 0,
            collateral: true,
            all: false,
            enabled: false,
            scoped: false,
        },
        Case {
            label: "borrow_existing_debt",
            op: "aave.borrow",
            supply: 10_000_000,
            debt: 100_000,
            collateral: true,
            all: false,
            enabled: false,
            scoped: false,
        },
        Case {
            label: "repay_exact",
            op: "aave.repay",
            supply: 0,
            debt: 2_000_000,
            collateral: false,
            all: false,
            enabled: false,
            scoped: false,
        },
        Case {
            label: "repay_all",
            op: "aave.repay",
            supply: 0,
            debt: 2_000_000,
            collateral: false,
            all: true,
            enabled: false,
            scoped: false,
        },
        Case {
            label: "withdraw_exact_no_debt",
            op: "aave.withdraw",
            supply: 10_000_000,
            debt: 0,
            collateral: false,
            all: false,
            enabled: false,
            scoped: false,
        },
        Case {
            label: "withdraw_all_no_debt",
            op: "aave.withdraw",
            supply: 999_999,
            debt: 0,
            collateral: false,
            all: true,
            enabled: false,
            scoped: false,
        },
        Case {
            label: "withdraw_exact_with_debt",
            op: "aave.withdraw",
            supply: 10_000_000,
            debt: 1_000_000,
            collateral: true,
            all: false,
            enabled: false,
            scoped: false,
        },
        Case {
            label: "withdraw_all_with_debt",
            op: "aave.withdraw",
            supply: 10_000_000,
            debt: 1_000_000,
            collateral: true,
            all: true,
            enabled: false,
            scoped: false,
        },
        Case {
            label: "collateral_enable",
            op: "aave.collateral",
            supply: 10_000_000,
            debt: 0,
            collateral: false,
            all: false,
            enabled: true,
            scoped: false,
        },
        Case {
            label: "collateral_enable_with_debt",
            op: "aave.collateral",
            supply: 10_000_000,
            debt: 1_000_000,
            collateral: false,
            all: false,
            enabled: true,
            scoped: false,
        },
        Case {
            label: "collateral_disable_no_debt",
            op: "aave.collateral",
            supply: 10_000_000,
            debt: 0,
            collateral: true,
            all: false,
            enabled: false,
            scoped: false,
        },
        Case {
            label: "collateral_disable_with_debt",
            op: "aave.collateral",
            supply: 10_000_000,
            debt: 1_000_000,
            collateral: true,
            all: false,
            enabled: false,
            scoped: false,
        },
        Case {
            label: "markets",
            op: "aave.markets",
            supply: 10_000_000,
            debt: 0,
            collateral: true,
            all: false,
            enabled: false,
            scoped: false,
        },
        Case {
            label: "reserves_scoped",
            op: "aave.reserves",
            supply: 10_000_000,
            debt: 0,
            collateral: true,
            all: false,
            enabled: false,
            scoped: true,
        },
        Case {
            label: "reserves_full",
            op: "aave.reserves",
            supply: 10_000_000,
            debt: 0,
            collateral: true,
            all: false,
            enabled: false,
            scoped: false,
        },
        Case {
            label: "positions_scoped",
            op: "aave.positions",
            supply: 10_000_000,
            debt: 0,
            collateral: true,
            all: false,
            enabled: false,
            scoped: true,
        },
        Case {
            label: "positions_full",
            op: "aave.positions",
            supply: 10_000_000,
            debt: 0,
            collateral: true,
            all: false,
            enabled: false,
            scoped: false,
        },
    ];
    let violations = Mutex::new(Vec::<Value>::new());
    let mut fixed_exposure_counts: BTreeMap<&str, [Vec<Counts>; 4]> = BTreeMap::new();
    for market_size in [2u64, 20, 50] {
        for case in cases {
            if case.label.ends_with("catalog_first") && market_size != 20 {
                continue;
            }
            let fixture = Arc::new(Fixture::new());
            {
                let mut state = fixture.states.lock().unwrap();
                let s = state.get_mut(&20).unwrap();
                s.scaled_supply = U256::from(case.supply);
                s.scaled_debt = U256::from(case.debt);
                s.collateral = case.collateral;
                s.supply_previous = s.income;
                s.debt_previous = s.debt_index;
            }
            let historical_state = fixture.states.lock().unwrap().get(&20).unwrap().clone();
            fixture.states.lock().unwrap().insert(19, historical_state);
            fixture.other_stable.store(0, Ordering::SeqCst);
            fixture.call_overrides.lock().unwrap().insert(
                Pool::getReservesListCall::SELECTOR,
                abi_value((101..101 + market_size).map(addr).collect::<Vec<_>>()),
            );
            if case.op == "aave.repay" && case.all {
                *fixture.allowance.lock().unwrap() = U256::from(3_000_000);
            }
            if case.debt > 0 {
                fixture.call_overrides.lock().unwrap().insert(
                    Pool::getUserAccountDataCall::SELECTOR,
                    abi_value((
                        U256::from(2_000_000_000u64),
                        U256::from(100_000_000),
                        U256::from(1_500_000_000),
                        U256::from(8500),
                        U256::from(8000),
                        U256::from(17) * pow10(18).unwrap(),
                    )),
                );
            }
            let physical = Arc::new(AtomicUsize::new(0));
            let worker_physical = physical.clone();
            let methods = Arc::new(Mutex::new(BTreeMap::<String, usize>::new()));
            let worker_methods = methods.clone();
            let aggregates = Arc::new(AtomicUsize::new(0));
            let worker_aggregates = aggregates.clone();
            let subcalls = Arc::new(AtomicUsize::new(0));
            let worker_subcalls = subcalls.clone();
            let direct = Arc::new(AtomicUsize::new(0));
            let worker_direct = direct.clone();
            let metadata_calls = Arc::new(AtomicUsize::new(0));
            let worker_metadata_calls = metadata_calls.clone();
            let mcp_posts = Arc::new(AtomicUsize::new(0));
            let mcp_tool_calls = Arc::new(AtomicUsize::new(0));
            let catalog_lookups = Arc::new(AtomicUsize::new(0));
            let metadata_remote_calls = Arc::new(AtomicUsize::new(0));
            let fault = Arc::new(AtomicUsize::new(0));
            let worker_fault = fault.clone();
            let facts = Arc::new(Mutex::new(BTreeMap::<String, usize>::new()));
            let worker_facts = facts.clone();
            let tracking = Arc::new(AtomicBool::new(true));
            let worker_tracking = tracking.clone();
            let phase_index = AtomicUsize::new(0);
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let stop = Arc::new(AtomicBool::new(false));
            let worker_stop = stop.clone();
            let worker_fixture = fixture.clone();
            let handle = tokio::runtime::Handle::current();
            let worker = std::thread::spawn(move || {
                while !worker_stop.load(Ordering::Relaxed) {
                    let Ok((mut stream, _)) = listener.accept() else {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        continue;
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                        .unwrap();
                    let mut bytes = Vec::new();
                    let (body_start, length) = loop {
                        let mut buffer = [0u8; 4096];
                        let n = stream.read(&mut buffer).unwrap();
                        assert!(n > 0);
                        bytes.extend_from_slice(&buffer[..n]);
                        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&bytes[..end]);
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    line.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap())
                                })
                                .unwrap();
                            break (end + 4, length);
                        }
                    };
                    while bytes.len() < body_start + length {
                        let mut buffer = [0u8; 4096];
                        let n = stream.read(&mut buffer).unwrap();
                        assert!(n > 0);
                        bytes.extend_from_slice(&buffer[..n]);
                    }
                    let request: Value =
                        serde_json::from_slice(&bytes[body_start..body_start + length]).unwrap();
                    worker_physical.fetch_add(1, Ordering::SeqCst);
                    let method = request["method"].as_str().unwrap();
                    *worker_methods
                        .lock()
                        .unwrap()
                        .entry(method.into())
                        .or_default() += 1;
                    let params = request["params"].clone();
                    let multicall = address(crate::collection::MULTICALL).unwrap();
                    if worker_tracking.load(Ordering::SeqCst)
                        && !(method == "eth_call"
                            && params[0]["to"] == json!(lower_address(multicall)))
                        && let Some(key) = fact_key(method, &params)
                    {
                        *worker_facts.lock().unwrap().entry(key).or_default() += 1;
                    }
                    let result = if method == "eth_call" && worker_fault.load(Ordering::SeqCst) == 4
                    {
                        Err(GoFinError::validation(
                            "Financial reads and simulation are unavailable after approval.",
                        ))
                    } else if method == "eth_call"
                        && params[0]["to"] == json!(lower_address(multicall))
                        && worker_fault.load(Ordering::SeqCst) == 3
                    {
                        worker_aggregates.fetch_add(1, Ordering::SeqCst);
                        Ok(json!("0x"))
                    } else if method == "eth_call"
                        && params[0]["to"] == json!(lower_address(multicall))
                    {
                        worker_aggregates.fetch_add(1, Ordering::SeqCst);
                        let data = gofin_evm::rpc_bytes(&params[0]["data"]).unwrap();
                        let calls = abi::Multicall3::aggregate3Call::abi_decode_validate(&data)
                            .unwrap()
                            .calls;
                        worker_subcalls.fetch_add(calls.len(), Ordering::SeqCst);
                        let mut rows = Vec::new();
                        for call in calls {
                            let subparams = json!([{"to":lower_address(call.target),"data":hex_bytes(&call.callData)},params[1]]);
                            if selected_metadata(&subparams) {
                                worker_metadata_calls.fetch_add(1, Ordering::SeqCst);
                            }
                            if worker_tracking.load(Ordering::SeqCst)
                                && let Some(key) = fact_key("eth_call", &subparams)
                            {
                                *worker_facts.lock().unwrap().entry(key).or_default() += 1;
                            }
                            rows.push(
                                match handle.block_on(synthetic_reply(
                                    &worker_fixture,
                                    case,
                                    "eth_call",
                                    subparams,
                                )) {
                                    Ok(raw) => abi::Multicall3::Result3 {
                                        success: true,
                                        returnData: gofin_evm::rpc_bytes(&raw).unwrap(),
                                    },
                                    Err(_) => abi::Multicall3::Result3 {
                                        success: false,
                                        returnData: Vec::new().into(),
                                    },
                                },
                            );
                        }
                        match worker_fault.load(Ordering::SeqCst) {
                            1 if !rows.is_empty() => {
                                rows[0].success = false;
                                rows[0].returnData = Vec::new().into();
                            }
                            2 => {
                                rows.pop();
                            }
                            _ => {}
                        }
                        Ok(json!(hex_bytes(&rows.abi_encode())))
                    } else {
                        if method == "eth_call" {
                            worker_direct.fetch_add(1, Ordering::SeqCst);
                            if selected_metadata(&params) {
                                worker_metadata_calls.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                        handle.block_on(synthetic_reply(&worker_fixture, case, method, params))
                    };
                    let response = match result {
                        Ok(value) => json!({"jsonrpc":"2.0","id":request["id"],"result":value}),
                        Err(_) => {
                            json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"synthetic target failure"}})
                        }
                    };
                    let body = response.to_string();
                    write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
                }
            });
            let counted = Arc::new(Counted {
                inner: gofin_evm::HttpRpc::isolated_localhost(url, 1).unwrap(),
                logical: AtomicUsize::new(0),
            });
            let mut adapter = adapter(fixture.clone()).with_aave_mcp(
                aave_mcp::AaveMcpClient::with_transport(Arc::new(CountedMcp {
                    inner: SyntheticAaveMcp {
                        calls: Arc::new(AtomicUsize::new(0)),
                    },
                    posts: mcp_posts.clone(),
                    tool_calls: mcp_tool_calls.clone(),
                })),
            );
            if case.label.ends_with("catalog_first") {
                adapter = adapter.with_token_discovery(Arc::new(LocalCatalog {
                    lookups: catalog_lookups.clone(),
                    remote_calls: metadata_remote_calls.clone(),
                }));
            }
            adapter.wallets[0].address = lower_address(measurement_owner());
            adapter.runtime = Arc::new(EvmRuntime::new(
                counted.clone(),
                1,
                Some(Arc::new(
                    gofin_evm::LocalEnvelopeSigner::from_private_key(MEASUREMENT_KEY).unwrap(),
                )),
            ));
            let measure = || {
                let phase = phase_index.fetch_add(1, Ordering::SeqCst);
                let observed_methods = std::mem::take(&mut *methods.lock().unwrap());
                assert!(
                    !observed_methods.contains_key("eth_getCode")
                        && !observed_methods.contains_key("eth_getStorageAt"),
                    "Aave must use trusted deployments without runtime code/slot reads: {} phase{} {:?}",
                    case.label,
                    phase,
                    observed_methods
                );
                let write = !matches!(case.op, "aave.markets" | "aave.reserves" | "aave.positions");
                if write && matches!(phase, 9 | 11) {
                    assert_eq!(
                        observed_methods,
                        BTreeMap::from([
                            ("eth_chainId".into(), 1),
                            ("eth_getBalance".into(), 1),
                            ("eth_getBlockByNumber".into(), 1),
                            ("eth_getTransactionCount".into(), 1),
                            ("eth_sendRawTransaction".into(), 1),
                        ]),
                        "execution must only check envelope requirements"
                    );
                }
                if write && phase == 14 {
                    assert_eq!(
                        observed_methods,
                        BTreeMap::from([
                            ("eth_getBalance".into(), 1),
                            ("eth_getBlockByNumber".into(), 1),
                            ("eth_sendRawTransaction".into(), 1),
                        ]),
                        "broker-observed chain and nonce must not be read twice"
                    );
                }
                if !observed_methods.is_empty() {
                    eprintln!(
                        "METHODS N={} {} phase={} {}",
                        market_size,
                        case.label,
                        phase,
                        serde_json::to_string(&observed_methods).unwrap()
                    );
                }
                let metadata = metadata_calls.swap(0, Ordering::SeqCst);
                let posts = mcp_posts.swap(0, Ordering::SeqCst);
                let tools = mcp_tool_calls.swap(0, Ordering::SeqCst);
                let local = catalog_lookups.swap(0, Ordering::SeqCst);
                let remote = metadata_remote_calls.swap(0, Ordering::SeqCst);
                if write && matches!(phase, 8..=14) {
                    assert_eq!(
                        metadata + posts + tools + local + remote,
                        0,
                        "execution must not recollect metadata or request MCP transactions"
                    );
                }
                if metadata + posts + local + remote > 0 {
                    eprintln!(
                        "EXTERNAL N={} {} phase={} token_metadata_rpc={} mcp_posts={} mcp_tool_calls={} local_catalog_lookups={} metadata_service_calls={}",
                        market_size, case.label, phase, metadata, posts, tools, local, remote
                    );
                }
                if matches!(case.op, "aave.reserves" | "aave.positions") && !case.scoped {
                    assert_eq!(
                        posts, 0,
                        "bulk native reads must not issue per-token MCP metadata requests"
                    );
                }
                if case.label.ends_with("catalog_first") {
                    assert_eq!(
                        metadata, 0,
                        "catalog-first capture must not request token decimals/symbol"
                    );
                    assert_eq!(remote, 0, "local metadata hint cannot hide remote lookups");
                    if phase == 0 {
                        assert_eq!(
                            posts, 0,
                            "catalog-first collection must not request MCP metadata"
                        );
                    }
                }
                let duplicates = std::mem::take(&mut *facts.lock().unwrap())
                    .into_iter()
                    .filter(|(_, count)| *count > 1)
                    .collect::<Vec<_>>();
                if !duplicates.is_empty() {
                    violations.lock().unwrap().push(json!({"market_size":market_size,"case":case.label,"phase_index":phase,"duplicates":duplicates}));
                }
                (
                    counted.logical.swap(0, Ordering::SeqCst),
                    physical.swap(0, Ordering::SeqCst),
                    aggregates.swap(0, Ordering::SeqCst),
                    subcalls.swap(0, Ordering::SeqCst),
                    direct.swap(0, Ordering::SeqCst),
                )
            };
            let reads = matches!(case.op, "aave.markets" | "aave.reserves" | "aave.positions");
            let mut input = if reads {
                json!({"wallet":"synthetic","network":"ethereum","chain_id":1})
            } else {
                aave_input(case.op)
            };
            if reads && case.scoped {
                input["tokens"] = json!([lower_address(addr(101))]);
            }
            if case.op == "aave.withdraw" {
                input["recipient"] = json!(lower_address(measurement_owner()));
            }
            if case.op == "aave.collateral" {
                input["enabled"] = json!(case.enabled);
                if case.debt > 0 {
                    input["min_health_factor"] = json!("1.5");
                }
            }
            if case.op == "aave.withdraw" && case.debt > 0 {
                input["min_health_factor"] = json!("1.5");
            }
            if case.all {
                input["amount"] = json!("all");
                input["max_amount"] = json!(if case.op == "aave.repay" {
                    "3"
                } else if case.debt > 0 {
                    "11"
                } else {
                    "1.02"
                });
            }
            let mut cold_session = adapter
                .operation_lifecycle(case.op, &input, None)
                .unwrap()
                .unwrap();
            let (cold_result, cold) = preview(cold_session.as_mut(), &measure).await;
            let mut warm_session = adapter
                .operation_lifecycle(case.op, &input, None)
                .unwrap()
                .unwrap();
            let (warm_result, warm) = preview(warm_session.as_mut(), &measure).await;
            assert!(
                cold_result.is_ok() && warm_result.is_ok(),
                "{} N={} initial preview failed",
                case.label,
                market_size
            );
            assert!(
                cold[0].2 > 0 && warm[0].2 > 0,
                "trusted aggregation not exercised"
            );
            for phase in [&cold, &warm] {
                assert_eq!(phase[1], (0, 0, 0, 0, 0));
                if reads {
                    assert_eq!(phase[3], (0, 0, 0, 0, 0));
                } else {
                    assert!(phase[3].1 > 0, "simulate must perform envelope simulation");
                }
            }
            if reads {
                input["block_number"] = json!(19);
                let historical = match adapter.operation_lifecycle(case.op, &input, None) {
                    Ok(Some(mut session)) => {
                        let (result, counts) = preview(session.as_mut(), &measure).await;
                        (result.map(|_| "ok").map_err(|e| e.code), counts)
                    }
                    Ok(None) => (Err("NO_LIFECYCLE".into()), vec![]),
                    Err(e) => (Err(e.code), vec![]),
                };
                eprintln!(
                    "AFTER N={} {} cold={:?} warm={:?} historical={:?} results={:?}/{:?}/{:?}",
                    market_size,
                    case.label,
                    cold,
                    warm,
                    historical.1,
                    cold_result.map(|_| "ok").map_err(|e| e.code),
                    warm_result.map(|_| "ok").map_err(|e| e.code),
                    historical.0
                );
            } else if let Ok(gofin_domain::operation_lifecycle::CandidateResult::Write(prepared)) =
                cold_result
            {
                // Contract reads/simulations are deliberately unavailable after
                // approval; executing the approved envelope must not need them.
                fault.store(4, Ordering::SeqCst);
                let (execution_result, execution) =
                    execute_session(cold_session.as_mut(), &prepared, None, &measure).await;
                let next = fixture.states.lock().unwrap().get(&20).unwrap().clone();
                fixture.states.lock().unwrap().insert(21, next);
                fixture.latest.store(21, Ordering::SeqCst);
                let (advanced_result, advanced_execution) =
                    execute_session(warm_session.as_mut(), &prepared, None, &measure).await;
                // Exercise the same adapter nonce discovery used under the
                // broker's resource lock, then hand that fact to execution.
                // The store allocation itself is covered by engine tests.
                let (scope, observed_nonce) =
                    adapter.evm_nonce_scope(&prepared).await.unwrap().unwrap();
                let nonce_scope_counts = measure();
                assert_eq!(nonce_scope_counts, (2, 2, 0, 0, 0));
                let reservation = gofin_domain::operation_lifecycle::NonceReservation::new(
                    scope,
                    observed_nonce,
                    observed_nonce,
                )
                .unwrap();
                let mut broker_session = adapter
                    .operation_lifecycle(case.op, &input, None)
                    .unwrap()
                    .unwrap();
                let (broker_result, broker_counts) = execute_session(
                    broker_session.as_mut(),
                    &prepared,
                    Some(reservation),
                    &measure,
                )
                .await;
                assert!(
                    broker_result.is_ok(),
                    "{} broker-observation execution: {:?}",
                    case.label,
                    broker_result.err()
                );
                assert_eq!(broker_counts[1], (3, 3, 0, 0, 0));
                eprintln!(
                    "BROKER_EXECUTION N={} {} nonce_scope={:?} authorization={:?} execute={:?}",
                    market_size, case.label, nonce_scope_counts, broker_counts[0], broker_counts[1]
                );
                fault.store(0, Ordering::SeqCst);
                let signature = [
                    cold.clone(),
                    warm.clone(),
                    execution.clone(),
                    advanced_execution.clone(),
                ];
                if let Some(previous) = fixed_exposure_counts.get(case.label) {
                    assert_eq!(
                        previous, &signature,
                        "inactive reserves changed phase counts for {}",
                        case.label
                    );
                } else {
                    fixed_exposure_counts.insert(case.label, signature);
                }
                let record = record((*prepared).clone());
                let observed = cold_session.observe(&record).await;
                let observed_counts = measure();
                let pending = match observed {
                    Ok(()) => cold_session
                        .evaluate_observation(&record)
                        .map(|r| format!("{:?}", r.status))
                        .map_err(|e| e.code),
                    Err(e) => Err(e.code),
                };
                let evaluated_counts = measure();
                assert!(
                    execution_result.is_ok() && advanced_result.is_ok(),
                    "{} N={} execution failed: {:?} / {:?}",
                    case.label,
                    market_size,
                    execution_result.as_ref().err(),
                    advanced_result.as_ref().err()
                );
                assert_eq!(pending.as_deref(), Ok("Pending"));
                assert_eq!(evaluated_counts, (0, 0, 0, 0, 0));
                let install = install_success_fixture(case, &fixture, &adapter, &prepared);
                let completed = if let Err(e) = install {
                    (Err(e.code), measure(), measure())
                } else {
                    let observed = cold_session.observe(&record).await;
                    let collect = measure();
                    let result = match observed {
                        Ok(()) => cold_session
                            .evaluate_observation(&record)
                            .map(|r| format!("{:?}", r.status))
                            .map_err(|e| format!("{}: {}", e.code, e.message)),
                        Err(e) => Err(format!("{}: {}", e.code, e.message)),
                    };
                    let evaluate = measure();
                    (result, collect, evaluate)
                };
                assert_eq!(
                    completed.0.as_deref(),
                    Ok("Completed"),
                    "{} N={} settlement",
                    case.label,
                    market_size
                );
                assert_eq!(completed.2, (0, 0, 0, 0, 0));
                eprintln!(
                    "OBSERVATION N={} {} completed={:?} collect={:?} evaluate={:?}",
                    market_size, case.label, completed.0, completed.1, completed.2
                );
                eprintln!(
                    "AFTER N={} {} cold={:?} warm={:?} execution={:?} advanced_execution={:?} pending={:?}/{:?} results=ok/{:?}/{:?}/{:?}/{:?}",
                    market_size,
                    case.label,
                    cold,
                    warm,
                    execution,
                    advanced_execution,
                    observed_counts,
                    evaluated_counts,
                    warm_result.map(|_| "ok").map_err(|e| e.code),
                    execution_result.map(|_| "ok").map_err(|e| e.code),
                    advanced_result.map(|_| "ok").map_err(|e| e.code),
                    pending
                );
            } else {
                eprintln!(
                    "AFTER N={} {} cold={:?} warm={:?} results={:?}/{:?}",
                    market_size,
                    case.label,
                    cold,
                    warm,
                    cold_result.err().map(|e| e.code),
                    warm_result.err().map(|e| e.code)
                );
            }
            if market_size == 2
                && matches!(
                    case.label,
                    "supply_new"
                        | "borrow_open"
                        | "repay_exact"
                        | "withdraw_exact_no_debt"
                        | "collateral_enable"
                )
            {
                tracking.store(false, Ordering::SeqCst);
                // Restore synthetic state at a newer block; never rewind a provider
                // whose freshness tracking has already observed the receipt head.
                let original_state = fixture.states.lock().unwrap().get(&20).unwrap().clone();
                fixture.states.lock().unwrap().insert(24, original_state);
                fixture.latest.store(24, Ordering::SeqCst);
                for mode in 1..=3 {
                    // A successful direct-fallback collection must also avoid
                    // repeated exact facts. Failed injections are not a pass.
                    tracking.store(mode == 3, Ordering::SeqCst);
                    fault.store(mode, Ordering::SeqCst);
                    let mut session = adapter
                        .operation_lifecycle(case.op, &input, None)
                        .unwrap()
                        .unwrap();
                    let result = match session.collect().await {
                        Ok(()) => session.evaluate(),
                        Err(e) => Err(e),
                    };
                    let counts = measure();
                    if mode == 3 {
                        assert!(
                            result.is_ok(),
                            "{} direct fallback failed: {:?}",
                            case.label,
                            result.err()
                        );
                        assert!(
                            counts.2 > 0,
                            "unavailable registered aggregate was attempted"
                        );
                        assert!(counts.4 > 0);
                    } else {
                        assert_eq!(
                            result.unwrap_err().code,
                            if mode == 1 {
                                "TOKEN_CALL_FAILED"
                            } else {
                                "MULTICALL_INCOMPLETE"
                            }
                        );
                    }
                    eprintln!("FAULT {} mode={} counts={:?}", case.label, mode, counts);
                }
                fault.store(0, Ordering::SeqCst);
            }
            stop.store(true, Ordering::Relaxed);
            worker.join().unwrap();
        }
    }
    let found = violations.into_inner().unwrap();
    if !found.is_empty() {
        eprintln!(
            "DUPLICATE_FACTS_JSON={}",
            serde_json::to_string(&found).unwrap()
        );
    }
    assert!(
        found.is_empty(),
        "duplicate fact observations within a lifecycle phase"
    );
}
