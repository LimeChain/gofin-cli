//! Replacement, restart, exhaustion, and concurrency regressions for durable
//! boundary grants, using only the shared synthetic Binance venue fixture.
use super::*;
use gofin_domain::boundaries::{BoundaryAuthorizationRequest, BoundaryAuthorizationStatus};

fn grant_with(
    input: Value,
    request_id: &str,
    maximum_spend: &str,
    maximum_fee: &str,
    replaces: Option<String>,
    valid_for_seconds: u64,
) -> BoundaryAuthorizationRequest {
    BoundaryAuthorizationRequest {
        request_id: request_id.into(),
        operation: "binance.spot.order-place".into(),
        input,
        bounds: json!({
            "maximum_spend": maximum_spend,
            "maximum_price": "2100",
            "maximum_fees": {"ETH": maximum_fee}
        }),
        valid_for_seconds,
        replaces,
    }
}

fn buy_attempt(request_id: &str, quantity: &str) -> OperationRequest {
    let mut candidate = input(request_id, "BUY", false);
    candidate["quantity"] = json!(quantity);
    request(candidate, false)
}

#[tokio::test]
async fn fresh_replacement_keeps_verified_spend_and_fees_as_cumulative_totals() {
    let venue = Venue::new(vec![Fill::partial(), Fill::full("ETH", "0.00002")]);
    let approvals = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        prices(),
        approvals.clone(),
        Store::in_memory().unwrap(),
    );
    let first_authorization = engine
        .authorize_boundary(grant_with(
            input("SYNTHETIC-replacement-template-1", "BUY", false),
            "SYNTHETIC-replacement-grant-1",
            "40",
            "0.00005",
            None,
            600,
        ))
        .await
        .unwrap();
    let first = engine
        .execute_boundary(
            &first_authorization.id,
            buy_attempt("SYNTHETIC-replacement-partial", "0.02"),
        )
        .await
        .unwrap();
    assert_eq!(first.status, Status::Completed, "{first:?}");

    let second_authorization = engine
        .authorize_boundary(grant_with(
            input("SYNTHETIC-replacement-template-2", "BUY", false),
            "SYNTHETIC-replacement-grant-2",
            "50",
            "0.00008",
            Some(first_authorization.id.clone()),
            600,
        ))
        .await
        .unwrap();
    assert_ne!(second_authorization.id, first_authorization.id);
    assert_eq!(
        second_authorization.intent_id,
        first_authorization.intent_id
    );
    assert_eq!(
        second_authorization.replaces.as_deref(),
        Some(first_authorization.id.as_str())
    );
    assert_eq!(
        second_authorization.spent_debits[0]
            .amount
            .atoms
            .to_decimal(36),
        "20"
    );
    assert_eq!(
        second_authorization.remaining_debits[0]
            .amount
            .atoms
            .to_decimal(36),
        "30"
    );
    assert_eq!(
        second_authorization.spent_costs[0]
            .amount
            .atoms
            .to_decimal(36),
        "0.00001"
    );
    assert_eq!(
        second_authorization.remaining_costs[0]
            .amount
            .atoms
            .to_decimal(36),
        "0.00007"
    );
    assert_eq!(
        engine
            .boundary_status(&first_authorization.id)
            .await
            .unwrap()
            .authorization
            .status,
        BoundaryAuthorizationStatus::Superseded
    );
    let old_grant_attempt = engine
        .execute_boundary(
            &first_authorization.id,
            buy_attempt("SYNTHETIC-old-grant-must-not-dispatch", "0.005"),
        )
        .await;
    assert!(old_grant_attempt.is_err());
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);

    let second = engine
        .execute_boundary(
            &second_authorization.id,
            buy_attempt("SYNTHETIC-replacement-rest", "0.015"),
        )
        .await
        .unwrap();
    assert_eq!(second.status, Status::Completed, "{second:?}");
    let final_status = engine
        .boundary_status(&second_authorization.id)
        .await
        .unwrap();
    let final_grant = &final_status.authorization;
    let linked_attempts = final_status
        .attempts
        .iter()
        .map(|attempt| attempt.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(linked_attempts.contains(first.id.as_str()));
    assert!(linked_attempts.contains(second.id.as_str()));
    assert_eq!(final_grant.intent_id, first_authorization.intent_id);
    assert_eq!(
        final_grant.spent_debits[0].amount.atoms.to_decimal(36),
        "50"
    );
    assert_eq!(
        final_grant.remaining_debits[0].amount.atoms.to_decimal(36),
        "0"
    );
    assert_eq!(
        final_grant.spent_costs[0].amount.atoms.to_decimal(36),
        "0.00003"
    );
    assert_eq!(
        final_grant.remaining_costs[0].amount.atoms.to_decimal(36),
        "0.00005"
    );
    assert_eq!(final_grant.status, BoundaryAuthorizationStatus::Exhausted);
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 2);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn revoked_and_expired_unused_grants_can_each_be_replaced_once() {
    for expire in [false, true] {
        let venue = Venue::new(vec![Fill::full("ETH", "0.00001")]);
        let approvals = approve(ApprovalMode::Accept);
        let engine = make_engine(
            venue.clone(),
            prices(),
            approvals.clone(),
            Store::in_memory().unwrap(),
        );
        let old = engine
            .authorize_boundary(grant_with(
                input("SYNTHETIC-revocable-template", "BUY", false),
                if expire {
                    "SYNTHETIC-expiring-grant"
                } else {
                    "SYNTHETIC-revoked-grant"
                },
                "40",
                "0.00005",
                None,
                if expire { 1 } else { 600 },
            ))
            .await
            .unwrap();
        if expire {
            tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
            assert_eq!(
                engine
                    .boundary_status(&old.id)
                    .await
                    .unwrap()
                    .authorization
                    .status,
                BoundaryAuthorizationStatus::Expired
            );
        } else {
            assert_eq!(
                engine
                    .revoke_boundary(&old.id)
                    .await
                    .unwrap()
                    .authorization
                    .status,
                BoundaryAuthorizationStatus::Revoked
            );
        }
        let replacement = engine
            .authorize_boundary(grant_with(
                input(
                    "SYNTHETIC-replacement-after-inactive-template",
                    "BUY",
                    false,
                ),
                if expire {
                    "SYNTHETIC-replacement-expired"
                } else {
                    "SYNTHETIC-replacement-revoked"
                },
                "40",
                "0.00005",
                Some(old.id.clone()),
                600,
            ))
            .await
            .unwrap();
        assert_eq!(replacement.intent_id, old.intent_id);
        assert_eq!(replacement.replaces.as_deref(), Some(old.id.as_str()));
        assert!(
            engine
                .execute_boundary(
                    &old.id,
                    buy_attempt("SYNTHETIC-inactive-grant-write", "0.005")
                )
                .await
                .is_err()
        );
        let executed = engine
            .execute_boundary(
                &replacement.id,
                buy_attempt("SYNTHETIC-active-replacement-write", "0.005"),
            )
            .await
            .unwrap();
        assert_eq!(executed.status, Status::Completed, "{executed:?}");
        assert_eq!(approvals.calls.load(Ordering::SeqCst), 2);
        assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn fee_or_spend_exhaustion_blocks_later_posts() {
    // A partial fill consumes the entire fee allowance even though spend remains.
    let venue = Venue::new(vec![Fill::partial(), Fill::full("ETH", "0.00001")]);
    let approvals = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        prices(),
        approvals.clone(),
        Store::in_memory().unwrap(),
    );
    let mut fee_template = input("SYNTHETIC-fee-exhaustion-template", "BUY", false);
    fee_template["maximum_fees"]["ETH"] = json!("0.00003");
    let authorization = engine
        .authorize_boundary(grant_with(
            fee_template,
            "SYNTHETIC-fee-exhaustion-grant",
            "40",
            "0.00003",
            None,
            600,
        ))
        .await
        .unwrap();
    assert_eq!(
        engine
            .execute_boundary(
                &authorization.id,
                buy_attempt("SYNTHETIC-fee-exhaustion-first", "0.02")
            )
            .await
            .unwrap()
            .status,
        Status::Completed
    );
    let failed = engine
        .execute_boundary(
            &authorization.id,
            buy_attempt("SYNTHETIC-fee-exhaustion-second", "0.005"),
        )
        .await
        .unwrap()
        .to_owned();
    assert_eq!(failed.status, Status::Failed);
    let error = failed.error.unwrap();
    assert_eq!(error.code, "BOUNDARY_VIOLATION");
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);

    // Separately, the debit total cannot be replenished after it is spent.
    let venue = Venue::new(vec![
        Fill::full("ETH", "0.00001"),
        Fill::full("ETH", "0.00001"),
    ]);
    let engine = make_engine(
        venue.clone(),
        prices(),
        approvals.clone(),
        Store::in_memory().unwrap(),
    );
    let authorization = engine
        .authorize_boundary(grant_with(
            input("SYNTHETIC-spend-exhaustion-template", "BUY", false),
            "SYNTHETIC-spend-exhaustion-grant",
            "40",
            "0.00005",
            None,
            600,
        ))
        .await
        .unwrap();
    assert_eq!(
        engine
            .execute_boundary(
                &authorization.id,
                buy_attempt("SYNTHETIC-spend-exhaustion-first", "0.02")
            )
            .await
            .unwrap()
            .status,
        Status::Completed
    );
    let error = engine
        .execute_boundary(
            &authorization.id,
            buy_attempt("SYNTHETIC-spend-exhaustion-second", "0.005"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "BOUNDARY_AUTHORIZATION_INVALID");
    assert_eq!(
        engine
            .boundary_status(&authorization.id)
            .await
            .unwrap()
            .authorization
            .status,
        BoundaryAuthorizationStatus::Exhausted
    );
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn concurrent_candidates_cannot_spend_the_same_remaining_boundary() {
    let venue = Venue::new(vec![Fill::full("ETH", "0.00001")]);
    let approvals = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        prices(),
        approvals.clone(),
        Store::in_memory().unwrap(),
    );
    let mut template = input("SYNTHETIC-concurrent-template", "BUY", false);
    template["quantity"] = json!("0.01");
    let authorization = engine
        .authorize_boundary(grant_with(
            template,
            "SYNTHETIC-concurrent-grant",
            "20",
            "0.00005",
            None,
            600,
        ))
        .await
        .unwrap();
    let (left, right) = tokio::join!(
        engine.execute_boundary(
            &authorization.id,
            buy_attempt("SYNTHETIC-concurrent-left", "0.0075")
        ),
        engine.execute_boundary(
            &authorization.id,
            buy_attempt("SYNTHETIC-concurrent-right", "0.0075")
        ),
    );
    let left = left.unwrap();
    let right = right.unwrap();
    assert_eq!(
        usize::from(left.status == Status::Completed)
            + usize::from(right.status == Status::Completed),
        1,
        "left={:?} right={:?} posts={}",
        left.status,
        right.status,
        venue.posts.load(Ordering::SeqCst),
    );
    let failure = if left.status == Status::Failed {
        &left
    } else {
        &right
    };
    let error = failure
        .error
        .as_ref()
        .expect("rejected candidate has an error");
    assert_eq!(error.code, "BOUNDARY_VIOLATION");
    assert_eq!(
        error.details.as_ref().unwrap()["boundary_violation"]["constraint"],
        "maximum_debit"
    );
    assert!(failure.submission.is_none());
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 1);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unused_valid_grant_survives_restart_without_touch_id_or_automatic_post() {
    let venue = Venue::new(vec![Fill::full("ETH", "0.00001")]);
    let approvals = approve(ApprovalMode::Accept);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("SYNTHETIC-unused-boundary.sqlite");
    let engine = make_engine(
        venue.clone(),
        prices(),
        approvals.clone(),
        Store::open(&path).unwrap(),
    );
    let authorization = engine
        .authorize_boundary(grant_with(
            input("SYNTHETIC-unused-template", "BUY", false),
            "SYNTHETIC-unused-grant",
            "40",
            "0.00005",
            None,
            600,
        ))
        .await
        .unwrap();
    assert_eq!(venue.posts.load(Ordering::SeqCst), 0);
    drop(engine);

    let restarted = make_engine(
        venue.clone(),
        prices(),
        approvals.clone(),
        Store::open(&path).unwrap(),
    );
    assert_eq!(
        venue.posts.load(Ordering::SeqCst),
        0,
        "restart must never automatically submit a grant"
    );
    let recovered = restarted.boundary_status(&authorization.id).await.unwrap();
    assert_eq!(
        recovered.authorization.status,
        BoundaryAuthorizationStatus::Active
    );
    assert!(recovered.attempts.is_empty());
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 1);
    let write = restarted
        .execute_boundary(
            &authorization.id,
            buy_attempt("SYNTHETIC-unused-after-restart", "0.005"),
        )
        .await
        .unwrap();
    assert_eq!(write.status, Status::Completed, "{write:?}");
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 1);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
}

