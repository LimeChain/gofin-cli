//! Best-effort UI observations. These never grant authority or control execution.
use serde::{Deserialize, Serialize};
use std::future::Future;
use tokio::sync::mpsc;

/// Foreground lifetime controls admission only. Cancellation must never cancel
/// an immutable submission that has already been durably admitted.
#[derive(Clone)]
pub struct Invocation {
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    deadline: tokio::time::Instant,
}

tokio::task_local! {
    static INVOCATION: Invocation;
}

impl Invocation {
    pub fn new(duration: std::time::Duration) -> Self {
        Self {
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            deadline: tokio::time::Instant::now() + duration,
        }
    }

    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn scope<F: Future>(&self, work: F) -> impl Future<Output = F::Output> + use<F> {
        // Box before constructing the task-local future: financial adapter
        // futures are large and must not be duplicated in nested async frames.
        INVOCATION.scope(self.clone(), Box::pin(work))
    }
}

pub fn invocation_active() -> bool {
    INVOCATION
        .try_with(|invocation| {
            !invocation
                .cancelled
                .load(std::sync::atomic::Ordering::SeqCst)
                && tokio::time::Instant::now() < invocation.deadline
        })
        .unwrap_or(true)
}

pub fn invocation_context_present() -> bool {
    INVOCATION.try_with(|_| ()).is_ok()
}

/// Cancels the shared foreground scope if its owning future is dropped.
pub struct InvocationDropGuard(Option<Invocation>);
impl InvocationDropGuard {
    pub fn disarm(&mut self) {
        self.0 = None;
    }
}
impl Drop for InvocationDropGuard {
    fn drop(&mut self) {
        if let Some(invocation) = self.0.take() {
            invocation.cancel();
        }
    }
}
pub fn invocation_drop_guard() -> Option<InvocationDropGuard> {
    INVOCATION
        .try_with(|invocation| InvocationDropGuard(Some(invocation.clone())))
        .ok()
}

