//! Synthetic permanent portfolio conversion. No secrets, RPCs or venue access.
use async_trait::async_trait;
use gofin_binance::*;
use gofin_domain::*;
use gofin_engine::{Engine, EngineOptions};
use gofin_store::Store;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

fn units(s: &str) -> Quantity {
    Quantity::from_decimal(s, 18).unwrap()
}
fn add(a: &str, b: &str) -> String {
    units(a).checked_add(&units(b)).unwrap().to_decimal(18)
}
fn sub(a: &str, b: &str) -> String {
    units(a).checked_sub(&units(b)).unwrap().to_decimal(18)
}
struct Venue {
    balances: Mutex<BTreeMap<(String, String), String>>,
    orders: Mutex<BTreeMap<String, Value>>,
    trades: Mutex<BTreeMap<String, Value>>,
    transfer: Mutex<Vec<Value>>,
    withdrawal: Mutex<Vec<Value>>,
    posts: AtomicUsize,
    lost: AtomicBool,
    missing_fills: AtomicBool,
    bad_fee: AtomicBool,
    discount: AtomicBool,
}
impl Venue {
    fn new() -> Self {
        Self {
            balances: Mutex::new(BTreeMap::from([
                (("sub".into(), "APT".into()), "2".into()),
                (("sub".into(), "SEI".into()), "10".into()),
                (("main".into(), "ETH".into()), "0.001".into()),
            ])),
            orders: Mutex::new(BTreeMap::new()),
            trades: Mutex::new(BTreeMap::new()),
            transfer: Mutex::new(vec![]),
            withdrawal: Mutex::new(vec![]),
            posts: AtomicUsize::new(0),
            lost: AtomicBool::new(false),
            missing_fills: AtomicBool::new(false),
            bad_fee: AtomicBool::new(false),
            discount: AtomicBool::new(false),
        }
    }
    fn free(&self, who: &str, coin: &str) -> String {
        self.balances
            .lock()
            .unwrap()
            .get(&(who.into(), coin.into()))
            .cloned()
            .unwrap_or("0".into())
    }
    fn change(&self, who: &str, coin: &str, delta: &str, increase: bool) {
        let before = self.free(who, coin);
        self.balances.lock().unwrap().insert(
            (who.into(), coin.into()),
            if increase {
                add(&before, delta)
            } else {
                sub(&before, delta)
            },
        );
    }
    fn balance_rows(&self, who: &str) -> Value {
        json!({"uid":if who=="main"{1}else{2},"accountType":"SPOT","balances":(["ETH","APT","SEI","USDC"].map(|coin|json!({"asset":coin,"free":self.free(who,coin),"locked":"0"})))})
    }
}
#[async_trait]
impl BinanceTransport for Venue {
    async fn request(
        &self,
        credential: &CredentialHandle,
        r: BinanceRequest,
    ) -> std::result::Result<Value, TransportError> {
        r.validate().unwrap();
        let who = if credential.as_str() == "synthetic-main" {
            "main"
        } else {
            "sub"
        };
        let p = |key| r.get(key).unwrap();
        use BinanceEndpoint as E;
        Ok(match r.endpoint {
            E::Account => self.balance_rows(who),
            E::SubAccountSpotAssets => self.balance_rows("sub"),
            E::ApiRestrictions => {
                json!({"enableSpotAndMarginTrading":true,"enableInternalTransfer":true,"permitsUniversalTransfer":true,"enableWithdrawals":true})
            }
            E::SubAccountList => {
                json!({"subAccounts":[{"email":"sub@example.invalid","isFreeze":false,"subAccountId":2}]})
            }
            E::ExchangeInfo => {
                let base = p("symbol").strip_suffix("USDC").unwrap();
                let ceiling = if base == "ETH" {
                    "2000"
                } else if base == "APT" {
                    "10"
                } else {
                    "1"
                };
                json!({"symbols":[{"symbol":p("symbol"),"status":"TRADING","isSpotTradingAllowed":true,"orderTypes":["LIMIT"],"baseAsset":base,"quoteAsset":"USDC","baseCommissionPrecision":8,"quoteCommissionPrecision":8,"filters":[{"filterType":"PRICE_FILTER","minPrice":"0.001","maxPrice":ceiling,"tickSize":"0.001"},{"filterType":"LOT_SIZE","minQty":"0.001","maxQty":"1000","stepSize":"0.001"},{"filterType":"MIN_NOTIONAL","minNotional":"1"}]}]})
            }
            E::SpotCommission => {
                json!({"symbol":p("symbol"),"standardCommission":{"taker":"0.001","buyer":"0","seller":"0"},"specialCommission":{"taker":"0","buyer":"0","seller":"0"},"taxCommission":{"taker":"0","buyer":"0","seller":"0"},"discount":{"enabledForAccount":self.discount.load(Ordering::SeqCst),"enabledForSymbol":true}})
            }
            E::SpotPlaceOrder => {
                let id = p("newClientOrderId").to_owned();
                assert!(!self.orders.lock().unwrap().contains_key(&id));
                let n = self.posts.fetch_add(1, Ordering::SeqCst) + 1;
                let base = p("symbol").strip_suffix("USDC").unwrap();
                let (quote, fee) = match base {
                    "APT" => ("20", "0.02"),
                    "SEI" => ("10", "0.01"),
                    "ETH" => ("20", "0.00001"),
                    _ => panic!(),
                };
                let buy = p("side") == "BUY";
                let (spent, received, spent_amount, received_amount) = if buy {
                    ("USDC", base, quote, p("quantity"))
                } else {
                    (base, "USDC", p("quantity"), quote)
                };
                self.change(who, spent, spent_amount, false);
                self.change(who, received, &sub(received_amount, fee), true);
                let order = json!({"orderId":n,"clientOrderId":id,"symbol":p("symbol"),"side":p("side"),"type":"LIMIT","timeInForce":"FOK","price":p("price"),"origQty":p("quantity"),"executedQty":p("quantity"),"cummulativeQuoteQty":quote,"status":"FILLED"});
                self.orders.lock().unwrap().insert(id, order.clone());
                self.trades.lock().unwrap().insert(n.to_string(),json!([{"id":n,"orderId":n,"symbol":p("symbol"),"isBuyer":buy,"isMaker":false,"price":p("price"),"qty":p("quantity"),"quoteQty":quote,"commission":fee,"commissionAsset":received}]));
                if self.lost.swap(false, Ordering::SeqCst) {
                    return Err(TransportError::Timeout);
                }
                order
            }
            E::SpotOrderStatus => self
                .orders
                .lock()
                .unwrap()
                .get(p("origClientOrderId"))
                .unwrap()
                .clone(),
            E::SpotTrades => {
                if self.missing_fills.load(Ordering::SeqCst) {
                    json!([])
                } else {
                    let mut rows = self
                        .trades
                        .lock()
                        .unwrap()
                        .get(p("orderId"))
                        .unwrap()
                        .clone();
                    if self.bad_fee.load(Ordering::SeqCst) {
                        rows[0]["commissionAsset"] = json!("BNB");
                    }
                    rows
                }
            }
            E::CapitalConfig => {
                json!([{"coin":"ETH","withdrawAllEnable":true,"networkList":[{"network":"ARBITRUM","coin":"ETH","withdrawEnable":true,"withdrawIntegerMultiple":"0.000001","withdrawFee":"0.0001","withdrawMin":"0.0001","withdrawMax":"100","withdrawTag":false,"busy":false}]}])
            }
            E::SubAccountTransferHistory => json!(self.transfer.lock().unwrap().iter().map(|row|json!({"counterParty":"master","email":"master@example.invalid","type":2,"asset":"ETH","qty":row["amount"],"fromAccountType":"SPOT","toAccountType":"SPOT","status":"SUCCESS","tranId":row["tranId"],"time":row["createTimeStamp"]})).collect::<Vec<_>>()),
            E::UniversalTransfer => {
                let n = self.posts.fetch_add(1, Ordering::SeqCst) + 1;
                self.change("sub", "ETH", p("amount"), false);
                self.change("main", "ETH", p("amount"), true);
                let row = json!({"tranId":n,"clientTranId":p("clientTranId"),"asset":"ETH","amount":p("amount"),"fromEmail":"sub@example.invalid","toEmail":"master@example.invalid","fromAccountType":"SPOT","toAccountType":"SPOT","status":"SUCCESS","createTimeStamp":chrono::Utc::now().timestamp_millis()});
                self.transfer.lock().unwrap().push(row);
                json!({"tranId":n,"clientTranId":p("clientTranId")})
            }
            E::UniversalTransferHistory => {
                json!({"totalCount":self.transfer.lock().unwrap().len(),"result":self.transfer.lock().unwrap().clone()})
            }
            E::TravelRuleRequirements => json!({"questionnaireCountryCode":"NIL"}),
            E::StandardWithdrawal => {
                self.posts.fetch_add(1, Ordering::SeqCst);
                assert_eq!(p("network"), "ARBITRUM");
                let gross = p("amount");
                let fee = "0.0001";
                let net = sub(gross, fee);
                self.change("main", "ETH", gross, false);
                self.withdrawal.lock().unwrap().push(json!({"id":"synthetic-withdrawal","withdrawOrderId":p("withdrawOrderId"),"coin":"ETH","network":"ARBITRUM","address":p("address"),"amount":net,"transactionFee":fee,"status":6,"txId":format!("0x{}","ab".repeat(32)),"transferType":0,"walletType":0,"applyTime":now()}));
                return Err(TransportError::Timeout);
            }
            E::WithdrawalHistory => json!(self.withdrawal.lock().unwrap().clone()),
            _ => panic!("unexpected synthetic endpoint {:?}", r.endpoint),
        })
    }
}
struct Prices;
#[async_trait]
impl BinancePriceProvider for Prices {
    async fn quote_usd(&self, coin: &str) -> Result<PriceQuote> {
        Ok(PriceQuote {
            usd_per_unit: match coin {
                "ETH" => "2000",
                "APT" => "10",
                _ => "1",
            }
            .into(),
            observed_at: now(),
            source: "synthetic-price".into(),
        })
    }
}
struct Approval {
    count: AtomicUsize,
    deny: AtomicBool,
}
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.count.fetch_add(1, Ordering::SeqCst);
        if self.deny.load(Ordering::SeqCst) {
            return Err(GoFinError::new(
                "APPROVAL_DENIED",
                "approval",
                "Synthetic denial",
                "Retry with a new request.",
            ));
        }
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "synthetic-only".into(),
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
struct Receipt(AtomicBool);
#[async_trait]
impl BinanceRecipientVerifier for Receipt {
    async fn verify(&self, r: PayoutRequest) -> PayoutResult {
        assert_eq!(r.chain_id, 42161);
        assert_eq!(r.recipient, "0x1111111111111111111111111111111111111111");
        assert_eq!(r.asset.network, "ethereum:42161");
        PayoutResult {
            status: if self.0.load(Ordering::SeqCst) {
                Status::Completed
            } else {
                Status::Pending
            },
            amount_atoms: Some(units("0.00989")),
            evidence: json!({"syntheticReceipt":true,"chainId":42161,"recipient":r.recipient,"canonical":self.0.load(Ordering::SeqCst)}),
        }
    }
}
fn adapter(v: Arc<Venue>, receipt: Arc<Receipt>) -> Arc<BinanceAdapter> {
    Arc::new(
        BinanceAdapter::new(
            BinanceConfig {
                accounts: vec![
                    BinanceAccount {
                        name: "main".into(),
                        resource: ResourceId("binance:uid:1:SPOT".into()),
                        credential: CredentialHandle::new("synthetic-main").unwrap(),
                        role: AccountRole::Main,
                        parent: None,
                        email: None,
                        wallet_type: "SPOT".into(),
                    },
                    BinanceAccount {
                        name: "sub".into(),
                        resource: ResourceId("binance:uid:2:SPOT".into()),
                        credential: CredentialHandle::new("synthetic-sub").unwrap(),
                        role: AccountRole::Sub,
                        parent: Some("main".into()),
                        email: Some("sub@example.invalid".into()),
                        wallet_type: "SPOT".into(),
                    },
                ],
                destinations: vec![EthereumDestination {
                    name: "wallet".into(),
                    resource: ResourceId(
                        "ethereum:42161:0x1111111111111111111111111111111111111111".into(),
                    ),
                    address: "0x1111111111111111111111111111111111111111".into(),
                    chain_id: 42161,
                    confirmations: 2,
                    assets: BTreeMap::from([(
                        "ETH".into(),
                        AssetId {
                            network: "ethereum:42161".into(),
                            reference: "native".into(),
                            decimals: 18,
                        },
                    )]),
                }],
            },
            v,
            Some(Arc::new(Prices)),
            Some(receipt),
        )
        .unwrap(),
    )
}
fn spot(id: &str, base: &str) -> OperationRequest {
    let (qty, price, fee, min, side) = match base {
        "APT" => ("2", "10", "0.021", "19.979", "SELL"),
        "SEI" => ("10", "1", "0.011", "9.989", "SELL"),
        _ => ("0.01", "2000", "0.00002", "0.00998", "BUY"),
    };
    OperationRequest {
        request_id: id.into(),
        operation: "binance.spot-order".into(),
        run_id: None,
        input: json!({"requestId":id,"account":"sub","symbol":format!("{base}USDC"),"side":side,"quantity":qty,"limitPrice":price,"maxFee":fee,"minReceiveAmount":min}),
    }
}
fn engine(store: Store, a: Arc<BinanceAdapter>, auth: Arc<Approval>, limit: &str) -> Engine {
    Engine::new(
        store,
        vec![a],
        auth,
        EngineOptions {
            max_movement_and_cost_usd_micros: limit.into(),
            max_run_allocation_usd_micros: limit.into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap()
}

#[tokio::test]
async fn permanent_portfolio_sell_buy_transfer_arbitrum_receipt_and_restart() {
    let v = Arc::new(Venue::new());
    let receipt = Arc::new(Receipt(AtomicBool::new(false)));
    let a = adapter(v.clone(), receipt.clone());
    let auth = Arc::new(Approval {
        count: AtomicUsize::new(0),
        deny: AtomicBool::new(false),
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("synthetic.sqlite");
    let e = engine(
        Store::open(&path).unwrap(),
        a.clone(),
        auth.clone(),
        "1000000000",
    );
    for (id, base) in [("sell-apt", "APT"), ("sell-sei", "SEI"), ("buy-eth", "ETH")] {
        let result = e.execute(spot(id, base)).await.unwrap();
        assert_eq!(result.status, Status::Completed, "{result:?}");
        assert!(result.run_id.is_none());
    }
    assert_eq!(v.free("sub", "APT"), "0");
    assert_eq!(v.free("sub", "SEI"), "0");
    let approved_buy = e.execute(spot("buy-eth", "ETH")).await.unwrap();
    let summary = &approved_buy.preview.as_ref().unwrap().summary;
    assert!(summary.contains("maximum debit 20 USDC"), "{summary}");
    assert!(summary.contains("maximum fee 0.00002 ETH"), "{summary}");
    assert_eq!(v.free("sub", "ETH"), "0.00999");
    assert_eq!(v.free("sub", "USDC"), "9.97");
    let duplicate = e.execute(spot("buy-eth", "ETH")).await.unwrap();
    assert_eq!(duplicate.status, Status::Completed);
    assert_eq!(v.posts.load(Ordering::SeqCst), 3);
    let result=e.execute(OperationRequest{request_id:"transfer".into(),operation:"binance.transfer".into(),run_id:None,input:json!({"requestId":"transfer","authority":"main","from":"sub","to":"main","coin":"ETH","amount":"0.00999","fromWallet":"SPOT","toWallet":"SPOT"})}).await.unwrap();
    assert_eq!(result.status, Status::Completed, "{result:?}");
    let request = OperationRequest {
        request_id: "withdraw".into(),
        operation: "binance.withdraw".into(),
        run_id: None,
        input: json!({"requestId":"withdraw","account":"main","destination":"wallet","coin":"ETH","amount":"0.00999","network":"ARBITRUM","walletType":"SPOT","maxFee":"0.0001","minReceiveAmount":"0.00989"}),
    };
    let pending = e.execute(request.clone()).await.unwrap();
    assert_eq!(pending.status, Status::Unknown, "{pending:?}");
    // Before generic route freezing, selected-chain withdrawals kept their
    // network inside the persisted endpoint params and a named destination.
    let mut legacy = pending.clone();
    let data = &mut legacy.submission.as_mut().unwrap().data;
    data.as_object_mut().unwrap().remove("network");
    let binding = data["binding"].as_object_mut().unwrap();
    for field in [
        "network",
        "receiptAddress",
        "receiptResource",
        "receiptMode",
    ] {
        binding.remove(field);
    }
    let reconciled = a.reconcile(&legacy).await.unwrap();
    assert_eq!(reconciled.status, Status::Pending, "{reconciled:?}");
    assert_eq!(v.posts.load(Ordering::SeqCst), 5);
    drop(e);
    let e = engine(Store::open(&path).unwrap(), a, auth.clone(), "0");
    e.recover_interrupted().unwrap();
    let pending = e.execute(request.clone()).await.unwrap();
    assert_eq!(
        pending.status,
        Status::Pending,
        "Venue completion is not recipient receipt: {pending:?}"
    );
    assert_eq!(v.posts.load(Ordering::SeqCst), 5);
    receipt.0.store(true, Ordering::SeqCst);
    let done = e.execute(request).await.unwrap();
    assert_eq!(done.status, Status::Completed, "{done:?}");
    assert_eq!(auth.count.load(Ordering::SeqCst), 5);
    assert_eq!(v.free("sub", "ETH"), "0");
    assert_eq!(v.free("main", "ETH"), "0.001");
    assert_eq!(v.posts.load(Ordering::SeqCst), 5);
}
#[tokio::test]
async fn denied_and_finite_policy_block_spot_before_submission() {
    let v = Arc::new(Venue::new());
    let a = adapter(v.clone(), Arc::new(Receipt(AtomicBool::new(false))));
    let auth = Arc::new(Approval {
        count: AtomicUsize::new(0),
        deny: AtomicBool::new(true),
    });
    let e = engine(Store::in_memory().unwrap(), a.clone(), auth.clone(), "0");
    let denied = e.execute(spot("denied", "APT")).await.unwrap();
    assert_ne!(denied.status, Status::Completed);
    assert_eq!(v.posts.load(Ordering::SeqCst), 0);
    auth.deny.store(false, Ordering::SeqCst);
    let e = engine(Store::in_memory().unwrap(), a, auth, "1000000");
    let limited = e.execute(spot("limited", "APT")).await.unwrap();
    assert_ne!(limited.status, Status::Completed);
    assert_eq!(v.posts.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn missing_or_wrong_fee_evidence_keeps_spot_unresolved_without_replay() {
    let v = Arc::new(Venue::new());
    v.lost.store(true, Ordering::SeqCst);
    v.missing_fills.store(true, Ordering::SeqCst);
    let a = adapter(v.clone(), Arc::new(Receipt(AtomicBool::new(false))));
    let auth = Arc::new(Approval {
        count: AtomicUsize::new(0),
        deny: AtomicBool::new(false),
    });
    let e = engine(Store::in_memory().unwrap(), a, auth, "0");
    let req = spot("lost", "APT");
    assert_eq!(
        e.execute(req.clone()).await.unwrap().status,
        Status::Unknown
    );
    assert_eq!(
        e.execute(req.clone()).await.unwrap().status,
        Status::Unknown
    );
    v.missing_fills.store(false, Ordering::SeqCst);
    v.bad_fee.store(true, Ordering::SeqCst);
    assert_eq!(
        e.execute(req.clone()).await.unwrap().status,
        Status::Unknown
    );
    v.bad_fee.store(false, Ordering::SeqCst);
    assert_eq!(e.execute(req).await.unwrap().status, Status::Completed);
    assert_eq!(v.posts.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn equivalent_padded_spot_inputs_keep_the_exact_approved_order_bounds() {
    let venue = Arc::new(Venue::new());
    let adapter = adapter(venue.clone(), Arc::new(Receipt(AtomicBool::new(false))));
    let mut request = spot("synthetic-padded-quantity", "APT");
    let canonical = adapter
        .prepare(&request.operation, &request.input)
        .await
        .unwrap();
    request.input["quantity"] = json!(format!("2.{}", "0".repeat(36)));
    request.input["limitPrice"] = json!("10.0000");
    let padded = adapter
        .prepare(&request.operation, &request.input)
        .await
        .unwrap();
    assert_eq!(padded.binding, canonical.binding);
    assert_eq!(padded.summary, canonical.summary);
    assert_eq!(
        serde_json::to_value(padded.intent).unwrap(),
        serde_json::to_value(canonical.intent).unwrap()
    );
    assert_eq!(venue.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn exact_input_filters_fee_asset_and_network_fail_closed() {
    let v = Arc::new(Venue::new());
    let a = adapter(v.clone(), Arc::new(Receipt(AtomicBool::new(false))));
    for field in ["quantity", "limitPrice", "maxFee", "minReceiveAmount"] {
        for invalid in [
            "-1",
            "1e1",
            "NaN",
            "0.0000000000000000000000000000000000001",
        ] {
            let mut r = spot("invalid", "APT");
            r.input[field] = json!(invalid);
            assert!(
                a.prepare(&r.operation, &r.input).await.is_err(),
                "{field}={invalid}"
            );
        }
    }
    let mut r = spot("invalid", "APT");
    r.input["maxFee"] = json!("0.001");
    assert!(a.prepare(&r.operation, &r.input).await.is_err());
    v.discount.store(true, Ordering::SeqCst);
    let r = spot("bnb", "APT");
    assert_eq!(
        a.prepare(&r.operation, &r.input).await.unwrap_err().code,
        "SPOT_COMMISSION_ASSET_UNSUPPORTED"
    );
    assert_eq!(v.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn multifill_rounded_commissions_are_reserved_and_proved_exactly() {
    let v = Arc::new(Venue::new());
    let a = adapter(v.clone(), Arc::new(Receipt(AtomicBool::new(false))));
    let mut too_tight = spot("rounding-cap", "APT");
    too_tight.input["maxFee"] = json!("0.02");
    assert_eq!(
        a.prepare(&too_tight.operation, &too_tight.input)
            .await
            .unwrap_err()
            .code,
        "FEE_BOUND_EXCEEDED"
    );
    v.missing_fills.store(true, Ordering::SeqCst);
    let auth = Arc::new(Approval {
        count: AtomicUsize::new(0),
        deny: AtomicBool::new(false),
    });
    let e = engine(Store::in_memory().unwrap(), a, auth, "0");
    let request = spot("rounded-fills", "APT");
    assert_eq!(
        e.execute(request.clone()).await.unwrap().status,
        Status::Unknown
    );
    let fill = |id| json!({"id":id,"orderId":1,"symbol":"APTUSDC","isBuyer":false,"isMaker":false,"price":"10","qty":"1","quoteQty":"10","commission":"0.01000001","commissionAsset":"USDC"});
    v.trades
        .lock()
        .unwrap()
        .insert("1".into(), json!([fill(1), fill(2)]));
    v.change("sub", "USDC", "0.00000002", false);
    v.missing_fills.store(false, Ordering::SeqCst);
    let settled = e.execute(request).await.unwrap();
    assert_eq!(settled.status, Status::Completed, "{settled:?}");
    assert_eq!(v.free("sub", "USDC"), "19.97999998");
    assert_eq!(v.posts.load(Ordering::SeqCst), 1);
}
