// Included under the existing HTTP fixture module. Every price is obtained
// through CoinGeckoPrices' production resolver/transport/parser, never a fake
// UsdPriceSource or a manually registered asset-to-coin mapping.
fn market_rows(pairs: &[(&str, &str)]) -> String {
    serde_json::json!({"symbols": pairs.iter().map(|(base, quote)| serde_json::json!({
        "symbol": format!("{base}{quote}"), "baseAsset": base, "quoteAsset": quote,
        "status": "TRADING", "isSpotTradingAllowed": true
    })).collect::<Vec<_>>()})
    .to_string()
}
fn ticker(base: &str, target: &str, id: &str, target_id: &str) -> serde_json::Value {
    serde_json::json!({"base":base,"target":target,"coin_id":id,"target_coin_id":target_id,"market":{"identifier":"binance"}})
}
fn page(rows: Vec<serde_json::Value>) -> String {
    serde_json::json!({"tickers":rows}).to_string()
}
fn response(body: &str) -> (Vec<u8>, Duration) {
    (http(200, body), Duration::ZERO)
}
async fn exchange_fixture(
    responses: Vec<(Vec<u8>, Duration)>,
) -> (Server, CoinGeckoPrices, Arc<AtomicI64>) {
    let (server, provider, clock) = fixture(responses).await;
    let mut venue = resolution::BinanceMarkets::new(provider.client.clone());
    venue.test_url = Some(format!("{}/venue/exchangeInfo", server.base));
    let provider = provider
        .with_exchange_market_source("binance", "binance", Arc::new(venue))
        .unwrap();
    (server, provider, clock)
}

#[tokio::test]
async fn apt_sei_usdc_eth_use_production_identity_and_quote_path_with_one_shared_catalog() {
    let (server, provider, _) = exchange_fixture(vec![
        response(&market_rows(&[
            ("APT", "USDC"),
            ("SEI", "USDC"),
            ("ETH", "USDC"),
        ])),
        response(&page(vec![
            ticker("APT", "USDC", "aptos", "usd-coin"),
            ticker("SEI", "USDC", "sei-network", "usd-coin"),
            ticker("ETH", "USDC", "ethereum", "usd-coin"),
        ])),
        response(&body("aptos", "0.91234567890123456789", NOW)),
        response(&body("sei-network", "0.060123456789", NOW)),
        response(&body("usd-coin", "0.9971", NOW)),
        response(&body("ethereum", "2000.123456789", NOW)),
    ])
    .await;
    for (coin, id, price) in [
        ("APT", "aptos", "0.91234567890123456789"),
        ("SEI", "sei-network", "0.060123456789"),
        ("USDC", "usd-coin", "0.9971"),
        ("ETH", "ethereum", "2000.123456789"),
    ] {
        let quote = provider.quote(&asset("binance", coin, 36)).await.unwrap();
        assert_eq!(quote.usd_per_unit, price);
        assert!(quote.source.contains(&format!("coin_id={id}")));
        assert!(quote.source.contains(&format!("exact_asset={coin}")));
    }
    assert_eq!(server.count(), 6);
    let requests = server.requests.lock().unwrap();
    assert!(requests[1].starts_with("GET /exchanges/binance/tickers?order=base_target&page=1 "));
    assert!(
        requests
            .iter()
            .all(|r| !r.contains("/search") && !r.contains("/coins/list"))
    );
}

