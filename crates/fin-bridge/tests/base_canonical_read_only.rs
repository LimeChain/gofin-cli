//! Explicit opt-in mainnet reads. No signer exists and the RPC wrapper denies
//! all transport methods other than a small read-only allowlist.
use async_trait::async_trait;
use fin_bridge::base_canonical::{
    BaseCanonicalConfig, BaseCanonicalProvider, BaseWithdrawal, L1_MESSENGER, L2_MESSENGER,
};
use fin_domain::{FinError, Quantity, Result};
use fin_evm::{EvmRpc, EvmRuntime, HttpRpc};
use serde_json::Value;
use std::sync::Arc;
struct ReadOnlyRpc(HttpRpc);
#[async_trait]
impl EvmRpc for ReadOnlyRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if !matches!(
            method,
            "eth_chainId"
                | "eth_blockNumber"
                | "eth_call"
                | "eth_getCode"
                | "eth_getBlockByNumber"
                | "eth_getProof"
                | "eth_getLogs"
        ) {
            return Err(FinError::new(
                "READ_ONLY_TEST",
                "read",
                "Mainnet verification rejects all other transport methods.",
                "Use an isolated fork for financial writes.",
            ));
        }
        self.0.request(method, params).await
    }
}
#[tokio::test]
#[ignore = "explicit read-only mainnet verification; needs working public RPCs"]
async fn reviewed_deployments_support_current_pair_and_portal_abi() {
    let eth = Arc::new(EvmRuntime::new(
        Arc::new(ReadOnlyRpc(
            HttpRpc::new(vec!["https://ethereum-rpc.publicnode.com".into()], 1).unwrap(),
        )),
        1,
        None,
    ));
    let base = Arc::new(EvmRuntime::new(
        Arc::new(ReadOnlyRpc(
            HttpRpc::new(vec!["https://mainnet.base.org".into()], 8453).unwrap(),
        )),
        8453,
        None,
    ));
    let provider = BaseCanonicalProvider::new(eth, base, BaseCanonicalConfig::reviewed()).unwrap();
    // Candidate address knowledge never grants authority; verify_pair checks
    // exact remote-token/bridge binding, deployed code, ERC165 and both decimals.
    assert_eq!(
        provider
            .verify_pair(
                "0x6b175474e89094c44da98b954eedeac495271d0f",
                "0x50c5725949a6f0c72e6c4a641f24049a917db0cb"
            )
            .await
            .unwrap(),
        18
    );
    let synthetic = BaseWithdrawal {
        nonce: Quantity::from_u64(0),
        sender: L2_MESSENGER.into(),
        target: L1_MESSENGER.into(),
        value: Quantity::from_u64(0),
        gas_limit: Quantity::from_u64(1),
        data: "0x".into(),
    };
    let readiness = provider
        .withdrawal_readiness(&synthetic, "0x1111111111111111111111111111111111111111")
        .await
        .unwrap();
    assert!(!readiness.ready);
    assert_eq!(readiness.proven_at, 0);
    assert!(!readiness.finalized);
    assert!(readiness.proof_maturity_seconds.integer() > 0u64.into());
}
