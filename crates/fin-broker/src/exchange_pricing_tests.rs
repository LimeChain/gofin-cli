//! Synthetic production-path spot/FX valuation through the broker and engine.
use super::*;
use async_trait::async_trait;
use fin_domain::{ApprovalAudit, AuthorizationChallenge, Authorizer, OperationRequest};
use fin_pricing::{
    ExchangeMarket, SpotBook, SpotMarketSource, StablecoinRates, StablecoinUsdSource,
};
use std::sync::{
    Mutex,
    atomic::{AtomicI64, AtomicUsize, Ordering},
};

struct VenuePrices {
    ask: Mutex<String>,
    old_snapshot: bool,
    apt_pair: bool,
}
#[async_trait]
impl SpotMarketSource for VenuePrices {
    async fn markets(&self, _: &str) -> fin_domain::Result<Vec<ExchangeMarket>> {
        let mut rows = vec![
            ExchangeMarket {
                market_id: "APTUSDC".into(),
                base_asset: "APT".into(),
                quote_asset: "USDC".into(),
                active: true,
            },
            ExchangeMarket {
                market_id: "BNBUSDT".into(),
                base_asset: "BNB".into(),
                quote_asset: "USDT".into(),
                active: true,
            },
        ];
        if !self.apt_pair {
            rows.remove(0);
            rows.push(ExchangeMarket {
                market_id: "ETHUSDC".into(),
                base_asset: "ETH".into(),
                quote_asset: "USDC".into(),
                active: true,
            });
        }
        Ok(rows)
    }
    async fn books(&self, _: &str, markets: &[String]) -> fin_domain::Result<Vec<SpotBook>> {
        Ok(markets
            .iter()
            .map(|id| SpotBook {
                market_id: id.clone(),
                bid: "9".into(),
                ask: if id == "APTUSDC" {
                    self.ask.lock().unwrap().clone()
                } else {
                    "20".into()
                },
                bid_quantity: "100".into(),
                ask_quantity: "100".into(),
                observed_at: (Utc::now()
                    - chrono::Duration::seconds(if self.old_snapshot { 11 } else { 0 }))
                .to_rfc3339(),
            })
            .collect())
    }
}
struct Rates {
    usdc: &'static str,
    calls: AtomicUsize,
    age_seconds: AtomicI64,
}
#[async_trait]
impl StablecoinUsdSource for Rates {
    async fn stablecoin_rates(&self) -> fin_domain::Result<StablecoinRates> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let observed = Utc::now();
        let quote = |value: &str, currency: &str| UsdQuote {
            usd_per_unit: value.into(),
            updated_at: (observed
                - chrono::Duration::seconds(self.age_seconds.load(Ordering::SeqCst)))
            .to_rfc3339(),
            observed_at: observed.to_rfc3339(),
            source: format!("synthetic stablecoin rate; coin_id={currency}"),
        };
        Ok(StablecoinRates {
            usdc: quote(self.usdc, "usd-coin"),
            usdt: quote("1.02", "tether"),
        })
    }
}
struct NoChain;
#[async_trait]
impl UsdPriceSource for NoChain {
    async fn quote(&self, _: &AssetId) -> fin_domain::Result<UsdQuote> {
        panic!("exchange valuation must not request a per-asset onchain/provider price")
    }
}
struct TradingReads {
    posts: AtomicUsize,
}
#[async_trait]
impl fin_binance::BinanceTransport for TradingReads {
    async fn request(
        &self,
        credential: &fin_binance::CredentialHandle,
        request: fin_binance::BinanceRequest,
    ) -> std::result::Result<Value, fin_binance::TransportError> {
        assert_eq!(credential.as_str(), "test-only:synthetic-spot-pricing");
        use fin_binance::BinanceEndpoint as E;
        match request.endpoint {
            E::ApiRestrictions => {
                Ok(json!({"enableReading":true,"enableSpotAndMarginTrading":true}))
            }
            E::Account => Ok(
                json!({"uid":1001,"accountType":"SPOT","canTrade":true,"balances":[{"asset":"APT","free":"100","locked":"0"},{"asset":"USDC","free":"1000","locked":"0"},{"asset":"BNB","free":"10","locked":"0"}]}),
            ),
            E::SpotExchangeInfo => Ok(
                json!({"symbols":[{"symbol":"APTUSDC","status":"TRADING","baseAsset":"APT","quoteAsset":"USDC","isSpotTradingAllowed":true,
                "filters":[{"filterType":"PRICE_FILTER","minPrice":"0.01","maxPrice":"1000","tickSize":"0.01"},{"filterType":"LOT_SIZE","minQty":"0.01","maxQty":"1000","stepSize":"0.01"},{"filterType":"MIN_NOTIONAL","minNotional":"5"}]}]}),
            ),
            E::SpotCommission => Ok(
                json!({"symbol":"APTUSDC","standardCommission":{"maker":"0.001","taker":"0.001","buyer":"0","seller":"0"},"taxCommission":{"maker":"0","taker":"0","buyer":"0","seller":"0"},"specialCommission":{"maker":"0","taker":"0","buyer":"0","seller":"0"},"discount":{"enabledForAccount":false,"enabledForSymbol":false,"discountAsset":"BNB","discount":"0.25"}}),
            ),
            E::SpotPlaceOrder => {
                self.posts.fetch_add(1, Ordering::SeqCst);
                Err(fin_binance::TransportError::Timeout)
            }
            _ => Err(fin_binance::TransportError::InvalidResponse),
        }
    }
}
struct Approval {
    calls: AtomicUsize,
    venue: Arc<VenuePrices>,
    change: Option<&'static str>,
}
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(
        &self,
        challenge: &AuthorizationChallenge,
    ) -> fin_domain::Result<ApprovalAudit> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(ask) = self.change {
            *self.venue.ask.lock().unwrap() = ask.into();
        } else {
            return Err(FinError::new(
                "AUTHORIZATION_REJECTED",
                "approval",
                "Synthetic approval declined.",
                "No real approval was requested.",
            ));
        }
        Ok(ApprovalAudit {
            mechanism: "synthetic-test-only".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}
fn fixture(
    usdc: &'static str,
    change: Option<&'static str>,
) -> (
    fin_engine::Engine,
    Arc<Rates>,
    Arc<Approval>,
    Arc<TradingReads>,
    Arc<SharedUsdValuator>,
) {
    fixture_options(usdc, change, true, false)
}
fn fixture_options(
    usdc: &'static str,
    change: Option<&'static str>,
    apt_pair: bool,
    allow_unpriced: bool,
) -> (
    fin_engine::Engine,
    Arc<Rates>,
    Arc<Approval>,
    Arc<TradingReads>,
    Arc<SharedUsdValuator>,
) {
    let venue = Arc::new(VenuePrices {
        ask: Mutex::new("10".into()),
        old_snapshot: change.is_some(),
        apt_pair,
    });
    let rates = Arc::new(Rates {
        usdc,
        calls: AtomicUsize::new(0),
        age_seconds: AtomicI64::new(0),
    });
    let prices = Arc::new(SharedUsdValuator::with_source(Arc::new(
        ExchangeSpotPrices::with_sources(Arc::new(NoChain), rates.clone(), venue.clone()),
    )));
    let transport = Arc::new(TradingReads {
        posts: AtomicUsize::new(0),
    });
    let config = fin_binance::BinanceConfig {
        accounts: vec![fin_binance::BinanceAccount {
            name: "synthetic-main".into(),
            resource: ResourceId("binance:uid:1001:SPOT".into()),
            credential: fin_binance::CredentialHandle::new("test-only:synthetic-spot-pricing")
                .unwrap(),
            role: fin_binance::AccountRole::Main,
            parent: None,
            email: None,
            wallet_type: "SPOT".into(),
        }],
        destinations: vec![],
    };
    let adapter = Arc::new(
        fin_binance::BinanceAdapter::new(config, transport.clone(), Some(prices.clone()), None)
            .unwrap(),
    );
    let approval = Arc::new(Approval {
        calls: AtomicUsize::new(0),
        venue,
        change,
    });
    let engine = fin_engine::Engine::new(
        fin_store::Store::in_memory().unwrap(),
        vec![adapter],
        approval.clone(),
        fin_engine::EngineOptions {
            allow_unpriced_exchange_assets: allow_unpriced,
            ..Default::default()
        },
    )
    .unwrap();
    (engine, rates, approval, transport, prices)
}
fn request(id: &str) -> OperationRequest {
    OperationRequest {
        request_id: id.into(),
        operation: "binance.spot.order-place".into(),
        run_id: None,
        input: json!({"request_id":id,"account":"synthetic-main","symbol":"APTUSDC","side":"SELL","quantity":"10","limit_price":"10","time_in_force":"FOK","maximum_fees":{"USDC":"0.2","BNB":"0.01"}}),
    }
}

#[tokio::test]
async fn derived_prices_feed_preview_fee_policy_and_allocation_without_parity() {
    for (usdc, expected, approval_count) in [("0.99", "99000000", 1), ("1.1", "110000000", 0)] {
        let (engine, rates, auth, transport, prices) = fixture(usdc, None);
        let request = request("synthetic-direct-pricing-policy");
        let preview = engine
            .preview(&request.operation, &request.input)
            .await
            .unwrap();
        let valuation = preview.valuation.as_ref().unwrap();
        assert_eq!(valuation.movement_usd_micros.as_str(), expected);
        if usdc == "0.99" {
            assert_eq!(valuation.maximum_fee_usd_micros.as_str(), "410000");
        }
        let quote = fin_binance::BinancePriceProvider::quote_usd(prices.as_ref(), "APT")
            .await
            .unwrap();
        assert_eq!(
            quote.usd_per_unit,
            if usdc == "0.99" { "9.9" } else { "11" }
        );
        assert_eq!(
            prices
                .usd_micros(&Amount {
                    asset: AssetId {
                        network: "binance".into(),
                        reference: "APT".into(),
                        decimals: 36
                    },
                    atoms: Quantity::from_decimal("10", 36).unwrap()
                })
                .await
                .unwrap()
                .0,
            expected
        );
        let result = engine.execute(request).await.unwrap();
        assert!(result.submission.is_none());
        assert_eq!(transport.posts.load(Ordering::SeqCst), 0);
        assert_eq!(auth.calls.load(Ordering::SeqCst), approval_count);
        if approval_count == 0 {
            assert!(result.error.unwrap().code.contains("MOVEMENT"));
        }
        assert_eq!(rates.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn refreshed_spot_prices_obey_the_approved_ceiling_and_duplicate_journal() {
    for (ask, expected_posts) in [("20", 0), ("9.9999", 1)] {
        let (engine, _, auth, transport, _) = fixture("0.99", Some(ask));
        let request = request("synthetic-direct-pricing-revalidation");
        let result = engine.execute(request.clone()).await.unwrap();
        assert_eq!(auth.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            transport.posts.load(Ordering::SeqCst),
            expected_posts,
            "{result:?}"
        );
        if expected_posts == 0 {
            assert!(result.submission.is_none());
            assert_eq!(result.error.as_ref().unwrap().code, "PREVIEW_CHANGED");
        } else {
            assert!(result.submission.is_some());
            assert_ne!(result.status, fin_domain::Status::Completed);
        }
        assert_eq!(engine.execute(request).await.unwrap().id, result.id);
        assert_eq!(transport.posts.load(Ordering::SeqCst), expected_posts);
        assert_eq!(auth.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn unsupported_pair_is_explicit_estimate_and_write_requires_protected_exception() {
    for enabled in [false, true] {
        let (engine, rates, auth, transport, prices) =
            fixture_options("0.99", Some("10"), false, enabled);
        let request = request("synthetic-unpriced-pair");
        let preview = engine
            .preview(&request.operation, &request.input)
            .await
            .unwrap();
        assert!(
            preview
                .warnings
                .iter()
                .any(|v| v.contains(fin_domain::unpriced::WARNING))
        );
        assert!(preview.summary.contains("Unpriced APT: $0 estimate"));
        let value = preview.valuation.as_ref().unwrap();
        assert!(value.prices.iter().any(fin_domain::unpriced::is_estimate));
        assert_eq!(value.movement_usd_micros.as_str(), "0");
        assert!(
            !value.maximum_fee_usd_micros.is_zero(),
            "priced fees remain bounded"
        );
        let price = prices
            .asset_price(&AssetId {
                network: "binance".into(),
                reference: "APT".into(),
                decimals: 36,
            })
            .await
            .unwrap();
        assert!(fin_domain::unpriced::is_estimate(&price));
        let record = engine.execute(request.clone()).await.unwrap();
        if enabled {
            assert!(record.submission.is_some());
            assert_eq!(engine.allocation_unpriced_operations().unwrap(), 1);
            assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
        } else {
            assert_eq!(
                record.error.unwrap().code,
                "UNPRICED_EXCHANGE_ASSET_DISABLED"
            );
            assert!(record.submission.is_none());
            assert_eq!(transport.posts.load(Ordering::SeqCst), 0);
        }
        assert_eq!(auth.calls.load(Ordering::SeqCst), usize::from(enabled));
        assert_eq!(rates.calls.load(Ordering::SeqCst), 1);
        let _ = engine.execute(request).await.unwrap();
        assert_eq!(auth.calls.load(Ordering::SeqCst), usize::from(enabled));
        assert_eq!(transport.posts.load(Ordering::SeqCst), usize::from(enabled));
    }
}

#[tokio::test]
async fn only_unsupported_binance_pair_becomes_zero_not_outages_stale_or_chain_errors() {
    struct Failure(&'static str);
    #[async_trait]
    impl UsdPriceSource for Failure {
        async fn quote(&self, _: &AssetId) -> fin_domain::Result<UsdQuote> {
            Err(FinError::new(
                self.0,
                "valuation",
                "synthetic missing evidence",
                "retry reads",
            ))
        }
    }
    for network in ["binance", "ethereum:1"] {
        for code in [
            "PRICE_SPOT_PAIR_UNSUPPORTED",
            "PRICE_RATE_LIMIT",
            "PRICE_SPOT_STALE",
            "PRICE_SPOT_IDENTITY_CONFLICT",
            "PRICE_CONVERSION_MISSING",
        ] {
            let provider = SharedUsdValuator::with_source(Arc::new(Failure(code)));
            let asset = AssetId {
                network: network.into(),
                reference: "APT".into(),
                decimals: 18,
            };
            let result = provider.asset_price(&asset).await;
            if network == "binance" && code == "PRICE_SPOT_PAIR_UNSUPPORTED" {
                assert!(fin_domain::unpriced::is_estimate(&result.unwrap()));
            } else {
                assert_eq!(result.unwrap_err().code, code);
            }
        }
    }
    assert!(
        super::tests::fixed_valuator("0")
            .asset_price(&AssetId {
                network: "binance".into(),
                reference: "APT".into(),
                decimals: 36
            })
            .await
            .is_err()
    );
}

#[tokio::test]
async fn five_minute_conversion_window_survives_every_execution_gate_without_restamping() {
    let (engine, rates, auth, transport, _) = fixture("0.99", Some("9.9999"));
    rates.age_seconds.store(150, Ordering::SeqCst);
    let request = request("synthetic-150-second-stablecoin-rate");
    let preview = engine
        .preview(&request.operation, &request.input)
        .await
        .unwrap();
    let row = &preview.valuation.as_ref().unwrap().prices[0];
    assert!(
        Utc::now()
            - chrono::DateTime::parse_from_rfc3339(&row.observed_at)
                .unwrap()
                .with_timezone(&Utc)
            > chrono::Duration::seconds(120)
    );
    assert!(row.source.contains("updated:"));
    let record = engine.execute(request).await.unwrap();
    assert!(record.submission.is_some(), "{:?}", record.error);
    assert_eq!(auth.calls.load(Ordering::SeqCst), 1);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
    let (engine, rates, auth, transport, _) = fixture("0.99", Some("9.9999"));
    rates.age_seconds.store(300, Ordering::SeqCst);
    let request = self::request("synthetic-expired-stablecoin-rate");
    assert_eq!(
        engine
            .preview(&request.operation, &request.input)
            .await
            .unwrap_err()
            .code,
        "PRICE_CONVERSION_STALE"
    );
    assert_eq!(auth.calls.load(Ordering::SeqCst), 0);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 0);
}