#[tokio::test]
async fn same_label_without_exact_venue_pair_is_not_identity_evidence() {
    let (server, provider, _) = exchange_fixture(vec![
        response(&market_rows(&[("APT", "USDC")])),
        response(&page(vec![
            ticker("APT", "FAKE", "unrelated-apt", "fake"),
            ticker("APT", "USDC", "aptos", "usd-coin"),
        ])),
        response(&body("aptos", "1.1", NOW)),
    ])
    .await;
    assert_eq!(
        provider
            .quote(&asset("binance", "APT", 36))
            .await
            .unwrap()
            .usd_per_unit,
        "1.1"
    );
    assert_eq!(server.count(), 3);
    let (_server, provider, _) = exchange_fixture(vec![
        response(&market_rows(&[("APT", "USDC")])),
        response(&page(vec![ticker("APT", "FAKE", "unrelated-apt", "fake")])),
    ])
    .await;
    assert_eq!(
        provider
            .quote(&asset("binance", "APT", 36))
            .await
            .unwrap_err()
            .code,
        "PRICE_IDENTITY_UNAVAILABLE"
    );
}

#[tokio::test]
async fn conflicting_ids_or_missing_pair_identity_never_select_the_first_candidate() {
    for missing in [false, true] {
        let mut second = ticker("APT", "USDT", "unrelated-apt", "tether");
        if missing {
            second["coin_id"] = serde_json::Value::Null;
        }
        let (server, provider, _) = exchange_fixture(vec![
            response(&market_rows(&[("APT", "USDC"), ("APT", "USDT")])),
            response(&page(vec![
                ticker("APT", "USDC", "aptos", "usd-coin"),
                second,
            ])),
        ])
        .await;
        assert_eq!(
            provider
                .quote(&asset("binance", "APT", 36))
                .await
                .unwrap_err()
                .code,
            if missing {
                "PRICE_IDENTITY_UNAVAILABLE"
            } else {
                "PRICE_IDENTITY_CONFLICT"
            }
        );
        assert_eq!(server.count(), 2);
        assert!(provider.cache.lock().await.is_empty());
    }
}

fn full_page() -> Vec<serde_json::Value> {
    let mut rows = vec![ticker("APT", "USDC", "aptos", "usd-coin")];
    rows.extend((1..100).map(|i| {
        ticker(
            &format!("UNMATCHED{i}"),
            "USDC",
            &format!("other-{i}"),
            "usd-coin",
        )
    }));
    rows
}

#[tokio::test]
async fn pagination_checks_later_conflicts_and_never_commits_partial_or_repeated_pages() {
    for (second, code) in [
        (
            response(&page(vec![ticker(
                "APT",
                "USDT",
                "different-apt",
                "tether",
            )])),
            "PRICE_IDENTITY_CONFLICT",
        ),
        (
            (http(503, "SYNTHETIC-PRIVATE-FAILURE"), Duration::ZERO),
            "PRICE_UNAVAILABLE",
        ),
        (response(&page(full_page())), "PRICE_IDENTITY_DATA_INVALID"),
    ] {
        let (server, provider, _) = exchange_fixture(vec![
            response(&market_rows(&[("APT", "USDC"), ("APT", "USDT")])),
            response(&page(full_page())),
            second,
        ])
        .await;
        assert_eq!(
            provider
                .quote(&asset("binance", "APT", 36))
                .await
                .unwrap_err()
                .code,
            code
        );
        assert_eq!(server.count(), 3);
        assert!(provider.cache.lock().await.is_empty());
    }
    let (server, provider, _) = exchange_fixture(vec![
        response(&market_rows(&[("SEI", "USDC")])),
        response(&page(full_page())),
        response(&page(vec![ticker(
            "SEI",
            "USDC",
            "sei-network",
            "usd-coin",
        )])),
        response(&body("sei-network", "0.06", NOW)),
    ])
    .await;
    assert_eq!(
        provider
            .quote(&asset("binance", "SEI", 36))
            .await
            .unwrap()
            .usd_per_unit,
        "0.06"
    );
    assert!(server.requests.lock().unwrap()[2].contains("page=2"));
}

