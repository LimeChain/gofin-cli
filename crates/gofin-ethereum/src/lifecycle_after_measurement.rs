//! Lifecycle loopback measurements, no external calls.
use super::*;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Counted {
    inner: gofin_evm::HttpRpc,
    logical: AtomicUsize,
}
#[async_trait]
impl EvmRpc for Counted {
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
    let owner = addr(1);
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
    let result = session.collect_final().await;
    phases.push(measure());
    if let Err(e) = result {
        return (Err(e), phases);
    }
    let result = session.finalize_candidate();
    phases.push(measure());
    (result, phases)
}
async fn revalidate_session(
    session: &mut dyn gofin_domain::operation_lifecycle::LifecycleSession,
    original: &PreparedOperation,
    measure: &impl Fn() -> Counts,
) -> (Result<PreparedOperation>, Vec<Counts>) {
    let mut phases = Vec::new();
    let result = session.refresh(original).await;
    phases.push(measure());
    if let Err(e) = result {
        return (Err(e), phases);
    }
    let result = session.revalidate(original);
    phases.push(measure());
    if let Err(e) = result {
        return (Err(e), phases);
    }
    let result = session.confirm(original).await;
    phases.push(measure());
    if let Err(e) = result {
        return (Err(e), phases);
    }
    let result = session.finalize_validation(original);
    phases.push(measure());
    (result, phases)
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
    let mut fixed_exposure_counts: BTreeMap<&str, [Vec<Counts>; 4]> = BTreeMap::new();
    for market_size in [2u64, 20, 50] {
        for case in cases {
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
            let aggregates = Arc::new(AtomicUsize::new(0));
            let worker_aggregates = aggregates.clone();
            let subcalls = Arc::new(AtomicUsize::new(0));
            let worker_subcalls = subcalls.clone();
            let direct = Arc::new(AtomicUsize::new(0));
            let worker_direct = direct.clone();
            let fault = Arc::new(AtomicUsize::new(0));
            let worker_fault = fault.clone();
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
                    let params = request["params"].clone();
                    let multicall = address(crate::collection::MULTICALL).unwrap();
                    let result = if method == "eth_getCode"
                        && params[0] == json!(lower_address(multicall))
                    {
                        if worker_fault.load(Ordering::SeqCst) == 3 {
                            Ok(json!("0x6000"))
                        } else {
                            Ok(json!(format!(
                                "0x{}",
                                include_str!("multicall/multicall3-runtime.hex").trim()
                            )))
                        }
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
            let mut adapter = adapter(fixture.clone());
            adapter.runtime = Arc::new(EvmRuntime::new(counted.clone(), 1, None));
            let measure = || {
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
                "verified aggregation not exercised"
            );
            for phase in [&cold, &warm] {
                assert_eq!(phase[1], (0, 0, 0, 0, 0));
                assert_eq!(phase[3], (0, 0, 0, 0, 0));
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
                let (reval_result, reval) =
                    revalidate_session(cold_session.as_mut(), &prepared, &measure).await;
                let next = fixture.states.lock().unwrap().get(&20).unwrap().clone();
                fixture.states.lock().unwrap().insert(21, next);
                fixture.latest.store(21, Ordering::SeqCst);
                let (history_result, historical) =
                    revalidate_session(cold_session.as_mut(), &prepared, &measure).await;
                let signature = [
                    cold.clone(),
                    warm.clone(),
                    reval.clone(),
                    historical.clone(),
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
                    reval_result.is_ok() && history_result.is_ok(),
                    "{} N={} revalidation failed",
                    case.label,
                    market_size
                );
                for phases in [&reval, &historical] {
                    assert_eq!(phases[1], (0, 0, 0, 0, 0));
                    assert_eq!(phases[3], (0, 0, 0, 0, 0));
                }
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
                    "AFTER N={} {} cold={:?} warm={:?} revalidation={:?} historical={:?} pending={:?}/{:?} results=ok/{:?}/{:?}/{:?}/{:?}",
                    market_size,
                    case.label,
                    cold,
                    warm,
                    reval,
                    historical,
                    observed_counts,
                    evaluated_counts,
                    warm_result.map(|_| "ok").map_err(|e| e.code),
                    reval_result.map(|_| "ok").map_err(|e| e.code),
                    history_result.map(|_| "ok").map_err(|e| e.code),
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
                fixture.latest.store(20, Ordering::SeqCst);
                for mode in 1..=3 {
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
                        assert_eq!(counts.2, 0);
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
}
