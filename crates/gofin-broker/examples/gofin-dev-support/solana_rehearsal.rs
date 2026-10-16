//! Explicit, in-memory Solana rehearsal transports for the development broker.
//! These fixtures are reachable only through `DEV_MODE=synthetic` and
//! `DEV_FIXTURE=solana`; they never construct an HTTP client.
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bincode::Options;
use gofin_broker::{
    BrokerConfigV2, BrokerCredentialProvider, BrokerPolicy, ConnectionConfig, DeploymentConfig,
    NetworkConfig,
};
use gofin_domain::{AssetId, GoFinError, ResourceId, Result};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use gofin_solana::{MAINNET_GENESIS_HASH, SolanaRpc, SolanaSigner};
use reqwest::Method;
use serde_json::{Value, json};
use solana_message::VersionedMessage;
use solana_transaction::versioned::VersionedTransaction;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

#[cfg(test)]
#[path = "solana_rehearsal/baseline_workflow_tests.rs"]
mod baseline_workflow_tests;
#[path = "solana_rehearsal/jlp.rs"]
mod jlp;
#[path = "meteora_rehearsal.rs"]
mod meteora_rehearsal;
#[path = "solana_rehearsal/perps.rs"]
mod perps;
#[cfg(test)]
#[path = "solana_rehearsal/slippage_workflow_tests.rs"]
mod slippage_workflow_tests;
#[path = "solana_rehearsal/trigger.rs"]
mod trigger;

// The rehearsal adapters share process-global synthetic fixture state. Keep
// every example-level test serialized against the same lock, including tests
// defined in child modules.
#[cfg(test)]
static FIXTURE_LOCK: Mutex<()> = Mutex::new(());

// Seed 0x07 repeated is intentionally public, synthetic test material. It is
// compiled only into the development example, never the installed broker.
const SYNTHETIC_KEYPAIR: &str =
    "99eUso3aSbE9tqGSTXzo3TLfKb9RkMTURrHKQ1K7Zh3StnzFNUx8FKCPPPPpR479qsw5zv2WNBKmgiz7WqgAJfM";
const SWAP_INPUT_MINT: &str = "So11111111111111111111111111111111111111112";
const SWAP_OUTPUT_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const PYUSD_MINT: &str = "2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo";
const ASSOCIATED_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const WSOL_ACCOUNT_RENT_LAMPORTS: u64 = 2_039_280;
const ROUTE_V2_DISCRIMINATOR: [u8; 8] = [187, 100, 250, 204, 49, 196, 175, 20];
const SHARED_ROUTE_V2_DISCRIMINATOR: [u8; 8] = [209, 152, 83, 147, 124, 254, 216, 233];
const ORDER_ENGINE_PROGRAM: &str = "61DFfeTKM7trxYcPQCM78bJ794ddZprZpAwAnLiwTpYH";
const ORDER_ENGINE_FILL_DISCRIMINATOR: [u8; 8] = [168, 96, 183, 163, 92, 10, 40, 160];
const JUPITERZ_SYNTHETIC_MAKER: &str = "Gymcte79ALGtFrWybRxqb7M7uRjnbjgaHjGgQf1v4qB8";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FixtureSwapLayout {
    Legacy,
    RouteV2,
    SharedAccountsRouteV2,
    JupiterZ,
    JupiterZNativeInput,
    JupiterZNativeOutput,
}

impl FixtureSwapLayout {
    fn is_jupiterz(self) -> bool {
        matches!(
            self,
            Self::JupiterZ | Self::JupiterZNativeInput | Self::JupiterZNativeOutput
        )
    }

    fn selected(path: &str, amount: u64, input_mint: &str, output_mint: &str) -> Self {
        match std::env::var("GOFIN_DEV_SOLANA_FIXTURE_JUPITER_LAYOUT").as_deref() {
            Ok("route_v2") => Self::RouteV2,
            Ok("shared_accounts_route_v2") => Self::SharedAccountsRouteV2,
            Ok("auto_spot_v2")
                if amount == 20_000_000
                    && input_mint == SWAP_INPUT_MINT
                    && output_mint == SWAP_OUTPUT_MINT =>
            {
                if path.starts_with("/swap/v2/build?") {
                    Self::RouteV2
                } else if path.starts_with("/swap/v2/order?") {
                    Self::SharedAccountsRouteV2
                } else {
                    Self::Legacy
                }
            }
            _ => Self::Legacy,
        }
    }
}

// Settlement uses the fixture's pinned IDL offsets and scenario values, so a
// production decoder regression cannot also redefine what the fake RPC accepts.
fn fixture_swap_fields(data: &[u8]) -> Option<(u64, u64, usize, usize)> {
    if data.starts_with(&ROUTE_V2_DISCRIMINATOR) {
        Some((
            u64::from_le_bytes(data.get(8..16)?.try_into().ok()?),
            u64::from_le_bytes(data.get(16..24)?.try_into().ok()?),
            1,
            2,
        ))
    } else if data.starts_with(&SHARED_ROUTE_V2_DISCRIMINATOR) {
        Some((
            u64::from_le_bytes(data.get(9..17)?.try_into().ok()?),
            u64::from_le_bytes(data.get(17..25)?.try_into().ok()?),
            2,
            5,
        ))
    } else {
        None
    }
}

pub fn applications() -> Result<(BrokerConfigV2, gofin_broker::ProtectedApplications)> {
    let (config, applications, _, _) = applications_with_fixture()?;
    Ok((config, applications))
}

fn applications_with_fixture() -> Result<(
    BrokerConfigV2,
    gofin_broker::ProtectedApplications,
    Arc<FixtureRpc>,
    Arc<FixtureJupiterApi>,
)> {
    let signer = Arc::new(SolanaSigner::parse(SYNTHETIC_KEYPAIR)?);
    let credential_ref = "00000000-0000-4000-8000-000000000007";
    let mut config = BrokerConfigV2 {
        version: 2,
        networks: Vec::<NetworkConfig>::new(),
        deployments: Vec::<DeploymentConfig>::new(),
        connections: vec![ConnectionConfig::Solana {
            name: "solana-rehearsal".into(),
            resource: ResourceId(format!("solana:mainnet-beta:{}", signer.address())),
            address: signer.address().to_owned(),
            credential_ref: Some(credential_ref.into()),
            signing_enabled: true,
            network: "solana:mainnet-beta".into(),
        }],
        policy: BrokerPolicy::beta(),
        approver_uid: unsafe { libc::geteuid() },
        extensions: Default::default(),
    };
    if config.approver_uid < 501 {
        config.approver_uid = 501;
    }
    let credentials: Arc<dyn BrokerCredentialProvider> = Arc::new(FixtureCredentials {
        reference: credential_ref,
        signer: signer.clone(),
        auxiliary_signers: Mutex::new(BTreeMap::new()),
    });
    let fixture_rpc = Arc::new(FixtureRpc::new(signer.address())?);
    let rpc: Arc<dyn SolanaRpc> = fixture_rpc.clone();
    let fixture_api = Arc::new(FixtureJupiterApi {
        wallet: signer.address().to_owned(),
        rpc: fixture_rpc.clone(),
    });
    let api: Arc<dyn gofin_jupiter::JupiterApi> = fixture_api.clone();
    let prices: Arc<dyn UsdPriceSource> = Arc::new(FixturePrices);
    let applications = gofin_broker::build_applications_with_network_transports(
        &config,
        credentials,
        prices,
        Default::default(),
        Some(rpc),
        Some(api),
    )?;
    Ok((config, applications, fixture_rpc, fixture_api))
}

struct FixtureCredentials {
    reference: &'static str,
    signer: Arc<SolanaSigner>,
    auxiliary_signers: Mutex<BTreeMap<String, Arc<SolanaSigner>>>,
}

impl gofin_exchange::ccxt::CredentialProvider for FixtureCredentials {
    fn credentials(
        &self,
        _reference: &str,
    ) -> std::result::Result<gofin_exchange::ccxt::Credentials, gofin_exchange::ccxt::CcxtError>
    {
        Err(gofin_exchange::ccxt::CcxtError::new(
            gofin_exchange::ccxt::CcxtErrorKind::Unsupported,
        ))
    }
}

impl BrokerCredentialProvider for FixtureCredentials {
    fn generated_solana_signer(
        &self,
        credential: &gofin_credentials::CredentialRef,
    ) -> Result<Arc<SolanaSigner>> {
        self.auxiliary_signers
            .lock()
            .map_err(|_| GoFinError::validation("Synthetic signer fixture is unavailable."))?
            .get(credential.as_str())
            .cloned()
            .ok_or_else(|| GoFinError::validation("Synthetic auxiliary signer was not created."))
    }

    fn create_solana_auxiliary_signer(
        &self,
        operation_key: &str,
        owner_address: &str,
        purpose: &str,
    ) -> Result<(gofin_credentials::CredentialRef, Arc<SolanaSigner>)> {
        use sha2::{Digest, Sha256};

        let operation = uuid::Uuid::parse_str(operation_key).map_err(|_| {
            GoFinError::validation("Invalid synthetic auxiliary signer operation identity.")
        })?;
        if operation.hyphenated().to_string() != operation_key
            || owner_address != self.signer.address()
            || !matches!(
                purpose,
                "meteora_position_nft_mint" | "meteora_temporary_wsol"
            )
        {
            return Err(GoFinError::validation(
                "Synthetic auxiliary signer identity is outside the requested rehearsal scope.",
            ));
        }
        let identity = format!("fin:synthetic:aux:{operation_key}:{owner_address}:{purpose}");
        let digest = Sha256::digest(identity.as_bytes());
        let mut id_bytes = [0u8; 16];
        id_bytes.copy_from_slice(&digest[..16]);
        id_bytes[6] = (id_bytes[6] & 0x0f) | 0x40;
        id_bytes[8] = (id_bytes[8] & 0x3f) | 0x80;
        let reference = gofin_credentials::CredentialRef::new(
            &uuid::Uuid::from_bytes(id_bytes).hyphenated().to_string(),
        )?;
        let seed: [u8; 32] = Sha256::digest(format!("{identity}:seed").as_bytes()).into();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed);
        let mut keypair = [0u8; 64];
        keypair[..32].copy_from_slice(&seed);
        keypair[32..].copy_from_slice(&signing_key.verifying_key().to_bytes());
        let encoded = serde_json::to_string(&keypair.to_vec()).map_err(|_| {
            GoFinError::validation("Synthetic auxiliary signer could not be encoded.")
        })?;
        let signer = Arc::new(SolanaSigner::parse(&encoded)?);
        let mut signers = self
            .auxiliary_signers
            .lock()
            .map_err(|_| GoFinError::validation("Synthetic signer fixture is unavailable."))?;
        if let Some(existing) = signers.get(reference.as_str()) {
            return Ok((reference, existing.clone()));
        }
        signers.insert(reference.as_str().to_owned(), signer.clone());
        Ok((reference, signer))
    }

    fn envelope_signer(
        &self,
        _credential: &gofin_credentials::CredentialRef,
    ) -> Result<(String, Arc<dyn gofin_evm::EnvelopeSigner>)> {
        Err(GoFinError::validation(
            "The Solana rehearsal fixture has no EVM signer.",
        ))
    }

    fn solana_signer(
        &self,
        credential: &gofin_credentials::CredentialRef,
    ) -> Result<Arc<SolanaSigner>> {
        if credential.as_str() != self.reference {
            return Err(GoFinError::validation(
                "The synthetic signer reference is invalid.",
            ));
        }
        Ok(self.signer.clone())
    }
}

struct FixturePrices;

