//! Explicit public-only compatibility probe. No account, credentials or writes.
use fin_binance::{BinanceCcxtTransport, BinanceConfig, trading_reads::trading_read};
use fin_exchange::ccxt::{CcxtClient, CcxtError, CredentialProvider, Credentials};
use serde_json::json;
use std::sync::Arc;

struct NoCredentials;
impl CredentialProvider for NoCredentials {
    fn credentials(&self, _: &str) -> Result<Credentials, CcxtError> {
        panic!("the public market probe must never access a credential")
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let transport =
        BinanceCcxtTransport::new(Arc::new(CcxtClient::new()?), Arc::new(NoCredentials));
    for scope in ["spot", "futures"] {
        let operation = format!("binance.{scope}.markets");
        let result = trading_read(
            &operation,
            &json!({}),
            &BinanceConfig::default(),
            &transport,
        )
        .await?;
        let markets = result["markets"].as_array().expect("typed read result");
        println!(
            "{}",
            json!({"operation":operation, "observedAt":result["observedAt"], "market_count":markets.len(), "non_ascii_symbols":markets.iter().filter_map(|row| row["symbol"].as_str()).filter(|symbol| !symbol.is_ascii()).collect::<Vec<_>>() })
        );
    }
    Ok(())
}
