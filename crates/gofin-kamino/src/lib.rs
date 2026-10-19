//! Native Kamino vault operations. Swaps remain separate named-plan operations.

pub mod adapter;
pub mod boundaries;
pub mod common;
pub mod farms;
pub mod klend;
pub mod lending;
pub mod liquidity;
mod liquidity_idl;
pub mod protocol;
pub mod settlement;
pub mod types;

pub use adapter::KaminoAdapter;
pub use types::operation_definitions;

pub const NETWORK: &str = gofin_solana::NETWORK;
pub const CLUSTER: &str = gofin_solana::CLUSTER;
