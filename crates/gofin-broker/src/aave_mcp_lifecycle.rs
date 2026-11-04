//! Informational MCP results are untrusted observations, never financial evidence.
use super::*;
use gofin_domain::operation_lifecycle::{
    CandidateResult, LifecycleSession, OperationLifecycle, TypedSession,
};
use gofin_ethereum::aave_mcp::{AaveMcpError, AaveMcpReadCatalog, AaveMcpReadResult};

struct McpRead;
#[derive(Clone)]
enum ReadInput {
    Catalog,
    Tool {
        operation: String,
        name: String,
        arguments: Value,
    },
}
#[derive(Clone)]
enum ReadEvidence {
    Catalog(AaveMcpReadCatalog),
    Tool(AaveMcpReadResult),
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct ReadOutput {
    value: Value,
    definitions: Option<Vec<Definition>>,
}
struct ReadService<'a> {
    runtime: &'a BrokerRuntime,
}

fn input(operation: &str, arguments: Value) -> gofin_domain::Result<ReadInput> {
    if operation == "aave.mcp.tools" {
        if !arguments.as_object().is_some_and(serde_json::Map::is_empty) {
            return Err(error(
                "AAVE_MCP_READ_INPUT_INVALID",
                "validation",
                "Aave MCP tool discovery accepts only an empty object.",
                "Call `gofin read aave.mcp.tools --input '{}'`.",
            ));
        }
        return Ok(ReadInput::Catalog);
    }
    let name = aave_mcp_read_tool_name(operation).ok_or_else(|| {
        error(
            "AAVE_MCP_READ_UNAVAILABLE",
            "read",
            "This Aave MCP operation is not a locally permitted read-only tool name.",
            "Discover available tools with `gofin read aave.mcp.tools --input '{}'`.",
        )
    })?;
    if broker_has_sensitive_input_key(&arguments) {
        return Err(error(
            "AAVE_MCP_READ_INPUT_INVALID",
            "validation",
            "Aave MCP read arguments cannot include credentials, signatures, permits or secrets.",
            "Pass only public address and query parameters from the upstream read schema.",
        ));
    }
    Ok(ReadInput::Tool {
        operation: operation.into(),
        name: name.into(),
        arguments,
    })
}
fn read_only<T>() -> gofin_domain::Result<T> {
    Err(GoFinError::validation(
        "Informational MCP reads cannot authorize, dispatch, or observe financial submissions.",
    ))
}
#[async_trait]
impl OperationLifecycle<McpRead> for ReadService<'_> {
    type Input = ReadInput;
    type Evidence = ReadEvidence;
    type Candidate = ReadOutput;
    type Prepared = ();
    type Simulated = ReadOutput;
    type ExecutionRef = ();
    type Observation = ();
    type Output = ReadOutput;

    async fn collect(&self, input: &ReadInput) -> gofin_domain::Result<ReadEvidence> {
        match input {
            ReadInput::Catalog => {
                self.runtime.clear_aave_mcp_read_definitions()?;
                self.runtime.aave_mcp_client()?.refresh_read_tools().await.map(ReadEvidence::Catalog).map_err(|_| error("AAVE_MCP_READ_UNAVAILABLE", "read", "The official Aave MCP read catalog could not be retrieved or validated.", "Retry this explicit read when the official Aave MCP is available. The offline GoFin catalog remains usable."))
            }
            ReadInput::Tool { name, arguments, .. } => self.runtime.aave_mcp_client()?.call_read_tool(name, arguments.clone()).await.map(ReadEvidence::Tool).map_err(|failure| match failure {
                AaveMcpError::InvalidReadArguments(_) => error("AAVE_MCP_READ_INPUT_INVALID", "validation", "The Aave MCP read arguments do not satisfy the discovered bounded input schema.", "Use the exact upstream input schema from `gofin read aave.mcp.tools --input '{}'`."),
                _ => { let _ = self.runtime.clear_aave_mcp_read_definitions(); error("AAVE_MCP_READ_UNAVAILABLE", "read", "The official Aave MCP read could not be completed or validated.", "Inspect the discovered schema and retry only this read when the upstream tool is available.") }
            }),
        }
    }
    fn evaluate(input: &ReadInput, evidence: &ReadEvidence) -> gofin_domain::Result<ReadOutput> {
        match (input, evidence) {
            (ReadInput::Catalog, ReadEvidence::Catalog(catalog)) => {
                let mut value = serde_json::to_value(catalog).map_err(|_| internal_error())?;
                value["provenance"] = json!("unverified_official_mcp");
                Ok(ReadOutput {
                    value,
                    definitions: Some(aave_mcp_read_definitions(catalog)),
                })
            }
            (
                ReadInput::Tool {
                    operation, name, ..
                },
                ReadEvidence::Tool(result),
            ) if result.tool_name == *name => {
                if result.result.get("isError") == Some(&Value::Bool(true)) {
                    return Err(error("AAVE_MCP_READ_FAILED", "read", "The upstream Aave MCP reported that this read failed.", "Inspect the preserved upstream result details, then retry this read only if appropriate.")
                        .with_details(json!({"operation":operation,"tool_name":result.tool_name,"upstream_result":result.result,"provenance":"unverified_official_mcp"})));
                }
                Ok(ReadOutput {
                    value: json!({"operation":operation,"tool_name":result.tool_name,"result":result.result,"provenance":"unverified_official_mcp"}),
                    definitions: None,
                })
            }
            _ => Err(GoFinError::validation(
                "MCP read evidence does not match its request.",
            )),
        }
    }
    async fn prepare(&self, _: &ReadInput, _: &ReadOutput) -> gofin_domain::Result<()> {
        Ok(())
    }
    async fn simulate(
        &self,
        _: &ReadInput,
        candidate: &ReadOutput,
        _: &(),
    ) -> gofin_domain::Result<ReadOutput> {
        Ok(candidate.clone())
    }
    fn candidate_result(
        candidate: &ReadOutput,
    ) -> gofin_domain::Result<CandidateResult<ReadOutput>> {
        Ok(CandidateResult::Read(candidate.clone()))
    }
    fn output_value(output: ReadOutput) -> gofin_domain::Result<Value> {
        serde_json::to_value(output).map_err(|_| internal_error())
    }
    async fn execute(
        &self,
        _: &ReadInput,
        _: &gofin_domain::operation_lifecycle::AuthorizedOperation,
        _: Arc<dyn SubmissionJournal>,
    ) -> gofin_domain::Result<()> {
        read_only()
    }
    fn submission(_: ()) -> gofin_domain::Result<Submission> {
        read_only()
    }
    async fn observe(&self, _: &ReadInput, _: &OperationRecord) -> gofin_domain::Result<()> {
        read_only()
    }
    fn evaluate_observation(
        _: &ReadInput,
        _: &OperationRecord,
        _: &(),
    ) -> gofin_domain::Result<Reconciliation> {
        read_only()
    }
}

