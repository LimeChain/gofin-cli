//! Jupiter application adapter for exact Solana mainnet-beta operations.
//!
//! Spot swaps trust Jupiter transaction construction and check order limits.
//! Other protocols retain their instruction validation. Finalized chain effects
//! establish settlement for every integration.

pub mod adapter;
pub mod api;
pub mod boundaries;
pub mod jlp;
pub mod perps;
pub mod pricing;
mod spot_settlement;
pub mod swap;
pub mod trigger;

pub use adapter::JupiterAdapter;
pub use adapter::operation_definitions;
pub use api::{
    HttpJupiterApi, JupiterApi, JupiterHost, RateBucket, RequestPriority, valid_api_key,
};
pub use jlp::JlpAdapter;
pub use perps::PerpsAdapter;
pub use pricing::JupiterUsdPrices;
pub use swap::SwapIntent;
pub use trigger::TriggerAdapter;

pub const NETWORK: &str = gofin_solana::NETWORK;
