//! Deterministic MCP transport used only by owned local Aave fork tests.
//! It derives candidate calls from the typed tool arguments and returns the
//! documented MCP envelope; production adapters keep the fixed official HTTP
//! transport.

use async_trait::async_trait;
use gofin_domain::Quantity;
use gofin_ethereum::aave_mcp::{AaveMcpError, AaveMcpTransport, McpHttpRequest, McpHttpResponse};
use gofin_evm::{
    alloy::{
        primitives::{Address, U256},
        sol_types::SolCall,
    },
    u256,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

const PROTOCOL_VERSION: &str = "2025-11-25";

fn fixture_token_decimals(token: &str) -> Result<u8, AaveMcpError> {
    // Explicitly enumerate the assets exercised by the guarded fork profiles.
    // Unknown tokens fail closed: these are not general-purpose decimals.
    match token.to_ascii_lowercase().as_str() {
        "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2" // Ethereum WETH
        | "0x4200000000000000000000000000000000000006" // Base WETH
        | "0x82af49447d8a07e3bd95bd0d56f35241523fbab1" // Arbitrum WETH
        | "0xcd5fe23c85820f7b72d0926fc9b05b43e359b7ee" => Ok(18), // Ethereum weETH
        "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48" // Ethereum USDC
        | "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913" // Base USDC
        | "0xaf88d065e77c8cc2239327c5edb3a432268e5831" // Arbitrum USDC
        | "0x43415eb6ff9db7e26a15b704e7a3edce97d31c4e" => Ok(6), // Ethereum USTB
        _ => Err(AaveMcpError::Protocol(
            "fixture has no decimals entry for this guarded fork token".into(),
        )),
    }
}

gofin_evm::alloy::sol! {
    interface FixturePool {
        function supply(address asset, uint256 amount, address onBehalfOf, uint16 referralCode) external;
        function borrow(address asset, uint256 amount, uint256 interestRateMode, uint16 referralCode, address onBehalfOf) external;
        function repay(address asset, uint256 amount, uint256 interestRateMode, address onBehalfOf) external;
        function withdraw(address asset, uint256 amount, address to) external;
        function setUserUseReserveAsCollateral(address asset, bool useAsCollateral) external;
    }
}

#[derive(Default)]
pub struct ForkAaveMcp {
    tool_calls: AtomicU64,
    rejected_approval_plans: AtomicU64,
}

impl ForkAaveMcp {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    #[allow(dead_code)] // The dev-broker binary uses the transport, while fork tests assert this evidence.
    pub fn tool_calls(&self) -> u64 {
        self.tool_calls.load(Ordering::SeqCst)
    }

    #[allow(dead_code)] // The dev-broker binary uses the transport, while fork tests assert this evidence.
    pub fn rejected_approval_plans(&self) -> u64 {
        self.rejected_approval_plans.load(Ordering::SeqCst)
    }

    fn transaction(arguments: &Value, action: &str) -> Result<Value, AaveMcpError> {
        let parse_address = |field: &str| -> Result<Address, AaveMcpError> {
            Address::from_str(arguments[field].as_str().ok_or_else(|| {
                AaveMcpError::Protocol(format!("fixture request omitted {field}"))
            })?)
            .map_err(|_| AaveMcpError::Protocol(format!("fixture {field} is not an address")))
        };
        let token = parse_address("token")?;
        let owner = parse_address("sender")?;
        let pool = parse_address("market")?;
        let amount = if action == "collateral" {
            U256::ZERO
        } else if arguments["max"].as_bool() == Some(true) {
            U256::MAX
        } else {
            let decimals = fixture_token_decimals(arguments["token"].as_str().unwrap_or_default())?;
            let decimal = arguments["amount"]
                .as_str()
                .ok_or_else(|| AaveMcpError::Protocol("fixture request omitted amount".into()))?;
            let exact = Quantity::from_decimal(decimal, decimals)
                .map_err(|_| AaveMcpError::Protocol("fixture amount is invalid".into()))?;
            u256(&exact)
                .map_err(|_| AaveMcpError::Protocol("fixture amount exceeds uint256".into()))?
        };
        let (operation, calldata) = match action {
            "supply" => (
                "SUPPLY",
                FixturePool::supplyCall {
                    asset: token,
                    amount,
                    onBehalfOf: owner,
                    referralCode: 0,
                }
                .abi_encode(),
            ),
            "borrow" => (
                "BORROW",
                FixturePool::borrowCall {
                    asset: token,
                    amount,
                    interestRateMode: U256::from(2),
                    referralCode: 0,
                    onBehalfOf: owner,
                }
                .abi_encode(),
            ),
            "repay" => (
                "REPAY",
                FixturePool::repayCall {
                    asset: token,
                    amount,
                    interestRateMode: U256::from(2),
                    onBehalfOf: owner,
                }
                .abi_encode(),
            ),
            "withdraw" => (
                "WITHDRAW",
                FixturePool::withdrawCall {
                    asset: token,
                    amount,
                    to: owner,
                }
                .abi_encode(),
            ),
            "collateral" => (
                "SET_COLLATERAL",
                FixturePool::setUserUseReserveAsCollateralCall {
                    asset: token,
                    useAsCollateral: arguments["enable"].as_bool().ok_or_else(|| {
                        AaveMcpError::Protocol("fixture request omitted collateral state".into())
                    })?,
                }
                .abi_encode(),
            ),
            _ => {
                return Err(AaveMcpError::UnsupportedPlan(
                    "fixture received an unsupported operation".into(),
                ));
            }
        };
        Ok(json!({
            "__typename":"TransactionRequest", "operation":operation,
            "to":format!("{pool:#x}"), "from":format!("{owner:#x}"),
            "data":format!("0x{}", hex::encode(calldata)), "value":"0",
            "chainId":arguments["chainId"]
        }))
    }

    fn result(&self, request: &Value) -> Result<Value, AaveMcpError> {
        match request["method"].as_str() {
            Some("initialize") => Ok(json!({
                "protocolVersion":PROTOCOL_VERSION,
                "serverInfo":{"name":"aave-mcp","version":"owned-fork-fixture"},
                "capabilities":{}
            })),
            Some("notifications/initialized") => Ok(Value::Null),
            Some("tools/call") => {
                self.tool_calls.fetch_add(1, Ordering::SeqCst);
                let arguments = &request["params"]["arguments"];
                let action = if request["params"]["name"] == "prepare_set_collateral" {
                    "collateral"
                } else {
                    arguments["action"].as_str().ok_or_else(|| {
                        AaveMcpError::Protocol("fixture tool action is missing".into())
                    })?
                };
                let data = if action == "supply"
                    && arguments["amount"].as_str() == Some("0.000000000000000001")
                {
                    self.rejected_approval_plans.fetch_add(1, Ordering::SeqCst);
                    let original = Self::transaction(arguments, action)?;
                    json!({
                        "__typename":"ApprovalRequired",
                        "approval":{"__typename":"TransactionRequest","to":arguments["token"],"from":arguments["sender"],"data":"0x095ea7b3","value":"0","chainId":arguments["chainId"]},
                        "originalTransaction":original
                    })
                } else {
                    Self::transaction(arguments, action)?
                };
                Ok(json!({"structuredContent":{"data":data}}))
            }
            _ => Err(AaveMcpError::Protocol(
                "fixture received unsupported MCP method".into(),
            )),
        }
    }
}

#[async_trait]
impl AaveMcpTransport for ForkAaveMcp {
    async fn post(&self, request: McpHttpRequest) -> Result<McpHttpResponse, AaveMcpError> {
        let request: Value = serde_json::from_slice(&request.body)
            .map_err(|_| AaveMcpError::Protocol("fixture request is not JSON".into()))?;
        if request["method"] == "notifications/initialized" {
            return Ok(McpHttpResponse {
                status: 202,
                headers: BTreeMap::new(),
                body: Vec::new(),
            });
        }
        let id = request["id"]
            .as_u64()
            .ok_or_else(|| AaveMcpError::Protocol("fixture request has no id".into()))?;
        let result = self.result(&request)?;
        let mut headers = BTreeMap::from([("content-type".into(), "application/json".into())]);
        if request["method"] == "initialize" {
            headers.insert("mcp-session-id".into(), "owned-aave-fork-fixture".into());
        }
        Ok(McpHttpResponse {
            status: 200,
            headers,
            body: serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"result":result}))
                .map_err(|_| AaveMcpError::Protocol("fixture response encoding failed".into()))?,
        })
    }
}
