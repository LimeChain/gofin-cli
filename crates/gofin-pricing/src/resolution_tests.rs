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
    clock.store(NOW + 86_401, Ordering::SeqCst);
    assert_eq!(
        provider.quote(&selected).await.unwrap_err().code,
        "PRICE_UNAVAILABLE"
    );
    assert_eq!(server.count(), 6);
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

#[derive(Default)]
struct MemoryIdentityStore(StdMutex<Option<Vec<u8>>>);
impl IdentityStore for MemoryIdentityStore {
    fn load(&self) -> Result<Option<Vec<u8>>> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn save(&self, bytes: &[u8]) -> Result<()> {
        *self.0.lock().unwrap() = Some(bytes.to_vec());
        Ok(())
    }
}


#[tokio::test]
async fn missing_stale_or_invalid_batch_member_cannot_cache_a_partial_batch() {
    for bad in ["missing", "stale", "negative"] {
        let first = body("ethereum", "2", NOW);
        let second = match bad {
            "missing" => "{}".into(),
            "stale" => body(CONTRACT, "1", NOW - 120),
            _ => body(CONTRACT, "-1", NOW),
        };
        // Two exact contracts on one platform are batched together.
        let other = "0x2222222222222222222222222222222222222222";
        let payload = format!(
            "{{\"{other}\":{{\"usd\":2,\"last_updated_at\":{NOW}}},{} }}",
            &second[1..second.len() - 1]
        );
        let payload = if bad == "missing" {
            body(other, "2", NOW)
        } else {
            payload
        };
        let (server, mut provider, _) = fixture(vec![
            response(&payload),
            response(&body(other, "3", NOW)),
            response(&first),
        ])
        .await;
        provider.authentication = Some((ApiPlan::Demo, HeaderValue::from_static("SYNTHETIC-DEMO-KEY")));
        assert_eq!(
            provider
                .quotes(&[
                    asset("ethereum:1", other, 18),
                    asset("ethereum:1", CONTRACT, 18)
                ])
                .await
                .unwrap_err().code,
            match bad {
                "missing" => "PRICE_UNAVAILABLE",
                "stale" => "PRICE_STALE",
                _ => "PRICE_DATA_INVALID",
            }
        );
        assert_eq!(
            provider
                .quote(&asset("ethereum:1", other, 18))
                .await
                .unwrap()
                .usd_per_unit,
            "3"
        );
        assert_eq!(server.count(), 2);
    }
}

#[tokio::test]
async fn batching_keeps_contracts_and_platforms_separate_and_bounds_action_size() {
    let other = "0x2222222222222222222222222222222222222222";
    let batch = format!(
        "{{\"{CONTRACT}\":{{\"usd\":1,\"last_updated_at\":{NOW}}},\"{other}\":{{\"usd\":2,\"last_updated_at\":{NOW}}}}}"
    );
    let (server, mut provider, _) = fixture(vec![
        response(&body("ethereum", "2000", NOW)),
        response(&body(CONTRACT, "3", NOW)),
        response(&batch),
    ])
    .await;
    provider.authentication = Some((ApiPlan::Demo, HeaderValue::from_static("SYNTHETIC-DEMO-KEY")));
    let assets = [
        native(),
        asset("ethereum:1", CONTRACT, 18),
        asset("ethereum:1", other, 18),
        asset("ethereum:42161", CONTRACT, 18),
    ];
    let quotes = provider.quotes(&assets).await.unwrap();
    assert_eq!(
        quotes
            .iter()
            .map(|q| q.usd_per_unit.as_str())
            .collect::<Vec<_>>(),
        ["2000", "1", "2", "3"]
    );
    assert_eq!(server.count(), 3);
    {
        let requests = server.requests.lock().unwrap();
        assert!(requests[1].contains("/simple/token_price/arbitrum-one?"));
        assert!(requests[2].contains("/simple/token_price/ethereum?contract_addresses="));
    }
    assert_eq!(
        provider.quotes(&vec![native(); 65]).await.unwrap_err().code,
        "PRICE_REQUEST_TOO_LARGE"
    );
    assert_eq!(server.count(), 3);
}


