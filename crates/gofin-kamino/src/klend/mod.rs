//! Native collateral and debt operations for KLend obligations.
pub mod adapter;
pub mod boundaries;
pub(crate) mod math;
pub(crate) mod native;
pub mod protocol;
pub(crate) mod reads;
pub mod settlement;
pub mod types;
pub use adapter::KlendAdapter;
pub use protocol::NativeKlendProtocol;
pub use types::operation_definitions;
