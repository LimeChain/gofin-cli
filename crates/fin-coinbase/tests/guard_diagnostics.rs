use async_trait::async_trait;
use fin_coinbase::{CoinbaseAccount, CoinbaseProduct, CoinbaseProfile, institutional};
use fin_domain::{FinError, ResourceId, Submission, SubmissionJournal};
use fin_exchange::ExchangeProfile;
use fin_exchange::coinbase::{
    CoinbaseApi, CoinbaseEndpoint, CoinbaseError, CoinbaseRequest, CoinbaseResponse, DispatchGuard,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct GuardApi {
    guard_result: Mutex<Option<bool>>,
    writes: Mutex<usize>,
}

#[async_trait]
impl CoinbaseApi for GuardApi {
    async fn request(
        &self,
        _: CoinbaseEndpoint,
        _: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        let allowed = guard.as_ref().is_some_and(|guard| guard());
        *self.guard_result.lock().unwrap() = Some(allowed);
        if !allowed {
            return Err(CoinbaseError::GuardRejected);
        }
        *self.writes.lock().unwrap() += 1;
        Err(CoinbaseError::Transport)
    }
}

#[derive(Default)]
struct RejectingJournal {
    evidence: Mutex<Option<Value>>,
}

impl SubmissionJournal for RejectingJournal {
    fn assert_authorized(&self) -> fin_domain::Result<()> {
        Ok(())
    }

    fn assert_candidate(&self, _: &fin_domain::PreparedOperation) -> fin_domain::Result<()> {
        Err(FinError::new(
            "PREPARED_CANDIDATE_CHANGED",
            "approval",
            "The protected request no longer matches the reviewed candidate.",
            "Refresh the preview and request approval again.",
        ))
    }

    fn persist_submission(&self, _: &Submission) -> fin_domain::Result<()> {
        Ok(())
    }

    fn persist_dispatch_evidence(
        &self,
        _: &Submission,
        evidence: &Value,
    ) -> fin_domain::Result<()> {
        *self.evidence.lock().unwrap() = Some(evidence.clone());
        Ok(())
    }
}

#[tokio::test]
async fn dispatch_guard_preserves_journal_reason_without_sending() {
    let api = Arc::new(GuardApi::default());
    let transport: Arc<dyn CoinbaseApi> = api.clone();
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "exchange".into(),
            resource: ResourceId("coinbaseexchange:profile:profile-1".into()),
            credential_ref: "credential".into(),
            physical_identity: "profile-1".into(),
            identity_verified: true,
            portfolio_id: None,
            product: CoinbaseProduct::Exchange,
        }],
        [("credential".into(), transport)].into(),
    )
    .unwrap();
    let prepared = institutional::prepare(
        &profile,
        "coinbaseexchange.create-balance-report",
        &json!({
            "account":"exchange",
            "requestId":"guard-test",
            "reportAt":"2025-01-01T00:00:00Z",
            "format":"csv"
        }),
    )
    .await
    .unwrap()
    .unwrap();
    let planned = institutional::plan_submission(&profile, &prepared)
        .await
        .unwrap()
        .unwrap();
    let journal = Arc::new(RejectingJournal::default());

    let error = profile
        .dispatch(&planned.request, journal.clone())
        .await
        .unwrap_err();

    assert_eq!(error.code, "PREPARED_CANDIDATE_CHANGED");
    assert_eq!(error.stage, "approval");
    assert_eq!(
        error.details.as_deref().unwrap()["coinbaseDispatch"]["sent"],
        false
    );
    assert_eq!(
        error.details.as_deref().unwrap()["coinbaseDispatch"]["guardDiagnostic"]["reason"],
        "prepared_candidate_rejected"
    );
    assert_eq!(
        error.details.as_deref().unwrap()["coinbaseDispatch"]["guardDiagnostic"]["journalErrorCode"],
        "PREPARED_CANDIDATE_CHANGED"
    );
    let evidence = journal.evidence.lock().unwrap().clone().unwrap();
    assert_eq!(
        evidence["finNoDispatch"]["reason"],
        "transport_guard_rejected"
    );
    assert_eq!(
        evidence["finNoDispatch"]["guardDiagnostic"]["journalErrorStage"],
        "approval"
    );
    assert_eq!(*api.guard_result.lock().unwrap(), Some(false));
    assert_eq!(*api.writes.lock().unwrap(), 0);
}
