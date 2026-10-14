//! Opt-in public mainnet reads through Fin's transport and reviewed defaults.
//! No broker enrollment, credentials, signing or transaction submission.
use fin_evm::{EvmRpc, HttpRpc, rpc_u256};
use serde_json::json;

const WETH: &str = "0x82af49447d8a07e3bd95bd0d56f35241523fbab1";
const ARCHIVE_RPC: &str = "https://arb-mainnet.g.alchemy.com/public";

#[tokio::test]
#[ignore = "opt-in public mainnet read; requires HTTPS and provider availability"]
async fn arbitrum_public_archive_and_default_pool_read_historical_weth() {
    let network = fin_networks::by_chain_id(42161).unwrap();
    assert_eq!(network.rpc_urls.last().copied(), Some(ARCHIVE_RPC));
    let archive = HttpRpc::new(vec![ARCHIVE_RPC.into()], network.chain_id).unwrap();
    let defaults = HttpRpc::new(
        network.rpc_urls.iter().map(|url| (*url).into()).collect(),
        network.chain_id,
    )
    .unwrap();
    // Independently compared with Nodies on 2026-10-12. Mutable supply differs
    // from latest state, so a gateway silently ignoring the tag cannot pass.
    for (height, block_hash, supply) in [
        (
            100_000_000_u64, // 2023-06-11
            "0xb5aeb03c97e45c59596b70905d077663bccfea4533bf3b2c3264871725ea86a8",
            "316134577423735736063261",
        ),
        (
            300_000_000, // 2025-01-28
            "0xcd596eb42e1004bed0f83993ef42849a4f065b84940241a65416f47a61f5a5b3",
            "216986589354614912817827",
        ),
    ] {
        for rpc in [&archive, &defaults] {
            let header = rpc
                .request(
                    "eth_getBlockByNumber",
                    json!([format!("0x{height:x}"), false]),
                )
                .await
                .unwrap();
            assert_eq!(header["hash"], block_hash);
            assert_eq!(header["number"], format!("0x{height:x}"));
            for tag in [
                json!(format!("0x{height:x}")),
                json!({"blockHash":block_hash, "requireCanonical":true}),
            ] {
                let result = rpc
                    .request("eth_call", json!([{"to":WETH, "data":"0x18160ddd"}, tag]))
                    .await
                    .unwrap();
                assert_eq!(rpc_u256(&result).unwrap().to_string(), supply);
            }
        }
    }
}
