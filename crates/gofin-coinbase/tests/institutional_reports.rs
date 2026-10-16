use async_trait::async_trait;
use gofin_coinbase::{CoinbaseAccount, CoinbaseProduct, CoinbaseProfile, institutional};
use gofin_domain::{OperationRecord, ResourceId, Status, Submission, SubmissionJournal};
use gofin_exchange::ExchangeProfile;
use gofin_exchange::coinbase::{
    CoinbaseApi, CoinbaseEndpoint, CoinbaseError, CoinbaseRequest, CoinbaseResponse, CoinbaseRoute,
    DispatchGuard,
};
use serde_json::{Value, json};
use sha2::Digest;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Api {
    responses: Mutex<VecDeque<CoinbaseResponse>>,
    seen: Mutex<Vec<(CoinbaseEndpoint, CoinbaseRequest, Option<DispatchGuard>)>>,
}
#[async_trait]
impl CoinbaseApi for Api {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        self.seen.lock().unwrap().push((endpoint, request, guard));
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(CoinbaseError::Transport)
    }
}
fn response(status: u16, body: Value) -> CoinbaseResponse {
    CoinbaseResponse::new(status, serde_json::to_vec(&body).unwrap())
}
fn fixture(responses: Vec<CoinbaseResponse>) -> (CoinbaseProfile, Arc<Api>) {
    let api = Arc::new(Api {
        responses: Mutex::new(responses.into()),
        seen: Mutex::new(Vec::new()),
    });
    let transport: Arc<dyn CoinbaseApi> = api.clone();
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "exchange-main".into(),
            resource: ResourceId("coinbaseexchange:portfolio:profile-1".into()),
            credential_ref: "cred".into(),
            physical_identity: "profile-1".into(),
            identity_verified: true,
            portfolio_id: None,
            product: CoinbaseProduct::Exchange,
        }],
        [("cred".into(), transport)].into(),
    )
    .unwrap();
    (profile, api)
}
fn input() -> Value {
    json!({"account":"exchange-main","requestId":"report-1","reportAt":"2025-02-03T04:05:06Z","format":"csv"})
}
fn report(status: &str) -> Value {
    json!({"id":"report-id-1","type":"balance","status":status,"user_id":"hidden-user","file_url":"https://private-download.invalid/signed","params":{"profile_id":"profile-1","format":"csv","balance":{"datetime":"2025-02-03T04:05:06Z"}},"created_at":"2025-02-03T04:06:00Z","file_count":"1"})
}
fn record(
    prepared: gofin_domain::PreparedOperation,
    submission: Submission,
    code: u16,
    body: Value,
) -> OperationRecord {
    let bytes = serde_json::to_vec(&body).unwrap();
    OperationRecord {
        id: "report-operation".into(),
        request_id: "report-1".into(),
        fingerprint: "fingerprint".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
        status: Status::Submitted,
        created_at: gofin_domain::now(),
        updated_at: gofin_domain::now(),
        input: prepared.input.clone(),
        preview: Some(prepared),
        approval: None,
        authorization_source: None,
        submission: Some(submission),
        dispatch_evidence: Some(
            json!({"httpStatus":code,"bodyHex":hex::encode(&bytes),"bodySha256":hex::encode(sha2::Sha256::digest(&bytes))}),
        ),
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    }
}

#[tokio::test]
async fn balance_report_is_closed_bound_to_profile_and_finishes_from_captured_id_and_native_readback()
 {
    let (profile, api) = fixture(vec![response(200, report("ready"))]);
    let prepared =
        institutional::prepare(&profile, "coinbaseexchange.create-balance-report", &input())
            .await
            .unwrap()
            .unwrap();
    assert_eq!(prepared.binding["physicalIdentity"], "profile-1");
    let plan = institutional::plan_submission(&profile, &prepared)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(plan.request.endpoint, CoinbaseRoute::ExchangeReportCreate);
    let body: Value = serde_json::from_str(&plan.request.exact_body).unwrap();
    assert_eq!(
        body,
        json!({"type":"balance","format":"csv","profile_id":"profile-1","balance":{"datetime":"2025-02-03T04:05:06Z"}})
    );
    let submission = plan.submission;
    let operation = record(
        prepared,
        submission.clone(),
        202,
        json!({"id":"report-id-1","type":"balance","status":"pending"}),
    );
    let result = institutional::reconcile(&profile, &operation, &submission)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, Status::Completed, "{:#?}", result.evidence);
    assert_eq!(result.evidence["report"]["id"], "report-id-1");
    assert_eq!(
        result.evidence["report"]["fileUrl"],
        "https://private-download.invalid/signed"
    );
    assert_eq!(api.seen.lock().unwrap().len(), 1);
    assert!(matches!(
        api.seen.lock().unwrap()[0].0,
        CoinbaseRoute::NativeRead { .. }
    ));
}

