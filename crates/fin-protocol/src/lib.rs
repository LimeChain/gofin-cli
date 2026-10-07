//! Bounded newline-delimited JSON protocol for the protected Fin broker.
//! The wire surface intentionally exposes typed operations, never credentials,
//! approval receipts, arbitrary venue methods, or client-supplied final state.

pub mod daemon;

use fin_domain::{FinError, OperationRequest, PlanStep, ResourceAmount};
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
    Doctor,
    Read {
        operation: String,
        input: Value,
    },
    Preview {
        operation: String,
        input: Value,
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
    RunStatus {
        id: String,
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

/// Client-selected initial holdings. The broker resolves them against its own
/// fresh snapshot and computes valuation; clients cannot submit a baseline.
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
    pub error: Option<FinError>,
}

impl BrokerResponse {
    pub fn success(result: Value) -> Self {
        Self {
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn failure(error: FinError) -> Self {
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
) -> Result<Option<BrokerRequest>, FinError> {
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
    let request = serde_json::from_slice(&bytes)
        .map_err(|_| protocol_error("The request frame is not valid protocol JSON."))?;
    Ok(Some(request))
}

pub fn encode_response(response: &BrokerResponse) -> Result<Vec<u8>, FinError> {
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

fn protocol_error(message: &str) -> FinError {
    FinError::new(
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

        let mut unknown = BufReader::new(&b"{\"method\":\"signed_venue_call\"}\n"[..]);
        assert!(read_request(&mut unknown).await.is_err());

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
