//! Opt-in public, read-only quote smoke tests. No wallet or transaction is submitted.
use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

fn config() -> ReviewedConfig {
    ReviewedConfig {
        chains: [(8453, "base"), (42161, "arbitrum")]
            .into_iter()
            .map(|(chain_id, protocol_chain_id)| {
                (
                    chain_id,
                    ReviewedChain {
                        chain_id,
                        protocol_chain_id: protocol_chain_id.into(),
                        depository: "0x4cd00e387622c35bddb9b4c962c136462338bc31".into(),
                        router_v3: Some("0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f".into()),
                        protocol: "relay-v2".into(),
                    },
                )
            })
            .collect(),
        tokens: BTreeSet::new(),
        relay_solver_protocol_chain_id: "base".into(),
        settlement_hub_chain_id: 537713,
        settlement_hub_address: "0xDDD361727C22A01EB137880678A20b0BEaE69318".into(),
    }
}

fn request() -> BridgeQuoteRequest {
    let source = crate::EvmAsset {
        chain_id: 8453,
        token: None,
        decimals: 18,
    };
    let destination = crate::EvmAsset {
        chain_id: 42161,
        token: None,
        decimals: 18,
    };
    let address = "0x1111111111111111111111111111111111111111";
    let fee_caps = [source.asset_id(), destination.asset_id()]
        .into_iter()
        .flat_map(|asset| {
            [
                FeeKind::Gas,
                FeeKind::AdditionalDebit,
                FeeKind::WithheldFromOutput,
            ]
            .into_iter()
            .map(move |kind| crate::FeeCap {
                asset: asset.clone(),
                kind,
                maximum: Quantity::from_u64(1_000_000_000_000_000),
                maximum_usd_micros: None,
            })
        })
        .collect();
    BridgeQuoteRequest {
        cctp: None,
        intent: BridgeIntent {
            wallet: gofin_domain::ResourceId("evm:read-only-smoke".into()),
            source_address: address.into(),
            source: Some(source.clone()),
            solana_source: None,
            solana_destination: None,
            destination_chain_id: Some(42161),
            destination_assets: vec![destination.clone()],
            recipient: address.into(),
            refund_to: address.into(),
            allowed_refund_assets: vec![source, destination],
            allowed_solana_refund_assets: vec![],
            amount: Quantity::from_u64(1_000_000_000_000_000),
        },
        bounds: BridgeBounds {
            fee_caps,
            ..Default::default()
        },
    }
}

async fn assert_live_quote(provider: RelayProvider) {
    let request = request();
    let batch = provider
        .quote(&request)
        .await
        .expect("quote request must validate");
    assert!(
        batch.failures.is_empty(),
        "quote failures: {:?}",
        batch.failures
    );
    assert_eq!(batch.quotes.len(), 1);
    let quote = &batch.quotes[0];
    assert_eq!(quote.provider, "relay");
    assert_eq!(quote.intent, request.intent);
    assert_eq!(
        quote.gross_debit.asset,
        request.intent.source.as_ref().unwrap().asset_id()
    );
    assert_eq!(
        quote.expected_receipt.asset,
        request.intent.destination_assets[0].asset_id()
    );
}

#[tokio::test]
#[ignore = "requires live public Relay quote service; read-only"]
async fn live_default_proxy_quote() {
    let mut provider = RelayProvider::new(config()).unwrap();
    // A successful quote must come from the default proxy, not fallback.
    provider.fallback_base = "http://127.0.0.1:0".into();
    assert_live_quote(provider).await;
}

#[tokio::test]
#[ignore = "requires live public Relay quote service; read-only"]
async fn live_public_fallback_quote() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = [0u8; 8192];
        let _ = stream.read(&mut buffer).await.unwrap();
        stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
    });
    let mut provider = RelayProvider::new(config()).unwrap();
    provider.api_base = format!("http://{address}");
    assert_live_quote(provider).await;
    server.await.unwrap();
}
