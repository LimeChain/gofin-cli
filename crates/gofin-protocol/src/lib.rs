//! Bounded newline-delimited JSON protocol for the protected GoFin broker.
//! The wire surface intentionally exposes typed operations, never credentials,
//! approval receipts, arbitrary venue methods, or client-supplied final state.

pub mod daemon;
pub mod progress;

use gofin_domain::boundaries::BoundaryAuthorizationRequest;
use gofin_domain::financial_plans::FinancialPlanAuthorizationRequest;
use gofin_domain::{GoFinError, OperationRequest, PlanStep, ResourceAmount};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

pub const MAX_FRAME_BYTES: usize = 1_048_576;
/// Responses can contain bounded full market catalogs and account snapshots.
/// Untrusted request frames retain the separate 1 MiB limit above.
pub const MAX_RESPONSE_FRAME_BYTES: usize = 8 * 1_048_576;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum BrokerRequest {
    Capabilities,
    Connections,
    PolicyShow,
    Doctor,
    Read {
        operation: String,
        input: Value,
    },
    /// Begin a bounded export read; no caller file paths cross the broker boundary.
    ReadArtifact {
        operation: String,
        input: Value,
    },
    /// Fetch one bounded chunk from an immutable, expiring export handle.
    ArtifactChunk {
        artifact_id: String,
        offset: u64,
    },
    /// Release an export handle after verified local delivery.
    ReleaseArtifact {
        artifact_id: String,
    },
    Preview {
        operation: String,
        input: Value,
    },
    /// Read-only preparation for a human-interactive configuration write.
    /// This method cannot execute the candidate; actual configuration writes
    /// use the protected human-terminal broker entry point.
    ConfigurationPreview {
        operation: String,
        input: Value,
    },
    /// Read-only human review of the summary that a boundary grant would authorize.
    PreviewBoundaryAuthorization {
        request: BoundaryAuthorizationRequest,
    },
    /// Grant bounded authority for a logical financial intent after fresh human approval.
    AuthorizeBoundary {
        request: BoundaryAuthorizationRequest,
    },
    /// Read-only review of a named plan and the permissions it would request.
    PreviewPlan {
        request: FinancialPlanAuthorizationRequest,
    },
    /// Create or replace one named plan revision after fresh human approval.
    AuthorizePlan {
        request: FinancialPlanAuthorizationRequest,
    },
    /// Read plan authority and execution progress; may reconcile but never execute.
    PlanStatus {
        plan_id: String,
    },
    /// Revoke future dispatch under a named plan without cancelling submitted work.
    RevokePlan {
        plan_id: String,
    },
    /// Evaluate one candidate against the next eligible plan step.
    PreviewPlanStep {
        plan_id: String,
        step_id: String,
        operation: String,
        input: Value,
    },
    /// Execute one explicit attempt under the named plan's active revision.
    ExecutePlanStep {
        plan_id: String,
        step_id: String,
        request: OperationRequest,
    },
    /// Replace a durable candidate only while its prior attempt is unsubmitted.
    ReplacePlanCandidate {
        plan_id: String,
        step_id: String,
        previous_attempt_id: String,
        request: OperationRequest,
    },
    /// Check and reconcile one bounded authorization and its attempts.
    BoundaryStatus {
        id: String,
    },
    /// Revoke future dispatch under one bounded authorization.
    RevokeBoundary {
        id: String,
    },
    /// Evaluate a candidate against current evidence and remaining authorization.
    PreviewBoundary {
        id: String,
        operation: String,
        input: Value,
    },
    /// Execute one exact attempt under an existing bounded authorization.
    ExecuteBoundary {
        authorization_id: String,
        request: OperationRequest,
    },
    /// Replace a durable candidate only while the prior attempt is unsubmitted.
    ReplaceBoundaryCandidate {
        authorization_id: String,
        previous_attempt_id: String,
        request: OperationRequest,
    },
    Execute {
        request: OperationRequest,
    },
    Status {
        id: String,
    },
    Operations,
    Snapshot,
    RunBegin {
        request: RunBeginRequest,
    },
    RunReplan {
        id: String,
        remaining: Vec<PlanStep>,
    },
    RunFinish {
        id: String,
    },
    DaemonStart,
    DaemonStop,
}

