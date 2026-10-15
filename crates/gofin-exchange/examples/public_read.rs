//! Explicit public-only smoke test. Never loads credentials or submits writes.
#[cfg(feature = "ccxt")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use gofin_domain::Adapter;
    use gofin_exchange::{DefaultExchangeProfile, ExchangeAdapter, ccxt::CcxtClient};
    use serde_json::json;
    let adapter = ExchangeAdapter::new(DefaultExchangeProfile::new(std::sync::Arc::new(
        CcxtClient::new()?,
    )));
    let mut venues: Vec<String> = std::env::args().skip(1).collect();
    if venues.is_empty() {
        venues.push("kraken".into());
    }
    let mut failed = false;
    for venue in venues {
        let pair = match venue.as_str() {
            "kraken" => "XBTUSD",
            "hyperliquid" => "BTC",
            "okx" => "BTC-USDT",
            "coinbase" => "BTC-USD",
            "bybit" => "BTCUSDT",
            _ => return Err("Unsupported smoke-test exchange".into()),
        };
        let result = async {
            let markets=adapter.read("exchange.markets", &json!({"exchange":venue})).await?;
            let ticker=adapter.read("exchange.ticker", &json!({"exchange":venue,"pair":pair})).await?;
            let data=&markets["data"];
            let count=match venue.as_str() {
                "kraken" => data["result"].as_object().map(|m|m.len()),
                "hyperliquid" => data[0]["universe"].as_array().map(|m|m.len()),
                "okx" => data["data"].as_array().map(|m|m.len()),
                "coinbase" => data["products"].as_array().map(|m|m.len()),
                "bybit" => data["result"]["list"].as_array().map(|m|m.len()),
                _ => None,
            };
            Ok::<_,gofin_domain::GoFinError>(json!({"exchange":venue,"observed_at":gofin_domain::now(),"market_count":count,"pair":pair,"ticker":ticker["data"],"public_only":true,"status":"passed"}))
        }.await;
        match result {
            Ok(value) => println!("{value}"),
            Err(error) => {
                failed = true;
                println!(
                    "{}",
                    json!({"exchange":venue,"observed_at":gofin_domain::now(),"status":"failed","error":error,"public_only":true})
                );
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}
#[cfg(not(feature = "ccxt"))]
fn main() {
    eprintln!("Enable the selected exchange features for this explicit public-only smoke test.");
    std::process::exit(1);
}
