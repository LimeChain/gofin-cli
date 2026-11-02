// Current Funding API rehearsal: real adapters and CLI, isolated venue/RPC transports.
use super::*;
use gofin_evm::EvmRpc;

const METHOD: &str = "00000000-0000-0000-0000-000000000004";
const ADDRESS_ID: &str = "ABTEST1-TEST1-TEST01";
const DEPOSIT_ADDRESS: &str = "0x1111111111111111111111111111111111111111";
fn payout_hash() -> String {
    format!("0x{}", "aa".repeat(32))
}

struct FundingRpc {
    inner: Arc<Rpc>,
    wallet: String,
    payout_available: AtomicBool,
}
#[async_trait]
impl EvmRpc for FundingRpc {
    async fn request(&self, method: &str, p: Value) -> Result<Value> {
        if method == "eth_call" && p[0]["to"] == usdc_fixture::TOKEN {
            let data = p[0]["data"]
                .as_str()
                .or_else(|| p[0]["input"].as_str())
                .unwrap_or("");
            if data.starts_with("0x70a08231") {
                let funded = self.payout_available.load(Ordering::SeqCst)
                    && self.inner.submitted_hash.lock().unwrap().is_none();
                return Ok(json!(format!(
                    "0x{:064x}",
                    if funded { 5_000_000u64 } else { 0 }
                )));
            }
        }
        if p[0] == payout_hash() {
            if !self.payout_available.load(Ordering::SeqCst) {
                return Ok(Value::Null);
            }
            let block_hash = format!("0x{}", "cd".repeat(32));
            if method == "eth_getTransactionReceipt" {
                return Ok(
                    json!({"transactionHash":payout_hash(),"blockHash":block_hash,"blockNumber":"0x64","status":"0x1","gasUsed":"0x5208","effectiveGasPrice":"0x3b9aca00","logs":[{"address":usdc_fixture::TOKEN,"topics":["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",format!("0x{}{}","0".repeat(24),"33".repeat(20)),format!("0x{}{}","0".repeat(24),self.wallet.trim_start_matches("0x"))],"data":format!("0x{:064x}",5_000_000u64),"transactionHash":payout_hash(),"blockHash":block_hash,"blockNumber":"0x64","logIndex":"0x0","removed":false}]}),
                );
            }
            if method == "eth_getTransactionByHash" {
                return Ok(
                    json!({"hash":payout_hash(),"blockHash":block_hash,"blockNumber":"0x64","chainId":"0x1","from":format!("0x{}","33".repeat(20)),"to":usdc_fixture::TOKEN,"value":"0x0"}),
                );
            }
        }
        let mut result = self.inner.request(method, p).await?;
        if method == "eth_getTransactionReceipt" && !result.is_null() {
            result["logs"][0]["data"] = json!(format!("0x{:064x}", 5_000_000u64));
        }
        Ok(result)
    }
}
struct FundingWire {
    base: Arc<Venue>,
    rpc: Arc<FundingRpc>,
    ethereum: Arc<gofin_ethereum::EthereumAdapter>,
    withdrawn: AtomicBool,
    credited: AtomicBool,
    writes: AtomicUsize,
    withdrawal_quotes: AtomicUsize,
}
#[async_trait]
impl KrakenTransport for FundingWire {
    async fn request(
        &self,
        e: E,
        account: Option<&KrakenAccount>,
        p: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e.mutating() {
            assert!(guard.as_ref().is_some_and(|g| g()));
        } else {
            assert!(guard.is_none());
        }
        let withdrawn = self.withdrawn.load(Ordering::SeqCst);
        let credited = self.credited.load(Ordering::SeqCst);
        let balance = if credited {
            "5"
        } else if withdrawn {
            "0"
        } else {
            "6"
        };
        let result = match e {
            E::KrakenPrivatePostGetApiKeyInfo => {
                json!({"permissions":["query-funds","withdraw-funds","query-ledger"],"iban":"SYNTHETIC"})
            }
            E::KrakenFundingGetFundingV1MethodsDirection if p["direction"] == "withdraw" => {
                json!({"methods":[{"method_id":METHOD,"method_name":"USDC - Ethereum","asset":{"class":"currency","name":"USDC"},"network":{"network_id":"00000000-0000-0000-0000-000000000002","network_name":"Ethereum","contract_address":usdc_fixture::TOKEN},"minimum_amount":"2","maximum_amount":"20","fees":{"base":{"asset":{"class":"currency","name":"USDC"},"amount":"1"},"included":true}}]})
            }
            E::KrakenFundingGetFundingV1Addresses => {
                json!({"addresses":[{"address_id":ADDRESS_ID,"scope":{"method_id":METHOD},"verified":true,"address_details":{"crypto":{"address":self.rpc.wallet}}}]})
            }
            E::KrakenFundingGetFundingV1FeesMethodId if p["method_id"] == METHOD => {
                self.withdrawal_quotes.fetch_add(1, Ordering::SeqCst);
                json!({"fee":{"asset":{"class":"currency","name":"USDC"},"amount":"1"},"gross_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"6"},"net_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"5"}})
            }
            E::KrakenFundingGetFundingV1FeesMethodId => {
                json!({"fee":{"asset":{"class":"currency","name":"USDC"},"amount":"0"},"gross_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"5"},"net_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"5"}})
            }
            E::KrakenFundingGetFundingV1LimitsWithdrawalAssetClassAsset => {
                json!({"withdrawal_limits":[{"method_id":METHOD,"maximum_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"20"}}]})
            }
            E::KrakenPrivatePostBalanceEx => json!({"USDC":{"balance":balance,"hold_trade":"0"}}),
            E::KrakenPrivatePostBalance => json!({"USDC":balance}),
            E::KrakenFundingPostFundingV1Withdrawals => {
                assert_eq!(p["scope"]["method_id"], METHOD);
                assert_eq!(p["address_id"], ADDRESS_ID);
                assert_eq!(p["expected_address"], self.rpc.wallet);
                assert_eq!(p["amount"]["asset_amount"]["asset"]["name"], "USDC");
                assert_eq!(p["amount"]["asset_amount"]["amount"], "6");
                assert_eq!(p["fee"]["fee_included"], true);
                assert_eq!(
                    p["fee"]["current_fee"]["max_fee"]["asset_amount"]["amount"],
                    "1"
                );
                assert!(!self.withdrawn.swap(true, Ordering::SeqCst));
                self.writes.fetch_add(1, Ordering::SeqCst);
                json!({"withdrawal_id":"FUNDING-WITHDRAW-1","gross_amount":{"asset_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"6"}},"net_amount":{"asset_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"5"}},"fee":{"asset_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"1"}}})
            }
            E::KrakenFundingGetFundingV1Withdrawals => {
                json!({"withdrawals":if withdrawn {json!([{"withdrawal_id":"FUNDING-WITHDRAW-1","status":"success","address_id":ADDRESS_ID,"method_id":METHOD,"amount":{"asset":{"class":"currency","name":"USDC"},"amount":"5"},"fee":{"asset":{"class":"currency","name":"USDC"},"amount":"1"},"onchain_transaction":payout_hash()}])}else{json!([])}})
            }
            E::KrakenPrivatePostDepositStatus => {
                if self.base.rpc.submitted_hash.lock().unwrap().is_none() {
                    json!([])
                } else {
                    json!([{"asset":"USDC","method":"USDC - Ethereum (Unified)","refid":"fixture-deposit-ref","txid":self.base.rpc.hash(),"info":DEPOSIT_ADDRESS,"amount":"5","fee":"0","status":if credited{"Success"}else{"Settled"}}])
                }
            }
            E::KrakenPrivatePostLedgers => {
                let mut rows = serde_json::Map::new();
                if withdrawn {
                    rows.insert("withdrawal-ledger".into(),json!({"refid":"FUNDING-WITHDRAW-1","type":"withdrawal","asset":"USDC","amount":"-5","fee":"1","balance":"0"}));
                }
                if credited {
                    rows.insert("deposit-ledger".into(),json!({"refid":"fixture-deposit-ref","type":"deposit","asset":"USDC","amount":"5","fee":"0","balance":"5"}));
                }
                json!({"ledger":rows})
            }
            _ => return self.base.request(e, account, p, guard).await,
        };
        Ok(KrakenResponse::Json(json!({"error":[],"result":result})))
    }
}
#[async_trait]
impl KrakenRecipientVerifier for FundingWire {
    async fn resolve(
        &self,
        network: &str,
        coin: &str,
        contract: Option<&str>,
        address: &str,
        selector: Option<&str>,
    ) -> Result<Option<KrakenDestination>> {
        assert_eq!((network, coin), ("Ethereum", "USDC"));
        assert_eq!(contract, Some(usdc_fixture::TOKEN));
        assert_eq!(address, self.rpc.wallet);
        assert_eq!(selector, Some("fixture-wallet"));
        Ok(Some(KrakenDestination {
            enrolled: true,
            resource: ResourceId(format!("ethereum:1:{}", self.rpc.wallet)),
            address: self.rpc.wallet.clone(),
            chain_id: 1,
            network: None,
            cluster: None,
            mint: None,
            asset: AssetId {
                network: "ethereum:1".into(),
                reference: usdc_fixture::TOKEN.into(),
                decimals: 6,
            },
            confirmations: 2,
        }))
    }
    async fn verify(&self, r: KrakenPayoutRequest) -> Result<KrakenPayoutResult> {
        assert_eq!(r.tx_hash, payout_hash());
        assert_eq!((&*r.minimum, &*r.maximum), ("5", "5"));
        let proof = self
            .ethereum
            .verify_external_credit(
                &r.tx_hash,
                &r.destination.address,
                &r.destination.asset,
                &r.minimum,
                &r.maximum,
                r.destination.confirmations,
            )
            .await?;
        Ok(KrakenPayoutResult {
            status: proof.status,
            amount: proof.amount_atoms,
            evidence: proof.evidence,
        })
    }
}
async fn call_execute(client: &Client, op: &str, input: Value) -> Value {
    let id = input["requestId"].as_str().unwrap().to_owned();
    let preview = client
        .call(
            args(&["preview", op, "--input", &input.to_string(), "--json"]),
            BrokerRequest::Preview {
                operation: op.into(),
                input: input.clone(),
            },
            true,
        )
        .await;
    #[cfg(target_os = "macos")]
    native_approval::assert_prepared_renders(&serde_json::from_value(preview).unwrap());
    client
        .call(
            args(&[
                "execute",
                op,
                "--input",
                &input.to_string(),
                "--id",
                &id,
                "--json",
            ]),
            BrokerRequest::Execute {
                request: OperationRequest {
                    request_id: id,
                    operation: op.into(),
                    input,
                    run_id: None,
                },
            },
            true,
        )
        .await
}
async fn status(client: &Client, record: &Value) -> Value {
    let id = record["id"].as_str().unwrap();
    client
        .call(
            args(&["status", id, "--json"]),
            BrokerRequest::Status { id: id.into() },
            true,
        )
        .await
}
#[test]
#[ignore = "requires GOFIN_KRAKEN_PLAN_CLI with synthetic-kraken-build"]
fn funding_v1_usdc_included_fee_roundtrip_via_public_cli() {
    let binary =
        std::env::var_os("GOFIN_KRAKEN_PLAN_CLI").expect("actual public CLI subprocess required");
    assert!(std::path::Path::new(&binary).is_file());
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(rehearse())
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn rehearse() {
    let temp = tempfile::tempdir().unwrap();
    let (mut deposit, venue, signer) = submission_tests::signed_adapter(None);
    venue.usdc_evm.store(true, Ordering::SeqCst);
    venue.rpc.usdc_evm.store(true, Ordering::SeqCst);
    let ethereum = Arc::get_mut(deposit.ethereum.get_mut(&1).unwrap()).unwrap();
    let rpc = Arc::new(FundingRpc {
        inner: venue.rpc.clone(),
        wallet: ethereum.wallets[0].address.clone(),
        payout_available: AtomicBool::new(false),
    });
    ethereum.runtime = Arc::new(gofin_evm::EvmRuntime::new(rpc.clone(), 1, Some(signer)));
    let ethereum = deposit.ethereum[&1].clone();
    let wire = Arc::new(FundingWire {
        base: venue.clone(),
        rpc: rpc.clone(),
        ethereum,
        withdrawn: AtomicBool::new(false),
        credited: AtomicBool::new(false),
        writes: AtomicUsize::new(0),
        withdrawal_quotes: AtomicUsize::new(0),
    });
    let prices: Arc<dyn gofin_pricing::UsdPriceSource> = Arc::new(PositivePrices);
    let profile = KrakenProfile::new(
        KrakenProduct::Spot,
        vec![KrakenAccount {
            name: "fixture-spot".into(),
            resource: ResourceId("kraken:fixture-spot".into()),
            credential_ref: "synthetic-credential-only".into(),
            physical_identity: "synthetic-uid-only".into(),
            wallet_account_id: None,
            default_wallet_account_id: None,
            identity_verified: true,
            product: KrakenProduct::Spot,
        }],
        wire.clone(),
    )
    .unwrap()
    .with_prices(prices.clone())
    .with_recipient_verifier(wire.clone());
    deposit.kraken = Arc::new(profile.clone());
    let approval = Arc::new(PositiveApproval(AtomicUsize::new(0)));
    let runtime = crate::compose_runtime_with_clock(
        crate::BrokerConfigV2 {
            version: 2,
            networks: crate::NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy: crate::BrokerPolicy::beta(),
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        crate::RuntimeDependencies {
            store: gofin_store::Store::open(temp.path().join("funding.sqlite")).unwrap(),
            adapters: vec![Arc::new(deposit), Arc::new(ExchangeAdapter::new(profile))],
            authorizer: approval.clone(),
            valuator: Arc::new(crate::SharedUsdValuator::with_source(prices)),
            bridge: None,
        },
        Arc::new(gofin_domain::clock::SystemClock),
    )
    .unwrap();
    let client = Client::new(runtime, temp.path()).await;
    let input = json!({"requestId":"funding-v1-withdraw","account":"fixture-spot","scope":{"method_id":METHOD},"address_id":ADDRESS_ID,"expected_address":rpc.wallet,"amount":{"asset_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"6"}},"fee":{"fee_included":true,"current_fee":{"max_fee":{"asset_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"1"}}}},"maxFee":"1","destination":"fixture-wallet","verificationRequirement":"destination_receipt"});
    let pending = call_execute(&client, "kraken.create-funding-withdrawal-v1", input).await;
    assert_eq!(
        wire.withdrawal_quotes.load(Ordering::SeqCst),
        3,
        "one explicit preview, initial execution preparation, and one locked admission refresh"
    );
    assert_ne!(
        pending["outcome"]["phase"], "done",
        "venue success without destination receipt cannot settle: {pending}"
    );
    assert!(pending["submission"].is_object());
    assert_eq!(wire.writes.load(Ordering::SeqCst), 1);
    let waiting = status(&client, &pending).await;
    assert_eq!(waiting["submission"], pending["submission"]);
    assert_ne!(waiting["outcome"]["phase"], "done");
    rpc.payout_available.store(true, Ordering::SeqCst);
    let done = status(&client, &pending).await;
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(done["outcome"]["phase"], "done");
    assert_eq!(done["submission"], pending["submission"]);
    let effects = done["effects"]["effects"].as_array().unwrap();
    for (kind, amount) in [("cash_debit", "6"), ("cost", "1")] {
        assert!(
            effects.iter().any(|e| e["kind"] == kind
                && e["amount"]["atoms"] == Quantity::from_decimal(amount, 36).unwrap().as_str()),
            "{done}"
        );
    }
    assert!(
        effects
            .iter()
            .any(|e| e["kind"] == "usable_credit" && e["amount"]["atoms"] == "5000000"),
        "{done}"
    );
    let mut deposit_input = super::input();
    deposit_input["requestId"] = json!("funding-v1-return");
    deposit_input["amount"] = json!("5");
    deposit_input["address"] = json!(DEPOSIT_ADDRESS);
    let returned = call_execute(&client, "kraken.deposit", deposit_input).await;
    assert_ne!(
        returned["outcome"]["phase"], "done",
        "chain inclusion is not available venue credit: {returned}"
    );
    wire.credited.store(true, Ordering::SeqCst);
    let complete = status(&client, &returned).await;
    assert_eq!(complete["status"], "completed", "{complete}");
    assert_eq!(complete["outcome"]["phase"], "done");
    assert_eq!(complete["submission"], returned["submission"]);
    assert!(
        complete["effects"]["effects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "transfer"
                && e["to"] == "kraken:fixture-spot"
                && e["debit"]["atoms"] == "5000000"
                && e["credit"]["atoms"] == Quantity::from_decimal("5", 36).unwrap().as_str()),
        "{complete}"
    );
    assert_eq!(approval.0.load(Ordering::SeqCst), 2);
    assert_eq!(wire.writes.load(Ordering::SeqCst), 1);
    assert_eq!(
        venue
            .rpc
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.as_str() == "eth_sendRawTransaction")
            .count(),
        1
    );
    assert_ne!(
        done["submission"]["reference"],
        complete["submission"]["reference"]
    );
    let balance_input = json!({"account":"fixture-spot"});
    let balance = client
        .call(
            args(&[
                "read",
                "kraken.get-extended-balance",
                "--input",
                &balance_input.to_string(),
                "--json",
            ]),
            BrokerRequest::Read {
                operation: "kraken.get-extended-balance".into(),
                input: balance_input,
            },
            true,
        )
        .await;
    assert_eq!(
        balance["data"]["result"]["USDC"]["balance"], "5",
        "{balance}"
    );
}
