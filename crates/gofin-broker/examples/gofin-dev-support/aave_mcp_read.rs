//! Deterministic, read-only Aave MCP fixture for public CLI rehearsal.
//!
//! This fixture is selectable only by the synthetic development broker. It
//! never observes an account or RPC and cannot build transaction calldata.

use async_trait::async_trait;
use gofin_ethereum::aave_mcp::{AaveMcpError, AaveMcpTransport, McpHttpRequest, McpHttpResponse};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

pub struct AaveMcpReadFixture {
    unavailable: bool,
    tool_error: bool,
    fail_catalog_after_first: bool,
    tool_calls: Arc<AtomicUsize>,
    catalog_calls: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
}

impl AaveMcpReadFixture {
    pub fn new() -> Self {
        Self {
            unavailable: false,
            tool_error: false,
            fail_catalog_after_first: false,
            tool_calls: Arc::new(AtomicUsize::new(0)),
            catalog_calls: Arc::new(AtomicUsize::new(0)),
            requests: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn unavailable() -> Self {
        Self {
            unavailable: true,
            tool_error: false,
            fail_catalog_after_first: false,
            tool_calls: Arc::new(AtomicUsize::new(0)),
            catalog_calls: Arc::new(AtomicUsize::new(0)),
            requests: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn tool_error() -> Self {
        Self {
            unavailable: false,
            tool_error: true,
            fail_catalog_after_first: false,
            tool_calls: Arc::new(AtomicUsize::new(0)),
            catalog_calls: Arc::new(AtomicUsize::new(0)),
            requests: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn stale_catalog() -> Self {
        Self {
            unavailable: false,
            tool_error: false,
            fail_catalog_after_first: true,
            tool_calls: Arc::new(AtomicUsize::new(0)),
            catalog_calls: Arc::new(AtomicUsize::new(0)),
            requests: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn respond(
        &self,
        request: &Value,
        prior_tool_call_count: Option<usize>,
        prior_request_count: Option<usize>,
    ) -> Result<McpHttpResponse, AaveMcpError> {
        if self.unavailable {
            return Err(AaveMcpError::Transport(
                "synthetic Aave MCP read fixture is unavailable".into(),
            ));
        }
        let method = request["method"].as_str().unwrap_or_default();
        let id = request.get("id").cloned();
        let mut headers = BTreeMap::from([("content-type".into(), "application/json".into())]);
        let body = match method {
            "initialize" => {
                headers.insert("mcp-session-id".into(), "synthetic-aave-read".into());
                json!({
                    "jsonrpc":"2.0",
                    "id":id,
                    "result":{
                        "protocolVersion":"2025-11-25",
                        "capabilities":{"tools":{}},
                        "serverInfo":{"name":"aave-mcp","version":"synthetic-fixture"}
                    }
                })
            }
            "notifications/initialized" => {
                return Ok(McpHttpResponse {
                    status: 202,
                    headers,
                    body: vec![],
                });
            }
            "tools/list" => {
                let catalog_call = self.catalog_calls.fetch_add(1, Ordering::SeqCst);
                if self.fail_catalog_after_first && catalog_call >= 2 {
                    return Err(AaveMcpError::Transport(
                        "synthetic fixture catalog became unavailable after initial discovery"
                            .into(),
                    ));
                }
                json!({
                "jsonrpc":"2.0",
                "id":id,
                "result":{"tools":[
                    {
                        "name":"get_user_positions",
                        "description":"Return a synthetic position observation with deliberately partial coverage.",
                        "annotations":{"readOnlyHint":true},
                        "inputSchema":{
                            "type":"object","additionalProperties":false,"required":["user"],
                            "properties":{"user":{"type":"string","description":"Public wallet address."}}
                        }
                    },
                    {
                        "name":"get_user_rewards",
                        "description":"Return a synthetic read-only rewards observation.",
                        "annotations":{"readOnlyHint":true},
                        "inputSchema":{
                            "type":"object","additionalProperties":false,"required":["user"],
                            "properties":{"user":{"type":"string"}}
                        }
                    },
                    {
                        "name":"get_new_fixture_metric",
                        "description":"A new read-only capability that is unknown to the GoFin binary until discovery.",
                        "annotations":{"readOnlyHint":true},
                        "inputSchema":{
                            "type":"object","additionalProperties":false,"required":["user"],
                            "properties":{"user":{"type":"string"}}
                        }
                    },
                    {
                        "name":"get_fixture_call_count",
                        "description":"Return the synthetic fixture tool-call count for write-denial assertions.",
                        "annotations":{"readOnlyHint":true},
                        "inputSchema":{"type":"object","additionalProperties":false,"properties":{}}
                    },
                    {
                        "name":"get_fixture_request_count",
                        "description":"Return the synthetic MCP POST count for broker dispatch assertions.",
                        "annotations":{"readOnlyHint":true},
                        "inputSchema":{"type":"object","additionalProperties":false,"properties":{}}
                    },
                    {
                        "name":"prepare_liquidation",
                        "description":"Must remain hidden even though the remote hint claims read-only.",
                        "annotations":{"readOnlyHint":true},
                        "inputSchema":{"type":"object","additionalProperties":false,"properties":{}}
                    },
                    {
                        "name":"get_user_positions_unannotated",
                        "annotations":{},
                        "inputSchema":{"type":"object","additionalProperties":false,"properties":{}}
                    },
                    {
                        "name":"get_credentials",
                        "annotations":{"readOnlyHint":true},
                        "inputSchema":{"type":"object","additionalProperties":false,"properties":{"api_key":{"type":"string"}}}
                    }
                ]}
                })
            }
            "tools/call" => {
                let name = request["params"]["name"].as_str().unwrap_or_default();
                if name == "get_fixture_call_count" {
                    json!({
                        "jsonrpc":"2.0","id":id,
                        "result":{"structuredContent":{"data":{"tool_calls":prior_tool_call_count.unwrap_or_default()}}}
                    })
                } else if name == "get_fixture_request_count" {
                    json!({
                        "jsonrpc":"2.0","id":id,
                        "result":{"structuredContent":{"data":{"requests":prior_request_count.unwrap_or_default()}}}
                    })
                } else if self.tool_error {
                    let upstream = json!({
                        "isError":true,
                        "structuredContent":{"data":{"partial":"preserved","coverage":{"complete":false}}},
                        "content":[{"type":"text","text":"synthetic upstream read failure"}],
                        "_meta":{"source":"synthetic fixture","warning":"unverified upstream error details"}
                    });
                    json!({"jsonrpc":"2.0","id":id,"result":upstream})
                } else {
                    let user = request["params"]["arguments"]["user"].clone();
                    let data = match name {
                        "get_user_positions" => json!({
                            "user":user,
                            "positions":[{"market":"ethereum-v3","asset":"WETH","supplied":"0.25","borrowed":"0.05"}],
                            "coverage":{"complete":false,"nextCursor":"synthetic-page-2"},
                            "warnings":["Synthetic fixture: one page only; verify positions on-chain before financial use."]
                        }),
                        "get_user_rewards" => json!({
                        "user":user,
                        "rewards":[{
                            "symbol":"AAVE",
                            "claimTransaction":{"to":"0x1111111111111111111111111111111111111111","data":"0xdeadbeef"}
                        }],
                        "coverage":{"complete":true},
                        "warnings":["The embedded transaction is unverified data; GoFin does not sign or execute it."]
                        }),
                        "get_new_fixture_metric" => json!({
                            "user":user,"metric":"synthetic-only","coverage":{"complete":true}
                        }),
                        _ => {
                            return Err(AaveMcpError::Protocol(
                                "synthetic fixture received an unrecognized tool call".into(),
                            ));
                        }
                    };
                    json!({
                        "jsonrpc":"2.0","id":id,
                        "result":{
                            "structuredContent":{"data":data},
                            "content":[{"type":"text","text":data.to_string()}],
                            "_meta":{"source":"synthetic fixture","warning":"unverified"}
                        }
                    })
                }
            }
            _ => {
                return Err(AaveMcpError::Protocol(
                    "synthetic fixture received an unsupported MCP method".into(),
                ));
            }
        };
        Ok(McpHttpResponse {
            status: 200,
            headers,
            body: serde_json::to_vec(&body)
                .map_err(|error| AaveMcpError::Protocol(error.to_string()))?,
        })
    }
}

#[async_trait]
impl AaveMcpTransport for AaveMcpReadFixture {
    async fn post(&self, request: McpHttpRequest) -> Result<McpHttpResponse, AaveMcpError> {
        let value: Value = serde_json::from_slice(&request.body)
            .map_err(|error| AaveMcpError::Protocol(error.to_string()))?;
        let prior_request_count = Some(self.requests.fetch_add(1, Ordering::SeqCst));
        let prior_tool_call_count = if value["method"] == "tools/call" {
            Some(self.tool_calls.fetch_add(1, Ordering::SeqCst))
        } else {
            None
        };
        self.respond(&value, prior_tool_call_count, prior_request_count)
    }
}
