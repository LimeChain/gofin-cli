//! Protected Meteora DLMM and DAMM v2 adapter for Solana mainnet-beta.
//!
//! Protocol modules decode and construct protocol-specific candidates. The
//! shared adapter owns financial authority, frozen transaction dispatch,
//! durable recovery, and proof-based settlement.

pub mod adapter;
pub mod boundaries;
pub mod protocol;
pub mod settlement;
pub mod types;

pub mod damm_v2;
pub mod dlmm;
pub(crate) mod presentation;

pub use adapter::{MeteoraAdapter, SecondarySignerVault};
pub use protocol::{MeteoraAction, MeteoraIntent, MeteoraProtocol, ProtocolFamily};
pub use types::operation_definitions;

pub const NETWORK: &str = gofin_solana::NETWORK;
pub const CLUSTER: &str = gofin_solana::CLUSTER;