#[tokio::test]
async fn newly_listed_exact_asset_refreshes_missing_identity_without_manual_enrollment() {
    let (server, provider, clock) = exchange_fixture(vec![
        response(&market_rows(&[("NEW123", "USDC")])),
        response(&page(vec![])),
        response(&market_rows(&[("NEW123", "USDC")])),
        response(&page(vec![ticker(
            "NEW123",
            "USDC",
            "totally-different-global-name",
            "usd-coin",
        )])),
        response(&body("totally-different-global-name", "2.125", NOW + 31)),
    ])
    .await;
    for _ in 0..2 {
        assert_eq!(
            provider
                .quote(&asset("binance", "NEW123", 36))
                .await
                .unwrap_err()
                .code,
            "PRICE_IDENTITY_UNAVAILABLE"
        );
    }
    assert_eq!(server.count(), 2);
    clock.store(NOW + 31, Ordering::SeqCst);
    let quote = provider
        .quote(&asset("binance", "NEW123", 36))
        .await
        .unwrap();
    assert_eq!(quote.usd_per_unit, "2.125");
    assert!(quote.source.contains("totally-different-global-name"));
    assert_eq!(server.count(), 5);
}

#[tokio::test]
async fn identity_cache_and_price_cache_have_separate_expiry_and_no_outage_fallback() {
    let (server, provider, clock) = exchange_fixture(vec![
        response(&market_rows(&[("APT", "USDC")])),
        response(&page(vec![ticker("APT", "USDC", "aptos", "usd-coin")])),
        response(&body("aptos", "1.25", NOW)),
        response(&body("aptos", "1.3", NOW + 31)),
        response(&market_rows(&[("APT", "USDC")])),
        (http(503, "SYNTHETIC-PRIVATE-FAILURE"), Duration::ZERO),
    ])
    .await;
    let selected = asset("binance", "APT", 36);
    provider.quote(&selected).await.unwrap();
    provider.quote(&selected).await.unwrap();
    assert_eq!(server.count(), 3);
    clock.store(NOW + 31, Ordering::SeqCst);
    assert_eq!(provider.quote(&selected).await.unwrap().usd_per_unit, "1.3");
    assert_eq!(server.count(), 4);
    clock.store(NOW + 901, Ordering::SeqCst);
    assert_eq!(
        provider.quote(&selected).await.unwrap_err().code,
        "PRICE_UNAVAILABLE"
    );
    assert_eq!(server.count(), 6);
}

#[tokio::test]
async fn metadata_retry_after_prevents_hammering_then_refreshes_automatically() {
    let rate =
        b"HTTP/1.1 429 Fixture\r\nRetry-After: 2\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_vec();
    let (server, provider, clock) = exchange_fixture(vec![
        response(&market_rows(&[("APT", "USDC")])),
        (rate, Duration::ZERO),
        response(&page(vec![ticker("APT", "USDC", "aptos", "usd-coin")])),
        response(&body("aptos", "1.5", NOW + 2)),
    ])
    .await;
    let selected = asset("binance", "APT", 36);
    for _ in 0..2 {
        assert_eq!(
            provider.quote(&selected).await.unwrap_err().code,
            "PRICE_RATE_LIMITED"
        );
    }
    assert_eq!(server.count(), 2);
    clock.store(NOW + 2, Ordering::SeqCst);
    assert_eq!(provider.quote(&selected).await.unwrap().usd_per_unit, "1.5");
    assert_eq!(server.count(), 4);
}

#[tokio::test]
async fn verified_exchange_identity_cannot_make_a_stale_or_wrong_coin_quote_valid() {
    for (quote, code) in [
        (body("aptos", "1", NOW - 120), "PRICE_STALE"),
        (body("another-apt", "1", NOW), "PRICE_UNAVAILABLE"),
    ] {
        let (server, provider, _) = exchange_fixture(vec![
            response(&market_rows(&[("APT", "USDC")])),
            response(&page(vec![ticker("APT", "USDC", "aptos", "usd-coin")])),
            response(&quote),
        ])
        .await;
        assert_eq!(
            provider
                .quote(&asset("binance", "APT", 36))
                .await
                .unwrap_err()
                .code,
            code
        );
        assert_eq!(server.count(), 3);
    }
}

