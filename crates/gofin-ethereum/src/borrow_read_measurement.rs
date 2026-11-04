//! Controlled synthetic borrow RPC measurements; no external network.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn borrow_http_rpc_measurement() {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct Counted {
        inner: gofin_evm::HttpRpc,
        logical: AtomicUsize,
        requests: Mutex<Vec<(String, Value)>>,
    }
    #[async_trait]
    impl EvmRpc for Counted {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            self.logical.fetch_add(1, Ordering::SeqCst);
            self.requests
                .lock()
                .unwrap()
                .push((method.to_owned(), params.clone()));
            self.inner.request(method, params).await
        }
    }
    fn check_pass(counted: &Counted, tag: &str) {
        let mut requests = counted.requests.lock().unwrap();
        let list_selector = hex_bytes(&Pool::getReservesListCall::SELECTOR);
        assert!(!requests.iter().any(|(method, params)| {
            method == "eth_call"
                && params[0]["data"]
                    .as_str()
                    .is_some_and(|data| data.starts_with(&list_selector))
        }));
        for selector in [
            Pool::ADDRESSES_PROVIDERCall::SELECTOR,
            AddressesProvider::getPoolCall::SELECTOR,
            AddressesProvider::getPriceOracleCall::SELECTOR,
            AddressesProvider::getPoolDataProviderCall::SELECTOR,
        ] {
            let selector = hex_bytes(&selector);
            let matches: Vec<_> = requests
                .iter()
                .filter(|(method, params)| {
                    method == "eth_call"
                        && params[1] == tag
                        && params[0]["data"]
                            .as_str()
                            .is_some_and(|data| data.starts_with(&selector))
                })
                .collect();
            assert_eq!(
                matches.len(),
                1,
                "pinned provider identity must be read once per pass: {selector}"
            );
        }
        requests.clear();
    }
    for active_count in [0u64, 1, 2] {
        let mut measured = Vec::new();
        for reserve_count in [2u64, 20, 50] {
            let fixture = Arc::new(Fixture::new());
            {
                let mut states = fixture.states.lock().unwrap();
                let state = states.get_mut(&20).unwrap();
                state.scaled_supply = U256::from(10_000_000);
                state.collateral = active_count > 0;
            }
            fixture.other_stable.store(0, Ordering::SeqCst);
            fixture.call_overrides.lock().unwrap().insert(
                Pool::getReservesListCall::SELECTOR,
                abi_value((101..101 + reserve_count).map(addr).collect::<Vec<_>>()),
            );
            let physical = Arc::new(AtomicUsize::new(0));
            let worker_physical = physical.clone();
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
                            let length: usize = headers
                                .lines()
                                .find_map(|line| {
                                    line.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse().unwrap())
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
                    let data =
                        gofin_evm::rpc_bytes(&request["params"][0]["data"]).unwrap_or_default();
                    let is_call = request["method"] == "eth_call" && data.len() >= 4;
                    let other_user = is_call
                        && data[..4] == DataProvider::getUserReserveDataCall::SELECTOR
                        && DataProvider::getUserReserveDataCall::abi_decode_validate(&data)
                            .unwrap()
                            .asset
                            != addr(101)
                        && !(active_count == 2
                            && DataProvider::getUserReserveDataCall::abi_decode_validate(&data)
                                .unwrap()
                                .asset
                                == addr(102));
                    let result = if is_call && data[..4] == Pool::getUserConfigurationCall::SELECTOR
                    {
                        abi_value(match active_count {
                            0 => U256::ZERO,
                            1 => U256::from(2),
                            _ => U256::from(2) | (U256::from(2) << 14),
                        })
                    } else if is_call && data[..4] == Pool::getReserveAddressByIdCall::SELECTOR {
                        let call =
                            Pool::getReserveAddressByIdCall::abi_decode_validate(&data).unwrap();
                        abi_value(match call.id {
                            0 => addr(101),
                            7 => addr(102),
                            _ => Address::ZERO,
                        })
                    } else if other_user {
                        abi_value((
                            U256::ZERO,
                            U256::ZERO,
                            U256::ZERO,
                            U256::ZERO,
                            U256::ZERO,
                            U256::ZERO,
                            U256::ZERO,
                            U40::ZERO,
                            false,
                        ))
                    } else {
                        handle
                            .block_on(worker_fixture.request(
                                request["method"].as_str().unwrap(),
                                request["params"].clone(),
                            ))
                            .unwrap()
                    };
                    let result = if is_call
                        && data[..4] == Pool::getReserveDataCall::SELECTOR
                        && Pool::getReserveDataCall::abi_decode_validate(&data)
                            .unwrap()
                            .asset
                            == addr(102)
                    {
                        let mut bytes = gofin_evm::rpc_bytes(&result).unwrap().to_vec();
                        // The eighth ABI word contains the reserve ID.
                        bytes[7 * 32 + 31] = 7;
                        json!(hex_bytes(&bytes))
                    } else {
                        result
                    };
                    let body =
                        json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string();
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
                }
            });
            let counted = Arc::new(Counted {
                inner: gofin_evm::HttpRpc::isolated_localhost(url.clone(), 1).unwrap(),
                logical: AtomicUsize::new(0),
                requests: Mutex::new(Vec::new()),
            });
            let mut a = adapter(fixture.clone());
            a.runtime = Arc::new(EvmRuntime::new(counted.clone(), 1, None));
            let input = aave_input("aave.borrow");
            let first_result = a.prepare("aave.borrow", &input).await;
            if active_count == 0 {
                let error = first_result.unwrap_err();
                assert_eq!(error.code, "HEALTH_FACTOR_LIMIT");
                let counts = (
                    counted.logical.load(Ordering::SeqCst),
                    physical.load(Ordering::SeqCst),
                );
                stop.store(true, Ordering::Relaxed);
                worker.join().unwrap();
                measured.push(vec![counts]);
                eprintln!("borrow N={reserve_count} C=0 rejected (logical, physical): {counts:?}");
                continue;
            }
            let first = first_result.unwrap();
            check_pass(&counted, "0x14");
            let cold = (
                counted.logical.swap(0, Ordering::SeqCst),
                physical.load(Ordering::SeqCst),
            );
            physical.store(0, Ordering::SeqCst);
            a.prepare("aave.borrow", &input).await.unwrap();
            check_pass(&counted, "0x14");
            let warm = (
                counted.logical.swap(0, Ordering::SeqCst),
                physical.load(Ordering::SeqCst),
            );
            physical.store(0, Ordering::SeqCst);
            // The legacy Adapter seam is a local immutable-binding check for
            // Aave v2; financial evidence is never recollected after approval.
            a.revalidate(&first).await.unwrap();
            assert!(counted.requests.lock().unwrap().is_empty());
            let revalidate = (
                counted.logical.swap(0, Ordering::SeqCst),
                physical.load(Ordering::SeqCst),
            );
            let next = fixture.states.lock().unwrap().get(&20).unwrap().clone();
            fixture.states.lock().unwrap().insert(21, next);
            fixture.latest.store(21, Ordering::SeqCst);
            physical.store(0, Ordering::SeqCst);
            a.revalidate(&first).await.unwrap();
            assert!(counted.requests.lock().unwrap().is_empty());
            let historical = (
                counted.logical.load(Ordering::SeqCst),
                physical.load(Ordering::SeqCst),
            );
            assert_eq!(revalidate, (0, 0));
            assert_eq!(historical, (0, 0));
            stop.store(true, Ordering::Relaxed);
            worker.join().unwrap();

            eprintln!(
                "borrow N={reserve_count} C={active_count} HTTP fixture (logical, physical): cold={cold:?}, warm={warm:?}, local_binding={revalidate:?}, advanced_local_binding={historical:?}"
            );
            measured.push(vec![cold, warm, revalidate, historical]);
        }
        assert!(
            measured.windows(2).all(|pair| pair[0] == pair[1]),
            "inactive reserve growth changed logical or physical requests: {measured:?}"
        );
    }
}
