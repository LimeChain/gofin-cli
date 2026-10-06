use async_trait::async_trait;
use fin_binance::{
    BinanceAccount, BinanceAdapter, BinanceConfig, BinanceEndpoint, BinancePriceProvider,
    BinanceRecipientVerifier, BinanceRequest, BinanceTransport, CredentialHandle,
    EthereumDestination, PayoutRequest, PayoutResult, PriceQuote, TransportError,
};
use fin_domain::{
    ApprovalAudit, AuthorizationChallenge, Authorizer, OperationRequest, Result, now,
};
use fin_engine::{Engine, EngineOptions};
use fin_store::Store;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct ScriptedPrices {
    quotes: Mutex<Vec<PriceQuote>>,
    calls: AtomicUsize,
}

impl ScriptedPrices {
    fn new(prices: &[&str]) -> Self {
        Self {
            quotes: Mutex::new(
                prices
                    .iter()
                    .map(|price| PriceQuote {
                        usd_per_unit: (*price).into(),
                        observed_at: now(),
                        source: "synthetic-fixture:quote-feed".into(),
                    })
                    .collect(),
            ),
            calls: AtomicUsize::new(0),
        }
    }

    fn stale() -> Self {
        Self {
            quotes: Mutex::new(vec![PriceQuote {
                usd_per_unit: "1.00".into(),
                observed_at: "2020-01-01T00:00:00Z".into(),
                source: "synthetic-fixture:stale-feed".into(),
            }]),
            calls: AtomicUsize::new(0),
        }
    }

    fn zero() -> Self {
        Self::new(&["0"])
    }
}

#[async_trait]
impl BinancePriceProvider for ScriptedPrices {
    async fn quote_usd(&self, _coin: &str) -> Result<PriceQuote> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.quotes.lock().unwrap().pop().ok_or_else(|| {
            fin_domain::FinError::new(
                "PRICE_UNAVAILABLE",
                "valuation",
                "Synthetic fixture has no remaining quote.",
                "Use a new synthetic fixture.",
            )
        })
    }
}

#[derive(Default)]
struct RecordingTransport {
    posts: AtomicUsize,
}

#[async_trait]
impl BinanceTransport for RecordingTransport {
    async fn request(
        &self,
        _credential: &CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, TransportError> {
        use BinanceEndpoint as E;
        match request.endpoint {
            E::ApiRestrictions => Ok(json!({
                "enableReading":true,"enableWithdrawals":true,
                "enableInternalTransfer":true,"permitsUniversalTransfer":true
            })),
            E::TravelRuleRequirements => Ok(json!({"questionnaireCountryCode":"NIL"})),
            E::CapitalConfig => Ok(json!([{
                "coin":"ETH","withdrawAllEnable":true,"depositAllEnable":true,
                "networkList":[{
                    "network":"ETH","coin":"ETH","depositEnable":true,
                    "withdrawEnable":true,"withdrawIntegerMultiple":"0.00000001",
                    "withdrawFee":"0.0004","withdrawMin":"0.001","withdrawMax":"1000",
                    "withdrawTag":false,"busy":false
                }]
            }])),
            E::Account => Ok(json!({"balances":[{
                "asset":"ETH","free":"200","locked":"0"
            }]})),
            E::StandardWithdrawal => {
                self.posts.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"id":"synthetic-withdrawal-id"}))
            }
            _ => Err(TransportError::InvalidResponse),
        }
    }
}

struct PendingRecipientVerifier;

#[async_trait]
impl BinanceRecipientVerifier for PendingRecipientVerifier {
    async fn verify(&self, _request: PayoutRequest) -> PayoutResult {
        PayoutResult {
            status: fin_domain::Status::Pending,
            amount_atoms: None,
            evidence: json!({"fixture":"synthetic only"}),
        }
    }
}

#[derive(Default)]
struct CountingAuthorizer(AtomicUsize);

