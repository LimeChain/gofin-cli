//! Synthetic native Aave read call counts; no network or live credentials.
use super::*;

struct CountRpc {
    inner: Arc<Fixture>,
    requests: Mutex<Vec<(String, Value)>>,
}
#[async_trait]
impl EvmRpc for CountRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.requests
            .lock()
            .unwrap()
            .push((method.into(), params.clone()));
        self.inner.request(method, params).await
    }
}

#[tokio::test]
async fn native_aave_read_fixture_counts_and_scope_are_reproducible() {
    for operation in ["aave.reserves", "aave.positions"] {
        for scoped in [true, false] {
            let fixture = Arc::new(Fixture::new());
            fixture.states.lock().unwrap().insert(
                19,
                AtBlock {
                    income: ray(),
                    debt_index: ray(),
                    ..Default::default()
                },
            );
            let counter = Arc::new(CountRpc {
                inner: fixture.clone(),
                requests: Mutex::new(Vec::new()),
            });
            let mut adapter = adapter(fixture);
            adapter.runtime = Arc::new(EvmRuntime::new(counter.clone(), 1, None));
            let mut input = json!({"wallet":"synthetic", "network":"evm", "chain_id":1});
            if scoped {
                input["tokens"] = json!([lower_address(addr(101))]);
            }
            let mut counts = Vec::new();
            for historical in [false, false, true] {
                if historical {
                    input["block_number"] = json!(19);
                }
                counter.requests.lock().unwrap().clear();
                let result = adapter.read(operation, &input).await.unwrap();
                let requests = counter.requests.lock().unwrap();
                let eth_calls = requests
                    .iter()
                    .filter(|(method, _)| method == "eth_call")
                    .count();
                let headers = requests
                    .iter()
                    .filter(|(method, _)| method == "eth_getBlockByNumber")
                    .count();
                let block_numbers = requests
                    .iter()
                    .filter(|(method, _)| method == "eth_blockNumber")
                    .count();
                let rows = if operation == "aave.positions" {
                    &result["data"]["positions"]
                } else {
                    &result["data"]
                };
                assert_eq!(rows.as_array().unwrap().len(), if scoped { 1 } else { 2 });
                assert_eq!(result["block"], if historical { 19 } else { 20 });
                assert!(
                    requests
                        .iter()
                        .filter(|(method, _)| method == "eth_call")
                        .all(|(_, params)| params[1] == if historical { "0x13" } else { "0x14" })
                );
                counts.push((requests.len(), eth_calls, headers, block_numbers));
            }
            let expected = match (operation, scoped) {
                ("aave.reserves", true) => (34, 20),
                ("aave.reserves", false) => (47, 32),
                ("aave.positions", true) => (42, 28),
                ("aave.positions", false) => (58, 43),
                _ => unreachable!(),
            };
            assert_eq!(
                counts,
                vec![
                    (expected.0, expected.1, 4, 1),
                    (expected.0, expected.1, 4, 1),
                    (expected.0 - 1, expected.1, 4, 0),
                ]
            );
            eprintln!("{operation} scoped={scoped}: cold/warm/historical {counts:?}");
        }
    }
}