#[async_trait]
impl UsdPriceSource for FixturePrices {
    async fn quote(&self, asset: &AssetId) -> Result<UsdQuote> {
        Ok(UsdQuote {
            usd_per_unit: if asset.reference == "native" {
                "100"
            } else {
                "1"
            }
            .into(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            observed_at: chrono::Utc::now().to_rfc3339(),
            source: "synthetic Solana rehearsal fixture".into(),
        })
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct StoredTransaction {
    signature: String,
    wire: String,
    pre_balances: Vec<u64>,
    post_balances: Vec<u64>,
    pre_token_balances: Vec<Value>,
    post_token_balances: Vec<Value>,
    logs: Vec<String>,
    #[serde(default)]
    inner_instructions: Vec<Value>,
}

fn synthetic_fee_for_message(encoded: &str) -> Result<u64> {
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| GoFinError::validation("The synthetic fee message is malformed."))?;
    let message: VersionedMessage = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .deserialize(&bytes)
        .map_err(|_| GoFinError::validation("The synthetic fee message is malformed."))?;
    synthetic_fee_for_versioned_message(&message)
}

fn synthetic_fee_for_versioned_message(message: &VersionedMessage) -> Result<u64> {
    // The synthetic JupiterZ maker is the only fee payer in this fixture. The
    // enrolled wallet also signs the Fill, but the rehearsal models a single
    // sponsor-paid network fee and records that debit against the maker.
    let signatures = if message
        .static_account_keys()
        .first()
        .is_some_and(|key| key.to_string() == JUPITERZ_SYNTHETIC_MAKER)
    {
        1
    } else {
        message.header().num_required_signatures
    };
    u64::from(signatures)
        .checked_mul(5_000)
        .ok_or_else(|| GoFinError::validation("The synthetic network fee overflowed."))
}

fn synthetic_fee_for_transaction(encoded: &str) -> Result<u64> {
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| GoFinError::validation("The synthetic transaction is malformed."))?;
    let transaction: VersionedTransaction = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .deserialize(&bytes)
        .map_err(|_| GoFinError::validation("The synthetic transaction is malformed."))?;
    synthetic_fee_for_versioned_message(&transaction.message)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct FixtureState {
    wallet: String,
    lamports: u64,
    #[serde(default = "default_sponsor_lamports")]
    sponsor_lamports: u64,
    #[serde(default)]
    blockhash_counter: u64,
    transactions: Vec<StoredTransaction>,
    account_data: BTreeMap<String, (String, Vec<u8>)>,
    token_amounts: BTreeMap<String, u64>,
    lost_response_consumed: bool,
    #[serde(default)]
    lost_router_response_consumed: bool,
    #[serde(default)]
    lost_meta_response_consumed: bool,
    #[serde(default)]
    perps_state: Option<Value>,
    #[serde(default)]
    perps_getblock_fault: Option<String>,
    #[serde(default)]
    rpc_method_counts: BTreeMap<String, u64>,
    #[serde(default)]
    get_account_info_addresses: Vec<String>,
    #[serde(default)]
    expiry_candidate_count: u64,
    #[serde(default)]
    expiry_candidate_height_checks: u64,
    #[serde(skip)]
    meta_execution_in_progress: bool,
}

fn default_sponsor_lamports() -> u64 {
    1_000_000_000
}

struct FixtureRpc(Mutex<FixtureState>);

fn fixture_state_path() -> Result<Option<PathBuf>> {
    let Some(raw_path) = std::env::var_os("GOFIN_DEV_SOLANA_FIXTURE_STATE") else {
        return Ok(None);
    };
    if std::env::var("GOFIN_DEV_MODE").ok().as_deref() != Some("synthetic") {
        return Err(fixture_state_error());
    }
    let root = std::env::var_os("GOFIN_DEV_STATE_HOME")
        .map(PathBuf::from)
        .ok_or_else(fixture_state_error)?;
    let path = PathBuf::from(raw_path);
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        || !path.starts_with(&root)
    {
        return Err(fixture_state_error());
    }
    std::fs::create_dir_all(path.parent().ok_or_else(fixture_state_error)?)
        .map_err(|_| fixture_state_error())?;
    Ok(Some(path))
}

fn fixture_state_error() -> GoFinError {
    GoFinError::new(
        "SOLANA_FIXTURE_STATE_INVALID",
        "fixture",
        "The isolated synthetic Solana ledger file is missing, invalid, or outside its state directory.",
        "Use a fresh disposable synthetic fixture environment.",
    )
}

fn transaction_amount_matches(
    transactions: &[StoredTransaction],
    signature: &str,
    amount: u64,
) -> bool {
    let Some(stored) = transactions
        .iter()
        .find(|transaction| transaction.signature == signature)
    else {
        return false;
    };
    let Ok(bytes) = STANDARD.decode(&stored.wire) else {
        return false;
    };
    let Ok(transaction) = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .deserialize::<VersionedTransaction>(&bytes)
    else {
        return false;
    };
    let keys = transaction.message.static_account_keys();
    transaction
        .message
        .instructions()
        .iter()
        .any(|instruction| {
            instruction.data.len() == 12
                && u32::from_le_bytes(instruction.data[..4].try_into().unwrap_or_default()) == 2
                && u64::from_le_bytes(instruction.data[4..12].try_into().unwrap_or_default())
                    == amount
                && keys
                    .get(usize::from(instruction.program_id_index))
                    .is_some_and(|key| key.to_string() == "11111111111111111111111111111111")
        })
        && !transaction
            .message
            .instructions()
            .iter()
            .any(|instruction| {
                instruction.data == [17]
                    && keys
                        .get(usize::from(instruction.program_id_index))
                        .is_some_and(|key| key.to_string() == gofin_solana::token::TOKEN_PROGRAM_ID)
            })
}

fn transaction_uses_jupiter_v6(transactions: &[StoredTransaction], signature: &str) -> bool {
    let Some(stored) = transactions
        .iter()
        .find(|transaction| transaction.signature == signature)
    else {
        return false;
    };
    let Ok(bytes) = STANDARD.decode(&stored.wire) else {
        return false;
    };
    let Ok(transaction) = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .deserialize::<VersionedTransaction>(&bytes)
    else {
        return false;
    };
    transaction
        .message
        .instructions()
        .iter()
        .any(|instruction| {
            transaction
                .message
                .static_account_keys()
                .get(usize::from(instruction.program_id_index))
                .is_some_and(|key| key.to_string() == gofin_jupiter::swap::JUPITER_V6_PROGRAM)
        })
}

fn transaction_swap_matches(
    transactions: &[StoredTransaction],
    signature: &str,
    wallet: &str,
    amount: u64,
    input_mint: &str,
    output_mint: &str,
) -> bool {
    let Some(stored) = transactions
        .iter()
        .find(|transaction| transaction.signature == signature)
    else {
        return false;
    };
    let Ok(bytes) = STANDARD.decode(&stored.wire) else {
        return false;
    };
    let Ok(transaction) = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .deserialize::<VersionedTransaction>(&bytes)
    else {
        return false;
    };
    let keys = transaction.message.static_account_keys();
    transaction
        .message
        .instructions()
        .iter()
        .any(|instruction| {
            let Some(program) = keys.get(usize::from(instruction.program_id_index)) else {
                return false;
            };
            let program_id = program.to_string();
            if matches!(
                program_id.as_str(),
                "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo"
                    | "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG"
            ) {
                let (source_slot, destination_slot) =
                    if program_id == "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo" {
                        (4, 5)
                    } else {
                        (2, 3)
                    };
                let actual_amount = instruction
                    .data
                    .get(8..16)
                    .and_then(|bytes| bytes.try_into().ok())
                    .map(u64::from_le_bytes);
                let Some(source_index) = instruction
                    .accounts
                    .get(source_slot)
                    .map(|index| usize::from(*index))
                else {
                    return false;
                };
                let Some(destination_index) = instruction
                    .accounts
                    .get(destination_slot)
                    .map(|index| usize::from(*index))
                else {
                    return false;
                };
                let Some(source) = keys.get(source_index) else {
                    return false;
                };
                let Some(destination) = keys.get(destination_index) else {
                    return false;
                };
                let is_temporary_wsol = |account: &str| {
                    transaction.message.instructions().iter().any(|candidate| {
                        keys.get(usize::from(candidate.program_id_index))
                            .is_some_and(|id| {
                                id.to_string() == gofin_solana::token::TOKEN_PROGRAM_ID
                            })
                            && candidate.data == [18]
                            && candidate
                                .accounts
                                .first()
                                .and_then(|index| keys.get(usize::from(*index)))
                                .is_some_and(|key| key.to_string() == account)
                            && candidate
                                .accounts
                                .get(1)
                                .and_then(|index| keys.get(usize::from(*index)))
                                .is_some_and(|key| key.to_string() == SWAP_INPUT_MINT)
                    })
                };
                let account_mint = |index: usize| {
                    stored
                        .pre_token_balances
                        .iter()
                        .chain(stored.post_token_balances.iter())
                        .find(|row| {
                            row.get("accountIndex").and_then(Value::as_u64) == Some(index as u64)
                        })
                        .and_then(|row| row.get("mint"))
                        .and_then(Value::as_str)
                };
                let actual_input = if is_temporary_wsol(&source.to_string()) {
                    Some(SWAP_INPUT_MINT)
                } else {
                    account_mint(source_index)
                };
                let actual_output = if is_temporary_wsol(&destination.to_string()) {
                    Some(SWAP_INPUT_MINT)
                } else {
                    account_mint(destination_index)
                };
                return actual_amount == Some(amount)
                    && actual_input == Some(input_mint)
                    && actual_output == Some(output_mint);
            }
            if program.to_string() != gofin_jupiter::swap::JUPITER_V6_PROGRAM {
                return false;
            }
            let (actual_input, actual_output, source_index, destination_index) =
                if let Some((input, _, source, destination)) =
                    fixture_swap_fields(&instruction.data)
                {
                    let (_, output, _, _) = fixture_swap_fields(&instruction.data).unwrap();
                    (input, output, source, destination)
                } else {
                    let Ok(decoded) =
                        gofin_jupiter::swap::decode_swap_instruction(&instruction.data)
                    else {
                        return false;
                    };
                    let shared = matches!(
                        decoded.variant,
                        gofin_jupiter::swap::SwapInstructionVariant::SharedExactIn
                            | gofin_jupiter::swap::SwapInstructionVariant::SharedExactOut
                            | gofin_jupiter::swap::SwapInstructionVariant::SharedTokenLedgerExactIn
                    );
                    (
                        decoded.input_amount.unwrap_or_default(),
                        decoded.quoted_output,
                        if shared { 3 } else { 2 },
                        if shared { 6 } else { 3 },
                    )
                };
            if actual_input != amount
                || swap_output_amount(amount, input_mint, output_mint).ok() != Some(actual_output)
            {
                return false;
            }
            let Some(source_index) = instruction
                .accounts
                .get(source_index)
                .and_then(|index| keys.get(usize::from(*index)))
            else {
                return false;
            };
            let Some(destination_index) = instruction
                .accounts
                .get(destination_index)
                .and_then(|index| keys.get(usize::from(*index)))
            else {
                return false;
            };
            let source = gofin_solana::associated_token_address(
                wallet,
                input_mint,
                mint_profile(input_mint).0,
            );
            let destination = gofin_solana::associated_token_address(
                wallet,
                output_mint,
                mint_profile(output_mint).0,
            );
            matches!((source, destination), (Ok(source), Ok(destination))
            if source_index.to_string() == source && destination_index.to_string() == destination)
        })
}

fn mint_data(decimals: u8) -> Vec<u8> {
    let mut data = vec![0u8; 82];
    data[44] = decimals;
    data[45] = 1;
    data
}

fn token_account_data(mint: &str, owner: &str, amount: u64) -> Vec<u8> {
    let mint = bs58::decode(mint).into_vec().expect("fixed synthetic mint");
    let owner = bs58::decode(owner).into_vec().expect("synthetic wallet");
    let mut data = vec![0u8; 165];
    data[..32].copy_from_slice(&mint);
    data[32..64].copy_from_slice(&owner);
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1;
    data
}

fn mint_profile(mint: &str) -> (&'static str, u8, u64) {
    match mint {
        SWAP_INPUT_MINT => (gofin_solana::token::TOKEN_PROGRAM_ID, 9, 100),
        PYUSD_MINT => (gofin_solana::token::TOKEN_2022_PROGRAM_ID, 6, 1),
        "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs" => {
            (gofin_solana::token::TOKEN_PROGRAM_ID, 9, 2_000)
        }
        "3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh" => {
            (gofin_solana::token::TOKEN_PROGRAM_ID, 8, 50_000)
        }
        "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB" => {
            (gofin_solana::token::TOKEN_PROGRAM_ID, 6, 1)
        }
        _ => (gofin_solana::token::TOKEN_PROGRAM_ID, 6, 1),
    }
}

fn swap_output_amount(input: u64, input_mint: &str, output_mint: &str) -> Result<u64> {
    let (_, input_decimals, input_price) = mint_profile(input_mint);
    let (_, output_decimals, output_price) = mint_profile(output_mint);
    let numerator = u128::from(input)
        .checked_mul(u128::from(input_price))
        .and_then(|value| value.checked_mul(10u128.pow(u32::from(output_decimals))))
        .ok_or_else(|| GoFinError::validation("The synthetic swap output overflows."))?;
    let denominator = 10u128
        .pow(u32::from(input_decimals))
        .checked_mul(u128::from(output_price))
        .ok_or_else(|| GoFinError::validation("The synthetic swap output overflows."))?;
    u64::try_from(numerator / denominator)
        .map_err(|_| GoFinError::validation("The synthetic swap output exceeds u64."))
}

fn create_wsol_ata_instruction(
    wallet: &solana_pubkey::Pubkey,
    account: &solana_pubkey::Pubkey,
) -> Result<solana_instruction::Instruction> {
    use solana_instruction::{AccountMeta, Instruction};
    use solana_pubkey::Pubkey;

    Ok(Instruction {
        program_id: ASSOCIATED_TOKEN_PROGRAM
            .parse::<Pubkey>()
            .map_err(|_| GoFinError::validation("The associated-token program is invalid."))?,
        accounts: vec![
            AccountMeta::new(*wallet, true),
            AccountMeta::new(*account, false),
            AccountMeta::new_readonly(*wallet, false),
            AccountMeta::new_readonly(
                SWAP_INPUT_MINT
                    .parse::<Pubkey>()
                    .map_err(|_| GoFinError::validation("The wrapped-SOL mint is invalid."))?,
                false,
            ),
            AccountMeta::new_readonly(
                "11111111111111111111111111111111"
                    .parse::<Pubkey>()
                    .map_err(|_| GoFinError::validation("The system program is invalid."))?,
                false,
            ),
            AccountMeta::new_readonly(
                gofin_solana::token::TOKEN_PROGRAM_ID
                    .parse::<Pubkey>()
                    .map_err(|_| GoFinError::validation("The SPL Token program is invalid."))?,
                false,
            ),
        ],
        data: vec![1],
    })
}

fn query_parameter<'a>(path: &'a str, name: &str) -> Option<&'a str> {
    path.split(['?', '&']).find_map(|part| {
        part.strip_prefix(name)
            .and_then(|value| value.strip_prefix('='))
    })
}

fn wrapped_token_account_data(owner: &str, amount: u64) -> Vec<u8> {
    let mut data = token_account_data(SWAP_INPUT_MINT, owner, amount);
    data[109] = 1;
    data[113..121].copy_from_slice(&2_039_280u64.to_le_bytes());
    data
}

fn account_mint(accounts: &BTreeMap<String, (String, Vec<u8>)>, address: &str) -> Result<String> {
    let (_, data) = accounts
        .get(address)
        .ok_or_else(|| GoFinError::validation("The synthetic token account is absent."))?;
    let bytes: [u8; 32] = data
        .get(..32)
        .ok_or_else(|| GoFinError::validation("The synthetic token account is malformed."))?
        .try_into()
        .map_err(|_| GoFinError::validation("The synthetic token account is malformed."))?;
    Ok(bs58::encode(bytes).into_string())
}

fn token_balance(
    index: usize,
    mint: &str,
    owner: &str,
    token_program: &str,
    decimals: u8,
    amount: u64,
) -> Value {
    // Solana RPC commonly returns both exact atoms and lossy display fields.
    // Include both so settlement tests ensure accounting uses `amount`, never
    // the floating-point UI representation.
    let scale = 10u64.pow(u32::from(decimals));
    let whole = amount / scale;
    let fraction = amount % scale;
    let ui_amount_string = if decimals == 0 {
        whole.to_string()
    } else {
        let fractional = format!("{fraction:0width$}", width = usize::from(decimals));
        let trimmed = fractional.trim_end_matches('0');
        if trimmed.is_empty() {
            whole.to_string()
        } else {
            format!("{whole}.{trimmed}")
        }
    };
    let ui_amount = ui_amount_string.parse::<f64>().unwrap_or_default();
    json!({"accountIndex":index,"mint":mint,"owner":owner,"programId":token_program,"uiTokenAmount":{"amount":amount.to_string(),"decimals":decimals,"uiAmount":ui_amount,"uiAmountString":ui_amount_string}})
}

impl FixtureRpc {
    fn begin_expiry_candidate(&self) -> Result<()> {
        if std::env::var_os("GOFIN_DEV_SOLANA_FIXTURE_EXPIRY_MODE").is_none() {
            return Ok(());
        }
        let mut state = self.0.lock().map_err(|_| fixture_state_error())?;
        state.expiry_candidate_count = state.expiry_candidate_count.saturating_add(1);
        state.expiry_candidate_height_checks = 0;
        self.persist_state(&state)
    }

    fn remove_wsol_ata_when_requested(&self) -> Result<()> {
        let Some(path) = std::env::var_os("GOFIN_DEV_SOLANA_FIXTURE_REMOVE_WSOL_ATA_FILE") else {
            return Ok(());
        };
        let path = std::path::PathBuf::from(path);
        if !path.exists() {
            return Ok(());
        }
        let mut state = self.0.lock().map_err(|_| fixture_state_error())?;
        let address = gofin_solana::associated_token_address(
            &state.wallet,
            SWAP_INPUT_MINT,
            gofin_solana::token::TOKEN_PROGRAM_ID,
        )?;
        state.account_data.remove(&address);
        state.token_amounts.remove(&address);
        self.persist_state(&state)?;
        std::fs::remove_file(path).map_err(|_| fixture_state_error())?;
        Ok(())
    }

    fn take_next_meta_router(&self) -> Result<Option<String>> {
        let Some(path) = std::env::var_os("GOFIN_DEV_SOLANA_FIXTURE_NEXT_META_ROUTER_FILE") else {
            return Ok(None);
        };
        let path = PathBuf::from(path);
        let Ok(value) = std::fs::read_to_string(&path) else {
            return Ok(None);
        };
        std::fs::remove_file(&path).map_err(|_| fixture_state_error())?;
        let value = value.trim();
        if matches!(
            value,
            "metis" | "jupiterz" | "jupiterz_native_input" | "jupiterz_native_output" | "dflow"
        ) {
            Ok(Some(value.to_owned()))
        } else {
            Err(GoFinError::validation(
                "The synthetic Meta router selector must name a configured fixture engine.",
            ))
        }
    }

    pub(super) fn next_fixture_counter(&self) -> Result<u64> {
        let mut state = self.0.lock().map_err(|_| fixture_state_error())?;
        state.blockhash_counter = state
            .blockhash_counter
            .checked_add(1)
            .ok_or_else(fixture_state_error)?;
        let counter = state.blockhash_counter;
        self.persist_state(&state)?;
        Ok(counter)
    }

