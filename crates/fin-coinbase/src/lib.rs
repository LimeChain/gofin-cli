//! Coinbase product profiles: Advanced Trade, Coinbase Exchange and
//! Coinbase International. The transport owns authentication and exact wire
//! bytes; this crate owns typed operation contracts and financial meaning.
pub use fin_exchange::coinbase::{
    CoinbaseApi, CoinbaseAuth, CoinbaseEndpoint, CoinbaseError, CoinbasePagination, CoinbaseQuery,
    CoinbaseRequest, CoinbaseResponse, CoinbaseRoute, CoinbaseTransport,
};

pub mod contracts;
pub mod funding;
pub mod institutional;
pub(crate) mod institutional_administration;
mod institutional_authority;
mod pricing;
mod profile;
mod reads;
mod snapshot;

pub use funding::{
    CoinbaseDestination, CoinbasePayoutRequest, CoinbasePayoutResult, CoinbaseRecipientVerifier,
};
pub use profile::{CoinbaseAccount, CoinbaseDispatch, CoinbaseProduct, CoinbaseProfile};

#[cfg(test)]
mod tests;
