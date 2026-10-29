//! Synthetic production-path spot/FX valuation through the broker and engine.
use super::*;
use async_trait::async_trait;
use gofin_domain::{ApprovalAudit, AuthorizationChallenge, Authorizer, OperationRequest};
use gofin_pricing::{
    ExchangeMarket, SpotBook, SpotMarketSource, StablecoinRates, StablecoinUsdSource,
};
use std::sync::{
    Mutex,
    atomic::{AtomicI64, AtomicUsize, Ordering},
};

#[cfg(target_os = "macos")]
use crate::native_approval;

struct VenuePrices {
    ask: Mutex<String>,
    old_snapshot: bool,
    apt_pair: bool,
}
#[async_trait]
impl SpotMarketSource for VenuePrices {
    async fn markets(&self, _: &str) -> gofin_domain::Result<Vec<ExchangeMarket>> {
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
    async fn books(&self, _: &str, markets: &[String]) -> gofin_domain::Result<Vec<SpotBook>> {
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
    async fn stablecoin_rates(&self) -> gofin_domain::Result<StablecoinRates> {
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
    async fn quote(&self, _: &AssetId) -> gofin_domain::Result<UsdQuote> {
        panic!("exchange valuation must not request a per-asset onchain/provider price")
    }
}
struct TradingReads {
    posts: AtomicUsize,
}
#[async_trait]
impl gofin_binance::BinanceTransport for TradingReads {
    async fn request(
        &self,
        credential: &gofin_binance::CredentialHandle,
        request: gofin_binance::BinanceRequest,
    ) -> std::result::Result<Value, gofin_binance::TransportError> {
        assert_eq!(credential.as_str(), "test-only:synthetic-spot-pricing");
        use gofin_binance::BinanceEndpoint as E;
        match request.endpoint {
            E::ApiRestrictions => {
                Ok(json!({"enableReading":true,"enableSpotAndMarginTrading":true}))
            }
            E::Account => Ok(
                json!({"uid":1001,"accountType":"SPOT","canTrade":true,"balances":[{"asset":"APT","free":"100","locked":"0"},{"asset":"USDC","free":"1000","locked":"0"},{"asset":"BNB","free":"10","locked":"0"}]}),
            ),
            E::SpotExchangeInfo => Ok(
                json!({"symbols":[{"symbol":"APTUSDC","status":"TRADING","baseAsset":"APT","quoteAsset":"USDC","isSpotTradingAllowed":true,"orderTypes":["LIMIT","MARKET","LIMIT_MAKER"],"quoteOrderQtyMarketAllowed":true,
                "filters":[{"filterType":"PRICE_FILTER","minPrice":"0.01","maxPrice":"1000","tickSize":"0.01"},{"filterType":"LOT_SIZE","minQty":"0.01","maxQty":"1000","stepSize":"0.01"},{"filterType":"MIN_NOTIONAL","minNotional":"5"}]}]}),
            ),
            E::SpotCommission => Ok(
                json!({"symbol":"APTUSDC","standardCommission":{"maker":"0.001","taker":"0.001","buyer":"0","seller":"0"},"taxCommission":{"maker":"0","taker":"0","buyer":"0","seller":"0"},"specialCommission":{"maker":"0","taker":"0","buyer":"0","seller":"0"},"discount":{"enabledForAccount":false,"enabledForSymbol":false,"discountAsset":"BNB","discount":"0.25"}}),
            ),
            E::SpotPlaceOrder => {
                self.posts.fetch_add(1, Ordering::SeqCst);
                Err(gofin_binance::TransportError::Timeout)
            }
            _ => Err(gofin_binance::TransportError::InvalidResponse),
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
    ) -> gofin_domain::Result<ApprovalAudit> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(ask) = self.change {
            *self.venue.ask.lock().unwrap() = ask.into();
        } else {
            return Err(GoFinError::new(
                "AUTHORIZATION_REJECTED",
                "approval",
                "Synthetic approval declined.",
                "No real approval was requested.",
            ));
        }
        Ok(ApprovalAudit {
            source: Some(gofin_domain::AuthorizationSource::Development),
            yolo_provenance: None,
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
    gofin_engine::Engine,
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
    gofin_engine::Engine,
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
    let config = gofin_binance::BinanceConfig {
        accounts: vec![gofin_binance::BinanceAccount {
            name: "synthetic-main".into(),
            resource: ResourceId("binance:uid:1001:SPOT".into()),
            credential: gofin_binance::CredentialHandle::new("test-only:synthetic-spot-pricing")
                .unwrap(),
            role: gofin_binance::AccountRole::Main,
            parent: None,
            email: None,
            wallet_type: "SPOT".into(),
        }],
        destinations: vec![],
    };
    let adapter = Arc::new(
        gofin_binance::BinanceAdapter::new(config, transport.clone(), Some(prices.clone()), None)
            .unwrap(),
    );
    let approval = Arc::new(Approval {
        calls: AtomicUsize::new(0),
        venue,
        change,
    });
    let engine = gofin_engine::Engine::new(
        gofin_store::Store::in_memory().unwrap(),
        vec![adapter],
        approval.clone(),
        gofin_engine::EngineOptions {
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
async fn native_spot_order_previews_preserve_material_terms_through_approval() {
    for (side, kind) in [
        ("BUY", "MARKET"),
        ("SELL", "MARKET"),
        ("BUY", "LIMIT"),
        ("SELL", "LIMIT"),
        ("BUY", "LIMIT_MAKER"),
        ("SELL", "LIMIT_MAKER"),
    ] {
        let (engine, _, auth, transport, _) = fixture("0.99", None);
        let mut request = request("synthetic-native-approval");
        let input = request.input.as_object_mut().unwrap();
        input.insert("side".into(), json!(side));
        input.insert("order_type".into(), json!(kind));
        if side == "BUY" {
            input.insert("maximum_fees".into(), json!({"APT":"0.02","BNB":"0.01"}));
        }
        if kind == "MARKET" {
            input.remove("limit_price");
            input.remove("time_in_force");
            if side == "BUY" {
                input.remove("quantity");
                input.insert("quote_order_quantity".into(), json!("100"));
            }
        } else if kind == "LIMIT_MAKER" {
            input.remove("time_in_force");
        } else {
            input.insert("time_in_force".into(), json!("GTC"));
        }
        let prepared = engine
            .preview(&request.operation, &request.input)
            .await
            .unwrap();
        let review = gofin_domain::approval_review::approval_review(&prepared).unwrap();
        let details = review.other_details.as_deref().unwrap();
        let input_leg = review.input.as_ref().unwrap();
        assert_eq!(review.market.as_deref(), Some("APTUSDC"));
        assert_eq!(
            input_leg.amount.as_deref(),
            Some(if side == "BUY" { "100" } else { "10" })
        );
        assert_eq!(
            input_leg.asset.as_ref().unwrap().identifier.reference,
            if side == "BUY" { "USDC" } else { "APT" }
        );
        assert!(details.contains(&format!("{side} {kind}")));
        assert_eq!(
            review.output.as_ref().unwrap().account,
            input_leg.account,
            "Both spot conversion legs belong to the enrolled account"
        );
        let material = if kind == "MARKET" {
            assert!(!details.contains("Limit:"));
            assert!(!details.contains("Time in force:"));
            if side == "BUY" {
                assert!(details.contains("Quote budget: 100"));
                "No price or minimum fill guarantee"
            } else {
                assert!(details.contains("Base quantity: 10"));
                "No price or minimum proceeds guarantee"
            }
        } else {
            assert!(details.contains("Base quantity: 10"));
            assert!(details.contains("Limit: 10"));
            if kind == "LIMIT_MAKER" {
                assert!(!details.contains("Time in force:"));
                "Maker only; rests until filled or canceled"
            } else {
                assert!(details.contains("Time in force: GTC"));
                "Rests until filled or canceled"
            }
        };
        assert!(details.contains(material));
        assert!(details.contains("Fee ≤: 0.01 BNB"));
        #[cfg(target_os = "macos")]
        {
            let reason = native_approval::assert_prepared_renders(&prepared);
            assert!(reason.contains(material), "{reason}");
            assert!(
                reason.contains(if side == "BUY" { "100 USDC" } else { "10 APT" }),
                "{reason}"
            );
            assert!(
                reason.contains(if side == "BUY" {
                    "Deducted cost ≤: 0.02 APT"
                } else {
                    "Deducted cost ≤: 0.2 USDC"
                }),
                "{reason}"
            );
            assert!(reason.contains("Fee ≤: 0.01 BNB"), "{reason}");
            assert!(reason.contains(&format!("{side} {kind}")), "{reason}");
            assert!(!reason.contains("binance:uid:"), "{reason}");
        }
        assert_eq!(auth.calls.load(Ordering::SeqCst), 0);
        assert_eq!(transport.posts.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn native_spot_schemas_validate_examples_and_reject_ambiguous_sizing() {
    for operation in ["binance.spot.order-place", "binance.futures.order-place"] {
        let definition = gofin_binance::operation_definitions()
            .into_iter()
            .find(|definition| definition.name == operation)
            .unwrap();
        let validator = jsonschema::options()
            .offline()
            .build(&definition.input_schema)
            .unwrap();
        for example in &definition.examples {
            validator
                .validate(example)
                .unwrap_or_else(|error| panic!("{operation}: {error}"));
        }
        let mut invalid = definition.examples[0].clone();
        invalid.as_object_mut().unwrap().remove("limit_price");
        assert!(!validator.is_valid(&invalid));
        if operation == "binance.futures.order-place" {
            let mut invalid = definition.examples[0].clone();
            invalid["time_in_force"] = json!("GTC");
            assert!(!validator.is_valid(&invalid));
            invalid["time_in_force"] = json!("IOC");
            invalid["order_type"] = json!("MARKET");
            assert!(!validator.is_valid(&invalid));
        } else {
            assert_eq!(definition.examples.len(), 5);
            for example in &definition.examples {
                let mut invalid = example.clone();
                if example["order_type"] == "MARKET" {
                    invalid["limit_price"] = json!("2000");
                } else if example["order_type"] == "LIMIT_MAKER" {
                    invalid["time_in_force"] = json!("GTC");
                } else {
                    invalid["quote_order_quantity"] = json!("20");
                }
                assert!(!validator.is_valid(&invalid), "{invalid}");
            }
        }
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
        let quote = gofin_binance::BinancePriceProvider::quote_usd(prices.as_ref(), "APT")
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
            assert_ne!(result.status, gofin_domain::Status::Completed);
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
                .any(|v| v.contains(gofin_domain::unpriced::WARNING))
        );
        assert!(preview.summary.contains("Unpriced APT: $0 estimate"));
        let value = preview.valuation.as_ref().unwrap();
        assert!(value.prices.iter().any(gofin_domain::unpriced::is_estimate));
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
        assert!(gofin_domain::unpriced::is_estimate(&price));
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
        async fn quote(&self, _: &AssetId) -> gofin_domain::Result<UsdQuote> {
            Err(GoFinError::new(
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
                assert!(gofin_domain::unpriced::is_estimate(&result.unwrap()));
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