    pub(super) fn next_fixture_blockhash(&self) -> Result<solana_hash::Hash> {
        let counter = self.next_fixture_counter()?;
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&counter.to_le_bytes());
        Ok(solana_hash::Hash::new_from_array(bytes))
    }

    fn new(wallet: &str) -> Result<Self> {
        let input_account = gofin_solana::associated_token_address(
            wallet,
            SWAP_INPUT_MINT,
            gofin_solana::token::TOKEN_PROGRAM_ID,
        )
        .expect("fixed synthetic input ATA");
        let output_account = gofin_solana::associated_token_address(
            wallet,
            SWAP_OUTPUT_MINT,
            gofin_solana::token::TOKEN_PROGRAM_ID,
        )
        .expect("fixed synthetic output ATA");
        let pyusd_account = gofin_solana::associated_token_address(
            wallet,
            PYUSD_MINT,
            gofin_solana::token::TOKEN_2022_PROGRAM_ID,
        )
        .expect("fixed synthetic PYUSD ATA");
        // The shared local ledger must fund both ordinary spot rehearsals and
        // the sequential $11 Perps long/short lifecycle without mode-specific
        // account initializers or handler-order-dependent balances.
        let initial_usdc = 100_000_000;
        let jlp_account = gofin_solana::associated_token_address(
            wallet,
            gofin_jupiter::jlp::JLP_MINT,
            gofin_solana::token::TOKEN_PROGRAM_ID,
        )
        .expect("fixed synthetic JLP ATA");
        let mut account_data = BTreeMap::new();
        account_data.insert(
            SWAP_INPUT_MINT.into(),
            (gofin_solana::token::TOKEN_PROGRAM_ID.into(), mint_data(9)),
        );
        account_data.insert(
            SWAP_OUTPUT_MINT.into(),
            (gofin_solana::token::TOKEN_PROGRAM_ID.into(), mint_data(6)),
        );
        account_data.insert(
            PYUSD_MINT.into(),
            (
                gofin_solana::token::TOKEN_2022_PROGRAM_ID.into(),
                mint_data(6),
            ),
        );
        account_data.insert(
            gofin_jupiter::jlp::JLP_MINT.into(),
            (gofin_solana::token::TOKEN_PROGRAM_ID.into(), mint_data(6)),
        );
        account_data.insert(
            input_account.clone(),
            (
                gofin_solana::token::TOKEN_PROGRAM_ID.into(),
                wrapped_token_account_data(wallet, 0),
            ),
        );
        account_data.insert(
            output_account.clone(),
            (
                gofin_solana::token::TOKEN_PROGRAM_ID.into(),
                token_account_data(SWAP_OUTPUT_MINT, wallet, initial_usdc),
            ),
        );
        account_data.insert(
            pyusd_account.clone(),
            (
                gofin_solana::token::TOKEN_2022_PROGRAM_ID.into(),
                token_account_data(PYUSD_MINT, wallet, 4_000_000),
            ),
        );
        account_data.insert(
            jlp_account.clone(),
            (
                gofin_solana::token::TOKEN_PROGRAM_ID.into(),
                token_account_data(gofin_jupiter::jlp::JLP_MINT, wallet, 0),
            ),
        );
        let mut state = FixtureState {
            wallet: wallet.to_owned(),
            lamports: 2_000_000_000,
            sponsor_lamports: default_sponsor_lamports(),
            blockhash_counter: 0,
            transactions: Vec::new(),
            account_data,
            token_amounts: BTreeMap::from([
                (input_account, 0),
                (output_account, initial_usdc),
                (pyusd_account, 4_000_000),
                (jlp_account, 0),
            ]),
            lost_response_consumed: false,
            lost_router_response_consumed: false,
            lost_meta_response_consumed: false,
            perps_state: None,
            perps_getblock_fault: None,
            rpc_method_counts: BTreeMap::new(),
            get_account_info_addresses: Vec::new(),
            expiry_candidate_count: 0,
            expiry_candidate_height_checks: 0,
            meta_execution_in_progress: false,
        };
        // The public rehearsal enables this optional synthetic portfolio to
        // exercise wallet reads with many unrelated holdings. These stable
        // mint/account pairs are only present in that isolated fixture run.
        if let Ok(raw_count) = std::env::var("GOFIN_DEV_SOLANA_FIXTURE_EXTRA_MINTS") {
            let count = raw_count
                .parse::<usize>()
                .map_err(|_| GoFinError::validation("Invalid synthetic extra-mint count."))?;
            if count > 100 {
                return Err(GoFinError::validation(
                    "Synthetic extra-mint count exceeds the supported fixture limit.",
                ));
            }
            for index in 0..count {
                let mut mint_bytes = [0u8; 32];
                mint_bytes[0] = 0x80;
                mint_bytes[24..].copy_from_slice(&(index as u64 + 1).to_be_bytes());
                let mint = bs58::encode(mint_bytes).into_string();
                let token_account = gofin_solana::associated_token_address(
                    wallet,
                    &mint,
                    gofin_solana::token::TOKEN_PROGRAM_ID,
                )?;
                let amount = 1_000 + index as u64;
                state.account_data.insert(
                    mint.clone(),
                    (gofin_solana::token::TOKEN_PROGRAM_ID.into(), mint_data(6)),
                );
                state.account_data.insert(
                    token_account.clone(),
                    (
                        gofin_solana::token::TOKEN_PROGRAM_ID.into(),
                        token_account_data(&mint, wallet, amount),
                    ),
                );
                state.token_amounts.insert(token_account, amount);
            }
        }
        if std::env::var_os("GOFIN_DEV_METEORA_REHEARSAL").is_some() {
            meteora_rehearsal::initialize(&mut state)?;
        }
        if let Some(path) = fixture_state_path()? {
            if path.exists() {
                let metadata = std::fs::metadata(&path).map_err(|_| fixture_state_error())?;
                if metadata.len() > 16 * 1024 * 1024 {
                    return Err(fixture_state_error());
                }
                let bytes = std::fs::read(&path).map_err(|_| fixture_state_error())?;
                state = serde_json::from_slice(&bytes).map_err(|_| fixture_state_error())?;
                if state.wallet != wallet {
                    return Err(fixture_state_error());
                }
            } else {
                let rpc = Self(Mutex::new(state));
                {
                    let guard = rpc.0.lock().unwrap();
                    rpc.persist_state(&guard)?;
                }
                return Ok(rpc);
            }
        }
        Ok(Self(Mutex::new(state)))
    }

    pub(super) fn persist_state(&self, state: &FixtureState) -> Result<()> {
        let Some(path) = fixture_state_path()? else {
            return Ok(());
        };
        let bytes = serde_json::to_vec(state).map_err(|_| fixture_state_error())?;
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, bytes).map_err(|_| fixture_state_error())?;
        std::fs::rename(&temporary, &path).map_err(|_| fixture_state_error())
    }

    fn record_submission(&self, state: &mut FixtureState, signature: String) -> Result<String> {
        let lose_transfer_response =
            std::env::var("GOFIN_DEV_SOLANA_LOSE_RESPONSE_TRANSFER_LAMPORTS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .is_some_and(|amount| {
                    !state.lost_response_consumed
                        && transaction_amount_matches(&state.transactions, &signature, amount)
                });
        let lose_router_response =
            std::env::var("GOFIN_DEV_SOLANA_LOSE_RESPONSE_ROUTER_INPUT_ATOMS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .zip(std::env::var("GOFIN_DEV_SOLANA_LOSE_RESPONSE_ROUTER_INPUT_MINT").ok())
                .zip(std::env::var("GOFIN_DEV_SOLANA_LOSE_RESPONSE_ROUTER_OUTPUT_MINT").ok())
                .is_some_and(|((amount, input_mint), output_mint)| {
                    !state.meta_execution_in_progress
                        && !state.lost_router_response_consumed
                        && transaction_swap_matches(
                            &state.transactions,
                            &signature,
                            &state.wallet,
                            amount,
                            &input_mint,
                            &output_mint,
                        )
                });
        if lose_transfer_response {
            state.lost_response_consumed = true;
        }
        if lose_router_response {
            state.lost_router_response_consumed = true;
        }
        self.persist_state(state)?;
        if lose_transfer_response || lose_router_response {
            return Err(GoFinError::new(
                "SOLANA_RPC_UNAVAILABLE",
                "submit",
                "The synthetic RPC accepted the transaction but lost its send response.",
                "Reconcile the same operation identity after restart; do not resubmit it.",
            ));
        }
        Ok(signature)
    }

    fn lose_meta_execute_response(&self, signature: &str, request_id: &str) -> Result<()> {
        let selected = std::env::var("GOFIN_DEV_SOLANA_LOSE_RESPONSE_META_REQUEST_ID").ok();
        if !selected
            .as_deref()
            .is_some_and(|selector| meta_request_selector_matches(selector, request_id))
        {
            return Ok(());
        }
        let mut state = self.0.lock().unwrap();
        if state.lost_meta_response_consumed
            || !transaction_uses_jupiter_v6(&state.transactions, signature)
        {
            return Ok(());
        }
        state.lost_meta_response_consumed = true;
        self.persist_state(&state)?;
        Err(GoFinError::new(
            "SOLANA_RPC_UNAVAILABLE",
            "provider",
            "The synthetic aggregator accepted the transaction but lost its execute response.",
            "Reconcile the same operation identity after restart; do not resubmit it.",
        ))
    }

    fn submit(&self, encoded: &str) -> Result<String> {
        if let Some(signature) = meteora_rehearsal::submit(self, encoded)? {
            return Ok(signature);
        }
        if let Some(signature) = jlp::submit(self, encoded)? {
            return Ok(signature);
        }
        match perps::submit(self, encoded) {
            Ok(Some(signature)) => return Ok(signature),
            Ok(None) => {}
            Err(error) => {
                if std::env::var_os("GOFIN_DEV_SOLANA_FIXTURE_TRACE").is_some() {
                    eprintln!(
                        "synthetic Perps submit rejected: code={} message={}",
                        error.code, error.message
                    );
                }
                return Err(error);
            }
        }
        match trigger::submit(self, encoded) {
            Ok(Some(signature)) => return Ok(signature),
            Ok(None) => {}
            Err(error) => {
                if std::env::var_os("GOFIN_DEV_SOLANA_FIXTURE_TRACE").is_some() {
                    eprintln!(
                        "synthetic Trigger submit rejected: code={} message={}",
                        error.code, error.message
                    );
                }
                return Err(error);
            }
        }
        if encoded.len() > 256 * 1024 {
            return Err(GoFinError::validation(
                "The synthetic transaction is too large.",
            ));
        }
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| GoFinError::validation("The synthetic transaction is malformed."))?;
        let transaction: VersionedTransaction = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .reject_trailing_bytes()
            .with_limit(128 * 1024)
            .deserialize(&bytes)
            .map_err(|_| GoFinError::validation("The synthetic transaction is malformed."))?;
        transaction
            .sanitize()
            .map_err(|_| GoFinError::validation("The synthetic transaction is malformed."))?;
        let keys = transaction.message.static_account_keys();
        let instructions = transaction.message.instructions();
        if let Some(ix) = instructions.iter().find(|ix| {
            keys.get(usize::from(ix.program_id_index))
                .is_some_and(|key| {
                    key.to_string() == gofin_jupiter::swap::JUPITER_V6_PROGRAM
                        || key.to_string() == ORDER_ENGINE_PROGRAM
                })
        }) {
            let is_jupiterz = keys
                .get(usize::from(ix.program_id_index))
                .is_some_and(|key| key.to_string() == ORDER_ENGINE_PROGRAM);
            let decoded = (!is_jupiterz)
                .then(|| gofin_jupiter::swap::decode_swap_instruction(&ix.data).ok())
                .flatten();
            let v2_fields = fixture_swap_fields(&ix.data);
            let (amount, output, source_meta, destination_meta) = if is_jupiterz {
                if ix.data.len() != 37
                    || ix.data[..8] != ORDER_ENGINE_FILL_DISCRIMINATOR
                    || ix.data[32..] != [0, 0, 0, 0, 0]
                {
                    return Err(GoFinError::validation(
                        "The synthetic JupiterZ Fill instruction is malformed.",
                    ));
                }
                (
                    u64::from_le_bytes(ix.data[8..16].try_into().unwrap_or_default()),
                    u64::from_le_bytes(ix.data[16..24].try_into().unwrap_or_default()),
                    2,
                    4,
                )
            } else if let Some(fields) = v2_fields {
                fields
            } else {
                let decoded = decoded.as_ref().ok_or_else(|| {
                    GoFinError::validation("The synthetic swap instruction is malformed.")
                })?;
                let shared = matches!(
                    decoded.variant,
                    gofin_jupiter::swap::SwapInstructionVariant::SharedExactIn
                        | gofin_jupiter::swap::SwapInstructionVariant::SharedExactOut
                        | gofin_jupiter::swap::SwapInstructionVariant::SharedTokenLedgerExactIn
                );
                (
                    decoded.input_amount.unwrap_or_default(),
                    decoded.quoted_output,
                    if shared { 3 } else { 2 },
                    if shared { 6 } else { 3 },
                )
            };
            let source_index =
                usize::from(*ix.accounts.get(source_meta).ok_or_else(|| {
                    GoFinError::validation("The synthetic swap source is missing.")
                })?);
            let destination_index =
                usize::from(*ix.accounts.get(destination_meta).ok_or_else(|| {
                    GoFinError::validation("The synthetic swap destination is missing.")
                })?);
            let source = keys
                .get(source_index)
                .ok_or_else(|| GoFinError::validation("The synthetic swap source is missing."))?
                .to_string();
            let destination = keys
                .get(destination_index)
                .ok_or_else(|| {
                    GoFinError::validation("The synthetic swap destination is missing.")
                })?
                .to_string();
            let direct_native_input = is_jupiterz && source == ORDER_ENGINE_PROGRAM;
            let direct_native_output = is_jupiterz && destination == ORDER_ENGINE_PROGRAM;
            let creates_source_ata = instructions.iter().any(|setup| {
                setup.data == [1]
                    && keys
                        .get(usize::from(setup.program_id_index))
                        .is_some_and(|key| key.to_string() == ASSOCIATED_TOKEN_PROGRAM)
                    && setup
                        .accounts
                        .get(1)
                        .and_then(|index| keys.get(usize::from(*index)))
                        .is_some_and(|key| key.to_string() == source)
            });
            if !is_jupiterz
                && v2_fields.is_none()
                && !matches!(
                    decoded.as_ref().map(|item| item.variant),
                    Some(
                        gofin_jupiter::swap::SwapInstructionVariant::ExactIn
                            | gofin_jupiter::swap::SwapInstructionVariant::SharedExactIn
                    )
                )
            {
                return Err(GoFinError::validation(
                    "The synthetic swap instruction is not the reviewed exact-in layout.",
                ));
            }
            let mut state = self.0.lock().unwrap();
            let created_source_ata =
                creates_source_ata && !state.account_data.contains_key(&source);
            if creates_source_ata {
                let expected_source = gofin_solana::associated_token_address(
                    &state.wallet,
                    SWAP_INPUT_MINT,
                    gofin_solana::token::TOKEN_PROGRAM_ID,
                )?;
                if source != expected_source {
                    return Err(GoFinError::validation(
                        "The synthetic ATA creation does not target the wallet's wrapped-SOL account.",
                    ));
                }
                if created_source_ata {
                    let wallet_address = state.wallet.clone();
                    state.account_data.insert(
                        source.clone(),
                        (
                            gofin_solana::token::TOKEN_PROGRAM_ID.into(),
                            wrapped_token_account_data(&wallet_address, 0),
                        ),
                    );
                    state.token_amounts.insert(source.clone(), 0);
                }
            }
            // Native-input Jupiter transactions can create their wrapped-SOL ATA in
            // the same transaction, so validate the independently quoted output
            // after materializing that synthetic account in fixture state.
            let source_mint = if direct_native_input {
                keys.get(usize::from(*ix.accounts.get(6).ok_or_else(|| {
                    GoFinError::validation("The synthetic JupiterZ input mint is missing.")
                })?))
                .ok_or_else(|| {
                    GoFinError::validation("The synthetic JupiterZ input mint is missing.")
                })?
                .to_string()
            } else {
                account_mint(&state.account_data, &source)?
            };
            let creates_destination_ata = instructions.iter().any(|setup| {
                setup.data == [1]
                    && keys
                        .get(usize::from(setup.program_id_index))
                        .is_some_and(|key| key.to_string() == ASSOCIATED_TOKEN_PROGRAM)
                    && setup
                        .accounts
                        .get(1)
                        .and_then(|index| keys.get(usize::from(*index)))
                        .is_some_and(|key| key.to_string() == destination)
            });
            let created_destination_ata =
                creates_destination_ata && !state.account_data.contains_key(&destination);
            if creates_destination_ata {
                let setup = instructions
                    .iter()
                    .find(|setup| {
                        setup.data == [1]
                            && keys
                                .get(usize::from(setup.program_id_index))
                                .is_some_and(|key| key.to_string() == ASSOCIATED_TOKEN_PROGRAM)
                            && setup
                                .accounts
                                .get(1)
                                .and_then(|index| keys.get(usize::from(*index)))
                                .is_some_and(|key| key.to_string() == destination)
                    })
                    .ok_or_else(|| {
                        GoFinError::validation("The synthetic output ATA setup is missing.")
                    })?;
                let mint = setup
                    .accounts
                    .get(3)
                    .and_then(|index| keys.get(usize::from(*index)))
                    .ok_or_else(|| {
                        GoFinError::validation("The synthetic output ATA mint is missing.")
                    })?
                    .to_string();
                let token_program = setup
                    .accounts
                    .get(5)
                    .and_then(|index| keys.get(usize::from(*index)))
                    .ok_or_else(|| {
                        GoFinError::validation("The synthetic output ATA program is missing.")
                    })?
                    .to_string();
                let expected_destination =
                    gofin_solana::associated_token_address(&state.wallet, &mint, &token_program)?;
                if destination != expected_destination {
                    return Err(GoFinError::validation(
                        "The synthetic ATA creation does not target the wallet's exact output account.",
                    ));
                }
                if created_destination_ata {
                    let wallet_address = state.wallet.clone();
                    state.account_data.insert(
                        destination.clone(),
                        (
                            token_program.clone(),
                            token_account_data(&mint, &wallet_address, 0),
                        ),
                    );
                    state.token_amounts.insert(destination.clone(), 0);
                }
            }
            let destination_mint = if direct_native_output {
                keys.get(usize::from(*ix.accounts.get(8).ok_or_else(|| {
                    GoFinError::validation("The synthetic JupiterZ output mint is missing.")
                })?))
                .ok_or_else(|| {
                    GoFinError::validation("The synthetic JupiterZ output mint is missing.")
                })?
                .to_string()
            } else {
                account_mint(&state.account_data, &destination)?
            };
            if swap_output_amount(amount, &source_mint, &destination_mint)? != output {
                return Err(GoFinError::validation(
                    "The synthetic swap output is outside the fixture quote bound.",
                ));
            }
            let source_before = if direct_native_input {
                0
            } else {
                *state.token_amounts.get(&source).ok_or_else(|| {
                    GoFinError::validation("The synthetic swap source account is absent.")
                })?
            };
            let destination_before = if direct_native_output {
                0
            } else {
                *state.token_amounts.get(&destination).ok_or_else(|| {
                    GoFinError::validation("The synthetic swap destination account is absent.")
                })?
            };
            let native_input = direct_native_input
                || instructions.iter().any(|setup| {
                    let program = keys.get(usize::from(setup.program_id_index));
                    program.is_some_and(|key| key.to_string() == "11111111111111111111111111111111")
                        && setup.data.len() == 12
                        && u32::from_le_bytes(setup.data[..4].try_into().unwrap_or_default()) == 2
                        && setup
                            .accounts
                            .get(1)
                            .and_then(|index| keys.get(usize::from(*index)))
                            .is_some_and(|key| key.to_string() == source)
                });
            let native_output = direct_native_output
                || instructions.iter().any(|cleanup| {
                    cleanup.data == [9]
                        && keys
                            .get(usize::from(cleanup.program_id_index))
                            .is_some_and(|key| {
                                key.to_string() == gofin_solana::token::TOKEN_PROGRAM_ID
                            })
                        && cleanup
                            .accounts
                            .first()
                            .and_then(|index| keys.get(usize::from(*index)))
                            .is_some_and(|key| key.to_string() == destination)
                });
            let closes_source_ata = instructions.iter().any(|cleanup| {
                cleanup.data == [9]
                    && keys
                        .get(usize::from(cleanup.program_id_index))
                        .is_some_and(|key| key.to_string() == gofin_solana::token::TOKEN_PROGRAM_ID)
                    && cleanup
                        .accounts
                        .first()
                        .and_then(|index| keys.get(usize::from(*index)))
                        .is_some_and(|key| key.to_string() == source)
            });
            if (created_source_ata && !closes_source_ata)
                || (created_destination_ata
                    && destination_mint == SWAP_INPUT_MINT
                    && !native_output)
            {
                return Err(GoFinError::validation(
                    "The synthetic temporary wrapped-SOL account must close in the swap transaction.",
                ));
            }
            let source_after = if direct_native_input {
                0
            } else if native_input {
                source_before
            } else {
                source_before.checked_sub(amount).ok_or_else(|| {
                    GoFinError::validation("The synthetic swap source balance is insufficient.")
                })?
            };
            let destination_after = if direct_native_output {
                0
            } else {
                destination_before.checked_add(output).ok_or_else(|| {
                    GoFinError::validation(
                        "The synthetic swap output exceeds its exact balance range.",
                    )
                })?
            };
            let sponsored_fee = is_jupiterz
                && keys
                    .first()
                    .is_some_and(|key| key.to_string() == JUPITERZ_SYNTHETIC_MAKER);
            let fee = if sponsored_fee { 0 } else { 5_000u64 };
            let wallet_before = state.lamports;
            let sponsor_before = state.sponsor_lamports;
            let native_in_lamports = if native_input { amount } else { 0 };
            let native_out_lamports = if native_output { output } else { 0 };
            let closed_rent = if (native_output && !direct_native_output) || closes_source_ata {
                WSOL_ACCOUNT_RENT_LAMPORTS
            } else {
                0
            };
            let account_funding_lamports = if created_source_ata {
                WSOL_ACCOUNT_RENT_LAMPORTS
            } else {
                0
            } + if created_destination_ata {
                WSOL_ACCOUNT_RENT_LAMPORTS
            } else {
                0
            };
            let wallet_after = wallet_before
                .checked_sub(fee)
                .and_then(|value| value.checked_sub(native_in_lamports))
                .and_then(|value| value.checked_sub(account_funding_lamports))
                .and_then(|value| value.checked_add(native_out_lamports))
                .and_then(|value| value.checked_add(closed_rent))
                .ok_or_else(|| {
                    GoFinError::validation("The synthetic wallet swap delta overflows.")
                })?;
            let sponsor_after = if sponsored_fee {
                sponsor_before.checked_sub(5_000).ok_or_else(|| {
                    GoFinError::validation(
                        "The synthetic JupiterZ sponsor balance is insufficient.",
                    )
                })?
            } else {
                sponsor_before
            };
            let source_position = keys
                .iter()
                .position(|key| key.to_string() == source)
                .ok_or_else(|| {
                    GoFinError::validation("The synthetic message omits its input account.")
                })?;
            let destination_position = keys
                .iter()
                .position(|key| key.to_string() == destination)
                .ok_or_else(|| {
                GoFinError::validation("The synthetic message omits its output account.")
            })?;
            if sponsored_fee && !direct_native_input && !direct_native_output {
                let wallet_key = state.wallet.clone();
                let wallet_pays_created_ata = instructions.iter().any(|setup| {
                    setup.data == [1]
                        && keys
                            .get(usize::from(setup.program_id_index))
                            .is_some_and(|key| key.to_string() == ASSOCIATED_TOKEN_PROGRAM)
                        && setup
                            .accounts
                            .first()
                            .and_then(|index| keys.get(usize::from(*index)))
                            .is_some_and(|key| key.to_string() == wallet_key)
                });
                if !wallet_pays_created_ata {
                    return Err(GoFinError::validation(
                        "The sponsored JupiterZ fixture must create the wallet-owned ATA with wallet funds.",
                    ));
                }
            }
            let input_mint = if direct_native_input {
                source_mint.clone()
            } else {
                account_mint(&state.account_data, &source)?
            };
            let output_mint = if direct_native_output {
                destination_mint.clone()
            } else {
                account_mint(&state.account_data, &destination)?
            };
            let input_program = if direct_native_input {
                mint_profile(&source_mint).0.to_owned()
            } else {
                state
                    .account_data
                    .get(&source)
                    .map(|entry| entry.0.clone())
                    .ok_or_else(|| {
                        GoFinError::validation("The synthetic input account is absent.")
                    })?
            };
            let output_program = if direct_native_output {
                mint_profile(&destination_mint).0.to_owned()
            } else {
                state
                    .account_data
                    .get(&destination)
                    .map(|entry| entry.0.clone())
                    .ok_or_else(|| {
                        GoFinError::validation("The synthetic output account is absent.")
                    })?
            };
            let input_decimals = mint_profile(&input_mint).1;
            let output_decimals = mint_profile(&output_mint).1;
            let mut pre_token_balances = Vec::new();
            if !direct_native_input && !created_source_ata {
                pre_token_balances.push(token_balance(
                    source_position,
                    &input_mint,
                    &state.wallet,
                    &input_program,
                    input_decimals,
                    source_before,
                ));
            }
            if !direct_native_output && !created_destination_ata {
                pre_token_balances.push(token_balance(
                    destination_position,
                    &output_mint,
                    &state.wallet,
                    &output_program,
                    output_decimals,
                    destination_before,
                ));
            }
            let mut post_token_balances = Vec::new();
            if !direct_native_input && !closes_source_ata {
                post_token_balances.push(token_balance(
                    source_position,
                    &input_mint,
                    &state.wallet,
                    &input_program,
                    input_decimals,
                    source_after,
                ));
            }
            // Source and destination accounts can have different lifecycles:
            // closing a temporary WSOL input must not hide the persistent
            // destination token-account balance from settlement proof.
            if !direct_native_output && !native_output {
                post_token_balances.push(token_balance(
                    destination_position,
                    &output_mint,
                    &state.wallet,
                    &output_program,
                    output_decimals,
                    destination_after,
                ));
            }
            let signature = transaction
                .signatures
                .first()
                .map(ToString::to_string)
                .ok_or_else(|| {
                    GoFinError::validation("The synthetic transaction has no signature.")
                })?;
            if direct_native_input {
                // Direct native input has no user WSOL token account.
            } else if closes_source_ata {
                state.account_data.remove(&source);
                state.token_amounts.remove(&source);
            } else {
                state.token_amounts.insert(source, source_after);
            }
            if direct_native_output {
                // Direct native output credits wallet SOL without a WSOL ATA.
            } else if native_output {
                state.account_data.remove(&destination);
                state.token_amounts.remove(&destination);
            } else {
                state.token_amounts.insert(destination, destination_after);
            }
            state.lamports = wallet_after;
            state.sponsor_lamports = sponsor_after;
            let mut pre_balances = vec![0u64; keys.len()];
            let mut post_balances = vec![0u64; keys.len()];
            if let Some(wallet_index) = keys.iter().position(|key| key.to_string() == state.wallet)
            {
                pre_balances[wallet_index] = wallet_before;
                post_balances[wallet_index] = wallet_after;
            }
            if sponsored_fee {
                let sponsor_index = keys
                    .iter()
                    .position(|key| key.to_string() == JUPITERZ_SYNTHETIC_MAKER)
                    .ok_or_else(|| GoFinError::validation("The JupiterZ sponsor is missing."))?;
                pre_balances[sponsor_index] = sponsor_before;
                post_balances[sponsor_index] = sponsor_after;
            }
            if created_source_ata && !closes_source_ata {
                post_balances[source_position] = WSOL_ACCOUNT_RENT_LAMPORTS;
            }
            if created_destination_ata && !native_output {
                post_balances[destination_position] = WSOL_ACCOUNT_RENT_LAMPORTS;
            }
            if native_output && !direct_native_output && !created_destination_ata {
                pre_balances[destination_position] = 2_039_280;
                post_balances[destination_position] = 0;
            }
            state.transactions.push(StoredTransaction {
                signature: signature.clone(),
                wire: encoded.to_owned(),
                pre_balances,
                post_balances,
                pre_token_balances,
                post_token_balances,
                logs: Vec::new(),
                inner_instructions: Vec::new(),
            });
            return self.record_submission(&mut state, signature);
        }
        let unwrap_transfer = instructions.iter().find_map(|ix| {
            let program = keys.get(usize::from(ix.program_id_index))?;
            if program.to_string() != gofin_solana::token::TOKEN_PROGRAM_ID
                || ix.data.len() != 10
                || ix.data[0] != 12
                || ix.data[9] != 9
            {
                return None;
            }
            Some((
                keys.get(usize::from(*ix.accounts.first()?))?.to_string(),
                keys.get(usize::from(*ix.accounts.get(2)?))?.to_string(),
                u64::from_le_bytes(ix.data[1..9].try_into().ok()?),
            ))
        });
        let closes_temporary = instructions.iter().any(|ix| {
            ix.data == [9]
                && keys
                    .get(usize::from(ix.program_id_index))
                    .is_some_and(|key| key.to_string() == gofin_solana::token::TOKEN_PROGRAM_ID)
        });
        if closes_temporary {
            let (source, temporary, amount) = unwrap_transfer.ok_or_else(|| {
                GoFinError::validation(
                    "The synthetic unwrap is missing its exact checked token transfer.",
                )
            })?;
            let mut state = self.0.lock().unwrap();
            if account_mint(&state.account_data, &source)? != SWAP_INPUT_MINT
                || account_mint(&state.account_data, &temporary).is_ok()
            {
                return Err(GoFinError::validation(
                    "The synthetic unwrap does not use the wallet's wrapped-SOL mint and a new temporary account.",
                ));
            }
            let source_before = *state.token_amounts.get(&source).ok_or_else(|| {
                GoFinError::validation("The synthetic unwrap source account is absent.")
            })?;
            let source_after = source_before.checked_sub(amount).ok_or_else(|| {
                GoFinError::validation("The synthetic wrapped-SOL balance is insufficient.")
            })?;
            let fee = 5_000u64;
            let wallet_before = state.lamports;
            let wallet_after = wallet_before
                .checked_add(amount)
                .and_then(|value| value.checked_sub(fee))
                .ok_or_else(|| GoFinError::validation("The synthetic unwrap balance overflows."))?;
            let source_index = keys
                .iter()
                .position(|key| key.to_string() == source)
                .ok_or_else(|| GoFinError::validation("The synthetic unwrap source is missing."))?;
            let wallet_index = keys
                .iter()
                .position(|key| key.to_string() == state.wallet)
                .ok_or_else(|| GoFinError::validation("The synthetic unwrap wallet is missing."))?;
            let temporary_index = keys
                .iter()
                .position(|key| key.to_string() == temporary)
                .ok_or_else(|| {
                    GoFinError::validation("The synthetic unwrap temp account is missing.")
                })?;
            let mut pre_balances = vec![0u64; keys.len()];
            let mut post_balances = vec![0u64; keys.len()];
            pre_balances[wallet_index] = wallet_before;
            post_balances[wallet_index] = wallet_after;
            pre_balances[temporary_index] = 0;
            post_balances[temporary_index] = 0;
            let pre_token_balances = vec![token_balance(
                source_index,
                SWAP_INPUT_MINT,
                &state.wallet,
                gofin_solana::token::TOKEN_PROGRAM_ID,
                9,
                source_before,
            )];
            let post_token_balances = vec![token_balance(
                source_index,
                SWAP_INPUT_MINT,
                &state.wallet,
                gofin_solana::token::TOKEN_PROGRAM_ID,
                9,
                source_after,
            )];
            let signature = transaction
                .signatures
                .first()
                .map(ToString::to_string)
                .ok_or_else(|| GoFinError::validation("The synthetic unwrap has no signature."))?;
            state.lamports = wallet_after;
            state.token_amounts.insert(source, source_after);
            state.transactions.push(StoredTransaction {
                signature: signature.clone(),
                wire: encoded.to_owned(),
                pre_balances,
                post_balances,
                pre_token_balances,
                post_token_balances,
                logs: Vec::new(),
                inner_instructions: Vec::new(),
            });
            return self.record_submission(&mut state, signature);
        }
        let wrapped_sync = instructions.iter().any(|ix| {
            ix.data == [17]
                && keys
                    .get(usize::from(ix.program_id_index))
                    .is_some_and(|key| key.to_string() == gofin_solana::token::TOKEN_PROGRAM_ID)
        });
        let (source, destination, amount) = instructions
            .iter()
            .find_map(|ix| {
                if ix.data.len() != 12 || u32::from_le_bytes(ix.data[..4].try_into().ok()?) != 2 {
                    return None;
                }
                let source = *ix.accounts.first()? as usize;
                let destination = *ix.accounts.get(1)? as usize;
                Some((
                    keys.get(source)?.to_string(),
                    keys.get(destination)?.to_string(),
                    u64::from_le_bytes(ix.data[4..12].try_into().ok()?),
                ))
            })
            .ok_or_else(|| {
                GoFinError::new(
                    "SOLANA_FIXTURE_OPERATION_UNSUPPORTED",
                    "fixture",
                    "The in-memory Solana rehearsal currently supports native SOL transfers only.",
                    "Use a transaction covered by this synthetic fixture.",
                )
            })?;
        if wrapped_sync {
            let mut state = self.0.lock().unwrap();
            if source != state.wallet
                || account_mint(&state.account_data, &destination)? != SWAP_INPUT_MINT
            {
                return Err(GoFinError::validation(
                    "The synthetic wrap does not target the wallet's wrapped-SOL account.",
                ));
            }
            let before = state
                .token_amounts
                .get(&destination)
                .copied()
                .ok_or_else(|| {
                    GoFinError::validation("The synthetic wrapped-SOL account is absent.")
                })?;
            let after = before.checked_add(amount).ok_or_else(|| {
                GoFinError::validation("The wrapped-SOL amount exceeds its exact balance range.")
            })?;
            let fee = 5_000u64;
            let debit = amount.checked_add(fee).ok_or_else(|| {
                GoFinError::validation("The synthetic wrap debit exceeds its exact range.")
            })?;
            let wallet_before = state.lamports;
            let wallet_after = wallet_before.checked_sub(debit).ok_or_else(|| {
                GoFinError::validation("The synthetic fixture wallet lacks SOL for wrapping.")
            })?;
            let source_index = keys
                .iter()
                .position(|key| key.to_string() == source)
                .ok_or_else(|| GoFinError::validation("The synthetic wrap payer is missing."))?;
            let destination_index = keys
                .iter()
                .position(|key| key.to_string() == destination)
                .ok_or_else(|| GoFinError::validation("The synthetic WSOL account is missing."))?;
            let mut pre_balances = vec![0u64; keys.len()];
            let mut post_balances = vec![0u64; keys.len()];
            pre_balances[source_index] = wallet_before;
            post_balances[source_index] = wallet_after;
            // A wrapped-SOL account's lamports contain its rent reserve and
            // its token amount. Wrapping changes the latter while preserving
            // the original rent reserve.
            pre_balances[destination_index] = 2_039_280;
            post_balances[destination_index] =
                2_039_280u64.checked_add(after).ok_or_else(|| {
                    GoFinError::validation("The synthetic WSOL account balance overflows.")
                })?;
            let pre_token_balances = vec![token_balance(
                destination_index,
                SWAP_INPUT_MINT,
                &state.wallet,
                gofin_solana::token::TOKEN_PROGRAM_ID,
                9,
                before,
            )];
            let post_token_balances = vec![token_balance(
                destination_index,
                SWAP_INPUT_MINT,
                &state.wallet,
                gofin_solana::token::TOKEN_PROGRAM_ID,
                9,
                after,
            )];
            let signature = transaction
                .signatures
                .first()
                .map(ToString::to_string)
                .ok_or_else(|| GoFinError::validation("The synthetic wrap has no signature."))?;
            state.lamports = wallet_after;
            state.token_amounts.insert(destination, after);
            state.transactions.push(StoredTransaction {
                signature: signature.clone(),
                wire: encoded.to_owned(),
                pre_balances,
                post_balances,
                pre_token_balances,
                post_token_balances,
                logs: Vec::new(),
                inner_instructions: Vec::new(),
            });
            return self.record_submission(&mut state, signature);
        }
        let mut state = self.0.lock().unwrap();
        if source != state.wallet || source == destination {
            return Err(GoFinError::validation(
                "The synthetic transfer is outside the fixture wallet scope.",
            ));
        }
        let fee = 5_000u64;
        let debit = amount.checked_add(fee).ok_or_else(|| {
            GoFinError::validation("The synthetic transfer exceeds its exact balance range.")
        })?;
        let wallet_after = state
            .lamports
            .checked_sub(debit)
            .ok_or_else(|| GoFinError::validation("The synthetic fixture wallet lacks SOL."))?;
        let mut pre = vec![0u64; keys.len()];
        let mut post = vec![0u64; keys.len()];
        let source_index = keys
            .iter()
            .position(|key| key.to_string() == source)
            .ok_or_else(|| {
                GoFinError::validation("The synthetic message lacks its transfer source.")
            })?;
        let destination_index = keys
            .iter()
            .position(|key| key.to_string() == destination)
            .ok_or_else(|| {
                GoFinError::validation("The synthetic message lacks its transfer destination.")
            })?;
        pre[source_index] = state.lamports;
        post[source_index] = wallet_after;
        post[destination_index] = amount;
        let signature = transaction
            .signatures
            .first()
            .map(ToString::to_string)
            .ok_or_else(|| GoFinError::validation("The synthetic transaction has no signature."))?;
        state.lamports = wallet_after;
        state.transactions.push(StoredTransaction {
            signature: signature.clone(),
            wire: encoded.to_owned(),
            pre_balances: pre,
            post_balances: post,
            pre_token_balances: Vec::new(),
            post_token_balances: Vec::new(),
            logs: Vec::new(),
            inner_instructions: Vec::new(),
        });
        self.record_submission(&mut state, signature)
    }
}