impl BrokerRequest {
    /// Eligible to negotiate RPC waiting, not a declaration of authority or
    /// transport use. The broker confirms the resolved adapter and journal.
    pub fn may_use_rpc_reads(&self) -> bool {
        match self {
            Self::Snapshot
            | Self::Status { .. }
            | Self::PlanStatus { .. }
            | Self::BoundaryStatus { .. } => true,
            Self::Read { operation, .. } | Self::ReadArtifact { operation, .. } => {
                operation == "fin.snapshot"
                    || [
                        "wallet.",
                        "aave.",
                        "uniswap.",
                        "ramses.",
                        "sushi.",
                        "pancakeswap.",
                    ]
                    .iter()
                    .any(|prefix| operation.starts_with(prefix))
                    || matches!(
                        operation.as_str(),
                        "bridge.quotes" | "bridge.status" | "bridge.recovery"
                    )
            }
            _ => false,
        }
    }
}

/// Public metadata only; administrative approval is never accepted over IPC.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigureRequest {
    pub operation: String,
    pub input: Value,
    pub request_id: String,
}

/// Retired wire payload retained so old clients receive an explicit retirement error.
/// New operations do not create restoration runs.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunBeginRequest {
    pub allocations: Vec<ResourceAmount>,
    pub planned_operations: Vec<PlanStep>,
    pub borrowed_proceeds_reserve_usd_micros: String,
    pub return_cost_reserve_usd_micros: String,
    pub unwind: Vec<PlanStep>,
}
pub type UnwindStep = PlanStep;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BrokerResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<GoFinError>,
}

