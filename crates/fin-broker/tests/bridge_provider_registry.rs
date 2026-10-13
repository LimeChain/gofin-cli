//! Provider aggregation and provider-preserving refresh over the public broker
//! surface. This uses the real Canonical quote/prepare implementation and a
//! synthetic unsupported Relay result; it performs no network or wallet writes.
use async_trait::async_trait;
use fin_bridge::{
    BridgeAssetInput, BridgeBounds, BridgeCapabilities, BridgeEvidenceCandidate, BridgeProvider,
    BridgeQuote, BridgeQuoteBatch, BridgeQuoteFailure, BridgeQuoteInput, BridgeQuoteRequest,
    BridgeReconciliation, BridgeTransferRef, PreparedBridge,
    canonical::{CanonicalConfig, CanonicalProvider},
};
use fin_broker::{BridgeAdapter, SharedUsdValuator};
use fin_domain::{Adapter, AssetId, FinError, Quantity, Result, now};
use fin_ethereum::{EthereumAdapter, EthereumConfig, WalletConnection};
use fin_evm::{EvmRpc, EvmRuntime, LocalEnvelopeSigner, lower_address};
use fin_pricing::{UsdPriceSource, UsdQuote};
use fin_store::{BridgeStateRepository, Store};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

fn q(value: &str) -> Quantity {
    Quantity::new(value).unwrap()
}

struct Rpc {}
#[async_trait]
impl EvmRpc for Rpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        Ok(match method {
            "eth_chainId" => json!("0x1"),
            "eth_call" => json!("0x"),
            "eth_estimateGas" => json!("0x5208"),
            "eth_maxPriorityFeePerGas" => json!("0x3b9aca00"),
            "eth_blockNumber" => json!("0x64"),
            "eth_getBlockByNumber" => {
                json!({"hash":format!("0x{}","44".repeat(32)),"number":params[0],"baseFeePerGas":"0x3b9aca00"})
            }
            "eth_getTransactionCount" => json!("0x0"),
            "eth_getBalance" => json!("0xde0b6b3a7640000"),
            "eth_getCode" => json!("0x"),
            other => panic!("unexpected read-only provider RPC {other}"),
        })
    }
}

struct Prices;
#[async_trait]
impl UsdPriceSource for Prices {
    async fn quote(&self, _: &AssetId) -> Result<UsdQuote> {
        Ok(UsdQuote {
            usd_per_unit: "2000".into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic fixed price".into(),
        })
    }
}

struct UnsupportedRelay;
#[async_trait]
impl BridgeProvider for UnsupportedRelay {
    fn capabilities(&self) -> BridgeCapabilities {
        BridgeCapabilities {
            provider: "relay".into(),
            chains: BTreeSet::from([1, 42161, 4663]),
            native_eth: true,
            erc20: true,
            protocol_versions: BTreeSet::from(["synthetic".into()]),
        }
    }
    async fn quote(&self, request: &BridgeQuoteRequest) -> Result<BridgeQuoteBatch> {
        Ok(BridgeQuoteBatch {
            quotes: vec![],
            failures: request
                .intent
                .destination_assets
                .iter()
                .cloned()
                .map(|destination_asset| BridgeQuoteFailure {
                    provider: None,
                    destination_asset,
                    error: FinError::new(
                        "BRIDGE_ROUTE_UNAVAILABLE",
                        "quote",
                        "No synthetic Relay route is configured.",
                        "Keep the saved provider or request another explicit quote.",
                    ),
                })
                .collect(),
        })
    }
    fn prepare(&self, _: &BridgeQuote, _: &BridgeBounds, _: &str) -> Result<PreparedBridge> {
        unreachable!("unsupported synthetic provider never returns quotes")
    }
    async fn reconcile(&self, _: &BridgeTransferRef) -> Result<BridgeReconciliation> {
        Ok(BridgeReconciliation {
            status: "pending".into(),
            failure: None,
            refund_fail_reason: None,
            candidates: Vec::<BridgeEvidenceCandidate>::new(),
        })
    }
}

#[tokio::test]
async fn canonical_quote_is_attributed_and_refresh_keeps_its_provider_and_step_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path().join("journal.sqlite"))
        .unwrap()
        .share();
    let signer = Arc::new(LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap());
    let sender = lower_address(signer.address());
    let rpc = Arc::new(Rpc {});
    let wallet_runtime = Arc::new(EvmRuntime::new(rpc.clone(), 1, Some(signer)));
    let source_adapter = Arc::new(
        EthereumAdapter::new(
            EthereumConfig::wallet_only(1, 1, vec![]),
            vec![WalletConnection {
                name: "wallet".into(),
                address: sender.clone(),
                signing_enabled: true,
            }],
            wallet_runtime.clone(),
        )
        .unwrap(),
    );
    let prices = Arc::new(SharedUsdValuator::with_source(Arc::new(Prices)));
    let canonical = Arc::new(
        CanonicalProvider::new(
            Arc::new(EvmRuntime::new(rpc, 1, None)),
            CanonicalConfig::reviewed(),
        )
        .unwrap(),
    );
    let bridge = BridgeAdapter::with_providers(
        vec![Arc::new(UnsupportedRelay), canonical],
        BTreeMap::from([(1, source_adapter)]),
        BTreeMap::from([(1, wallet_runtime)]),
        prices,
    )
    .unwrap();
    bridge
        .attach_state(BridgeStateRepository::new(store))
        .unwrap();
    let input = |refresh_transfer_id: Option<String>| BridgeQuoteInput {
        provider: None,
        cctp: None,
        intent: fin_bridge::BridgeQuoteIntentInput {
            wallet: "wallet".into(),
            source: BridgeAssetInput {
                chain_id: 1,
                token: None,
            },
            solana_destination: None,
            destination_chain_id: Some(42161),
            destination_assets: vec![BridgeAssetInput {
                chain_id: 42161,
                token: None,
            }],
            recipient: sender.clone(),
            refund_to: sender.clone(),
            allowed_refund_assets: vec![],
            amount: q("10000000000000000"),
        },
        bounds: BridgeBounds::default(),
        refresh_transfer_id,
    };
    let first = bridge
        .read("bridge.quotes", &serde_json::to_value(input(None)).unwrap())
        .await
        .unwrap();
    assert_eq!(first["coverage"]["compared_providers"], 2);
    assert_eq!(first["coverage"]["market_wide_comparison"], false);
    assert_eq!(first["transfers"].as_array().unwrap().len(), 1);
    assert_eq!(
        first["transfers"][0]["quote"]["provider"],
        "arbitrum_canonical"
    );
    assert_eq!(first["failures"].as_array().unwrap().len(), 1);
    assert_eq!(first["failures"][0]["provider"], "relay");
    assert_eq!(first["transfers"][0]["steps"].as_array().unwrap().len(), 1);
    let transfer_id = first["transfers"][0]["transfer_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let step_id = first["transfers"][0]["steps"][0]["step_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let refreshed = bridge
        .read(
            "bridge.quotes",
            &serde_json::to_value(input(Some(transfer_id.clone()))).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(refreshed["coverage"]["compared_providers"], 1);
    assert_eq!(refreshed["transfers"][0]["transfer_id"], transfer_id);
    assert_eq!(
        refreshed["transfers"][0]["quote"]["provider"],
        "arbitrum_canonical"
    );
    assert_eq!(refreshed["transfers"][0]["steps"][0]["step_id"], step_id);
    assert!(refreshed["failures"].as_array().unwrap().is_empty());
}
