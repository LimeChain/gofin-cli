use super::*;
use crate as gofin_broker;
use gofin_domain::{ApprovalAudit, AuthorizationChallenge, Authorizer, OperationRequest};
use gofin_exchange::ExchangeAdapter;
use gofin_kraken::funding::{
    KrakenDestination, KrakenPayoutRequest, KrakenPayoutResult, KrakenRecipientVerifier,
};
use gofin_protocol::BrokerRequest;
use std::sync::atomic::AtomicUsize;
include!("../tests/support/kraken_plan_client.rs");
#[cfg(target_os = "macos")]
#[path = "../tests/support/native_approval.rs"]
mod native_approval;

fn units(s: &str) -> i128 {
    Quantity::from_decimal(s, 6)
        .unwrap()
        .as_str()
        .parse()
        .unwrap()
}
fn decimal(n: i128) -> String {
    Quantity::new(n.to_string()).unwrap().to_decimal(6)
}
#[derive(Default)]
struct Ledger {
    cash: i128,
    eth: i128,
    orders: BTreeMap<String, Value>,
    receipt: Option<Value>,
    withdrawal: bool,
    writes: usize,
}
struct Wire {
    deposit: Arc<Venue>,
    ledger: Mutex<Ledger>,
}
#[async_trait]
impl KrakenTransport for Wire {
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
        let result = match e {
            E::KrakenPrivatePostGetApiKeyInfo => {
                json!({"permissions":["query-funds","query-open-trades","query-closed-trades","query-ledger","modify-trades","close-trades","withdraw-funds"],"valid_until":"0","iban":"SYNTHETIC"})
            }
            E::KrakenPublicGetAssets => json!({"XETH":{"decimals":18},"USDC":{"decimals":6}}),
            E::KrakenPublicGetAssetPairs => {
                json!({"ETHUSDC":{"altname":"ETHUSDC","base":"XETH","quote":"USDC","status":"online","lot_decimals":8,"pair_decimals":2,"cost_decimals":6,"ordermin":"0.0001"}})
            }
            E::KrakenPublicGetTicker => {
                json!({"ETHUSDC":{"b":["1999","1","1"],"a":["2000","1","1"]}})
            }
            E::KrakenPrivatePostTradeVolume => {
                json!({"fees":{"ETHUSDC":{"fee":"0.4"}},"fees_maker":{"ETHUSDC":{"fee":"0.25"}}})
            }
            E::KrakenPrivatePostBalanceEx => {
                let s = self.ledger.lock().unwrap();
                let hold = s.orders.values().any(|o| o["status"] == "open");
                json!({"XETH":{"balance":decimal(s.eth),"hold_trade":if hold {"0.002988"}else{"0"}},"USDC":{"balance":decimal(s.cash),"hold_trade":"0"}})
            }
            E::KrakenPrivatePostBalance => {
                let s = self.ledger.lock().unwrap();
                json!({"XETH":decimal(s.eth),"USDC":decimal(s.cash)})
            }
            E::KrakenPrivatePostOpenPositions | E::KrakenPrivatePostCreditLines => json!({}),
            E::KrakenPrivatePostEarnAllocations => json!({"items":[]}),
            E::KrakenPrivatePostAddOrder => {
                let size = p["volume"].as_str().unwrap();
                let side = p["type"].as_str().unwrap();
                let price = p["price"].as_str().unwrap_or("2000");
                let qty = units(size);
                let value = qty * units("2000") / 1_000_000;
                let resting = price == "2100";
                let base_fee = side == "sell" && qty == units("0.002988") && !resting;
                let fee = if base_fee {
                    units("0.000012")
                } else {
                    value * 4 / 1000
                };
                let mut s = self.ledger.lock().unwrap();
                s.writes += 1;
                let id = format!("ORDER-{}", s.writes);
                let trade = format!("TRADE-{}", s.writes);
                if !resting {
                    if side == "buy" {
                        s.cash -= value + fee;
                        s.eth += qty;
                    } else {
                        s.cash += value - if base_fee { 0 } else { fee };
                        s.eth -= qty + if base_fee { fee } else { 0 };
                    }
                    assert!(s.cash >= 0 && s.eth >= 0);
                }
                s.orders.insert(id.clone(),json!({"cl_ord_id":p["cl_ord_id"],"opentm":format!("{}.{:09}",chrono::Utc::now().timestamp(),chrono::Utc::now().timestamp_subsec_nanos()),"descr":{"pair":"ETHUSDC","type":side},"status":if resting {"open"}else{"closed"},"vol":size,"vol_exec":if resting {"0".to_owned()}else{decimal(qty)},"cost":if resting {"0".to_owned()}else{decimal(value)},"fee":if resting {"0".to_owned()}else{decimal(fee)},"trades":if resting {json!([])}else{json!([trade])}}));
                json!({"txid":[id]})
            }
            E::KrakenPrivatePostQueryOrders => {
                let ids = p["txid"].as_str().unwrap();
                let s = self.ledger.lock().unwrap();
                let mut map = serde_json::Map::new();
                for id in ids.split(',') {
                    if let Some(o) = s.orders.get(id) {
                        map.insert(id.into(), o.clone());
                    }
                }
                Value::Object(map)
            }
            E::KrakenPrivatePostOpenOrders | E::KrakenPrivatePostClosedOrders => {
                let s = self.ledger.lock().unwrap();
                let open = e == E::KrakenPrivatePostOpenOrders;
                let rows: BTreeMap<_, _> = s
                    .orders
                    .iter()
                    .filter(|(_, o)| (o["status"] == "open") == open)
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                if open {
                    json!({"open":rows})
                } else {
                    json!({"closed":rows})
                }
            }
            E::KrakenPrivatePostQueryTrades => {
                let s = self.ledger.lock().unwrap();
                let mut rows = serde_json::Map::new();
                for (id, o) in &s.orders {
                    if let Some(t) = o["trades"]
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(Value::as_str)
                    {
                        rows.insert(t.into(),json!({"ordertxid":id,"pair":"ETHUSDC","type":o["descr"]["type"],"vol":o["vol_exec"],"cost":o["cost"],"fee":o["fee"],"margin":"0","leverage":"0","ledgers":[format!("{t}-base"),format!("{t}-quote")]}));
                    }
                }
                Value::Object(rows)
            }
            E::KrakenPrivatePostQueryLedgers => {
                let s = self.ledger.lock().unwrap();
                let mut rows = serde_json::Map::new();
                for o in s.orders.values() {
                    if let Some(t) = o["trades"]
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(Value::as_str)
                    {
                        let buy = o["descr"]["type"] == "buy";
                        let base_fee = o["vol_exec"] == "0.002988";
                        rows.insert(format!("{t}-base"), json!({"refid":t,"type":"trade","asset":"XETH","amount":if buy {o["vol_exec"].as_str().unwrap().to_owned()} else {format!("-{}",o["vol_exec"].as_str().unwrap())},"fee":if base_fee {o["fee"].clone()} else {json!("0")}}));
                        rows.insert(format!("{t}-quote"), json!({"refid":t,"type":"trade","asset":"USDC","amount":if buy {format!("-{}",o["cost"].as_str().unwrap())} else {o["cost"].as_str().unwrap().to_owned()},"fee":if base_fee {json!("0")} else {o["fee"].clone()}}));
                    }
                }
                Value::Object(rows)
            }
            E::KrakenPrivatePostCancelOrder | E::KrakenPrivatePostCancelOrderBatch => {
                let mut s = self.ledger.lock().unwrap();
                s.writes += 1;
                let id = p["txid"]
                    .as_str()
                    .or_else(|| p["orders"][0]["txid"].as_str())
                    .unwrap();
                let o = s.orders.get_mut(id).unwrap();
                assert_eq!(o["status"], "open");
                o["status"] = json!("canceled");
                json!({"count":1})
            }
            E::KrakenPrivatePostWithdrawInfo => {
                json!({"method":"USDC - Ethereum","amount":"109.804","fee":"0.1","limit":decimal(self.ledger.lock().unwrap().cash)})
            }
            E::KrakenPrivatePostWithdrawAddresses => {
                json!([{"key":"synthetic-ethereum","method":"USDC - Ethereum","verified":true,"address":"0x1111111111111111111111111111111111111111"}])
            }
            E::KrakenPrivatePostWithdrawMethods => {
                json!([{"method":"USDC - Ethereum","network":"Ethereum","minimum":"0.01"}])
            }
            E::KrakenPrivatePostWithdraw => {
                assert_eq!(p["amount"], "109.904");
                let mut s = self.ledger.lock().unwrap();
                assert!(!s.withdrawal);
                assert_eq!(s.cash, units("109.904"));
                s.withdrawal = true;
                s.cash = 0;
                s.writes += 1;
                json!({"refid":"WITHDRAW-1"})
            }
            E::KrakenPrivatePostWithdrawStatus => {
                json!([{"refid":"WITHDRAW-1","status":"Success","asset":"USDC","method":"USDC - Ethereum","key":"synthetic-ethereum","info":"0x1111111111111111111111111111111111111111","amount":"109.804","fee":"0.1","txid":format!("0x{}","aa".repeat(32))}])
            }
            E::KrakenPrivatePostLedgers if self.ledger.lock().unwrap().withdrawal => {
                json!({"ledger":{"WITHDRAW-LEDGER":{"refid":"WITHDRAW-1","type":"withdrawal","asset":"USDC","amount":"-109.804","fee":"0.1","balance":"0"}}})
            }
            _ => return self.deposit.request(e, account, p, guard).await,
        };
        Ok(KrakenResponse::Json(json!({"error":[],"result":result})))
    }
}

