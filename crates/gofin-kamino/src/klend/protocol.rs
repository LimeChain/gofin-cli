//! Direct, group-zero floating-rate KLend obligations using the pinned official interface.
use super::{native, reads, types::*};
use async_trait::async_trait;
use gofin_domain::{GoFinError, Result};
use gofin_solana::{FinalizedReceiptProof, SolanaRuntime};

#[derive(Default)]
pub struct NativeKlendProtocol;
impl NativeKlendProtocol {
    pub fn new() -> Self {
        Self
    }
}
pub(crate) const PROGRAM: &str = "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD";
pub(crate) const ZERO: &str = "11111111111111111111111111111111";
#[track_caller]
pub(crate) fn invalid() -> GoFinError {
    GoFinError::new(
        "KAMINO_LENDING_EVIDENCE_INVALID",
        "validation",
        "KLend account or instruction evidence is inconsistent.",
        "Refresh the market, reserve and obligation before preparing a new action.",
    ).with_details(serde_json::json!({"validation_check":format!("{}:{}",std::panic::Location::caller().file(),std::panic::Location::caller().line())}))
}
pub(crate) fn unavailable(message: &str) -> GoFinError {
    GoFinError::new(
        "KAMINO_LENDING_UNAVAILABLE",
        "preparation",
        message,
        "Review the explicit reserve and obligation restrictions and use a supported native action.",
    )
}
pub(crate) fn pending() -> GoFinError {
    GoFinError::new(
        "KAMINO_LENDING_EVIDENCE_PENDING",
        "settlement",
        "The finalized obligation state cannot yet be attributed exclusively to this transaction.",
        "Reconcile the original submission; do not submit the operation again.",
    )
}

#[async_trait]
impl KlendProtocol for NativeKlendProtocol {
    fn default_obligation_address(&self, owner: &str, market: &str) -> Result<String> {
        reads::vanilla(owner, market)
    }
    async fn markets(&self, runtime: &SolanaRuntime, query: &LendingQuery) -> Result<MarketPage> {
        reads::markets(runtime, query).await
    }
    async fn market(&self, runtime: &SolanaRuntime, address: &str) -> Result<MarketInfo> {
        Ok(reads::load_market(runtime, address).await?.view)
    }
    async fn reserves(
        &self,
        runtime: &SolanaRuntime,
        market: &str,
        query: &LendingQuery,
    ) -> Result<ReservePage> {
        reads::reserves(runtime, market, query).await
    }
    async fn reserve(
        &self,
        runtime: &SolanaRuntime,
        market: &str,
        address: &str,
    ) -> Result<ReserveInfo> {
        reads::public_reserve(runtime, market, address).await
    }
    async fn obligations(
        &self,
        runtime: &SolanaRuntime,
        owner: &str,
        market: Option<&str>,
        query: &LendingQuery,
    ) -> Result<ObligationPage> {
        reads::obligations(runtime, owner, market, query).await
    }
    async fn obligation(
        &self,
        runtime: &SolanaRuntime,
        owner: &str,
        market: &str,
        address: &str,
    ) -> Result<ObligationInfo> {
        reads::load_obligation(runtime, owner, market, address)
            .await
            .map(|v| v.view)
    }
    async fn build_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &LendingIntent,
        owner: &str,
    ) -> Result<LendingCandidate> {
        native::build(runtime, intent, owner).await
    }
    async fn validate_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &LendingIntent,
        owner: &str,
        candidate: &LendingCandidate,
    ) -> Result<()> {
        native::validate(runtime, intent, owner, candidate).await
    }
    async fn verify_finalized(
        &self,
        runtime: &SolanaRuntime,
        intent: &LendingIntent,
        owner: &str,
        candidate: &LendingCandidate,
        proof: &FinalizedReceiptProof,
    ) -> Result<VerifiedLendingEffects> {
        native::finalized(runtime, intent, owner, candidate, proof).await
    }
}