fn meta_request_selector_matches(selector: &str, request_id: &str) -> bool {
    request_id == selector
        || selector
            .strip_prefix("synthetic-order")
            .is_some_and(|suffix| request_id.ends_with(suffix))
}

#[async_trait]
impl SolanaRpc for FixtureRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.remove_wsol_ata_when_requested()?;
        {
            let mut state = self.0.lock().unwrap();
            let count = state
                .rpc_method_counts
                .entry(method.to_owned())
                .or_default();
            *count = count.saturating_add(1);
            if method == "getAccountInfo"
                && let Some(address) = params
                    .as_array()
                    .and_then(|values| values.first())
                    .and_then(Value::as_str)
            {
                state.get_account_info_addresses.push(address.to_owned());
            }
            self.persist_state(&state)?;
        }
        if let Some(value) = perps::shared_account_info(self, method, &params)? {
            return Ok(value);
        }
        if let Some(mut value) = perps::rpc_request(self, method, &params).await? {
            // The perps RPC fixture may answer getMultipleAccounts before the
            // Meteora fixture. Preserve its perps rows while filling the
            // synthetic payer's System account from the shared Solana ledger.
            // Quote probes need this pre-state even when the payer is not a
            // perps-owned account.
            if method == "getMultipleAccounts" {
                let requested = params
                    .as_array()
                    .and_then(|values| values.first())
                    .and_then(Value::as_array);
                if let (Some(requested), Some(rows)) = (
                    requested,
                    value.get_mut("value").and_then(Value::as_array_mut),
                ) {
                    let state = self.0.lock().map_err(|_| fixture_state_error())?;
                    for (index, address) in requested.iter().enumerate() {
                        let Some(address) = address.as_str() else {
                            continue;
                        };
                        let lamports = if address == state.wallet {
                            Some(state.lamports)
                        } else if address == JUPITERZ_SYNTHETIC_MAKER {
                            Some(state.sponsor_lamports)
                        } else {
                            None
                        };
                        if rows.get(index).is_some_and(Value::is_null)
                            && let Some(lamports) = lamports
                        {
                            rows[index] = json!({
                                "owner": "11111111111111111111111111111111",
                                "data": ["", "base64"],
                                "lamports": lamports,
                                "executable": false,
                                "rentEpoch": 0
                            });
                        }
                    }
                }
            }
            return Ok(value);
        }
        if let Some(value) = jlp::rpc_request(self, method, &params).await? {
            return Ok(value);
        }
        if let Some(value) = trigger::rpc_request(self, method, &params).await? {
            return Ok(value);
        }
        if let Some(value) = meteora_rehearsal::rpc_request(self, method, &params)? {
            return Ok(value);
        }
        let value = match method {
            "getGenesisHash" => json!(MAINNET_GENESIS_HASH),
            "getSlot" => json!(100),
            "getBalance" => {
                let address = params
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let state = self.0.lock().unwrap();
                json!({"context":{"slot":100},"value":if address==state.wallet {state.lamports} else if address==JUPITERZ_SYNTHETIC_MAKER {state.sponsor_lamports} else {0}})
            }
            "getEpochInfo" => {
                json!({"epoch":1,"slotIndex":100,"slotsInEpoch":432000,"absoluteSlot":100,"blockHeight":100,"transactionCount":1})
            }
            "getTokenAccountsByOwner" => {
                let args = params.as_array();
                let program = args
                    .and_then(|a| a.get(1))
                    .and_then(|v| v.get("programId"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let state = self.0.lock().unwrap();
                let value = state
                    .account_data
                    .iter()
                    .filter_map(|(address, (owner, data))| {
                        if owner != program || data.len() != 165 {
                            return None;
                        }
                        let token_owner = bs58::encode(&data[32..64]).into_string();
                        if token_owner != state.wallet {
                            return None;
                        }
                        let amount = state.token_amounts.get(address).copied().unwrap_or(0);
                        let mut data = data.clone();
                        data[64..72].copy_from_slice(&amount.to_le_bytes());
                        let mint = bs58::encode(&data[..32]).into_string();
                        let lamports = if mint == SWAP_INPUT_MINT {
                            WSOL_ACCOUNT_RENT_LAMPORTS.saturating_add(amount)
                        } else {
                            WSOL_ACCOUNT_RENT_LAMPORTS
                        };
                        Some(json!({"pubkey":address,"account":{"owner":owner,"data":[STANDARD.encode(data),"base64"],"lamports":lamports}}))
                    })
                    .collect::<Vec<_>>();
                json!({"context":{"slot":100},"value":value})
            }
            "getBlockTime" => json!(chrono::Utc::now().timestamp()),
            "getLatestBlockhash" => {
                let mut state = self.0.lock().unwrap();
                state.blockhash_counter = state
                    .blockhash_counter
                    .checked_add(1)
                    .ok_or_else(fixture_state_error)?;
                let mut bytes = [0u8; 32];
                bytes[..8].copy_from_slice(&state.blockhash_counter.to_le_bytes());
                let blockhash = solana_hash::Hash::new_from_array(bytes).to_string();
                self.persist_state(&state)?;
                json!({"context":{"slot":100},"value":{"blockhash":blockhash,"lastValidBlockHeight":1000}})
            }
            "getBlockHeight" => {
                let mode = std::env::var("GOFIN_DEV_SOLANA_FIXTURE_EXPIRY_MODE").ok();
                if let Some(mode) = mode {
                    let mut state = self.0.lock().map_err(|_| fixture_state_error())?;
                    state.expiry_candidate_height_checks =
                        state.expiry_candidate_height_checks.saturating_add(1);
                    let phase_ordinal = match mode.as_str() {
                        "revalidate-once" | "revalidate-every" => 3,
                        "dispatch-once" | "dispatch-every" => 4,
                        "uncertain" => u64::MAX,
                        _ => return Err(GoFinError::validation("Unknown synthetic expiry mode.")),
                    };
                    let candidate_selected = match mode.as_str() {
                        "revalidate-once" | "dispatch-once" => state.expiry_candidate_count == 1,
                        "revalidate-every" | "dispatch-every" => true,
                        _ => false,
                    };
                    let expired =
                        candidate_selected && state.expiry_candidate_height_checks == phase_ordinal;
                    self.persist_state(&state)?;
                    json!(if expired { 1001 } else { 100 })
                } else {
                    json!(100)
                }
            }
            "isBlockhashValid" => json!({"context":{"slot":100},"value":true}),
            "getFeeForMessage" => {
                let encoded_message = params
                    .as_array()
                    .and_then(|params| params.first())
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        GoFinError::validation("The synthetic fee query is malformed.")
                    })?;
                json!({"context":{"slot":100},"value":synthetic_fee_for_message(encoded_message)?})
            }
            "simulateTransaction" => match meteora_rehearsal::simulate(self, &params)? {
                Some(value) => value,
                None => {
                    json!({"context":{"slot":100},"value":{"err":null,"unitsConsumed":100_000}})
                }
            },
            "getMinimumBalanceForRentExemption" => json!(WSOL_ACCOUNT_RENT_LAMPORTS),
            "getAccountInfo" => {
                let address = params
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let state = self.0.lock().unwrap();
                if let Some((owner, bytes)) = state.account_data.get(address) {
                    let mut bytes = bytes.clone();
                    if bytes.len() == 165 {
                        let amount = state.token_amounts.get(address).copied().unwrap_or(0);
                        bytes[64..72].copy_from_slice(&amount.to_le_bytes());
                    }
                    let lamports = if account_mint(&state.account_data, address).ok().as_deref()
                        == Some(SWAP_INPUT_MINT)
                    {
                        2_039_280u64
                            .saturating_add(state.token_amounts.get(address).copied().unwrap_or(0))
                    } else {
                        2_039_280
                    };
                    json!({"context":{"slot":100},"value":{"owner":owner,"data":[STANDARD.encode(bytes),"base64"],"lamports":lamports}})
                } else {
                    json!({"context":{"slot":100},"value":null})
                }
            }
            "getMultipleAccounts" => {
                let addresses = params
                    .as_array()
                    .and_then(|args| args.first())
                    .and_then(Value::as_array)
                    .ok_or_else(|| GoFinError::validation("Malformed synthetic account batch."))?;
                let state = self.0.lock().unwrap();
                let values = addresses
                    .iter()
                    .map(|address| {
                        let address = address.as_str().unwrap_or_default();
                        state.account_data.get(address).map(|(owner, data)| {
                            let mut data = data.clone();
                            if data.len() == 165 {
                                let amount = state.token_amounts.get(address).copied().unwrap_or(0);
                                data[64..72].copy_from_slice(&amount.to_le_bytes());
                            }
                            let lamports = if account_mint(&state.account_data, address)
                                .ok()
                                .as_deref()
                                == Some(SWAP_INPUT_MINT)
                            {
                                2_039_280u64.saturating_add(
                                    state.token_amounts.get(address).copied().unwrap_or(0),
                                )
                            } else {
                                2_039_280
                            };
                            json!({"owner":owner,"data":[STANDARD.encode(data),"base64"],"lamports":lamports})
                        })
                    })
                    .collect::<Vec<_>>();
                json!({"context":{"slot":100},"value":values})
            }
            "sendTransaction" => {
                let encoded = params
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        GoFinError::validation("The synthetic transaction is malformed.")
                    })?;
                return match self.submit(encoded) {
                    Ok(signature) => Ok(json!(signature)),
                    Err(error) => {
                        if std::env::var_os("GOFIN_DEV_SOLANA_FIXTURE_TRACE").is_some() {
                            eprintln!(
                                "synthetic Solana sendTransaction rejected: code={} message={}",
                                error.code, error.message
                            );
                        }
                        Err(error)
                    }
                };
            }
            "getSignatureStatuses" => {
                let signatures = params
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        GoFinError::validation("The synthetic signature query is malformed.")
                    })?;
                let state = self.0.lock().unwrap();
                let values = signatures
                    .iter()
                    .map(|signature| {
                        let signature = signature.as_str().unwrap_or_default();
                        if state
                            .transactions
                            .iter()
                            .any(|tx| tx.signature == signature)
                        {
                            json!({"confirmationStatus":"finalized","err":null,"slot":100})
                        } else {
                            Value::Null
                        }
                    })
                    .collect::<Vec<_>>();
                json!({"context":{"slot":100},"value":values})
            }
            "getTransaction" => {
                let signature = params
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let state = self.0.lock().unwrap();
                let transaction = state
                    .transactions
                    .iter()
                    .find(|tx| tx.signature == signature)
                    .ok_or_else(|| {
                        GoFinError::new(
                            "SOLANA_SIGNATURE_NOT_FOUND",
                            "reconciliation",
                            "The synthetic transaction was not found.",
                            "Reconcile the same request after a finalized receipt is available.",
                        )
                    })?;
                let fee = synthetic_fee_for_transaction(&transaction.wire)?;
                json!({"slot":100,"transaction":[transaction.wire,"base64"],"meta":{"fee":fee,"err":null,"preBalances":transaction.pre_balances,"postBalances":transaction.post_balances,"preTokenBalances":transaction.pre_token_balances,"postTokenBalances":transaction.post_token_balances,"loadedAddresses":{"writable":[],"readonly":[]},"innerInstructions":transaction.inner_instructions,"logMessages":transaction.logs}})
            }
            "getSignaturesForAddress" => {
                let state = self.0.lock().unwrap();
                let signatures = state.transactions.iter().rev().map(|tx| json!({"signature":tx.signature,"confirmationStatus":"finalized","slot":100})).collect::<Vec<_>>();
                json!(signatures)
            }
            _ => {
                let _ = params;
                return Err(GoFinError::new(
                    "SOLANA_RPC_METHOD_UNAVAILABLE",
                    "rpc",
                    format!(
                        "The synthetic Solana rehearsal does not implement RPC method {method}."
                    ),
                    "Use an operation covered by the selected rehearsal fixture.",
                ));
            }
        };
        Ok(value)
    }
}

