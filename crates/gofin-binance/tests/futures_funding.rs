//! Synthetic-only futures funding verification; no credentials or live writes.
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

struct Venue {
    wallets: Mutex<BTreeMap<(String, String), String>>,
    history: Mutex<Vec<Value>>,
    posts: AtomicUsize,
    timeout: AtomicBool,
    permissions: AtomicBool,
    internal_permission: AtomicBool,
    history_mode: AtomicUsize,
    can_transfer: AtomicBool,
    withdraw_limit: Mutex<String>,
}
impl Venue {
    fn new() -> Self {
        let mut wallets = BTreeMap::new();
        for who in ["main", "sub"] {
            wallets.insert((who.into(), "SPOT".into()), "100".into());
            wallets.insert((who.into(), "USDT_FUTURE".into()), "50".into());
        }
        Self {
            wallets: Mutex::new(wallets),
            history: Mutex::new(vec![]),
            posts: AtomicUsize::new(0),
            timeout: AtomicBool::new(false),
            permissions: AtomicBool::new(true),
            internal_permission: AtomicBool::new(true),
            history_mode: AtomicUsize::new(0),
            can_transfer: AtomicBool::new(true),
            withdraw_limit: Mutex::new("50".into()),
        }
    }
    fn balance(&self, who: &str, wallet: &str) -> String {
        self.wallets.lock().unwrap()[&(who.into(), wallet.into())].clone()
    }
    fn rows(&self, who: &str) -> Value {
        json!({"uid":if who=="main" {1}else{2},"accountType":"SPOT","balances":[{"asset":"USDT","free":self.balance(who,"SPOT"),"locked":"0"}]})
    }
    fn change(&self, who: &str, wallet: &str, amount: &str, add: bool) {
        let before = Quantity::from_decimal(&self.balance(who, wallet), 36).unwrap();
        let value = Quantity::from_decimal(amount, 36).unwrap();
        let after = if add {
            before.checked_add(&value)
        } else {
            before.checked_sub(&value)
        }
        .unwrap();
        self.wallets
            .lock()
            .unwrap()
            .insert((who.into(), wallet.into()), after.to_decimal(36));
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
            assert_eq!(credential.as_str(), "synthetic-sub");
            "sub"
        };
        use BinanceEndpoint as E;
        Ok(match r.endpoint {
            E::Account => self.rows(who),
            E::SubAccountSpotAssets => {
                assert_eq!(who, "main");
                assert_eq!(r.get("email"), Some("sub@example.invalid"));
                self.rows("sub")
            }
            E::SubAccountList => {
                json!({"subAccounts":[{"email":"sub@example.invalid","isFreeze":false,"subAccountId":2}]})
            }
            E::ApiRestrictions => {
                json!({"enableReading":true,"enableFutures":true,"enableInternalTransfer":self.permissions.load(Ordering::SeqCst) && self.internal_permission.load(Ordering::SeqCst),"permitsUniversalTransfer":self.permissions.load(Ordering::SeqCst)})
            }
            E::FuturesAccountConfig => {
                json!({"dualSidePosition":false,"multiAssetsMargin":false,"canDeposit":self.can_transfer.load(Ordering::SeqCst),"canWithdraw":self.can_transfer.load(Ordering::SeqCst),"canTrade":true})
            }
            E::FuturesAccount => {
                json!({"assets":[{"asset":"USDT","walletBalance":self.balance(who,"USDT_FUTURE"),"availableBalance":"0","maxWithdrawAmount":self.withdraw_limit.lock().unwrap().clone()}]})
            }
            E::WalletTransfer => {
                assert_eq!(
                    who, "sub",
                    "Self route must use the selected child credential"
                );
                assert!(r.get("clientTranId").is_none());
                let (from, to) = if r.get("type") == Some("MAIN_UMFUTURE") {
                    ("SPOT", "USDT_FUTURE")
                } else {
                    ("USDT_FUTURE", "SPOT")
                };
                let amount = r.get("amount").unwrap();
                self.change(who, from, amount, false);
                self.change(who, to, amount, true);
                let id = self.posts.fetch_add(1, Ordering::SeqCst) + 1;
                self.history.lock().unwrap().push(json!({"tranId":id,"asset":"USDT","amount":amount,"type":r.get("type"),"status":"CONFIRMED","timestamp":chrono::Utc::now().timestamp_millis()}));
                if self.timeout.swap(false, Ordering::SeqCst) {
                    return Err(TransportError::Timeout);
                }
                json!({"tranId":id})
            }
            E::WalletTransferHistory => {
                assert_eq!(who, "sub");
                assert_eq!(r.get("size"), Some("100"));
                let mut rows: Vec<Value> = self
                    .history
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|row| row["type"] == r.get("type").unwrap())
                    .filter(|row| {
                        let timestamp = row["timestamp"].as_u64().unwrap();
                        timestamp >= r.get("startTime").unwrap().parse::<u64>().unwrap()
                            && timestamp <= r.get("endTime").unwrap().parse::<u64>().unwrap()
                    })
                    .cloned()
                    .collect();
                match self.history_mode.load(Ordering::SeqCst) {
                    1 => rows.clear(),
                    2 => {
                        if !rows.is_empty() {
                            rows.push(rows[0].clone());
                        }
                    }
                    3 => {
                        if !rows.is_empty() {
                            rows[0]["type"] = json!("wrong");
                        }
                    }
                    4 => {
                        if !rows.is_empty() {
                            rows[0]["amount"] = json!("11");
                        }
                    }
                    5 => return Ok(json!({"total":101,"rows":rows})),
                    6 => {
                        if !rows.is_empty() {
                            rows[0]["tranId"] = json!(999);
                        }
                    }
                    7 => {
                        if !rows.is_empty() {
                            rows[0]["timestamp"] = json!(1);
                        }
                    }
                    8 if !rows.is_empty() => {
                        rows[0]["asset"] = json!("USDC");
                    }
                    9 => return Ok(json!({"total":0})),
                    10 => return Ok(json!({"total":1})),
                    _ => {}
                }
                json!({"total":rows.len(),"rows":rows})
            }
            E::UniversalTransfer => {
                assert_eq!(who, "main", "Margin transfer must use master authority");
                let target = if r.get("fromEmail").is_some() {
                    assert_eq!(r.get("fromEmail"), r.get("toEmail"));
                    "sub"
                } else {
                    "main"
                };
                let amount = r.get("amount").unwrap();
                self.change(target, r.get("fromAccountType").unwrap(), amount, false);
                self.change(target, r.get("toAccountType").unwrap(), amount, true);
                let id = self.posts.fetch_add(1, Ordering::SeqCst) + 1;
                let email = if target == "main" {
                    "master@example.invalid"
                } else {
                    "sub@example.invalid"
                };
                self.history.lock().unwrap().push(json!({"tranId":id,"clientTranId":r.get("clientTranId"),"asset":"USDT","amount":amount,"fromAccountType":r.get("fromAccountType"),"toAccountType":r.get("toAccountType"),"fromEmail":email,"toEmail":email,"status":"SUCCESS","createTimeStamp":chrono::Utc::now().timestamp_millis()}));
                if self.timeout.swap(false, Ordering::SeqCst) {
                    return Err(TransportError::Timeout);
                }
                json!({"tranId":id})
            }
            E::UniversalTransferHistory => {
                assert_eq!(who, "main");
                assert!(r.get("toEmail").is_none());
                if let Some(email) = r.get("fromEmail") {
                    assert_eq!(email, "sub@example.invalid");
                }
                let mut rows: Vec<Value> = self
                    .history
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|row| row["clientTranId"] == r.get("clientTranId").unwrap())
                    .cloned()
                    .collect();
                match self.history_mode.load(Ordering::SeqCst) {
                    1 => rows.clear(),
                    2 => {
                        if !rows.is_empty() {
                            rows.push(rows[0].clone());
                        }
                    }
                    3 => {
                        if !rows.is_empty() {
                            rows[0]["toEmail"] = json!("wrong@example.invalid");
                        }
                    }
                    4 => {
                        if !rows.is_empty() {
                            rows[0]["amount"] = json!("11");
                        }
                    }
                    5 => {
                        return Ok(json!({"totalCount":501,"result":rows}));
                    }
                    _ => {}
                }
                json!({"totalCount":rows.len(),"result":rows})
            }
            other => panic!("unexpected synthetic endpoint {other:?}"),
        })
    }
}
struct Prices;
#[async_trait]
impl BinancePriceProvider for Prices {
    async fn quote_usd(&self, coin: &str) -> Result<PriceQuote> {
        assert_eq!(coin, "USDT");
        Ok(PriceQuote {
            usd_per_unit: "1.01".into(),
            observed_at: now(),
            source: "synthetic-price".into(),
        })
    }
}
#[derive(Default)]
struct Approval {
    count: AtomicUsize,
    deny: AtomicBool,
}
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.count.fetch_add(1, Ordering::SeqCst);
        if self.deny.load(Ordering::SeqCst) {
            return Err(GoFinError::validation("Synthetic approval denied"));
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
fn adapter(v: Arc<Venue>) -> Arc<BinanceAdapter> {
    configured_adapter(v, false)
}
fn configured_adapter(v: Arc<Venue>, standalone: bool) -> Arc<BinanceAdapter> {
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
                        parent: (!standalone).then(|| "main".into()),
                        email: (!standalone).then(|| "sub@example.invalid".into()),
                        wallet_type: "SPOT".into(),
                    },
                ]
                .into_iter()
                .filter(|a| !standalone || a.name == "sub")
                .collect(),
                destinations: vec![],
            },
            v,
            Some(Arc::new(Prices)),
            None,
        )
        .unwrap(),
    )
}
fn engine(store: Store, a: Arc<BinanceAdapter>, auth: Arc<Approval>) -> Engine {
    Engine::new(
        store,
        vec![a],
        auth,
        EngineOptions {
            max_movement_and_cost_usd_micros: "100000000".into(),
            max_run_allocation_usd_micros: "100000000".into(),
            ..Default::default()
        },
    )
    .unwrap()
}
fn request(id: &str, account: &str, direction: &str) -> OperationRequest {
    OperationRequest {
        request_id: id.into(),
        operation: "binance.futures.transfer".into(),
        run_id: None,
        input: json!({"request_id":id,"authority":"main","account":account,"direction":direction,"amount":"10"}),
    }
}