impl BrokerResponse {
    pub fn success(result: Value) -> Self {
        Self {
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn failure(error: GoFinError) -> Self {
        Self {
            ok: false,
            result: None,
            error: Some(error),
        }
    }

    pub fn validate(&self) -> bool {
        matches!(
            (self.ok, &self.result, &self.error),
            (true, Some(_), None) | (false, None, Some(_))
        )
    }
}

/// Read exactly one bounded newline-terminated request. The `take` limit keeps
/// an untrusted peer from growing the allocation without bound.
pub async fn read_request<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<BrokerRequest>, GoFinError> {
    read_request_with_progress(reader)
        .await
        .map(|request| request.map(|(request, _)| request))
}

/// Old clients retain one final response; only explicit observers receive stages.
pub async fn read_request_with_progress<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<(BrokerRequest, bool)>, GoFinError> {
    read_request_envelope(reader)
        .await
        .map(|envelope| envelope.map(|envelope| (envelope.request, envelope.progress)))
}

/// Keep transport options explicit while accepting legacy plain requests.
pub async fn read_request_envelope<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<progress::RequestEnvelope>, GoFinError> {
    let mut bytes = Vec::with_capacity(1024);
    let read = reader
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(|_| protocol_error("The request frame could not be read."))?;
    if read == 0 {
        return Ok(None);
    }
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(protocol_error("The request frame exceeds the 1 MiB limit."));
    }
    if bytes.last() != Some(&b'\n') {
        return Err(protocol_error("The request frame must end with a newline."));
    }
    // Decode typed structs directly so duplicate keys are still rejected.
    if let Ok(request) = serde_json::from_slice::<BrokerRequest>(&bytes) {
        return Ok(Some(progress::RequestEnvelope {
            request,
            progress: false,
            cancel_on_disconnect: false,
        }));
    }
    let envelope: progress::RequestEnvelope = serde_json::from_slice(&bytes)
        .map_err(|_| protocol_error("The request frame is not valid protocol JSON."))?;
    Ok(Some(envelope))
}

pub fn encode_response(response: &BrokerResponse) -> Result<Vec<u8>, GoFinError> {
    if !response.validate() {
        return Err(protocol_error("The response envelope is inconsistent."));
    }
    let mut bytes = serde_json::to_vec(response)
        .map_err(|_| protocol_error("The response could not be encoded."))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_RESPONSE_FRAME_BYTES {
        return Err(protocol_error(
            "The response frame exceeds the 8 MiB limit.",
        ));
    }
    Ok(bytes)
}

fn protocol_error(message: &str) -> GoFinError {
    GoFinError::new(
        "INVALID_PROTOCOL_FRAME",
        "protocol",
        message,
        "Send one supported newline-terminated broker request and inspect the published schema.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn request_is_typed_denies_unknown_methods_and_requires_newline() {
        let mut input = BufReader::new(&b"{\"method\":\"status\",\"id\":\"synthetic-id\"}\n"[..]);
        assert!(matches!(
            read_request(&mut input).await.unwrap(),
            Some(BrokerRequest::Status { .. })
        ));

        let mut boundary = BufReader::new(
            &b"{\"method\":\"authorize_boundary\",\"request\":{\"request_id\":\"synthetic-intent\",\"operation\":\"binance.spot-order\",\"input\":{},\"bounds\":{}}}\n"[..],
        );
        assert!(matches!(
            read_request(&mut boundary).await.unwrap(),
            Some(BrokerRequest::AuthorizeBoundary { request })
                if request.request_id == "synthetic-intent" && request.valid_for_seconds == 600
        ));

        let attempt = serde_json::json!({
            "method":"execute_boundary",
            "authorization_id":"synthetic-authorization",
            "request":{"request_id":"synthetic-attempt","operation":"binance.spot-order","input":{},"run_id":null}
        });
        let attempt_frame = format!("{attempt}\n");
        assert!(matches!(
            read_request(&mut BufReader::new(attempt_frame.as_bytes()))
                .await
                .unwrap(),
            Some(BrokerRequest::ExecuteBoundary { authorization_id, request })
                if authorization_id == "synthetic-authorization" && request.request_id == "synthetic-attempt"
        ));

        let replacement = serde_json::json!({
            "method":"replace_boundary_candidate",
            "authorization_id":"synthetic-authorization",
            "previous_attempt_id":"synthetic-old-attempt",
            "request":{"request_id":"synthetic-replacement","operation":"binance.spot-order","input":{},"run_id":null}
        });
        let replacement_frame = format!("{replacement}\n");
        assert!(matches!(
            read_request(&mut BufReader::new(replacement_frame.as_bytes()))
                .await
                .unwrap(),
            Some(BrokerRequest::ReplaceBoundaryCandidate {
                authorization_id,
                previous_attempt_id,
                request,
            }) if authorization_id == "synthetic-authorization"
                && previous_attempt_id == "synthetic-old-attempt"
                && request.request_id == "synthetic-replacement"
        ));

        let mut unknown = BufReader::new(&b"{\"method\":\"signed_venue_call\"}\n"[..]);
        assert!(read_request(&mut unknown).await.is_err());

        let mut removed =
            BufReader::new(&b"{\"method\":\"run_status\",\"id\":\"synthetic-run\"}\n"[..]);
        assert_eq!(
            read_request(&mut removed).await.unwrap_err().code,
            "INVALID_PROTOCOL_FRAME"
        );

        let mut unterminated = BufReader::new(&b"{\"method\":\"capabilities\"}"[..]);
        assert!(read_request(&mut unterminated).await.is_err());

        let replan = serde_json::json!({
            "method": "run_replan",
            "id": "synthetic-run",
            "remaining": [{
                "operation": "ethereum.unwrap",
                "input": {"amount":"all","max_amount":"1","wallet":"synthetic"},
                "maximum_fee_usd_micros": "1000",
                "description": "synthetic cleanup"
            }]
        });
        let replan_frame = format!("{replan}\n");
        let mut replan = BufReader::new(replan_frame.as_bytes());
        assert!(matches!(
            read_request(&mut replan).await.unwrap(),
            Some(BrokerRequest::RunReplan { id, remaining })
                if id == "synthetic-run" && remaining.len() == 1
        ));
    }

    #[tokio::test]
    async fn named_plan_requests_and_step_authority_are_typed() {
        let request = serde_json::json!({
            "method": "authorize_plan",
            "request": {
                "request_id": "request-1",
                "valid_for_seconds": 600,
                "plan": {
                    "version": 1,
                    "name": "sample two step plan",
                    "steps": [{
                        "step_id": "sell",
                        "operation": "binance.spot-order",
                        "input": {"symbol":"ETH/USDT"},
                        "permitted_variations": {"price":"bounded"},
                        "completion": {"principal_spend": null, "minimum_net_receipt": {
                            "resource":"binance:account:synthetic",
                            "amount":{"asset":{"network":"binance","reference":"USDT","decimals":6},"atoms":"1"}
                        }},
                        "funding_dependencies": []
                    }],
                    "aggregate_bounds": {"maximum_debits":[],"maximum_costs":[]}
                }
            }
        });
        let frame = format!("{request}\n");
        assert!(matches!(
            read_request(&mut BufReader::new(frame.as_bytes())).await.unwrap(),
            Some(BrokerRequest::AuthorizePlan { request })
                if request.request_id == "request-1"
                    && request.plan.name == "sample two step plan"
                    && request.valid_for_seconds == Some(600)
        ));

        let attempt = serde_json::json!({
            "method":"execute_plan_step",
            "plan_id":"plan-1",
            "step_id":"sell",
            "request":{"request_id":"attempt-1","operation":"binance.spot-order","input":{},"run_id":null}
        });
        let frame = format!("{attempt}\n");
        assert!(matches!(
            read_request(&mut BufReader::new(frame.as_bytes())).await.unwrap(),
            Some(BrokerRequest::ExecutePlanStep { plan_id, step_id, request })
                if plan_id == "plan-1" && step_id == "sell" && request.request_id == "attempt-1"
        ));
    }

    #[tokio::test]
    async fn request_frame_is_size_bounded() {
        let mut bytes = vec![b'a'; MAX_FRAME_BYTES + 2];
        bytes.push(b'\n');
        let mut input = BufReader::new(bytes.as_slice());
        assert_eq!(
            read_request(&mut input).await.unwrap_err().code,
            "INVALID_PROTOCOL_FRAME"
        );
    }

    #[test]
    fn response_envelope_is_exactly_one_of_result_or_error() {
        let success = BrokerResponse::success(serde_json::json!({"available":true}));
        let encoded = encode_response(&success).unwrap();
        assert_eq!(encoded.last(), Some(&b'\n'));
        assert!(String::from_utf8(encoded).unwrap().contains("\"ok\":true"));
        let invalid = BrokerResponse {
            ok: true,
            result: None,
            error: None,
        };
        assert!(encode_response(&invalid).is_err());
    }

    #[tokio::test]
    async fn large_catalog_response_does_not_increase_the_request_limit() {
        let value = serde_json::json!({"markets": "x".repeat(MAX_FRAME_BYTES + 1)});
        let encoded = encode_response(&BrokerResponse::success(value.clone())).unwrap();
        assert!(encoded.len() > MAX_FRAME_BYTES);
        assert!(encoded.len() < MAX_RESPONSE_FRAME_BYTES);
        let request =
            serde_json::json!({"method":"read", "operation":"synthetic.read", "input":value});
        let mut request = serde_json::to_vec(&request).unwrap();
        request.push(b'\n');
        assert!(
            read_request(&mut BufReader::new(request.as_slice()))
                .await
                .is_err()
        );
        let oversized = BrokerResponse::success(
            serde_json::json!({"markets":"x".repeat(MAX_RESPONSE_FRAME_BYTES)}),
        );
        assert_eq!(
            encode_response(&oversized).unwrap_err().code,
            "INVALID_PROTOCOL_FRAME"
        );
    }
}
