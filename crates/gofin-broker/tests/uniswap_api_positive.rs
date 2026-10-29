//! Public CLI -> broker/engine -> real Ethereum adapter/signing -> owned Anvil.
//! HTTP routing and the permissionless test reactor are synthetic. No upstream
//! provider calls, production credentials, or live funds are used.
use async_trait::async_trait;
use gofin_broker::{
    BrokerConfigV2, BrokerPolicy, NetworkConfig, RuntimeDependencies, SharedUsdValuator,
    compose_runtime_with_clock,
};
use gofin_domain::*;
use gofin_ethereum::uniswap_api::api::{UniswapApiClient, UniswapApiRequest, UniswapApiTransport};
use gofin_ethereum::{EthereumAdapter, EthereumConfig, WalletConnection, WalletValuation};
use gofin_evm::alloy::{
    primitives::{Address, B256, U256},
    sol,
    sol_types::{SolCall, SolValue},
};
use gofin_evm::{EvmRpc, EvmRuntime, LocalEnvelopeSigner, hex_bytes, lower_address};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use gofin_protocol::BrokerRequest;
use gofin_store::Store;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const CLIENT_IDENTITY: &str = "synthetic-uniswap";
const KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";
const NATIVE: &str = "0x0000000000000000000000000000000000000000";
const INPUT: &str = "0x0000000000000000000000000000000000000065";
const OUTPUT: &str = "0x0000000000000000000000000000000000000066";
const REACTOR: &str = "0x00000000000000000000000000000000000001d6";
const PERMIT2: &str = "0x000000000022D473030F116dDEE9F6B43aC78BA3";
include!("support/uniswap_cli.rs");
sol! {
    function mint(address to,uint256 amount);
    function balanceOf(address owner) external view returns(uint256);
    function exchange(address tokenIn,address tokenOut,address swapper,uint256 amountIn,uint256 amountOut,bytes32 orderHash,uint256 nonce);
}
fn addr(v: &str) -> Address {
    v.parse().unwrap()
}
fn owner() -> String {
    lower_address(
        LocalEnvelopeSigner::from_private_key(KEY)
            .unwrap()
            .address(),
    )
}
fn order_hash() -> String {
    format!("0x{}", "ab".repeat(32))
}
fn fail(message: &str) -> GoFinError {
    GoFinError::validation(message)
}
struct Node(std::process::Child);
impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Rpc {
    url: String,
    client: reqwest::Client,
    broadcasts: AtomicUsize,
    traces: AtomicUsize,
}
#[async_trait]
impl EvmRpc for Rpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let v: Value = self
            .client
            .post(&self.url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
            .map_err(|e| fail(&e.to_string()))?
            .json()
            .await
            .map_err(|e| fail(&e.to_string()))?;
        if !v["error"].is_null() {
            return Err(fail(&format!("{method}: {}", v["error"])));
        }
        if method == "eth_sendRawTransaction" {
            self.broadcasts.fetch_add(1, Ordering::SeqCst);
        }
        if method == "debug_traceTransaction" {
            self.traces.fetch_add(1, Ordering::SeqCst);
        }
        Ok(v["result"].clone())
    }
}
async fn node(chain: u64) -> (Node, Arc<Rpc>, String) {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let child = std::process::Command::new("anvil")
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--chain-id",
            &chain.to_string(),
            "--silent",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("owned Anvil required");
    let node = Node(child);
    let rpc = Arc::new(Rpc {
        url: format!("http://127.0.0.1:{port}"),
        client: reqwest::Client::new(),
        broadcasts: AtomicUsize::new(0),
        traces: AtomicUsize::new(0),
    });
    for attempt in 0..100 {
        if rpc.request("eth_chainId", json!([])).await.is_ok() {
            break;
        }
        assert!(attempt < 99, "Anvil did not start");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let filler = rpc.request("eth_accounts", json!([])).await.unwrap()[0]
        .as_str()
        .unwrap()
        .to_owned();
    for address in [INPUT, OUTPUT, REACTOR] {
        rpc.request(
            "anvil_setCode",
            json!([
                address,
                format!("0x{}", include_str!("fixtures/uniswap/runtime.hex").trim())
            ]),
        )
        .await
        .unwrap();
    }
    rpc.request("anvil_setBalance", json!([owner(), "0x8ac7230489e80000"]))
        .await
        .unwrap();
    for (token, to) in [(INPUT, owner()), (OUTPUT, REACTOR.into())] {
        let call = mintCall {
            to: addr(&to),
            amount: U256::from(10_000_000),
        }
        .abi_encode();
        rpc.request(
            "eth_sendTransaction",
            json!([{"from":filler,"to":token,"data":hex_bytes(&call),"gas":"0x186a0"}]),
        )
        .await
        .unwrap();
    }
    (node, rpc, filler)
}
struct Prices;
#[async_trait]
impl UsdPriceSource for Prices {
    async fn quote(&self, asset: &AssetId) -> Result<UsdQuote> {
        Ok(UsdQuote {
            usd_per_unit: if asset.reference == "native" {
                "3000"
            } else {
                "1"
            }
            .into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic-owned-anvil-prices".into(),
        })
    }
}
#[async_trait]
impl WalletValuation for Prices {
    async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
        Ok((
            units.mul_div_ceil(
                &Quantity::from_u64(if asset.reference == "native" {
                    3_000_000_000
                } else {
                    1_000_000
                }),
                &Quantity::new(format!("1{}", "0".repeat(asset.decimals as usize)))?,
            )?,
            "synthetic-owned-anvil-prices".into(),
        ))
    }
}
#[derive(Default)]
struct Approval(AtomicUsize);
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "automatic isolated Anvil test approval".into(),
            source: None,
            yolo_provenance: None,
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
struct Provider {
    rpc: Arc<Rpc>,
    chain: u64,
    routing: &'static str,
    deadline: u64,
    filler: String,
    fill: Mutex<BTreeMap<String, String>>,
    orders: AtomicUsize,
    swaps: AtomicUsize,
    store_path: std::path::PathBuf,
}
impl Provider {
    fn permit(&self) -> Value {
        let domain = json!({"name":"Permit2","chainId":self.chain,"verifyingContract":PERMIT2});
        if self.routing == "CLASSIC" {
            json!({"domain":domain,"types":{"PermitSingle":[{"name":"details","type":"PermitDetails"},{"name":"spender","type":"address"},{"name":"sigDeadline","type":"uint256"}],"PermitDetails":[{"name":"token","type":"address"},{"name":"amount","type":"uint160"},{"name":"expiration","type":"uint48"},{"name":"nonce","type":"uint48"}]},"values":{"details":{"token":INPUT,"amount":"1000000","expiration":self.deadline,"nonce":"7"},"spender":REACTOR,"sigDeadline":self.deadline.to_string()}})
        } else {
            json!({"domain":domain,"types":{"PermitWitnessTransferFrom":[{"name":"permitted","type":"TokenPermissions"},{"name":"spender","type":"address"},{"name":"nonce","type":"uint256"},{"name":"deadline","type":"uint256"},{"name":"witness","type":"Order"}],"TokenPermissions":[{"name":"token","type":"address"},{"name":"amount","type":"uint256"}],"Order":[{"name":"recipient","type":"address"},{"name":"minimumOutput","type":"uint256"}]},"values":{"permitted":{"token":INPUT,"amount":"1000000"},"spender":REACTOR,"nonce":"7","deadline":self.deadline.to_string(),"witness":{"recipient":owner(),"minimumOutput":"990000"}}})
        }
    }
    fn quote(&self, body: &Value) -> Value {
        let mut q = json!({"routing":self.routing,"quote":{"quoteId":"synthetic-anvil-quote","chainId":self.chain,"swapper":owner(),"tradeType":"EXACT_INPUT","input":{"token":INPUT,"amount":"1000000","maximumAmount":"1000000"},"output":{"token":OUTPUT,"recipient":owner(),"amount":"1000000","minimumAmount":"990000"},"slippage":1,"slippageTolerance":1,"priceImpact":0,"gasFee":"100000000000000"},"permitData":self.permit()});
        if self.routing != "CLASSIC" {
            q["quote"]["orderId"] = json!(order_hash());
            q["quote"]["orderInfo"] = json!({"chainId":self.chain,"swapper":owner(),"reactor":REACTOR,"nonce":"7","deadline":self.deadline,"input":{"token":INPUT,"startAmount":"1000000","endAmount":"1000000","maxAmount":"1000000"},"outputs":[{"token":OUTPUT,"recipient":owner(),"startAmount":"1000000","endAmount":"990000","minAmount":"990000"}]});
        }
        let input = body["tokenIn"].as_str().unwrap();
        let output = body["tokenOut"].as_str().unwrap();
        let amount: u64 = body["amount"].as_str().unwrap().parse().unwrap();
        let output_amount = quoted_output(input, output, amount);
        let minimum = output_amount * 99 / 100;
        let nonce = if input == INPUT { 7 } else { 8 };
        let id = if input == INPUT {
            order_hash()
        } else {
            format!("0x{}", "ac".repeat(32))
        };
        q["quote"]["input"] =
            json!({"token":input,"amount":amount.to_string(),"maximumAmount":amount.to_string()});
        q["quote"]["output"] = json!({"token":output,"recipient":owner(),"amount":output_amount.to_string(),"minimumAmount":minimum.to_string()});
        q["quote"]["quoteId"] = json!(format!("synthetic-{nonce}"));
        if self.routing == "CLASSIC" {
            q["permitData"]["values"]["details"]["token"] = json!(input);
            q["permitData"]["values"]["details"]["amount"] = json!(amount.to_string());
            q["permitData"]["values"]["details"]["nonce"] = json!(nonce.to_string());
        } else {
            q["quote"]["orderId"] = json!(id);
            q["quote"]["orderInfo"]["nonce"] = json!(nonce.to_string());
            q["quote"]["orderInfo"]["input"] = json!({"token":input,"startAmount":amount.to_string(),"endAmount":amount.to_string(),"maxAmount":amount.to_string()});
            q["quote"]["orderInfo"]["outputs"] = json!([{"token":output,"recipient":owner(),"startAmount":amount.to_string(),"endAmount":minimum.to_string(),"minAmount":minimum.to_string()}]);
            q["permitData"]["values"]["permitted"] =
                json!({"token":input,"amount":amount.to_string()});
            q["permitData"]["values"]["nonce"] = json!(nonce.to_string());
            q["permitData"]["values"]["witness"]["minimumOutput"] = json!(minimum.to_string());
        }
        if input == NATIVE {
            q["permitData"] = Value::Null;
        }
        // Real provider quotes carry fractional ancillary metadata. It must
        // survive durable financial bindings and return as numbers to the API.
        q["quote"]["priceImpact"] = json!(0.1);
        q["quote"]["priceDifference"] = json!(-0.012345);
        q["quote"]["gasEstimates"] = json!([{"strategy":{"limitInflationFactor":1.15,"maxPriorityFeeGwei":0.1,"baseFeeMultiplier":1.2,"minPriorityFeeRatioOfBaseFee":0.1}}]);
        q
    }
    fn calldata(&self, quote: &Value) -> String {
        let amount: u64 = quote["input"]["amount"].as_str().unwrap().parse().unwrap();
        hex_bytes(
            &exchangeCall {
                tokenIn: addr(quote["input"]["token"].as_str().unwrap()),
                tokenOut: addr(quote["output"]["token"].as_str().unwrap()),
                swapper: addr(&owner()),
                amountIn: U256::from(amount),
                amountOut: U256::from(
                    quoted_output(
                        quote["input"]["token"].as_str().unwrap(),
                        quote["output"]["token"].as_str().unwrap(),
                        amount,
                    ) * 995
                        / 1000,
                ),
                orderHash: if self.routing == "CLASSIC" {
                    B256::ZERO
                } else {
                    quote["orderId"].as_str().unwrap().parse().unwrap()
                },
                nonce: U256::from(if quote["input"]["token"] == INPUT {
                    7
                } else {
                    8
                }),
            }
            .abi_encode(),
        )
    }
}
#[async_trait]
impl UniswapApiTransport for Provider {
    async fn request(&self, request: UniswapApiRequest) -> Result<Value> {
        match request {
            UniswapApiRequest::Post { path, body } => match path.as_str() {
                "/quote" => {
                    assert_eq!(body["permitAmount"], "EXACT");
                    Ok(self.quote(&body))
                }
                "/swap" => {
                    assert_eq!(body["quote"]["priceImpact"], json!(0.1));
                    assert_eq!(
                        body["quote"]["gasEstimates"][0]["strategy"]["limitInflationFactor"],
                        json!(1.15)
                    );
                    if body["quote"]["input"]["token"] != NATIVE {
                        assert_eq!(body["signature"].as_str().unwrap().len(), 132);
                    }
                    let value = if body["quote"]["input"]["token"] == NATIVE {
                        body["quote"]["input"]["amount"].clone()
                    } else {
                        json!("0")
                    };
                    self.swaps.fetch_add(1, Ordering::SeqCst);
                    Ok(
                        json!({"swap":{"chainId":self.chain,"from":owner(),"to":REACTOR,"value":value,"data":self.calldata(&body["quote"]),"gasLimit":"200000"}}),
                    )
                }
                "/order" => {
                    assert_eq!(body["quote"]["priceDifference"], json!(-0.012345));
                    assert_eq!(
                        body["quote"]["gasEstimates"][0]["strategy"]["baseFeeMultiplier"],
                        json!(1.2)
                    );
                    assert_eq!(body["signature"].as_str().unwrap().len(), 132);
                    let store = Store::open(&self.store_path).unwrap();
                    let request = store
                        .by_request(if body["quote"]["input"]["token"] == INPUT {
                            "positive-swap"
                        } else {
                            "positive-return"
                        })
                        .unwrap()
                        .expect("durable request");
                    let record: OperationRecord = serde_json::from_value(
                        store
                            .get_record(&request.operation_id)
                            .unwrap()
                            .expect("durable operation"),
                    )
                    .unwrap();
                    assert_eq!(
                        record.submission.as_ref().unwrap().reference,
                        body["quote"]["orderId"].as_str().unwrap(),
                        "journal must precede provider submission"
                    );
                    self.orders.fetch_add(1, Ordering::SeqCst);
                    let id = body["quote"]["orderId"].as_str().unwrap().to_owned();
                    assert!(
                        !self.fill.lock().unwrap().contains_key(&id),
                        "order must not replay"
                    );
                    let tx=self.rpc.request("eth_sendTransaction",json!([{"from":self.filler,"to":REACTOR,"data":self.calldata(&body["quote"]),"gas":"0x30d40"}])).await?;
                    self.fill
                        .lock()
                        .unwrap()
                        .insert(id.clone(), tx.as_str().unwrap().into());
                    Ok(json!({"orderId":id,"orderStatus":"open"}))
                }
                _ => Err(fail("unexpected fixture API request")),
            },
            UniswapApiRequest::Get { path, query } => {
                assert_eq!(path, "/orders");
                assert_eq!(query.len(), 1);
                assert_eq!(query[0].0, "orderId");
                let id = &query[0].1;
                Ok(
                    json!({"orders":[{"orderId":id,"orderStatus":"filled","txHash":self.fill.lock().unwrap().get(id)}]}),
                )
            }
        }
    }
}
fn runtime(
    path: &Path,
    rpc: Arc<Rpc>,
    provider: Arc<Provider>,
    approval: Arc<Approval>,
) -> Arc<gofin_broker::BrokerRuntime> {
    let signer = Arc::new(LocalEnvelopeSigner::from_private_key(KEY).unwrap());
    let mapped = Arc::new(
        gofin_broker::MappedEnvelopeSigner::new(BTreeMap::from([(
            owner(),
            signer as Arc<dyn gofin_evm::EnvelopeSigner>,
        )]))
        .unwrap(),
    );
    let adapter = EthereumAdapter::new(
        EthereumConfig::wallet_only(provider.chain, 1, vec![INPUT.into(), OUTPUT.into()]),
        vec![WalletConnection {
            name: "synthetic".into(),
            address: owner(),
            signing_enabled: true,
        }],
        Arc::new(EvmRuntime::new(rpc, provider.chain, Some(mapped))),
    )
    .unwrap()
    .with_wallet_valuation(Arc::new(Prices))
    .with_uniswap_api(UniswapApiClient::with_transport(provider));
    // Production routes every EVM operation through this chain-bound wrapper,
    // including provider quotes whose transaction is built only after approval.
    let router = gofin_broker::EvmNetworkRouter::new(BTreeMap::from([(
        adapter.config.chain_id,
        Arc::new(adapter),
    )]));
    compose_runtime_with_clock(
        BrokerConfigV2 {
            version: 2,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy: BrokerPolicy::beta(),
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        RuntimeDependencies {
            store: Store::open(path).unwrap(),
            adapters: vec![Arc::new(router)],
            authorizer: approval,
            valuator: Arc::new(SharedUsdValuator::with_source(Arc::new(Prices))),
            bridge: None,
        },
        Arc::new(gofin_domain::clock::SystemClock),
    )
    .unwrap()
}
fn quoted_output(input: &str, output: &str, amount: u64) -> u64 {
    if input == NATIVE {
        amount / 1_000_000_000 * 3
    } else if output == NATIVE {
        amount / 3 * 1_000_000_000
    } else {
        amount
    }
}
fn decimals(token: &str) -> u8 {
    if token == NATIVE { 18 } else { 6 }
}
async fn balance(rpc: &Rpc, token: &str) -> U256 {
    if token == NATIVE {
        return U256::from_str_radix(
            rpc.request("eth_getBalance", json!([owner(), "latest"]))
                .await
                .unwrap()
                .as_str()
                .unwrap()
                .trim_start_matches("0x"),
            16,
        )
        .unwrap();
    }
    let value=rpc.request("eth_call",json!([{"to":token,"data":hex_bytes(&balanceOfCall{owner:addr(&owner())}.abi_encode())},"latest"])).await.unwrap();
    U256::abi_decode(&gofin_evm::rpc_bytes(&value).unwrap()).unwrap()
}
async fn positive(routing: &'static str, native: bool) {
    let cli = std::env::var("GOFIN_UNISWAP_CLI").expect("public CLI path required");
    assert!(Path::new(&cli).is_file());
    let chain = if routing == "DUTCH_V3" { 42161 } else { 1 };
    let (_node, rpc, filler) = node(chain).await;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("store.sqlite");
    let deadline = chrono::Utc::now().timestamp() as u64 + 300;
    let provider = Arc::new(Provider {
        rpc: rpc.clone(),
        chain,
        routing,
        deadline,
        filler,
        fill: Mutex::new(BTreeMap::new()),
        orders: AtomicUsize::new(0),
        swaps: AtomicUsize::new(0),
        store_path: path.clone(),
    });
    let approval = Arc::new(Approval::default());
    let client = Client::new(
        runtime(&path, rpc.clone(), provider.clone(), approval.clone()),
        temp.path(),
    )
    .await;
    let source = if native { NATIVE } else { INPUT };
    let initial_units = if native {
        1_000_000_000_000_000
    } else {
        1_000_000
    };
    let intermediate_units = quoted_output(source, OUTPUT, initial_units) * 995 / 1000;
    let mut gas_paid = U256::ZERO;
    let initial_source = balance(&rpc, source).await;
    let initial_destination = balance(&rpc, OUTPUT).await;
    for (index, (token_in, token_out, units, id)) in [
        (source, OUTPUT, initial_units, "positive-swap"),
        (OUTPUT, source, intermediate_units, "positive-return"),
    ]
    .into_iter()
    .enumerate()
    {
        let output_units = quoted_output(token_in, token_out, units);
        let minimum = output_units * 99 / 100;
        let received = output_units * 995 / 1000;
        let input = json!({"wallet":"synthetic","network":"ethereum","chain_id":chain,"request_id":id,"token_in":if token_in==NATIVE {"native"}else{token_in},"token_out":if token_out==NATIVE {"native"}else{token_out},"recipient":owner(),"amount":Quantity::from_u64(units).to_decimal(decimals(token_in)),"min_output":Quantity::from_u64(minimum).to_decimal(decimals(token_out)),"max_fee_eth":"0.001","slippage_bps":100,"deadline":deadline,"route_preference":if routing=="CLASSIC"{"amm"}else{"uniswapx"}});
        let before_in = balance(&rpc, token_in).await;
        let before_out = balance(&rpc, token_out).await;
        let preview = client
            .call(
                args(&[
                    "preview",
                    "uniswap.swap",
                    "--input",
                    &input.to_string(),
                    "--json",
                ]),
                BrokerRequest::Preview {
                    operation: "uniswap.swap".into(),
                    input: input.clone(),
                },
                true,
            )
            .await;
        assert_eq!(preview["binding"]["quote"]["routing"], routing);
        assert_eq!(
            rpc.broadcasts.load(Ordering::SeqCst),
            index * usize::from(routing == "CLASSIC")
        );
        assert_eq!(
            provider.orders.load(Ordering::SeqCst),
            index * usize::from(routing != "CLASSIC")
        );
        let execute_args = args(&[
            "execute",
            "uniswap.swap",
            "--input",
            &input.to_string(),
            "--id",
            id,
            "--json",
        ]);
        let request = BrokerRequest::Execute {
            request: OperationRequest {
                request_id: id.into(),
                operation: "uniswap.swap".into(),
                input,
                run_id: None,
            },
        };
        let mut result = client
            .call(execute_args.clone(), request.clone(), true)
            .await;
        let operation_id = result["id"].as_str().unwrap().to_owned();
        for _ in 0..10 {
            if result["status"] == "completed" {
                break;
            }
            // Anvil's finalized head trails latest; mine real confirmations.
            rpc.request("anvil_mine", json!(["0x40"])).await.unwrap();
            result = client
                .call(
                    args(&["status", &operation_id, "--json"]),
                    BrokerRequest::Status {
                        id: operation_id.clone(),
                    },
                    true,
                )
                .await;
        }
        assert_eq!(
            result["status"], "completed",
            "evidence: {}; error: {}",
            result["last_reconciliation_evidence"], result["error"]
        );

        let replay = client.call(execute_args, request, true).await;
        assert_eq!(replay["id"], result["id"]);
        assert_eq!(replay["status"], "completed");
        assert_eq!(approval.0.load(Ordering::SeqCst), index + 1);
        assert_eq!(
            rpc.broadcasts.load(Ordering::SeqCst),
            (index + 1) * usize::from(routing == "CLASSIC")
        );
        assert_eq!(
            provider.orders.load(Ordering::SeqCst),
            (index + 1) * usize::from(routing != "CLASSIC")
        );
        let record: OperationRecord = serde_json::from_value(
            Store::open(&path)
                .unwrap()
                .get_record(&operation_id)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let effects = record
            .effects
            .expect("independently verified settlement effects");
        let fee = effects
            .effects
            .iter()
            .filter_map(|effect| match effect {
                FinancialEffect::Cost { amount, .. } => {
                    Some(U256::from_str_radix(amount.atoms.as_str(), 10).unwrap())
                }
                _ => None,
            })
            .fold(U256::ZERO, |a, b| a + b);
        gas_paid += fee;
        assert_eq!(
            before_in - balance(&rpc, token_in).await,
            U256::from(units) + if token_in == NATIVE { fee } else { U256::ZERO }
        );
        assert_eq!(
            balance(&rpc, token_out).await + if token_out == NATIVE { fee } else { U256::ZERO }
                - before_out,
            U256::from(received)
        );
        assert!(effects.effects.iter().any(|effect|matches!(effect,FinancialEffect::Conversion{spent,received:credited,..} if spent.atoms==Quantity::from_u64(units)&&credited.atoms==Quantity::from_u64(received))));
    }
    assert_eq!(
        balance(&rpc, OUTPUT).await,
        initial_destination,
        "round trip must consume all intermediate output"
    );
    assert_eq!(
        balance(&rpc, source).await,
        initial_source - U256::from(initial_units)
            + U256::from(quoted_output(OUTPUT, source, intermediate_units) * 995 / 1000)
            - if native { gas_paid } else { U256::ZERO },
        "round trip restores source asset with explicit synthetic spread"
    );
    if native {
        assert!(
            rpc.traces.load(Ordering::SeqCst) >= 2,
            "native settlement must use real Anvil callTracer evidence"
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires public gofin-dev CLI and owned Anvil"]
async fn public_cli_uniswap_api_amm_settles() {
    positive("CLASSIC", false).await;
    positive("CLASSIC", true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires public gofin-dev CLI and owned Anvil"]
async fn public_cli_uniswap_api_uniswapx_settles() {
    positive("DUTCH_V2", false).await;
    positive("DUTCH_V3", false).await;
}