#[tokio::test]
async fn saved_pagination_resumes_after_recreation_and_cooldown_without_accepting_partial_identity()
{
    let store = Arc::new(MemoryIdentityStore::default());
    let rows = (0..100)
        .map(|_| ticker("APT", "USDC", "aptos", "usd-coin"))
        .collect();
    let rate =
        b"HTTP/1.1 429 Fixture\r\nRetry-After: 2\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_vec();
    let (_first_server, provider, _) = exchange_fixture(vec![
        response(&market_rows(&[("APT", "USDC")])),
        response(&page(rows)),
        (rate, Duration::ZERO),
    ])
    .await;
    let provider = provider.with_identity_store(store.clone()).unwrap();
    assert_eq!(
        provider
            .quote(&asset("binance", "APT", 36))
            .await
            .unwrap_err()
            .code,
        "PRICE_RATE_LIMITED"
    );
    let (server, provider, clock) = exchange_fixture(vec![
        response(&page(vec![])),
        response(&body("aptos", "2", NOW + 2)),
    ])
    .await;
    clock.store(NOW + 2, Ordering::SeqCst);
    let provider = provider.with_identity_store(store).unwrap();
    assert_eq!(
        provider
            .quote(&asset("binance", "APT", 36))
            .await
            .unwrap()
            .usd_per_unit,
        "2"
    );
    assert_eq!(server.count(), 2);
    assert!(
        server.requests.lock().unwrap()[0]
            .contains("/exchanges/binance/tickers?order=base_target&page=2")
    );
}

#[tokio::test]
async fn corrupt_or_different_provider_state_is_refreshed_instead_of_trusted() {
    for bytes in [b"invalid cache".to_vec(), br#"{"version":999,"origin":"https://attacker.invalid","exchanges":{},"cache":{"exchanges":{},"pending":{}}}"#.to_vec()] {
        let store = Arc::new(MemoryIdentityStore(StdMutex::new(Some(bytes))));
        let (server, provider, _) = exchange_fixture(vec![response(&market_rows(&[("APT", "USDC")])), response(&page(vec![ticker("APT", "USDC", "aptos", "usd-coin")])), response(&body("aptos", "2", NOW))]).await;
        let provider = provider.with_identity_store(store).unwrap();
        assert_eq!(provider.quote(&asset("binance", "APT", 36)).await.unwrap().usd_per_unit, "2");
        assert_eq!(server.count(), 3);
    }
}

#[tokio::test]
async fn request_pacing_is_shared_and_cancelling_a_queue_does_not_spend_an_extra_slot() {
    let (server, mut provider, _) = fixture(vec![
        response(&body("ethereum", "2", NOW)),
        response(&body(CONTRACT, "3", NOW)),
    ])
    .await;
    provider.test_pacing = Some(Duration::from_millis(50));
    provider.quote(&native()).await.unwrap();
    let reserved = *provider.next_request.lock().await;
    let provider = Arc::new(provider);
    let pending = {
        let provider = provider.clone();
        tokio::spawn(async move { provider.quote(&asset("ethereum:1", CONTRACT, 18)).await })
    };
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(server.count(), 1);
    pending.abort();
    let _ = pending.await;
    assert_eq!(*provider.next_request.lock().await, reserved);
    assert_eq!(
        provider
            .quote(&asset("ethereum:1", CONTRACT, 18))
            .await
            .unwrap()
            .usd_per_unit,
        "3"
    );
    assert_eq!(server.count(), 2);
}