struct FixtureJupiterApi {
    wallet: String,
    rpc: Arc<FixtureRpc>,
}

impl FixtureJupiterApi {
    fn swap_instruction(
        &self,
        amount: u64,
        input_mint: &str,
        output_mint: &str,
        layout: FixtureSwapLayout,
    ) -> Result<solana_instruction::Instruction> {
        use solana_instruction::{AccountMeta, Instruction};
        use solana_pubkey::Pubkey;

        let wallet = self
            .wallet
            .parse::<Pubkey>()
            .map_err(|_| GoFinError::validation("The synthetic wallet address is invalid."))?;
        let input = gofin_solana::associated_token_address(
            &self.wallet,
            input_mint,
            mint_profile(input_mint).0,
        )?
        .parse::<Pubkey>()
        .map_err(|_| GoFinError::validation("The synthetic input account is invalid."))?;
        let output = gofin_solana::associated_token_address(
            &self.wallet,
            output_mint,
            mint_profile(output_mint).0,
        )?
        .parse::<Pubkey>()
        .map_err(|_| GoFinError::validation("The synthetic output account is invalid."))?;
        let token_program = mint_profile(input_mint)
            .0
            .parse::<Pubkey>()
            .map_err(|_| GoFinError::validation("The synthetic token program is invalid."))?;
        let program = gofin_jupiter::swap::JUPITER_V6_PROGRAM
            .parse::<Pubkey>()
            .map_err(|_| GoFinError::validation("The synthetic swap program is invalid."))?;
        let destination_mint = output_mint
            .parse::<Pubkey>()
            .map_err(|_| GoFinError::validation("The synthetic destination mint is invalid."))?;
        let source_mint = input_mint
            .parse::<Pubkey>()
            .map_err(|_| GoFinError::validation("The synthetic source mint is invalid."))?;
        let event_authority = "D8cy77BBepLMngZx6ZukaTff5hCt1HrWyKk3Hnd9oitf"
            .parse::<Pubkey>()
            .map_err(|_| GoFinError::validation("The pinned event authority is invalid."))?;
        let saber_program = "SSwpkEEcbUqx4vtoEByFjSkhKdCT862DNVb52nZg1UZ"
            .parse::<Pubkey>()
            .map_err(|_| GoFinError::validation("The Saber program address is invalid."))?;
        let output_amount = swap_output_amount(amount, input_mint, output_mint)?;
        let legacy_shared = layout == FixtureSwapLayout::Legacy
            && (mint_profile(input_mint).0 == gofin_solana::token::TOKEN_2022_PROGRAM_ID
                || mint_profile(output_mint).0 == gofin_solana::token::TOKEN_2022_PROGRAM_ID);
        let v2_shared = layout == FixtureSwapLayout::SharedAccountsRouteV2;
        let shared = legacy_shared || v2_shared;
        let mut data = match layout {
            FixtureSwapLayout::Legacy if legacy_shared => {
                vec![193, 32, 155, 51, 65, 214, 156, 129, 0]
            }
            FixtureSwapLayout::Legacy => {
                vec![229, 23, 203, 151, 122, 227, 173, 42]
            }
            FixtureSwapLayout::RouteV2 => ROUTE_V2_DISCRIMINATOR.to_vec(),
            FixtureSwapLayout::SharedAccountsRouteV2 => {
                [SHARED_ROUTE_V2_DISCRIMINATOR.as_slice(), &[0]].concat()
            }
            FixtureSwapLayout::JupiterZ
            | FixtureSwapLayout::JupiterZNativeInput
            | FixtureSwapLayout::JupiterZNativeOutput => {
                return Err(GoFinError::validation(
                    "JupiterZ uses the OrderEngine fill instruction, not a Jupiter route instruction.",
                ));
            }
        };
        match layout {
            FixtureSwapLayout::Legacy => {
                data.extend_from_slice(&1u32.to_le_bytes());
                data.extend_from_slice(&[0, 100, 0, 1]);
                data.extend_from_slice(&amount.to_le_bytes());
                data.extend_from_slice(&output_amount.to_le_bytes());
                data.extend_from_slice(&50u16.to_le_bytes());
                data.push(0);
            }
            FixtureSwapLayout::RouteV2 | FixtureSwapLayout::SharedAccountsRouteV2 => {
                data.extend_from_slice(&amount.to_le_bytes());
                data.extend_from_slice(&output_amount.to_le_bytes());
                data.extend_from_slice(&50u16.to_le_bytes());
                data.extend_from_slice(&0u16.to_le_bytes());
                data.extend_from_slice(&0u16.to_le_bytes());
                data.extend_from_slice(&1u32.to_le_bytes());
                data.push(38); // MeteoraDlmm, a no-payload variant in RoutePlanStepV2.
                data.extend_from_slice(&10_000u16.to_le_bytes());
                data.extend_from_slice(&[0, 1]);
            }
            FixtureSwapLayout::JupiterZ
            | FixtureSwapLayout::JupiterZNativeInput
            | FixtureSwapLayout::JupiterZNativeOutput => {
                unreachable!("JupiterZ does not use a Jupiter route")
            }
        }
        let accounts = if v2_shared {
            let authority = Pubkey::find_program_address(&[b"authority", &[0]], &program).0;
            let program_source = gofin_solana::associated_token_address(
                &authority.to_string(),
                input_mint,
                mint_profile(input_mint).0,
            )?
            .parse::<Pubkey>()
            .map_err(|_| GoFinError::validation("The shared route source account is invalid."))?;
            let program_destination = gofin_solana::associated_token_address(
                &authority.to_string(),
                output_mint,
                mint_profile(output_mint).0,
            )?
            .parse::<Pubkey>()
            .map_err(|_| {
                GoFinError::validation("The shared route destination account is invalid.")
            })?;
            {
                let mut state = self.rpc.0.lock().unwrap();
                for (address, mint, token_program) in [
                    (program_source, input_mint, mint_profile(input_mint).0),
                    (
                        program_destination,
                        output_mint,
                        mint_profile(output_mint).0,
                    ),
                ] {
                    let address = address.to_string();
                    if !state.account_data.contains_key(&address) {
                        state.account_data.insert(
                            address.clone(),
                            (
                                token_program.into(),
                                token_account_data(mint, &authority.to_string(), 0),
                            ),
                        );
                        state.token_amounts.insert(address, 0);
                    }
                }
            }
            let source_program = mint_profile(input_mint)
                .0
                .parse::<Pubkey>()
                .map_err(|_| GoFinError::validation("The source token program is invalid."))?;
            let destination_program = mint_profile(output_mint)
                .0
                .parse::<Pubkey>()
                .map_err(|_| GoFinError::validation("The destination token program is invalid."))?;
            vec![
                AccountMeta::new_readonly(authority, false),
                AccountMeta::new_readonly(wallet, true),
                AccountMeta::new(input, false),
                AccountMeta::new(program_source, false),
                AccountMeta::new(program_destination, false),
                AccountMeta::new(output, false),
                AccountMeta::new_readonly(source_mint, false),
                AccountMeta::new_readonly(destination_mint, false),
                AccountMeta::new_readonly(source_program, false),
                AccountMeta::new_readonly(destination_program, false),
                AccountMeta::new_readonly(event_authority, false),
                AccountMeta::new_readonly(program, false),
            ]
        } else if layout == FixtureSwapLayout::RouteV2 {
            let source_program = mint_profile(input_mint)
                .0
                .parse::<Pubkey>()
                .map_err(|_| GoFinError::validation("The source token program is invalid."))?;
            let destination_program = mint_profile(output_mint)
                .0
                .parse::<Pubkey>()
                .map_err(|_| GoFinError::validation("The destination token program is invalid."))?;
            vec![
                AccountMeta::new_readonly(wallet, true),
                AccountMeta::new(input, false),
                AccountMeta::new(output, false),
                AccountMeta::new_readonly(source_mint, false),
                AccountMeta::new_readonly(destination_mint, false),
                AccountMeta::new_readonly(source_program, false),
                AccountMeta::new_readonly(destination_program, false),
                AccountMeta::new_readonly(program, false), // no optional destination account
                AccountMeta::new_readonly(event_authority, false),
                AccountMeta::new_readonly(program, false),
            ]
        } else if shared {
            let authority = Pubkey::find_program_address(&[b"authority", &[0]], &program).0;
            let program_source = gofin_solana::associated_token_address(
                &authority.to_string(),
                input_mint,
                mint_profile(input_mint).0,
            )?
            .parse::<Pubkey>()
            .map_err(|_| GoFinError::validation("The shared route source account is invalid."))?;
            let program_destination = gofin_solana::associated_token_address(
                &authority.to_string(),
                output_mint,
                mint_profile(output_mint).0,
            )?
            .parse::<Pubkey>()
            .map_err(|_| {
                GoFinError::validation("The shared route destination account is invalid.")
            })?;
            {
                let mut state = self.rpc.0.lock().unwrap();
                for (address, mint, token_program) in [
                    (program_source, input_mint, mint_profile(input_mint).0),
                    (
                        program_destination,
                        output_mint,
                        mint_profile(output_mint).0,
                    ),
                ] {
                    let address = address.to_string();
                    if !state.account_data.contains_key(&address) {
                        state.account_data.insert(
                            address.clone(),
                            (
                                token_program.into(),
                                token_account_data(mint, &authority.to_string(), 0),
                            ),
                        );
                        state.token_amounts.insert(address, 0);
                    }
                }
            }
            let token_2022_program = if mint_profile(input_mint).0
                == gofin_solana::token::TOKEN_2022_PROGRAM_ID
                || mint_profile(output_mint).0 == gofin_solana::token::TOKEN_2022_PROGRAM_ID
            {
                gofin_solana::token::TOKEN_2022_PROGRAM_ID
                    .parse::<Pubkey>()
                    .map_err(|_| GoFinError::validation("The Token-2022 program is invalid."))?
            } else {
                program
            };
            vec![
                AccountMeta::new_readonly(
                    gofin_solana::token::TOKEN_PROGRAM_ID
                        .parse::<Pubkey>()
                        .map_err(|_| GoFinError::validation("The SPL Token program is invalid."))?,
                    false,
                ),
                AccountMeta::new_readonly(authority, false),
                AccountMeta::new_readonly(wallet, true),
                AccountMeta::new(input, false),
                AccountMeta::new(program_source, false),
                AccountMeta::new(program_destination, false),
                AccountMeta::new(output, false),
                AccountMeta::new_readonly(source_mint, false),
                AccountMeta::new_readonly(destination_mint, false),
                AccountMeta::new_readonly(program, false),
                AccountMeta::new_readonly(token_2022_program, false),
                AccountMeta::new_readonly(event_authority, false),
                AccountMeta::new_readonly(program, false),
                AccountMeta::new_readonly(saber_program, false),
                AccountMeta::new(Pubkey::new_from_array([0x41; 32]), false),
                AccountMeta::new(Pubkey::new_from_array([0x42; 32]), false),
                AccountMeta::new(Pubkey::new_from_array([0x43; 32]), false),
            ]
        } else {
            vec![
                AccountMeta::new_readonly(token_program, false),
                AccountMeta::new_readonly(wallet, true),
                AccountMeta::new(input, false),
                AccountMeta::new(output, false),
                AccountMeta::new_readonly(program, false),
                AccountMeta::new_readonly(destination_mint, false),
                AccountMeta::new_readonly(program, false),
                AccountMeta::new_readonly(event_authority, false),
                AccountMeta::new_readonly(program, false),
                AccountMeta::new_readonly(saber_program, false),
                AccountMeta::new(Pubkey::new_from_array([0x41; 32]), false),
                AccountMeta::new(Pubkey::new_from_array([0x42; 32]), false),
                AccountMeta::new(Pubkey::new_from_array([0x43; 32]), false),
            ]
        };
        let instruction = Instruction {
            program_id: program,
            accounts,
            data,
        };
        Ok(instruction)
    }

