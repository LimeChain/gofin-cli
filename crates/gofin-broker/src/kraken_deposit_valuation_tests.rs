use super::*;
use std::sync::atomic::{AtomicU64, AtomicUsize};

pub(super) struct DepositPrices {
    pub usdc_micros: AtomicU64,
    next_wallet_price: AtomicU64,
    pub transitions: AtomicUsize,
}
impl DepositPrices {
    pub fn new() -> Self {
        Self {
            usdc_micros: AtomicU64::new(999_800),
            next_wallet_price: AtomicU64::new(0),
            transitions: AtomicUsize::new(0),
        }
    }
    pub fn arm_after_next_wallet_quote(&self, next_usdc_micros: u64) {
        assert!(next_usdc_micros > 0);
        self.next_wallet_price
            .store(next_usdc_micros, Ordering::SeqCst);
    }
    fn price(&self, asset: &AssetId) -> u64 {
        if asset.reference == "USDC" || asset.reference == usdc_fixture::TOKEN {
            self.usdc_micros.load(Ordering::SeqCst)
        } else {
            2_000_000_000
        }
    }
}
#[async_trait]
impl gofin_pricing::UsdPriceSource for DepositPrices {
    async fn quote(&self, asset: &AssetId) -> Result<gofin_pricing::UsdQuote> {
        Ok(gofin_pricing::UsdQuote {
            usd_per_unit: Quantity::from_u64(self.price(asset)).to_decimal(6),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic-deposit-price".into(),
        })
    }
}
#[async_trait]
impl gofin_ethereum::WalletValuation for DepositPrices {
    async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
        let price = self.price(asset);
        if asset.reference == "USDC" || asset.reference == usdc_fixture::TOKEN {
            let next = self.next_wallet_price.swap(0, Ordering::SeqCst);
            if next != 0 {
                self.usdc_micros.store(next, Ordering::SeqCst);
                self.transitions.fetch_add(1, Ordering::SeqCst);
            }
        }
        Ok((
            units.mul_div_ceil(
                &Quantity::from_u64(price),
                &Quantity::new(format!("1{}", "0".repeat(asset.decimals as usize)))?,
            )?,
            "synthetic-deposit-price".into(),
        ))
    }
}
pub(super) fn with_deposit_prices(
    adapter: KrakenEvmDepositAdapter,
    prices: Arc<DepositPrices>,
) -> KrakenEvmDepositAdapter {
    let KrakenEvmDepositAdapter {
        kraken,
        ethereum,
        solana,
    } = adapter;
    let ethereum = ethereum
        .into_iter()
        .map(|(id, adapter)| {
            (
                id,
                Arc::new(
                    Arc::try_unwrap(adapter)
                        .ok()
                        .expect("unique fixture adapter")
                        .with_wallet_valuation(prices.clone()),
                ),
            )
        })
        .collect();
    KrakenEvmDepositAdapter::with_solana(
        Arc::new(
            Arc::try_unwrap(kraken)
                .ok()
                .expect("unique fixture profile")
                .with_prices(prices),
        ),
        ethereum,
        solana,
    )
}
fn fixture() -> (
    KrakenEvmDepositAdapter,
    Arc<Venue>,
    Arc<DepositPrices>,
    Value,
) {
    let (adapter, venue) = adapter();
    venue.usdc_evm.store(true, Ordering::SeqCst);
    venue.rpc.usdc_evm.store(true, Ordering::SeqCst);
    let prices = Arc::new(DepositPrices::new());
    let adapter = with_deposit_prices(adapter, prices.clone());
    let mut input = input();
    input["amount"] = json!("100");
    input["address"] = json!("0x1111111111111111111111111111111111111111");
    (adapter, venue, prices, input)
}
#[tokio::test]
async fn tiny_deposit_price_drift_keeps_original_inner_and_outer_authority() {
    let (adapter, venue, prices, input) = fixture();
    let approved = adapter.prepare("kraken.deposit", &input).await.unwrap();
    let inner = decode_binding(&approved.binding).unwrap().inner;
    let key = gofin_domain::value_bounds::KEY;
    assert_eq!(approved.binding[key]["movement_usd_micros"], "100980000");
    assert_eq!(inner.binding[key]["movement_usd_micros"], "100980000");
    assert!(approved.summary.contains("1% pricing buffer"));
    assert_eq!(inner.summary.matches("USD ceilings:").count(), 1);
    prices.usdc_micros.store(999_804, Ordering::SeqCst);
    let refreshed = adapter.revalidate(&approved).await.unwrap();
    assert_eq!(refreshed.binding, approved.binding);
    assert_eq!(refreshed.intent, approved.intent);
    assert_eq!(
        refreshed.valuation.as_ref().unwrap().movement_usd_micros,
        approved.valuation.as_ref().unwrap().movement_usd_micros
    );
    assert!(venue.rpc.submitted_hash.lock().unwrap().is_none());
}
#[tokio::test]
async fn legacy_deposit_ceiling_is_not_widened_and_actual_buffer_excess_still_rejects() {
    let (adapter, _, prices, input) = fixture();
    let approved = adapter.prepare("kraken.deposit", &input).await.unwrap();
    let mut legacy = approved.clone();
    let key = gofin_domain::value_bounds::KEY;
    legacy.binding[key]["movement_usd_micros"] = json!("99980000");
    legacy.valuation.as_mut().unwrap().movement_usd_micros = Quantity::from_u64(99_980_000);
    legacy.binding["inner"]["binding"][key]["movement_usd_micros"] = json!("99980000");
    legacy.binding["inner"]["valuation"]["movement_usd_micros"] = json!("99980000");
    let original = legacy.clone();
    prices.usdc_micros.store(999_804, Ordering::SeqCst);
    let error = adapter.revalidate(&legacy).await.unwrap_err();
    assert_eq!(error.code, "VALUE_BOUND_EXCEEDED");
    assert_eq!(
        error.details.as_ref().unwrap()["observed_usd_micros"],
        "99980400"
    );
    assert_eq!(
        error.details.as_ref().unwrap()["maximum_usd_micros"],
        "99980000"
    );
    assert_eq!(legacy, original);
    prices.usdc_micros.store(1_020_000, Ordering::SeqCst);
    assert_eq!(
        adapter.revalidate(&approved).await.unwrap_err().code,
        "VALUE_BOUND_EXCEEDED"
    );
}
#[tokio::test]
async fn legacy_outer_deposit_ceiling_is_not_widened_with_buffered_inner() {
    let (adapter, venue, prices, input) = fixture();
    let mut legacy = adapter.prepare("kraken.deposit", &input).await.unwrap();
    let key = gofin_domain::value_bounds::KEY;
    assert_eq!(
        legacy.binding["inner"]["binding"][key]["movement_usd_micros"],
        "100980000"
    );
    legacy.binding[key]["movement_usd_micros"] = json!("99980000");
    legacy.valuation.as_mut().unwrap().movement_usd_micros = Quantity::from_u64(99_980_000);
    let original = legacy.clone();
    prices.usdc_micros.store(999_804, Ordering::SeqCst);
    let error = adapter.revalidate(&legacy).await.unwrap_err();
    assert_eq!(error.code, "VALUE_BOUND_EXCEEDED");
    assert_eq!(
        error.details.as_ref().unwrap()["observed_usd_micros"],
        "99980400"
    );
    assert_eq!(
        error.details.as_ref().unwrap()["maximum_usd_micros"],
        "99980000"
    );
    assert_eq!(legacy, original);
    assert!(venue.rpc.submitted_hash.lock().unwrap().is_none());
}
#[tokio::test]
async fn deposit_price_reserve_cannot_authorize_changed_asset_identity() {
    let (adapter, venue, _, input) = fixture();
    let approved = adapter.prepare("kraken.deposit", &input).await.unwrap();
    venue.wrong_network.store(true, Ordering::SeqCst);
    // The synthetic USDC route fixes its network, so assert the independent
    // original price identity guard directly on this real prepared candidate.
    let mut changed = approved.clone();
    changed.valuation.as_mut().unwrap().prices[0]
        .asset
        .reference = "different-asset".into();
    assert_eq!(
        gofin_domain::value_bounds::carry(&approved, &mut changed)
            .unwrap_err()
            .code,
        "VALUE_BOUND_EXCEEDED"
    );
    assert!(venue.rpc.submitted_hash.lock().unwrap().is_none());
}