#[tokio::test]
async fn persisted_pacing_recovers_after_quiet_without_shortening_active_cooldown() {
    for learned in [48, 60] {
        let store = Arc::new(MemoryIdentityStore::default());
        let (_, provider, clock) = fixture(vec![]).await;
        let provider = provider.with_identity_store(store.clone()).unwrap();
        provider.pacing_seconds.store(learned, Ordering::Relaxed);
        *provider.rate_limited_until.lock().await = Some(at(NOW + 10));
        provider
            .save_identities(&*provider.identities.lock().await)
            .unwrap();
        let (_, reloaded, _) = fixture(vec![]).await;
        let mut reloaded = reloaded.with_identity_store(store.clone()).unwrap();
        reloaded.test_clock = Some(clock.clone());
        assert_eq!(reloaded.pacing_seconds.load(Ordering::Relaxed), learned);
        for successful in [false, true] {
            reloaded.recover_pacing(successful).await.unwrap();
            assert_eq!(reloaded.pacing_seconds.load(Ordering::Relaxed), learned);
            assert_eq!(reloaded.check_rate_limit().await.unwrap_err().code, "PRICE_RATE_LIMITED");
        }
        clock.store(NOW + 69, Ordering::SeqCst);
        reloaded.recover_pacing(false).await.unwrap();
        assert_eq!(reloaded.pacing_seconds.load(Ordering::Relaxed), learned);
        clock.store(NOW + 70, Ordering::SeqCst);
        let reserved = tokio::time::Instant::now() + Duration::from_secs(50);
        *reloaded.next_request.lock().await = Some(reserved);
        reloaded.recover_pacing(false).await.unwrap();
        assert_eq!(reloaded.pacing_seconds.load(Ordering::Relaxed), 6);
        assert_eq!(*reloaded.next_request.lock().await, Some(reserved));
        assert_eq!(*reloaded.rate_limited_until.lock().await, Some(at(NOW + 10)));
        let (_, restarted, _) = fixture(vec![]).await;
        let restarted = restarted.with_identity_store(store).unwrap();
        assert_eq!(restarted.pacing_seconds.load(Ordering::Relaxed), 6);
        assert_eq!(*restarted.rate_limited_until.lock().await, Some(at(NOW + 10)));
    }
}

#[tokio::test]
async fn contended_cooldown_snapshot_never_erases_persisted_retry_after() {
    let store = Arc::new(MemoryIdentityStore::default());
    let (_, provider, _) = fixture(vec![]).await;
    let provider = provider.with_identity_store(store.clone()).unwrap();
    *provider.rate_limited_until.lock().await = Some(at(NOW + 60));
    let cache = provider.identities.lock().await;
    provider.save_identities(&cache).unwrap();
    let previous = store.load().unwrap();
    let _busy = provider.rate_limited_until.lock().await;
    provider.save_identities(&cache).unwrap();
    assert_eq!(store.load().unwrap(), previous);
    let (_, reloaded, _) = fixture(vec![]).await;
    let reloaded = reloaded.with_identity_store(store).unwrap();
    assert_eq!(reloaded.check_rate_limit().await.unwrap_err().code, "PRICE_RATE_LIMITED");
}



#[tokio::test]
async fn repeated_rate_limits_still_raise_spacing_and_preserve_retry_after() {
    let rate = b"HTTP/1.1 429 Fixture\r\nRetry-After: 10\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
    let (server, provider, clock) = fixture(vec![(rate.clone(), Duration::ZERO); 5]).await;
    for (index, spacing) in [12, 24, 48, 60, 60].into_iter().enumerate() {
        let current = NOW + (index as i64 * 10);
        clock.store(current, Ordering::SeqCst);
        assert_eq!(provider.bounded_request("/fixture").await.unwrap_err().code, "PRICE_RATE_LIMITED");
        assert_eq!(provider.pacing_seconds.load(Ordering::Relaxed), spacing);
        assert_eq!(*provider.rate_limited_until.lock().await, Some(at(current + 10)));
        provider.recover_pacing(true).await.unwrap();
        assert_eq!(provider.pacing_seconds.load(Ordering::Relaxed), spacing);
    }
    assert_eq!(server.count(), 5);
}