#[tokio::test]
async fn missing_report_receipt_id_stays_unknown_without_scanning_or_replaying() {
    let (profile, api) = fixture(vec![]);
    let prepared =
        institutional::prepare(&profile, "coinbaseexchange.create-balance-report", &input())
            .await
            .unwrap()
            .unwrap();
    let submission = institutional::plan_submission(&profile, &prepared)
        .await
        .unwrap()
        .unwrap()
        .submission;
    let operation = record(
        prepared,
        submission.clone(),
        200,
        json!({"type":"balance","status":"pending"}),
    );
    let result = institutional::reconcile(&profile, &operation, &submission)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert_eq!(api.seen.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn report_pending_native_status_remains_pending_until_file_is_ready() {
    let (profile, _) = fixture(vec![response(200, report("pending"))]);
    let prepared =
        institutional::prepare(&profile, "coinbaseexchange.create-balance-report", &input())
            .await
            .unwrap()
            .unwrap();
    let plan = institutional::plan_submission(&profile, &prepared)
        .await
        .unwrap()
        .unwrap();
    let submitted = record(
        prepared,
        plan.submission.clone(),
        202,
        json!({"id":"report-id-1","type":"balance","status":"pending"}),
    );
    let result = institutional::reconcile(&profile, &submitted, &plan.submission)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, Status::Pending);
}

#[tokio::test]
async fn report_metadata_read_rejects_cross_profile_and_returns_only_whitelisted_fields() {
    let (profile, api) = fixture(vec![response(200, report("ready"))]);
    let result = institutional::read(
        &profile,
        "coinbaseexchange.get-report",
        &json!({"account":"exchange-main","reportId":"report-id-1"}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result["id"], "report-id-1");
    assert!(result.get("user_id").is_none());
    assert!(result.get("email").is_none());
    assert_eq!(api.seen.lock().unwrap().len(), 1);

    let (profile, _) = fixture(vec![response(200, {
        let mut r = report("ready");
        r["params"]["profile_id"] = json!("other-profile");
        r
    })]);
    assert!(
        institutional::read(
            &profile,
            "coinbaseexchange.get-report",
            &json!({"account":"exchange-main","reportId":"report-id-1"})
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn report_creation_rejects_future_timestamps_and_unknown_fields_before_io() {
    let (profile, api) = fixture(vec![]);
    let mut future = input();
    future["reportAt"] = json!("2999-01-01T00:00:00Z");
    assert!(
        institutional::prepare(&profile, "coinbaseexchange.create-balance-report", &future)
            .await
            .is_err()
    );
    let mut extra = input();
    extra["url"] = json!("https://attacker.invalid/file");
    assert!(
        institutional::prepare(&profile, "coinbaseexchange.create-balance-report", &extra)
            .await
            .is_err()
    );
    assert!(api.seen.lock().unwrap().is_empty());
}

struct GuardRejectingApi {
    seen: Mutex<Vec<(CoinbaseEndpoint, CoinbaseRequest, bool)>>,
    http_writes: AtomicUsize,
}
#[async_trait]
impl CoinbaseApi for GuardRejectingApi {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        let allowed = guard.as_ref().is_some_and(|check| check());
        self.seen.lock().unwrap().push((endpoint, request, allowed));
        if !allowed {
            return Err(CoinbaseError::GuardRejected);
        }
        self.http_writes.fetch_add(1, Ordering::SeqCst);
        Err(CoinbaseError::Transport)
    }
}

#[derive(Default)]
struct ExpiringJournal {
    authorization_checks: AtomicUsize,
    evidence: Mutex<Vec<Value>>,
}
impl SubmissionJournal for ExpiringJournal {
    fn assert_authorized(&self) -> gofin_domain::Result<()> {
        if self.authorization_checks.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(())
        } else {
            Err(gofin_domain::GoFinError::validation(
                "Synthetic report approval expired before dispatch.",
            ))
        }
    }
    fn assert_candidate(&self, _: &gofin_domain::PreparedOperation) -> gofin_domain::Result<()> {
        Ok(())
    }
    fn persist_submission(&self, _: &Submission) -> gofin_domain::Result<()> {
        Ok(())
    }
    fn persist_dispatch_evidence(
        &self,
        _: &Submission,
        evidence: &Value,
    ) -> gofin_domain::Result<()> {
        self.evidence.lock().unwrap().push(evidence.clone());
        Ok(())
    }
}

#[tokio::test]
async fn expired_report_dispatch_certificate_reconciles_from_store_without_get_or_replay() {
    let api = Arc::new(GuardRejectingApi {
        seen: Mutex::new(Vec::new()),
        http_writes: AtomicUsize::new(0),
    });
    let transport: Arc<dyn CoinbaseApi> = api.clone();
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "exchange-main".into(),
            resource: ResourceId("coinbaseexchange:portfolio:profile-1".into()),
            credential_ref: "cred".into(),
            physical_identity: "profile-1".into(),
            identity_verified: true,
            portfolio_id: None,
            product: CoinbaseProduct::Exchange,
        }],
        [("cred".into(), transport)].into(),
    )
    .unwrap();
    let prepared =
        institutional::prepare(&profile, "coinbaseexchange.create-balance-report", &input())
            .await
            .unwrap()
            .unwrap();
    let refreshed = profile.refresh_prepared(&prepared).await.unwrap();
    assert_eq!(refreshed.operation, prepared.operation);
    assert_eq!(refreshed.input, prepared.input);
    assert_eq!(refreshed.binding, prepared.binding);
    let planned = institutional::plan_submission(&profile, &refreshed)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(planned.request.candidate, refreshed);

    let mut store = gofin_store::Store::in_memory().unwrap();
    let pending = OperationRecord {
        id: "report-operation".into(),
        request_id: "report-1".into(),
        fingerprint: "fingerprint".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
        status: Status::Submitted,
        created_at: gofin_domain::now(),
        updated_at: gofin_domain::now(),
        input: prepared.input.clone(),
        preview: Some(prepared.clone()),
        approval: None,
        authorization_source: None,
        submission: Some(planned.submission.clone()),
        dispatch_evidence: None,
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    store
        .create_request(
            &pending.id,
            &pending.request_id,
            &pending.fingerprint,
            &pending.operation,
            pending.run_id.as_deref(),
            "submitted",
            &serde_json::to_value(&pending).unwrap(),
            &pending.created_at,
        )
        .unwrap();
    let submission_row = gofin_store::SubmissionRow {
        kind: planned.submission.kind.clone(),
        reference: planned.submission.reference.clone(),
        data: planned.submission.data.clone(),
    };
    store
        .persist_submission(&pending.id, &submission_row, &gofin_domain::now())
        .unwrap();

    let journal = Arc::new(ExpiringJournal::default());
    let error = profile
        .dispatch(&planned.request, journal.clone())
        .await
        .unwrap_err();
    // Keep the journal's original rejection code while attaching Coinbase's
    // no-dispatch guard diagnostic to its details/evidence.
    assert_eq!(error.code, "INVALID_INPUT");
    let certificate = journal.evidence.lock().unwrap()[0].clone();
    assert_eq!(
        certificate["finNoDispatch"]["reason"],
        "transport_guard_rejected"
    );
    assert_eq!(
        certificate["finNoDispatch"]["operation"],
        prepared.operation
    );
    store
        .persist_dispatch_evidence(&pending.id, &submission_row, &certificate)
        .unwrap();

    let submitted: OperationRecord =
        serde_json::from_value(store.get_record(&pending.id).unwrap().unwrap()).unwrap();
    assert_eq!(submitted.dispatch_evidence.as_ref(), Some(&certificate));
    let failed = profile
        .reconcile_submission(&submitted, &planned.submission)
        .await
        .unwrap();
    assert_eq!(failed.status, Status::Failed);
    let proof = failed
        .verified
        .expect("validated empty-effect certificate proof");
    assert!(proof.effects.is_empty());
    assert_eq!(proof.submission, planned.submission);

    let calls = api.seen.lock().unwrap();
    assert_eq!(calls.len(), 1, "reconciliation must not call Coinbase");
    assert_eq!(calls[0].0, CoinbaseRoute::ExchangeReportCreate);
    assert_eq!(calls[0].0.method(), "POST");
    assert!(calls.iter().all(|(route, _, _)| route.method() != "GET"));
    assert_eq!(api.http_writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn exchange_lending_overview_is_a_scoped_read_not_cash_balance() {
    let native = json!({
        "overview":{"open_loan_value":"100.00","collateral_value":"200.00","available_to_borrow":"50.00","available_per_asset":{"BTC":{"available":"0.1","notional":"5000"}}},
        "loans":[{"id":"loan-1","currency":"USD","outstanding_principal_amount":"100.00","interest_currency":"USD","status":"open"}]
    });
    let (profile, api) = fixture(vec![response(200, native.clone())]);
    let output = institutional::read(
        &profile,
        "coinbaseexchange.get-lending-overview",
        &json!({"account":"exchange-main"}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(output["profileId"], "profile-1");
    assert_eq!(output["lendingOverview"], native);
    let seen = api.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, CoinbaseRoute::ExchangeLoanLendingOverview);
    assert_eq!(seen[0].0.method(), "GET");
    assert_eq!(seen[0].0.path().unwrap(), "/loans/lending-overview");
    assert!(!seen[0].0.mutating());
}
