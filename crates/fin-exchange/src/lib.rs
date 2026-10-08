//! Shared exchange lifecycle. Profiles interpret venue evidence; the shared
//! adapter enforces operation boundaries, revalidation and journal-before-write.
mod lifecycle;
pub use lifecycle::{ExchangeAdapter, ExchangeProfile, PlannedSubmission};

#[cfg(feature = "ccxt")]
pub mod ccxt;
mod default_profile;
pub use default_profile::{DefaultExchangeProfile, PublicExchangeData};
mod order_book;
