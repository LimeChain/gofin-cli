//! Modeled Base wallet/OP fee receipts. No network access, signing or broadcast.
use super::*;
use gofin_evm::alloy::primitives::keccak256;
const BASE_WETH: &str = "0x4200000000000000000000000000000000000006";
const ORACLE: &str = "0x420000000000000000000000000000000000000f";
const ATTRIBUTES: &str = "0x4200000000000000000000000000000000000015";

struct BaseRpc {
    inner: Arc<Fixture>,
    l1_estimate: AtomicU64,
    l1_actual: Mutex<Option<u64>>,
}
impl BaseRpc {
    fn new() -> Self {
        let inner = Arc::new(Fixture::new());
        inner.chain.store(8453, Ordering::SeqCst);
        Self {
            inner,
            l1_estimate: AtomicU64::new(1000),
            l1_actual: Mutex::new(Some(500)),
        }
    }
}
#[async_trait]
impl EvmRpc for BaseRpc {
    async fn request(&self, method: &str, mut params: Value) -> Result<Value> {
        if method == "eth_call" {
            let to = params[0]["to"].as_str().unwrap_or("").to_owned();
            if to == ORACLE || to == ATTRIBUTES {
                assert_eq!(params[1]["requireCanonical"], true);
                assert!(gofin_evm::hash(params[1]["blockHash"].as_str().unwrap()).is_ok());
                let bytes = gofin_evm::rpc_bytes(&params[0]["data"])?;
                let selector = &bytes[..4];
                let output = if ["isFjord()", "isIsthmus()", "isJovian()"]
                    .iter()
                    .any(|sig| selector == &keccak256(sig.as_bytes())[..4])
                {
                    U256::from(1)
                } else if selector == &keccak256("getL1FeeUpperBound(uint256)")[..4] {
                    U256::from(self.l1_estimate.load(Ordering::SeqCst))
                } else if selector == &keccak256("getOperatorFee(uint256)")[..4] {
                    U256::from_be_slice(&bytes[4..36]) * U256::from(200) + U256::from(7)
                } else if selector == &keccak256("operatorFeeScalar()")[..4] {
                    U256::from(2)
                } else if selector == &keccak256("operatorFeeConstant()")[..4] {
                    U256::from(7)
                } else {
                    panic!("Unexpected Base oracle selector")
                };
                return Ok(abi_value(output));
            }
            if to == BASE_WETH {
                params[0]["to"] = json!(lower_address(addr(100)));
            }
        }
        let mut result = self.inner.request(method, params).await?;
        if method == "eth_getBlockByNumber" {
            result["extraData"] = json!(format!("0x01{}", "00".repeat(16)));
        }
        if method == "eth_getTransactionReceipt" && !result.is_null() {
            if let Some(l1) = *self.l1_actual.lock().unwrap() {
                result["l1Fee"] = json!(format!("{l1:#x}"));
            }
            result["operatorFeeScalar"] = json!("0x2");
            result["operatorFeeConstant"] = json!("0x7");
        }
        Ok(result)
    }
}
struct Prices;
#[async_trait]
impl WalletValuation for Prices {
    async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
        assert_eq!(asset.network, "ethereum:8453");
        Ok((
            units.mul_div_ceil(
                &Quantity::from_u64(3_000_000_000),
                &Quantity::new(pow10(asset.decimals)?.to_string()).unwrap(),
            )?,
            "SYNTHETIC Base exact-identity prices".into(),
        ))
    }
}
fn base_adapter(rpc: Arc<BaseRpc>) -> EthereumAdapter {
    EthereumAdapter::new(
        EthereumConfig::wallet_only(8453, 1, vec![]),
        vec![WalletConnection {
            name: "synthetic".into(),
            address: lower_address(addr(1)),
            signing_enabled: true,
        }],
        Arc::new(EvmRuntime::new(rpc, 8453, None)),
    )
    .unwrap()
    .with_wallet_valuation(Arc::new(Prices))
}
fn base_input() -> Value {
    json!({"wallet":"synthetic","network":"evm","chain_id":8453,"request_id":"SYNTHETIC-Base-fee","max_fee_eth":"0.001","amount":"0.001"})
}