#[tokio::test]
async fn late_shorter_rate_limit_preserves_longer_active_deadline_and_restart() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut clients = JoinSet::new();
        let mut first = Some((started_tx, release_rx));
        for index in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let signals = first.take();
            clients.spawn(async move {
                let mut request = Vec::new();
                let mut chunk = [0; 1024];
                while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&chunk[..count]);
                }
                if let Some((started, release)) = signals {
                    started.send(()).unwrap();
                    release.await.unwrap();
                }
                let seconds = if index == 0 { 10 } else { 60 };
                let response = format!("HTTP/1.1 429 Fixture\r\nRetry-After: {seconds}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                socket.write_all(response.as_bytes()).await.unwrap();
            });
        }
        while let Some(result) = clients.join_next().await {
            result.unwrap();
        }
    });
    let store = Arc::new(MemoryIdentityStore::default());
    let (_, mut provider, _) = fixture(vec![]).await;
    provider.test_base_url = Some(base);
    let provider = Arc::new(provider.with_identity_store(store.clone()).unwrap());
    let late = {
        let provider = provider.clone();
        tokio::spawn(async move { provider.bounded_request("/late").await })
    };
    started_rx.await.unwrap();
    assert_eq!(provider.bounded_request("/first").await.unwrap_err().code, "PRICE_RATE_LIMITED");
    assert_eq!(*provider.rate_limited_until.lock().await, Some(at(NOW + 60)));
    release_tx.send(()).unwrap();
    assert_eq!(late.await.unwrap().unwrap_err().code, "PRICE_RATE_LIMITED");
    assert_eq!(*provider.rate_limited_until.lock().await, Some(at(NOW + 60)));
    assert_eq!(provider.pacing_seconds.load(Ordering::Relaxed), 24);
    let (_, reloaded, _) = fixture(vec![]).await;
    let reloaded = reloaded.with_identity_store(store).unwrap();
    assert_eq!(*reloaded.rate_limited_until.lock().await, Some(at(NOW + 60)));
    assert_eq!(reloaded.check_rate_limit().await.unwrap_err().code, "PRICE_RATE_LIMITED");
    server.await.unwrap();
}


#[tokio::test]
async fn anonymous_token_quotes_use_single_contract_requests_while_authenticated_plans_batch() {
    let other = "0x2222222222222222222222222222222222222222";
    let first = body(CONTRACT, "1.000000000000000001", NOW);
    let second = body(other, "2305.0373531630667", NOW);
    for plan in [None, Some(ApiPlan::Demo), Some(ApiPlan::Pro)] {
        let responses = if plan.is_none() {
            vec![response(&first), response(&second)]
        } else {
            let batch = format!(
                "{{{},{} }}",
                &first[1..first.len() - 1],
                &second[1..second.len() - 1]
            );
            vec![response(&batch)]
        };
        let (server, mut provider, _) = fixture(responses).await;
        provider.authentication =
            plan.map(|plan| (plan, HeaderValue::from_static("SYNTHETIC-PLAN-KEY")));
        // Results retain caller order and repeated identities share one lookup.
        let assets = [
            asset("ethereum:1", other, 18),
            asset("ethereum:1", CONTRACT, 6),
            asset("ethereum:1", other, 18),
        ];
        let quotes = provider.quotes(&assets).await.unwrap();
        assert_eq!(
            quotes
                .iter()
                .map(|q| q.usd_per_unit.as_str())
                .collect::<Vec<_>>(),
            [
                "2305.0373531630667",
                "1.000000000000000001",
                "2305.0373531630667"
            ]
        );
        provider.quotes(&assets).await.unwrap();
        assert_eq!(server.count(), if plan.is_none() { 2 } else { 1 });
        let requests = server.requests.lock().unwrap();
        for request in requests.iter() {
            let url = reqwest::Url::parse(&format!(
                "https://fixture.invalid{}",
                request.split_whitespace().nth(1).unwrap()
            ))
            .unwrap();
            assert_eq!(url.path(), "/simple/token_price/ethereum");
            let contracts = url
                .query_pairs()
                .find(|(key, _)| key == "contract_addresses")
                .unwrap()
                .1;
            assert_eq!(
                contracts.split(',').count(),
                if plan.is_none() { 1 } else { 2 }
            );
            assert!(!url.as_str().contains("SYNTHETIC"));
        }
    }
}
