//! Real Binance adapter/engine/store integration with synthetic venue evidence.
use super::*;
use gofin_domain::boundaries::{BoundaryAuthorizationRequest, BoundaryAuthorizationStatus};
use gofin_domain::product_names::MetadataValueExt;

struct CaptureBoundaryApproval {
    challenge: Mutex<Option<AuthorizationChallenge>>,
}
#[async_trait]
impl Authorizer for CaptureBoundaryApproval {
    async fn authorize(
        &self,
        challenge: &AuthorizationChallenge,
    ) -> gofin_domain::Result<ApprovalAudit> {
        *self.challenge.lock().unwrap() = Some(challenge.clone());
        Ok(ApprovalAudit {
            mechanism: "SYNTHETIC-test-only".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

#[tokio::test]
async fn consent_and_biometric_deadlines_are_independent_of_quote_preview_expiry() {
    let venue = Venue::new(vec![]);
    let source = prices();
    let capture = Arc::new(CaptureBoundaryApproval {
        challenge: Mutex::new(None),
    });
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter(venue.clone(), source.clone())],
        capture.clone(),
        EngineOptions::default(),
    )
    .unwrap();
    let authorization = engine
        .authorize_boundary(grant(
            input("SYNTHETIC-lifetime-template", "BUY", false),
            "SYNTHETIC-lifetime-grant",
        ))
        .await
        .unwrap();
    let challenge = capture.challenge.lock().unwrap().clone().unwrap();
    let prompt_expiry = chrono::DateTime::parse_from_rfc3339(&challenge.expires_at).unwrap();
    assert!(
        prompt_expiry
            .signed_duration_since(chrono::Utc::now())
            .num_seconds()
            > 80,
        "The biometric prompt gets its own 90s deadline, not the 60s candidate preview deadline."
    );
    let approved = chrono::DateTime::parse_from_rfc3339(&authorization.approved_at).unwrap();
    let consent_expiry = chrono::DateTime::parse_from_rfc3339(&authorization.expires_at).unwrap();
    assert_eq!(
        consent_expiry.signed_duration_since(approved).num_seconds(),
        600
    );
    assert_eq!(venue.posts.load(Ordering::SeqCst), 0);
}

fn grant(input: Value, id: &str) -> BoundaryAuthorizationRequest {
    BoundaryAuthorizationRequest {
        request_id: id.into(),
        operation: "binance.spot.order-place".into(),
        input,
        bounds: json!({"maximum_spend":"40", "maximum_price":"2100", "maximum_fees":{"ETH":"0.00005"}}),
        valid_for_seconds: 600,
        replaces: None,
    }
}

#[tokio::test]
async fn replacement_preview_shows_new_remaining_and_rejects_spent_caps_before_touch_id() {
    let venue = Venue::new(vec![Fill::partial()]);
    let approvals = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue,
        prices(),
        approvals.clone(),
        Store::in_memory().unwrap(),
    );
    let first = engine
        .authorize_boundary(grant(
            input("SYNTHETIC-used-template", "BUY", false),
            "SYNTHETIC-used-grant",
        ))
        .await
        .unwrap();
    let result = engine
        .execute_boundary(
            &first.id,
            request(input("SYNTHETIC-used-partial", "BUY", false), false),
        )
        .await
        .unwrap();
    assert_eq!(result.status, Status::Completed, "{:?}", result.error);
    let mut remainder = input("SYNTHETIC-used-remainder", "BUY", false);
    remainder["quantity"] = json!("0.005");
    let mut replacement = grant(remainder, "SYNTHETIC-used-replacement");
    replacement.replaces = Some(first.id.clone());
    replacement.bounds["maximum_spend"] = json!("50");
    let preview = engine
        .preview_boundary_authorization(replacement.clone())
        .await
        .unwrap();
    assert_eq!(
        preview["replacement_progress"]["remaining_debits"][0]["amount"]["atoms"],
        gofin_domain::Quantity::from_decimal("30", 36)
            .unwrap()
            .as_str()
    );
    replacement.bounds["maximum_spend"] = json!("10");
    assert!(engine.authorize_boundary(replacement).await.is_err());
    assert_eq!(
        approvals.calls.load(Ordering::SeqCst),
        1,
        "Impossible replacement must be rejected before another approval."
    );
    assert_eq!(
        engine
            .boundary_status(&first.id)
            .await
            .unwrap()
            .authorization
            .status,
        BoundaryAuthorizationStatus::Active
    );
}

#[tokio::test]
async fn partial_completion_does_not_double_charge_the_rolling_budget_at_dispatch() {
    let venue = Venue::new(vec![Fill::partial(), Fill::full("ETH", "0.00001")]);
    let source = prices();
    let approvals = approve(ApprovalMode::Accept);
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter(venue.clone(), source.clone())],
        approvals.clone(),
        EngineOptions {
            max_run_allocation_usd_micros: "45000000".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let authorization = engine
        .authorize_boundary(grant(
            input("SYNTHETIC-budget-template", "BUY", false),
            "SYNTHETIC-budget-grant",
        ))
        .await
        .unwrap();
    let first = engine
        .execute_boundary(
            &authorization.id,
            request(input("SYNTHETIC-budget-partial", "BUY", false), false),
        )
        .await
        .unwrap();
    assert_eq!(first.status, Status::Completed, "{:?}", first.error);
    let mut remainder = input("SYNTHETIC-budget-remainder", "BUY", false);
    remainder["quantity"] = json!("0.01");
    let second = engine
        .execute_boundary(&authorization.id, request(remainder, false))
        .await
        .unwrap();
    assert_eq!(second.status, Status::Completed, "{:?}", second.error);
    assert_eq!(engine.allocation_used_usd_micros().unwrap(), "40040000");
    assert_eq!(venue.posts.load(Ordering::SeqCst), 2);
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn boundaries_complete_partial_fill_with_one_approval_and_no_replayed_quantity() {
    let venue = Venue::new(vec![Fill::partial(), Fill::full("ETH", "0.00001")]);
    let authorizer = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        prices(),
        authorizer.clone(),
        Store::in_memory().unwrap(),
    );
    let first = input("SYNTHETIC-boundary-first", "BUY", false);
    let authorization = engine
        .authorize_boundary(grant(first.clone(), "SYNTHETIC-partial-grant"))
        .await
        .unwrap();
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 0);
    let placed = engine
        .execute_boundary(&authorization.id, request(first.clone(), false))
        .await
        .unwrap();
    assert_eq!(placed.status, Status::Completed, "{placed:?}");
    let status = engine.boundary_status(&authorization.id).await.unwrap();
    assert_eq!(
        status.authorization.remaining_debits[0]
            .amount
            .atoms
            .to_decimal(36),
        "20"
    );
    assert_eq!(
        status.authorization.spent_costs[0]
            .amount
            .atoms
            .to_decimal(36),
        "0.00001"
    );
    assert_eq!(
        status.authorization.remaining_costs[0]
            .amount
            .atoms
            .to_decimal(36),
        "0.00004"
    );
    let mut second = input("SYNTHETIC-boundary-second", "BUY", false);
    second["quantity"] = json!("0.01");
    let placed_second = engine
        .execute_boundary(&authorization.id, request(second, false))
        .await
        .unwrap();
    assert_eq!(placed_second.status, Status::Completed, "{placed_second:?}");
    let status = engine.boundary_status(&authorization.id).await.unwrap();
    assert_eq!(
        status.authorization.remaining_debits[0]
            .amount
            .atoms
            .to_decimal(36),
        "0"
    );
    assert_eq!(
        status.authorization.spent_costs[0]
            .amount
            .atoms
            .to_decimal(36),
        "0.00002"
    );
    assert_eq!(
        status.authorization.status,
        gofin_domain::boundaries::BoundaryAuthorizationStatus::Exhausted
    );
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 2);
    let replay = engine
        .execute_boundary(&authorization.id, request(first, false))
        .await
        .unwrap();
    assert_eq!(replay.id, placed.id);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn boundaries_reprice_without_freezing_incidental_usd_estimate() {
    let venue = Venue::new(vec![Fill::full("ETH", "0.00001")]);
    let source = prices();
    let authorizer = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        source.clone(),
        authorizer.clone(),
        Store::in_memory().unwrap(),
    );
    let initial = input("SYNTHETIC-price-template", "BUY", false);
    let authorization = engine
        .authorize_boundary(grant(initial, "SYNTHETIC-reprice-grant"))
        .await
        .unwrap();
    source.changed.store(true, Ordering::SeqCst); // Real quote path: USDT/USD 1 -> 1.01.
    let mut candidate = input("SYNTHETIC-new-price-attempt", "BUY", false);
    candidate["quantity"] = json!("0.0199");
    candidate["limit_price"] = json!("2010");
    let preview = engine
        .preview_boundary(&authorization.id, "binance.spot.order-place", &candidate)
        .await
        .unwrap();
    assert!(
        preview
            .prepared
            .binding
            .product_get(gofin_domain::value_bounds::KEY)
            .unwrap()
            .is_none()
    );
    let outcome = engine
        .execute_boundary(&authorization.id, request(candidate, false))
        .await
        .unwrap();
    assert_eq!(outcome.status, Status::Completed, "{outcome:?}");
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn boundaries_reject_outside_price_scope_and_revoke_without_approval() {
    let venue = Venue::new(vec![]);
    let authorizer = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        prices(),
        authorizer.clone(),
        Store::in_memory().unwrap(),
    );
    let authorization = engine
        .authorize_boundary(grant(
            input("SYNTHETIC-boundary-template", "BUY", false),
            "SYNTHETIC-guard-grant",
        ))
        .await
        .unwrap();
    let mut candidate = input("SYNTHETIC-too-high", "BUY", false);
    candidate["quantity"] = json!("0.01");
    candidate["limit_price"] = json!("2101");
    let error = engine
        .preview_boundary(&authorization.id, "binance.spot.order-place", &candidate)
        .await
        .unwrap_err();
    assert_eq!(error.code, "BOUNDARY_VIOLATION");
    assert!(error.message.contains("maximum_price"));
    let revoked = engine.revoke_boundary(&authorization.id).await.unwrap();
    assert_eq!(
        revoked.authorization.status,
        BoundaryAuthorizationStatus::Revoked
    );
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert!(
        engine
            .execute_boundary(
                &authorization.id,
                request(input("SYNTHETIC-after-revoke", "BUY", false), false)
            )
            .await
            .is_err()
    );
    assert_eq!(venue.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn boundaries_unknown_submission_and_restart_preserve_original_identity() {
    let mut fill = Fill::full("ETH", "0.00001");
    fill.timeout = true;
    fill.pending = true;
    let venue = Venue::new(vec![fill]);
    let authorizer = approve(ApprovalMode::Accept);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("SYNTHETIC-boundary-restart.sqlite");
    let engine = make_engine(
        venue.clone(),
        prices(),
        authorizer.clone(),
        Store::open(&path).unwrap(),
    );
    let first = input("SYNTHETIC-unknown-boundary", "BUY", false);
    let authorization = engine
        .authorize_boundary(grant(first.clone(), "SYNTHETIC-restart-grant"))
        .await
        .unwrap();
    let result = engine
        .execute_boundary(&authorization.id, request(first.clone(), false))
        .await
        .unwrap();
    assert!(
        matches!(result.status, Status::Pending | Status::Unknown),
        "{result:?}"
    );
    assert!(
        engine
            .execute_boundary(
                &authorization.id,
                request(input("SYNTHETIC-conflicting-attempt", "BUY", false), false)
            )
            .await
            .is_err()
    );
    drop(engine);
    let restarted = make_engine(
        venue.clone(),
        prices(),
        authorizer.clone(),
        Store::open(&path).unwrap(),
    );
    let recovered = restarted
        .execute_boundary(&authorization.id, request(first, false))
        .await
        .unwrap();
    assert_eq!(recovered.id, result.id);
    assert_eq!(recovered.submission, result.submission);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
}
