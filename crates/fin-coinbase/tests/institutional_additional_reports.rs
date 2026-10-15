use async_trait::async_trait;
use fin_coinbase::{CoinbaseAccount, CoinbaseProduct, CoinbaseProfile, institutional};
use fin_domain::{OperationRecord, ResourceId, Status, Submission};
use fin_exchange::coinbase::{
    CoinbaseApi, CoinbaseEndpoint, CoinbaseError, CoinbaseRequest, CoinbaseResponse, CoinbaseRoute,
    DispatchGuard,
};
use serde_json::{Value, json};
use sha2::Digest;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
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

fn fixture(profile_id: &str, responses: Vec<CoinbaseResponse>) -> (CoinbaseProfile, Arc<Api>) {
    let api = Arc::new(Api {
        responses: Mutex::new(responses.into()),
        seen: Mutex::new(Vec::new()),
    });
    let transport: Arc<dyn CoinbaseApi> = api.clone();
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "exchange-main".into(),
            resource: ResourceId(format!("coinbaseexchange:portfolio:{profile_id}")),
            credential_ref: "cred".into(),
            physical_identity: profile_id.into(),
            identity_verified: true,
            portfolio_id: None,
            product: CoinbaseProduct::Exchange,
        }],
        [("cred".into(), transport)].into(),
    )
    .unwrap();
    (profile, api)
}

fn account(account_id: &str) -> Value {
    json!({"id":account_id,"currency":"USD","profile_id":"profile-1","balance":"1","hold":"0"})
}

fn product(product_id: &str) -> Value {
    json!({"id":product_id,"base_currency":"BTC","quote_currency":"USD","status":"online"})
}

fn report(report_type: &str, scope: Value) -> Value {
    let mut params = json!({"profile_id":"profile-1","format":"csv"});
    params
        .as_object_mut()
        .unwrap()
        .extend(scope.as_object().unwrap().clone());
    json!({
        "id":"report-1",
        "type":report_type,
        "status":"ready",
        "file_url":"https://signed.invalid/path",
        "params":params
    })
}

fn record(
    prepared: fin_domain::PreparedOperation,
    submission: Submission,
    receipt: Value,
) -> OperationRecord {
    let bytes = serde_json::to_vec(&receipt).unwrap();
    OperationRecord {
        id: "additional-report-operation".into(),
        request_id: prepared.binding["requestId"].as_str().unwrap().into(),
        fingerprint: "additional-report-fingerprint".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
        status: Status::Submitted,
        created_at: fin_domain::now(),
        updated_at: fin_domain::now(),
        input: prepared.input.clone(),
        preview: Some(prepared),
        approval: None,
        submission: Some(submission),
        dispatch_evidence: Some(json!({
            "httpStatus":202,
            "bodyHex":hex::encode(&bytes),
            "bodySha256":hex::encode(sha2::Sha256::digest(&bytes))
        })),
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    }
}

#[tokio::test]
async fn account_report_binds_native_account_interval_and_profile_readback() {
    let (profile, api) = fixture(
        "profile-1",
        vec![
            response(200, account("acct-1")),
            response(200, account("acct-1")),
            response(
                200,
                report(
                    "account",
                    json!({
                        "start_date":"2025-01-01T00:00:00Z",
                        "end_date":"2025-01-02T00:00:00Z",
                        "account_id":"acct-1"
                    }),
                ),
            ),
        ],
    );
    let input = json!({
        "account":"exchange-main",
        "requestId":"account-report-1",
        "accountId":"acct-1",
        "startDate":"2025-01-01T00:00:00Z",
        "endDate":"2025-01-02T00:00:00Z",
        "format":"csv"
    });
    let prepared =
        institutional::prepare(&profile, "coinbaseexchange.create-account-report", &input)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(prepared.binding["physicalIdentity"], "profile-1");
    assert_eq!(
        serde_json::from_str::<Value>(prepared.binding["requestBodyExact"].as_str().unwrap())
            .unwrap(),
        json!({
            "type":"account","format":"csv","profile_id":"profile-1",
            "account":{"start_date":"2025-01-01T00:00:00Z","end_date":"2025-01-02T00:00:00Z","account_id":"acct-1"}
        })
    );
    let planned = institutional::plan_submission(&profile, &prepared)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        planned.request.endpoint,
        CoinbaseRoute::ExchangeReportCreate
    );
    assert_eq!(
        planned.request.exact_body,
        prepared.binding["requestBodyExact"]
    );
    let operation = record(
        prepared,
        planned.submission.clone(),
        json!({"id":"report-1","type":"account","status":"pending"}),
    );
    let result = institutional::reconcile(&profile, &operation, &planned.submission)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, Status::Completed, "{:#?}", result.evidence);
    assert_eq!(result.evidence["report"]["accountId"], "acct-1");
    let seen = api.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert!(matches!(seen[0].0, CoinbaseRoute::ExchangeAccount { .. }));
    assert!(matches!(seen[1].0, CoinbaseRoute::ExchangeAccount { .. }));
    assert!(matches!(seen[2].0, CoinbaseRoute::NativeRead { .. }));
}

