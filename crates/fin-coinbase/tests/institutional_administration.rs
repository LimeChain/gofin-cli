use async_trait::async_trait;
use fin_coinbase::{CoinbaseAccount, CoinbaseProduct, CoinbaseProfile, institutional};
use fin_domain::{ExecutionActor, OperationKind, OperationRecord, ResourceId, Status};
use fin_exchange::coinbase::{
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

struct Api(Mutex<VecDeque<CoinbaseResponse>>, AtomicUsize);
#[async_trait]
impl CoinbaseApi for Api {
    async fn request(
        &self,
        _: CoinbaseEndpoint,
        _: CoinbaseRequest,
        _: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(CoinbaseError::Transport)
    }
}
fn response(body: Value) -> CoinbaseResponse {
    CoinbaseResponse::new(200, serde_json::to_vec(&body).unwrap())
}
fn make_profile(api: Arc<Api>) -> CoinbaseProfile {
    let transport: Arc<dyn CoinbaseApi> = api;
    CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "exchange".into(),
            resource: ResourceId("coinbaseexchange:profile:profile-1".into()),
            credential_ref: "key".into(),
            physical_identity: "profile-1".into(),
            identity_verified: true,
            portfolio_id: None,
            product: CoinbaseProduct::Exchange,
        }],
        [("key".into(), transport)].into(),
    )
    .unwrap()
}

#[test]
fn exchange_profile_routes_are_closed_and_use_native_methods() {
    let routes = [
        (CoinbaseRoute::ExchangeProfiles, "GET", "/profiles", false),
        (
            CoinbaseRoute::ExchangeProfile {
                profile_id: "p/1".into(),
            },
            "GET",
            "/profiles/p%2F1",
            false,
        ),
        (
            CoinbaseRoute::ExchangeProfileCreate,
            "POST",
            "/profiles",
            true,
        ),
        (
            CoinbaseRoute::ExchangeProfileRename {
                profile_id: "p1".into(),
            },
            "PUT",
            "/profiles/p1",
            true,
        ),
    ];
    for (route, method, path, mutating) in routes {
        assert_eq!(route.method(), method);
        assert_eq!(route.path().unwrap(), path);
        assert_eq!(route.mutating(), mutating);
    }
}

