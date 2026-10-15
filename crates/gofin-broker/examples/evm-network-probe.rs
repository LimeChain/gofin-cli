//! Explicit public, read-only connectivity check. No configuration or keys.
use gofin_broker::NetworkConfig;
use gofin_evm::{EvmRpc, HttpRpc};
use serde_json::{Value, json};

#[tokio::main]
async fn main() {
    if std::env::args().skip(1).collect::<Vec<_>>() != ["--public-read-only"] {
        eprintln!(
            "Pass --public-read-only to check published EVM RPCs; no wallet or credentials are used."
        );
        std::process::exit(2);
    }
    let mut results = Vec::new();
    for network in NetworkConfig::reviewed_evm_networks()
        .into_iter()
        .filter(|network| network.chain_id != 1)
    {
        let rpc =
            HttpRpc::new(network.rpc_urls.clone(), network.chain_id).expect("reviewed profile");
        let mut result = json!({"chain_id":network.chain_id,"rpc_urls":network.rpc_urls,"financial_writes":false});
        for (name, method, params) in [
            ("identity", "eth_chainId", json!([])),
            (
                "finalized",
                "eth_getBlockByNumber",
                json!(["finalized", false]),
            ),
            ("priority_fee", "eth_maxPriorityFeePerGas", json!([])),
        ] {
            result[name] = match rpc.request(method, params).await {
                Ok(value) if name == "finalized" => {
                    json!({"available":!value.is_null(),"number":value["number"],"hash":value["hash"]})
                }
                Ok(value) => value,
                Err(error) => json!({"error":error.code}),
            };
        }
        let token = if network.chain_id == 42161 {
            "0xaf88d065e77c8cc2239327c5edb3a432268e5831"
        } else {
            "0x5fc5360d0400a0fd4f2af552add042d716f1d168"
        };
        result["reviewed_token_decimals"] = match rpc
            .request(
                "eth_call",
                json!([{"to":token,"data":"0x313ce567"},"latest"]),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => json!({"error":error.code}),
        };
        results.push(result);
    }
    let failed = results.iter().any(|result| {
        result["identity"]
            .as_str()
            .and_then(|value| u64::from_str_radix(value.trim_start_matches("0x"), 16).ok())
            != result["chain_id"].as_u64()
            || result["finalized"]["available"] != true
            || result["priority_fee"].get("error").is_some()
            || result["reviewed_token_decimals"].get("error").is_some()
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&Value::Array(results)).expect("safe JSON")
    );
    if failed {
        std::process::exit(1);
    }
}
