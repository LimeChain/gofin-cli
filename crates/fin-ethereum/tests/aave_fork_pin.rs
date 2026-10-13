//! Public chain pin through Fin's maintained read transport; no signer.
use fin_domain::Result;
use fin_evm::{EvmRpc, HttpRpc, rpc_u64};
use serde_json::json;
#[tokio::test]
#[ignore = "Explicit public read for an independently owned Aave fork"]
async fn public_aave_fork_pin() -> Result<()> {
    let chain = std::env::var("FIN_AAVE_PIN_CHAIN_ID")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let network = fin_networks::by_chain_id(chain)
        .ok_or_else(|| fin_domain::FinError::validation("Unknown fork network."))?;
    let rpc = HttpRpc::new(
        network.rpc_urls.iter().map(|v| v.to_string()).collect(),
        chain,
    )?;
    assert_eq!(
        rpc_u64(&rpc.request("eth_chainId", json!([])).await?)?,
        chain
    );
    let height = if let Ok(saved) = std::env::var("FIN_AAVE_PIN_BLOCK") {
        saved
            .parse::<u64>()
            .ok()
            .filter(|v| *v > 0)
            .ok_or_else(|| fin_domain::FinError::validation("Saved fork block must be positive."))?
    } else {
        let latest = rpc.request("eth_blockNumber", json!([])).await?;
        rpc_u64(&latest)?
            .checked_sub(if chain == 1 { 5 } else { 10 })
            .ok_or_else(|| fin_domain::FinError::validation("Fork head lacks a stable parent."))?
    };
    let block = rpc
        .request(
            "eth_getBlockByNumber",
            json!([format!("{height:#x}"), false]),
        )
        .await?;
    println!(
        "AAVE_FORK_PIN={}",
        json!({"chain_id":chain,"block":rpc_u64(&block["number"])? ,"hash":block["hash"],"timestamp":rpc_u64(&block["timestamp"])? ,"direct_rpc_reason":"Fin exposes no public fork-control lifecycle; pinning is read-only test orchestration."})
    );
    Ok(())
}
