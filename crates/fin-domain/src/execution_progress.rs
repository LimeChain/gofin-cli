//! Best-effort UI observations. These never grant authority or control execution.
use serde::{Deserialize, Serialize};
use std::future::Future;
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Preparing,
    RefreshingCandidate,
    AwaitingApproval,
    Revalidating,
    Submitting,
    Verifying,
}

tokio::task_local! {
    static OBSERVER: mpsc::Sender<Stage>;
}

pub fn enabled() -> bool {
    OBSERVER.try_with(|_| ()).is_ok()
}

pub fn emit(stage: Stage) {
    // Full or disconnected observers must never hold up financial work.
    let _ = OBSERVER.try_with(|observer| observer.try_send(stage));
}

pub fn channel() -> (mpsc::Sender<Stage>, mpsc::Receiver<Stage>) {
    mpsc::channel(32)
}

pub async fn observe<F: Future>(observer: mpsc::Sender<Stage>, work: F) -> F::Output {
    OBSERVER.scope(observer, work).await
}