pub(super) async fn read(
    runtime: &BrokerRuntime,
    operation: &str,
    arguments: Value,
) -> gofin_domain::Result<Value> {
    let input = input(operation, arguments)?;
    let service = ReadService { runtime };
    let mut session = TypedSession::<_, McpRead>::new(&service, input);
    session.collect().await?;
    session.evaluate()?;
    session.prepare().await?;
    let CandidateResult::Read(value) = session.simulate().await? else {
        return read_only();
    };
    let output: ReadOutput = serde_json::from_value(value).map_err(|_| internal_error())?;
    if let Some(definitions) = output.definitions {
        *runtime
            .aave_mcp_read_definitions
            .lock()
            .map_err(|_| internal_error())? = definitions;
    }
    Ok(output.value)
}

#[cfg(test)]
#[allow(dead_code)]
#[path = "../examples/gofin-dev-support/aave_mcp_read.rs"]
mod fixture;

#[cfg(test)]
mod tests {
    use super::*;
    use gofin_ethereum::aave_mcp::{AaveMcpClient, AaveMcpReadProvenance};

    struct CountingTransport {
        inner: fixture::AaveMcpReadFixture,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait]
    impl gofin_ethereum::aave_mcp::AaveMcpTransport for CountingTransport {
        async fn post(
            &self,
            request: gofin_ethereum::aave_mcp::McpHttpRequest,
        ) -> std::result::Result<gofin_ethereum::aave_mcp::McpHttpResponse, AaveMcpError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.post(request).await
        }
    }

    #[tokio::test]
    async fn static_startup_and_rejected_inputs_do_not_contact_mcp() {
        let runtime = crate::tests::runtime();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        runtime
            .with_aave_mcp_client(AaveMcpClient::with_transport(Arc::new(CountingTransport {
                inner: fixture::AaveMcpReadFixture::new(),
                calls: calls.clone(),
            })))
            .unwrap();
        assert!(
            runtime
                .cached_aave_mcp_read_definitions()
                .unwrap()
                .is_empty()
        );
        let invalid = runtime
            .handle(BrokerRequest::Read {
                operation: "aave.mcp.get_user_positions".into(),
                input: json!({"secret":"private"}),
            })
            .await;
        assert_eq!(invalid.error.unwrap().code, "AAVE_MCP_READ_INPUT_INVALID");
        let invalid_catalog = runtime
            .handle(BrokerRequest::Read {
                operation: "aave.mcp.tools".into(),
                input: json!({"x":1}),
            })
            .await;
        assert_eq!(
            invalid_catalog.error.unwrap().code,
            "AAVE_MCP_READ_INPUT_INVALID"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let explicit = runtime
            .handle(BrokerRequest::Read {
                operation: "aave.mcp.tools".into(),
                input: json!({}),
            })
            .await;
        assert!(explicit.ok);
        assert!(calls.load(std::sync::atomic::Ordering::SeqCst) > 0);
    }

    #[tokio::test]
    async fn catalog_failure_clears_previous_discovery_and_tool_error_preserves_details() {
        let runtime = crate::tests::runtime();
        runtime
            .with_aave_mcp_client(AaveMcpClient::with_transport(Arc::new(
                fixture::AaveMcpReadFixture::stale_catalog(),
            )))
            .unwrap();
        let request = || BrokerRequest::Read {
            operation: "aave.mcp.tools".into(),
            input: json!({}),
        };
        assert!(runtime.handle(request()).await.ok);
        assert!(
            !runtime
                .cached_aave_mcp_read_definitions()
                .unwrap()
                .is_empty()
        );
        assert!(runtime.handle(request()).await.ok);
        assert_eq!(
            runtime.handle(request()).await.error.unwrap().code,
            "AAVE_MCP_READ_UNAVAILABLE"
        );
        assert!(
            runtime
                .cached_aave_mcp_read_definitions()
                .unwrap()
                .is_empty()
        );
        runtime
            .with_aave_mcp_client(AaveMcpClient::with_transport(Arc::new(
                fixture::AaveMcpReadFixture::tool_error(),
            )))
            .unwrap();
        let response = runtime
            .handle(BrokerRequest::Read {
                operation: "aave.mcp.get_user_positions".into(),
                input: json!({"user":"0x1111111111111111111111111111111111111111"}),
            })
            .await;
        let failure = response.error.unwrap();
        assert_eq!(failure.code, "AAVE_MCP_READ_FAILED");
        assert_eq!(
            failure.details.unwrap()["upstream_result"]["structuredContent"]["data"]["coverage"]["complete"],
            false
        );
        assert!(runtime.engine.operations().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dynamic_catalog_and_tool_flow_preserve_untrusted_results() {
        let runtime = crate::tests::runtime();
        runtime
            .with_aave_mcp_client(AaveMcpClient::with_transport(Arc::new(
                fixture::AaveMcpReadFixture::new(),
            )))
            .unwrap();
        assert!(
            runtime
                .cached_aave_mcp_read_definitions()
                .unwrap()
                .is_empty()
        );
        let catalog = runtime
            .handle(BrokerRequest::Read {
                operation: "aave.mcp.tools".into(),
                input: json!({}),
            })
            .await;
        assert!(catalog.ok);
        let value = catalog.result.unwrap();
        assert_eq!(value["provenance"], "unverified_official_mcp");
        let definitions = runtime.cached_aave_mcp_read_definitions().unwrap();
        assert!(
            definitions
                .iter()
                .any(|item| item.name == "aave.mcp.get_user_positions")
        );
        assert!(
            !definitions
                .iter()
                .any(|item| item.name.contains("liquidation"))
        );
        let result = runtime
            .handle(BrokerRequest::Read {
                operation: "aave.mcp.get_user_positions".into(),
                input: json!({"user":"0x1111111111111111111111111111111111111111"}),
            })
            .await;
        assert!(result.ok);
        let value = result.result.unwrap();
        assert_eq!(value["provenance"], "unverified_official_mcp");
        assert_eq!(
            value["result"]["structuredContent"]["data"]["positions"][0]["asset"],
            "WETH"
        );
        assert_eq!(
            value["result"]["structuredContent"]["data"]["coverage"]["complete"],
            false
        );
        assert_eq!(
            value["result"]["structuredContent"]["data"]["coverage"]["nextCursor"],
            "synthetic-page-2"
        );
        let dynamic = runtime
            .handle(BrokerRequest::Read {
                operation: "aave.mcp.get_new_fixture_metric".into(),
                input: json!({"user":"0x1111111111111111111111111111111111111111"}),
            })
            .await;
        assert!(
            dynamic.ok,
            "newly discovered tools use the same generic read lifecycle"
        );
        let rewards = runtime
            .handle(BrokerRequest::Read {
                operation: "aave.mcp.get_user_rewards".into(),
                input: json!({"user":"0x1111111111111111111111111111111111111111"}),
            })
            .await;
        assert_eq!(
            rewards.result.unwrap()["result"]["structuredContent"]["data"]["rewards"][0]["claimTransaction"]
                ["data"],
            "0xdeadbeef"
        );
        assert!(
            runtime.engine.operations().unwrap().is_empty(),
            "returned transaction data is inert"
        );
    }

    #[test]
    fn evaluation_preserves_upstream_error_details_and_rejects_mismatched_tool() {
        let request = ReadInput::Tool {
            operation: "aave.mcp.get_example".into(),
            name: "get_example".into(),
            arguments: json!({}),
        };
        let evidence = ReadEvidence::Tool(AaveMcpReadResult {
            tool_name: "get_example".into(),
            result: json!({"isError":true,"structuredContent":{"coverage":{"complete":false}},"content":[{"type":"text","text":"upstream detail"}]}),
            provenance: AaveMcpReadProvenance::UnverifiedOfficialMcp,
        });
        let failure = ReadService::evaluate(&request, &evidence).err().unwrap();
        assert_eq!(failure.code, "AAVE_MCP_READ_FAILED");
        assert_eq!(
            failure.details.unwrap()["upstream_result"]["structuredContent"]["coverage"]["complete"],
            false
        );
        let other = ReadInput::Tool {
            operation: "aave.mcp.get_other".into(),
            name: "get_other".into(),
            arguments: json!({}),
        };
        assert!(ReadService::evaluate(&other, &evidence).is_err());
        assert_eq!(
            input("aave.mcp.tools", json!({"extra":1}))
                .err()
                .unwrap()
                .code,
            "AAVE_MCP_READ_INPUT_INVALID"
        );
        assert_eq!(
            input("aave.mcp.get_example", json!({"api_key":"secret"}))
                .err()
                .unwrap()
                .code,
            "AAVE_MCP_READ_INPUT_INVALID"
        );
    }
}