    fn order_transaction(
        &self,
        amount: u64,
        input_mint: &str,
        output_mint: &str,
        blockhash: solana_hash::Hash,
        layout: FixtureSwapLayout,
        expire_at: Option<i64>,
    ) -> Result<String> {
        use ed25519_dalek::{Signer, SigningKey};
        use solana_message::{Message, VersionedMessage};
        use solana_pubkey::Pubkey;
        use solana_signature::Signature;

        let wallet = self
            .wallet
            .parse::<Pubkey>()
            .map_err(|_| GoFinError::validation("The synthetic wallet address is invalid."))?;
        let mut instructions = Vec::new();
        let mut closes_created_input_ata = false;
        let native_input_direct = layout == FixtureSwapLayout::JupiterZNativeInput;
        let native_output_direct = layout == FixtureSwapLayout::JupiterZNativeOutput;
        if input_mint == SWAP_INPUT_MINT && !native_input_direct {
            let input_address = gofin_solana::associated_token_address(
                &self.wallet,
                SWAP_INPUT_MINT,
                gofin_solana::token::TOKEN_PROGRAM_ID,
            )?;
            let input_exists = self
                .rpc
                .0
                .lock()
                .unwrap()
                .account_data
                .contains_key(&input_address);
            let input_account = input_address.parse::<Pubkey>().map_err(|_| {
                GoFinError::validation("The synthetic wrapped-SOL account is invalid.")
            })?;
            if !input_exists {
                instructions.push(create_wsol_ata_instruction(&wallet, &input_account)?);
                closes_created_input_ata = true;
            }
            let mut transfer_data = 2u32.to_le_bytes().to_vec();
            transfer_data.extend_from_slice(&amount.to_le_bytes());
            instructions.push(solana_instruction::Instruction {
                program_id: "11111111111111111111111111111111"
                    .parse::<Pubkey>()
                    .map_err(|_| {
                        GoFinError::validation("The synthetic system program is invalid.")
                    })?,
                accounts: vec![
                    solana_instruction::AccountMeta::new(wallet, true),
                    solana_instruction::AccountMeta::new(input_account, false),
                ],
                data: transfer_data,
            });
            instructions.push(solana_instruction::Instruction {
                program_id: gofin_solana::token::TOKEN_PROGRAM_ID.parse().map_err(|_| {
                    GoFinError::validation("The synthetic token program is invalid.")
                })?,
                accounts: vec![solana_instruction::AccountMeta::new(input_account, false)],
                data: vec![17],
            });
        }
        if output_mint == SWAP_INPUT_MINT && !native_output_direct {
            let output_address = gofin_solana::associated_token_address(
                &self.wallet,
                SWAP_INPUT_MINT,
                gofin_solana::token::TOKEN_PROGRAM_ID,
            )?;
            let output_exists = self
                .rpc
                .0
                .lock()
                .unwrap()
                .account_data
                .contains_key(&output_address);
            if !output_exists {
                let output_account = output_address.parse::<Pubkey>().map_err(|_| {
                    GoFinError::validation("The synthetic wrapped-SOL account is invalid.")
                })?;
                instructions.push(create_wsol_ata_instruction(&wallet, &output_account)?);
            }
        }
        let route_instruction = if layout.is_jupiterz() {
            self.jupiterz_fill_instruction(
                amount,
                input_mint,
                output_mint,
                layout,
                expire_at
                    .ok_or_else(|| GoFinError::validation("JupiterZ fixture expiry is missing."))?,
            )?
        } else {
            self.swap_instruction(amount, input_mint, output_mint, layout)?
        };
        instructions.push(route_instruction);
        if output_mint == SWAP_INPUT_MINT && !native_output_direct {
            let output_account = gofin_solana::associated_token_address(
                &self.wallet,
                SWAP_INPUT_MINT,
                gofin_solana::token::TOKEN_PROGRAM_ID,
            )?
            .parse::<Pubkey>()
            .map_err(|_| GoFinError::validation("The synthetic wrapped-SOL account is invalid."))?;
            instructions.push(solana_instruction::Instruction {
                program_id: gofin_solana::token::TOKEN_PROGRAM_ID.parse().map_err(|_| {
                    GoFinError::validation("The synthetic token program is invalid.")
                })?,
                accounts: vec![
                    solana_instruction::AccountMeta::new(output_account, false),
                    solana_instruction::AccountMeta::new(wallet, false),
                    solana_instruction::AccountMeta::new_readonly(wallet, true),
                ],
                data: vec![9],
            });
        } else if closes_created_input_ata {
            let input_account = gofin_solana::associated_token_address(
                &self.wallet,
                SWAP_INPUT_MINT,
                gofin_solana::token::TOKEN_PROGRAM_ID,
            )?
            .parse::<Pubkey>()
            .map_err(|_| GoFinError::validation("The synthetic wrapped-SOL account is invalid."))?;
            instructions.push(solana_instruction::Instruction {
                program_id: gofin_solana::token::TOKEN_PROGRAM_ID.parse().map_err(|_| {
                    GoFinError::validation("The synthetic token program is invalid.")
                })?,
                accounts: vec![
                    solana_instruction::AccountMeta::new(input_account, false),
                    solana_instruction::AccountMeta::new(wallet, false),
                    solana_instruction::AccountMeta::new_readonly(wallet, true),
                ],
                data: vec![9],
            });
        }
        let maker =
            Pubkey::new_from_array(SigningKey::from_bytes(&[92; 32]).verifying_key().to_bytes());
        let fee_payer = if layout.is_jupiterz() {
            &maker
        } else {
            &wallet
        };
        // JupiterZ's remote maker sponsors the network fee, while any native
        // ATA create instruction above still names the wallet as its payer.
        let message = Message::new_with_blockhash(&instructions, Some(fee_payer), &blockhash);
        let signature_count = usize::from(message.header.num_required_signatures);
        let transaction = VersionedTransaction {
            signatures: (0..signature_count).map(|_| Signature::default()).collect(),
            message: VersionedMessage::Legacy(message),
        };
        let bytes = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .reject_trailing_bytes()
            .with_limit(128 * 1024)
            .serialize(&transaction)
            .map_err(|_| GoFinError::validation("The synthetic order could not be serialized."))?;
        let encoded = STANDARD.encode(bytes);
        if layout.is_jupiterz() {
            let mut transaction: VersionedTransaction = bincode::DefaultOptions::new()
                .with_fixint_encoding()
                .with_little_endian()
                .reject_trailing_bytes()
                .with_limit(128 * 1024)
                .deserialize(&STANDARD.decode(&encoded).map_err(|_| {
                    GoFinError::validation("The synthetic JupiterZ order could not be decoded.")
                })?)
                .map_err(|_| {
                    GoFinError::validation("The synthetic JupiterZ order is malformed.")
                })?;
            let message_bytes = bincode::DefaultOptions::new()
                .with_fixint_encoding()
                .with_little_endian()
                .serialize(&transaction.message)
                .map_err(|_| {
                    GoFinError::validation("The synthetic JupiterZ message is malformed.")
                })?;
            let VersionedMessage::Legacy(message) = &transaction.message else {
                return Err(GoFinError::validation(
                    "The synthetic JupiterZ order must be legacy.",
                ));
            };
            let maker_index = message
                .account_keys
                .iter()
                .take(usize::from(message.header.num_required_signatures))
                .position(|key| key == &maker)
                .ok_or_else(|| GoFinError::validation("The JupiterZ maker signer is missing."))?;
            transaction.signatures[maker_index] = Signature::from(
                SigningKey::from_bytes(&[92; 32])
                    .sign(&message_bytes)
                    .to_bytes(),
            );
            let signed = bincode::DefaultOptions::new()
                .with_fixint_encoding()
                .with_little_endian()
                .serialize(&transaction)
                .map_err(|_| {
                    GoFinError::validation(
                        "The synthetic JupiterZ signature could not be serialized.",
                    )
                })?;
            return Ok(STANDARD.encode(signed));
        }
        Ok(encoded)
    }

    fn jupiterz_fill_instruction(
        &self,
        amount: u64,
        input_mint: &str,
        output_mint: &str,
        layout: FixtureSwapLayout,
        expire_at: i64,
    ) -> Result<solana_instruction::Instruction> {
        use ed25519_dalek::SigningKey;
        use solana_instruction::{AccountMeta, Instruction};
        use solana_pubkey::Pubkey;

        let wallet = self
            .wallet
            .parse::<Pubkey>()
            .map_err(|_| GoFinError::validation("The synthetic wallet address is invalid."))?;
        // A distinct remote maker signer is part of JupiterZ's Fill message.
        let maker =
            Pubkey::new_from_array(SigningKey::from_bytes(&[92; 32]).verifying_key().to_bytes());
        let input_program = mint_profile(input_mint)
            .0
            .parse::<Pubkey>()
            .map_err(|_| GoFinError::validation("The JupiterZ input token program is invalid."))?;
        let output_program = mint_profile(output_mint)
            .0
            .parse::<Pubkey>()
            .map_err(|_| GoFinError::validation("The JupiterZ output token program is invalid."))?;
        let input_mint_key = input_mint
            .parse::<Pubkey>()
            .map_err(|_| GoFinError::validation("The JupiterZ input mint is invalid."))?;
        let output_mint_key = output_mint
            .parse::<Pubkey>()
            .map_err(|_| GoFinError::validation("The JupiterZ output mint is invalid."))?;
        let user_source = if layout == FixtureSwapLayout::JupiterZNativeInput {
            ORDER_ENGINE_PROGRAM.parse::<Pubkey>().map_err(|_| {
                GoFinError::validation("The JupiterZ native-input sentinel is invalid.")
            })?
        } else {
            gofin_solana::associated_token_address(
                &wallet.to_string(),
                input_mint,
                mint_profile(input_mint).0,
            )?
            .parse::<Pubkey>()
            .map_err(|_| GoFinError::validation("The JupiterZ user source account is invalid."))?
        };
        let maker_source = gofin_solana::associated_token_address(
            &maker.to_string(),
            input_mint,
            mint_profile(input_mint).0,
        )?
        .parse::<Pubkey>()
        .map_err(|_| GoFinError::validation("The JupiterZ maker source account is invalid."))?;
        let user_destination = if layout == FixtureSwapLayout::JupiterZNativeOutput {
            ORDER_ENGINE_PROGRAM.parse::<Pubkey>().map_err(|_| {
                GoFinError::validation("The JupiterZ native-output sentinel is invalid.")
            })?
        } else {
            gofin_solana::associated_token_address(
                &wallet.to_string(),
                output_mint,
                mint_profile(output_mint).0,
            )?
            .parse::<Pubkey>()
            .map_err(|_| {
                GoFinError::validation("The JupiterZ user destination account is invalid.")
            })?
        };
        let maker_destination = gofin_solana::associated_token_address(
            &maker.to_string(),
            output_mint,
            mint_profile(output_mint).0,
        )?
        .parse::<Pubkey>()
        .map_err(|_| {
            GoFinError::validation("The JupiterZ maker destination account is invalid.")
        })?;
        {
            let mut state = self.rpc.0.lock().unwrap();
            for (address, mint, owner, program) in [
                (maker_source, input_mint_key, maker, input_program),
                (maker_destination, output_mint_key, maker, output_program),
            ] {
                let address = address.to_string();
                state
                    .account_data
                    .entry(address.clone())
                    .or_insert_with(|| {
                        (
                            program.to_string(),
                            token_account_data(&mint.to_string(), &owner.to_string(), 100_000_000),
                        )
                    });
                state.token_amounts.entry(address).or_insert(100_000_000);
            }
        }
        let output = swap_output_amount(amount, input_mint, output_mint)?;
        let mut data = ORDER_ENGINE_FILL_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&amount.to_le_bytes());
        data.extend_from_slice(&output.to_le_bytes());
        data.extend_from_slice(&expire_at.to_le_bytes());
        // Jupiter's OrderEngine Fill adds fee_bps (u16), flags (u8), and
        // integrator_fee_bps (u16) after the IDL-defined arguments.
        data.extend_from_slice(&0u16.to_le_bytes());
        data.push(0); // exact-in, no integrator-fee-side bit set
        data.extend_from_slice(&0u16.to_le_bytes());
        Ok(Instruction {
            program_id: ORDER_ENGINE_PROGRAM.parse::<Pubkey>().map_err(|_| {
                GoFinError::validation("The JupiterZ OrderEngine program is invalid.")
            })?,
            accounts: vec![
                AccountMeta::new(wallet, true),
                AccountMeta::new(maker, true),
                if layout == FixtureSwapLayout::JupiterZNativeInput {
                    AccountMeta::new_readonly(user_source, false)
                } else {
                    AccountMeta::new(user_source, false)
                },
                AccountMeta::new(maker_source, false),
                if layout == FixtureSwapLayout::JupiterZNativeOutput {
                    AccountMeta::new_readonly(user_destination, false)
                } else {
                    AccountMeta::new(user_destination, false)
                },
                AccountMeta::new(maker_destination, false),
                AccountMeta::new_readonly(input_mint_key, false),
                AccountMeta::new_readonly(input_program, false),
                AccountMeta::new_readonly(output_mint_key, false),
                AccountMeta::new_readonly(output_program, false),
                AccountMeta::new_readonly(
                    "11111111111111111111111111111111"
                        .parse::<Pubkey>()
                        .map_err(|_| {
                            GoFinError::validation("The synthetic system program is invalid.")
                        })?,
                    false,
                ),
            ],
            data,
        })
    }
}