#[tokio::test]
async fn main_and_child_margin_roundtrips_settle_exact_wallet_transfers_with_fresh_approval() {
    for account in ["main", "sub"] {
        let v = Arc::new(Venue::new());
        let a = adapter(v.clone());
        let auth = Arc::new(Approval::default());
        let e = engine(Store::in_memory().unwrap(), a, auth.clone());
        for (id, direction) in [
            ("deposit", "SPOT_TO_FUTURES"),
            ("return", "FUTURES_TO_SPOT"),
        ] {
            let req = request(id, account, direction);
            let result = e.execute(req.clone()).await.unwrap();
            assert_eq!(result.status, Status::Completed, "{result:?}");
            let effects = &result.effects.as_ref().unwrap().effects;
            assert!(
                matches!(&effects[..],[FinancialEffect::Transfer{debit,credit,..}] if debit==credit)
            );
            assert_eq!(e.execute(req).await.unwrap().status, Status::Completed);
        }
        assert_eq!(v.balance(account, "SPOT"), "100");
        assert_eq!(v.balance(account, "USDT_FUTURE"), "50");
        assert_eq!(v.posts.load(Ordering::SeqCst), 2);
        assert_eq!(auth.count.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn lost_response_restart_recovers_original_transfer_without_reapproval_or_replay() {
    let v = Arc::new(Venue::new());
    v.timeout.store(true, Ordering::SeqCst);
    let a = adapter(v.clone());
    let auth = Arc::new(Approval::default());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("synthetic-margin.sqlite");
    let req = request("lost-margin", "sub", "SPOT_TO_FUTURES");
    let e = engine(Store::open(&path).unwrap(), a.clone(), auth.clone());
    assert_eq!(
        e.execute(req.clone()).await.unwrap().status,
        Status::Unknown
    );
    drop(e);
    let e = engine(Store::open(&path).unwrap(), a, auth.clone());
    e.recover_interrupted().unwrap();
    assert_eq!(e.execute(req).await.unwrap().status, Status::Completed);
    assert_eq!(v.posts.load(Ordering::SeqCst), 1);
    assert_eq!(auth.count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn missing_duplicate_mismatched_and_incomplete_history_never_settle_or_replay() {
    for mode in 1..=5 {
        let v = Arc::new(Venue::new());
        v.history_mode.store(mode, Ordering::SeqCst);
        let e = engine(
            Store::in_memory().unwrap(),
            adapter(v.clone()),
            Arc::new(Approval::default()),
        );
        let req = request("ambiguous-margin", "main", "SPOT_TO_FUTURES");
        let result = e.execute(req.clone()).await.unwrap();
        assert_eq!(result.status, Status::Unknown, "mode {mode}: {result:?}");
        assert!(result.effects.is_none());
        e.execute(req.clone()).await.unwrap();
        assert_eq!(v.posts.load(Ordering::SeqCst), 1);
        v.history_mode.store(0, Ordering::SeqCst);
        assert_eq!(e.execute(req).await.unwrap().status, Status::Completed);
    }
}

#[tokio::test]
async fn concurrent_futures_wallet_changes_do_not_masquerade_as_transfer_credit() {
    let v = Arc::new(Venue::new());
    v.timeout.store(true, Ordering::SeqCst);
    let e = engine(
        Store::in_memory().unwrap(),
        adapter(v.clone()),
        Arc::new(Approval::default()),
    );
    let req = request("funding-charge", "main", "SPOT_TO_FUTURES");
    assert_eq!(
        e.execute(req.clone()).await.unwrap().status,
        Status::Unknown
    );
    v.change("main", "USDT_FUTURE", "1", false);
    let result = e.execute(req).await.unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(result.effects.is_none());
    assert_eq!(v.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn transfer_permission_withdraw_capacity_and_approval_fail_before_post() {
    for mode in 0..4 {
        let v = Arc::new(Venue::new());
        let auth = Arc::new(Approval::default());
        match mode {
            0 => v.permissions.store(false, Ordering::SeqCst),
            1 => v.can_transfer.store(false, Ordering::SeqCst),
            2 => *v.withdraw_limit.lock().unwrap() = "9".into(),
            _ => auth.deny.store(true, Ordering::SeqCst),
        }
        let e = engine(Store::in_memory().unwrap(), adapter(v.clone()), auth);
        let result = e
            .execute(request("blocked", "main", "FUTURES_TO_SPOT"))
            .await;
        assert!(result.is_err() || result.as_ref().is_ok_and(|r| r.status != Status::Completed));
        assert_eq!(v.posts.load(Ordering::SeqCst), 0);
    }
}

fn self_request(id: &str, direction: &str) -> OperationRequest {
    let mut req = request(id, "sub", direction);
    req.input["authority"] = json!("self");
    req
}

#[tokio::test]
async fn standalone_sub_own_wallet_roundtrip_uses_durable_venue_receipts_and_no_main_permission() {
    let v = Arc::new(Venue::new());
    v.internal_permission.store(false, Ordering::SeqCst);
    let auth = Arc::new(Approval::default());
    let e = engine(
        Store::in_memory().unwrap(),
        configured_adapter(v.clone(), true),
        auth.clone(),
    );
    for (id, direction) in [
        ("self-deposit", "SPOT_TO_FUTURES"),
        ("self-return", "FUTURES_TO_SPOT"),
    ] {
        let req = self_request(id, direction);
        let result = e.execute(req.clone()).await.unwrap();
        assert_eq!(result.status, Status::Completed, "{result:?}");
        assert_eq!(
            result.dispatch_evidence.as_ref().unwrap()["route"],
            "self_wallet"
        );
        let effects = &result.effects.as_ref().unwrap().effects;
        assert!(
            matches!(&effects[..], [FinancialEffect::Transfer{from,to,debit,credit}] if from != to && debit == credit)
        );
        assert_eq!(e.execute(req).await.unwrap().status, Status::Completed);
    }
    assert_eq!(v.balance("sub", "SPOT"), "100");
    assert_eq!(v.balance("sub", "USDT_FUTURE"), "50");
    assert_eq!(v.posts.load(Ordering::SeqCst), 2);
    assert_eq!(auth.count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn self_transfer_response_loss_stays_unknown_after_restart_without_guessing_or_replay() {
    let v = Arc::new(Venue::new());
    v.timeout.store(true, Ordering::SeqCst);
    let a = configured_adapter(v.clone(), true);
    let auth = Arc::new(Approval::default());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("self-transfer.sqlite");
    let req = self_request("self-lost", "SPOT_TO_FUTURES");
    let e = engine(Store::open(&path).unwrap(), a.clone(), auth.clone());
    let result = e.execute(req.clone()).await.unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(result.dispatch_evidence.is_none());
    drop(e);
    let e = engine(Store::open(&path).unwrap(), a, auth.clone());
    e.recover_interrupted().unwrap();
    let result = e.execute(req).await.unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(result.effects.is_none());
    assert_eq!(v.posts.load(Ordering::SeqCst), 1);
    assert_eq!(auth.count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn self_transfer_durable_receipt_recovers_after_restart_and_rejects_bad_history() {
    for mode in 1..=8 {
        let v = Arc::new(Venue::new());
        v.history_mode.store(mode, Ordering::SeqCst);
        let a = configured_adapter(v.clone(), true);
        let auth = Arc::new(Approval::default());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("self-receipt.sqlite");
        let req = self_request("self-receipt", "SPOT_TO_FUTURES");
        let e = engine(Store::open(&path).unwrap(), a.clone(), auth.clone());
        let result = e.execute(req.clone()).await.unwrap();
        assert_eq!(result.status, Status::Unknown, "mode {mode}: {result:?}");
        assert!(result.dispatch_evidence.is_some());
        assert!(result.effects.is_none());
        drop(e);
        v.history_mode.store(0, Ordering::SeqCst);
        let e = engine(Store::open(&path).unwrap(), a, auth.clone());
        e.recover_interrupted().unwrap();
        assert_eq!(e.execute(req).await.unwrap().status, Status::Completed);
        assert_eq!(v.posts.load(Ordering::SeqCst), 1);
        assert_eq!(auth.count.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn self_transfer_rejects_missing_permissions_capacity_approval_and_linked_account() {
    for mode in 0..5 {
        let v = Arc::new(Venue::new());
        let auth = Arc::new(Approval::default());
        match mode {
            0 => v.permissions.store(false, Ordering::SeqCst),
            1 => v.can_transfer.store(false, Ordering::SeqCst),
            2 => *v.withdraw_limit.lock().unwrap() = "9".into(),
            3 => auth.deny.store(true, Ordering::SeqCst),
            _ => {}
        }
        let e = engine(
            Store::in_memory().unwrap(),
            configured_adapter(v.clone(), mode != 4),
            auth,
        );
        let result = e
            .execute(self_request("self-blocked", "FUTURES_TO_SPOT"))
            .await;
        assert!(result.is_err() || result.as_ref().is_ok_and(|r| r.status != Status::Completed));
        assert_eq!(v.posts.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn self_transfer_receipt_cannot_explain_concurrent_futures_balance_change() {
    let v = Arc::new(Venue::new());
    v.history_mode.store(1, Ordering::SeqCst);
    let e = engine(
        Store::in_memory().unwrap(),
        configured_adapter(v.clone(), true),
        Arc::new(Approval::default()),
    );
    let req = self_request("self-concurrent", "SPOT_TO_FUTURES");
    assert_eq!(
        e.execute(req.clone()).await.unwrap().status,
        Status::Unknown
    );
    v.history_mode.store(0, Ordering::SeqCst);
    v.change("sub", "USDT_FUTURE", "1", false);
    let result = e.execute(req).await.unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(result.effects.is_none());
    assert_eq!(v.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn self_transfer_history_allows_whole_second_precision_only_with_exact_receipt() {
    for (offset, expected) in [(-395, Status::Completed), (-2_000, Status::Unknown)] {
        let v = Arc::new(Venue::new());
        v.history_mode.store(1, Ordering::SeqCst);
        let e = engine(
            Store::in_memory().unwrap(),
            configured_adapter(v.clone(), true),
            Arc::new(Approval::default()),
        );
        let req = self_request("second-precision", "SPOT_TO_FUTURES");
        let result = e.execute(req.clone()).await.unwrap();
        assert_eq!(result.status, Status::Unknown);
        assert!(result.dispatch_evidence.is_some());
        let submitted = chrono::DateTime::parse_from_rfc3339(
            result.submission.as_ref().unwrap().data["submittedAt"]
                .as_str()
                .unwrap(),
        )
        .unwrap()
        .timestamp_millis();
        v.history.lock().unwrap()[0]["timestamp"] = json!(submitted + offset);
        v.history_mode.store(0, Ordering::SeqCst);
        let result = e.execute(req.clone()).await.unwrap();
        assert_eq!(result.status, expected, "offset {offset}: {result:?}");
        assert_eq!(e.execute(req).await.unwrap().status, expected);
        assert_eq!(v.posts.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn self_transfer_omitted_rows_with_zero_total_is_empty_evidence_and_never_replayed() {
    for mode in [9, 10] {
        let v = Arc::new(Venue::new());
        v.history_mode.store(mode, Ordering::SeqCst);
        let e = engine(
            Store::in_memory().unwrap(),
            configured_adapter(v.clone(), true),
            Arc::new(Approval::default()),
        );
        let req = self_request("omitted-rows", "SPOT_TO_FUTURES");
        let result = e.execute(req.clone()).await.unwrap();
        assert_eq!(result.status, Status::Unknown);
        assert!(result.effects.is_none());
        let reason = result.last_reconciliation_evidence.as_ref().unwrap()["reason"]
            .as_str()
            .unwrap();
        if mode == 9 {
            assert!(reason.contains("One matching venue receipt"), "{reason}");
        } else {
            assert!(
                reason.contains("Malformed own-wallet history rows"),
                "{reason}"
            );
        }
        assert_eq!(
            e.execute(req.clone()).await.unwrap().status,
            Status::Unknown
        );
        v.history_mode.store(0, Ordering::SeqCst);
        assert_eq!(e.execute(req).await.unwrap().status, Status::Completed);
        assert_eq!(v.posts.load(Ordering::SeqCst), 1);
    }
}
