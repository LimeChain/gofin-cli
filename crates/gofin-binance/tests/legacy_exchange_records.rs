//! Frozen pre-CCXT wire format: no new prepare/submit produces this record.
//! Every account/key reference is synthetic. The old submission intentionally
//! predates historySelector; reconciliation must derive it from binding.params.
use async_trait::async_trait;
use gofin_binance::{
    AccountRole, BinanceAccount, BinanceAdapter, BinanceConfig, BinanceEndpoint, BinanceRequest,
    BinanceTransport, ClientIdentity, CredentialHandle, TransportError,
};
use gofin_domain::{
    Adapter, OperationRecord, OperationRequest, ResourceId, Status, request_fingerprint,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const FROZEN_RECORD: &str = include_str!("fixtures/legacy-transfer-v1.json");
const CLIENT_ID: &str = "finb6b1eaf8415152486463b5254df00";
const FINGERPRINT: &str = "75c5aba864ac8e3d678b2f92dd4f9aa5c2e9270f4d3ac1fa063a8124c95471a2";

#[derive(Default)]
struct HistoryOnly(Mutex<Vec<BinanceRequest>>);
#[async_trait]
impl BinanceTransport for HistoryOnly {
    async fn request(
        &self,
        _: &CredentialHandle,
        request: BinanceRequest,
    ) -> Result<Value, TransportError> {
        // Any replay, preparation, unrelated query or financial POST is a failure.
        assert_eq!(request.endpoint, BinanceEndpoint::UniversalTransferHistory);
        assert_eq!(request.get("clientTranId"), Some(CLIENT_ID));
        assert_eq!(request.get("toEmail"), Some("child@example.invalid"));
        assert_eq!(request.get("fromEmail"), None);
        self.0.lock().unwrap().push(request);
        Ok(json!({"totalCount":0,"result":[]}))
    }
}
fn adapter(transport: Arc<HistoryOnly>) -> BinanceAdapter {
    let config = BinanceConfig {
        accounts: vec![
            BinanceAccount {
                name: "main-fixture".into(),
                resource: ResourceId("binance:uid:1001:SPOT".into()),
                credential: CredentialHandle::new("test-only:synthetic-main").unwrap(),
                role: AccountRole::Main,
                parent: None,
                email: None,
                wallet_type: "SPOT".into(),
            },
            BinanceAccount {
                name: "child-fixture".into(),
                resource: ResourceId("binance:uid:1002:SPOT".into()),
                credential: CredentialHandle::new("test-only:synthetic-child").unwrap(),
                role: AccountRole::Sub,
                parent: Some("main-fixture".into()),
                email: Some("child@example.invalid".into()),
                wallet_type: "SPOT".into(),
            },
        ],
        destinations: vec![],
    };
    BinanceAdapter::new(config, transport, None, None).unwrap()
}
#[tokio::test]
async fn frozen_v1_record_keeps_fingerprint_identity_request_and_unknown_status_without_replay() {
    let record: OperationRecord = serde_json::from_str(FROZEN_RECORD).unwrap();
    let frozen: Value = serde_json::from_str(FROZEN_RECORD).unwrap();
    assert_eq!(serde_json::to_value(&record).unwrap(), frozen);
    assert_eq!(
        ClientIdentity::transfer(&record.request_id)
            .unwrap()
            .as_str(),
        CLIENT_ID
    );
    assert_eq!(record.fingerprint, FINGERPRINT);
    assert_eq!(
        request_fingerprint(&OperationRequest {
            request_id: record.request_id.clone(),
            operation: record.operation.clone(),
            input: record.input.clone(),
            run_id: record.run_id.clone()
        })
        .unwrap(),
        FINGERPRINT
    );
    let submission = record.submission.as_ref().unwrap();
    assert_eq!(submission.kind, "binance");
    assert_eq!(submission.reference, CLIENT_ID);
    assert!(submission.data.get("historySelector").is_none());
    let request: BinanceRequest =
        serde_json::from_value(submission.data["binding"]["params"].clone()).unwrap();
    request.validate().unwrap();
    assert_eq!(request.get("amount"), Some("0.000000000000000001"));
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        submission.data["binding"]["params"]
    );

    let transport = Arc::new(HistoryOnly::default());
    let restarted = adapter(transport.clone());
    // An old record outside the history window remains unresolved, never replayed.
    let result = restarted.reconcile(&record).await.unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(result.verified.is_none());
    assert!(transport.0.lock().unwrap().is_empty());
    assert_eq!(serde_json::to_value(&record).unwrap(), frozen);

    // Move only fixture observation time into the supported read window. All
    // frozen IDs, fingerprints, params, quantities and immutable bindings remain.
    let mut in_window = record.clone();
    in_window.submission.as_mut().unwrap().data["submittedAt"] = json!(gofin_domain::now());
    let before = serde_json::to_value(&in_window).unwrap();
    let result = restarted.reconcile(&in_window).await.unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(result.verified.is_none());
    assert_eq!(result.evidence["clientTranId"], CLIENT_ID);
    assert_eq!(transport.0.lock().unwrap().len(), 1);
    assert_eq!(serde_json::to_value(&in_window).unwrap(), before);
}
