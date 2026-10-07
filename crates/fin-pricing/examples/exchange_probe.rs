//! Explicit anonymous public reads through the production identity/price path.
//! No protected configuration, environment credentials, accounts or writes.
use fin_domain::AssetId;
use fin_pricing::{CoinGeckoPrices, ExchangeAssetKey, ExchangeAssetResolver, UsdPriceSource};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let cache_path = match arguments.as_slice() {
        [flag] if flag == "--public-read-only" => None,
        [flag, option, path] if flag == "--public-read-only" && option == "--identity-cache" => {
            Some(std::path::PathBuf::from(path))
        }
        _ => {
            return Err(
                "Pass --public-read-only for anonymous CoinGecko/Binance metadata and USD reads."
                    .into(),
            );
        }
    };
    let provider = CoinGeckoPrices::new()?;
    let provider = if let Some(path) = cache_path {
        provider.with_identity_store(std::sync::Arc::new(PublicProbeStore(path)))?
    } else {
        provider
    };
    let assets = ["APT", "SEI", "USDC", "ETH", "BNB"].map(|coin| AssetId {
        network: "binance".into(),
        reference: coin.into(),
        decimals: 36,
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(600);
    let quotes = loop {
        match provider.quotes(&assets).await {
            Ok(quotes) => break quotes,
            Err(error)
                if matches!(
                    error.code.as_str(),
                    "PRICE_RATE_LIMITED" | "PRICE_IDENTITY_PENDING"
                ) && tokio::time::Instant::now() < deadline =>
            {
                eprintln!("{}: {}", error.code, error.message);
                tokio::time::sleep(std::time::Duration::from_secs(6)).await;
            }
            Err(error) => {
                println!("{}", serde_json::json!({"ok":false,"error":error}));
                return Err(
                    "Anonymous batch pricing did not pass; inspect the safe result.".into(),
                );
            }
        }
    };
    for (asset, quote) in assets.iter().zip(quotes) {
        let identity = provider
            .resolve_exchange_asset(&ExchangeAssetKey {
                exchange: "binance".into(),
                asset: asset.reference.clone(),
            })
            .await?;
        println!(
            "{}",
            serde_json::json!({"asset":asset,"ok":true,"result":{"identity":identity,"quote":quote}})
        );
    }
    Ok(())
}

// Explicit public-read harness storage, never passed to the protected broker.
struct PublicProbeStore(std::path::PathBuf);
impl fin_pricing::IdentityStore for PublicProbeStore {
    fn load(&self) -> fin_domain::Result<Option<Vec<u8>>> {
        match std::fs::read(&self.0) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(fin_domain::FinError::validation(
                "Public probe identity cache could not be read.",
            )),
        }
    }
    fn save(&self, bytes: &[u8]) -> fin_domain::Result<()> {
        use std::io::Write;
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        options
            .open(&self.0)
            .and_then(|mut file| file.write_all(bytes))
            .map_err(|_| {
                fin_domain::FinError::validation("Public probe identity cache could not be saved.")
            })
    }
}