#[async_trait]
impl KrakenRecipientVerifier for Wire {
    async fn resolve(
        &self,
        network: &str,
        coin: &str,
        contract: Option<&str>,
        address: &str,
        selector: Option<&str>,
    ) -> Result<Option<KrakenDestination>> {
        if (network, coin, contract, address, selector)
            != (
                "Ethereum",
                "USDC",
                None,
                "0x1111111111111111111111111111111111111111",
                Some("ethereum:1:0x1111111111111111111111111111111111111111"),
            )
        {
            return Ok(None);
        }
        Ok(Some(KrakenDestination {
            enrolled: true,
            resource: ResourceId("ethereum:1:0x1111111111111111111111111111111111111111".into()),
            address: "0x1111111111111111111111111111111111111111".into(),
            chain_id: 1,
            network: None,
            cluster: None,
            mint: None,
            asset: AssetId {
                network: "ethereum:1".into(),
                reference: usdc_fixture::TOKEN.into(),
                decimals: 6,
            },
            confirmations: 1,
        }))
    }
    async fn verify(&self, r: KrakenPayoutRequest) -> Result<KrakenPayoutResult> {
        let s = self.ledger.lock().unwrap();
        let receipt = s.receipt.as_ref();
        let correct = receipt.is_some_and(|v| {
            v["status"] == "0x1"
                && v["chainId"] == r.destination.chain_id
                && v["hash"] == r.tx_hash
                && v["to"] == r.destination.address
                && v["contract"] == r.destination.asset.reference
                && v["amount"] == r.minimum
                && r.minimum == r.maximum
        });
        Ok(KrakenPayoutResult {
            status: if correct {
                Status::Completed
            } else {
                Status::Pending
            },
            amount: correct.then(|| Quantity::from_decimal(&r.minimum, 6).unwrap()),
            evidence: json!({"independentReceipt":receipt}),
        })
    }
}
struct PositiveApproval(AtomicUsize);
#[async_trait]
impl Authorizer for PositiveApproval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        assert_eq!(c.purpose, "financial_write");
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "synthetic spot approval".into(),
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
struct PositivePrices;
#[async_trait]
impl gofin_pricing::UsdPriceSource for PositivePrices {
    async fn quote(&self, a: &AssetId) -> Result<gofin_pricing::UsdQuote> {
        Ok(gofin_pricing::UsdQuote {
            usd_per_unit: if matches!(a.reference.as_str(), "XETH" | "native") {
                "2000"
            } else {
                "1"
            }
            .into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic independent spot prices".into(),
        })
    }
}

