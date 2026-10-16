//! Solana mainnet-beta runtime primitives and wallet adapter.
//!
//! This crate deliberately accepts only the full mainnet genesis identity and
//! never treats an RPC submission response as settlement evidence. Application
//! adapters remain responsible for decoding and validating the financial
//! meaning of every instruction before they freeze a transaction.

pub mod funding;
pub mod key;
pub mod rpc;
pub mod token;
pub mod transaction;
pub mod wallet;

pub use funding::{
    AmountBounds, ExternalPayoutEvidence, ExternalPayoutRequest, ExternalPayoutStatus,
};
pub use key::SolanaSigner;
pub use rpc::{HttpRpc, RpcReadRequirements, SolanaRpc};
pub use transaction::{
    AsyncDispatchGuard, FinalizedReceiptProof, FrozenTransaction, NormalizedInstruction,
    ReceiptOutcome, SimulatedAccountInfo, SimulationWithAccountsResult, SolanaRuntime,
    TransactionSender,
};
pub use wallet::{SolanaWalletAdapter, operation_definitions};

pub const NETWORK: &str = "solana:mainnet-beta";
pub const CLUSTER: &str = "mainnet-beta";
pub const MAINNET_GENESIS_HASH: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";

/// Parse and canonicalize one exact 32-byte base58 Solana address.
pub fn parse_address(value: &str) -> gofin_domain::Result<String> {
    if value.len() > 44 {
        return Err(gofin_domain::GoFinError::validation(
            "A valid 32-byte base58 Solana address is required.",
        ));
    }
    let mut decoded = zeroize::Zeroizing::new([0u8; 32]);
    let written = bs58::decode(value).onto(decoded.as_mut()).map_err(|_| {
        gofin_domain::GoFinError::validation("A valid 32-byte base58 Solana address is required.")
    })?;
    if written != 32 || bs58::encode(decoded.as_ref()).into_string() != value {
        return Err(gofin_domain::GoFinError::validation(
            "A valid 32-byte base58 Solana address is required.",
        ));
    }
    Ok(value.to_owned())
}

/// Derive an associated token address for either supported token program.
pub fn associated_token_address(
    owner: &str,
    mint: &str,
    token_program: &str,
) -> gofin_domain::Result<String> {
    let owner = parse_address(owner)?;
    let mint = parse_address(mint)?;
    let token_program = parse_address(token_program)?;
    let owner = owner
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| parse_error())?;
    let mint = mint
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| parse_error())?;
    let token_program = token_program
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| parse_error())?;
    Ok(spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(&owner, &mint, &token_program).to_string())
}

pub(crate) fn failure(
    code: &'static str,
    stage: &'static str,
    message: &'static str,
    next: &'static str,
) -> gofin_domain::GoFinError {
    gofin_domain::GoFinError::new(code, stage, message, next)
}

pub(crate) fn parse_error() -> gofin_domain::GoFinError {
    failure(
        "SOLANA_RESPONSE_INVALID",
        "rpc",
        "The Solana node returned an invalid response.",
        "Retry the read or use a trusted mainnet-beta RPC endpoint.",
    )
}
