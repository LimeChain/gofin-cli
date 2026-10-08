//! Explicit public market-data probe; no wallet/account credentials or writes.
use fin_domain::AssetId;
use fin_pricing::{CoinGeckoPrices, ExchangeSpotPrices, UsdPriceSource};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().skip(1).collect::<Vec<_>>() != ["--public-read-only"] {
        return Err("Pass --public-read-only to make anonymous public CoinGecko requests.".into());
    }
    let provider = ExchangeSpotPrices::new(CoinGeckoPrices::new()?)?;
    let mut failures = 0;
    for (network, reference, decimals) in [
        ("ethereum:1", "native", 18),
        ("ethereum:42161", "native", 18),
        ("ethereum:4663", "native", 18),
        (
            "ethereum:42161",
            "0xaf88d065e77c8cc2239327c5edb3a432268e5831",
            6,
        ),
        (
            "ethereum:4663",
            "0x5fc5360d0400a0fd4f2af552add042d716f1d168",
            6,
        ),
        ("binance", "USDG", 6),
    ] {
        let asset = AssetId {
            network: network.into(),
            reference: reference.into(),
            decimals,
        };
        match provider.quote(&asset).await {
            Ok(quote) => {
                let updated = chrono::DateTime::parse_from_rfc3339(&quote.updated_at)?;
                println!(
                    "{}",
                    serde_json::json!({"asset":asset,"quote":quote,"provider_age_seconds":(chrono::Utc::now()-updated.with_timezone(&chrono::Utc)).num_seconds()})
                );
            }
            Err(error) => {
                failures += 1;
                println!("{}", serde_json::json!({"asset":asset,"error":error}));
            }
        }
    }
    if failures > 0 {
        return Err("Some public quotes are unavailable; inspect the structured results.".into());
    }
    Ok(())
}
