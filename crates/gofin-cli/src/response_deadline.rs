//! Client observation deadlines; these never cancel or authorize broker work.
use super::{BrokerRequest, broker_response_timeout};
use gofin_domain::execution_progress::Stage;
use std::time::Duration;
use tokio::time::Instant;

pub fn execution_progress_required(request: &BrokerRequest) -> bool {
    matches!(
        request,
        BrokerRequest::Execute { .. }
            | BrokerRequest::ExecuteBoundary { .. }
            | BrokerRequest::ReplaceBoundaryCandidate { .. }
            | BrokerRequest::ExecutePlanStep { .. }
            | BrokerRequest::ReplacePlanCandidate { .. }
    )
}

pub struct ResponseDeadline {
    deadline: Option<Instant>,
    idle_timeout: Option<Duration>,
    last_stage: Option<Stage>,
}

impl ResponseDeadline {
    pub fn new(request: Option<&BrokerRequest>) -> Self {
        Self {
            deadline: request.map(|request| Instant::now() + broker_response_timeout(request)),
            idle_timeout: request
                .filter(|request| execution_progress_required(request))
                .map(broker_response_timeout),
            last_stage: None,
        }
    }

    pub fn at(&self) -> Option<Instant> {
        self.deadline
    }

    /// Only a changed execution stage renews the existing idle budget. Duplicate
    /// stages and partial frames cannot keep a stalled observer waiting forever.
    pub fn observe_progress(&mut self, stage: Stage) {
        if let Some(timeout) = self.idle_timeout
            && self.last_stage != Some(stage)
            && stage != Stage::WaitingForBroker
        {
            self.deadline = Some(Instant::now() + timeout);
        }
        self.last_stage = Some(stage);
    }

    /// The caller first validates the authenticated, read-only RPC wait frame.
    pub fn allow_rpc_read(&mut self) {
        self.deadline = None;
    }
}