#[tokio::test]
async fn fills_report_binds_verified_product_interval_and_profile_readback() {
    let (profile, api) = fixture(
        "profile-1",
        vec![
            response(200, product("BTC-USD")),
            response(200, product("BTC-USD")),
            response(
                200,
                report(
                    "fills",
                    json!({
                        "start_date":"2025-01-01T00:00:00Z",
                        "end_date":"2025-01-02T00:00:00Z",
                        "product_id":"BTC-USD"
                    }),
                ),
            ),
        ],
    );
    let input = json!({
        "account":"exchange-main",
        "requestId":"fills-report-1",
        "productId":"BTC-USD",
        "startDate":"2025-01-01T00:00:00Z",
        "endDate":"2025-01-02T00:00:00Z",
        "format":"csv"
    });
    let prepared = institutional::prepare(&profile, "coinbaseexchange.create-fills-report", &input)
        .await
        .unwrap()
        .unwrap();
    let planned = institutional::plan_submission(&profile, &prepared)
        .await
        .unwrap()
        .unwrap();
    let operation = record(
        prepared,
        planned.submission.clone(),
        json!({"id":"report-1","type":"fills","status":"pending"}),
    );
    let result = institutional::reconcile(&profile, &operation, &planned.submission)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, Status::Completed, "{:#?}", result.evidence);
    assert_eq!(result.evidence["report"]["productId"], "BTC-USD");
    assert_eq!(api.seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn additional_report_inputs_are_closed_and_rejected_before_native_io() {
    let (profile, api) = fixture(
        "profile-1",
        vec![
            response(200, product("BTC-USD")),
            response(200, product("BTC-USD")),
        ],
    );
    let mut unknown = json!({
        "account":"exchange-main","requestId":"fills-report-2","productId":"BTC-USD",
        "startDate":"2025-01-01T00:00:00Z","endDate":"2025-01-02T00:00:00Z","format":"csv"
    });
    unknown["email"] = json!("outside@example.invalid");
    assert!(
        institutional::prepare(&profile, "coinbaseexchange.create-fills-report", &unknown)
            .await
            .is_err()
    );
    let future = json!({
        "account":"exchange-main","requestId":"account-report-2","accountId":"acct-1",
        "startDate":"2025-01-01T00:00:00Z","endDate":"2999-01-02T00:00:00Z","format":"csv"
    });
    assert!(
        institutional::prepare(&profile, "coinbaseexchange.create-account-report", &future)
            .await
            .is_err()
    );
    assert!(api.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn report_receipt_hash_mismatch_stays_unknown_without_readback_or_replay() {
    let (profile, api) = fixture(
        "profile-1",
        vec![
            response(200, product("BTC-USD")),
            response(200, product("BTC-USD")),
        ],
    );
    let prepared = institutional::prepare(
        &profile,
        "coinbaseexchange.create-fills-report",
        &json!({
            "account":"exchange-main","requestId":"fills-report-3","productId":"BTC-USD",
            "startDate":"2025-01-01T00:00:00Z","endDate":"2025-01-02T00:00:00Z","format":"csv"
        }),
    )
    .await
    .unwrap()
    .unwrap();
    let planned = institutional::plan_submission(&profile, &prepared)
        .await
        .unwrap()
        .unwrap();
    let seen_before_reconciliation = api.seen.lock().unwrap().len();
    let mut operation = record(
        prepared,
        planned.submission.clone(),
        json!({"id":"report-1","type":"fills","status":"pending"}),
    );
    operation.dispatch_evidence.as_mut().unwrap()["bodySha256"] = json!("0".repeat(64));
    let result = institutional::reconcile(&profile, &operation, &planned.submission)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert_eq!(api.seen.lock().unwrap().len(), seen_before_reconciliation);
}

#[tokio::test]
async fn report_reconciliation_rejects_cross_paired_durable_submission_before_readback() {
    let (profile, api) = fixture(
        "profile-1",
        vec![
            response(200, product("BTC-USD")),
            response(200, product("BTC-USD")),
        ],
    );
    let prepared = institutional::prepare(
        &profile,
        "coinbaseexchange.create-fills-report",
        &json!({
            "account":"exchange-main","requestId":"fills-report-cross-submission","productId":"BTC-USD",
            "startDate":"2025-01-01T00:00:00Z","endDate":"2025-01-02T00:00:00Z","format":"csv"
        }),
    )
    .await
    .unwrap()
    .unwrap();
    let planned = institutional::plan_submission(&profile, &prepared)
        .await
        .unwrap()
        .unwrap();
    let operation = record(
        prepared,
        planned.submission.clone(),
        json!({"id":"report-1","type":"fills","status":"pending"}),
    );
    let mut mismatched = planned.submission;
    mismatched.reference.push_str("-other");
    let seen_before_reconciliation = api.seen.lock().unwrap().len();

    let result = institutional::reconcile(&profile, &operation, &mismatched)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert_eq!(api.seen.lock().unwrap().len(), seen_before_reconciliation);
}

#[tokio::test]
async fn report_reconciliation_rejects_tampered_submission_provenance_before_readback() {
    let (profile, api) = fixture(
        "profile-1",
        vec![
            response(200, product("BTC-USD")),
            response(200, product("BTC-USD")),
        ],
    );
    let prepared = institutional::prepare(
        &profile,
        "coinbaseexchange.create-fills-report",
        &json!({
            "account":"exchange-main","requestId":"fills-report-provenance","productId":"BTC-USD",
            "startDate":"2025-01-01T00:00:00Z","endDate":"2025-01-02T00:00:00Z","format":"csv"
        }),
    )
    .await
    .unwrap()
    .unwrap();
    let planned = institutional::plan_submission(&profile, &prepared)
        .await
        .unwrap()
        .unwrap();
    let original = record(
        prepared,
        planned.submission.clone(),
        json!({"id":"report-1","type":"fills","status":"pending"}),
    );
    let seen_before_reconciliation = api.seen.lock().unwrap().len();

    for field in ["requestId", "profileId"] {
        let mut operation = original.clone();
        let mut tampered = planned.submission.clone();
        tampered.data[field] = json!("another-native-identity");
        operation.submission = Some(tampered.clone());
        let result = institutional::reconcile(&profile, &operation, &tampered)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.status, Status::Unknown, "tampered {field}");
        assert_eq!(api.seen.lock().unwrap().len(), seen_before_reconciliation);
    }
}

#[tokio::test]
async fn report_reconciliation_rejects_preview_operation_mismatch_before_readback() {
    let (profile, api) = fixture(
        "profile-1",
        vec![
            response(200, product("BTC-USD")),
            response(200, product("BTC-USD")),
        ],
    );
    let prepared = institutional::prepare(
        &profile,
        "coinbaseexchange.create-fills-report",
        &json!({
            "account":"exchange-main","requestId":"fills-report-cross-operation","productId":"BTC-USD",
            "startDate":"2025-01-01T00:00:00Z","endDate":"2025-01-02T00:00:00Z","format":"csv"
        }),
    )
    .await
    .unwrap()
    .unwrap();
    let planned = institutional::plan_submission(&profile, &prepared)
        .await
        .unwrap()
        .unwrap();
    let mut operation = record(
        prepared,
        planned.submission.clone(),
        json!({"id":"report-1","type":"fills","status":"pending"}),
    );
    operation.operation = "coinbaseexchange.create-account-report".into();
    let seen_before_reconciliation = api.seen.lock().unwrap().len();

    let result = institutional::reconcile(&profile, &operation, &planned.submission)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert_eq!(api.seen.lock().unwrap().len(), seen_before_reconciliation);
}

#[tokio::test]
async fn otc_rfq_and_tax_invoice_reports_use_exact_documented_product_scope() {
    for (report_type, operation) in [
        ("otc-fills", "coinbaseexchange.create-otc-fills-report"),
        ("rfq-fills", "coinbaseexchange.create-rfq-fills-report"),
        ("tax-invoice", "coinbaseexchange.create-tax-invoice-report"),
    ] {
        let (profile, api) = fixture(
            "profile-1",
            vec![
                response(200, product("BTC-USD")),
                response(200, product("BTC-USD")),
                response(
                    200,
                    report(
                        report_type,
                        json!({
                            "start_date":"2025-01-01T00:00:00Z",
                            "end_date":"2025-01-02T00:00:00Z",
                            "product_id":"BTC-USD"
                        }),
                    ),
                ),
            ],
        );
        let input = json!({
            "account":"exchange-main",
            "requestId":format!("{report_type}-report-1"),
            "productId":"BTC-USD",
            "startDate":"2025-01-01T00:00:00Z",
            "endDate":"2025-01-02T00:00:00Z",
            "format":"csv"
        });
        let prepared = institutional::prepare(&profile, operation, &input)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(prepared.binding["reportType"], report_type);
        let body =
            serde_json::from_str::<Value>(prepared.binding["requestBodyExact"].as_str().unwrap())
                .unwrap();
        let scope_key = match report_type {
            "otc-fills" => "otc_fills",
            "rfq-fills" => "rfq_fills",
            _ => "tax_invoice",
        };
        assert_eq!(body["type"], report_type);
        assert_eq!(
            body[scope_key],
            json!({
                "start_date":"2025-01-01T00:00:00Z",
                "end_date":"2025-01-02T00:00:00Z",
                "product_id":"BTC-USD"
            })
        );
        let planned = institutional::plan_submission(&profile, &prepared)
            .await
            .unwrap()
            .unwrap();
        let operation = record(
            prepared,
            planned.submission.clone(),
            json!({"id":"report-1","type":report_type,"status":"pending"}),
        );
        let result = institutional::reconcile(&profile, &operation, &planned.submission)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.status, Status::Completed, "{:#?}", result.evidence);
        assert_eq!(result.evidence["report"]["productId"], "BTC-USD");
        assert_eq!(api.seen.lock().unwrap().len(), 3);
    }
}

#[tokio::test]
async fn otc_rfq_and_tax_invoice_reports_reject_mismatched_native_scope_without_replay() {
    for (report_type, operation) in [
        ("otc-fills", "coinbaseexchange.create-otc-fills-report"),
        ("rfq-fills", "coinbaseexchange.create-rfq-fills-report"),
        ("tax-invoice", "coinbaseexchange.create-tax-invoice-report"),
    ] {
        for (field, wrong_value) in [
            ("product_id", json!("ETH-USD")),
            ("start_date", json!("2025-01-01T01:00:00Z")),
            ("end_date", json!("2025-01-02T01:00:00Z")),
        ] {
            let scope = json!({
                "start_date":"2025-01-01T00:00:00Z",
                "end_date":"2025-01-02T00:00:00Z",
                "product_id":"BTC-USD"
            });
            let mut mismatched_scope = scope.clone();
            mismatched_scope[field] = wrong_value;
            let (profile, api) = fixture(
                "profile-1",
                vec![
                    response(200, product("BTC-USD")),
                    response(200, product("BTC-USD")),
                    response(200, report(report_type, mismatched_scope)),
                ],
            );
            let input = json!({
                "account":"exchange-main",
                "requestId":format!("{report_type}-scope-mismatch-{field}"),
                "productId":"BTC-USD",
                "startDate":"2025-01-01T00:00:00Z",
                "endDate":"2025-01-02T00:00:00Z",
                "format":"csv"
            });
            let prepared = institutional::prepare(&profile, operation, &input)
                .await
                .unwrap()
                .unwrap();
            let planned = institutional::plan_submission(&profile, &prepared)
                .await
                .unwrap()
                .unwrap();
            let operation_record = record(
                prepared,
                planned.submission.clone(),
                json!({"id":"report-1","type":report_type,"status":"pending"}),
            );
            let result = institutional::reconcile(&profile, &operation_record, &planned.submission)
                .await
                .unwrap()
                .unwrap();

            assert_eq!(
                result.status,
                Status::Unknown,
                "{report_type} readback with mismatched {field} must not complete"
            );
            let seen = api.seen.lock().unwrap();
            assert_eq!(seen.len(), 3, "only preflight reads plus exact report GET");
            assert!(seen.iter().all(|(endpoint, _, _)| !endpoint.mutating()));
            assert!(matches!(seen[2].0, CoinbaseRoute::NativeRead { .. }));
        }
    }
}
