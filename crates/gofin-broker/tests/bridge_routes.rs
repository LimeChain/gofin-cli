//! No signer, external transport, quote persistence or financial authority.
use async_trait::async_trait;
use gofin_bridge::*;
use gofin_broker::{BridgeAdapter, SharedUsdValuator};
use gofin_domain::{Adapter, AssetId, Quantity, Result, now};
use gofin_evm::{EvmRpc, EvmRuntime};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use gofin_store::{BridgeStateRepository, Store};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

const SOURCE: &str = "0x9999999999999999999999999999999999999999";
const DESTINATION: &str = "0x8888888888888888888888888888888888888888";
struct Rpc {
    chain: u64,
}
#[async_trait]
impl EvmRpc for Rpc {
    async fn request(&self, method: &str, _params: Value) -> Result<Value> {
        Ok(match method {
            "eth_chainId" => json!(format!("{:#x}", self.chain)),
            "eth_blockNumber" => json!("0x64"),
            "eth_getCode" => json!("0x6000"),
            "eth_call" => json!(format!("0x{:064x}", 18)),
            _ => panic!("Discovery called an unexpected financial/runtime method: {method}"),
        })
    }
}
struct Prices;
#[async_trait]
impl UsdPriceSource for Prices {
    async fn quote(&self, _: &AssetId) -> Result<UsdQuote> {
        panic!("Indicative route discovery does not obtain financial pricing or approval")
    }
}
struct Provider {
    wrong_chain: bool,
    wrong_decimals: bool,
}
#[async_trait]
impl BridgeProvider for Provider {
    fn capabilities(&self) -> BridgeCapabilities {
        BridgeCapabilities {
            provider: "fixture".into(),
            chains: BTreeSet::from([1, 8453]),
            native_eth: true,
            erc20: true,
            protocol_versions: BTreeSet::new(),
        }
    }
    async fn discover_routes(&self, request: &BridgeRouteRequest) -> Result<BridgeRouteDiscovery> {
        assert_eq!(request.source.token.as_deref(), Some(SOURCE));
        assert_eq!(request.source.decimals, 18);
        Ok(BridgeRouteDiscovery {
            provider: "fixture".into(),
            availability: RouteAvailability::Candidate,
            candidates: vec![BridgeRouteCandidate {
                destination: BridgeAssetInput {
                    chain_id: if self.wrong_chain { 1 } else { 8453 },
                    token: Some(DESTINATION.into()),
                },
                reported_decimals: Some(if self.wrong_decimals { 6 } else { 18 }),
            }],
            observed_at: now(),
            evidence_source: "synthetic exact-identity candidates".into(),
            complete: false,
            limitations: vec!["partial candidates; executable quote required".into()],
            error: None,
        })
    }
    async fn quote(&self, _: &BridgeQuoteRequest) -> Result<BridgeQuoteBatch> {
        panic!("Discovery must not persist executable quotes")
    }
    fn prepare(&self, _: &BridgeQuote, _: &BridgeBounds, _: &str) -> Result<PreparedBridge> {
        panic!("Discovery must not prepare financial writes")
    }
    async fn reconcile(&self, _: &BridgeTransferRef) -> Result<BridgeReconciliation> {
        panic!("Discovery has no saved transfer")
    }
}
fn fixture(provider: Provider) -> (tempfile::TempDir, BridgeAdapter, BridgeStateRepository) {
    let dir = tempfile::tempdir().unwrap();
    let state = BridgeStateRepository::new(
        Store::open(dir.path().join("journal.sqlite"))
            .unwrap()
            .share(),
    );
    let observers = BTreeMap::from([1, 8453].map(|chain| {
        (
            chain,
            Arc::new(EvmRuntime::new(Arc::new(Rpc { chain }), chain, None)),
        )
    }));
    let adapter = BridgeAdapter::with_providers(
        vec![Arc::new(provider)],
        BTreeMap::new(),
        observers,
        Arc::new(SharedUsdValuator::with_source(Arc::new(Prices))),
    )
    .unwrap();
    adapter.attach_state(state.clone()).unwrap();
    (dir, adapter, state)
}
#[tokio::test]
async fn dynamically_resolved_non_allowlisted_tokens_are_indicative_and_have_no_authority() {
    let (_dir, adapter, state) = fixture(Provider {
        wrong_chain: false,
        wrong_decimals: false,
    });
    let result = adapter
        .read(
            "bridge.routes",
            &json!({"source":{"chainId":1,"token":SOURCE},"destinationChainId":8453}),
        )
        .await
        .unwrap();
    assert_eq!(result["source"]["decimals"], 18);
    assert_eq!(
        result["routes"][0]["destination_asset"]["token"],
        DESTINATION
    );
    assert_eq!(result["routes"][0]["identity_verified"], true);
    assert_eq!(result["routes"][0]["availability"], "candidate");
    assert_eq!(result["indicative"], true);
    assert_eq!(result["coverage"]["complete"], false);
    assert!(result.get("transfer_id").is_none());
    assert!(state.transfers().unwrap().is_empty());
    for bad in [
        json!({"source":{"chainId":1,"symbol":"USDC"},"destinationChainId":8453}),
        json!({"source":{"chainId":1},"destinationChainId":1}),
        json!({"source":{"chainId":1},"destinationChainId":56}),
    ] {
        assert!(adapter.read("bridge.routes", &bad).await.is_err());
    }
}
#[tokio::test]
async fn malicious_provider_chain_precision_and_exact_destination_are_not_verified() {
    for provider in [
        Provider {
            wrong_chain: true,
            wrong_decimals: false,
        },
        Provider {
            wrong_chain: false,
            wrong_decimals: true,
        },
        Provider {
            wrong_chain: false,
            wrong_decimals: false,
        },
    ] {
        let (_dir, adapter, state) = fixture(provider);
        let result=adapter.read("bridge.routes",&json!({"source":{"chainId":1,"token":SOURCE},"destinationChainId":8453,"destinationAsset":{"chainId":8453,"token":"0x7777777777777777777777777777777777777777"}})).await.unwrap();
        assert_eq!(result["routes"][0]["identity_verified"], false);
        assert_eq!(result["routes"][0]["availability"], "unavailable");
        assert!(state.transfers().unwrap().is_empty());
    }
}

#[test]
fn operation_discovery_schema_cannot_accept_tokens_by_symbol_or_stage_authority() {
    let def = BridgeAdapter::operation_definitions()
        .into_iter()
        .find(|d| d.name == "bridge.routes")
        .unwrap();
    assert_eq!(def.kind, gofin_domain::OperationKind::Read);
    assert_eq!(def.semantics["may_submit"], false);
    let _ = Quantity::zero();
}
