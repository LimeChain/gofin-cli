//! Narrow recovery for Binance's documented withdrawal allowlist rejection.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

const SOURCE_FREE: &str = "0.00394";
const ADDRESS: &str = "0x2222222222222222222222222222222222222222";

#[derive(Clone, Copy)]
enum WithdrawalHistory {
    Empty,
    Matching,
    Unavailable,
}

struct WhitelistRejectionTransport {
    binance_code: i64,
    history: WithdrawalHistory,
    current_free: std::sync::Mutex<String>,
    current_uid: std::sync::Mutex<String>,
    posts: AtomicUsize,
}

impl WhitelistRejectionTransport {
    fn new(binance_code: i64, history: WithdrawalHistory) -> Self {
        Self {
            binance_code,
            history,
            current_free: std::sync::Mutex::new(SOURCE_FREE.into()),
            current_uid: std::sync::Mutex::new("1001".into()),
            posts: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl fin_binance::BinanceTransport for WhitelistRejectionTransport {
    async fn request(
        &self,
        _credential: &fin_binance::CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, fin_binance::TransportError> {
        use fin_binance::{BinanceEndpoint as E, HttpFailureKind};
        match request.endpoint {
            E::ApiRestrictions => Ok(json!({"enableReading":true,"enableWithdrawals":true})),
            E::TravelRuleRequirements => Ok(json!({"questionnaireCountryCode":"NIL"})),
            E::CapitalConfig => Ok(json!([{
                "coin":"ETH","withdrawAllEnable":true,"depositAllEnable":true,
                "networkList":[{
                    "network":"ETH","coin":"ETH","depositEnable":true,
                    "withdrawEnable":true,"withdrawIntegerMultiple":"0.00000001",
                    "withdrawFee":"0.0004","withdrawMin":"0.0001","withdrawMax":"1000",
                    "withdrawTag":false,"busy":false
                }]
            }])),
            E::Account => Ok(json!({
                "uid":self.current_uid.lock().unwrap().clone(),
                "accountType":"SPOT",
                "balances":[{
                    "asset":"ETH","free":self.current_free.lock().unwrap().clone(),"locked":"0"
                }]
            })),
            E::StandardWithdrawal => {
                self.posts.fetch_add(1, Ordering::SeqCst);
                Err(fin_binance::TransportError::HttpFailure {
                    status: 400,
                    binance_code: Some(self.binance_code),
                    kind: HttpFailureKind::Rejected,
                })
            }
            E::WithdrawalHistory => match self.history {
                WithdrawalHistory::Unavailable => Err(fin_binance::TransportError::Timeout),
                WithdrawalHistory::Empty => Ok(json!([])),
                WithdrawalHistory::Matching => Ok(json!([{
                    "id":"synthetic-withdrawal-row","coin":"ETH","amount":"0.001",
                    "transactionFee":"0.0004","network":"ETH","address":ADDRESS,
                    "txId":format!("0x{}","a".repeat(64)),"status":6,
                    "completeTime":"2026-10-09 12:00:00",
                    "withdrawOrderId":request.get("withdrawOrderId"),
                    "transferType":0,"walletType":0
                }])),
            },
            endpoint => panic!("unexpected whitelist rejection endpoint {endpoint:?}"),
        }
    }
}

async fn rejected_withdrawal_record(
    transport: std::sync::Arc<WhitelistRejectionTransport>,
) -> (fin_binance::BinanceAdapter, fin_domain::OperationRecord) {
    use fin_domain::Adapter;
    let mut config = one_main_config();
    config.accounts[0].resource = fin_domain::ResourceId("binance:uid:1001:SPOT".into());
    let adapter = fin_binance::BinanceAdapter::new(
        config,
        transport,
        Some(std::sync::Arc::new(SyntheticPriceProvider)),
        None,
    )
    .unwrap();
    let input = json!({
        "requestId":"synthetic-whitelist-rejection",
        "account":"main-fixture","destination":"","address":ADDRESS,
        "coin":"ETH","amount":"0.001","network":"ETH","walletType":"SPOT",
        "maxFee":"0.001","minReceiveAmount":"0.0005"
    });
    let preview = adapter.prepare("binance.withdraw", &input).await.unwrap();
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let journal = std::sync::Arc::new(RecordingJournal {
        events,
        persisted: std::sync::Mutex::new(Vec::new()),
    });
    let error = adapter.submit(&preview, journal.clone()).await.unwrap_err();
    let submission = journal.persisted.lock().unwrap()[0].clone();
    let record = fin_domain::OperationRecord {
        id: "synthetic-whitelist-rejection-operation".into(),
        request_id: "synthetic-whitelist-rejection".into(),
        fingerprint: "synthetic-withdrawal-fingerprint".into(),
        fingerprint_version: 1,
        operation: "binance.withdraw".into(),
        input,
        status: fin_domain::Status::Unknown,
        created_at: fin_domain::now(),
        updated_at: fin_domain::now(),
        preview: Some(preview),
        approval: None,
        submission: Some(submission),
        dispatch_evidence: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: Some(error),
        run_id: None,
    };
    (adapter, record)
}

#[tokio::test]
async fn only_exact_complete_no_effects_withdrawal_whitelist_rejection_is_terminal() {
    use fin_domain::Adapter;

    let exact_transport = std::sync::Arc::new(WhitelistRejectionTransport::new(
        -4035,
        WithdrawalHistory::Empty,
    ));
    let (adapter, record) = rejected_withdrawal_record(exact_transport.clone()).await;
    assert_eq!(record.error.as_ref().unwrap().code, "VENUE_REQUEST_FAILED");
    assert_eq!(record.error.as_ref().unwrap().stage, "submission");
    assert_eq!(
        record.error.as_ref().unwrap().details.as_deref().unwrap()["http_status"],
        400
    );
    assert_eq!(
        record.error.as_ref().unwrap().details.as_deref().unwrap()["binance_code"],
        -4035
    );
    assert!(
        record
            .submission
            .as_ref()
            .unwrap()
            .data
            .get("venueId")
            .is_none()
    );
    let recovered = adapter.reconcile(&record).await.unwrap();
    assert_eq!(recovered.status, fin_domain::Status::Failed);
    let proof = recovered.verified.unwrap();
    assert!(proof.effects.is_empty());
    assert_eq!(
        proof.evidence["method"],
        "recorded-binance-withdrawal-rejection-v1"
    );
    assert_eq!(
        proof.evidence["proofBasis"],
        "deterministic_whitelist_rejection"
    );
    assert_eq!(proof.evidence["balancesUsedForProof"], false);
    assert_eq!(proof.evidence["matchingHistoryRows"], 0);
    assert_eq!(exact_transport.posts.load(Ordering::SeqCst), 1);

    // Balance changes are unrelated to this deterministic rejection proof;
    // uncertainty and identity mismatches still keep the attempt unresolved.
    let cases = [
        (
            -2010,
            WithdrawalHistory::Empty,
            "unchanged",
            false,
            "other-code",
        ),
        (
            -4035,
            WithdrawalHistory::Unavailable,
            "unchanged",
            false,
            "history-unavailable",
        ),
        (
            -4035,
            WithdrawalHistory::Matching,
            "unchanged",
            false,
            "matching-row",
        ),
        (
            -4035,
            WithdrawalHistory::Empty,
            "balance-decreased",
            false,
            "unrelated-debit",
        ),
        (
            -4035,
            WithdrawalHistory::Empty,
            "balance-increased",
            false,
            "unrelated-credit",
        ),
        (
            -4035,
            WithdrawalHistory::Empty,
            "unchanged",
            true,
            "dispatch-receipt",
        ),
        (
            -4035,
            WithdrawalHistory::Empty,
            "unchanged",
            false,
            "tampered-request-id",
        ),
        (
            -4035,
            WithdrawalHistory::Empty,
            "unchanged",
            false,
            "tampered-preview-request-id",
        ),
        (
            -4035,
            WithdrawalHistory::Empty,
            "unchanged",
            false,
            "tampered-account",
        ),
        (
            -4035,
            WithdrawalHistory::Empty,
            "unchanged",
            false,
            "wrong-current-uid",
        ),
    ];
    for (code, history, balance, dispatch_evidence, label) in cases {
        let transport = std::sync::Arc::new(WhitelistRejectionTransport::new(code, history));
        let (adapter, mut record) = rejected_withdrawal_record(transport.clone()).await;
        match balance {
            "balance-decreased" => *transport.current_free.lock().unwrap() = "0.00294".into(),
            "balance-increased" => *transport.current_free.lock().unwrap() = "0.01659".into(),
            _ => {}
        }
        if label == "wrong-current-uid" {
            *transport.current_uid.lock().unwrap() = "9999".into();
        }
        if dispatch_evidence {
            record.dispatch_evidence = Some(json!({"venueId":"unexpected-receipt"}));
        }
        match label {
            "tampered-request-id" => {
                record.input["requestId"] = json!("different-id");
            }
            "tampered-preview-request-id" => {
                record.preview.as_mut().unwrap().input["requestId"] = json!("different-id");
            }
            "tampered-account" => record.input["account"] = json!("other-account"),
            _ => {}
        }
        let result = adapter.reconcile(&record).await.unwrap();
        if matches!(label, "unrelated-debit" | "unrelated-credit") {
            assert_eq!(result.status, fin_domain::Status::Failed, "{label}");
            let proof = result.verified.expect("deterministic rejection proof");
            assert!(proof.effects.is_empty(), "{label}");
            assert_eq!(proof.evidence["balancesUsedForProof"], false, "{label}");
        } else {
            assert_eq!(result.status, fin_domain::Status::Unknown, "{label}");
            assert!(result.verified.is_none(), "{label}");
        }
        assert_eq!(transport.posts.load(Ordering::SeqCst), 1, "{label}");
    }
}