pub fn ensure_invocation_active() -> crate::Result<()> {
    if invocation_active() {
        Ok(())
    } else {
        Err(crate::GoFinError::new(
            "INVOCATION_ENDED",
            "admission",
            "The foreground execution ended before another stage was admitted.",
            "Observe the original operation and use its continuation when ready.",
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    WaitingForBroker,
    Preparing,
    RefreshingCandidate,
    AwaitingApproval,
    Revalidating,
    Submitting,
    Verifying,
}

/// Only fixed diagnostic codes cross the progress boundary. Provider messages,
/// request inputs and arbitrary error details are intentionally excluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RefreshReason {
    PreviewExpired,
    SolanaBlockhashExpired,
    SolanaTxExpired,
    JupiterRfqExpired,
    StaleValuation,
    PriceStale,
    NonceChanged,
    EvidenceTimeout,
    RpcReadTimeout,
    RpcUnavailable,
    ValueBoundExceeded,
    VerifiedNonDispatchExpiry,
}

impl RefreshReason {
    pub fn from_code(code: &str) -> Option<Self> {
        Some(match code {
            "PREVIEW_EXPIRED" | "BRIDGE_QUOTE_EXPIRED" => Self::PreviewExpired,
            "SOLANA_BLOCKHASH_EXPIRED" => Self::SolanaBlockhashExpired,
            "SOLANA_TX_EXPIRED" => Self::SolanaTxExpired,
            "JUPITER_RFQ_EXPIRED" => Self::JupiterRfqExpired,
            "STALE_VALUATION" => Self::StaleValuation,
            "PRICE_STALE" => Self::PriceStale,
            "NONCE_CHANGED" => Self::NonceChanged,
            "EVIDENCE_TIMEOUT" => Self::EvidenceTimeout,
            "RPC_READ_TIMEOUT" => Self::RpcReadTimeout,
            // Preserve the negotiated progress wire enum for existing clients.
            // The terminal error retains the exact RPC code and cooldown.
            "RPC_UNAVAILABLE" | "RPC_HISTORICAL_STATE_UNAVAILABLE" | "RPC_RATE_LIMITED" => {
                Self::RpcUnavailable
            }
            "VALUE_BOUND_EXCEEDED" => Self::ValueBoundExceeded,
            _ => return None,
        })
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::PreviewExpired => "preview expired",
            Self::SolanaBlockhashExpired
            | Self::SolanaTxExpired
            | Self::VerifiedNonDispatchExpiry => "transaction expired",
            Self::JupiterRfqExpired => "quote expired",
            Self::StaleValuation | Self::PriceStale => "price evidence expired",
            Self::NonceChanged => "transaction nonce changed",
            Self::EvidenceTimeout => "evidence read timed out",
            Self::RpcReadTimeout => "RPC read timed out",
            Self::RpcUnavailable => "RPC unavailable",
            Self::ValueBoundExceeded => "price moved beyond candidate ceiling",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshPhase {
    Preparation,
    Revalidation,
    VerifiedNonDispatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateRefresh {
    pub reason_code: RefreshReason,
    pub phase: RefreshPhase,
    /// One-based refresh number, excluding the initial candidate.
    pub refresh_count: usize,
    pub refresh_limit: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub stage: Stage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_refresh: Option<CandidateRefresh>,
}

impl From<Stage> for Event {
    fn from(stage: Stage) -> Self {
        Self {
            stage,
            candidate_refresh: None,
        }
    }
}

#[derive(Clone)]
enum Observer {
    Stages(mpsc::Sender<Stage>),
    Events(mpsc::Sender<Event>),
}

tokio::task_local! {
    static OBSERVER: Observer;
}

/// Copy foreground cancellation and progress into a separately scheduled
/// financial task. The shared invocation still stops new admissions when the
/// calling client disconnects.
pub fn inherit_current<F: Future>(work: F) -> impl Future<Output = F::Output> + use<F> {
    let invocation = INVOCATION.try_with(Clone::clone).ok();
    let observer = OBSERVER.try_with(Clone::clone).ok();
    async move {
        match (invocation, observer) {
            (Some(invocation), Some(observer)) => {
                INVOCATION
                    .scope(invocation, OBSERVER.scope(observer, work))
                    .await
            }
            (Some(invocation), None) => INVOCATION.scope(invocation, work).await,
            (None, Some(observer)) => OBSERVER.scope(observer, work).await,
            (None, None) => work.await,
        }
    }
}

pub fn enabled() -> bool {
    OBSERVER.try_with(|_| ()).is_ok()
}

pub fn emit(stage: Stage) {
    emit_event(stage.into());
}

pub fn emit_refresh(refresh: CandidateRefresh) {
    emit_event(Event {
        stage: Stage::RefreshingCandidate,
        candidate_refresh: Some(refresh),
    });
}

pub fn emit_event(event: Event) {
    // Full or disconnected observers must never hold up financial work.
    let _ = OBSERVER.try_with(|observer| match observer {
        Observer::Stages(sender) => {
            let _ = sender.try_send(event.stage);
        }
        Observer::Events(sender) => {
            let _ = sender.try_send(event);
        }
    });
}

pub fn channel() -> (mpsc::Sender<Stage>, mpsc::Receiver<Stage>) {
    mpsc::channel(32)
}

pub fn event_channel() -> (mpsc::Sender<Event>, mpsc::Receiver<Event>) {
    mpsc::channel(32)
}

pub async fn observe<F: Future>(observer: mpsc::Sender<Stage>, work: F) -> F::Output {
    OBSERVER.scope(Observer::Stages(observer), work).await
}

pub async fn observe_events<F: Future>(observer: mpsc::Sender<Event>, work: F) -> F::Output {
    OBSERVER.scope(Observer::Events(observer), work).await
}
