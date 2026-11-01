//! Production-size broker worker stack with a bounded no-dispatch refresh.
use super::*;
use gofin_bridge::{
    BridgeCapabilities, BridgeProvider, BridgeQuote, BridgeQuoteBatch, BridgeQuoteRequest,
    BridgeReconciliation, BridgeTransferRef, PreparedBridge,
};

#[derive(Default)]
pub(super) struct RefreshFault {
    armed: AtomicBool,
    failures: AtomicUsize,
    quotes: AtomicUsize,
}
pub(super) struct Provider {
    pub inner: Arc<CctpProvider>,
    pub fault: Arc<RefreshFault>,
}
#[async_trait]
impl BridgeProvider for Provider {
    fn as_cctp(&self) -> Option<&CctpProvider> {
        Some(&self.inner)
    }
    fn capabilities(&self) -> BridgeCapabilities {
        self.inner.capabilities()
    }
    async fn quote(&self, request: &BridgeQuoteRequest) -> Result<BridgeQuoteBatch> {
        let mut result = self.inner.quote(request).await?;
        if self.fault.quotes.fetch_add(1, Ordering::SeqCst) == 0 {
            for quote in &mut result.quotes {
                quote.valid_until =
                    (chrono::Utc::now() + chrono::Duration::seconds(10)).to_rfc3339();
            }
        }
        Ok(result)
    }
    fn prepare(
        &self,
        quote: &BridgeQuote,
        bounds: &BridgeBounds,
        wallet: &str,
    ) -> Result<PreparedBridge> {
        if self.fault.armed.swap(false, Ordering::SeqCst) {
            self.fault.failures.fetch_add(1, Ordering::SeqCst);
            return Err(GoFinError::new(
                "PREVIEW_EXPIRED",
                "prepare",
                "Controlled candidate expiry before quote persistence.",
                "Refresh the same unsubmitted candidate within existing authority.",
            ));
        }
        self.inner.prepare(quote, bounds, wallet)
    }
    async fn reconcile(&self, transfer: &BridgeTransferRef) -> Result<BridgeReconciliation> {
        self.inner.reconcile(transfer).await
    }
}

#[test]
fn candidate_expiry_refreshes_saved_cctp_on_production_worker_stack() {
    // Only test construction gets a larger stack. Every async poll, including
    // the real public CLI's broker handler, runs on production-sized workers.
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(2 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                tokio::spawn(Box::pin(exercise())).await.unwrap();
            });
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn exercise() {
    let fault = Arc::new(RefreshFault::default());
    let chain = Arc::new(Mutex::new(ChainState {
        source_balance: U256::from(20_000_000),
        refresh_fault: Some(fault.clone()),
        ..Default::default()
    }));
    let h = Harness::build(
        tempfile::tempdir().unwrap(),
        chain.clone(),
        Arc::new(Approvals::default()),
        Arc::new(Iris::new(chain)),
        OffsetClock::realtime(),
    );
    h.chain.lock().unwrap().lose_source_ordinal = Some(1);
    let (id, transfer) = Box::pin(h.start_compound("stack-refresh")).await;
    let _ = h
        .call(
            args(&["status", &id, "--json"]),
            BrokerRequest::Status { id: id.clone() },
        )
        .await;
    let before = h.state.transfer(&transfer).unwrap().unwrap();
    let expires = chrono::DateTime::parse_from_rfc3339(&before.quote_revisions[0].valid_until)
        .unwrap()
        .with_timezone(&chrono::Utc);
    if let Ok(wait) = (expires - chrono::Utc::now()).to_std() {
        tokio::time::sleep(wait + std::time::Duration::from_millis(5)).await;
    }
    fault.armed.store(true, Ordering::SeqCst);
    let _ = Box::pin(h.continue_compound(&id)).await;
    assert_eq!(fault.failures.load(Ordering::SeqCst), 1);
    let refreshed = h.state.transfer(&transfer).unwrap().unwrap();
    assert_eq!(
        refreshed.steps[0].operation_id,
        before.steps[0].operation_id
    );
    assert!(refreshed.quote_revisions.len() > before.quote_revisions.len());
    assert_eq!(h.chain.lock().unwrap().broadcasts, BTreeMap::from([(1, 2)]));
    h.chain.lock().unwrap().attestation_ready = true;
    let _ = Box::pin(h.continue_compound(&id)).await;
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(1, 2), (42161, 1)])
    );
}
