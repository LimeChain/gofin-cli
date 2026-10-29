use super::*;

#[test]
fn plan_recovery_selects_latest_linked_candidate_and_keeps_step_identity() {
    let status = json!({"steps":[{"attempts":[
        {"attempt_id":"first","request_id":"logical","step_id":"wrap","status":"failed"},
        {"attempt_id":"second","request_id":"refresh-1","logical_request_id":"logical","step_id":"wrap","status":"failed"},
        {"attempt_id":"third","request_id":"refresh-2","logical_request_id":"logical","step_id":"wrap","status":"completed"},
        {"attempt_id":"unrelated","request_id":"other","logical_request_id":"logical","step_id":"supply","status":"pending"}
    ]}]});
    assert_eq!(
        find_plan_attempt(&status, "wrap", "logical").unwrap()["attempt_id"],
        "third"
    );
    assert!(find_plan_attempt(&status, "wrap", "missing").is_none());
    let legacy =
        json!({"attempts":[{"attempt_id":"original","request_id":"logical","step_id":"wrap"}]});
    assert_eq!(
        find_plan_attempt(&legacy, "wrap", "logical").unwrap()["attempt_id"],
        "original"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn named_plan_stream_delivers_progress_and_final_failed_record_over_unix_transport() {
    check_named_plan_stream(false).await;
    check_named_plan_stream(true).await;
}

#[cfg(target_os = "macos")]
async fn check_named_plan_stream(rich: bool) {
    use gofin_domain::execution_progress::{self, CandidateRefresh, RefreshPhase, RefreshReason, Stage};
    use std::sync::{Arc, Mutex};
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("broker.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let failed = json!({"id":"durable-attempt","request_id":"logical-wrap","status":"failed","submission":null,"error":{"code":"PREPARATION_EXPIRED","stage":"preflight","message":"Synthetic pre-submission failure.","next_action":"Inspect the original attempt."}});
    let expected = failed.clone();
    let server = tokio::spawn(async move {
        // New clients negotiate through a harmless ordinary read. An old broker
        // has no feature metadata and must never receive the unknown opt-in field.
        let (probe, _) = listener.accept().await.unwrap();
        let mut probe = BufReader::new(probe);
        let envelope = gofin_protocol::read_request_envelope(&mut probe).await.unwrap().unwrap();
        assert_eq!(envelope.request, BrokerRequest::Capabilities);
        assert!(!envelope.progress_details);
        let capabilities = if rich { json!({"progress_features":["candidate_refresh_details"]}) } else { json!({"version":2}) };
        probe.get_mut().write_all(&gofin_protocol::encode_response(&BrokerResponse::success(capabilities)).unwrap()).await.unwrap();
        drop(probe);
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let envelope = gofin_protocol::read_request_envelope(&mut reader)
            .await
            .unwrap()
            .unwrap();
        assert!(envelope.progress);
        assert_eq!(envelope.progress_details, rich);
        assert!(!envelope.cancellable_read());
        assert!(
            matches!(&envelope.request,BrokerRequest::ExecutePlanStep {plan_id,step_id,request} if plan_id=="plan" && step_id=="wrap" && request.request_id=="logical-wrap")
        );
        let response =
            gofin_protocol::progress::observe_request_with_details(reader.get_mut(), true, envelope.progress_details, false, async {
                execution_progress::emit(Stage::WaitingForBroker);
                tokio::task::yield_now().await;
                execution_progress::emit(Stage::Preparing);
                tokio::task::yield_now().await;
                for refresh_count in 1..=2 {
                    execution_progress::emit_refresh(CandidateRefresh {
                        reason_code: RefreshReason::EvidenceTimeout,
                        phase: RefreshPhase::Preparation,
                        refresh_count,
                        refresh_limit: 2,
                    });
                    tokio::task::yield_now().await;
                }
                BrokerResponse::success(failed)
            })
            .await
            .unwrap();
        reader
            .get_mut()
            .write_all(&gofin_protocol::encode_response(&response).unwrap())
            .await
            .unwrap();
    });
    let transport = UnixTransport {
        progress_details: tokio::sync::OnceCell::new(),
        path,
        expected_uid: unsafe { libc::geteuid() },
    };
    let cli = Cli::try_parse_from([
        "gofin",
        "execute",
        "wallet.wrap",
        "--input",
        "{}",
        "--id",
        "logical-wrap",
        "--plan",
        "plan",
        "--step",
        "wrap",
        "--json",
        "--stream",
    ])
    .unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let stream = json_stream::JsonStream::new(Capture(captured.clone()))
        .with_context(stream_context(&cli.command));
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        execution_feedback(
            &Ui::new(true, false),
            true,
            false,
            Some(&stream),
            dispatch(cli, &transport),
        ),
    )
    .await
    .expect("final failed record must finish without waiting for observer channel closure");
    assert_eq!(result.as_ref().unwrap(), &expected);
    assert_eq!(transport.progress_details.get(), Some(&rich));
    assert_eq!(
        stream.finish(result),
        0,
        "durable failed records retain ordinary machine exit behavior"
    );
    server.await.unwrap();
    let bytes = captured.lock().unwrap();
    let events = std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let mut expected_stages = vec![Some("connecting"), Some("waiting_for_broker"), Some("preparing"), Some("refreshing_candidate")];
    if rich { expected_stages.push(Some("refreshing_candidate")); }
    expected_stages.push(None);
    assert_eq!(events.iter().map(|event| event["stage"].as_str()).collect::<Vec<_>>(), expected_stages);
    if rich {
        for (event, count) in events[3..5].iter().zip(1..=2) {
            assert_eq!(event["candidate_refresh"], json!({"reason_code":"EVIDENCE_TIMEOUT", "phase":"preparation", "refresh_count":count, "refresh_limit":2}));
        }
    } else {
        assert!(events.iter().all(|event| event.get("candidate_refresh").is_none()));
    }
    assert_eq!(events.last().unwrap()["result"], expected);
    assert!(
        events
            .iter()
            .all(|event| event["context"]["plan_id"] == "plan")
    );
}