#[test]
#[ignore = "requires GOFIN_KRAKEN_PLAN_CLI with synthetic-kraken-build"]
fn positive_kraken_spot_lifecycle_via_public_cli() {
    let binary = std::env::var_os("GOFIN_KRAKEN_PLAN_CLI").expect("public CLI required");
    assert!(std::path::Path::new(&binary).is_file());
    std::thread::Builder::new().stack_size(8*1024*1024).spawn(||tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_stack_size(8*1024*1024).enable_all().build().unwrap().block_on(async {
        let temp=tempfile::tempdir().unwrap();let (mut deposit, venue, _signer)=submission_tests::signed_adapter(None);venue.usdc_evm.store(true,Ordering::SeqCst);venue.rpc.usdc_evm.store(true,Ordering::SeqCst);
        let wire=Arc::new(Wire {deposit:venue.clone(),ledger:Mutex::new(Ledger {cash:units("10"),..Default::default()})});
        let prices:Arc<dyn gofin_pricing::UsdPriceSource>=Arc::new(PositivePrices);
        let profile=KrakenProfile::new(KrakenProduct::Spot,vec![KrakenAccount {name:"fixture-spot".into(),resource:ResourceId("kraken:fixture-spot".into()),credential_ref:"synthetic-credential-only".into(),physical_identity:"synthetic-uid-only".into(),wallet_account_id:None,default_wallet_account_id:None,identity_verified:true,product:KrakenProduct::Spot}],wire.clone()).unwrap().with_prices(prices.clone()).with_recipient_verifier(wire.clone());
        deposit.kraken=Arc::new(profile.clone());let exchange=Arc::new(ExchangeAdapter::new(profile));let approval=Arc::new(PositiveApproval(AtomicUsize::new(0)));let mut policy=crate::BrokerPolicy::beta();policy.max_movement_usd_micros="300000000".into();policy.max_run_allocation_usd_micros="500000000".into();
        let runtime=crate::compose_runtime_with_clock(crate::BrokerConfigV2 {version:2,networks:crate::NetworkConfig::reviewed_evm_networks(),deployments:vec![],connections:vec![],policy,approver_uid:unsafe{libc::geteuid()},extensions:BTreeMap::new()},crate::RuntimeDependencies {store:gofin_store::Store::open(temp.path().join("spot.sqlite")).unwrap(),adapters:vec![Arc::new(deposit),exchange],authorizer:approval.clone(),valuator:Arc::new(crate::SharedUsdValuator::with_source(prices)),bridge:None},Arc::new(gofin_domain::clock::SystemClock)).unwrap();
        let client=Client::new(runtime,temp.path()).await;
        async fn execute(client:&Client,op:&str,input:Value)->Value {let id=input["requestId"].as_str().unwrap().to_owned();let preview=client.call(args(&["preview",op,"--input",&input.to_string(),"--json"]),BrokerRequest::Preview {operation:op.into(),input:input.clone()},true).await;#[cfg(target_os="macos")] native_approval::assert_prepared_renders(&serde_json::from_value(preview).unwrap());client.call(args(&["execute",op,"--input",&input.to_string(),"--id",&id,"--json"]),BrokerRequest::Execute {request:OperationRequest {request_id:id,operation:op.into(),input,run_id:None}},true).await}
        let mut deposit_input=input();deposit_input["amount"]=json!("100");deposit_input["address"]=json!("0x1111111111111111111111111111111111111111");let deposit=execute(&client,"kraken.deposit",deposit_input).await;assert!(deposit["status"]=="pending"||deposit["status"]=="unknown","{deposit}");wire.ledger.lock().unwrap().cash=units("110");let id=deposit["id"].as_str().unwrap();let deposit=client.call(args(&["status",id,"--json"]),BrokerRequest::Status{id:id.into()},true).await;assert_eq!(deposit["status"],"completed","{deposit}");
        let mut order_submissions=std::collections::BTreeSet::new();
        for (id,side,kind,size,price,status) in [("buy","buy","limit","0.006","2000","completed"),("part","sell","market","0.003","2000","completed"),("rest","sell","limit","0.002988","2100","pending"),("final","sell","market","0.002988","2000","completed")] {
            let mut order=json!({"requestId":id,"account":"fixture-spot","pair":"ETHUSDC","type":side,"ordertype":kind,"volume":size,"maximumFees":{"XETH":if id=="rest"||id=="final" {"0.000012"}else{"0.00003"},"USDC":"0.05"},"minimumReceive":if side=="buy" {"0.00597"} else {"5.9"}});if kind=="limit" {order["price"]=json!(price);order["timeinforce"]=json!(if id=="rest" {"GTC"}else{"IOC"});}
            let result=execute(&client,"kraken.add-order",order).await;assert_eq!(result["status"],status,"{result}");assert!(order_submissions.insert(result["submission"]["reference"].as_str().unwrap().to_owned()),"distinct orders must not share a durable submission");
            if id=="final" { assert!(result["effects"]["effects"].as_array().unwrap().iter().any(|effect| effect["kind"]=="cost" && effect["amount"]["asset"]["reference"]=="XETH" && effect["amount"]["atoms"]==Quantity::from_decimal("0.000012",36).unwrap().as_str()), "{result}"); }
            if id=="rest" {let balance=wire.request(E::KrakenPrivatePostBalanceEx,None,json!({}),None).await.unwrap();let KrakenResponse::Json(balance)=balance else {panic!("expected balance")};assert_eq!(balance["result"]["XETH"]["hold_trade"],"0.002988");let (order_id,row)=wire.ledger.lock().unwrap().orders.iter().find(|(_,o)|o["status"]=="open").map(|(k,v)|(k.clone(),v.clone())).unwrap();let cancel=json!({"requestId":"cancel","account":"fixture-spot","txid":order_id,"cl_ord_id":row["cl_ord_id"],"operationId":result["id"],"originalSubmission":result["submission"]});let cancelled=execute(&client,"kraken.cancel-order",cancel).await;assert_eq!(cancelled["status"],"completed","{cancelled}");let parent_id=result["id"].as_str().unwrap();let parent=client.call(args(&["status",parent_id,"--json"]),BrokerRequest::Status{id:parent_id.into()},true).await;assert!(parent["status"]=="failed"||parent["status"]=="completed","{parent}");}
        }
        assert_eq!(order_submissions.len(),4);
        assert_eq!(wire.ledger.lock().unwrap().eth,0);assert_eq!(wire.ledger.lock().unwrap().cash,units("109.904"));
        let withdraw=json!({"requestId":"withdraw","account":"fixture-spot","asset":"USDC","key":"synthetic-ethereum","amount":"109.904","max_fee":"0.1","destination":"ethereum:1:0x1111111111111111111111111111111111111111"});let result=execute(&client,"kraken.withdraw-funds",withdraw).await;assert!(result["status"]=="pending"||result["status"]=="unknown","{result}");wire.ledger.lock().unwrap().receipt=Some(json!({"hash":format!("0x{}","aa".repeat(32)),"to":"0x1111111111111111111111111111111111111111","contract":usdc_fixture::TOKEN,"amount":"109.804","status":"0x1","chainId":1}));let id=result["id"].as_str().unwrap();let done=client.call(args(&["status",id,"--json"]),BrokerRequest::Status{id:id.into()},true).await;assert_eq!(done["status"],"completed","{done}");assert_eq!(wire.ledger.lock().unwrap().cash,0);assert_eq!(wire.ledger.lock().unwrap().writes,6);assert_eq!(approval.0.load(Ordering::SeqCst),7);
    })).unwrap().join().unwrap();
}