#[tokio::test]
async fn profile_create_and_rename_are_human_configuration_with_exact_bindings() {
    let definitions = institutional::definitions();
    for (operation, route) in [
        ("coinbaseexchange.create-profile", "POST /profiles"),
        (
            "coinbaseexchange.rename-profile",
            "PUT /profiles/{profile_id}",
        ),
    ] {
        let definition = definitions
            .iter()
            .find(|definition| definition.name == operation)
            .expect("profile definition");
        assert_eq!(definition.kind, OperationKind::Configuration);
        assert_eq!(definition.execution_actor, ExecutionActor::HumanInteractive);
        assert_eq!(definition.semantics["route"], route);
    }

    let rows = json!([
        {"id":"profile-1","user_id":"user-1","name":"old-name","active":true,"is_default":false,"created_at":"2025-01-01T00:00:00Z"}
    ]);
    let api = Arc::new(Api(
        Mutex::new(VecDeque::from([response(rows.clone()), response(rows)])),
        AtomicUsize::new(0),
    ));
    let profile = make_profile(api);
    let create = institutional::prepare(
        &profile,
        "coinbaseexchange.create-profile",
        &json!({"account":"exchange","requestId":"create-1","name":"new-profile"}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(create.binding["physicalIdentity"], "profile-1");
    assert_eq!(
        create.binding["requestBodyExact"],
        "{\"name\":\"new-profile\"}"
    );
    let mut second_create = create.clone();
    second_create.input["requestId"] = json!("create-1-retry");
    second_create.binding["requestId"] = json!("create-1-retry");
    let first_submission = institutional::plan_submission(&profile, &create)
        .await
        .unwrap()
        .unwrap()
        .submission;
    let second_submission = institutional::plan_submission(&profile, &second_create)
        .await
        .unwrap()
        .unwrap()
        .submission;
    assert_ne!(first_submission.reference, second_submission.reference);
    let rename = institutional::prepare(&profile, "coinbaseexchange.rename-profile", &json!({"account":"exchange","requestId":"rename-1","profileId":"profile-1","name":"renamed"})).await.unwrap().unwrap();
    assert_eq!(rename.binding["profileId"], "profile-1");
    assert_eq!(rename.binding["previousName"], "old-name");
    assert_eq!(
        rename.binding["requestBodyExact"],
        "{\"name\":\"renamed\",\"profile_id\":\"profile-1\"}"
    );
}

#[tokio::test]
async fn profile_names_reject_whitespace_controls_and_reserved_names_before_io() {
    let api = Arc::new(Api(Mutex::new(VecDeque::new()), AtomicUsize::new(0)));
    let profile = make_profile(api.clone());
    for name in [
        "   ",
        " leading",
        "trailing ",
        "line\nbreak",
        "default",
        "margin",
    ] {
        let result = institutional::prepare(
            &profile,
            "coinbaseexchange.create-profile",
            &json!({"account":"exchange","requestId":"bad-name","name":name}),
        )
        .await;
        assert!(
            result.is_err(),
            "invalid profile name {name:?} must be rejected"
        );
    }
    for name in ["\tname", "name\r"] {
        let result = institutional::prepare(
            &profile,
            "coinbaseexchange.rename-profile",
            &json!({"account":"exchange","requestId":"bad-rename","profileId":"profile-1","name":name}),
        )
        .await;
        assert!(
            result.is_err(),
            "invalid profile name {name:?} must be rejected"
        );
    }
    assert_eq!(api.1.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn profile_create_reconciles_only_captured_receipt_and_same_user_exact_readback() {
    let enrolled = json!([{"id":"profile-1","user_id":"user-1","name":"old-name","active":true,"is_default":false,"created_at":"2025-01-01T00:00:00Z"}]);
    let created = json!({"id":"profile-2","user_id":"user-1","name":"new-profile","active":true,"is_default":false,"created_at":"2026-01-01T00:00:00Z"});
    let api = Arc::new(Api(
        Mutex::new(VecDeque::from([
            response(enrolled.clone()),
            response(created.clone()),
        ])),
        AtomicUsize::new(0),
    ));
    let profile = make_profile(api);
    let prepared = institutional::prepare(
        &profile,
        "coinbaseexchange.create-profile",
        &json!({"account":"exchange","requestId":"create-2","name":"new-profile"}),
    )
    .await
    .unwrap()
    .unwrap();
    let planned = institutional::plan_submission(&profile, &prepared)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        planned.request.endpoint,
        CoinbaseRoute::ExchangeProfileCreate
    );
    assert_eq!(planned.request.exact_body, "{\"name\":\"new-profile\"}");
    let receipt = serde_json::to_vec(&created).unwrap();
    let record = OperationRecord {
        id: "profile-create-operation".into(),
        request_id: "create-2".into(),
        fingerprint: "fingerprint".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
        status: Status::Submitted,
        created_at: fin_domain::now(),
        updated_at: fin_domain::now(),
        input: prepared.input.clone(),
        preview: Some(prepared),
        approval: None,
        submission: Some(planned.submission.clone()),
        dispatch_evidence: Some(
            json!({"httpStatus":200,"bodyHex":hex::encode(&receipt),"bodySha256":hex::encode(sha2::Sha256::digest(&receipt))}),
        ),
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let result = institutional::reconcile(&profile, &record, &planned.submission)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, Status::Completed);
    assert_eq!(result.evidence["nativeProfileId"], "profile-2");

    let other_user =
        json!({"id":"profile-2","user_id":"different-user","name":"new-profile","active":true});
    let api = Arc::new(Api(
        Mutex::new(VecDeque::from([response(enrolled), response(other_user)])),
        AtomicUsize::new(0),
    ));
    let profile = make_profile(api);
    let prepared = institutional::prepare(
        &profile,
        "coinbaseexchange.create-profile",
        &json!({"account":"exchange","requestId":"create-3","name":"new-profile"}),
    )
    .await
    .unwrap()
    .unwrap();
    let planned = institutional::plan_submission(&profile, &prepared)
        .await
        .unwrap()
        .unwrap();
    let receipt = serde_json::to_vec(&created).unwrap();
    let record = OperationRecord {
        id: "profile-create-other-user".into(),
        request_id: "create-3".into(),
        fingerprint: "fingerprint".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
        status: Status::Submitted,
        created_at: fin_domain::now(),
        updated_at: fin_domain::now(),
        input: prepared.input.clone(),
        preview: Some(prepared),
        approval: None,
        submission: Some(planned.submission.clone()),
        dispatch_evidence: Some(
            json!({"httpStatus":200,"bodyHex":hex::encode(&receipt),"bodySha256":hex::encode(sha2::Sha256::digest(&receipt))}),
        ),
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let result = institutional::reconcile(&profile, &record, &planned.submission)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, Status::Unknown);
}

#[tokio::test]
async fn profile_create_rejects_empty_native_id_without_readback() {
    let enrolled = json!([{"id":"profile-1","user_id":"user-1","name":"old-name","active":true}]);
    let api = Arc::new(Api(
        Mutex::new(VecDeque::from([response(enrolled)])),
        AtomicUsize::new(0),
    ));
    let profile = make_profile(api.clone());
    let prepared = institutional::prepare(
        &profile,
        "coinbaseexchange.create-profile",
        &json!({"account":"exchange","requestId":"create-empty-id","name":"new-profile"}),
    )
    .await
    .unwrap()
    .unwrap();
    let planned = institutional::plan_submission(&profile, &prepared)
        .await
        .unwrap()
        .unwrap();
    let receipt = json!({"id":"","user_id":"user-1","name":"new-profile","active":true});
    let bytes = serde_json::to_vec(&receipt).unwrap();
    let record = OperationRecord {
        id: "profile-create-empty-id".into(),
        request_id: "create-empty-id".into(),
        fingerprint: "fingerprint".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
        status: Status::Submitted,
        created_at: fin_domain::now(),
        updated_at: fin_domain::now(),
        input: prepared.input.clone(),
        preview: Some(prepared),
        approval: None,
        submission: Some(planned.submission.clone()),
        dispatch_evidence: Some(json!({
            "httpStatus":200,
            "bodyHex":hex::encode(&bytes),
            "bodySha256":hex::encode(sha2::Sha256::digest(&bytes))
        })),
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let result = institutional::reconcile(&profile, &record, &planned.submission)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert_eq!(api.1.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn profile_reconcile_rejects_cross_paired_or_tampered_submissions_before_readback() {
    let enrolled = json!([{"id":"profile-1","user_id":"user-1","name":"old-name","active":true}]);
    let api = Arc::new(Api(
        Mutex::new(VecDeque::from([response(enrolled)])),
        AtomicUsize::new(0),
    ));
    let profile = make_profile(api.clone());
    let prepared = institutional::prepare(
        &profile,
        "coinbaseexchange.create-profile",
        &json!({"account":"exchange","requestId":"create-4","name":"new-profile"}),
    )
    .await
    .unwrap()
    .unwrap();
    let planned = institutional::plan_submission(&profile, &prepared)
        .await
        .unwrap()
        .unwrap();
    let receipt = json!({"id":"profile-2","user_id":"user-1","name":"new-profile","active":true});
    let bytes = serde_json::to_vec(&receipt).unwrap();
    let original = OperationRecord {
        id: "profile-create-operation".into(),
        request_id: "create-4".into(),
        fingerprint: "fingerprint".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
        status: Status::Submitted,
        created_at: fin_domain::now(),
        updated_at: fin_domain::now(),
        input: prepared.input.clone(),
        preview: Some(prepared),
        approval: None,
        submission: Some(planned.submission.clone()),
        dispatch_evidence: Some(
            json!({"httpStatus":200,"bodyHex":hex::encode(&bytes),"bodySha256":hex::encode(sha2::Sha256::digest(&bytes))}),
        ),
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };

    let mut missing_durable_submission = original.clone();
    missing_durable_submission.submission = None;
    let result =
        institutional::reconcile(&profile, &missing_durable_submission, &planned.submission)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(result.status, Status::Unknown);

    let mut mismatched_operation = original.clone();
    mismatched_operation.operation = "coinbaseexchange.rename-profile".into();
    let result = institutional::reconcile(&profile, &mismatched_operation, &planned.submission)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, Status::Unknown);

    for mutation in ["kind", "reference", "product", "operation", "binding"] {
        let mut tampered = planned.submission.clone();
        match mutation {
            "kind" => tampered.kind = "another-provider".into(),
            "reference" => tampered.reference.push_str("-other"),
            "product" => tampered.data["product"] = json!("coinbase"),
            "operation" => {
                tampered.data["operation"] = json!("coinbaseexchange.create-fills-report")
            }
            "binding" => tampered.data["binding"]["name"] = json!("different-name"),
            _ => unreachable!(),
        }
        let mut record = original.clone();
        record.submission = Some(tampered.clone());
        let result = institutional::reconcile(&profile, &record, &tampered)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            result.status,
            Status::Unknown,
            "tampered {mutation} must fail closed"
        );
    }
    assert_eq!(
        api.1.load(Ordering::SeqCst),
        1,
        "cross-pairing must be rejected before native profile GET"
    );
}
