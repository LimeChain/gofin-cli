//! Controlled expiry and timestamp precision; all financial adapter behavior is real.
use async_trait::async_trait;
use gofin_bridge::*;
use gofin_domain::Result;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct Provider {
    pub inner: super::bridge_fixture::Erc20Provider,
    pub expire_first: bool,
    pub calls: AtomicUsize,
}

#[async_trait]
impl BridgeProvider for Provider {
    fn capabilities(&self) -> BridgeCapabilities {
        self.inner.capabilities()
    }

    async fn quote(&self, request: &BridgeQuoteRequest) -> Result<BridgeQuoteBatch> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let mut batch = self.inner.quote(request).await?;
        if self.expire_first && call == 0 {
            // The caller lets this genuinely saved quote expire before executing.
            // Replacement quotes retain the provider fixture's normal finite TTL.
            for quote in &mut batch.quotes {
                quote.valid_until =
                    (chrono::Utc::now() + chrono::Duration::seconds(5)).to_rfc3339();
            }
        }
        Ok(batch)
    }

    fn prepare(
        &self,
        quote: &BridgeQuote,
        bounds: &BridgeBounds,
        wallet: &str,
    ) -> Result<PreparedBridge> {
        self.inner.prepare(quote, bounds, wallet)
    }

    async fn reconcile(&self, transfer: &BridgeTransferRef) -> Result<BridgeReconciliation> {
        self.inner.reconcile(transfer).await
    }
}
