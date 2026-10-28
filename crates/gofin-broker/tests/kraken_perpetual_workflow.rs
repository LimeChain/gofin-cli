//! Synthetic Kraken USD-M futures lifecycle through the public development CLI.
use async_trait::async_trait;
use gofin_broker::{
    BrokerConfigV2, BrokerPolicy, NetworkConfig, RuntimeDependencies, SharedUsdValuator,
    compose_runtime_with_clock,
};
use gofin_domain::{clock::SystemClock, *};
use gofin_exchange::{ExchangeAdapter, kraken::DispatchGuard};
use gofin_kraken::{
    KrakenAccount, KrakenEndpoint as E, KrakenProduct, KrakenProfile, KrakenResponse,
    KrakenTransport,
};
use gofin_protocol::BrokerRequest;
use gofin_store::Store;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
include!("support/kraken_plan_client.rs");
#[cfg(target_os = "macos")]
#[path = "support/native_approval.rs"]
mod native_approval;

fn units(v: &str) -> i128 {
    Quantity::from_decimal(v, 6)
        .unwrap()
        .as_str()
        .parse()
        .unwrap()
}
fn dec(v: i128) -> String {
    let n = Quantity::new(v.unsigned_abs().to_string())
        .unwrap()
        .to_decimal(6);
    if v < 0 { format!("-{n}") } else { n }
}
#[derive(Default)]
struct State {
    cash: i128,
    flex: i128,
    spot: i128,
    position: i128,
    orders: BTreeMap<String, Value>,
    transfers: Vec<Value>,
    logs: Vec<Value>,
    writes: usize,
}
struct Wire(Mutex<State>);
#[async_trait]
impl KrakenTransport for Wire {
    async fn request(
        &self,
        e: E,
        _: Option<&KrakenAccount>,
        p: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e.mutating() {
            assert!(guard.as_ref().is_some_and(|g| g()));
        } else {
            assert!(guard.is_none());
        }
        let mut s = self.0.lock().unwrap();
        let r = match e {
            E::KrakenfuturesAuthGetApiKeysV3Check => {
                json!({"result":"success","permissions":{"general":"FULL_ACCESS","transfer":"FULL_ACCESS"},"accountUid":"uid","iiban":"SYNTHETIC"})
            }
            E::KrakenPublicGetAssets => json!({"result":{"ZUSD":{"altname":"USD","decimals":4}}}),
            E::KrakenPrivatePostGetApiKeyInfo => {
                json!({"result":{"permissions":["query-funds","query-ledger"],"valid_until":"0","iban":"SYNTHETIC"}})
            }
            E::KrakenfuturesPublicGetInstruments => {
                json!({"result":"success","instruments":[{"symbol":"PF_XBTUSD","base":"XBT","quote":"USD","type":"futures_vanilla","contractSize":1,"marginLevels":[{"contracts":0,"initialMargin":"0.02","maintenanceMargin":"0.01"}],"retailMarginLevels":[{"contracts":0,"initialMargin":"0.2","maintenanceMargin":"0.1"}],"tickSize":0.5,"maxPositionSize":100,"contractValueTradePrecision":3,"tradeable":true,"isExpired":false}]})
            }
            E::KrakenfuturesPublicGetTickersSymbol => {
                json!({"result":"success","ticker":{"symbol":"PF_XBTUSD","bid":"19999.5","ask":"20000","markPrice":"20000","suspended":false}})
            }
            E::KrakenfuturesPrivateGetAccounts => {
                json!({"result":"success","accounts":{"cash":{"balances":{"USD":dec(s.cash)}},"flex":{"type":"multiCollateralMarginAccount","currencies":{"USD":{"quantity":dec(s.flex),"available":dec(s.flex)}},"availableMargin":dec(s.flex)}}})
            }
            E::KrakenfuturesPrivateGetLeveragepreferences => {
                json!({"result":"success","leveragePreferences":[{"symbol":"PF_XBTUSD","maxLeverage":"3"}]})
            }
            E::KrakenfuturesPrivateGetOpenpositions => {
                json!({"result":"success","openPositions":if s.position==0 {vec![]}else{vec![json!({"symbol":"PF_XBTUSD","side":if s.position>0 {"long"}else{"short"},"size":dec(s.position.abs()),"price":"20000"})]}})
            }
            E::KrakenPrivatePostTradeVolume => {
                json!({"result":{"fees":{"PF_XBTUSD":{"fee":"0.2"}},"fees_maker":{"PF_XBTUSD":{"fee":"0.1"}}}})
            }
            E::KrakenfuturesPrivateGetOpenorders => {
                let rows:Vec<_>=s.orders.values().filter(|o|o["working"]==true).map(|o|json!({"order_id":o["id"],"cliOrdId":o["client"],"symbol":"PF_XBTUSD","side":o["side"],"filledSize":"0","unfilledSize":o["size"]})).collect();
                json!({"result":"success","openOrders":rows})
            }
            E::KrakenPrivatePostBalanceEx => {
                json!({"result":{"ZUSD":{"balance":dec(s.spot),"hold_trade":"0"}}})
            }
            E::KrakenPrivatePostLedgers => {
                let mut ledger = serde_json::Map::new();
                for t in &s.transfers {
                    if t["to"] == "spot" {
                        ledger.insert(format!("spot-{}",t["id"].as_str().unwrap()),json!({"refid":t["id"],"asset":"ZUSD","type":"transfer","amount":dec(units(t["toAfter"].as_str().unwrap())-units(t["toBefore"].as_str().unwrap())),"fee":"0"}));
                    }
                }
                json!({"result":{"ledger":ledger}})
            }
            E::KrakenfuturesPrivatePostWithdrawal => {
                assert_eq!(p["currency"], "USD");
                assert_eq!(p["sourceWallet"], "cash");
                let amount = units(p["amount"].as_str().unwrap());
                assert!(s.cash >= amount);
                let from = s.cash;
                let to = s.spot;
                s.cash -= amount;
                s.spot += amount;
                s.writes += 1;
                let id = format!("withdraw-{}", s.writes);
                let after = s.spot;
                s.transfers.push(json!({"id":id,"from":"cash","to":"spot","fromBefore":dec(from),"fromAfter":dec(from-amount),"toBefore":dec(to),"toAfter":dec(after)}));
                json!({"result":"success","transfer_id":id})
            }
            E::KrakenfuturesPrivatePostTransfer => {
                let amount = units(p["amount"].as_str().unwrap());
                let from = p["fromAccount"].as_str().unwrap();
                let to = p["toAccount"].as_str().unwrap();
                assert_eq!(p["unit"], "USD");
                let (before_from, before_to) = if from == "cash" && to == "flex" {
                    (s.cash, s.flex)
                } else if from == "flex" && to == "cash" {
                    (s.flex, s.cash)
                } else {
                    panic!("unexpected wallets")
                };
                assert!(before_from >= amount);
                if from == "cash" {
                    s.cash -= amount;
                    s.flex += amount;
                } else {
                    s.flex -= amount;
                    s.cash += amount;
                }
                s.writes += 1;
                let id = format!("transfer-{}", s.writes);
                let uid = "uid";
                s.transfers.push(json!({"id":id,"from":from,"to":to,"fromBefore":dec(before_from),"fromAfter":dec(before_from-amount),"toBefore":dec(before_to),"toAfter":dec(before_to+amount),"uid":uid}));
                json!({"result":"success","transfer_id":id})
            }
            E::KrakenfuturesPrivatePostSendorder => {
                assert_eq!(p["symbol"], "PF_XBTUSD");
                assert!(p["orderType"] == "ioc" || p["orderType"] == "lmt");
                assert!(p["limitPrice"] == "20000" || p["limitPrice"] == "21000");
                let side = p["side"].as_str().unwrap();
                assert!(side == "buy" || side == "sell");
                let size = units(p["size"].as_str().unwrap());
                let resting = p["limitPrice"].as_str() == Some("21000");
                let change = if side == "buy" { size } else { -size };
                let old = s.position;
                if p["reduceOnly"] == true {
                    assert!(old != 0 && old.signum() != change.signum() && old.abs() >= size);
                } else {
                    assert_eq!(old, 0);
                }
                s.writes += 1;
                let id = format!("ORDER-{}", s.writes);
                let client = p["cliOrdId"].as_str().unwrap();
                let timestamp = chrono::Utc::now().timestamp_millis() + 1;
                let new = if resting { old } else { old + change };
                let fee = if resting { 0 } else { units("0.02") };
                let loss = if !resting && p["reduceOnly"] == true {
                    units("0.01")
                } else {
                    0
                };
                let old_balance = s.flex;
                if !resting {
                    s.position = new;
                    s.flex -= fee + loss;
                }
                let order = json!({"id":id,"client":client,"side":side,"size":dec(size),"old":dec(old),"new":dec(new),"fee":dec(fee),"pnl":dec(-loss),"old_balance":dec(old_balance),"new_balance":dec(s.flex),"timestamp":timestamp,"working":resting});
                s.orders.insert(id.clone(), order);
                json!({"result":"success","sendStatus":{"order_id":id,"status":if resting {"placed"} else {"filled"}}})
            }
            E::KrakenfuturesHistoryGetAccountLog => {
                let mut rows = s.logs.clone();
                for o in s
                    .orders
                    .values()
                    .filter(|o| o["working"] == false && o["cancelled"] != true)
                {
                    let id = o["id"].as_str().unwrap();
                    rows.push(json!({"asset":"USD","booking_uid":format!("booking-{id}"),"contract":"PF_XBTUSD","execution":format!("execution-{id}"),"id":1,"margin_account":"flex","fee":o["fee"],"realized_pnl":o["pnl"],"realized_funding":0,"old_balance":o["old_balance"],"new_balance":o["new_balance"]}));
                }
                for t in &s.transfers {
                    for (wallet, a, b) in [
                        (
                            t["from"].as_str().unwrap(),
                            &t["fromBefore"],
                            &t["fromAfter"],
                        ),
                        (t["to"].as_str().unwrap(), &t["toBefore"], &t["toAfter"]),
                    ] {
                        if wallet == "spot" {
                            continue;
                        }
                        rows.push(json!({"asset":"USD","booking_uid":format!("{}-{wallet}",t["id"].as_str().unwrap()),"execution":t["id"],"margin_account":wallet,"info":"transfer","fee":0,"old_balance":a,"new_balance":b}));
                    }
                }
                json!({"accountUid":"uid","logs":rows})
            }
            E::KrakenfuturesHistoryGetOrders => {
                let rows:Vec<_>=s.orders.values().map(|o| {let native=native_order(o);json!({"uid":format!("event-{}",o["id"].as_str().unwrap()),"timestamp":o["timestamp"],"event":if o["working"]==true {json!({"OrderPlaced":{"order":native,"reason":"new","reducedQuantity":""}})} else {json!({"OrderCancelled":{"order":native,"reason":"ioc"}})}})}).collect();
                json!({"elements":rows})
            }
            E::KrakenfuturesHistoryGetExecutions => {
                let rows:Vec<_>=s.orders.values().filter(|o|o["working"]==false && o["cancelled"]!=true).map(|o| {let native=native_order(o);let id=o["id"].as_str().unwrap();json!({"uid":format!("execution-event-{id}"),"timestamp":o["timestamp"],"event":{"execution":{"execution":{"uid":format!("execution-{id}"),"order":native,"quantity":o["size"],"price":"20000","executionType":"maker","timestamp":o["timestamp"]},"takerReducedQuantity":""}}})}).collect();
                json!({"elements":rows})
            }
            E::KrakenfuturesHistoryGetPositions => {
                let rows:Vec<_>=s.orders.values().filter(|o|o["working"]==false && o["cancelled"]!=true).map(|o| {let id=o["id"].as_str().unwrap();json!({"uid":format!("position-event-{id}"),"timestamp":o["timestamp"],"event":{"PositionUpdate":{"tradeable":"PF_XBTUSD","oldPosition":o["old"],"newPosition":o["new"],"feeCurrency":"USD","fee":o["fee"],"realizedPnL":o["pnl"],"realizedFunding":"0","executionUid":format!("execution-{id}"),"tradeType":"userExecution","updateReason":"trade","timestamp":o["timestamp"]}}})}).collect();
                json!({"elements":rows})
            }
            E::KrakenfuturesPrivatePostBatchorder => {
                let id = p["batchOrder"][0]["order_id"].as_str().unwrap();
                let o = s.orders.get_mut(id).unwrap();
                assert_eq!(o["working"], true);
                o["working"] = json!(false);
                o["cancelled"] = json!(true);
                s.writes += 1;
                json!({"result":"success","batchStatus":[{"order_id":id,"status":"cancelled"}]})
            }
            _ => panic!("unhandled futures endpoint {e:?} {p}"),
        };
        Ok(KrakenResponse::Json(r))
    }
}