#[async_trait]
impl Authorizer for CountingAuthorizer {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "synthetic-test-only".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

fn config() -> BinanceConfig {
    BinanceConfig {
        accounts: vec![BinanceAccount {
            name: "synthetic-main".into(),
            resource: fin_domain::ResourceId("binance:synthetic-main".into()),
            credential: CredentialHandle::new("test-only:synthetic-main").unwrap(),
            role: fin_binance::AccountRole::Main,
            parent: None,
            email: None,
            wallet_type: "SPOT".into(),
        }],
        destinations: vec![EthereumDestination {
            name: "synthetic-wallet".into(),
            resource: fin_domain::ResourceId("ethereum:synthetic-wallet".into()),
            address: "0x1111111111111111111111111111111111111111".into(),
            chain_id: 1,
            assets: std::collections::BTreeMap::from([(
                "ETH".into(),
                fin_domain::AssetId {
                    network: "ethereum:1".into(),
                    reference: "native".into(),
                    decimals: 18,
                },
            )]),
            confirmations: 2,
        }],
    }
}

fn withdrawal_input() -> Value {
    json!({
        "requestId":"synthetic-valuation-revalidation",
        "account":"synthetic-main","destination":"synthetic-wallet",
        "coin":"ETH","amount":"0.01","network":"ETH","walletType":"SPOT",
        "maxFee":"0.001","minReceiveAmount":"0.009"
    })
}

#[tokio::test]
async fn engine_rejects_third_quote_movement_before_any_binance_post() {
    // This synthetic price feed values a selected 99-unit withdrawal at $99
    // for prepare and engine revalidation, then $100.98 at Binance's final
    // submit revalidation. Engine execution performs adapter prepare (1.00),
    // its post-approval revalidation (1.00), then Binance submit performs the
    // final adapter revalidation (1.02). The last quote must not escape the
    // approved $100 movement ceiling.
    let prices = Arc::new(ScriptedPrices::new(&["1.02", "1.00", "1.00"]));
    let transport = Arc::new(RecordingTransport::default());
    let adapter = Arc::new(
        BinanceAdapter::new(
            config(),
            transport.clone(),
            Some(prices.clone()),
            Some(Arc::new(PendingRecipientVerifier)),
        )
        .unwrap(),
    );
    let authorizer = Arc::new(CountingAuthorizer::default());
    let directory = tempfile::tempdir().unwrap();
    let engine = Engine::new(
        Store::open(directory.path().join("journal.sqlite")).unwrap(),
        vec![adapter],
        authorizer.clone(),
        EngineOptions::default(),
    )
    .unwrap();

    let mut input = withdrawal_input();
    input["amount"] = json!("99");
    input["minReceiveAmount"] = json!("98.999");
    let record = engine
        .execute(OperationRequest {
            request_id: "synthetic-valuation-revalidation".into(),
            operation: "binance.withdraw".into(),
            input,
            run_id: None,
        })
        .await
        .unwrap();

    assert_eq!(prices.calls.load(Ordering::SeqCst), 3);
    assert_eq!(authorizer.0.load(Ordering::SeqCst), 1);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 0);
    assert!(record.approval.is_some());
    assert!(record.submission.is_none());
    let approved_movement = record
        .preview
        .as_ref()
        .unwrap()
        .valuation
        .as_ref()
        .unwrap()
        .movement_usd_micros
        .as_str()
        .parse::<u64>()
        .unwrap();
    let final_quote_usd_micros_per_unit = 1_020_000u64;
    let final_movement = 99u64 * final_quote_usd_micros_per_unit;
    assert_eq!(approved_movement, 99_000_000);
    assert_eq!(final_movement, 100_980_000);
    assert!(final_movement > 100_000_000);
    assert_eq!(
        record.error.as_ref().map(|error| error.code.as_str()),
        Some("PREVIEW_CHANGED")
    );
}

#[tokio::test]
async fn adapter_accepts_unchanged_quote_with_engine_enriched_price_rows() {
    use fin_domain::Adapter;
    let prices = Arc::new(ScriptedPrices::new(&["1.00", "1.00"]));
    let adapter = BinanceAdapter::new(
        config(),
        Arc::new(RecordingTransport::default()),
        Some(prices.clone()),
        Some(Arc::new(PendingRecipientVerifier)),
    )
    .unwrap();
    let mut approved = adapter
        .prepare("binance.withdraw", &withdrawal_input())
        .await
        .unwrap();
    let valuation = approved.valuation.as_mut().unwrap();
    valuation.prices.push(fin_domain::AssetPrice {
        asset: fin_domain::AssetId {
            network: "binance".into(),
            reference: "USDC".into(),
            decimals: 36,
        },
        usd_micros_per_unit: fin_domain::Quantity::new("1000000").unwrap(),
        unit_atoms: fin_domain::Quantity::new(format!("1{}", "0".repeat(36))).unwrap(),
        observed_at: now(),
        source: "synthetic-engine-cost-origin-price".into(),
    });
    // Similar symbols on another network are independent exact identities;
    // engine enrichment rows must not be mistaken for the Binance quote.
    valuation.prices.push(fin_domain::AssetPrice {
        asset: fin_domain::AssetId {
            network: "ethereum:1".into(),
            reference: "ETH".into(),
            decimals: 36,
        },
        usd_micros_per_unit: fin_domain::Quantity::new("1000000").unwrap(),
        unit_atoms: fin_domain::Quantity::new(format!("1{}", "0".repeat(36))).unwrap(),
        observed_at: now(),
        source: "synthetic-engine-other-network-price".into(),
    });
    let refreshed = adapter.revalidate(&approved).await.unwrap();
    assert_eq!(prices.calls.load(Ordering::SeqCst), 2);
    assert_eq!(refreshed.valuation.as_ref().unwrap().prices.len(), 2);
}

