//! Recover positive original-identity proof beyond the first closed-order page.
use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum History {
    LaterPage,
    Missing,
    Ambiguous,
    Repeated,
    Incomplete,
    BeyondBound,
}
struct PagedHistory {
    inner: Arc<Fixture>,
    mode: History,
    offsets: Mutex<Vec<usize>>,
    history_ready: AtomicBool,
}
#[async_trait]
impl KrakenTransport for PagedHistory {
    async fn request(
        &self,
        endpoint: KrakenEndpoint,
        account: Option<&KrakenAccount>,
        parameters: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if matches!(
            endpoint,
            KrakenEndpoint::KrakenPrivatePostOpenOrders
                | KrakenEndpoint::KrakenPrivatePostClosedOrders
        ) && !self.history_ready.load(Ordering::SeqCst)
        {
            return Err(GoFinError::new(
                "KRAKEN_UNAVAILABLE",
                "reconciliation",
                "Synthetic temporary history outage.",
                "Inspect the original operation.",
            ));
        }
        if endpoint != KrakenEndpoint::KrakenPrivatePostClosedOrders {
            return self
                .inner
                .request(endpoint, account, parameters, guard)
                .await;
        }
        assert!(
            parameters["start"].as_i64().is_some(),
            "history is scoped to original submission time"
        );
        let offset = parameters["ofs"].as_u64().unwrap() as usize;
        self.offsets.lock().unwrap().push(offset);
        let mut rows = serde_json::Map::new();
        if offset == 0 || self.mode == History::Repeated || self.mode == History::BeyondBound {
            let start = if self.mode == History::Repeated {
                0
            } else {
                offset
            };
            for n in start..start + 50 {
                rows.insert(
                    format!("UNRELATED-{n}"),
                    json!({"cl_ord_id":format!("unrelated-{n}")}),
                );
            }
        } else {
            match self.mode {
                History::LaterPage | History::Ambiguous => {
                    rows.insert("ORDER-1".into(), self.inner.order(&parameters));
                    if self.mode == History::Ambiguous {
                        rows.insert("ORDER-2".into(), self.inner.order(&parameters));
                    }
                }
                History::Missing => {
                    rows.insert("UNRELATED-50".into(), json!({"cl_ord_id":"unrelated-50"}));
                }
                History::Incomplete => (),
                _ => unreachable!(),
            }
        }
        let count = match self.mode {
            History::Ambiguous => 52,
            History::Repeated => 100,
            History::BeyondBound => offset + 51,
            _ => 51,
        };
        Ok(KrakenResponse::Json(
            json!({"error":[],"result":{"closed":rows,"count":count}}),
        ))
    }
}
fn history_engine(path: &std::path::Path, wire: Arc<PagedHistory>) -> gofin_engine::Engine {
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
    let venue = ExchangeAdapter::new(
        KrakenProfile::new(KrakenProduct::Spot, vec![account], wire)
            .unwrap()
            .with_prices(Arc::new(Prices)),
    );
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
    .unwrap()
}

#[tokio::test]
async fn paginated_original_order_proof_settles_after_sqlite_restart_without_replay() {
    for mode in [
        History::LaterPage,
        History::Missing,
        History::Ambiguous,
        History::Repeated,
        History::Incomplete,
        History::BeyondBound,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("paged-history.sqlite");
        let (_, inner) = setup("closed", "0.01", "200", "0.4", false, false, true);
        let wire = Arc::new(PagedHistory {
            inner,
            mode,
            offsets: Mutex::new(vec![]),
            history_ready: AtomicBool::new(false),
        });
        let engine = history_engine(&path, wire.clone());
        let request = gofin_domain::OperationRequest {
            request_id: format!("synthetic-paged-{mode:?}"),
            operation: KrakenEndpoint::KrakenPrivatePostAddOrder.operation().into(),
            input: input(),
            run_id: None,
        };
        let original = engine.execute(request.clone()).await.unwrap();
        assert_eq!(original.status, Status::Unknown);
        assert!(original.dispatch_evidence.is_none());
        assert!(
            gofin_store::Store::open(&path)
                .unwrap()
                .reservation_count(&original.id)
                .unwrap()
                > 0
        );
        drop(engine);
        wire.history_ready.store(true, Ordering::SeqCst);
        let engine = history_engine(&path, wire.clone());
        engine.recover_interrupted().unwrap();
        let observed = engine.status(&original.id, true).await;
        if mode == History::LaterPage {
            let settled = observed.unwrap();
            assert_eq!(settled.status, Status::Completed, "{:?}", settled.error);
            assert!(settled.effects.is_some());
            assert_eq!(
                gofin_store::Store::open(&path)
                    .unwrap()
                    .reservation_count(&original.id)
                    .unwrap(),
                0
            );
            assert_eq!(*wire.offsets.lock().unwrap(), vec![0, 50]);
        } else {
            if let Ok(record) = observed {
                assert_eq!(record.status, Status::Unknown, "{mode:?}");
            }
            assert!(
                gofin_store::Store::open(&path)
                    .unwrap()
                    .reservation_count(&original.id)
                    .unwrap()
                    > 0,
                "{mode:?}"
            );
            if mode == History::BeyondBound {
                assert_eq!(wire.offsets.lock().unwrap().len(), 64);
            }
        }
        assert_eq!(engine.execute(request).await.unwrap().id, original.id);
        assert_eq!(
            wire.inner
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(endpoint, _)| *endpoint == KrakenEndpoint::KrakenPrivatePostAddOrder)
                .count(),
            1
        );
    }
}
