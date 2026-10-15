//! Jupiter application adapter for exact Solana mainnet-beta operations.
//!
//! Provider routes are treated as candidate transactions. The adapter must
//! decode and bind every actual instruction and verify finalized chain effects
//! before returning a successful reconciliation.

pub mod adapter;
pub mod api;
pub mod boundaries;
pub(crate) mod dflow;
pub mod jlp;
pub mod perps;
pub mod pricing;
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
pub use swap::{
    DecodedSwapInstruction, SwapInstructionVariant, SwapIntent, SwapRoute, decode_swap_instruction,
};
pub use trigger::TriggerAdapter;

pub const NETWORK: &str = gofin_solana::NETWORK;