struct ExtraSpotMarket(Arc<Venue>);

#[async_trait]
impl BinanceTransport for ExtraSpotMarket {
    async fn request(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
    ) -> Result<Value, TransportError> {
        let endpoint = request.endpoint;
        let mut response = self.0.request(credential, request).await?;
        if endpoint == E::SpotExchangeInfo {
            let mut alternate = response["symbols"][0].clone();
            alternate["symbol"] = json!("ETHUSDTALT");
            response["symbols"].as_array_mut().unwrap().push(alternate);
        }
        Ok(response)
    }

    async fn request_authorized(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Value, TransportError> {
        let endpoint = request.endpoint;
        let mut response = self
            .0
            .request_authorized(credential, request, journal)
            .await?;
        if endpoint == E::SpotExchangeInfo {
            let mut alternate = response["symbols"][0].clone();
            alternate["symbol"] = json!("ETHUSDTALT");
            response["symbols"].as_array_mut().unwrap().push(alternate);
        }
        Ok(response)
    }
}

#[tokio::test]
async fn fixed_account_pair_and_side_cannot_change_under_the_same_grant() {
    let venue = Venue::new(vec![]);
    venue
        .state
        .lock()
        .unwrap()
        .spot
        .insert("ETH".into(), units("1"));
    let prices = prices();
    let approvals = approve(ApprovalMode::Accept);
    let mut cfg = config();
    let mut alternate_account = cfg.accounts[0].clone();
    alternate_account.name = "synthetic-alternate".into();
    cfg.accounts.push(alternate_account);
    let adapter = Arc::new(
        BinanceAdapter::new(
            cfg,
            Arc::new(ExtraSpotMarket(venue.clone())),
            Some(prices.clone()),
            None,
        )
        .unwrap(),
    );
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter],
        approvals.clone(),
        EngineOptions {
            max_run_allocation_usd_micros: "1000000000".into(),
            ..Default::default()
        },
    )
    .unwrap()
    .with_cost_origin_price_provider(prices);

    let authorization = engine
        .authorize_boundary(grant_with(
            input("SYNTHETIC-fixed-scope-template", "BUY", false),
            "SYNTHETIC-fixed-scope-grant",
            "40",
            "0.00005",
            None,
            600,
        ))
        .await
        .unwrap();

    let mut changed_account = input("SYNTHETIC-fixed-account", "BUY", false);
    changed_account["account"] = json!("synthetic-alternate");
    assert_fixed_identity_violation(
        engine
            .preview_boundary(
                &authorization.id,
                "binance.spot.order-place",
                &changed_account,
            )
            .await
            .unwrap_err(),
    );

    let mut changed_pair = input("SYNTHETIC-fixed-pair", "BUY", false);
    changed_pair["symbol"] = json!("ETHUSDTALT");
    assert_fixed_identity_violation(
        engine
            .preview_boundary(&authorization.id, "binance.spot.order-place", &changed_pair)
            .await
            .unwrap_err(),
    );

    let changed_side = input("SYNTHETIC-fixed-side", "SELL", false);
    assert_fixed_identity_violation(
        engine
            .preview_boundary(&authorization.id, "binance.spot.order-place", &changed_side)
            .await
            .unwrap_err(),
    );

    assert_eq!(approvals.calls.load(Ordering::SeqCst), 1);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 0);
}

