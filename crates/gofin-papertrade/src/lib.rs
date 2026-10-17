//! Typed Papertrade protocol support. Signing and submission stay behind the
//! broker-provided runtime; this crate has no generic signing or endpoint API.
mod adapter;
mod proof;
mod protocol;

pub use adapter::*;
pub use proof::*;
pub use protocol::*;
