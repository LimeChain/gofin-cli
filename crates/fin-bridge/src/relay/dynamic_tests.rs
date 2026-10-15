//! Strictly synthetic Relay HTTP/currency metadata; no API or chain access.
use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const SOURCE: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DESTINATION: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const REFUND: &str = "0xcccccccccccccccccccccccccccccccccccccccc";

#[test]
fn zero_fee_rows_do_not_require_invented_debit_caps_but_nonzero_unknown_fees_do() {
    let currency = json!({"chainId":1,"address":format!("{:#x}",Address::ZERO),"decimals":18});
    let mut raw = json!({"fees":{"gas":{"currency":currency,"amount":"10"},"app":{"currency":currency,"amount":"0"},"subsidy":{"currency":currency,"amount":"0"}}});
    let mut bounds = BridgeBounds::default();
    bounds.fee_caps.push(crate::FeeCap {
        asset: crate::EvmAsset {
            chain_id: 1,
            token: None,
            decimals: 18,
        }
        .asset_id(),
        kind: FeeKind::Gas,
        maximum: Quantity::from_u64(10),
        maximum_usd_micros: None,
    });
    let fees = parse_order_fees(&raw, &config()).unwrap();
    assert_eq!(fees.len(), 1);
    validate_fee_caps(&fees, &bounds).unwrap();
    raw["fees"]["app"]["amount"] = json!("1");
    let fees = parse_order_fees(&raw, &config()).unwrap();
    assert_eq!(fees.len(), 2);
    assert_eq!(
        validate_fee_caps(&fees, &bounds).unwrap_err().code,
        "BRIDGE_BOUNDS_VIOLATED"
    );
    raw["fees"]["app"]["amount"] = json!("0.0");
    assert!(parse_order_fees(&raw, &config()).is_err());
    raw["fees"]["app"]["amount"] = json!(0);
    assert!(parse_order_fees(&raw, &config()).is_err());
    raw["fees"]["app"]["amount"] = json!("0");
    raw["fees"]["app"]["currency"]["decimals"] = json!(6);
    assert!(parse_order_fees(&raw, &config()).is_err());
}