#[tokio::test]
async fn duplicate_adapter_native_price_identity_is_rejected() {
    use fin_domain::Adapter;
    let prices = Arc::new(ScriptedPrices::new(&["1.00", "1.00"]));
    let adapter = BinanceAdapter::new(
        config(),
        Arc::new(RecordingTransport::default()),
        Some(prices),
        Some(Arc::new(PendingRecipientVerifier)),
    )
    .unwrap();
    let mut approved = adapter
        .prepare("binance.withdraw", &withdrawal_input())
        .await
        .unwrap();
    let native_binance_row = approved.valuation.as_ref().unwrap().prices[0].clone();
    approved
        .valuation
        .as_mut()
        .unwrap()
        .prices
        .push(native_binance_row);
    assert_eq!(
        adapter.revalidate(&approved).await.unwrap_err().code,
        "PREVIEW_CHANGED"
    );
}

#[tokio::test]
async fn provider_timestamp_survives_prepare_and_expired_quotes_block_submission() {
    use fin_domain::Adapter;
    for seconds in [90, 121] {
        let timestamp = (chrono::Utc::now() - chrono::Duration::seconds(seconds)).to_rfc3339();
        let prices = Arc::new(ScriptedPrices {
            quotes: Mutex::new(vec![PriceQuote {
                usd_per_unit: "1.02".into(),
                observed_at: timestamp.clone(),
                source: "synthetic-fixture:CoinGecko-source-time".into(),
            }]),
            calls: AtomicUsize::new(0),
        });
        let transport = Arc::new(RecordingTransport::default());
        let adapter = BinanceAdapter::new(
            config(),
            transport.clone(),
            Some(prices),
            Some(Arc::new(PendingRecipientVerifier)),
        )
        .unwrap();
        let result = adapter
            .prepare("binance.withdraw", &withdrawal_input())
            .await;
        if seconds == 90 {
            let valuation = result.unwrap().valuation.unwrap();
            assert_eq!(valuation.observed_at, timestamp);
            assert_eq!(valuation.prices[0].observed_at, timestamp);
        } else {
            assert_eq!(result.unwrap_err().code, "STALE_VALUATION");
        }
        assert_eq!(transport.posts.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn stale_zero_and_unavailable_quotes_fail_closed_during_prepare() {
    use fin_domain::Adapter;
    for (provider, expected) in [
        (Arc::new(ScriptedPrices::stale()), "STALE_VALUATION"),
        (Arc::new(ScriptedPrices::zero()), "VALUATION_INVALID"),
    ] {
        let adapter = BinanceAdapter::new(
            config(),
            Arc::new(RecordingTransport::default()),
            Some(provider),
            Some(Arc::new(PendingRecipientVerifier)),
        )
        .unwrap();
        assert_eq!(
            adapter
                .prepare("binance.withdraw", &withdrawal_input())
                .await
                .unwrap_err()
                .code,
            expected
        );
    }

    struct MissingPrice;
    #[async_trait]
    impl BinancePriceProvider for MissingPrice {
        async fn quote_usd(&self, _coin: &str) -> Result<PriceQuote> {
            Err(fin_domain::FinError::new(
                "PRICE_UNAVAILABLE",
                "valuation",
                "Synthetic quote is unavailable.",
                "Retry with a fresh quote.",
            ))
        }
    }
    let adapter = BinanceAdapter::new(
        config(),
        Arc::new(RecordingTransport::default()),
        Some(Arc::new(MissingPrice)),
        Some(Arc::new(PendingRecipientVerifier)),
    )
    .unwrap();
    assert_eq!(
        adapter
            .prepare("binance.withdraw", &withdrawal_input())
            .await
            .unwrap_err()
            .code,
        "PRICE_UNAVAILABLE"
    );
}
