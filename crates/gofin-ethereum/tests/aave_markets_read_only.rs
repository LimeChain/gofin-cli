//! Explicit public compatibility reads; no keys, signer or financial authority.
use async_trait::async_trait;
use gofin_domain::{Adapter, GoFinError, Result};
use gofin_ethereum::{AaveConfig, EthereumAdapter, EthereumConfig, WalletConnection, aave};
use gofin_evm::{EvmRpc, EvmRuntime, HttpRpc};
use serde_json::{Value, json};
use std::sync::Arc;
struct PublicReads(HttpRpc);
#[async_trait]
impl EvmRpc for PublicReads {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if !matches!(
            method,
            "eth_chainId"
                | "eth_blockNumber"
                | "eth_getBlockByNumber"
                | "eth_getCode"
                | "eth_call"
                | "eth_getBalance"
        ) {
            return Err(GoFinError::validation(
                "Public compatibility checks cannot write.",
            ));
        }
        let result = self.0.request(method, params.clone()).await;
        if result.is_err() {
            println!(
                "AAVE_PUBLIC_READ_FAILURE={}",
                json!({"method":method,"params":params,"error":result.as_ref().err()})
            );
        }
        result
    }
}
#[tokio::test]
#[ignore = "Explicit public Aave market compatibility reads; no signer or writes"]
async fn reviewed_aave_markets_have_correlated_profiles_and_reserve_revisions() -> Result<()> {
    for chain in [1, 8453, 42161, 4663] {
        let network = gofin_networks::by_chain_id(chain).unwrap();
        let profiles = aave::reviewed_markets(chain);
        let mut config = EthereumConfig::wallet_only(chain, network.minimum_confirmations, vec![]);
        config.aave_markets = profiles
            .iter()
            .map(|p| AaveConfig {
                pool: p.pool.clone(),
                oracle: p.oracle.clone(),
            })
            .collect();
        config.aave = config.aave_markets.first().cloned();
        let rpc = Arc::new(PublicReads(HttpRpc::new(
            network.rpc_urls.iter().map(|url| url.to_string()).collect(),
            chain,
        )?));
        let adapter = EthereumAdapter::new(
            config,
            vec![WalletConnection {
                name: "public".into(),
                address: "0x1111111111111111111111111111111111111111".into(),
                signing_enabled: false,
            }],
            Arc::new(EvmRuntime::new(rpc, chain, None)),
        )?;
        let input = json!({"wallet":"public","network":"evm","chain_id":chain});
        let markets = adapter.read("aave.markets", &input).await?;
        println!("AAVE_MARKETS={markets}");
        assert_eq!(markets["data"]["available"], json!(!profiles.is_empty()));
        for market in markets["data"]["markets"].as_array().unwrap() {
            assert_eq!(market["provider_verified"], true);
            assert_eq!(market["risk_revision_compatible"], true);
        }
        for profile in profiles {
            let token = match chain {
                1 => "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                8453 => "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",
                42161 => "0xaf88d065e77c8cc2239327c5edb3a432268e5831",
                _ => unreachable!(),
            };
            let mut input = input.clone();
            input["market"] = json!(profile.pool);
            input["tokens"] = json!([token]);
            let reserve = adapter.read("aave.reserves", &input).await?;
            assert_eq!(reserve["data"][0]["token"]["reference"], json!(token));
            println!("AAVE_RESERVE={}", json!({"market":profile,"read":reserve}));
        }
    }
    Ok(())
}
