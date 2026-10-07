//! Explicit anonymous public reads through the production identity/price path.
//! No protected configuration, environment credentials, accounts or writes.
use fin_domain::AssetId;
use fin_pricing::{CoinGeckoPrices, ExchangeAssetKey, ExchangeAssetResolver, UsdPriceSource};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().skip(1).collect::<Vec<_>>() != ["--public-read-only"] {
        return Err(
            "Pass --public-read-only for anonymous CoinGecko/Binance metadata and USD reads."
                .into(),
        );
    }
    let provider = CoinGeckoPrices::new()?;
    let mut failed = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
    for coin in ["APT", "SEI", "USDC", "ETH"] {
        let asset = AssetId {
            network: "binance".into(),
            reference: coin.into(),
            decimals: 36,
        };
        let result = loop {
            let attempt = async {
                let identity = provider
                    .resolve_exchange_asset(&ExchangeAssetKey {
                        exchange: "binance".into(),
                        asset: coin.into(),
                    })
                    .await?;
                let quote = provider.quote(&asset).await?;
                Ok::<_, fin_domain::FinError>(
                    serde_json::json!({"identity":identity,"quote":quote}),
                )
            }
            .await;
            if attempt
                .as_ref()
                .is_err_and(|error| error.code == "PRICE_RATE_LIMITED")
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
            break attempt;
        };
        match result {
            Ok(result) => println!(
                "{}",
                serde_json::json!({"asset":asset,"ok":true,"result":result})
            ),
            Err(error) => {
                failed = true;
                println!(
                    "{}",
                    serde_json::json!({"asset":asset,"ok":false,"error":error})
                );
            }
        }
    }
    if failed {
        return Err(
            "Some public identities/quotes were unavailable; inspect the safe results.".into(),
        );
    }
    Ok(())
}