#[async_trait]
impl gofin_jupiter::JupiterApi for FixtureJupiterApi {
    async fn request(
        &self,
        _host: gofin_jupiter::JupiterHost,
        method: Method,
        path: &str,
        body: Option<Value>,
        _bucket: gofin_jupiter::RateBucket,
        _priority: gofin_jupiter::RequestPriority,
        _max_wait: Duration,
    ) -> Result<Value> {
        // Provider fixture construction inspects account state directly before
        // the adapter's RPC preflight, so consume this scenario marker here too.
        self.rpc.remove_wsol_ata_when_requested()?;
        if let Some(value) = jlp::api_request(self, method.clone(), path, body.clone()).await? {
            return Ok(value);
        }
        if let Some(value) = perps::api_request(self, method.clone(), path, body.clone()).await? {
            return Ok(value);
        }
        if let Some(value) = trigger::api_request(self, method.clone(), path, body.clone()).await? {
            return Ok(value);
        }
        if let Some(ids) = path.strip_prefix("/price/v3?ids=") {
            let values = ids
                .split(',')
                .map(|mint| {
                    let (_, decimals, price) = mint_profile(mint);
                    (
                        mint.to_owned(),
                        json!({"usdPrice":price.to_string(),"decimals":decimals,"blockId":100}),
                    )
                })
                .collect::<serde_json::Map<_, _>>();
            return Ok(Value::Object(values));
        }
        if path.starts_with("/swap/v2/build?") {
            self.rpc.begin_expiry_candidate()?;
            let amount = path
                .split('&')
                .find_map(|part| part.strip_prefix("amount="))
                .and_then(|amount| amount.parse::<u64>().ok())
                .ok_or_else(|| GoFinError::validation("The synthetic swap amount is malformed."))?;
            let input_mint = query_parameter(path, "inputMint").unwrap_or(SWAP_INPUT_MINT);
            let output_mint = query_parameter(path, "outputMint").unwrap_or(SWAP_OUTPUT_MINT);
            let output_amount = swap_output_amount(amount, input_mint, output_mint)?;
            let blockhash = self.rpc.next_fixture_blockhash()?;
            let layout = FixtureSwapLayout::selected(path, amount, input_mint, output_mint);
            let swap_instruction =
                self.swap_instruction(amount, input_mint, output_mint, layout)?;
            let accounts = swap_instruction
                .accounts
                .iter()
                .map(|account| {
                    json!({
                        "pubkey":account.pubkey.to_string(),
                        "isSigner":account.is_signer,
                        "isWritable":account.is_writable
                    })
                })
                .collect::<Vec<_>>();
            let native_wrap = query_parameter(path, "wrapAndUnwrapSol") == Some("true")
                && input_mint == SWAP_INPUT_MINT;
            let mut setup_instructions = Vec::new();
            let mut input_cleanup_instruction = None;
            if native_wrap {
                let source = gofin_solana::associated_token_address(
                    &self.wallet,
                    SWAP_INPUT_MINT,
                    gofin_solana::token::TOKEN_PROGRAM_ID,
                )?;
                let source_exists = self
                    .rpc
                    .0
                    .lock()
                    .unwrap()
                    .account_data
                    .contains_key(&source);
                if !source_exists {
                    let payer = self.wallet.parse::<solana_pubkey::Pubkey>().map_err(|_| {
                        GoFinError::validation("The synthetic wallet address is invalid.")
                    })?;
                    let account = source.parse::<solana_pubkey::Pubkey>().map_err(|_| {
                        GoFinError::validation("The synthetic wrapped-SOL account is invalid.")
                    })?;
                    setup_instructions.push(json!({
                        "programId":ASSOCIATED_TOKEN_PROGRAM,
                        "accounts":[
                            {"pubkey":self.wallet,"isSigner":true,"isWritable":true},
                            {"pubkey":source,"isSigner":false,"isWritable":true},
                            {"pubkey":self.wallet,"isSigner":false,"isWritable":false},
                            {"pubkey":SWAP_INPUT_MINT,"isSigner":false,"isWritable":false},
                            {"pubkey":"11111111111111111111111111111111","isSigner":false,"isWritable":false},
                            {"pubkey":gofin_solana::token::TOKEN_PROGRAM_ID,"isSigner":false,"isWritable":false}
                        ],
                        "data":STANDARD.encode(create_wsol_ata_instruction(&payer, &account)?.data)
                    }));
                    input_cleanup_instruction = Some(json!({
                        "programId":gofin_solana::token::TOKEN_PROGRAM_ID,
                        "accounts":[
                            {"pubkey":source,"isSigner":false,"isWritable":true},
                            {"pubkey":self.wallet,"isSigner":false,"isWritable":true},
                            {"pubkey":self.wallet,"isSigner":true,"isWritable":false}
                        ],
                        "data":STANDARD.encode([9u8])
                    }));
                }
                setup_instructions.push(json!({
                    "programId":"11111111111111111111111111111111",
                    "accounts":[{"pubkey":self.wallet,"isSigner":true,"isWritable":true},{"pubkey":source,"isSigner":false,"isWritable":true}],
                    "data":STANDARD.encode([2u32.to_le_bytes().as_slice(),amount.to_le_bytes().as_slice()].concat())
                }));
                setup_instructions.push(json!({
                    "programId":gofin_solana::token::TOKEN_PROGRAM_ID,
                    "accounts":[{"pubkey":source,"isSigner":false,"isWritable":true}],
                    "data":STANDARD.encode([17u8])
                }));
            }
            let native_unwrap = query_parameter(path, "wrapAndUnwrapSol") == Some("true")
                && output_mint == SWAP_INPUT_MINT;
            if native_unwrap {
                let destination = gofin_solana::associated_token_address(
                    &self.wallet,
                    SWAP_INPUT_MINT,
                    gofin_solana::token::TOKEN_PROGRAM_ID,
                )?;
                let destination_exists = self
                    .rpc
                    .0
                    .lock()
                    .unwrap()
                    .account_data
                    .contains_key(&destination);
                if !destination_exists {
                    let payer = self.wallet.parse::<solana_pubkey::Pubkey>().map_err(|_| {
                        GoFinError::validation("The synthetic wallet address is invalid.")
                    })?;
                    let account = destination.parse::<solana_pubkey::Pubkey>().map_err(|_| {
                        GoFinError::validation("The synthetic wrapped-SOL account is invalid.")
                    })?;
                    setup_instructions.push(json!({
                        "programId":ASSOCIATED_TOKEN_PROGRAM,
                        "accounts":[
                            {"pubkey":self.wallet,"isSigner":true,"isWritable":true},
                            {"pubkey":destination,"isSigner":false,"isWritable":true},
                            {"pubkey":self.wallet,"isSigner":false,"isWritable":false},
                            {"pubkey":SWAP_INPUT_MINT,"isSigner":false,"isWritable":false},
                            {"pubkey":"11111111111111111111111111111111","isSigner":false,"isWritable":false},
                            {"pubkey":gofin_solana::token::TOKEN_PROGRAM_ID,"isSigner":false,"isWritable":false}
                        ],
                        "data":STANDARD.encode(create_wsol_ata_instruction(&payer, &account)?.data)
                    }));
                }
            } else {
                let token_program = mint_profile(output_mint).0;
                let destination = gofin_solana::associated_token_address(
                    &self.wallet,
                    output_mint,
                    token_program,
                )?;
                let destination_exists = self
                    .rpc
                    .0
                    .lock()
                    .unwrap()
                    .account_data
                    .contains_key(&destination);
                if !destination_exists {
                    let payer = self.wallet.parse::<solana_pubkey::Pubkey>().map_err(|_| {
                        GoFinError::validation("The synthetic wallet address is invalid.")
                    })?;
                    let account = destination.parse::<solana_pubkey::Pubkey>().map_err(|_| {
                        GoFinError::validation("The synthetic output account is invalid.")
                    })?;
                    setup_instructions.push(json!({
                        "programId":ASSOCIATED_TOKEN_PROGRAM,
                        "accounts":[
                            {"pubkey":self.wallet,"isSigner":true,"isWritable":true},
                            {"pubkey":destination,"isSigner":false,"isWritable":true},
                            {"pubkey":self.wallet,"isSigner":false,"isWritable":false},
                            {"pubkey":output_mint,"isSigner":false,"isWritable":false},
                            {"pubkey":"11111111111111111111111111111111","isSigner":false,"isWritable":false},
                            {"pubkey":token_program,"isSigner":false,"isWritable":false}
                        ],
                        "data":STANDARD.encode(create_wsol_ata_instruction(&payer, &account)?.data)
                    }));
                }
            }
            let cleanup_instruction = if native_unwrap {
                let destination = gofin_solana::associated_token_address(
                    &self.wallet,
                    SWAP_INPUT_MINT,
                    gofin_solana::token::TOKEN_PROGRAM_ID,
                )?;
                Some(json!({
                    "programId":gofin_solana::token::TOKEN_PROGRAM_ID,
                    "accounts":[{"pubkey":destination,"isSigner":false,"isWritable":true},{"pubkey":self.wallet,"isSigner":false,"isWritable":true},{"pubkey":self.wallet,"isSigner":true,"isWritable":false}],
                    "data":STANDARD.encode([9u8])
                }))
            } else {
                input_cleanup_instruction
            };
            let mut response = json!({
                "inputMint":input_mint,
                "outputMint":output_mint,
                "inAmount":amount.to_string(),
                "outAmount":output_amount.to_string(),
                "otherAmountThreshold":(u128::from(output_amount)*9950/10000).to_string(),
                "swapMode":"ExactIn",
                "slippageBps":50,
                "computeBudgetInstructions":[],
                "setupInstructions":setup_instructions,
                "swapInstruction":{
                    "programId":gofin_jupiter::swap::JUPITER_V6_PROGRAM,
                    "accounts":accounts,
                    "data":STANDARD.encode(&swap_instruction.data)
                },
                "cleanupInstruction":cleanup_instruction,
                "otherInstructions":[],
                "tipInstruction":null,
                "addressesByLookupTableAddress":null,
                "blockhashWithMetadata":{"blockhash":blockhash.to_bytes().to_vec(),"lastValidBlockHeight":1000}
            });
            // Exercise local size rejection followed by a valid constrained
            // Router build through the real public CLI execution workflow.
            if let Ok(marker) = std::env::var("GOFIN_DEV_SOLANA_FIXTURE_OVERSIZED_ROUTER_FILE") {
                let marker = std::path::Path::new(&marker);
                if marker.is_file() {
                    let mut limits: Vec<Value> = serde_json::from_slice(
                        &std::fs::read(marker).map_err(|_| fixture_state_error())?,
                    )
                    .map_err(|_| fixture_state_error())?;
                    limits.push(json!(query_parameter(path, "maxAccounts")));
                    std::fs::write(
                        marker,
                        serde_json::to_vec(&limits).map_err(|_| fixture_state_error())?,
                    )
                    .map_err(|_| fixture_state_error())?;
                    if limits.len() == 1 {
                        response["swapInstruction"]["accounts"]
                            .as_array_mut()
                            .ok_or_else(fixture_state_error)?
                            // Keep the real, decodable Jupiter route data and
                            // make only the message account-key table large.
                            // The first response should fail during local
                            // wire-size compilation, before approval or send.
                            .extend((100..=129u8).map(|index| {
                                json!({
                                    "pubkey":bs58::encode([index;32]).into_string(),
                                    "isSigner":false,"isWritable":false
                                })
                            }));
                    }
                }
            }
            return Ok(response);
        }
        if path.starts_with("/swap/v2/order?") && method == Method::GET {
            self.rpc.begin_expiry_candidate()?;
            let amount = path
                .split('&')
                .find_map(|part| part.strip_prefix("amount="))
                .and_then(|amount| amount.parse::<u64>().ok())
                .ok_or_else(|| GoFinError::validation("The synthetic swap amount is malformed."))?;
            let input_mint = query_parameter(path, "inputMint").unwrap_or(SWAP_INPUT_MINT);
            let output_mint = query_parameter(path, "outputMint").unwrap_or(SWAP_OUTPUT_MINT);
            let output_amount = swap_output_amount(amount, input_mint, output_mint)?;
            let blockhash = self.rpc.next_fixture_blockhash()?;
            let fallback_trace = std::env::var_os("GOFIN_DEV_SOLANA_FIXTURE_META_FALLBACK_FILE")
                .map(PathBuf::from)
                .filter(|path| path.is_file());
            if let Some(trace) = &fallback_trace {
                let mut requests: Vec<Value> = serde_json::from_slice(
                    &std::fs::read(trace).map_err(|_| fixture_state_error())?,
                )
                .map_err(|_| fixture_state_error())?;
                requests.push(json!({
                    "inputMint":query_parameter(path,"inputMint"),
                    "outputMint":query_parameter(path,"outputMint"),
                    "amount":query_parameter(path,"amount"),
                    "taker":query_parameter(path,"taker"),
                    "slippageBps":query_parameter(path,"slippageBps"),
                    "excludeRouters":query_parameter(path,"excludeRouters")
                }));
                std::fs::write(
                    trace,
                    serde_json::to_vec(&requests).map_err(|_| fixture_state_error())?,
                )
                .map_err(|_| fixture_state_error())?;
            }
            let router_selector = self
                .rpc
                .take_next_meta_router()?
                .unwrap_or_else(|| "metis".to_owned());
            let jupiterz = router_selector.starts_with("jupiterz");
            let router = if jupiterz {
                "jupiterz"
            } else {
                router_selector.as_str()
            };
            let expire_at = jupiterz.then(|| chrono::Utc::now().timestamp() + 300);
            let layout = match router_selector.as_str() {
                "jupiterz_native_input" => FixtureSwapLayout::JupiterZNativeInput,
                "jupiterz_native_output" => FixtureSwapLayout::JupiterZNativeOutput,
                "jupiterz" => FixtureSwapLayout::JupiterZ,
                _ => FixtureSwapLayout::selected(path, amount, input_mint, output_mint),
            };
            let request_id =
                format!("synthetic-order-{blockhash}-{amount}-{input_mint}-{output_mint}");
            let malformed_marker =
                std::env::var_os("GOFIN_DEV_SOLANA_FIXTURE_META_UNSUPPORTED_FILE")
                    .map(PathBuf::from)
                    .filter(|path| path.is_file());
            let mut transaction = self.order_transaction(
                amount,
                input_mint,
                output_mint,
                blockhash,
                layout,
                expire_at,
            )?;
            if let Some(marker) = malformed_marker {
                std::fs::remove_file(marker).map_err(|_| fixture_state_error())?;
                transaction = "private-not-base64".to_owned();
            }
            return Ok(json!({
                "transaction":transaction,
                "requestId":request_id,
                "outAmount":output_amount.to_string(),
                "router":router,
                "mode":"ExactIn",
                "lastValidBlockHeight":if expire_at.is_some() { Value::Null } else { json!(1000) },
                "feeBps":0,
                "feeMint":null,
                "expireAt":expire_at.map(|value| value.to_string())
            }));
        }
        if path == "/swap/v2/execute" && method == Method::POST {
            let body = body.ok_or_else(|| {
                GoFinError::validation("The synthetic order execution body is missing.")
            })?;
            let encoded = body
                .get("signedTransaction")
                .and_then(Value::as_str)
                .ok_or_else(|| GoFinError::validation("The synthetic signed order is missing."))?;
            if body.get("requestId").and_then(Value::as_str).is_none() {
                return Err(GoFinError::validation(
                    "The synthetic order identity is missing.",
                ));
            }
            self.rpc.0.lock().unwrap().meta_execution_in_progress = true;
            let submitted = self.rpc.submit(encoded);
            self.rpc.0.lock().unwrap().meta_execution_in_progress = false;
            let signature = submitted?;
            let request_id = body
                .get("requestId")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    GoFinError::validation("The synthetic order identity is missing.")
                })?;
            self.rpc
                .lose_meta_execute_response(&signature, request_id)?;
            return Ok(json!({"status":"Success","signature":signature}));
        }
        Err(GoFinError::new(
            "JUPITER_FIXTURE_ROUTE_UNAVAILABLE",
            "provider",
            "The selected synthetic Jupiter fixture does not implement this route yet.",
            "Use a route explicitly covered by the rehearsal fixture.",
        ))
    }
}

#[cfg(test)]
mod focused_adapter_tests {
    use super::*;
    use gofin_domain::execution_state::ExecutionState;
    use gofin_domain::{Adapter, OperationRecord, SnapshotScope, Submission, SubmissionJournal};
    use std::collections::BTreeMap;

    #[test]
    fn transfer_lost_response_selector_does_not_match_sync_native_wrap() {
        use solana_instruction::{AccountMeta, Instruction};
        use solana_message::{Message, VersionedMessage};
        use solana_pubkey::Pubkey;
        use solana_signature::Signature;
        use solana_transaction::versioned::VersionedTransaction;

        fn stored(instructions: Vec<Instruction>, signature: &str) -> StoredTransaction {
            let payer = Pubkey::new_from_array([61; 32]);
            let message = Message::new_with_blockhash(
                &instructions,
                Some(&payer),
                &solana_hash::Hash::new_from_array([62; 32]),
            );
            let transaction = VersionedTransaction {
                signatures: vec![Signature::default()],
                message: VersionedMessage::Legacy(message),
            };
            let wire = bincode::DefaultOptions::new()
                .with_fixint_encoding()
                .with_little_endian()
                .serialize(&transaction)
                .expect("synthetic transaction serialization");
            StoredTransaction {
                signature: signature.to_owned(),
                wire: STANDARD.encode(wire),
                pre_balances: vec![],
                post_balances: vec![],
                pre_token_balances: vec![],
                post_token_balances: vec![],
                logs: vec![],
                inner_instructions: vec![],
            }
        }

        let payer = Pubkey::new_from_array([61; 32]);
        let destination = Pubkey::new_from_array([63; 32]);
        let system = "11111111111111111111111111111111"
            .parse::<Pubkey>()
            .expect("system program");
        let token = gofin_solana::token::TOKEN_PROGRAM_ID
            .parse::<Pubkey>()
            .expect("token program");
        let mut transfer_data = 2u32.to_le_bytes().to_vec();
        transfer_data.extend_from_slice(&100_000u64.to_le_bytes());
        let transfer = Instruction {
            program_id: system,
            accounts: vec![
                AccountMeta::new(payer, true),
                AccountMeta::new(destination, false),
            ],
            data: transfer_data,
        };
        let plain = stored(vec![transfer.clone()], "plain-transfer");
        assert!(transaction_amount_matches(
            std::slice::from_ref(&plain),
            "plain-transfer",
            100_000
        ));

        let sync_native = Instruction {
            program_id: token,
            accounts: vec![AccountMeta::new(destination, false)],
            data: vec![17],
        };
        let wrap = stored(vec![transfer, sync_native], "wrap");
        assert!(!transaction_amount_matches(
            std::slice::from_ref(&wrap),
            "wrap",
            100_000
        ));
    }

    #[test]
    fn meta_lost_response_selector_targets_unique_request_id_suffix() {
        let selector = format!("synthetic-order-1000000-{PYUSD_MINT}-{SWAP_OUTPUT_MINT}");
        assert!(meta_request_selector_matches(
            &selector,
            &format!("synthetic-order-unique-blockhash-1000000-{PYUSD_MINT}-{SWAP_OUTPUT_MINT}")
        ));
        assert!(!meta_request_selector_matches(
            &selector,
            &format!("synthetic-order-other-hash-2000000-{PYUSD_MINT}-{SWAP_OUTPUT_MINT}")
        ));
    }

    #[derive(Default)]
    struct RehearsalJournal {
        submission: Mutex<Option<Submission>>,
        dispatch_evidence: Mutex<Option<Value>>,
    }

    #[derive(Default)]
    struct RehearsalExecutionState {
        operations: Mutex<BTreeMap<String, OperationRecord>>,
        recovery_children: Mutex<BTreeMap<String, String>>,
        nonce: Mutex<u64>,
    }

    impl RehearsalExecutionState {
        fn save(&self, record: OperationRecord, parent: Option<&str>) {
            if let Some(parent) = parent {
                self.recovery_children
                    .lock()
                    .expect("recovery links")
                    .insert(parent.to_owned(), record.id.clone());
            }
            self.operations
                .lock()
                .expect("durable fixture operations")
                .insert(record.id.clone(), record);
        }
    }

    impl ExecutionState for RehearsalExecutionState {
        fn allocate_nonce(&self, _signer_scope: &str, _now_ms: u64) -> Result<u64> {
            let mut nonce = self.nonce.lock().expect("fixture nonce");
            *nonce = nonce
                .checked_add(1)
                .ok_or_else(|| GoFinError::validation("Synthetic nonce overflow."))?;
            Ok(*nonce)
        }

