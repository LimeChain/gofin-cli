//! Explicit public Base compatibility reads; no signer or authority exists.
use async_trait::async_trait;
use gofin_domain::{Adapter, GoFinError, Result};
use gofin_ethereum::{EthereumAdapter, EthereumConfig, WalletConnection};
use gofin_evm::{EvmRpc, EvmRuntime, rpc_u64};
use serde_json::{Value, json};
use std::sync::Arc;
const WETH: &str = "0x4200000000000000000000000000000000000006";
fn blocked(message: &str) -> GoFinError {
    GoFinError::new(
        "READ_ONLY_BASE_AAVE",
        "read",
        message,
        "Use a separately owned fork for write verification.",
    )
}

struct PublicReadOnly(gofin_evm::HttpRpc);
#[async_trait]
impl EvmRpc for PublicReadOnly {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if !matches!(
            method,
            "eth_chainId" | "eth_blockNumber" | "eth_getBlockByNumber" | "eth_getCode" | "eth_call"
        ) {
            return Err(blocked(
                "Public readiness probe rejects every write method.",
            ));
        }
        self.0.request(method, params).await
    }
}
#[tokio::test]
#[ignore = "Explicit no-secret public Base reads only; no signer or financial authority"]
async fn deployed_base_aave_revision_and_provider_readiness() -> Result<()> {
    let rpc = Arc::new(PublicReadOnly(gofin_evm::HttpRpc::new(
        vec!["https://mainnet.base.org".into()],
        8453,
    )?));
    let config = EthereumConfig::base_aave(vec![]);
    let adapter = EthereumAdapter::new(
        config,
        vec![WalletConnection {
            name: "public-read-only".into(),
            address: "0x1111111111111111111111111111111111111111".into(),
            signing_enabled: false,
        }],
        Arc::new(EvmRuntime::new(rpc.clone(), 8453, None)),
    )?;
    let block = rpc
        .request("eth_getBlockByNumber", json!(["latest", false]))
        .await?;
    let pin = json!({"block":rpc_u64(&block["number"])? ,"hash":block["hash"],"timestamp":rpc_u64(&block["timestamp"])? ,"chain_id":8453});
    println!("BASE_AAVE_PIN={pin}");
    // GoFin has no public deployed-token revision read. These explicitly allowed
    // adapter transport reads identify the compatibility gap without a signer.
    let read = adapter
        .read(
            "aave.reserves",
            &json!({"wallet":"public-read-only","network":"evm","chain_id":8453,"tokens":[WETH]}),
        )
        .await?;
    let reserve = &read["data"][0];
    let mut observed = Vec::new();
    for (token, function) in [
        (reserve["a_token"].as_str().unwrap(), "ATOKEN_REVISION()"),
        (
            reserve["variable_debt_token"].as_str().unwrap(),
            "DEBT_TOKEN_REVISION()",
        ),
    ] {
        let selector = format!(
            "0x{}",
            hex::encode(&gofin_evm::alloy::primitives::keccak256(function)[..4])
        );
        let revision = rpc
            .request(
                "eth_call",
                json!([{"to":token,"data":selector},block["number"]]),
            )
            .await?;
        observed.push(json!({"token":token,"function":function,"revision":gofin_evm::rpc_u256(&revision)?.to_string()}));
    }
    println!(
        "BASE_AAVE_READINESS={}",
        json!({"pin":pin,"profile":adapter.config,"reserve":reserve,"revisions":observed,"signer":false,"live_writes":false,"gofin_gap":"deployed token revision introspection is not a public GoFin read"})
    );
    Ok(())
}