#[tokio::test]
async fn provider_keys_are_never_attached_to_public_venue_metadata() {
    let (server, mut provider, _) = exchange_fixture(vec![
        response(&market_rows(&[("APT", "USDC")])),
        response(&page(vec![ticker("APT", "USDC", "aptos", "usd-coin")])),
        response(&body("aptos", "1.1", NOW)),
    ])
    .await;
    let mut key = HeaderValue::from_static("SYNTHETIC-ONLY-MARKET-DATA-KEY");
    key.set_sensitive(true);
    provider.authentication = Some((ApiPlan::Demo, key));
    provider.quote(&asset("binance", "APT", 36)).await.unwrap();
    let requests = server.requests.lock().unwrap();
    assert!(!requests[0].to_ascii_lowercase().contains("x-cg-"));
    for request in &requests[1..] {
        assert!(request.to_ascii_lowercase().contains("x-cg-demo-api-key:"));
        assert!(!request.lines().next().unwrap().contains("SYNTHETIC"));
    }
}

#[tokio::test]
async fn exact_evm_contract_and_network_remain_independent_of_exchange_labels() {
    let contract = format!("0x{}", "ab".repeat(20));
    let other = "0x2222222222222222222222222222222222222222";
    let (server, provider, _) = fixture(vec![
        response(&body(&contract, "1.02", NOW)),
        response(&body(&contract, "0.9", NOW)),
        response(&body(other, "7", NOW)),
    ])
    .await;
    let eth = asset(
        "ethereum:1",
        &contract.to_uppercase().replacen("0X", "0x", 1),
        6,
    );
    assert_eq!(provider.quote(&eth).await.unwrap().usd_per_unit, "1.02");
    assert_eq!(
        provider
            .quote(&asset("ethereum:42161", &contract, 6))
            .await
            .unwrap()
            .usd_per_unit,
        "0.9"
    );
    assert_eq!(
        provider
            .quote(&asset("ethereum:1", other, 6))
            .await
            .unwrap()
            .usd_per_unit,
        "7"
    );
    let requests = server.requests.lock().unwrap();
    assert!(requests[0].contains("/simple/token_price/ethereum?contract_addresses="));
    assert!(requests[1].contains("/simple/token_price/arbitrum-one?contract_addresses="));
}

#[tokio::test]
async fn rate_limited_later_page_resumes_without_accepting_partial_mapping() {
    let rate =
        b"HTTP/1.1 429 Fixture\r\nRetry-After: 2\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_vec();
    let (server, provider, clock) = exchange_fixture(vec![
        response(&market_rows(&[("APT", "USDC")])),
        response(&page(full_page())),
        (rate, Duration::ZERO),
        response(&page(vec![])),
        response(&body("aptos", "1.5", NOW + 2)),
    ])
    .await;
    let selected = asset("binance", "APT", 36);
    assert_eq!(
        provider.quote(&selected).await.unwrap_err().code,
        "PRICE_RATE_LIMITED"
    );
    assert!(provider.cache.lock().await.is_empty());
    assert_eq!(server.count(), 3);
    clock.store(NOW + 2, Ordering::SeqCst);
    assert_eq!(provider.quote(&selected).await.unwrap().usd_per_unit, "1.5");
    let requests = server.requests.lock().unwrap();
    assert!(requests[2].contains("page=2"));
    assert!(requests[3].contains("page=2"));
    assert_eq!(
        requests.iter().filter(|row| row.contains("page=1")).count(),
        1
    );
}

