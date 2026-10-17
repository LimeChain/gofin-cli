//! Hyperliquid typed operations and independently verified financial effects.
pub mod account_mode;
pub mod catalog;
pub mod profile;
pub mod reads;
pub mod types;
pub use account_mode::{
    AccountMode, AccountModeState, AccountRole, CashWallet, RecipientAccountMode,
    read_account_mode, read_recipient_account_mode,
};
pub use profile::*;
pub type HyperliquidAdapter = gofin_exchange::ExchangeAdapter<HyperliquidProfile>;
pub mod account_actions;
pub mod amend;
pub mod fills;
pub mod funding;
pub mod identity;
mod lifecycle;
mod read_profile;
mod settlement;
pub mod twap;
pub mod writes;
