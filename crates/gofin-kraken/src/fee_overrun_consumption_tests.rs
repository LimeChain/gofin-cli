//! Full native trade/ledger evidence remains a budget charge without a grant.
use super::*;
fn engine(
    path: &std::path::Path,
    remote: Option<Arc<Fixture>>,
) -> (gofin_engine::Engine, Arc<Fixture>) {
    let (venue, wire) = if let Some(wire) = remote {
        let account = KrakenAccount {
            name: "spot".into(),
            resource: ResourceId("kraken:synthetic:spot".into()),
            credential_ref: "synthetic-only".into(),
            physical_identity: "synthetic-uid".into(),
            wallet_account_id: None,
            default_wallet_account_id: None,
            identity_verified: true,
            product: KrakenProduct::Spot,
        };
        (
            ExchangeAdapter::new(
                KrakenProfile::new(KrakenProduct::Spot, vec![account], wire.clone())
                    .unwrap()
                    .with_prices(Arc::new(Prices)),
            ),
            wire,
        )
    } else {
        setup("closed", "0.01", "400", "1.6", false, false, false)
    };
    (
        gofin_engine::Engine::new(
            gofin_store::Store::open(path).unwrap(),
            vec![Arc::new(ObservedAdapter {
                venue,
                base_balance: "1",
            })],
            Arc::new(SyntheticApproval),
            gofin_engine::EngineOptions {
                max_movement_and_cost_usd_micros: "500000000".into(),
                max_run_allocation_usd_micros: "500000000".into(),
                allow_unpriced_exchange_assets: false,
            },
        )
        .unwrap(),
        wire,
    )
}
#[tokio::test]
async fn native_fee_overrun_retains_exact_excess_budget_without_cash_credit_authority_or_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fee-overrun.sqlite");
    let (engine, wire) = engine(&path, None);
    let mut i = input();
    i["type"] = json!("sell");
    i["requestId"] = json!("synthetic-overrun-recovery");
    let request = gofin_domain::OperationRequest {
        request_id: "synthetic-overrun-recovery".into(),
        operation: KrakenEndpoint::KrakenPrivatePostAddOrder.operation().into(),
        input: i,
        run_id: None,
    };
    let record = engine.execute(request.clone()).await.unwrap();
    assert_eq!(record.status, Status::Unknown);
    assert!(record.effects.is_none());
    let preview = record.preview.clone();
    let approval = record.approval.clone();
    let submission = record.submission.clone();
    let proof = gofin_domain::unresolved_consumption::read(
        record.last_reconciliation_evidence.as_ref().unwrap(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(proof.submission, submission.clone().unwrap());
    assert!(
        proof
            .costs
            .iter()
            .all(|effect| matches!(effect, FinancialEffect::Cost { .. }))
    );
    let again = engine.status(&record.id, true).await.unwrap();
    assert_eq!(again.preview, preview);
    assert_eq!(again.approval, approval);
    assert_eq!(again.submission, submission);
    assert!(again.effects.is_none());
    drop(engine);
    let (engine, _) = self::engine(&path, Some(wire.clone()));
    let restored = engine.execute(request).await.unwrap();
    assert_eq!(restored.id, record.id);
    assert!(
        gofin_domain::unresolved_consumption::read(
            restored.last_reconciliation_evidence.as_ref().unwrap()
        )
        .unwrap()
        .is_some()
    );
    assert_eq!(
        wire.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(endpoint, _)| *endpoint == KrakenEndpoint::KrakenPrivatePostAddOrder)
            .count(),
        1
    );
}
