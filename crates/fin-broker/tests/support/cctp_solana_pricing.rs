//! Synthetic rate-limited transport around the real Jupiter price source.
//! The full named-plan fixture must not exhaust one pricing window before mint.
use super::*;
use fin_jupiter::api::{JupiterApi, JupiterHost, RateBucket, RequestPriority};
use fin_jupiter::pricing::JupiterUsdPrices;
use reqwest::Method;
use std::time::Duration;

#[derive(Default)]
pub(super) struct PriceApi(AtomicUsize);
impl PriceApi {
    pub(super) fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}
#[async_trait]
impl JupiterApi for PriceApi {
    async fn request(
        &self,
        _host: JupiterHost,
        method: Method,
        path: &str,
        body: Option<Value>,
        _bucket: RateBucket,
        _priority: RequestPriority,
        _max_wait: Duration,
    ) -> Result<Value> {
        assert_eq!(method, Method::GET);
        assert!(body.is_none());
        if self.0.fetch_add(1, Ordering::SeqCst) >= 29 {
            return Err(fail("JUPITER_RATE_LIMIT_WAIT"));
        }
        let mints = path.strip_prefix("/price/v3?ids=").unwrap();
        let mut result = serde_json::Map::new();
        for mint in mints.split(',') {
            let (decimals, price) = match mint {
                "So11111111111111111111111111111111111111112" => (9, "2000"),
                protocol::USDC_MINT => (6, "1"),
                other => panic!("Unexpected priced mint {other}"),
            };
            result.insert(
                mint.into(),
                json!({"usdPrice":price,"decimals":decimals,"blockId":100}),
            );
        }
        Ok(Value::Object(result))
    }
}
struct PriceRpc;
#[async_trait]
impl SolanaRpc for PriceRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        assert_eq!(method, "getBlockTime");
        assert_eq!(params, json!([100]));
        Ok(json!(chrono::Utc::now().timestamp()))
    }
}
struct NetworkPrices(JupiterUsdPrices);
#[async_trait]
impl UsdPriceSource for NetworkPrices {
    async fn quote(&self, asset: &AssetId) -> Result<UsdQuote> {
        if asset.network == fin_solana::NETWORK {
            self.0.quote(asset).await
        } else {
            Prices.quote(asset).await
        }
    }
}
pub(super) fn enable(fixture: &SolanaFixture) -> Arc<PriceApi> {
    let api = Arc::new(PriceApi::default());
    assert!(
        fixture
            .prices
            .set(Arc::new(NetworkPrices(JupiterUsdPrices::new(
                api.clone(),
                Arc::new(PriceRpc)
            ))))
            .is_ok()
    );
    api
}