fn config() -> ReviewedConfig {
    ReviewedConfig {
        chains: [(1, "ethereum"), (8453, "base"), (42161, "arbitrum")]
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
fn asset(chain_id: u64, token: &str, decimals: u8) -> crate::EvmAsset {
    crate::EvmAsset {
        chain_id,
        token: Some(token.into()),
        decimals,
    }
}
fn intent() -> BridgeIntent {
    BridgeIntent {
        wallet: fin_domain::ResourceId("evm:synthetic-wallet".into()),
        source_address: "0x1111111111111111111111111111111111111111".into(),
        source: Some(asset(8453, SOURCE, 6)),
        solana_source: None,
        solana_destination: None,
        destination_chain_id: Some(1),
        destination_assets: vec![asset(1, DESTINATION, 18)],
        recipient: "0x2222222222222222222222222222222222222222".into(),
        refund_to: "0x1111111111111111111111111111111111111111".into(),
        allowed_refund_assets: vec![asset(8453, SOURCE, 6), asset(1, REFUND, 6)],
        allowed_solana_refund_assets: vec![],
        amount: Quantity::from_u64(1_000_000),
    }
}
fn request() -> crate::BridgeRouteRequest {
    crate::BridgeRouteRequest {
        source: intent().source,
        solana_source: None,
        destination_chain_id: 1,
        solana_destination: None,
        destination_asset: None,
    }
}
fn currencies() -> Value {
    json!([
        {"chainId":8453,"address":SOURCE,"decimals":6,"vmType":"evm","symbol":"LOOKALIKE"},
        {"chainId":1,"address":DESTINATION,"decimals":18,"vmType":"evm","symbol":"DIFFERENT"},
        {"chainId":1,"address":REFUND,"decimals":6,"vmType":"evm","symbol":"LOOKALIKE"}
    ])
}

#[test]
fn dynamic_exact_assets_support_base_and_heterogeneous_precision_without_mutating_deployments() {
    let original = config();
    assert!(original.tokens.is_empty());
    let scoped = original.with_intent_assets(&intent()).unwrap();
    validate_intent(&scoped, &intent(), &BridgeBounds::default()).unwrap();
    assert_eq!(scoped.token_decimals(8453, SOURCE), Some(6));
    assert_eq!(scoped.token_decimals(1, DESTINATION), Some(18));
    assert_eq!(scoped.token_decimals(1, REFUND), Some(6));
    assert_eq!(scoped.tokens.len(), 3);
    assert!(original.tokens.is_empty());
    assert_eq!(scoped.chains, original.chains);
    assert_eq!(
        scoped.settlement_hub_address,
        original.settlement_hub_address
    );
    // Identical contract bytes on another chain are independently observed identities.
    let mut cross_chain = intent();
    cross_chain.destination_assets = vec![asset(1, SOURCE, 18)];
    let scoped = original.with_intent_assets(&cross_chain).unwrap();
    assert_eq!(scoped.token_decimals(8453, SOURCE), Some(6));
    assert_eq!(scoped.token_decimals(1, SOURCE), Some(18));
}

#[test]
fn dynamic_exact_asset_conflicts_zero_and_unreviewed_network_fail_closed() {
    let original = config();
    let mut wrong = intent();
    wrong.allowed_refund_assets.push(asset(8453, SOURCE, 18));
    assert_eq!(
        original.with_intent_assets(&wrong).unwrap_err().code,
        "BRIDGE_ASSET_INVALID"
    );
    let mut pinned = original.clone();
    pinned.tokens.insert(ReviewedTokenKey {
        chain_id: 8453,
        address: SOURCE.into(),
        decimals: 18,
    });
    assert_eq!(
        pinned.with_intent_assets(&intent()).unwrap_err().code,
        "BRIDGE_ASSET_INVALID"
    );
    wrong = intent();
    wrong.source.as_mut().unwrap().token = Some(address_string(Address::ZERO));
    assert!(original.with_intent_assets(&wrong).is_err());
    wrong = intent();
    wrong.source.as_mut().unwrap().chain_id = 10;
    assert_eq!(
        original.with_intent_assets(&wrong).unwrap_err().code,
        "BRIDGE_ROUTE_UNAVAILABLE"
    );
}

#[test]
fn currency_related_candidates_require_exact_source_chain_contract_precision_and_vm() {
    let request = request();
    let good = currencies();
    assert_eq!(parse_currency_candidates(&good, &request).unwrap().len(), 2);
    for (field, wrong) in [
        ("chainId", json!(1)),
        ("address", json!(DESTINATION)),
        ("decimals", json!(18)),
        ("decimals", json!("6")),
        ("vmType", json!("svm")),
        ("vmType", Value::Null),
    ] {
        let mut bad = good.clone();
        bad[0][field] = wrong;
        assert!(
            parse_currency_candidates(&bad, &request)
                .unwrap()
                .is_empty(),
            "{field}"
        );
    }
    let mut symbols_only = good.clone();
    symbols_only.as_array_mut().unwrap().remove(0);
    assert!(
        parse_currency_candidates(&symbols_only, &request)
            .unwrap()
            .is_empty()
    );
    let mut symbol_change = good;
    symbol_change[0]["symbol"] = Value::Null;
    symbol_change[1]["symbol"] = json!("LOOKALIKE");
    assert_eq!(
        parse_currency_candidates(&symbol_change, &request)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn currency_destination_filter_and_precision_are_exact_and_ambiguous_identity_is_rejected() {
    let request = request();
    let mut raw = currencies();
    let extra = json!({"chainId":42161,"address":DESTINATION,"decimals":6,"vmType":"evm"});
    raw.as_array_mut().unwrap().push(extra);
    raw.as_array_mut()
        .unwrap()
        .push(json!({"chainId":1,"address":SOURCE,"decimals":6,"vmType":"svm"}));
    let duplicate = raw[1].clone();
    raw.as_array_mut().unwrap().push(duplicate);
    let candidates = parse_currency_candidates(&raw, &request).unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(
        candidates[0].destination.as_ref().unwrap().token.as_deref(),
        Some(DESTINATION)
    );
    assert_eq!(candidates[0].reported_decimals, Some(18));
    assert_eq!(candidates[1].reported_decimals, Some(6));
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.destination.as_ref().unwrap().chain_id == 1)
    );
    for bad in [json!(-1), json!(256), json!("18"), json!(1.5), Value::Null] {
        let mut malformed = currencies();
        malformed[1]["decimals"] = bad;
        assert!(parse_currency_candidates(&malformed, &request).is_err());
    }
    for bad in [Value::Null, json!("0x1234")] {
        let mut malformed = currencies();
        malformed[1]["address"] = bad;
        assert!(parse_currency_candidates(&malformed, &request).is_err());
    }
    let mut conflict = currencies();
    let mut duplicate = conflict[1].clone();
    duplicate["decimals"] = json!(6);
    conflict.as_array_mut().unwrap().push(duplicate);
    assert_eq!(
        parse_currency_candidates(&conflict, &request)
            .unwrap_err()
            .code,
        "BRIDGE_RESPONSE_INVALID"
    );
    let mut native = currencies();
    native[1]["address"] = json!(address_string(Address::ZERO));
    assert!(
        parse_currency_candidates(&native, &request).unwrap()[0]
            .destination
            .as_ref()
            .unwrap()
            .token
            .is_none()
    );
    native[1]["decimals"] = json!(6);
    assert_eq!(
        parse_currency_candidates(&native, &request)
            .unwrap_err()
            .code,
        "BRIDGE_RESPONSE_INVALID"
    );
}

async fn fixture(body: String) -> (RelayProvider, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0; 4096];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
        let length: usize = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|length| length.trim().parse().unwrap())
            })
            .unwrap();
        while bytes.len() < header_end + length {
            let mut chunk = [0; 4096];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
        }
        let response = format!(
            "HTTP/1.1 200 Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8(bytes).unwrap()
    });
    let provider = RelayProvider {
        client: Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap(),
        config: config(),
        api_base,
        api_key: None,
    };
    (provider, task)
}