        fn operation(&self, operation_id: &str) -> Result<Option<OperationRecord>> {
            Ok(self
                .operations
                .lock()
                .expect("durable fixture operations")
                .get(operation_id)
                .cloned())
        }

        fn operations_by_prepared_binding(
            &self,
            operation: &str,
            binding_pointer: &str,
            value: &Value,
        ) -> Result<Vec<OperationRecord>> {
            Ok(self
                .operations
                .lock()
                .expect("durable fixture operations")
                .values()
                .filter(|record| {
                    record.operation == operation
                        && record
                            .preview
                            .as_ref()
                            .and_then(|preview| preview.binding.pointer(binding_pointer))
                            == Some(value)
                })
                .cloned()
                .collect())
        }

        fn recovery_child(&self, parent_operation_id: &str) -> Result<Option<OperationRecord>> {
            let child = self
                .recovery_children
                .lock()
                .expect("recovery links")
                .get(parent_operation_id)
                .cloned();
            Ok(child.and_then(|id| {
                self.operations
                    .lock()
                    .expect("durable fixture operations")
                    .get(&id)
                    .cloned()
            }))
        }
    }

    impl SubmissionJournal for RehearsalJournal {
        fn assert_authorized(&self) -> Result<()> {
            Ok(())
        }

        fn persist_submission(&self, submission: &Submission) -> Result<()> {
            *self.submission.lock().expect("submission lock") = Some(submission.clone());
            Ok(())
        }

        fn persist_dispatch_evidence(
            &self,
            _submission: &Submission,
            evidence: &Value,
        ) -> Result<()> {
            *self
                .dispatch_evidence
                .lock()
                .expect("dispatch evidence lock") = Some(evidence.clone());
            Ok(())
        }
    }

    async fn execute_and_reconcile(
        adapter: &dyn Adapter,
        operation: &str,
        input: Value,
        id: &str,
    ) -> Result<gofin_domain::Reconciliation> {
        let prepared = adapter.prepare(operation, &input).await?;
        let prepared = adapter.revalidate(&prepared).await?;
        let journal = Arc::new(RehearsalJournal::default());
        let submission = adapter.submit(&prepared, journal.clone()).await?;
        let now = chrono::Utc::now().to_rfc3339();
        let record = gofin_domain::OperationRecord {
            id: id.into(),
            request_id: format!("{id}-request"),
            fingerprint: format!("{id}-fingerprint"),
            fingerprint_version: 1,
            operation: operation.into(),
            input,
            status: gofin_domain::Status::Submitted,
            created_at: now.clone(),
            updated_at: now,
            preview: Some(prepared),
            approval: None,
            submission: Some(submission),
            dispatch_evidence: journal
                .dispatch_evidence
                .lock()
                .expect("dispatch evidence lock")
                .clone(),
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
            position_baseline: None,
        };
        adapter.reconcile(&record).await
    }

    fn operation_record(
        id: &str,
        operation: &str,
        input: Value,
        preview: gofin_domain::PreparedOperation,
        journal: &RehearsalJournal,
    ) -> gofin_domain::OperationRecord {
        let now = chrono::Utc::now().to_rfc3339();
        gofin_domain::OperationRecord {
            id: id.into(),
            request_id: format!("{id}-request"),
            fingerprint: format!("{id}-fingerprint"),
            fingerprint_version: 1,
            operation: operation.into(),
            input,
            status: gofin_domain::Status::Submitted,
            created_at: now.clone(),
            updated_at: now,
            preview: Some(preview),
            approval: None,
            submission: journal.submission.lock().expect("submission lock").clone(),
            dispatch_evidence: journal
                .dispatch_evidence
                .lock()
                .expect("dispatch evidence lock")
                .clone(),
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
            position_baseline: None,
        }
    }

    fn jlp_adapter(applications: &gofin_broker::ProtectedApplications) -> &dyn Adapter {
        applications
            .adapters
            .iter()
            .find(|adapter| {
                adapter
                    .definitions()
                    .iter()
                    .any(|definition| definition.name == "jupiter.jlp.mint")
            })
            .expect("JLP adapter")
            .as_ref()
    }

    fn trigger_adapter(applications: &gofin_broker::ProtectedApplications) -> &dyn Adapter {
        applications
            .adapters
            .iter()
            .find(|adapter| {
                adapter
                    .definitions()
                    .iter()
                    .any(|definition| definition.name == "jupiter.limit.create")
            })
            .expect("Trigger adapter")
            .as_ref()
    }

    #[tokio::test]
    async fn trigger_order_fixture_matches_expiry_and_exact_plan_amounts() {
        let _guard = FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (_, applications) = applications().expect("synthetic fixture applications");
        let adapter = trigger_adapter(&applications);
        let expired_at = chrono::Utc::now().timestamp() + 1800;
        let prepared = adapter
            .prepare(
                "jupiter.limit.create",
                &json!({
                    "wallet":"solana-rehearsal",
                    "network":"solana",
                    "cluster":"mainnet-beta",
                    "input_mint":SWAP_OUTPUT_MINT,
                    "output_mint":SWAP_INPUT_MINT,
                    "making_amount_atoms":"100000",
                    "taking_amount_atoms":"99000",
                    "expired_at_unix":expired_at,
                    "maximum_network_fee_lamports":"10000",
                    "maximum_account_funding_lamports":"5000000"
                }),
            )
            .await
            .unwrap_or_else(|error| panic!("Trigger fixture prepare failed: {error}"));
        assert_eq!(prepared.binding["amount_atoms"], "100000");
        assert_eq!(prepared.binding["output_atoms"], "99000");
        assert!(prepared.binding["order_key"].as_str().is_some());
    }

    #[tokio::test]
    async fn trigger_execute_fixture_submits_original_signed_order_and_remains_pending() {
        let _guard = FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (_, applications) = applications().expect("synthetic fixture applications");
        let adapter = trigger_adapter(&applications);
        let input = json!({
            "wallet":"solana-rehearsal",
            "network":"solana",
            "cluster":"mainnet-beta",
            "input_mint":SWAP_OUTPUT_MINT,
            "output_mint":SWAP_INPUT_MINT,
            "making_amount_atoms":"100000",
            "taking_amount_atoms":"99000",
            "maximum_network_fee_lamports":"10000",
            "maximum_account_funding_lamports":"5000000"
        });
        let expected_order = adapter
            .prepare("jupiter.limit.create", &input)
            .await
            .expect("Trigger create prepares for status check")
            .binding["order_key"]
            .as_str()
            .expect("prepared order PDA")
            .to_owned();
        let reconciliation = execute_and_reconcile(
            adapter,
            "jupiter.limit.create",
            input,
            "trigger-execute-fixture",
        )
        .await
        .unwrap_or_else(|error| panic!("Trigger execution/reconciliation failed: {error}"));
        assert_eq!(reconciliation.status, gofin_domain::Status::Pending);
        assert!(!reconciliation.evidence.is_null());
        let read = adapter
            .read(
                "jupiter.limit.status",
                &json!({
                    "wallet":"solana-rehearsal",
                    "network":"solana",
                    "cluster":"mainnet-beta",
                    "order_key":expected_order
                }),
            )
            .await
            .unwrap_or_else(|error| panic!("Trigger finalized order read failed: {error}"));
        assert_eq!(read["state"], "open_escrowed", "read={read}");
        assert_eq!(read["remaining_making_atoms"], "100000", "read={read}");
    }

    #[tokio::test]
    async fn trigger_cancel_fixture_completes_within_bounded_time_and_refunds_open_escrow() {
        let _guard = FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (_, applications, _, _) =
            applications_with_fixture().expect("synthetic fixture applications");
        let adapter = trigger_adapter(&applications);
        let execution = Arc::new(RehearsalExecutionState::default());
        adapter
            .attach_execution_state(execution.clone())
            .expect("attach durable synthetic execution state");
        let create_input = json!({
            "wallet":"solana-rehearsal",
            "network":"solana",
            "cluster":"mainnet-beta",
            "input_mint":SWAP_OUTPUT_MINT,
            "output_mint":SWAP_INPUT_MINT,
            "making_amount_atoms":"100000",
            "taking_amount_atoms":"99000",
            "maximum_network_fee_lamports":"10000",
            "maximum_account_funding_lamports":"5000000"
        });
        let create_preview = adapter
            .prepare("jupiter.limit.create", &create_input)
            .await
            .expect("Trigger placement prepares");
        let order_key = create_preview.binding["order_key"]
            .as_str()
            .expect("exact Trigger order PDA")
            .to_owned();
        let create_preview = adapter
            .revalidate(&create_preview)
            .await
            .expect("Trigger placement revalidates");
        let create_journal = Arc::new(RehearsalJournal::default());
        adapter
            .submit(&create_preview, create_journal.clone())
            .await
            .expect("Trigger create executes through the fixture sender");
        let create_record = operation_record(
            "trigger-create-cancel-fixture",
            "jupiter.limit.create",
            create_input,
            create_preview,
            &create_journal,
        );
        let created = adapter
            .reconcile(&create_record)
            .await
            .expect("finalized Trigger placement reconciles");
        assert_eq!(created.status, gofin_domain::Status::Pending);
        assert!(
            created.verified.is_some(),
            "a finalized placement should persist its verified funding progress"
        );
        let mut create_record = create_record;
        create_record.status = created.status.clone();
        create_record.effects = created.verified.clone();
        create_record.last_reconciliation_evidence = Some(created.evidence.clone());
        execution.save(create_record.clone(), None);

        let cancel_input = json!({
            "wallet":"solana-rehearsal",
            "network":"solana",
            "cluster":"mainnet-beta",
            "order_key":order_key,
            "original_operation_id":create_record.id,
            "original_submission":serde_json::to_value(create_record.submission.as_ref().expect("durable create submission")).expect("submission JSON"),
            "maximum_network_fee_lamports":"10000",
            "maximum_account_funding_lamports":"0"
        });
        let cancelled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let preview = adapter
                .prepare("jupiter.limit.cancel", &cancel_input)
                .await?;
            let preview = adapter.revalidate(&preview).await?;
            let journal = Arc::new(RehearsalJournal::default());
            adapter.submit(&preview, journal.clone()).await?;
            let record = operation_record(
                "trigger-cancel-fixture",
                "jupiter.limit.cancel",
                cancel_input,
                preview,
                &journal,
            );
            execution.save(record.clone(), Some(&create_record.id));
            adapter.reconcile(&record).await
        })
        .await
        .expect("Trigger cancel preparation and execute must not block on a fixture lock")
        .expect("Trigger cancel settles from its finalized proof");
        assert_eq!(cancelled.status, gofin_domain::Status::Completed);
        let original = adapter
            .reconcile(&create_record)
            .await
            .expect("original Trigger placement reconciles after cancellation");
        assert_eq!(original.status, gofin_domain::Status::Completed);
        assert!(original.evidence["settlement_progress"].is_object());
    }

    #[tokio::test]
    async fn jlp_usdc_fixture_candidate_passes_production_prepare() {
        let _guard = FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (_, applications) = applications().expect("synthetic fixture applications");
        let adapter = applications
            .adapters
            .iter()
            .find(|adapter| {
                adapter
                    .definitions()
                    .iter()
                    .any(|definition| definition.name == "jupiter.jlp.mint")
            })
            .expect("JLP adapter");
        let prepared = adapter
            .prepare(
                "jupiter.jlp.mint",
                &json!({
                    "network":"solana",
                    "cluster":"mainnet-beta",
                    "wallet":"solana-rehearsal",
                    "deposit_token":"USDC",
                    "amount_atoms":"1000000",
                    "minimum_jlp_atoms":"990000",
                    "maximum_network_fee_lamports":"10000",
                    "maximum_account_funding_lamports":"0"
                }),
            )
            .await
            .unwrap_or_else(|error| panic!("JLP fixture prepare failed: {error}"));
        assert_eq!(prepared.binding["input_atoms"], "1000000");
    }

    #[tokio::test]
    async fn jlp_usdc_and_native_roundtrips_settle_from_synthetic_receipt_proofs() {
        let _guard = FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (_, applications) = applications().expect("synthetic fixture applications");
        let adapter = jlp_adapter(&applications);
        let cases = [
            (
                "jupiter.jlp.mint",
                json!({"network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal","deposit_token":"USDC","amount_atoms":"1000000","minimum_jlp_atoms":"990000","maximum_network_fee_lamports":"10000","maximum_account_funding_lamports":"0"}),
                "jlp-usdc-mint",
            ),
            (
                "jupiter.jlp.burn",
                json!({"network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal","receive_token":"USDC","jlp_atoms":"1000000","minimum_receive_atoms":"990000","maximum_network_fee_lamports":"10000","maximum_account_funding_lamports":"0"}),
                "jlp-usdc-burn",
            ),
            (
                "jupiter.jlp.mint",
                json!({"network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal","deposit_token":"SOL","amount_atoms":"20000000","minimum_jlp_atoms":"1980000","maximum_network_fee_lamports":"10000","maximum_account_funding_lamports":"0"}),
                "jlp-sol-mint",
            ),
            (
                "jupiter.jlp.burn",
                json!({"network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal","receive_token":"SOL","jlp_atoms":"2000000","minimum_receive_atoms":"19800000","maximum_network_fee_lamports":"10000","maximum_account_funding_lamports":"0"}),
                "jlp-sol-burn",
            ),
        ];
        for (operation, input, id) in cases {
            let result = execute_and_reconcile(adapter, operation, input, id)
                .await
                .unwrap_or_else(|error| panic!("{id} fixture lifecycle failed: {error}"));
            assert_eq!(result.status, gofin_domain::Status::Completed, "{id}");
            let verified = result.verified.expect("finalized JLP effects");
            assert!(!verified.proof_digest.is_empty(), "{id}");
            assert!(
                verified.effects.iter().any(|effect| matches!(
                    effect,
                    gofin_domain::FinancialEffect::Conversion { .. }
                )),
                "{id}"
            );
        }
    }

    #[tokio::test]
    async fn router_lost_response_selector_is_bound_to_exact_swap_identity() {
        let _guard = FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let wallet = SolanaSigner::parse(SYNTHETIC_KEYPAIR)
            .expect("synthetic signer")
            .address()
            .to_owned();
        let rpc = Arc::new(FixtureRpc::new(&wallet).expect("synthetic fixture RPC"));
        let api = FixtureJupiterApi {
            wallet: wallet.clone(),
            rpc,
        };
        let transaction = api
            .order_transaction(
                1_000_000,
                PYUSD_MINT,
                SWAP_OUTPUT_MINT,
                api.rpc.next_fixture_blockhash().unwrap(),
                FixtureSwapLayout::Legacy,
                None,
            )
            .expect("exact target transaction");
        let independently_prepared = api
            .order_transaction(
                1_000_000,
                PYUSD_MINT,
                SWAP_OUTPUT_MINT,
                api.rpc.next_fixture_blockhash().unwrap(),
                FixtureSwapLayout::Legacy,
                None,
            )
            .expect("second independent transaction");
        assert_ne!(
            transaction, independently_prepared,
            "different fixture operations must not reuse a canonical message identity"
        );
        let signature = "synthetic-selector-signature";
        let stored = StoredTransaction {
            signature: signature.into(),
            wire: transaction,
            pre_balances: vec![],
            post_balances: vec![],
            pre_token_balances: vec![],
            post_token_balances: vec![],
            logs: vec![],
            inner_instructions: vec![],
        };
        let transactions = vec![stored];
        assert!(transaction_swap_matches(
            &transactions,
            signature,
            &wallet,
            1_000_000,
            PYUSD_MINT,
            SWAP_OUTPUT_MINT,
        ));
        assert!(!transaction_swap_matches(
            &transactions,
            signature,
            &wallet,
            2_000_000,
            PYUSD_MINT,
            SWAP_OUTPUT_MINT,
        ));
        assert!(!transaction_swap_matches(
            &transactions,
            signature,
            &wallet,
            1_000_000,
            SWAP_OUTPUT_MINT,
            PYUSD_MINT,
        ));
    }

    #[tokio::test]
    async fn initial_perps_position_observer_is_complete_and_empty() {
        let _guard = FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (_, applications) = applications().expect("synthetic fixture applications");
        let adapter = applications
            .adapters
            .iter()
            .find(|adapter| {
                adapter
                    .definitions()
                    .iter()
                    .any(|definition| definition.name == "jupiter.perps.positions")
            })
            .expect("Perps adapter");
        let address = SolanaSigner::parse(SYNTHETIC_KEYPAIR)
            .expect("synthetic key")
            .address()
            .to_owned();
        let scope = SnapshotScope {
            resources: vec![ResourceId(format!("solana:mainnet-beta:{address}"))],
            networks: Vec::new(),
            assets: Vec::new(),
        };
        let snapshot = adapter
            .snapshot_scoped(&scope)
            .await
            .expect("initial Perps observer snapshot");
        assert!(snapshot.complete, "pending={:?}", snapshot.pending);
        assert!(snapshot.pending.is_empty());
        assert!(snapshot.state.positions.is_empty());
    }

    #[tokio::test]
    async fn every_registered_position_observer_is_complete_for_initial_wallet_scope() {
        let _guard = FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (_, applications) = applications().expect("synthetic fixture applications");
        let address = SolanaSigner::parse(SYNTHETIC_KEYPAIR)
            .expect("synthetic key")
            .address()
            .to_owned();
        let resource = ResourceId(format!("solana:mainnet-beta:{address}"));
        let scope = SnapshotScope {
            resources: vec![resource.clone()],
            networks: Vec::new(),
            assets: Vec::new(),
        };
        for adapter in &applications.adapters {
            if !adapter
                .snapshot_identities()
                .iter()
                .any(|identity| identity.resource == resource)
            {
                continue;
            }
            let names = adapter
                .definitions()
                .into_iter()
                .map(|definition| definition.name)
                .collect::<Vec<_>>();
            let snapshot = adapter
                .snapshot_scoped(&scope)
                .await
                .unwrap_or_else(|error| panic!("observer {names:?} failed: {error}"));
            assert!(
                snapshot.complete && snapshot.pending.is_empty(),
                "observer {names:?} incomplete: coverage={:?} pending={:?}",
                snapshot.coverage,
                snapshot.pending
            );
        }
    }
}
