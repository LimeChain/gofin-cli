use super::*;
use std::sync::{
    Mutex as StdMutex,
    atomic::{AtomicI64, AtomicUsize, Ordering},
};

const NOW: i64 = 1_800_000_000;

#[tokio::test]
async fn coinbase_fiat_usd_is_distinct_from_usdc_and_never_needs_stablecoin_parity() {
    for exchange in ["coinbase", "coinbaseexchange"] {
        let (prices, venue, fx, _, chain) = fixture(vec![market("BTC-USD", "BTC", "USD", true)]);
        *fx.fail.lock().unwrap() = true;
        let prices = prices
            .with_exchange_source(exchange, venue.clone())
            .unwrap();
        let quotes = prices
            .quotes(&[
                AssetId {
                    network: exchange.into(),
                    reference: "BTC".into(),
                    decimals: 8,
                },
                AssetId {
                    network: exchange.into(),
                    reference: "USD".into(),
                    decimals: 2,
                },
            ])
            .await
            .unwrap();
        assert_eq!(quotes[0].usd_per_unit, "10");
        assert_eq!(quotes[1].usd_per_unit, "1");
        assert!(quotes[0].source.contains("venue_market=BTC-USD"));
        assert!(quotes[0].source.contains("quote_asset=USD"));
        assert_eq!(fx.calls.load(Ordering::SeqCst), 0);
        assert!(chain.0.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn coinbase_usdc_retains_observed_depeg_and_quote_freshness() {
    let (prices, venue, fx, clock, _) = fixture(vec![market("BTC-USDC", "BTC", "USDC", true)]);
    let prices = prices.with_exchange_source("coinbase", venue).unwrap();
    let coinbase = |reference: &str| AssetId {
        network: "coinbase".into(),
        reference: reference.into(),
        decimals: 8,
    };
    let quotes = prices
        .quotes(&[coinbase("BTC"), coinbase("USDC")])
        .await
        .unwrap();
    assert_eq!(quotes[0].usd_per_unit, "9.9");
    assert_eq!(quotes[1].usd_per_unit, "0.99");
    assert_eq!(fx.calls.load(Ordering::SeqCst), 1);
    clock.fetch_add(1000, Ordering::SeqCst);
    *fx.time.lock().unwrap() = Some(NOW);
    assert!(prices.quote(&coinbase("USDC")).await.is_err());
}

#[tokio::test]
async fn international_spot_uses_its_own_namespace_and_observed_usdc_conversion() {
    let (prices, venue, _, _, chain) = fixture(vec![market("BTC-USDC", "BTC", "USDC", true)]);
    let prices = prices
        .with_exchange_source("coinbaseinternational", venue)
        .unwrap();
    let quote = prices
        .quote(&AssetId {
            network: "coinbaseinternational".into(),
            reference: "BTC".into(),
            decimals: 36,
        })
        .await
        .unwrap();
    assert_eq!(quote.usd_per_unit, "9.9");
    assert!(quote.source.contains("exchange=coinbaseinternational"));
    assert!(fin_domain::price_freshness::is_fresh(
        &quote.updated_at,
        &quote.source,
        DateTime::from_timestamp(NOW, 0).unwrap()
    ));
    assert!(chain.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn another_venue_cannot_adopt_coinbase_fiat_or_kraken_quote_units() {
    for currency in ["USD", "ZUSD"] {
        let (prices, _, _, _, _) = fixture(vec![market("BTC-OTHER", "BTC", currency, true)]);
        assert_eq!(
            prices.quote(&asset("BTC")).await.unwrap_err().code,
            "PRICE_SPOT_PAIR_UNSUPPORTED"
        );
    }
}
fn at(seconds: i64) -> String {
    DateTime::from_timestamp(seconds, 0).unwrap().to_rfc3339()
}
fn asset(coin: &str) -> AssetId {
    AssetId {
        network: "binance".into(),
        reference: coin.into(),
        decimals: 36,
    }
}
fn market(id: &str, base: &str, quote: &str, active: bool) -> ExchangeMarket {
    ExchangeMarket {
        market_id: id.into(),
        base_asset: base.into(),
        quote_asset: quote.into(),
        active,
    }
}
struct Venue {
    clock: Arc<AtomicI64>,
    markets: StdMutex<Vec<ExchangeMarket>>,
    books: StdMutex<BTreeMap<String, SpotBook>>,
    market_calls: AtomicUsize,
    book_calls: StdMutex<Vec<Vec<String>>>,
}
#[async_trait]
impl SpotMarketSource for Venue {
    async fn markets(&self, _: &str) -> Result<Vec<ExchangeMarket>> {
        self.market_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.markets.lock().unwrap().clone())
    }
    async fn books(&self, _: &str, markets: &[String]) -> Result<Vec<SpotBook>> {
        self.book_calls.lock().unwrap().push(markets.to_vec());
        Ok(markets
            .iter()
            .filter_map(|market| self.books.lock().unwrap().get(market).cloned())
            .map(|mut book| {
                if book.observed_at.is_empty() {
                    book.observed_at = at(self.clock.load(Ordering::SeqCst));
                }
                book
            })
            .collect())
    }
}
struct Fx {
    clock: Arc<AtomicI64>,
    calls: AtomicUsize,
    usdc: StdMutex<String>,
    usdt: StdMutex<String>,
    time: StdMutex<Option<i64>>,
    fail: StdMutex<bool>,
}
#[async_trait]
impl StablecoinUsdSource for Fx {
    async fn stablecoin_rates(&self) -> Result<StablecoinRates> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if *self.fail.lock().unwrap() {
            return Err(spot_error(
                "PRICE_RATE_LIMITED",
                "Synthetic stablecoin batch unavailable.",
            ));
        }
        let observed = at(self.clock.load(Ordering::SeqCst));
        let updated = at(self
            .time
            .lock()
            .unwrap()
            .unwrap_or(self.clock.load(Ordering::SeqCst)));
        let quote = |price: String, id: &str| UsdQuote {
            usd_per_unit: price,
            updated_at: updated.clone(),
            observed_at: observed.clone(),
            source: format!("synthetic conversion; coin_id={id}"),
        };
        Ok(StablecoinRates {
            usdc: quote(self.usdc.lock().unwrap().clone(), "usd-coin"),
            usdt: quote(self.usdt.lock().unwrap().clone(), "tether"),
        })
    }
}
struct Chain(StdMutex<Vec<AssetId>>);
#[async_trait]
impl UsdPriceSource for Chain {
    async fn quote(&self, asset: &AssetId) -> Result<UsdQuote> {
        assert!(!asset.network.starts_with("binance"));
        self.0.lock().unwrap().push(asset.clone());
        Ok(UsdQuote {
            usd_per_unit: "SYNTHETIC-unchanged-chain-result".into(),
            updated_at: at(NOW),
            observed_at: at(NOW),
            source: asset.reference.clone(),
        })
    }
}
fn fixture(
    rows: Vec<ExchangeMarket>,
) -> (
    ExchangeSpotPrices,
    Arc<Venue>,
    Arc<Fx>,
    Arc<AtomicI64>,
    Arc<Chain>,
) {
    let clock = Arc::new(AtomicI64::new(NOW));
    let books = rows
        .iter()
        .map(|row| {
            (
                row.market_id.clone(),
                SpotBook {
                    market_id: row.market_id.clone(),
                    bid: "9".into(),
                    ask: "10".into(),
                    bid_quantity: "100".into(),
                    ask_quantity: "100".into(),
                    observed_at: String::new(),
                },
            )
        })
        .collect();
    let venue = Arc::new(Venue {
        clock: clock.clone(),
        markets: StdMutex::new(rows),
        books: StdMutex::new(books),
        market_calls: AtomicUsize::new(0),
        book_calls: StdMutex::new(vec![]),
    });
    let fx = Arc::new(Fx {
        clock: clock.clone(),
        calls: AtomicUsize::new(0),
        usdc: StdMutex::new("0.99".into()),
        usdt: StdMutex::new("1.02".into()),
        time: StdMutex::new(None),
        fail: StdMutex::new(false),
    });
    let chain = Arc::new(Chain(StdMutex::new(vec![])));
    let mut prices = ExchangeSpotPrices::with_sources(chain.clone(), fx.clone(), venue.clone());
    prices.clock = Some(clock.clone());
    (prices, venue, fx, clock, chain)
}

#[tokio::test]
async fn exact_conversion_depegs_stable_holdings_and_fee_assets_share_one_batch() {
    let (prices, venue, fx, _, chain) = fixture(vec![
        market("APTUSDC", "APT", "USDC", true),
        market("SEIUSDT", "SEI", "USDT", true),
        market("BNBUSDC", "BNB", "USDC", true),
    ]);
    let quotes = prices
        .quotes(&[
            asset("APT"),
            asset("SEI"),
            asset("BNB"),
            asset("USDC"),
            asset("USDT"),
            asset("APT"),
        ])
        .await
        .unwrap();
    assert_eq!(
        quotes
            .iter()
            .map(|quote| quote.usd_per_unit.as_str())
            .collect::<Vec<_>>(),
        ["9.9", "10.2", "9.9", "0.99", "1.02", "9.9"]
    );
    assert_eq!(fx.calls.load(Ordering::SeqCst), 1);
    assert_eq!(venue.market_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *venue.book_calls.lock().unwrap(),
        [vec!["APTUSDC", "BNBUSDC", "SEIUSDT"]]
    );
    assert!(chain.0.lock().unwrap().is_empty());
    assert!(quotes[0].source.contains("venue_market=APTUSDC"));
    assert!(quotes[0].source.contains("coin_id=usd-coin"));
    prices.quote(&asset("APT")).await.unwrap();
    assert_eq!(fx.calls.load(Ordering::SeqCst), 1);
    assert_eq!(venue.book_calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn deterministic_pair_choice_uses_metadata_not_constructed_symbols_or_liquidity() {
    let (prices, venue, _, _, _) = fixture(vec![
        market("APTUSDT", "APT", "USDT", true),
        market("EXACT-ALIAS", "APT", "USDC", true),
    ]);
    venue.books.lock().unwrap().get_mut("APTUSDT").unwrap().ask = "100".into();
    let quote = prices.quote(&asset("APT")).await.unwrap();
    assert_eq!(quote.usd_per_unit, "9.9");
    assert!(quote.source.contains("venue_market=EXACT-ALIAS"));
    let (prices, _, _, _, _) = fixture(vec![
        market("APTUSDC", "OTHER", "USDC", true),
        market("APTUSDT", "APT", "USDT", true),
    ]);
    assert_eq!(
        prices.quote(&asset("APT")).await.unwrap().usd_per_unit,
        "10.2"
    );
    let (prices, _, _, _, _) = fixture(vec![
        market("APTUSDC", "APT", "USDC", false),
        market("APTUSDT", "APT", "USDT", true),
    ]);
    assert_eq!(
        prices.quote(&asset("APT")).await.unwrap().usd_per_unit,
        "10.2"
    );
}

#[tokio::test]
async fn unsupported_ambiguous_inactive_inverse_and_wrong_case_pairs_fail_explicitly() {
    for rows in [
        vec![market("USDCAPT", "USDC", "APT", true)],
        vec![market("APTUSDC", "APT", "USDC", false)],
        vec![market("APTUSDG", "APT", "USDG", true)],
        vec![market("APTUSDC", "apt", "USDC", true)],
    ] {
        let (prices, _, fx, _, _) = fixture(rows);
        assert_eq!(
            prices.quote(&asset("APT")).await.unwrap_err().code,
            "PRICE_SPOT_PAIR_UNSUPPORTED"
        );
        assert_eq!(fx.calls.load(Ordering::SeqCst), 0);
    }
    let (prices, _, _, _, _) = fixture(vec![
        market("one", "APT", "USDC", true),
        market("two", "APT", "USDC", true),
    ]);
    assert_eq!(
        prices.quote(&asset("APT")).await.unwrap_err().code,
        "PRICE_SPOT_IDENTITY_CONFLICT"
    );
    let (prices, _, _, _, _) = fixture(vec![
        market("same", "APT", "USDC", true),
        market("same", "SEI", "USDT", true),
    ]);
    assert_eq!(
        prices.quote(&asset("APT")).await.unwrap_err().code,
        "PRICE_SPOT_IDENTITY_CONFLICT"
    );
}

#[tokio::test]
async fn missing_or_bad_preferred_book_never_silently_switches_to_usdt() {
    for bad in ["missing", "zero", "crossed", "stale", "future"] {
        let (prices, venue, _, _, _) = fixture(vec![
            market("APTUSDC", "APT", "USDC", true),
            market("APTUSDT", "APT", "USDT", true),
        ]);
        if bad == "missing" {
            venue.books.lock().unwrap().remove("APTUSDC");
        } else {
            let mut books = venue.books.lock().unwrap();
            let book = books.get_mut("APTUSDC").unwrap();
            match bad {
                "zero" => book.ask_quantity = "0".into(),
                "crossed" => book.bid = "11".into(),
                "stale" => book.observed_at = at(NOW - 30),
                _ => book.observed_at = at(NOW + 1),
            }
        }
        assert!(prices.quote(&asset("APT")).await.is_err(), "{bad}");
        assert_eq!(*venue.book_calls.lock().unwrap(), [vec!["APTUSDC"]]);
    }
}

#[tokio::test]
async fn cache_ages_refresh_independently_without_freshening_either_old_leg() {
    let (prices, venue, fx, clock, _) = fixture(vec![market("APTUSDC", "APT", "USDC", true)]);
    let first = prices.quote(&asset("APT")).await.unwrap();
    clock.store(NOW + 9, Ordering::SeqCst);
    let cached = prices.quote(&asset("APT")).await.unwrap();
    assert_eq!(first.observed_at, cached.observed_at);
    clock.store(NOW + 10, Ordering::SeqCst);
    let quote = prices.quote(&asset("APT")).await.unwrap();
    assert_eq!(venue.book_calls.lock().unwrap().len(), 2);
    assert_eq!(fx.calls.load(Ordering::SeqCst), 1);
    assert_eq!(quote.updated_at, at(NOW));
    assert_eq!(quote.observed_at, at(NOW + 10));
    clock.store(NOW + 30, Ordering::SeqCst);
    prices.quote(&asset("APT")).await.unwrap();
    assert_eq!(fx.calls.load(Ordering::SeqCst), 2);
    assert_eq!(venue.market_calls.load(Ordering::SeqCst), 2);
    *fx.fail.lock().unwrap() = true;
    clock.store(NOW + 60, Ordering::SeqCst);
    assert_eq!(
        prices.quote(&asset("APT")).await.unwrap_err().code,
        "PRICE_RATE_LIMITED"
    );
    *fx.fail.lock().unwrap() = false;
    *fx.time.lock().unwrap() = Some(NOW + 60 - 300);
    assert_eq!(
        prices.quote(&asset("APT")).await.unwrap_err().code,
        "PRICE_CONVERSION_STALE"
    );
}

#[tokio::test]
async fn either_conversion_member_missing_zero_or_stale_rejects_the_shared_batch() {
    for value in ["0", "", "NaN"] {
        let (prices, _, fx, _, _) = fixture(vec![market("APTUSDC", "APT", "USDC", true)]);
        *fx.usdt.lock().unwrap() = value.into();
        assert!(prices.quote(&asset("APT")).await.is_err());
        *fx.usdt.lock().unwrap() = "1.02".into();
        assert!(prices.quote(&asset("APT")).await.is_ok());
        assert_eq!(fx.calls.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn newly_listed_pairs_refresh_without_manual_enrollment_and_other_venues_fail() {
    let (prices, venue, _, clock, _) = fixture(vec![market("APTUSDC", "APT", "USDC", true)]);
    assert_eq!(
        prices.quote(&asset("NEW")).await.unwrap_err().code,
        "PRICE_SPOT_PAIR_UNSUPPORTED"
    );
    venue
        .markets
        .lock()
        .unwrap()
        .push(market("NEWUSDC", "NEW", "USDC", true));
    venue.books.lock().unwrap().insert(
        "NEWUSDC".into(),
        SpotBook {
            market_id: "NEWUSDC".into(),
            bid: "9".into(),
            ask: "10".into(),
            bid_quantity: "1".into(),
            ask_quantity: "1".into(),
            observed_at: String::new(),
        },
    );
    clock.store(NOW + 30, Ordering::SeqCst);
    assert_eq!(
        prices.quote(&asset("NEW")).await.unwrap().usd_per_unit,
        "9.9"
    );
    let mut unknown = asset("APT");
    unknown.network = "exchange:okx".into();
    assert_eq!(
        prices.quote(&unknown).await.unwrap_err().code,
        "PRICE_EXCHANGE_UNSUPPORTED"
    );
}

#[tokio::test]
async fn mixed_requests_preserve_exact_onchain_identity_order_and_behavior() {
    let (prices, _, fx, _, chain) = fixture(vec![market("APTUSDC", "APT", "USDC", true)]);
    let token = AssetId {
        network: "ethereum:42161".into(),
        reference: "0x1111111111111111111111111111111111111111".into(),
        decimals: 6,
    };
    let quotes = prices
        .quotes(&[token.clone(), asset("APT"), token.clone()])
        .await
        .unwrap();
    assert_eq!(quotes[0].usd_per_unit, "SYNTHETIC-unchanged-chain-result");
    assert_eq!(quotes[1].usd_per_unit, "9.9");
    assert_eq!(quotes[2].source, token.reference);
    assert_eq!(*chain.0.lock().unwrap(), [token.clone(), token]);
    assert_eq!(fx.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_assets_coalesce_conversion_and_venue_reads() {
    let (prices, venue, fx, _, _) = fixture(vec![market("APTUSDC", "APT", "USDC", true)]);
    let a = asset("APT");
    let b = a.clone();
    let (a, b) = tokio::join!(prices.quote(&a), prices.quote(&b));
    a.unwrap();
    b.unwrap();
    assert_eq!(fx.calls.load(Ordering::SeqCst), 1);
    assert_eq!(venue.book_calls.lock().unwrap().len(), 1);
}

#[test]
fn exact_product_preserves_large_values_and_rejects_unrepresentable_precision() {
    assert_eq!(
        multiply("9007199254740993.01", "0.99").unwrap(),
        "8917127262193583.0799"
    );
    assert_eq!(multiply("1e-8", "1.02").unwrap(), "0.0000000102");
    assert_eq!(
        multiply("1e-36", "0.99").unwrap_err().code,
        "PRICE_CONVERSION_PRECISION_UNSUPPORTED"
    );
}

#[tokio::test]
async fn coinbase_fiat_inverse_uses_bid_and_observed_stablecoin_conversion() {
    let (prices, venue, _, clock, _) = fixture(vec![market("USDC-EUR", "USDC", "EUR", true)]);
    {
        let mut books = venue.books.lock().unwrap();
        let book = books.get_mut("USDC-EUR").unwrap();
        book.bid = "0.8".into();
        book.ask = "0.9".into();
    }
    let prices = prices.with_exchange_source("coinbase", venue).unwrap();
    let quote = prices
        .quote(&AssetId {
            network: "coinbase".into(),
            reference: "EUR".into(),
            decimals: 36,
        })
        .await
        .unwrap();
    assert!(fin_domain::price_freshness::is_fresh(
        &quote.updated_at,
        &quote.source,
        DateTime::from_timestamp(NOW, 0).unwrap()
    ));
    assert_eq!(quote.usd_per_unit, "1.2375"); // 0.99 USD / 0.8 EUR, not parity or inverse ask.
    assert!(quote.source.contains("venue_market=USDC-EUR"));
    assert!(
        quote
            .source
            .contains("base_asset=USDC; quote_asset=EUR; conversion_asset=USDC")
    );
    clock.store(NOW + 10, Ordering::SeqCst);
    let refreshed = prices
        .quote(&AssetId {
            network: "coinbase".into(),
            reference: "EUR".into(),
            decimals: 36,
        })
        .await
        .unwrap();
    assert_eq!(
        refreshed.updated_at,
        at(NOW),
        "cached FX timestamp is not refreshed with the new book"
    );
    assert_eq!(refreshed.observed_at, at(NOW + 10));
    assert!(fin_domain::price_freshness::is_fresh(
        &refreshed.updated_at,
        &refreshed.source,
        DateTime::from_timestamp(NOW + 10, 0).unwrap()
    ));
}

#[test]
fn inverse_estimated_price_rounds_up_and_rejects_zero_bid() {
    let value = inverse_conversion("3", "1").unwrap();
    assert_eq!(value, "0.333333333333333333333333333333333334");
    assert!(inverse_conversion("0", "1").is_err());
}