#[tokio::test]
async fn currencies_http_uses_bounded_exact_source_request_and_labels_partial_candidates() {
    let (provider, task) = fixture(currencies().to_string()).await;
    let result = provider.discover_routes(&request()).await.unwrap();
    assert_eq!(result.availability, crate::RouteAvailability::Candidate);
    assert!(!result.complete);
    assert_eq!(result.candidates.len(), 2);
    assert!(
        result
            .limitations
            .iter()
            .any(|item| item.contains("partial"))
    );
    let wire = task.await.unwrap();
    assert!(wire.starts_with("POST /currencies/v2 HTTP/1.1"));
    assert!(!wire.to_ascii_lowercase().contains("x-api-key"));
    let body: Value = serde_json::from_str(wire.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(
        body,
        json!({"chainIds":[8453],"address":SOURCE,"includeAllChains":true,"limit":100,"defaultList":false})
    );
    let (provider, task) = fixture(json!([currencies()[1].clone()]).to_string()).await;
    let unknown = provider.discover_routes(&request()).await.unwrap();
    assert_eq!(unknown.availability, crate::RouteAvailability::Unknown);
    assert!(!unknown.complete);
    assert!(unknown.candidates.is_empty());
    task.await.unwrap();
}

#[tokio::test]
async fn currencies_http_rejects_count_shape_and_byte_bound_without_inferring_complete_coverage() {
    for body in [
        json!({"currencies":currencies()}).to_string(),
        json!(vec![currencies()[0].clone(); 101]).to_string(),
        format!("[{}]", " ".repeat(256 * 1024)),
    ] {
        let (provider, task) = fixture(body).await;
        assert!(provider.discover_routes(&request()).await.is_err());
        task.await.unwrap();
    }
}

#[tokio::test]
async fn explicit_destination_bypasses_partial_list_but_remains_an_unverified_candidate() {
    let provider = RelayProvider {
        client: Client::new(),
        config: config(),
        api_base: "http://127.0.0.1:0".into(),
        api_key: None,
    };
    let mut request = request();
    request.destination_asset = Some(asset(1, DESTINATION, 18));
    let result = provider.discover_routes(&request).await.unwrap();
    assert_eq!(result.availability, crate::RouteAvailability::Candidate);
    assert_eq!(result.candidates.len(), 1);
    assert!(!result.complete);
    assert!(result.evidence_source.contains("Explicit destination"));
    request.destination_asset.as_mut().unwrap().chain_id = 42161;
    assert_eq!(
        provider.discover_routes(&request).await.unwrap_err().code,
        "BRIDGE_ASSET_INVALID"
    );
}