fn native_order(o: &Value) -> Value {
    json!({"uid":o["id"],"timestamp":o["timestamp"],"accountUid":"uid","clientId":o["client"],"tradeable":"PF_XBTUSD","quantity":o["size"],"filled":if o["working"]==true || o["cancelled"]==true {json!("0")}else{o["size"].clone()},"direction":if o["side"]=="buy" {"Buy"}else{"Sell"}})
}
struct Quotes;
#[async_trait]
impl gofin_pricing::UsdPriceSource for Quotes {
    async fn quote(&self, a: &AssetId) -> Result<gofin_pricing::UsdQuote> {
        Ok(gofin_pricing::UsdQuote {
            usd_per_unit: if a.reference.eq_ignore_ascii_case("xbt") {
                "20000"
            } else {
                "1"
            }
            .into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic futures quote".into(),
        })
    }
}
struct Approval(AtomicUsize);
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        assert_eq!(c.purpose, "financial_write");
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "synthetic futures approval".into(),
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
#[test]
#[ignore = "requires GOFIN_KRAKEN_PLAN_CLI with synthetic-kraken-build"]
fn positive_kraken_perpetual_lifecycle_via_public_cli() {
    let binary = std::env::var_os("GOFIN_KRAKEN_PLAN_CLI").expect("public CLI required");
    assert!(std::path::Path::new(&binary).is_file());
    std::thread::Builder::new().stack_size(8*1024*1024).spawn(||tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_stack_size(8*1024*1024).enable_all().build().unwrap().block_on(async {
 let temp=tempfile::tempdir().unwrap();let venue=Arc::new(Wire(Mutex::new(State {cash:units("100"),..Default::default()})));let prices:Arc<dyn gofin_pricing::UsdPriceSource>=Arc::new(Quotes);
 let profile=KrakenProfile::new(KrakenProduct::Derivatives,vec![KrakenAccount {name:"futures".into(),resource:ResourceId("krakenfutures:uid".into()),credential_ref:"synthetic".into(),physical_identity:"uid".into(),wallet_account_id:None,default_wallet_account_id:None,identity_verified:true,product:KrakenProduct::Derivatives}],venue.clone()).unwrap().with_prices(prices.clone()).with_related_accounts(vec![KrakenAccount {name:"spot".into(),resource:ResourceId("kraken:spot".into()),credential_ref:"synthetic-spot".into(),physical_identity:"SYNTHETIC".into(),wallet_account_id:None,default_wallet_account_id:None,identity_verified:true,product:KrakenProduct::Spot}]);
 let approval=Arc::new(Approval(AtomicUsize::new(0)));let mut policy=BrokerPolicy::beta();policy.max_movement_usd_micros="300000000".into();policy.max_run_allocation_usd_micros="500000000".into();
 let runtime=compose_runtime_with_clock(BrokerConfigV2 {version:2,networks:NetworkConfig::reviewed_evm_networks(),deployments:vec![],connections:vec![],policy,approver_uid:unsafe{libc::geteuid()},extensions:BTreeMap::new()},RuntimeDependencies {store:Store::open(temp.path().join("futures.sqlite")).unwrap(),adapters:vec![Arc::new(ExchangeAdapter::new(profile))],authorizer:approval.clone(),valuator:Arc::new(SharedUsdValuator::with_source(prices)),bridge:None},Arc::new(SystemClock)).unwrap();let client=Client::new(runtime,temp.path()).await;
 async fn execute(client:&Client,op:&str,input:Value)->Value {let id=input["requestId"].as_str().unwrap().to_owned();let preview=client.call(args(&["preview",op,"--input",&input.to_string(),"--json"]),BrokerRequest::Preview {operation:op.into(),input:input.clone()},true).await;#[cfg(target_os="macos")] native_approval::assert_prepared_renders(&serde_json::from_value(preview).unwrap());client.call(args(&["execute",op,"--input",&input.to_string(),"--id",&id,"--json"]),BrokerRequest::Execute {request:OperationRequest {request_id:id,operation:op.into(),input,run_id:None}},true).await}
 let mut submission_ids=std::collections::BTreeSet::new();
 let transfer=json!({"requestId":"fund","account":"futures","fromAccount":"cash","toAccount":"flex","unit":"USD","amount":"50"});let fund=execute(&client,"krakenfutures.transfer",transfer).await;assert_eq!(fund["status"],"completed","{fund}");assert!(submission_ids.insert(fund["submission"]["reference"].as_str().unwrap().to_owned()));
 for (id,side,reduce) in [("long-open","buy",false),("long-close","sell",true),("short-open","sell",false),("short-close","buy",true)] {let order=json!({"requestId":id,"account":"futures","symbol":"PF_XBTUSD","orderType":"ioc","size":"0.001","limitPrice":"20000","side":side,"reduceOnly":reduce,"maximumFees":{"USD":"0.5"},"maximumLoss":"0.5","maximumProfit":"0.5","maximumFundingLoss":"0.1","maximumFundingProfit":"0.1"});let result=execute(&client,"krakenfutures.send-order",order).await;assert_eq!(result["status"],"completed","{result}");assert_eq!(result["preview"]["binding"]["derivative"]["leveragePreference"]["maxLeverage"],"3");assert!(submission_ids.insert(result["submission"]["reference"].as_str().unwrap().to_owned()));
 if reduce {assert!(result["effects"]["effects"].as_array().unwrap().iter().any(|effect|effect["kind"]=="settlement" && effect["category"]=="realized_pnl" && effect["delta"]=="-10000000000000000000000000000000000"),"{result}");}
 if id=="long-close" {let resting=json!({"requestId":"rest","account":"futures","symbol":"PF_XBTUSD","orderType":"lmt","size":"0.001","limitPrice":"21000","side":"sell","maximumFees":{"USD":"0.5"},"maximumLoss":"0.5","maximumProfit":"0.5","maximumFundingLoss":"0.1","maximumFundingProfit":"0.1"});let open=execute(&client,"krakenfutures.send-order",resting).await;assert_eq!(open["status"],"pending","{open}");assert!(submission_ids.insert(open["submission"]["reference"].as_str().unwrap().to_owned()));let client_id=venue.0.lock().unwrap().orders.values().find(|o|o["working"]==true).unwrap()["client"].clone();let cancel=json!({"requestId":"cancel","account":"futures","operationId":open["id"],"originalSubmission":open["submission"],"cliOrdId":client_id});let canceled=execute(&client,"krakenfutures.cancel-order",cancel).await;assert_eq!(canceled["status"],"completed","{canceled}");assert!(submission_ids.insert(canceled["submission"]["reference"].as_str().unwrap().to_owned()));let parent_id=open["id"].as_str().unwrap();let parent=client.call(args(&["status",parent_id,"--json"]),BrokerRequest::Status{id:parent_id.into()},true).await;assert!(parent["status"]=="failed"||parent["status"]=="completed","{parent}");}
 }
 let back=json!({"requestId":"return","account":"futures","fromAccount":"flex","toAccount":"cash","unit":"USD","amount":"49.90"});let result=execute(&client,"krakenfutures.transfer",back).await;assert_eq!(result["status"],"completed","{result}");assert!(submission_ids.insert(result["submission"]["reference"].as_str().unwrap().to_owned()));let withdrawal=json!({"requestId":"withdraw","account":"futures","destinationAccount":"spot","currency":"USD","amount":"99.90","sourceWallet":"cash"});let withdrawn=execute(&client,"krakenfutures.withdrawal",withdrawal).await;assert_eq!(withdrawn["status"],"completed","{withdrawn}");assert!(submission_ids.insert(withdrawn["submission"]["reference"].as_str().unwrap().to_owned()));assert_eq!(submission_ids.len(),9);let state=venue.0.lock().unwrap();assert_eq!(state.cash,0);assert_eq!(state.spot,units("99.90"));assert_eq!(state.flex,0);assert_eq!(state.position,0);
 })).unwrap().join().unwrap();
}