#[tokio::test]
async fn base_native_and_erc20_transfers_settle_at_one_full_block_with_total_fee() {
    for token in ["native".to_owned(), lower_address(addr(101))] {
        let rpc = Arc::new(BaseRpc::new());
        let adapter = base_adapter(rpc.clone());
        let mut input = base_input();
        input["token"] = json!(token);
        input["recipient"] = json!(lower_address(addr(2)));
        if token != "native" {
            input["amount"] = json!("1");
        }
        let prepared = adapter.prepare("wallet.transfer", &input).await.unwrap();
        let binding: Binding = serde_json::from_value(prepared.binding.clone()).unwrap();
        assert_eq!(binding.token.network, "ethereum:8453");
        assert!(binding.transaction.rollup_fee.is_some());
        assert_eq!(
            prepared.intent.maximum_costs[0].amount.atoms,
            quantity(binding.transaction.maximum_cost().unwrap())
        );
        assert!(binding.transaction.maximum_cost().unwrap() <= exact("0.001", 18).unwrap());
        let logs = if token == "native" {
            vec![]
        } else {
            vec![event(
                addr(101),
                Erc20::Transfer {
                    from: addr(1),
                    to: addr(2),
                    value: gofin_evm::u256(&binding.units).unwrap(),
                },
                21,
            )]
        };
        rpc.inner.latest.store(21, Ordering::SeqCst);
        rpc.inner.set_receipt(&binding, 21, logs);
        let original = record(prepared);
        let done = adapter.reconcile(&original).await.unwrap();
        assert_eq!(done.status, Status::Completed);
        assert_eq!(done.evidence["confirmations"], 1);
        assert_eq!(done.evidence["finality"], "sequencer-inclusion");
        let verified = done.verified.unwrap();
        assert_eq!(Some(&verified.submission), original.submission.as_ref());
        let fee = U256::from(65536) * U256::from(10_000_000)
            + U256::from(500)
            + U256::from(65536 * 200 + 7);
        assert!(verified.effects.iter().any(|effect|matches!(effect,FinancialEffect::Cost{amount,..} if amount.atoms==quantity(fee))));
        assert!(
            !rpc.inner
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|method| method == "eth_sendRawTransaction")
        );
        rpc.inner.receipt.lock().unwrap()["blockHash"] = json!(blockhash(99));
        assert_ne!(
            adapter.reconcile(&original).await.unwrap().status,
            Status::Completed
        );
    }
}

#[tokio::test]
async fn base_wrap_unwrap_and_allowance_reads_use_reviewed_contract_identity() {
    for operation in ["wallet.wrap", "wallet.unwrap"] {
        let rpc = Arc::new(BaseRpc::new());
        let adapter = base_adapter(rpc.clone());
        let prepared = adapter.prepare(operation, &base_input()).await.unwrap();
        let binding: Binding = serde_json::from_value(prepared.binding.clone()).unwrap();
        assert_eq!(
            binding.token.reference,
            if operation == "wallet.wrap" {
                "native"
            } else {
                BASE_WETH
            }
        );
        assert_eq!(binding.transaction.to, BASE_WETH);
        let units = gofin_evm::u256(&binding.units).unwrap();
        let logs = if operation == "wallet.wrap" {
            vec![event(
                address(BASE_WETH).unwrap(),
                Weth::Deposit {
                    dst: addr(1),
                    wad: units,
                },
                21,
            )]
        } else {
            vec![event(
                address(BASE_WETH).unwrap(),
                Weth::Withdrawal {
                    src: addr(1),
                    wad: units,
                },
                21,
            )]
        };
        rpc.inner.latest.store(21, Ordering::SeqCst);
        rpc.inner.set_receipt(&binding, 21, logs);
        assert_eq!(
            adapter.reconcile(&record(prepared)).await.unwrap().status,
            Status::Completed
        );
    }
    let rpc = Arc::new(BaseRpc::new());
    let adapter = base_adapter(rpc);
    let allowance=adapter.read("wallet.allowance",&json!({"wallet":"synthetic","network":"evm","chain_id":8453,"token":lower_address(addr(101)),"spender":lower_address(addr(300))})).await.unwrap();
    assert_eq!(allowance["chain_id"], 8453);
    assert!(!allowance.is_null());
}

#[tokio::test]
async fn base_receipt_missing_fee_evidence_and_fee_rise_never_settle_or_dispatch() {
    let rpc = Arc::new(BaseRpc::new());
    let adapter = base_adapter(rpc.clone());
    let mut input = base_input();
    input["token"] = json!("native");
    input["recipient"] = json!(lower_address(addr(2)));
    let prepared = adapter.prepare("wallet.transfer", &input).await.unwrap();
    let binding: Binding = serde_json::from_value(prepared.binding.clone()).unwrap();
    rpc.inner.latest.store(21, Ordering::SeqCst);
    rpc.inner.set_receipt(&binding, 21, vec![]);
    *rpc.l1_actual.lock().unwrap() = None;
    assert!(adapter.reconcile(&record(prepared)).await.is_err());
    rpc.l1_estimate.store(2000, Ordering::SeqCst);
    assert_eq!(
        adapter
            .runtime
            .revalidate(&binding.transaction)
            .await
            .unwrap_err()
            .code,
        "FEE_BOUND_CHANGED"
    );
    assert!(
        !rpc.inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "eth_sendRawTransaction")
    );
}
