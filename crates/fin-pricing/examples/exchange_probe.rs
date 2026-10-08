//! Explicit anonymous public spot/FX reads; no protected accounts or writes.
use fin_domain::AssetId;
use fin_pricing::{CoinGeckoPrices, ExchangeSpotPrices, UsdPriceSource};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().skip(1).collect::<Vec<_>>() != ["--public-read-only"] {
        return Err("Pass --public-read-only for Binance spot and batched stablecoin/USD reads. No identity-cache file is needed.".into());
    }
    let provider = ExchangeSpotPrices::new(CoinGeckoPrices::new()?)?;
    let assets = ["APT", "SEI", "USDC", "USDT", "ETH", "BNB"].map(|coin| AssetId {
        network: "binance".into(),
        reference: coin.into(),
        decimals: 36,
    });
    let quotes = provider.quotes(&assets).await?;
    for (asset, quote) in assets.iter().zip(quotes) {
        println!(
            "{}",
            serde_json::json!({"asset":asset,"ok":true,"quote":quote})
        );
    }
    Ok(())
}