fn assert_fixed_identity_violation(error: GoFinError) {
    assert_eq!(error.code, "BOUNDARY_VIOLATION", "{error:?}");
    let details = error
        .details
        .expect("structured boundary violation details");
    assert_eq!(
        details["boundary_violation"]["constraint"], "fixed identities",
        "{details:?}"
    );
    assert_eq!(
        details["boundary_violation"]["reauthorization_required"], true,
        "{details:?}"
    );
    assert_eq!(
        details["boundary_violation"]["reconciliation_required"], false,
        "{details:?}"
    );
}

struct PrepareBarrier {
    venue: Arc<Venue>,
    armed: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl PrepareBarrier {
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    async fn block_first_spot_market_read(&self, endpoint: E) {
        if endpoint == E::SpotExchangeInfo && self.armed.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
    }
}

#[async_trait]
impl BinanceTransport for PrepareBarrier {
    async fn request(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
    ) -> Result<Value, TransportError> {
        self.block_first_spot_market_read(request.endpoint).await;
        self.venue.request(credential, request).await
    }

    async fn request_authorized(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Value, TransportError> {
        self.block_first_spot_market_read(request.endpoint).await;
        self.venue
            .request_authorized(credential, request, journal)
            .await
    }
}

#[tokio::test]
async fn replacement_cancels_exact_unsubmitted_candidate_before_dispatch_race() {
    let venue = Venue::new(vec![Fill::full("ETH", "0.00001")]);
    let barrier = Arc::new(PrepareBarrier {
        venue: venue.clone(),
        armed: AtomicBool::new(false),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let prices = prices();
    let approvals = approve(ApprovalMode::Accept);
    let adapter = Arc::new(
        BinanceAdapter::new(config(), barrier.clone(), Some(prices.clone()), None).unwrap(),
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory
        .path()
        .join("SYNTHETIC-boundary-replacement-race.sqlite");
    let store = Store::open(&path).unwrap();
    let observer = Store::open(&path).unwrap();
    let engine = Arc::new(
        Engine::new(
            store,
            vec![adapter],
            approvals.clone(),
            EngineOptions {
                max_run_allocation_usd_micros: "1000000000".into(),
                ..Default::default()
            },
        )
        .unwrap()
        .with_cost_origin_price_provider(prices),
    );
    let authorization = engine
        .authorize_boundary(grant_with(
            input("SYNTHETIC-race-template", "BUY", false),
            "SYNTHETIC-race-grant",
            "40",
            "0.00005",
            None,
            600,
        ))
        .await
        .unwrap();

    barrier.arm();
    let first_request = buy_attempt("SYNTHETIC-race-old-candidate", "0.01");
    let first = {
        let engine = engine.clone();
        let authorization_id = authorization.id.clone();
        tokio::spawn(async move {
            engine
                .execute_boundary(&authorization_id, first_request)
                .await
        })
    };
    barrier.entered.notified().await;

    let previous_attempt_id = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Some(row) = observer.by_request("SYNTHETIC-race-old-candidate").unwrap() {
                break row.operation_id;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("candidate persisted before adapter preparation");

    let replacement = {
        let engine = engine.clone();
        let authorization_id = authorization.id.clone();
        let old_id = previous_attempt_id.clone();
        tokio::spawn(async move {
            engine
                .execute_boundary_replacing(
                    &authorization_id,
                    buy_attempt("SYNTHETIC-race-replacement", "0.01"),
                    &old_id,
                )
                .await
        })
    };

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let row = observer
                .by_request("SYNTHETIC-race-old-candidate")
                .unwrap()
                .expect("previous candidate remains durable");
            if row.record["status"] == "cancelled" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("CAS supersession wins before the candidate can dispatch");

    barrier.release.notify_one();
    let first = first.await.unwrap();
    let replacement = replacement.await.unwrap().unwrap();
    assert!(
        first.is_err(),
        "the superseded candidate must not proceed: {first:?}"
    );
    assert_eq!(replacement.status, Status::Completed, "{replacement:?}");
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    assert_eq!(venue.authorized_posts.load(Ordering::SeqCst), 1);
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 1);
    assert!(replacement.submission.is_some());
}
