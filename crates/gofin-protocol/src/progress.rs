//! Optional progress framing. Financial dispatch outlives observer failures.
use crate::BrokerRequest;
use gofin_domain::execution_progress::{self, CandidateRefresh, Event, Stage};
use serde::{Deserialize, Serialize};
use std::{future::Future, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestEnvelope {
    pub request: BrokerRequest,
    pub progress: bool,
    /// Negotiated through capabilities; legacy observers receive stage-only frames.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub progress_details: bool,
    /// Opt-in read clients keep their socket write half open until completion.
    /// Legacy clients may half-close it while legitimately awaiting a response.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancel_on_disconnect: bool,
}

impl RequestEnvelope {
    pub fn cancellable_read(&self) -> bool {
        self.cancel_on_disconnect && self.request.may_use_rpc_reads()
    }
}

/// Authenticated broker confirmation that this observation uses the RPC pool.
/// Clients consume this transport frame without changing the final JSON result.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcReadFrame {
    pub rpc_read: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressFrame {
    pub progress: Stage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_refresh: Option<CandidateRefresh>,
}

impl ProgressFrame {
    pub fn into_event(self) -> Event {
        Event {
            stage: self.progress,
            candidate_refresh: self.candidate_refresh,
        }
    }
}

/// Concurrent delivery ensures an unresponsive/disconnected UI cannot cancel or
/// stall dispatch. Each request has a bounded queue of fixed, non-sensitive enums.
/// The caller writes its existing final response after this returns.
pub async fn observe<W: AsyncWrite + Unpin, F: Future>(
    writer: &mut W,
    enabled: bool,
    work: F,
) -> F::Output {
    observe_with_details(writer, enabled, false, work).await
}

async fn observe_with_details<W: AsyncWrite + Unpin, F: Future>(
    writer: &mut W,
    enabled: bool,
    details: bool,
    work: F,
) -> F::Output {
    // Composite financial futures are large. Keep the work out of the inline
    // task-local/join state so observing progress does not multiply stack use.
    let work = Box::pin(work);
    if !enabled {
        return work.await;
    }
    let (sender, mut receiver) = execution_progress::event_channel();
    let delivery = async {
        let mut connected = true;
        while let Some(progress) = receiver.recv().await {
            if connected {
                let mut frame = serde_json::to_vec(&ProgressFrame {
                    progress: progress.stage,
                    candidate_refresh: if details {
                        progress.candidate_refresh
                    } else {
                        None
                    },
                })
                .expect("fixed stage enum serializes");
                frame.push(b'\n');
                connected = matches!(
                    tokio::time::timeout(Duration::from_millis(100), writer.write_all(&frame))
                        .await,
                    Ok(Ok(()))
                );
            }
        }
    };
    let (result, ()) = tokio::join!(execution_progress::observe_events(sender, work), delivery);
    result
}

/// A negotiated read can be dropped when its client disconnects. Financial
/// dispatch and legacy half-closed clients retain the existing observer path.
/// Pin work before entering async state so large financial futures stay on heap.
pub fn observe_request<S: AsyncRead + AsyncWrite + Unpin, F: Future>(
    stream: &mut S,
    enabled: bool,
    cancellable_read: bool,
    work: F,
) -> impl Future<Output = Option<F::Output>> {
    observe_request_with_details(stream, enabled, false, cancellable_read, work)
}

pub fn observe_request_with_details<S: AsyncRead + AsyncWrite + Unpin, F: Future>(
    stream: &mut S,
    enabled: bool,
    details: bool,
    cancellable_read: bool,
    work: F,
) -> impl Future<Output = Option<F::Output>> {
    let work = Box::pin(work);
    async move {
        if !cancellable_read {
            return Some(observe_with_details(stream, enabled, details, work).await);
        }
        let mut policy = serde_json::to_vec(&RpcReadFrame { rpc_read: true })
            .expect("fixed RPC policy serializes");
        policy.push(b'\n');
        if !matches!(
            tokio::time::timeout(Duration::from_millis(100), stream.write_all(&policy)).await,
            Ok(Ok(()))
        ) {
            return None;
        }
        let (mut reader, mut writer) = tokio::io::split(stream);
        let mut unexpected = [0];
        tokio::select! {
            result = observe_with_details(&mut writer, enabled, details, work) => Some(result),
            _ = reader.read(&mut unexpected) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[test]
    fn kamino_reads_negotiate_cancellation_but_financial_actions_do_not() {
        for operation in [
            "kamino.vaults",
            "kamino.positions",
            "kamino.lending.reserves",
        ] {
            for request in [
                BrokerRequest::Read {
                    operation: operation.into(),
                    input: serde_json::json!({}),
                },
                BrokerRequest::ReadArtifact {
                    operation: operation.into(),
                    input: serde_json::json!({}),
                },
            ] {
                assert!(
                    RequestEnvelope {
                        request,
                        progress: false,
                        cancel_on_disconnect: true
                    }
                    .cancellable_read()
                );
            }
        }
        for operation in [
            "kamino.deposit",
            "kamino.withdraw",
            "kamino.lending.borrow",
            "kamino.lending.repay",
        ] {
            let request = gofin_domain::OperationRequest {
                operation: operation.into(),
                input: serde_json::json!({}),
                request_id: "synthetic".into(),
                run_id: None,
            };
            for request in [
                BrokerRequest::Execute {
                    request: request.clone(),
                },
                BrokerRequest::ExecutePlanStep {
                    plan_id: "plan".into(),
                    step_id: "step".into(),
                    request,
                },
                BrokerRequest::Preview {
                    operation: operation.into(),
                    input: serde_json::json!({}),
                },
            ] {
                assert!(
                    !RequestEnvelope {
                        request,
                        progress: false,
                        cancel_on_disconnect: true
                    }
                    .cancellable_read()
                );
            }
        }
    }

    #[tokio::test]
    async fn opt_in_is_explicit_and_legacy_requests_are_unchanged() {
        let legacy = b"{\"method\":\"capabilities\"}\n";
        assert_eq!(
            crate::read_request_with_progress(&mut &legacy[..])
                .await
                .unwrap(),
            Some((BrokerRequest::Capabilities, false))
        );
        let wire = b"{\"request\":{\"method\":\"capabilities\"},\"progress\":true}\n";
        assert_eq!(
            crate::read_request_with_progress(&mut &wire[..])
                .await
                .unwrap(),
            Some((BrokerRequest::Capabilities, true))
        );
        let invalid =
            b"{\"request\":{\"method\":\"capabilities\"},\"progress\":true,\"authority\":true}\n";
        assert!(
            crate::read_request_with_progress(&mut &invalid[..])
                .await
                .is_err()
        );
        for duplicate in [
            &b"{\"method\":\"status\",\"id\":\"one\",\"id\":\"two\"}\n"[..],
            &b"{\"request\":{\"method\":\"status\",\"id\":\"one\",\"id\":\"two\"},\"progress\":true}\n"[..],
        ] {
            assert!(crate::read_request_with_progress(&mut &duplicate[..]).await.is_err());
        }
    }

    #[tokio::test]
    async fn observed_stages_precede_the_unchanged_final_response() {
        let (mut writer, reader) = tokio::io::duplex(1024);
        let execute = async {
            let result = observe(&mut writer, true, async {
                for stage in [
                    Stage::Preparing,
                    Stage::Revalidating,
                    Stage::Submitting,
                    Stage::Verifying,
                ] {
                    execution_progress::emit(stage);
                }
                crate::BrokerResponse::success(serde_json::json!({"status":"pending"}))
            })
            .await;
            writer
                .write_all(&crate::encode_response(&result).unwrap())
                .await
                .unwrap();
        };
        let read = async {
            let mut lines = BufReader::new(reader).lines();
            for expected in [
                Stage::Preparing,
                Stage::Revalidating,
                Stage::Submitting,
                Stage::Verifying,
            ] {
                let frame: ProgressFrame =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                assert_eq!(frame.progress, expected);
            }
            let response: crate::BrokerResponse =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(response.result.unwrap()["status"], "pending");
        };
        tokio::join!(execute, read);
    }

    #[tokio::test]
    async fn refresh_details_require_opt_in_and_legacy_frames_stay_identical() {
        use execution_progress::{CandidateRefresh, RefreshPhase, RefreshReason};
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyFrame {
            progress: Stage,
        }
        let refresh = CandidateRefresh {
            reason_code: RefreshReason::RpcReadTimeout,
            phase: RefreshPhase::Preparation,
            refresh_count: 1,
            refresh_limit: 2,
        };
        for detailed in [false, true] {
            let mut bytes = Vec::new();
            let result = observe_with_details(&mut bytes, true, detailed, async {
                execution_progress::emit_refresh(refresh);
                "unchanged result"
            })
            .await;
            assert_eq!(result, "unchanged result");
            let frame: ProgressFrame = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(frame.progress, Stage::RefreshingCandidate);
            assert_eq!(frame.candidate_refresh, detailed.then_some(refresh));
            if !detailed {
                let old: LegacyFrame = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(old.progress, Stage::RefreshingCandidate);
                assert_eq!(bytes, b"{\"progress\":\"refreshing_candidate\"}\n");
            }
        }
        let plain: ProgressFrame = serde_json::from_str(r#"{"progress":"preparing"}"#).unwrap();
        assert_eq!(plain.into_event(), Stage::Preparing.into());
        assert!(RefreshReason::from_code("provider secret or URL").is_none());
    }

    #[tokio::test]
    async fn failed_or_unread_progress_cannot_cancel_or_repeat_financial_work() {
        for disconnected in [false, true] {
            let (mut writer, reader) = tokio::io::duplex(1);
            let _reader = if disconnected {
                drop(reader);
                None
            } else {
                Some(reader)
            };
            let mut dispatches = 0;
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                observe(&mut writer, true, async {
                    execution_progress::emit(Stage::Submitting);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    dispatches += 1;
                    execution_progress::emit(Stage::Verifying);
                    for _ in 0..1000 {
                        execution_progress::emit(Stage::Verifying);
                    }
                    "durably pending"
                }),
            )
            .await
            .unwrap();
            assert_eq!(result, "durably pending");
            assert_eq!(dispatches, 1);
        }
    }

    #[tokio::test]
    async fn non_observing_clients_get_no_progress_bytes() {
        let (mut writer, mut reader) = tokio::io::duplex(128);
        let result = observe(&mut writer, false, async {
            execution_progress::emit(Stage::Preparing);
            7
        })
        .await;
        assert_eq!(result, 7);
        assert!(!execution_progress::enabled());
        use tokio::io::AsyncReadExt;
        assert!(
            tokio::time::timeout(Duration::from_millis(10), reader.read_u8())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn negotiated_rpc_read_disconnect_drops_the_observation() {
        let (mut server, client) = tokio::io::duplex(1024);
        let (dropped, dropped_receiver) = tokio::sync::oneshot::channel();
        struct OnDrop(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                let _ = self.0.take().unwrap().send(());
            }
        }
        let guard = OnDrop(Some(dropped));
        let serving = tokio::spawn(async move {
            observe_request(&mut server, true, true, async move {
                let _guard = guard;
                execution_progress::emit(Stage::Preparing);
                std::future::pending::<()>().await;
            })
            .await
        });
        let mut client = BufReader::new(client);
        let mut line = String::new();
        client.read_line(&mut line).await.unwrap();
        let frame: RpcReadFrame = serde_json::from_str(&line).unwrap();
        assert!(frame.rpc_read);
        line.clear();
        client.read_line(&mut line).await.unwrap();
        let frame: ProgressFrame = serde_json::from_str(&line).unwrap();
        assert_eq!(frame.progress, Stage::Preparing);
        drop(client);
        assert!(serving.await.unwrap().is_none());
        dropped_receiver
            .await
            .expect("the waiting read future must be dropped");
    }

    #[tokio::test]
    async fn legacy_half_close_and_financial_opt_in_do_not_cancel_work() {
        let envelope = RequestEnvelope {
            request: BrokerRequest::Execute {
                request: gofin_domain::OperationRequest {
                    operation: "wallet.transfer".into(),
                    input: serde_json::json!({}),
                    request_id: "synthetic".into(),
                    run_id: None,
                },
            },
            progress: true,
            progress_details: false,
            cancel_on_disconnect: true,
        };
        assert!(!envelope.cancellable_read());
        for disconnected in [false, true] {
            let (mut server, mut client) = tokio::io::duplex(1024);
            client.shutdown().await.unwrap();
            let _client = if disconnected {
                drop(client);
                None
            } else {
                Some(client)
            };
            let mut dispatches = 0;
            let result = observe_request(&mut server, true, envelope.cancellable_read(), async {
                execution_progress::emit(Stage::Submitting);
                tokio::task::yield_now().await;
                dispatches += 1;
                "original submission"
            })
            .await;
            assert_eq!(result, Some("original submission"));
            assert_eq!(dispatches, 1);
        }
    }
}
