//! Kraken-specific contracts, financial interpretation and verified effects.
//! Transport endpoints are a closed allowlist owned by gofin-exchange.
pub use gofin_exchange::kraken::{KrakenEndpoint, KrakenResponse};

pub mod administration;
pub mod contracts;
pub mod funding;
mod inputs;

mod derivative_batch;
mod derivative_edit;
mod derivative_fee_units;
mod derivatives;
mod feasibility;
mod margin;
mod margin_fee_admission;
mod observation;
mod permissions;
mod pricing;
mod profile;
mod trading;
pub use profile::{
    KrakenAccount, KrakenCcxtTransport, KrakenDispatch, KrakenProduct, KrakenProfile,
    KrakenTransport,
};

#[cfg(test)]
mod trading_tests;

#[cfg(test)]
mod derivatives_tests;

#[cfg(test)]
mod margin_tests;

#[cfg(test)]
mod credential_redaction_tests;
#[cfg(test)]
mod read_contract_tests;

#[cfg(test)]
mod control_tests;

mod derivative_groups;

mod conditional_sequences;
mod derivative_batch_edit;
mod derivative_compound;
mod derivative_grouped_edits;
mod derivative_unified;
mod mixed_batch_edit;
mod mixed_spot_margin;
#[cfg(test)]
mod mixed_spot_margin_tests;
mod original_identities;
mod spot_batch_edit;

#[cfg(test)]
mod mutation_contract_tests;

mod conditional_spot_edit;

mod unresolved_consumption;
