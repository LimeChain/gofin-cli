use async_trait::async_trait;
use fin_binance::{
    AccountRole, BinanceAccount, BinanceAdapter, BinanceConfig, BinanceEndpoint,
    BinancePriceProvider, BinanceRequest, BinanceTransport, CredentialHandle, PriceQuote,
    TransportError,
};
use fin_domain::{Adapter, Authorizer, Submission, SubmissionJournal};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct Prices;

#[async_trait]
impl BinancePriceProvider for Prices {
    async fn quote_usd(&self, _coin: &str) -> Result<PriceQuote, fin_domain::FinError> {
        Ok(PriceQuote {
            usd_per_unit: "100".into(),
            observed_at: fin_domain::now(),
            source: "synthetic-fixture".into(),
        })
    }
}

#[derive(Clone, Copy)]
enum PostResult {
    Success,
    LoseResponse,
}

struct TransferTransport {
    post_result: PostResult,
    permission: bool,
    child_visible: bool,
    child_free: Mutex<String>,
    master_free: Mutex<String>,
    child_used_for_post: Mutex<Vec<bool>>,
    posts: AtomicUsize,
    history_reads: AtomicUsize,
}

impl TransferTransport {
    fn new(post_result: PostResult) -> Self {
        Self {
            post_result,
            permission: true,
            child_visible: true,
            child_free: Mutex::new("0.5".into()),
            master_free: Mutex::new("1".into()),
            child_used_for_post: Mutex::new(Vec::new()),
            posts: AtomicUsize::new(0),
            history_reads: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl BinanceTransport for TransferTransport {
    async fn request(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
    ) -> Result<Value, TransportError> {
        use BinanceEndpoint as E;
        let is_child = credential.as_str().ends_with("synthetic-child");
        match request.endpoint {
            E::SubAccountList => Ok(if self.child_visible {
                json!({"subAccounts":[{"email":"child@example.invalid","isFreeze":false,"isMaster":false}]})
            } else {
                json!({"subAccounts":[]})
            }),
            E::Account => Ok(json!({
                "accountType":"SPOT","uid":if is_child {"1002"} else {"1001"},
                "balances":[{"asset":"ETH","free":if is_child {self.child_free.lock().unwrap().clone()} else {self.master_free.lock().unwrap().clone()},"locked":"0"}]
            })),
            E::SubAccountSpotAssets => Ok(json!({"balances":[{
                "asset":"ETH","free":self.child_free.lock().unwrap().clone(),"locked":"0"
            }]})),
            E::SubAccountTransferHistory => {
                self.history_reads.fetch_add(1, Ordering::SeqCst);
                if self.posts.load(Ordering::SeqCst) == 0 {
                    Ok(json!([]))
                } else {
                    Ok(
                        json!([{"counterParty":"master","email":"master@example.invalid","type":2,"asset":"ETH","qty":"0.25","fromAccountType":"SPOT","toAccountType":"SPOT","status":"SUCCESS","tranId":123456789,"time":chrono::Utc::now().timestamp_millis()}]),
                    )
                }
            }
            E::ApiRestrictions => Ok(json!({
                "enableReading":true,
                "enableSpotAndMarginTrading":if is_child {self.permission} else {false}
            })),
            E::CapitalConfig => Ok(
                json!([{"coin":"ETH","withdrawAllEnable":true,"networkList":[{
                    "network":"ETH","coin":"ETH","withdrawIntegerMultiple":"0.000001",
                    "withdrawFee":"0","withdrawMin":"0.000001","withdrawMax":"1000"
                }]}]),
            ),
            E::SubToMasterTransfer => {
                self.posts.fetch_add(1, Ordering::SeqCst);
                self.child_used_for_post.lock().unwrap().push(is_child);
                assert_eq!(request.get("asset"), Some("ETH"));
                assert_eq!(request.get("amount"), Some("0.25"));
                assert!(request.get("recvWindow").is_some());
                assert_eq!(
                    request.params.len(),
                    3,
                    "route cannot carry a client identity or recipient"
                );
                *self.child_free.lock().unwrap() = "0.25".into();
                *self.master_free.lock().unwrap() = "1.25".into();
                match self.post_result {
                    PostResult::Success => Ok(json!({"txnId":123456789})),
                    PostResult::LoseResponse => Err(TransportError::Timeout),
                }
            }
            endpoint => panic!("unexpected fixture endpoint {endpoint:?}"),
        }
    }
}

fn config() -> BinanceConfig {
    BinanceConfig {
        accounts: vec![
            BinanceAccount {
                name: "master".into(),
                resource: fin_domain::ResourceId("binance:uid:1001:SPOT".into()),
                credential: CredentialHandle::new("test-only:synthetic-master").unwrap(),
                role: AccountRole::Main,
                parent: None,
                email: None,
                wallet_type: "SPOT".into(),
            },
            BinanceAccount {
                name: "child".into(),
                resource: fin_domain::ResourceId("binance:uid:1002:SPOT".into()),
                credential: CredentialHandle::new("test-only:synthetic-child").unwrap(),
                role: AccountRole::Sub,
                parent: Some("master".into()),
                email: Some("child@example.invalid".into()),
                wallet_type: "SPOT".into(),
            },
        ],
        destinations: vec![],
    }
}

fn input(authority: &str, from: &str, to: &str, request_id: &str) -> Value {
    json!({"requestId":request_id,"authority":authority,"from":from,"to":to,"coin":"ETH","amount":"0.25","fromWallet":"SPOT","toWallet":"SPOT"})
}

struct Journal {
    submission: Mutex<Option<Submission>>,
    evidence: Mutex<Option<Value>>,
    allow: bool,
}

impl SubmissionJournal for Journal {
    fn assert_authorized(&self) -> fin_domain::Result<()> {
        if self.allow {
            Ok(())
        } else {
            Err(fin_domain::FinError::new(
                "APPROVAL_DENIED",
                "approval",
                "Synthetic approval denied.",
                "No write was dispatched.",
            ))
        }
    }
    fn persist_submission(&self, submission: &Submission) -> fin_domain::Result<()> {
        self.submission.lock().unwrap().replace(submission.clone());
        Ok(())
    }
    fn persist_dispatch_evidence(
        &self,
        submission: &Submission,
        evidence: &Value,
    ) -> fin_domain::Result<()> {
        if self.submission.lock().unwrap().as_ref() != Some(submission) {
            return Err(fin_domain::FinError::validation(
                "Receipt does not match the durable submission.",
            ));
        }
        self.evidence.lock().unwrap().replace(evidence.clone());
        Ok(())
    }
}

fn journal(allow: bool) -> Arc<Journal> {
    Arc::new(Journal {
        submission: Mutex::new(None),
        evidence: Mutex::new(None),
        allow,
    })
}

fn adapter<T: BinanceTransport + 'static>(transport: Arc<T>) -> BinanceAdapter {
    BinanceAdapter::new(config(), transport, Some(Arc::new(Prices)), None).unwrap()
}

#[tokio::test]
async fn sub_to_master_uses_child_signature_and_durable_receipt_then_reconciles_exact_history_and_deltas()
 {
    let transport = Arc::new(TransferTransport::new(PostResult::Success));
    let execution_adapter = adapter(transport.clone());
    let mut request = input("child", "child", "master", "sub-success");
    // The durable input keeps the user's decimal spelling; the adapter sends
    // Binance the same exact quantity in canonical decimal form.
    request["amount"] = json!("0.2500");
    let preview = execution_adapter
        .prepare("binance.transfer", &request)
        .await
        .unwrap();
    assert_eq!(preview.binding["route"], "sub_to_master");
    assert_eq!(preview.binding["signer"], "child");
    assert!(preview.binding.get("clientTranId").is_none());
    let journal = journal(true);
    let submission = execution_adapter
        .submit(&preview, journal.clone())
        .await
        .unwrap();
    assert_eq!(submission.reference, preview.binding["localSubmissionId"]);
    assert_eq!(*transport.child_used_for_post.lock().unwrap(), vec![true]);
    assert_eq!(
        *journal.evidence.lock().unwrap(),
        Some(json!({"route":"sub_to_master","venueId":"123456789"}))
    );
    let submitted_at_ms = chrono::Utc::now().timestamp_millis();
    let history_time_ms = submitted_at_ms.div_euclid(1_000) * 1_000;
    let submitted_at = chrono::DateTime::from_timestamp_millis(history_time_ms + 390).unwrap();
    let mut submission = submission;
    submission.data["submittedAt"] = json!(submitted_at.to_rfc3339());
    let history_transport = Arc::new(HistoryTransport::success_at(history_time_ms as u64));
    let receipt_adapter = adapter(history_transport.clone());
    let record = fin_domain::OperationRecord {
        id: "sub-success-op".into(),
        request_id: "sub-success".into(),
        fingerprint: "synthetic".into(),
        fingerprint_version: 1,
        operation: "binance.transfer".into(),
        input: preview.input.clone(),
        status: fin_domain::Status::Submitted,
        created_at: fin_domain::now(),
        updated_at: fin_domain::now(),
        preview: Some(preview),
        approval: None,
        submission: Some(submission.clone()),
        dispatch_evidence: journal.evidence.lock().unwrap().clone(),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let result = receipt_adapter.reconcile(&record).await.unwrap();
    assert_eq!(result.status, fin_domain::Status::Completed);
    assert_eq!(result.verified.unwrap().submission, submission);
    assert_eq!(history_transport.posts.load(Ordering::SeqCst), 0);
    assert_eq!(history_transport.history_reads.load(Ordering::SeqCst), 1);

    let mut changed_amount = record.clone();
    changed_amount.input["amount"] = json!("0.251");
    changed_amount.submission.as_mut().unwrap().data["amount"] = json!("0.251");
    let error = receipt_adapter
        .reconcile(&changed_amount)
        .await
        .unwrap_err();
    assert_eq!(error.code, "RECONCILIATION_INVALID");

    let mismatched_history = Arc::new(HistoryTransport {
        wrong: true,
        ..HistoryTransport::success()
    });
    let mismatch_adapter = adapter(mismatched_history.clone());
    let mismatch = mismatch_adapter.reconcile(&record).await.unwrap();
    assert_eq!(mismatch.status, fin_domain::Status::Unknown);
    assert!(mismatch.verified.is_none());

    for transport in [
        Arc::new(HistoryTransport {
            wrong_transaction_id: true,
            ..HistoryTransport::success()
        }),
        Arc::new(HistoryTransport {
            wrong_quantity: true,
            ..HistoryTransport::success()
        }),
        Arc::new(HistoryTransport {
            wrong_id: true,
            ..HistoryTransport::success()
        }),
    ] {
        let strict_adapter = adapter(transport);
        let result = strict_adapter.reconcile(&record).await.unwrap();
        assert_eq!(result.status, fin_domain::Status::Unknown);
        assert!(result.verified.is_none());
    }
}

struct HistoryTransport {
    posts: AtomicUsize,
    history_reads: AtomicUsize,
    child_free: Mutex<String>,
    master_free: Mutex<String>,
    wrong: bool,
    wrong_transaction_id: bool,
    wrong_quantity: bool,
    timestamp_offset_ms: i64,
    wrong_id: bool,
    history_time_ms: Option<u64>,
}
impl HistoryTransport {
    fn success() -> Self {
        Self {
            posts: AtomicUsize::new(0),
            history_reads: AtomicUsize::new(0),
            child_free: Mutex::new("0.25".into()),
            master_free: Mutex::new("1.25".into()),
            wrong: false,
            wrong_transaction_id: false,
            wrong_quantity: false,
            // Binance may round its history time just before local submission.
            timestamp_offset_ms: -2_000,
            wrong_id: false,
            history_time_ms: None,
        }
    }

    fn success_at(history_time_ms: u64) -> Self {
        Self {
            history_time_ms: Some(history_time_ms),
            ..Self::success()
        }
    }
}
#[async_trait]
impl BinanceTransport for HistoryTransport {
    async fn request(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
    ) -> Result<Value, TransportError> {
        use BinanceEndpoint as E;
        let is_child = credential.as_str().ends_with("synthetic-child");
        match request.endpoint {
            E::SubAccountList => {
                Ok(json!({"subAccounts":[{"email":"child@example.invalid","isFreeze":false}]}))
            }
            E::Account => Ok(
                json!({"accountType":"SPOT","uid":if is_child {"1002"} else {"1001"},"balances":[{"asset":"ETH","free":if is_child {self.child_free.lock().unwrap().clone()} else {self.master_free.lock().unwrap().clone()},"locked":"0"}]}),
            ),
            E::SubAccountSpotAssets => Ok(
                json!({"balances":[{"asset":"ETH","free":self.child_free.lock().unwrap().clone(),"locked":"0"}]}),
            ),
            E::ApiRestrictions => Ok(json!({"enableSpotAndMarginTrading":true})),
            E::SubAccountTransferHistory => {
                self.history_reads.fetch_add(1, Ordering::SeqCst);
                let timestamp = self
                    .history_time_ms
                    .map(|time| time as i64)
                    .unwrap_or_else(|| {
                        chrono::Utc::now().timestamp_millis() + self.timestamp_offset_ms
                    });
                let request_start = request
                    .get("startTime")
                    .and_then(|value| value.parse::<i64>().ok())
                    .unwrap();
                let request_end = request
                    .get("endTime")
                    .and_then(|value| value.parse::<i64>().ok())
                    .unwrap();
                if timestamp < request_start || timestamp > request_end {
                    return Ok(json!([]));
                }
                if let Some(history_time_ms) = self.history_time_ms {
                    let expected_start = (history_time_ms / 1_000 * 1_000).saturating_sub(5_000);
                    assert_eq!(
                        request.get("startTime"),
                        Some(expected_start.to_string().as_str())
                    );
                }
                Ok(
                    json!([{"counterParty":"master","email":"master@example.invalid","type":2,"asset":if self.wrong {"BTC"} else {"ETH"},"qty":if self.wrong_quantity {"0.251"} else {"0.25"},"fromAccountType":"SPOT","toAccountType":"SPOT","status":"SUCCESS","tranId":if self.wrong_transaction_id || self.wrong_id {123456788} else {123456789},"time":timestamp}]),
                )
            }
            endpoint => panic!("unexpected recovery endpoint {endpoint:?}"),
        }
    }
}

struct Deny;
#[async_trait]
impl Authorizer for Deny {
    async fn authorize(
        &self,
        _challenge: &fin_domain::AuthorizationChallenge,
    ) -> fin_domain::Result<fin_domain::ApprovalAudit> {
        Err(fin_domain::FinError::new(
            "APPROVAL_DENIED",
            "approval",
            "Synthetic refusal.",
            "Retry after approval.",
        ))
    }
}

struct Allow(AtomicUsize);
#[async_trait]
impl Authorizer for Allow {
    async fn authorize(
        &self,
        challenge: &fin_domain::AuthorizationChallenge,
    ) -> fin_domain::Result<fin_domain::ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(fin_domain::ApprovalAudit {
            mechanism: "synthetic-fixture-only".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: fin_domain::now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

#[tokio::test]
async fn engine_persists_sub_to_master_receipt_settles_once_and_preserves_it_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("sub_to_master.sqlite");
    let transport = Arc::new(TransferTransport::new(PostResult::Success));
    let adapter = Arc::new(adapter(transport.clone()));
    let authorizer = Arc::new(Allow(AtomicUsize::new(0)));
    let request = fin_domain::OperationRequest {
        request_id: "engine-sub-to-master".into(),
        operation: "binance.transfer".into(),
        input: input("child", "child", "master", "engine-sub-to-master"),
        run_id: None,
    };
    let first_engine = fin_engine::Engine::new(
        fin_store::Store::open(&database).unwrap(),
        vec![adapter.clone()],
        authorizer.clone(),
        fin_engine::EngineOptions::default(),
    )
    .unwrap();
    let first = first_engine.execute(request.clone()).await.unwrap();
    assert_eq!(
        first.status,
        fin_domain::Status::Completed,
        "record={first:#?}"
    );
    assert_eq!(
        first.dispatch_evidence,
        Some(json!({"route":"sub_to_master","venueId":"123456789"}))
    );
    assert_eq!(first.effects.as_ref().unwrap().effects.len(), 1);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
    assert_eq!(authorizer.0.load(Ordering::SeqCst), 1);
    let duplicate = first_engine.execute(request).await.unwrap();
    assert_eq!(duplicate.id, first.id);
    assert_eq!(duplicate.submission, first.submission);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
    drop(first_engine);

    let restarted = fin_engine::Engine::new(
        fin_store::Store::open(&database).unwrap(),
        vec![adapter],
        authorizer.clone(),
        fin_engine::EngineOptions::default(),
    )
    .unwrap();
    let recovered = restarted.status(&first.id, false).await.unwrap();
    assert_eq!(recovered.status, fin_domain::Status::Completed);
    assert_eq!(recovered.submission, first.submission);
    assert_eq!(recovered.dispatch_evidence, first.dispatch_evidence);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
    assert_eq!(authorizer.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn sub_to_master_response_loss_without_receipt_stays_unknown_after_restart_and_never_queries_history()
 {
    let transport = Arc::new(TransferTransport::new(PostResult::LoseResponse));
    let execution_adapter = adapter(transport.clone());
    let preview = execution_adapter
        .prepare(
            "binance.transfer",
            &input("child", "child", "master", "sub-loss"),
        )
        .await
        .unwrap();
    let journal = journal(true);
    assert_eq!(
        execution_adapter
            .submit(&preview, journal.clone())
            .await
            .unwrap_err()
            .code,
        "VENUE_TIMEOUT"
    );
    let original = journal.submission.lock().unwrap().clone().unwrap();
    assert_eq!(journal.evidence.lock().unwrap().as_ref(), None);
    let record = fin_domain::OperationRecord {
        id: "sub-loss-op".into(),
        request_id: "sub-loss".into(),
        fingerprint: "synthetic".into(),
        fingerprint_version: 1,
        operation: "binance.transfer".into(),
        input: preview.input.clone(),
        status: fin_domain::Status::Unknown,
        created_at: fin_domain::now(),
        updated_at: fin_domain::now(),
        preview: Some(preview),
        approval: None,
        submission: Some(original),
        dispatch_evidence: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let recovery = Arc::new(HistoryTransport::success());
    let restarted = adapter(recovery.clone());
    let result = restarted.reconcile(&record).await.unwrap();
    assert_eq!(result.status, fin_domain::Status::Unknown);
    assert_eq!(recovery.history_reads.load(Ordering::SeqCst), 0);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unresolved_sub_to_master_keeps_both_resources_reserved_against_a_new_request_identity() {
    let transport = Arc::new(TransferTransport::new(PostResult::LoseResponse));
    let adapter = Arc::new(adapter(transport.clone()));
    let engine = fin_engine::Engine::new(
        fin_store::Store::in_memory().unwrap(),
        vec![adapter],
        Arc::new(Allow(AtomicUsize::new(0))),
        fin_engine::EngineOptions::default(),
    )
    .unwrap();
    let first_request = fin_domain::OperationRequest {
        request_id: "sub-uncertain-original".into(),
        operation: "binance.transfer".into(),
        input: input("child", "child", "master", "sub-uncertain-original"),
        run_id: None,
    };
    let first = engine.execute(first_request).await.unwrap();
    assert_eq!(first.status, fin_domain::Status::Unknown);
    assert!(first.submission.is_some());
    assert_eq!(first.dispatch_evidence, None);
    let replacement = fin_domain::OperationRequest {
        request_id: "sub-uncertain-replacement".into(),
        operation: "binance.transfer".into(),
        input: input("child", "child", "master", "sub-uncertain-replacement"),
        run_id: None,
    };
    let rejected = engine.execute(replacement).await.unwrap();
    assert_eq!(rejected.status, fin_domain::Status::Failed);
    assert_eq!(rejected.error.unwrap().code, "RESOURCE_RESERVED");
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn sub_to_master_rejects_wrong_hierarchy_route_and_missing_child_permission_before_post() {
    let transport = Arc::new(TransferTransport::new(PostResult::Success));
    let execution_adapter = adapter(transport.clone());
    for (request, code) in [
        (
            input("child", "master", "child", "wrong-way"),
            "WRONG_AUTHORITY",
        ),
        (
            input("child", "child", "child", "not-a-parent-return"),
            "UNSUPPORTED_ROUTE",
        ),
    ] {
        assert_eq!(
            execution_adapter
                .prepare("binance.transfer", &request)
                .await
                .unwrap_err()
                .code,
            code
        );
    }
    let mut wrong_config = config();
    wrong_config.accounts[1].parent = Some("missing-master".into());
    assert!(
        BinanceAdapter::new(
            wrong_config,
            transport.clone(),
            Some(Arc::new(Prices)),
            None
        )
        .is_err()
    );
    let bad_hierarchy = Arc::new(TransferTransport {
        child_visible: false,
        ..TransferTransport::new(PostResult::Success)
    });
    let err = adapter(bad_hierarchy.clone())
        .prepare(
            "binance.transfer",
            &input("child", "child", "master", "missing-hierarchy"),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "HIERARCHY_UNVERIFIED");
    let no_permission = Arc::new(TransferTransport {
        permission: false,
        ..TransferTransport::new(PostResult::Success)
    });
    let err = adapter(no_permission.clone())
        .prepare(
            "binance.transfer",
            &input("child", "child", "master", "no-permission"),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "MISSING_PERMISSION");
    assert_eq!(no_permission.posts.load(Ordering::SeqCst), 0);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn denied_dispatch_does_not_post_and_a_repeated_engine_request_does_not_post_twice() {
    let transport = Arc::new(TransferTransport::new(PostResult::Success));
    let adapter = Arc::new(adapter(transport.clone()));
    let preview = adapter
        .prepare(
            "binance.transfer",
            &input("child", "child", "master", "denied"),
        )
        .await
        .unwrap();
    let denied = journal(false);
    assert_eq!(
        adapter.submit(&preview, denied).await.unwrap_err().code,
        "APPROVAL_DENIED"
    );
    assert_eq!(transport.posts.load(Ordering::SeqCst), 0);

    let engine = fin_engine::Engine::new(
        fin_store::Store::in_memory().unwrap(),
        vec![adapter],
        Arc::new(Deny),
        fin_engine::EngineOptions::default(),
    )
    .unwrap();
    let request = fin_domain::OperationRequest {
        request_id: "engine-denied".into(),
        operation: "binance.transfer".into(),
        input: input("child", "child", "master", "engine-denied"),
        run_id: None,
    };
    let first = engine.execute(request.clone()).await.unwrap();
    let second = engine.execute(request).await.unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 0);
}