struct OtherVenueMarkets;
#[async_trait]
impl ExchangeMarketSource for OtherVenueMarkets {
    async fn markets(&self, _: &str) -> Result<Vec<ExchangeMarket>> {
        Ok(vec![ExchangeMarket {
            market_id: "APT-USD".into(),
            base_asset: "APT".into(),
            quote_asset: "USD".into(),
            active: true,
        }])
    }
}
#[tokio::test]
async fn exchange_local_identifiers_do_not_share_identity_mappings_between_venues() {
    let mut first = ticker("APT", "USD", "other-apt", "fiat-usd");
    first["market"]["identifier"] = serde_json::json!("other-provider-venue");
    let (server, provider, _) = exchange_fixture(vec![
        response(&market_rows(&[("APT", "USDC")])),
        response(&page(vec![ticker("APT", "USDC", "aptos", "usd-coin")])),
        response(&body("aptos", "1.1", NOW)),
        response(&page(vec![first])),
        response(&body("other-apt", "9.5", NOW)),
    ])
    .await;
    let provider = provider
        .with_exchange_market_source(
            "other-venue",
            "other-provider-venue",
            Arc::new(OtherVenueMarkets),
        )
        .unwrap();
    assert_eq!(
        provider
            .quote(&asset("binance", "APT", 36))
            .await
            .unwrap()
            .usd_per_unit,
        "1.1"
    );
    let other = provider
        .quote(&asset("exchange:other-venue", "APT", 36))
        .await
        .unwrap();
    assert_eq!(other.usd_per_unit, "9.5");
    assert!(other.source.contains("exchange=other-venue"));
    assert_eq!(server.count(), 5);
}

#[tokio::test]
async fn wrong_provider_exchange_or_duplicate_venue_markets_never_qualify() {
    let mut wrong = ticker("APT", "USDC", "aptos", "usd-coin");
    wrong["market"]["identifier"] = serde_json::json!("binance-us");
    let (_server, provider, _) = exchange_fixture(vec![
        response(&market_rows(&[("APT", "USDC")])),
        response(&page(vec![wrong])),
    ])
    .await;
    assert_eq!(
        provider
            .quote(&asset("binance", "APT", 36))
            .await
            .unwrap_err()
            .code,
        "PRICE_IDENTITY_DATA_INVALID"
    );
    let (_server, provider, _) = exchange_fixture(vec![response(&market_rows(&[
        ("APT", "USDC"),
        ("APT", "USDC"),
    ]))])
    .await;
    assert_eq!(
        provider
            .quote(&asset("binance", "APT", 36))
            .await
            .unwrap_err()
            .code,
        "PRICE_IDENTITY_DATA_INVALID"
    );
}

#[tokio::test]
async fn overlapping_identical_rows_are_deduplicated_without_ignoring_later_identities() {
    let (server, provider, _) = exchange_fixture(vec![
        response(&market_rows(&[("APT", "USDC"), ("SEI", "USDC")])),
        response(&page(full_page())),
        response(&page(vec![
            ticker("APT", "USDC", "aptos", "usd-coin"),
            ticker("SEI", "USDC", "sei-network", "usd-coin"),
        ])),
        response(&body("sei-network", "0.06", NOW)),
    ])
    .await;
    assert_eq!(
        provider
            .quote(&asset("binance", "SEI", 36))
            .await
            .unwrap()
            .usd_per_unit,
        "0.06"
    );
    assert_eq!(server.count(), 4);
}

#[tokio::test]
async fn venue_outage_and_retry_after_do_not_fall_back_to_symbol_prices() {
    for (status, code) in [
        (503, "PRICE_VENUE_UNAVAILABLE"),
        (429, "PRICE_VENUE_RATE_LIMITED"),
    ] {
        let (server, provider, _) = exchange_fixture(vec![(
            http(status, "SYNTHETIC-PRIVATE-ERROR"),
            Duration::ZERO,
        )])
        .await;
        assert_eq!(
            provider
                .quote(&asset("binance", "APT", 36))
                .await
                .unwrap_err()
                .code,
            code
        );
        if status == 429 {
            assert_eq!(
                provider
                    .quote(&asset("binance", "APT", 36))
                    .await
                    .unwrap_err()
                    .code,
                code
            );
            assert_eq!(server.count(), 1);
        }
    }
}
