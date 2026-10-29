//! Reviewed CCTP V2 direct native-USDC burns and explicit destination mints.
//! The broker owns submission identities, authority, finality and persistence.
//! Circle's signed message changes four fields relative to MessageSent; source
//! proof is therefore bound using the immutable message, never raw hash equality.
pub mod hypercore;
pub mod hyperevm;
pub mod solana;
use crate::canonical::CanonicalRawLog;
use crate::*;
use alloy::{
    primitives::{Address, B256, Bytes, Signature, U256, keccak256},
    sol,
    sol_types::{SolCall, SolEvent},
};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use gofin_evm::{EvmRuntime, address, hash, hex_bytes, quantity, rpc_bytes, rpc_u64, u256};
use num_bigint::BigUint;
use num_traits::Zero;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};

pub const PROVIDER_ID: &str = "cctp";
pub const TOKEN_MESSENGER: &str = "0x28b5a0e9c621a5badaa536219b3a228c8168cf5d";
pub const MESSAGE_TRANSMITTER: &str = "0x81d40f21f12a8f0e3252bccb954d722d4c464b64";
pub const TOKEN_MINTER: &str = "0xfd78ee919681417d192449715b2594ab58f5d002";
pub const IRIS_URL: &str = "https://iris-api.circle.com";
const MESSAGE_LEN: usize = 376;
const RESPONSE_LIMIT: usize = 1024 * 1024;

sol! {
    #[allow(clippy::too_many_arguments)]
    interface Messenger {
        function localMessageTransmitter() external view returns (address);
        function localMinter() external view returns (address);
        function messageBodyVersion() external view returns (uint32);
        function remoteTokenMessengers(uint32 remoteDomain) external view returns (bytes32);
        function depositForBurn(uint256 amount, uint32 destinationDomain, bytes32 mintRecipient, address burnToken, bytes32 destinationCaller, uint256 maxFee, uint32 minFinalityThreshold) external;
        event DepositForBurn(address indexed burnToken, uint256 amount, address indexed depositor, bytes32 mintRecipient, uint32 destinationDomain, bytes32 destinationTokenMessenger, bytes32 destinationCaller, uint256 maxFee, uint32 indexed minFinalityThreshold, bytes hookData);
    }
    interface Transmitter {
        function localDomain() external view returns (uint32);
        function version() external view returns (uint32);
        function receiveMessage(bytes message, bytes attestation) external returns (bool);
        function usedNonces(bytes32 nonce) external view returns (uint256);
        function signatureThreshold() external view returns (uint256);
        function isEnabledAttester(address attester) external view returns (bool);
        event MessageSent(bytes message);
        event MessageReceived(address indexed caller, uint32 sourceDomain, bytes32 indexed nonce, bytes32 sender, uint32 indexed finalityThresholdExecuted, bytes messageBody);
    }
    interface Token {
        function decimals() external view returns (uint8);
        function allowance(address owner, address spender) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
        event Transfer(address indexed from, address indexed to, uint256 value);
    }
    interface Minter {
        function getLocalToken(uint32 remoteDomain, bytes32 remoteToken) external view returns (address);
        function localTokenMessenger() external view returns (address);
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CctpTransferMode {
    #[default]
    Standard,
    Fast,
}
impl CctpTransferMode {
    pub const fn threshold(self) -> u32 {
        match self {
            Self::Standard => 2000,
            Self::Fast => 1000,
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CctpOptions {
    #[serde(default)]
    pub transfer_mode: CctpTransferMode,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CctpQuoteMetadata {
    pub protocol_version: u32,
    pub source_domain: u32,
    pub destination_domain: u32,
    pub transfer_mode: CctpTransferMode,
    pub max_fee: Quantity,
    pub min_finality_threshold: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CctpConfig {
    pub iris_url: String,
}
impl CctpConfig {
    pub fn reviewed() -> Self {
        Self {
            iris_url: IRIS_URL.into(),
        }
    }
    pub fn validate(&self) -> Result<()> {
        let url = reqwest::Url::parse(&self.iris_url).map_err(|_| invalid("Invalid Iris URL."))?;
        ensure(
            self.iris_url == IRIS_URL
                || (url.scheme() == "http"
                    && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))),
            "BRIDGE_CONFIG_INVALID",
            "configuration",
            "Iris must use the reviewed HTTPS origin or an isolated loopback fixture.",
        )
    }
}
pub fn domain(chain: u64) -> Result<u32> {
    match chain {
        1 => Ok(0),
        8453 => Ok(6),
        42161 => Ok(3),
        999 => Ok(19),
        _ => Err(invalid(
            "CCTP is reviewed only for Ethereum, Base, Arbitrum One and HyperEVM.",
        )),
    }
}
pub fn usdc_asset(chain: u64) -> Result<EvmAsset> {
    let token = match chain {
        1 => "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
        8453 => "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",
        42161 => "0xaf88d065e77c8cc2239327c5edb3a432268e5831",
        999 => "0xb88339cb7199b77e23db6e890353e22632ba630f",
        _ => {
            return Err(invalid(
                "Network has no reviewed CCTP native USDC deployment.",
            ));
        }
    };
    Ok(EvmAsset {
        chain_id: chain,
        token: Some(token.into()),
        decimals: 6,
    })
}
fn invalid(message: &str) -> GoFinError {
    bridge_error(
        "CCTP_VALIDATION_FAILED",
        "bridge",
        message,
        "Inspect the original transfer and its verified evidence; do not repeat a burn.",
    )
}
fn check(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(invalid(message))
    }
}
fn bytes(value: &str) -> Result<Vec<u8>> {
    Ok(rpc_bytes(&json!(value))?.to_vec())
}
fn destination_domain(intent: &BridgeIntent) -> Result<u32> {
    if intent.solana_destination.is_some() {
        Ok(solana::DOMAIN)
    } else {
        domain(intent.evm_destination_chain_id()?)
    }
}
fn destination_asset(intent: &BridgeIntent) -> Result<AssetId> {
    if let Some(d) = &intent.solana_destination {
        Ok(d.asset.clone())
    } else {
        Ok(usdc_asset(intent.evm_destination_chain_id()?)?.asset_id())
    }
}
fn destination_amount(intent: &BridgeIntent, atoms: Quantity) -> Result<Amount> {
    Ok(Amount {
        asset: destination_asset(intent)?,
        atoms,
    })
}
fn destination_word(intent: &BridgeIntent, kind: u8) -> Result<B256> {
    if let Some(d) = &intent.solana_destination {
        Ok(B256::from(solana::pubkey_bytes(match kind {
            0 => solana::TOKEN_MESSENGER_MINTER,
            1 => &d.destination_caller,
            _ => &d.recipient_token_account,
        })?))
    } else {
        Ok(address(match kind {
            0 => TOKEN_MESSENGER,
            1 => &intent.source_address,
            _ => &intent.recipient,
        })?
        .into_word())
    }
}
fn message_word(value: &str, destination_domain: u32) -> Result<B256> {
    if destination_domain == solana::DOMAIN {
        Ok(B256::from(solana::pubkey_bytes(value)?))
    } else {
        Ok(address(value)?.into_word())
    }
}
fn route_id(intent: &BridgeIntent) -> Result<String> {
    Ok(format!(
        "cctp-v2:{}:{}",
        intent.evm_source()?.chain_id,
        match intent.destination_chain_id {
            Some(id) => id.to_string(),
            None => intent.destination_network()?,
        }
    ))
}
fn destination_gas_asset(intent: &BridgeIntent) -> Result<AssetId> {
    if intent.solana_destination.is_some() {
        Ok(AssetId {
            network: gofin_solana::NETWORK.into(),
            reference: "native".into(),
            decimals: 9,
        })
    } else {
        Ok(EvmAsset {
            chain_id: intent.evm_destination_chain_id()?,
            token: None,
            decimals: 18,
        }
        .asset_id())
    }
}
fn destination_payer(intent: &BridgeIntent) -> &str {
    intent
        .solana_destination
        .as_ref()
        .map_or(&intent.source_address, |d| &d.wallet_address)
}
fn validate_intent(intent: &BridgeIntent) -> Result<()> {
    check(
        intent.source.as_ref() == Some(&usdc_asset(intent.evm_source()?.chain_id)?),
        "CCTP requires native USDC source.",
    )?;
    if let Some(d) = &intent.solana_destination {
        check(
            intent.destination_chain_id.is_none()
                && intent.destination_assets.is_empty()
                && intent.recipient == d.recipient,
            "Ambiguous Solana CCTP destination.",
        )?;
        solana::validate_destination(d)?;
        u64::try_from(u256(&intent.amount)?)
            .map_err(|_| invalid("Solana CCTP amount exceeds u64."))?;
    } else {
        let chain = intent.evm_destination_chain_id()?;
        check(
            chain != intent.evm_source()?.chain_id
                && intent.destination_assets == vec![usdc_asset(chain)?]
                && address(&intent.recipient)? != Address::ZERO,
            "CCTP requires distinct reviewed native-USDC networks.",
        )?;
    }
    check(
        !intent.amount.is_zero() && address(&intent.source_address)? != Address::ZERO,
        "CCTP requires a positive amount and nonzero sender.",
    )?;
    u256(&intent.amount)?;
    check(
        address(&intent.refund_to)? == address(&intent.source_address)?,
        "CCTP has no independent refund destination.",
    )
}
fn validate_metadata(intent: &BridgeIntent, metadata: &CctpQuoteMetadata) -> Result<()> {
    validate_intent(intent)?;
    check(
        metadata.protocol_version == 2
            && metadata.source_domain == domain(intent.evm_source()?.chain_id)?
            && metadata.destination_domain == destination_domain(intent)?
            && metadata.min_finality_threshold == metadata.transfer_mode.threshold()
            && metadata.max_fee < intent.amount,
        "CCTP metadata differs from its exact route, mode or amount.",
    )
}
pub fn burn_call(intent: &BridgeIntent, metadata: &CctpQuoteMetadata) -> Result<EvmCall> {
    validate_metadata(intent, metadata)?;
    Ok(EvmCall {
        chain_id: intent.evm_source()?.chain_id,
        to: TOKEN_MESSENGER.into(),
        value: Quantity::from_u64(0),
        data: hex_bytes(
            &Messenger::depositForBurnCall {
                amount: u256(&intent.amount)?,
                destinationDomain: metadata.destination_domain,
                mintRecipient: destination_word(intent, 2)?,
                burnToken: address(
                    intent
                        .evm_source()?
                        .token
                        .as_deref()
                        .ok_or_else(|| invalid("Missing native USDC identity."))?,
                )?,
                destinationCaller: destination_word(intent, 1)?,
                maxFee: u256(&metadata.max_fee)?,
                minFinalityThreshold: metadata.min_finality_threshold,
            }
            .abi_encode(),
        ),
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CctpMessage {
    pub source_domain: u32,
    pub destination_domain: u32,
    pub nonce: String,
    pub sender: String,
    pub recipient: String,
    pub destination_caller: String,
    pub min_finality_threshold: u32,
    pub finality_threshold_executed: u32,
    pub burn_token: String,
    pub mint_recipient: String,
    pub amount: Quantity,
    pub message_sender: String,
    pub max_fee: Quantity,
    pub fee_executed: Quantity,
    pub expiration_block: Quantity,
}
fn word_address(raw: &[u8]) -> Result<String> {
    check(
        raw.len() == 32 && raw[..12].iter().all(|b| *b == 0),
        "Noncanonical EVM address word in CCTP message.",
    )?;
    Ok(format!("{:#x}", Address::from_slice(&raw[12..])))
}
pub fn decode_message(raw: &str) -> Result<CctpMessage> {
    let b = bytes(raw)?;
    let source_is_solana = b.len() >= MESSAGE_LEN && b[4..8] == solana::DOMAIN.to_be_bytes();
    let inbound_core_hook =
        b.len() == MESSAGE_LEN + 56 && b[8..12] == hypercore::DOMAIN.to_be_bytes();
    let outbound_core_hook = b.len() == MESSAGE_LEN + 60
        && b[4..8] == hypercore::DOMAIN.to_be_bytes()
        && matches!(
            u32::from_be_bytes(b[8..12].try_into().expect("checked header")),
            0 | 3 | 5
        );
    check(
        b.len() == MESSAGE_LEN || inbound_core_hook || outbound_core_hook,
        "CCTP message must contain exactly the reviewed body and an allowed fixed HyperCore route hook.",
    )?;
    let body = &b[..MESSAGE_LEN];
    if inbound_core_hook {
        hypercore::validate_forward_hook_shape(&b[MESSAGE_LEN..])?;
    }
    if outbound_core_hook {
        hypercore::validate_outbound_hook_shape(&b[MESSAGE_LEN..])?;
    }
    let n = |offset: usize| {
        u32::from_be_bytes(body[offset..offset + 4].try_into().expect("checked length"))
    };
    let q = |offset: usize| quantity(U256::from_be_slice(&body[offset..offset + 32]));
    check(
        n(0) == 1 && n(148) == 1,
        "CCTP V2 header/body wire version must be 1.",
    )?;
    Ok(CctpMessage {
        source_domain: n(4),
        destination_domain: n(8),
        nonce: hex_bytes(&body[12..44]),
        sender: if source_is_solana {
            solana::public_key(&body[44..76])?
        } else {
            word_address(&body[44..76])?
        },
        recipient: if source_is_solana || n(8) == solana::DOMAIN {
            solana::public_key(&body[76..108])?
        } else {
            word_address(&body[76..108])?
        },
        destination_caller: if n(8) == solana::DOMAIN {
            solana::public_key(&body[108..140])?
        } else {
            word_address(&body[108..140])?
        },
        min_finality_threshold: n(140),
        finality_threshold_executed: n(144),
        burn_token: if source_is_solana {
            solana::public_key(&body[152..184])?
        } else {
            word_address(&body[152..184])?
        },
        mint_recipient: if n(8) == solana::DOMAIN {
            solana::public_key(&body[184..216])?
        } else {
            word_address(&body[184..216])?
        },
        amount: q(216),
        message_sender: if source_is_solana {
            solana::public_key(&body[248..280])?
        } else {
            word_address(&body[248..280])?
        },
        max_fee: q(280),
        fee_executed: q(312),
        expiration_block: q(344),
    })
}
/// Stable correlation across Iris attestation and re-attestation generations.
pub fn immutable_message_hash(raw: &str) -> Result<String> {
    decode_message(raw)?;
    let mut b = bytes(raw)?;
    for range in [12..44, 144..148, 312..344, 344..376] {
        b[range].fill(0);
    }
    Ok(format!("{:#x}", keccak256(b)))
}
pub fn validate_attested_message(
    source_raw: &str,
    metadata: &CctpQuoteMetadata,
    attested_raw: &str,
) -> Result<CctpMessage> {
    let original = decode_message(source_raw)?;
    let result = decode_message(attested_raw)?;
    check(
        hash(&original.nonce)? == B256::ZERO
            && original.finality_threshold_executed == 0
            && original.fee_executed.integer().is_zero()
            && original.expiration_block.integer().is_zero(),
        "Source message has nonzero attester-populated fields.",
    )?;
    check(
        immutable_message_hash(source_raw)? == immutable_message_hash(attested_raw)?,
        "Attestation changed immutable source-burn fields.",
    )?;
    check(
        result.source_domain == metadata.source_domain
            && result.destination_domain == metadata.destination_domain
            && result.max_fee == metadata.max_fee
            && result.min_finality_threshold == metadata.min_finality_threshold
            && metadata.protocol_version == 2
            && metadata.min_finality_threshold == metadata.transfer_mode.threshold(),
        "Attestation differs from approved CCTP metadata.",
    )?;
    check(
        hash(&result.nonce)? != B256::ZERO
            && matches!(result.finality_threshold_executed, 1000 | 2000)
            && result.finality_threshold_executed >= result.min_finality_threshold
            && result.fee_executed <= result.max_fee
            && result.fee_executed < result.amount,
        "Attestation has invalid nonce, finality or fee.",
    )?;
    Ok(result)
}
pub fn mint_call(destination_chain: u64, message: &str, attestation: &str) -> Result<EvmCall> {
    let decoded = decode_message(message)?;
    check(
        domain(destination_chain)? == decoded.destination_domain
            && decoded.recipient == TOKEN_MESSENGER,
        "Mint message targets another domain or token messenger.",
    )?;
    let attestation = bytes(attestation)?;
    check(
        !attestation.is_empty() && attestation.len() % 65 == 0 && attestation.len() <= 65 * 32,
        "Invalid CCTP attestation length.",
    )?;
    Ok(EvmCall {
        chain_id: destination_chain,
        to: MESSAGE_TRANSMITTER.into(),
        value: Quantity::from_u64(0),
        data: hex_bytes(
            &Transmitter::receiveMessageCall {
                message: Bytes::from(bytes(message)?),
                attestation: Bytes::from(attestation),
            }
            .abi_encode(),
        ),
    })
}

/// Extract the attestation generation actually consumed by an observed mint.
/// The broker must verify the transaction destination and canonical receipt,
/// then bind this message back to its persisted immutable source evidence.
pub fn decode_mint_calldata(calldata: &str) -> Result<(String, String)> {
    let raw = bytes(calldata)?;
    let call = Transmitter::receiveMessageCall::abi_decode_validate(&raw)
        .map_err(|_| invalid("Destination calldata is not a reviewed CCTP receiveMessage call."))?;
    check(
        call.abi_encode() == raw,
        "Destination CCTP call has noncanonical ABI bytes.",
    )?;
    let message = hex_bytes(&call.message);
    decode_message(&message)?;
    check(
        !call.attestation.is_empty()
            && call.attestation.len() % 65 == 0
            && call.attestation.len() <= 65 * 32,
        "Destination call has invalid attestation length.",
    )?;
    Ok((message, hex_bytes(&call.attestation)))
}

pub fn decode_mint_call(chain: u64, to: &str, input: &str) -> Result<(String, String)> {
    check(
        address(to)? == address(MESSAGE_TRANSMITTER)?,
        "Observed CCTP mint targets an unreviewed contract.",
    )?;
    let (message, attestation) = decode_mint_calldata(input)?;
    check(
        mint_call(chain, &message, &attestation)?
            .data
            .eq_ignore_ascii_case(input),
        "Observed CCTP mint differs from its destination domain or canonical call.",
    )?;
    Ok((message, attestation))
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CctpSourceMessage {
    pub raw_message: String,
    pub message_hash: String,
    pub immutable_message_hash: String,
    pub log_index: u32,
}
fn event<E: SolEvent>(log: &CanonicalRawLog) -> Result<E> {
    E::decode_raw_log_validate(
        log.topics
            .iter()
            .map(|v| hash(v))
            .collect::<Result<Vec<_>>>()?,
        &bytes(&log.data)?,
    )
    .map_err(|_| invalid("CCTP log differs from its exact event ABI."))
}
fn unique_log<'a, E: SolEvent>(
    logs: &'a [CanonicalRawLog],
    emitter: &str,
) -> Result<&'a CanonicalRawLog> {
    let mut selected = logs.iter().filter(|l| {
        l.address.eq_ignore_ascii_case(emitter)
            && l.topics
                .first()
                .is_some_and(|t| t.eq_ignore_ascii_case(&format!("{:#x}", E::SIGNATURE_HASH)))
    });
    let value = selected
        .next()
        .ok_or_else(|| invalid("Required CCTP event is missing."))?;
    check(
        selected.next().is_none(),
        "Ambiguous duplicate CCTP events.",
    )?;
    Ok(value)
}
pub fn verify_source_logs(
    logs: &[CanonicalRawLog],
    intent: &BridgeIntent,
    metadata: &CctpQuoteMetadata,
) -> Result<CctpSourceMessage> {
    validate_metadata(intent, metadata)?;
    let log = unique_log::<Transmitter::MessageSent>(logs, MESSAGE_TRANSMITTER)?;
    let raw_message = hex_bytes(&event::<Transmitter::MessageSent>(log)?.message);
    let m = decode_message(&raw_message)?;
    check(
        m.source_domain == metadata.source_domain
            && m.destination_domain == metadata.destination_domain
            && m.sender == TOKEN_MESSENGER
            && message_word(&m.recipient, m.destination_domain)? == destination_word(intent, 0)?
            && message_word(&m.destination_caller, m.destination_domain)?
                == destination_word(intent, 1)?
            && m.min_finality_threshold == metadata.min_finality_threshold
            && m.burn_token == intent.evm_source()?.token.clone().unwrap_or_default()
            && message_word(&m.mint_recipient, m.destination_domain)?
                == destination_word(intent, 2)?
            && address(&m.message_sender)? == address(&intent.source_address)?
            && m.amount == intent.amount
            && m.max_fee == metadata.max_fee
            && hash(&m.nonce)? == B256::ZERO
            && m.finality_threshold_executed == 0
            && m.fee_executed.integer().is_zero()
            && m.expiration_block.integer().is_zero(),
        "Source CCTP message differs from the approved burn.",
    )?;
    let burn_log = unique_log::<Messenger::DepositForBurn>(logs, TOKEN_MESSENGER)?;
    let burn = event::<Messenger::DepositForBurn>(burn_log)?;
    check(
        burn.burnToken == address(&m.burn_token)?
            && burn.amount == u256(&m.amount)?
            && burn.depositor == address(&m.message_sender)?
            && burn.mintRecipient == destination_word(intent, 2)?
            && burn.destinationDomain == m.destination_domain
            && burn.destinationTokenMessenger == destination_word(intent, 0)?
            && burn.destinationCaller == destination_word(intent, 1)?
            && burn.maxFee == u256(&m.max_fee)?
            && burn.minFinalityThreshold == m.min_finality_threshold
            && burn.hookData.is_empty()
            && burn_log.log_index > log.log_index,
        "DepositForBurn does not bind the emitted message.",
    )?;
    Ok(CctpSourceMessage {
        message_hash: format!("{:#x}", keccak256(bytes(&raw_message)?)),
        immutable_message_hash: immutable_message_hash(&raw_message)?,
        raw_message,
        log_index: log.log_index,
    })
}
pub fn verify_destination_logs(logs: &[CanonicalRawLog], attested_raw: &str) -> Result<u32> {
    let m = decode_message(attested_raw)?;
    let log = unique_log::<Transmitter::MessageReceived>(logs, MESSAGE_TRANSMITTER)?;
    let e = event::<Transmitter::MessageReceived>(log)?;
    let raw = bytes(attested_raw)?;
    let expected_sender = if m.source_domain == solana::DOMAIN {
        B256::from_slice(&raw[44..76])
    } else {
        address(TOKEN_MESSENGER)?.into_word()
    };
    check(
        e.sourceDomain == m.source_domain
            && e.nonce == hash(&m.nonce)?
            && e.sender == expected_sender
            && e.finalityThresholdExecuted == m.finality_threshold_executed
            && e.messageBody.as_ref() == &raw[148..]
            && e.caller == address(&m.destination_caller)?,
        "Destination receive event differs from the signed CCTP message.",
    )?;
    let chain = match m.destination_domain {
        0 => 1,
        6 => 8453,
        3 => 42161,
        19 => 999,
        _ => return Err(invalid("Unsupported destination domain.")),
    };
    let token = usdc_asset(chain)?.token.unwrap_or_default();
    let net = u256(&m.amount)?
        .checked_sub(u256(&m.fee_executed)?)
        .ok_or_else(|| invalid("Mint fee exceeds burn amount."))?;
    let mut count = 0;
    for l in logs.iter().filter(|l| {
        l.address.eq_ignore_ascii_case(&token)
            && l.topics.first().is_some_and(|t| {
                t.eq_ignore_ascii_case(&format!("{:#x}", Token::Transfer::SIGNATURE_HASH))
            })
    }) {
        let transfer = event::<Token::Transfer>(l)?;
        if transfer.from == Address::ZERO
            && transfer.to == address(&m.mint_recipient)?
            && transfer.value == net
            && l.log_index < log.log_index
        {
            count += 1;
        }
    }
    check(
        count == 1,
        "Destination native-USDC mint evidence is missing or ambiguous.",
    )?;
    Ok(log.log_index)
}

/// Exact ceil(amount * decimal basis points / 10,000), without floating math.
pub fn fee_from_bps(amount: &Quantity, bps: &str) -> Result<Quantity> {
    let (whole, fraction) = bps.split_once('.').unwrap_or((bps, ""));
    check(
        !whole.is_empty()
            && whole.bytes().all(|c| c.is_ascii_digit())
            && fraction.bytes().all(|c| c.is_ascii_digit())
            && fraction.len() <= 18
            && whole.len() <= 10,
        "Fee basis points must be a bounded nonnegative decimal.",
    )?;
    let numerator = BigUint::parse_bytes(format!("{whole}{fraction}").as_bytes(), 10)
        .ok_or_else(|| invalid("Invalid fee rate."))?;
    let denominator = BigUint::from(10_000u32) * BigUint::from(10u8).pow(fraction.len() as u32);
    let value = (amount.integer() * numerator + &denominator - BigUint::from(1u8)) / denominator;
    Quantity::new(value.to_string()).map_err(|_| invalid("Fee quantity cannot be represented."))
}

fn bounded_max_fee(
    intent: &BridgeIntent,
    bounds: &BridgeBounds,
    quoted_fee: &Quantity,
) -> Result<Quantity> {
    let asset = destination_asset(intent)?;
    // Multiple applicable caps are intersected, never added or widened.
    let max_fee = bounds
        .fee_caps
        .iter()
        .filter(|cap| cap.kind == FeeKind::WithheldFromOutput && cap.asset == asset)
        .map(|cap| cap.maximum.clone())
        .min()
        .unwrap_or_else(|| quoted_fee.clone());
    check(
        max_fee >= *quoted_fee && max_fee < intent.amount,
        "Current CCTP fee exceeds the approved cap or leaves no net receipt.",
    )?;
    Ok(max_fee)
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CctpAttestation {
    pub source_transaction_hash: String,
    pub message: String,
    pub attestation: String,
    pub nonce: String,
    /// Circle's destination-forward transaction lookup hint. Settlement must
    /// still verify the canonical destination receipt and exact mint evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward_tx_hash: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CctpAttestationStatus {
    Pending,
    Ready { messages: Vec<CctpAttestation> },
}

#[derive(Clone)]
pub struct CctpProvider {
    runtimes: BTreeMap<u64, Arc<EvmRuntime>>,
    config: CctpConfig,
    client: reqwest::Client,
    solana_runtime: Option<Arc<gofin_solana::SolanaRuntime>>,
}
impl CctpProvider {
    pub fn new(runtimes: BTreeMap<u64, Arc<EvmRuntime>>, config: CctpConfig) -> Result<Self> {
        config.validate()?;
        for (chain, runtime) in &runtimes {
            domain(*chain)?;
            check(
                *chain == runtime.chain_id && !runtime.can_sign(),
                "CCTP requires exact read-only network runtimes.",
            )?;
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| invalid("Cannot construct Iris transport."))?;
        Ok(Self {
            runtimes,
            config,
            client,
            solana_runtime: None,
        })
    }
    pub fn with_solana_runtime(mut self, runtime: Arc<gofin_solana::SolanaRuntime>) -> Self {
        self.solana_runtime = Some(runtime);
        self
    }
    pub async fn solana_state(
        &self,
        source_chain: u64,
        destination: &SolanaDestination,
        nonce: Option<&str>,
    ) -> Result<solana::SolanaCctpState> {
        solana::inspect_state(
            self.solana_runtime
                .as_ref()
                .ok_or_else(|| invalid("Solana CCTP runtime is not configured."))?,
            source_chain,
            destination,
            nonce,
        )
        .await
    }
    fn runtime(&self, chain: u64) -> Result<&Arc<EvmRuntime>> {
        self.runtimes
            .get(&chain)
            .ok_or_else(|| invalid("CCTP network RPC is not configured."))
    }

    /// Fresh, same-block deployment relationships on both selected networks.
    /// Call again before a new source burn; configuration addresses alone are
    /// insufficient evidence of the current proxy deployment and token mapping.
    pub async fn verify_deployments(&self, intent: &BridgeIntent) -> Result<()> {
        validate_intent(intent)?;
        // At most four independent reads per chain and two selected chains.
        // Each chain captures its own block before starting any dependent read.
        if let Some(d) = &intent.solana_destination {
            let (_, state) = tokio::try_join!(
                self.verify_deployment_route(
                    intent.evm_source()?.chain_id,
                    solana::DOMAIN,
                    B256::from(solana::pubkey_bytes(solana::TOKEN_MESSENGER_MINTER)?),
                    B256::from(solana::pubkey_bytes(solana::USDC_MINT)?)
                ),
                self.solana_state(intent.evm_source()?.chain_id, d, None)
            )?;
            check(
                Quantity::from_u64(state.custody_balance) >= intent.amount,
                "CCTP Solana custody cannot presently cover the burn amount.",
            )?;
        } else {
            let destination = intent.evm_destination_chain_id()?;
            tokio::try_join!(
                self.verify_deployment(intent.evm_source()?.chain_id, destination),
                self.verify_deployment(destination, intent.evm_source()?.chain_id)
            )?;
        }
        Ok(())
    }

    async fn verify_deployment(&self, chain: u64, remote: u64) -> Result<()> {
        self.verify_deployment_route(
            chain,
            domain(remote)?,
            address(TOKEN_MESSENGER)?.into_word(),
            address(usdc_asset(remote)?.token.as_deref().unwrap_or_default())?.into_word(),
        )
        .await
    }
    async fn verify_deployment_route(
        &self,
        chain: u64,
        remote_domain: u32,
        remote_messenger_expected: B256,
        remote_token: B256,
    ) -> Result<()> {
        let messenger = address(TOKEN_MESSENGER)?;
        let transmitter = address(MESSAGE_TRANSMITTER)?;
        let minter = address(TOKEN_MINTER)?;
        let runtime = self.runtime(chain)?;
        runtime.assert_chain().await?;
        let block = Some(rpc_u64(
            &runtime.rpc.request("eth_blockNumber", json!([])).await?,
        )?);
        let token = address(usdc_asset(chain)?.token.as_deref().unwrap_or_default())?;
        tokio::try_join!(
            runtime.code_at(messenger, block),
            runtime.code_at(transmitter, block),
            runtime.code_at(minter, block),
            runtime.code_at(token, block),
        )?;
        let remote_messenger_call = Messenger::remoteTokenMessengersCall {
            remoteDomain: remote_domain,
        };
        let (local_transmitter, local_minter, body_version, remote_messenger) = tokio::try_join!(
            runtime.call(messenger, &Messenger::localMessageTransmitterCall {}, block),
            runtime.call(messenger, &Messenger::localMinterCall {}, block),
            runtime.call(messenger, &Messenger::messageBodyVersionCall {}, block),
            runtime.call(messenger, &remote_messenger_call, block),
        )?;
        let (local_domain, version, decimals, local_messenger) = tokio::try_join!(
            runtime.call(transmitter, &Transmitter::localDomainCall {}, block),
            runtime.call(transmitter, &Transmitter::versionCall {}, block),
            runtime.call(token, &Token::decimalsCall {}, block),
            runtime.call(minter, &Minter::localTokenMessengerCall {}, block),
        )?;
        let local_token = runtime
            .call(
                minter,
                &Minter::getLocalTokenCall {
                    remoteDomain: remote_domain,
                    remoteToken: remote_token,
                },
                block,
            )
            .await?;
        check(
            local_transmitter == transmitter
                && local_minter == minter
                && body_version == 1
                && remote_messenger == remote_messenger_expected
                && local_domain == domain(chain)?
                && version == 1
                && decimals == 6
                && local_messenger == messenger
                && local_token == token,
            "Deployed CCTP relationships, versions, domain or native-USDC mapping differ from the reviewed route.",
        )
    }
    async fn response(&self, request: reqwest::RequestBuilder) -> Result<Option<Value>> {
        let mut response = request.send().await.map_err(|_| {
            bridge_error(
                "CCTP_SERVICE_UNAVAILABLE",
                "read",
                "Iris request failed.",
                "Retry this transfer later; do not create another burn.",
            )
        })?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        check(
            response.status().is_success(),
            "Iris returned an unsuccessful HTTP status; retry with backoff.",
        )?;
        check(
            response
                .content_length()
                .is_none_or(|n| n <= RESPONSE_LIMIT as u64),
            "Iris response exceeds the size limit.",
        )?;
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| invalid("Iris response body could not be read."))?
        {
            check(
                body.len() + chunk.len() <= RESPONSE_LIMIT,
                "Iris response exceeds the size limit.",
            )?;
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|_| invalid("Iris response is not valid JSON."))
    }
    pub async fn fetch_attestation(
        &self,
        source_domain: u32,
        source_tx_hash: &str,
    ) -> Result<CctpAttestationStatus> {
        match source_domain {
            5 => {
                let signature = bs58::decode(source_tx_hash)
                    .into_vec()
                    .map_err(|_| invalid("Solana CCTP source signature is malformed."))?;
                check(
                    signature.len() == 64
                        && bs58::encode(&signature).into_string() == source_tx_hash,
                    "Solana CCTP source signature is not canonical base58.",
                )?;
            }
            0 | 3 | 6 | 19 => {
                hash(source_tx_hash)?;
            }
            _ => return Err(invalid("Unsupported CCTP source domain.")),
        }
        let Some(value) = self
            .response(
                self.client
                    .get(format!(
                        "{}/v2/messages/{source_domain}",
                        self.config.iris_url
                    ))
                    .query(&[("transactionHash", source_tx_hash)]),
            )
            .await?
        else {
            return Ok(CctpAttestationStatus::Pending);
        };
        if let Some(tx) = value["sourceTxHash"].as_str() {
            let matches_source = if source_domain == 5 {
                tx == source_tx_hash
            } else {
                hash(tx)? == hash(source_tx_hash)?
            };
            check(
                matches_source,
                "Iris response belongs to another source transaction.",
            )?;
        }
        let messages = value["messages"]
            .as_array()
            .ok_or_else(|| invalid("Missing Iris messages array."))?;
        check(messages.len() <= 32, "Too many messages in Iris response.")?;
        let mut ready = Vec::new();
        for item in messages {
            if item["status"].as_str() != Some("complete") {
                continue;
            }
            check(
                value["sourceTxHash"].as_str().is_some(),
                "Completed Iris response is missing its source transaction identity.",
            )?;
            check(
                item["cctpVersion"].as_u64() == Some(2),
                "Iris returned an unsupported CCTP version.",
            )?;
            let message = item["message"]
                .as_str()
                .ok_or_else(|| invalid("Missing attested message."))?;
            let attestation = item["attestation"]
                .as_str()
                .ok_or_else(|| invalid("Missing attestation signature."))?;
            let decoded = decode_message(message)?;
            check(
                decoded.source_domain == source_domain,
                "Iris message source domain differs.",
            )?;
            let signature = bytes(attestation)?;
            check(
                !signature.is_empty() && signature.len() % 65 == 0 && signature.len() <= 65 * 32,
                "Invalid attestation length.",
            )?;
            let forward_tx_hash = if item["forwardState"].as_str() == Some("COMPLETE") {
                let raw_hash = item["forwardTxHash"]
                    .as_str()
                    .ok_or_else(|| invalid("Completed forward is missing its transaction hash."))?;
                if decoded.destination_domain == solana::DOMAIN {
                    let signature = bs58::decode(raw_hash)
                        .into_vec()
                        .map_err(|_| invalid("Circle Solana forward signature is malformed."))?;
                    check(
                        signature.len() == 64 && bs58::encode(&signature).into_string() == raw_hash,
                        "Circle Solana forward signature is not canonical base58.",
                    )?;
                    Some(raw_hash.to_owned())
                } else {
                    let parsed_hash = hash(raw_hash)?;
                    check(
                        parsed_hash != B256::ZERO,
                        "Invalid Circle forward transaction hash.",
                    )?;
                    Some(format!("{parsed_hash:#x}"))
                }
            } else {
                None
            };
            ready.push(CctpAttestation {
                source_transaction_hash: source_tx_hash.into(),
                message: message.into(),
                attestation: attestation.into(),
                nonce: decoded.nonce,
                forward_tx_hash,
            });
        }
        Ok(if ready.is_empty() {
            CctpAttestationStatus::Pending
        } else {
            CctpAttestationStatus::Ready { messages: ready }
        })
    }
    pub async fn reattest(&self, nonce: &str) -> Result<()> {
        let nonce = hash(nonce)?;
        check(
            nonce != B256::ZERO,
            "Cannot re-attest the source placeholder nonce.",
        )?;
        self.response(
            self.client
                .post(format!("{}/v2/reattest/{nonce:#x}", self.config.iris_url)),
        )
        .await?
        .ok_or_else(|| invalid("Iris does not yet recognize this nonce."))?;
        Ok(())
    }
    pub async fn nonce_used(&self, destination_chain: u64, nonce: &str) -> Result<bool> {
        let runtime = self.runtime(destination_chain)?;
        runtime.assert_chain().await?;
        Ok(runtime
            .call(
                address(MESSAGE_TRANSMITTER)?,
                &Transmitter::usedNoncesCall {
                    nonce: hash(nonce)?,
                },
                None,
            )
            .await?
            != U256::ZERO)
    }
    pub async fn verify_attestation(
        &self,
        destination_chain: u64,
        message: &str,
        attestation: &str,
    ) -> Result<()> {
        let runtime = self.runtime(destination_chain)?;
        runtime.assert_chain().await?;
        let decoded = decode_message(message)?;
        check(
            decoded.destination_domain == domain(destination_chain)?,
            "Wrong attestation destination.",
        )?;
        let transmitter = address(MESSAGE_TRANSMITTER)?;
        let block = rpc_u64(&runtime.rpc.request("eth_blockNumber", json!([])).await?)?;
        let threshold = runtime
            .call(
                transmitter,
                &Transmitter::signatureThresholdCall {},
                Some(block),
            )
            .await?;
        let sigs = bytes(attestation)?;
        check(
            threshold > U256::ZERO
                && threshold <= U256::from(32)
                && U256::from(sigs.len()) == threshold * U256::from(65),
            "Attestation signature count differs from destination threshold.",
        )?;
        let digest = keccak256(bytes(message)?);
        let mut previous = Address::ZERO;
        for sig in sigs.chunks_exact(65) {
            check(
                matches!(sig[64], 27 | 28),
                "Attestation recovery id must use Solidity's 27/28 encoding.",
            )?;
            let signature = Signature::from_raw(sig)
                .map_err(|_| invalid("Invalid attestation signature encoding."))?;
            check(
                signature.normalize_s().is_none(),
                "Attestation signature has noncanonical high s.",
            )?;
            let signer = signature
                .recover_address_from_prehash(&digest)
                .map_err(|_| invalid("Attestation signer recovery failed."))?;
            check(
                signer > previous
                    && runtime
                        .call(
                            transmitter,
                            &Transmitter::isEnabledAttesterCall { attester: signer },
                            Some(block),
                        )
                        .await?,
                "Attestation signer is disabled, duplicated or out of order.",
            )?;
            previous = signer;
        }
        Ok(())
    }
    /// Arbitrum's EVM block.number for expiration follows Ethereum L1 height.
    pub async fn expiration_block_number(&self, destination_chain: u64) -> Result<u64> {
        let runtime = self.runtime(if destination_chain == 42161 {
            1
        } else {
            destination_chain
        })?;
        runtime.assert_chain().await?;
        rpc_u64(&runtime.rpc.request("eth_blockNumber", json!([])).await?)
    }

    async fn gas_budget(
        &self,
        chain: u64,
        sender: &str,
        call: &EvmCall,
        gas: u64,
    ) -> Result<Quantity> {
        let runtime = self.runtime(chain)?;
        let (max_fee, priority) = runtime.fees().await?;
        let mut tx = gofin_evm::FrozenTransaction {
            chain_id: chain,
            sender: sender.into(),
            to: call.to.clone(),
            value: call.value.clone(),
            calldata: call.data.clone(),
            nonce: 0,
            gas,
            max_fee_per_gas: quantity(max_fee),
            max_priority_fee_per_gas: quantity(priority),
            rollup_fee: None,
        };
        tx.rollup_fee = runtime.rollup_fee_bound(&tx).await?;
        Ok(quantity(tx.maximum_cost()?))
    }
}

fn step(intent: &BridgeIntent, ordinal: u8, kind: BridgeStepKind, call: EvmCall) -> BridgeStep {
    BridgeStep {
        ordinal,
        kind,
        call: Some(call),
        solana_mint: None,
        solana_transaction: None,
        expected_sender: intent.source_address.clone(),
        maximum_debit: if kind == BridgeStepKind::Deposit {
            vec![
                intent
                    .source_amount(intent.amount.clone())
                    .expect("validated CCTP source"),
            ]
        } else {
            vec![]
        },
        dependencies: if ordinal == 0 {
            vec![]
        } else {
            vec![ordinal - 1]
        },
    }
}
fn approval_call(intent: &BridgeIntent, amount: U256) -> Result<EvmCall> {
    Ok(EvmCall {
        chain_id: intent.evm_source()?.chain_id,
        to: intent
            .evm_source()?
            .token
            .clone()
            .ok_or_else(|| invalid("Missing USDC identity."))?,
        value: Quantity::zero(),
        data: hex_bytes(
            &Token::approveCall {
                spender: address(TOKEN_MESSENGER)?,
                amount,
            }
            .abi_encode(),
        ),
    })
}
fn mint_template(intent: &BridgeIntent) -> Result<EvmCall> {
    Ok(EvmCall {
        chain_id: intent.evm_destination_chain_id()?,
        to: MESSAGE_TRANSMITTER.into(),
        value: Quantity::zero(),
        data: "0x".into(),
    })
}
fn mint_step(intent: &BridgeIntent, ordinal: u8) -> Result<BridgeStep> {
    if let Some(d) = &intent.solana_destination {
        Ok(BridgeStep {
            ordinal,
            kind: BridgeStepKind::Mint,
            call: None,
            solana_mint: Some(SolanaMintSpec {
                destination: d.clone(),
                message_transmitter: solana::MESSAGE_TRANSMITTER.into(),
                token_messenger_minter: solana::TOKEN_MESSENGER_MINTER.into(),
            }),
            solana_transaction: None,
            expected_sender: d.wallet_address.clone(),
            maximum_debit: vec![],
            dependencies: vec![ordinal - 1],
        })
    } else {
        Ok(step(
            intent,
            ordinal,
            BridgeStepKind::Mint,
            mint_template(intent)?,
        ))
    }
}

#[async_trait]
impl BridgeProvider for CctpProvider {
    fn as_cctp(&self) -> Option<&CctpProvider> {
        Some(self)
    }
    fn capabilities(&self) -> BridgeCapabilities {
        BridgeCapabilities {
            provider: PROVIDER_ID.into(),
            chains: BTreeSet::from([1, 8453, 42161]),
            native_eth: false,
            erc20: true,
            protocol_versions: BTreeSet::from(["cctp_v2".into()]),
        }
    }
    async fn discover_routes(&self, request: &BridgeRouteRequest) -> Result<BridgeRouteDiscovery> {
        let available = request.evm_source()?.chain_id != request.destination_chain_id
            && usdc_asset(request.evm_source()?.chain_id)
                .is_ok_and(|a| request.source.as_ref() == Some(&a))
            && usdc_asset(request.destination_chain_id)
                .is_ok_and(|a| request.destination_asset.as_ref().is_none_or(|v| *v == a))
            && self.runtimes.contains_key(&request.evm_source()?.chain_id)
            && self.runtimes.contains_key(&request.destination_chain_id);
        Ok(BridgeRouteDiscovery {provider:PROVIDER_ID.into(),availability:if available {RouteAvailability::Candidate} else {RouteAvailability::Unavailable},candidates:if available {vec![BridgeRouteCandidate {destination:Some(BridgeAssetInput {chain_id:request.destination_chain_id,token:usdc_asset(request.destination_chain_id)?.token}),solana_destination:None,reported_decimals:Some(6)}]} else {vec![]}, observed_at:Utc::now().to_rfc3339(),evidence_source:"Reviewed Circle CCTP V2 mainnet contracts and native USDC identities".into(),complete:true,limitations:vec!["Direct native-USDC transfer only; source and destination gas are required. Fast is requested, not guaranteed.".into()],error:None})
    }
    async fn quote(&self, request: &BridgeQuoteRequest) -> Result<BridgeQuoteBatch> {
        let intent = &request.intent;
        validate_intent(intent)?;
        let mode = request.cctp.clone().unwrap_or_default().transfer_mode;
        let source = self.runtime(intent.evm_source()?.chain_id)?;
        // Iris and chain deployment evidence are independent reads. Neither
        // result is accepted until both validations complete.
        let (_, rates) = tokio::try_join!(
            self.verify_deployments(intent),
            self.response(self.client.get(format!(
                "{}/v2/burn/USDC/fees/{}/{}",
                self.config.iris_url,
                domain(intent.evm_source()?.chain_id)?,
                destination_domain(intent)?
            )))
        )?;
        let rates = rates.ok_or_else(|| invalid("Iris has no fee schedule for this route."))?;
        let rates = rates
            .as_array()
            .ok_or_else(|| invalid("Iris fee schedule must be an array."))?;
        check(rates.len() <= 16, "Oversized Iris fee schedule.")?;
        let selected: Vec<_> = rates
            .iter()
            .filter(|r| r["finalityThreshold"].as_u64() == Some(u64::from(mode.threshold())))
            .collect();
        check(
            selected.len() == 1,
            "Missing or ambiguous fee schedule for requested transfer mode.",
        )?;
        let rate = &selected[0]["minimumFee"];
        check(
            rate.is_number(),
            "Iris fee rate must be numeric basis points.",
        )?;
        let quoted_fee = fee_from_bps(&intent.amount, &rate.to_string())?;
        let max_fee = bounded_max_fee(intent, &request.bounds, &quoted_fee)?;
        let metadata = CctpQuoteMetadata {
            protocol_version: 2,
            source_domain: domain(intent.evm_source()?.chain_id)?,
            destination_domain: destination_domain(intent)?,
            transfer_mode: mode,
            max_fee: max_fee.clone(),
            min_finality_threshold: mode.threshold(),
        };
        validate_metadata(intent, &metadata)?;
        let allowance = source
            .call(
                address(intent.evm_source()?.token.as_deref().unwrap_or_default())?,
                &Token::allowanceCall {
                    owner: address(&intent.source_address)?,
                    spender: address(TOKEN_MESSENGER)?,
                },
                None,
            )
            .await?;
        let mut steps = Vec::new();
        // Declare a finite approval template even when allowance is satisfied.
        // The broker skips satisfied writes and preserves approved topology
        // across refreshes; omission here loses the spender identity on refresh.
        let approval = Some(ApprovalRequirement {
            token: intent.evm_source()?.token.clone().unwrap_or_default(),
            spender: TOKEN_MESSENGER.into(),
            amount: intent.amount.clone(),
        });
        if allowance < u256(&intent.amount)? && allowance != U256::ZERO {
            steps.push(step(
                intent,
                steps.len() as u8,
                BridgeStepKind::ResetAllowance,
                approval_call(intent, U256::ZERO)?,
            ));
        }
        steps.push(step(
            intent,
            steps.len() as u8,
            BridgeStepKind::Approval,
            approval_call(intent, u256(&intent.amount)?)?,
        ));
        steps.push(step(
            intent,
            steps.len() as u8,
            BridgeStepKind::Deposit,
            burn_call(intent, &metadata)?,
        ));
        let source_gas = async {
            let mut source_gas = Quantity::zero();
            for s in &steps {
                let estimate = source
                    .prepare(
                        address(&intent.source_address)?,
                        address(&s.evm_call()?.to)?,
                        U256::ZERO,
                        rpc_bytes(&json!(s.evm_call()?.data))?,
                        U256::MAX,
                    )
                    .await;
                let fee = match estimate {
                    Ok(prepared) => quantity(prepared.transaction.maximum_cost()?),
                    Err(_) if allowance < u256(&intent.amount)? && !s.dependencies.is_empty() => {
                        self.gas_budget(
                            intent.evm_source()?.chain_id,
                            &intent.source_address,
                            s.evm_call()?,
                            500_000,
                        )
                        .await?
                    }
                    Err(error) => return Err(error),
                };
                source_gas = source_gas.checked_add(&fee)?;
            }
            Ok::<_, GoFinError>(source_gas)
        };
        let destination_gas = async {
            // A future signed message is unavailable before burn. The quote includes
            // a conservative gas estimate; mint dispatch must simulate actual bytes.
            let solana_state = if let Some(d) = &intent.solana_destination {
                Some(
                    self.solana_state(intent.evm_source()?.chain_id, d, None)
                        .await?,
                )
            } else {
                None
            };
            let destination_gas = if solana_state.is_some() {
                Quantity::from_u64(5000)
            } else {
                let estimation_call = EvmCall {
                    data: hex_bytes(
                        &Transmitter::receiveMessageCall {
                            message: Bytes::from(vec![0xff; MESSAGE_LEN]),
                            attestation: Bytes::from(vec![0xff; 65 * 2]),
                        }
                        .abi_encode(),
                    ),
                    ..mint_template(intent)?
                };
                self.gas_budget(
                    intent.evm_destination_chain_id()?,
                    &intent.source_address,
                    &estimation_call,
                    500_000,
                )
                .await?
            };
            Ok::<_, GoFinError>((destination_gas, solana_state))
        };
        let (source_gas, (destination_gas, solana_state)) =
            tokio::try_join!(source_gas, destination_gas)?;
        steps.push(mint_step(intent, steps.len() as u8)?);
        let receipt = destination_amount(intent, intent.amount.checked_sub(&max_fee)?)?;
        let mut fees = vec![
            BridgeFee {
                amount: EvmAsset {
                    chain_id: intent.evm_source()?.chain_id,
                    token: None,
                    decimals: 18,
                }
                .amount(source_gas),
                kind: FeeKind::Gas,
                payer: intent.source_address.clone(),
            },
            BridgeFee {
                amount: Amount {
                    asset: destination_gas_asset(intent)?,
                    atoms: destination_gas,
                },
                kind: FeeKind::Gas,
                payer: destination_payer(intent).into(),
            },
            BridgeFee {
                amount: destination_amount(intent, max_fee)?,
                kind: FeeKind::WithheldFromOutput,
                payer: intent.source_address.clone(),
            },
        ];
        if let Some(state) = solana_state {
            fees.push(BridgeFee {
                amount: Amount {
                    asset: destination_gas_asset(intent)?,
                    atoms: Quantity::from_u64(state.account_funding_lamports),
                },
                kind: FeeKind::AdditionalDebit,
                payer: destination_payer(intent).into(),
            });
        }
        let now = Utc::now();
        let identity = serde_json::to_vec(&(intent, &metadata, &steps, now.to_rfc3339()))
            .map_err(|_| invalid("Cannot encode CCTP quote identity."))?;
        let quote = BridgeQuote {
            quote_id: format!("cctp:{:#x}", keccak256(identity)),
            provider: PROVIDER_ID.into(),
            route_id: route_id(intent)?,
            intent: intent.clone(),
            gross_debit: intent.evm_source()?.amount(intent.amount.clone()),
            expected_receipt: receipt.clone(),
            minimum_receipt: receipt,
            fees,
            approval,
            protocol_steps: steps,
            // Plan for standard fallback even when faster attestation is requested.
            estimated_arrival_seconds: Some(if intent.evm_source()?.chain_id == 8453 {
                2400
            } else {
                1140
            }),
            observed_at: now.to_rfc3339(),
            valid_until: (now + Duration::seconds(90)).to_rfc3339(),
            class: QuoteClass::Executable,
            refund_paths: vec![],
            order_solver: None,
            protocol_order_id: None,
            provider_request_id: None,
            cctp: Some(metadata),
        };
        scope_digest(&quote, &request.bounds, &intent.source_address)?;
        Ok(BridgeQuoteBatch {
            quotes: vec![quote],
            failures: vec![],
        })
    }
    fn prepare(
        &self,
        quote: &BridgeQuote,
        bounds: &BridgeBounds,
        wallet: &str,
    ) -> Result<PreparedBridge> {
        let until = chrono::DateTime::parse_from_rfc3339(&quote.valid_until)
            .map_err(|_| invalid("Invalid CCTP quote expiry."))?;
        if until <= Utc::now() {
            return Err(GoFinError::new(
                "BRIDGE_QUOTE_EXPIRED",
                "prepare",
                "CCTP quote expired; refresh the saved transfer before preparing its source step.",
                "Call bridge.quotes with refreshTransferId set to the original saved transfer ID and the same provider, transfer mode, intent and bounds, then retry the saved step under the existing plan. Reuse valid authority within its approved limits.",
            ));
        }
        Ok(PreparedBridge {
            quote: quote.clone(),
            bounds: bounds.clone(),
            steps: quote.protocol_steps.clone(),
            source_order_id: None,
            fixed_scope_digest: scope_digest(quote, bounds, wallet)?,
        })
    }
    async fn reconcile(&self, _transfer: &BridgeTransferRef) -> Result<BridgeReconciliation> {
        Ok(BridgeReconciliation {
            status: "requires_verified_source_message".into(),
            failure: None,
            refund_fail_reason: None,
            candidates: vec![],
        })
    }
}

/// Revalidates the persisted finite topology, without requiring quote freshness
/// for a later mint that completes an already independently proven burn.
pub fn scope_digest(quote: &BridgeQuote, bounds: &BridgeBounds, wallet: &str) -> Result<String> {
    let intent = &quote.intent;
    let m = quote
        .cctp
        .as_ref()
        .ok_or_else(|| invalid("Missing CCTP quote metadata."))?;
    validate_metadata(intent, m)?;
    check(
        quote.provider == PROVIDER_ID
            && quote.class == QuoteClass::Executable
            && quote.route_id == route_id(intent)?
            && address(&intent.source_address)? == address(wallet)?
            && quote.refund_paths.is_empty()
            && quote.order_solver.is_none()
            && quote.protocol_order_id.is_none()
            && quote.provider_request_id.is_none(),
        "CCTP quote identity or unsupported provider fields changed.",
    )?;
    let receipt = destination_amount(intent, intent.amount.checked_sub(&m.max_fee)?)?;
    check(
        quote.gross_debit == intent.evm_source()?.amount(intent.amount.clone())
            && quote.expected_receipt == receipt
            && quote.minimum_receipt == receipt,
        "CCTP amounts do not conserve burn principal minus maximum fee.",
    )?;
    let mut expected = Vec::new();
    if let Some(approval) = &quote.approval {
        check(
            approval.token == intent.evm_source()?.token.clone().unwrap_or_default()
                && approval.spender == TOKEN_MESSENGER
                && approval.amount == intent.amount,
            "CCTP allowance must be finite and bound to the reviewed messenger.",
        )?;
        if quote
            .protocol_steps
            .first()
            .is_some_and(|s| s.kind == BridgeStepKind::ResetAllowance)
        {
            expected.push(step(
                intent,
                expected.len() as u8,
                BridgeStepKind::ResetAllowance,
                approval_call(intent, U256::ZERO)?,
            ));
        }
        expected.push(step(
            intent,
            expected.len() as u8,
            BridgeStepKind::Approval,
            approval_call(intent, u256(&intent.amount)?)?,
        ));
    }
    expected.push(step(
        intent,
        expected.len() as u8,
        BridgeStepKind::Deposit,
        burn_call(intent, m)?,
    ));
    expected.push(mint_step(intent, expected.len() as u8)?);
    check(
        expected == quote.protocol_steps,
        "CCTP step topology or calldata changed.",
    )?;
    check(
        quote.fees.len()
            == if intent.solana_destination.is_some() {
                4
            } else {
                3
            },
        "CCTP fee disclosure count differs.",
    )?;
    let source_gas_asset = EvmAsset {
        chain_id: intent.evm_source()?.chain_id,
        token: None,
        decimals: 18,
    }
    .asset_id();
    for (index, asset, payer) in [
        (0, source_gas_asset, intent.source_address.as_str()),
        (1, destination_gas_asset(intent)?, destination_payer(intent)),
    ] {
        let fee = &quote.fees[index];
        check(
            fee.kind == FeeKind::Gas && fee.amount.asset == asset && fee.payer == payer,
            "CCTP gas identity or payer differs.",
        )?;
        u256(&fee.amount.atoms)?;
    }
    let fee = &quote.fees[2];
    check(
        fee.kind == FeeKind::WithheldFromOutput
            && fee.amount == destination_amount(intent, m.max_fee.clone())?
            && fee.payer == intent.source_address,
        "CCTP protocol fee differs.",
    )?;
    if intent.solana_destination.is_some() {
        let fee = &quote.fees[3];
        check(
            fee.kind == FeeKind::AdditionalDebit
                && fee.amount.asset == destination_gas_asset(intent)?
                && fee.payer == destination_payer(intent)
                && !fee.amount.atoms.is_zero(),
            "CCTP used nonce rent identity differs.",
        )?;
    }
    check(
        bounds.latest_protocol_deadline.is_none(),
        "CCTP burn has no enforceable completion deadline.",
    )?;
    if let Some(v) = &bounds.input {
        check(
            v.minimum <= intent.amount && intent.amount <= v.maximum,
            "CCTP input amount exceeds approved interval.",
        )?;
    }
    if let Some(v) = &bounds.maximum_source_debit {
        check(intent.amount <= *v, "CCTP source debit exceeds cap.")?;
    }
    if let Some(v) = &bounds.minimum_net_receipt {
        check(
            v.asset == receipt.asset && v.atoms <= receipt.atoms,
            "CCTP net receipt is below the approved floor.",
        )?;
    }
    if let Some(v) = &bounds.minimum_output_input_ratio {
        check(
            !v.denominator.is_zero()
                && receipt.atoms.integer() * v.denominator.integer()
                    >= intent.amount.integer() * v.numerator.integer(),
            "CCTP receipt ratio is below the approved floor.",
        )?;
    }
    for cap in &bounds.fee_caps {
        let sum = quote
            .fees
            .iter()
            .filter(|f| f.kind == cap.kind && f.amount.asset == cap.asset)
            .fold(BigUint::zero(), |n, f| n + f.amount.atoms.integer());
        check(
            sum <= cap.maximum.integer(),
            "CCTP fee exceeds the approved cap.",
        )?;
    }
    let encoded = serde_json::to_vec(&("cctp-v2", intent, m, bounds, wallet))
        .map_err(|_| invalid("Cannot encode CCTP authority scope."))?;
    Ok(format!("{:#x}", keccak256(encoded)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::sol_types::SolValue;
    const SENDER: &str = "0x1111111111111111111111111111111111111111";
    const RECIPIENT: &str = "0x2222222222222222222222222222222222222222";
    fn fixture(
        source: u64,
        destination: u64,
        mode: CctpTransferMode,
    ) -> (BridgeIntent, CctpQuoteMetadata) {
        (
            BridgeIntent {
                wallet: ResourceId("wallet".into()),
                source_address: SENDER.into(),
                source: Some(usdc_asset(source).unwrap()),
                destination_chain_id: Some(destination),
                solana_source: None,
                solana_destination: None,
                destination_assets: vec![usdc_asset(destination).unwrap()],
                recipient: RECIPIENT.into(),
                refund_to: SENDER.into(),
                allowed_refund_assets: vec![],
                allowed_solana_refund_assets: vec![],
                amount: Quantity::from_u64(1_000_000),
            },
            CctpQuoteMetadata {
                protocol_version: 2,
                source_domain: domain(source).unwrap(),
                destination_domain: domain(destination).unwrap(),
                transfer_mode: mode,
                max_fee: Quantity::from_u64(if mode == CctpTransferMode::Fast {
                    130
                } else {
                    0
                }),
                min_finality_threshold: mode.threshold(),
            },
        )
    }
    fn raw_message(intent: &BridgeIntent, m: &CctpQuoteMetadata, attested: bool) -> String {
        let mut b = Vec::new();
        b.extend(1u32.to_be_bytes());
        b.extend(m.source_domain.to_be_bytes());
        b.extend(m.destination_domain.to_be_bytes());
        b.extend(if attested { [7u8; 32] } else { [0u8; 32] });
        for a in [
            TOKEN_MESSENGER,
            TOKEN_MESSENGER,
            intent.source_address.as_str(),
        ] {
            b.extend(address(a).unwrap().into_word());
        }
        b.extend(m.min_finality_threshold.to_be_bytes());
        b.extend(
            if attested {
                m.min_finality_threshold
            } else {
                0
            }
            .to_be_bytes(),
        );
        b.extend(1u32.to_be_bytes());
        b.extend(
            address(intent.source.as_ref().unwrap().token.as_deref().unwrap())
                .unwrap()
                .into_word(),
        );
        b.extend(address(&intent.recipient).unwrap().into_word());
        b.extend(u256(&intent.amount).unwrap().to_be_bytes::<32>());
        b.extend(address(&intent.source_address).unwrap().into_word());
        b.extend(u256(&m.max_fee).unwrap().to_be_bytes::<32>());
        b.extend(
            if attested {
                u256(&m.max_fee).unwrap()
            } else {
                U256::ZERO
            }
            .to_be_bytes::<32>(),
        );
        b.extend(
            if attested && m.transfer_mode == CctpTransferMode::Fast {
                U256::from(900)
            } else {
                U256::ZERO
            }
            .to_be_bytes::<32>(),
        );
        hex_bytes(&b)
    }
    fn log<E: SolEvent>(emitter: &str, index: u32, e: E) -> CanonicalRawLog {
        CanonicalRawLog {
            address: emitter.into(),
            log_index: index,
            topics: e
                .encode_topics()
                .iter()
                .map(|t| format!("{:#x}", t.0))
                .collect(),
            data: hex_bytes(&e.encode_data()),
        }
    }
    fn source_logs(intent: &BridgeIntent, m: &CctpQuoteMetadata) -> Vec<CanonicalRawLog> {
        vec![
            log(
                MESSAGE_TRANSMITTER,
                3,
                Transmitter::MessageSent {
                    message: bytes(&raw_message(intent, m, false)).unwrap().into(),
                },
            ),
            log(
                TOKEN_MESSENGER,
                4,
                Messenger::DepositForBurn {
                    burnToken: address(intent.source.as_ref().unwrap().token.as_deref().unwrap())
                        .unwrap(),
                    amount: u256(&intent.amount).unwrap(),
                    depositor: address(SENDER).unwrap(),
                    mintRecipient: address(RECIPIENT).unwrap().into_word(),
                    destinationDomain: m.destination_domain,
                    destinationTokenMessenger: address(TOKEN_MESSENGER).unwrap().into_word(),
                    destinationCaller: address(SENDER).unwrap().into_word(),
                    maxFee: u256(&m.max_fee).unwrap(),
                    minFinalityThreshold: m.min_finality_threshold,
                    hookData: Bytes::new(),
                },
            ),
        ]
    }
    #[test]
    fn all_six_routes_have_exact_native_tokens_domains_and_burn_calldata() {
        for source in [1, 8453, 42161] {
            for destination in [1, 8453, 42161] {
                if source == destination {
                    continue;
                }
                for mode in [CctpTransferMode::Standard, CctpTransferMode::Fast] {
                    let (intent, m) = fixture(source, destination, mode);
                    let call = burn_call(&intent, &m).unwrap();
                    let burn = Messenger::depositForBurnCall::abi_decode_validate(
                        &bytes(&call.data).unwrap(),
                    )
                    .unwrap();
                    assert_eq!(burn.destinationDomain, domain(destination).unwrap());
                    assert_eq!(burn.minFinalityThreshold, mode.threshold());
                    assert_eq!(burn.destinationCaller, address(SENDER).unwrap().into_word());
                    assert!(verify_source_logs(&source_logs(&intent, &m), &intent, &m).is_ok());
                }
            }
        }
        assert!(domain(4663).is_err());
        assert!(usdc_asset(10).is_err());
    }
    #[test]
    fn native_usdc_only_and_fee_must_leave_positive_mint() {
        let (mut intent, mut m) = fixture(1, 8453, CctpTransferMode::Standard);
        m.max_fee = intent.amount.clone();
        assert!(burn_call(&intent, &m).is_err());
        m.max_fee = Quantity::zero();
        intent.source.as_mut().unwrap().token = Some(RECIPIENT.into());
        assert!(burn_call(&intent, &m).is_err());
    }
    #[test]
    fn attestation_permits_only_four_offchain_fields() {
        let (intent, m) = fixture(1, 42161, CctpTransferMode::Fast);
        let source = raw_message(&intent, &m, false);
        let attested = raw_message(&intent, &m, true);
        assert!(validate_attested_message(&source, &m, &attested).is_ok());
        assert_ne!(
            keccak256(bytes(&source).unwrap()),
            keccak256(bytes(&attested).unwrap())
        );
        assert_eq!(
            immutable_message_hash(&source).unwrap(),
            immutable_message_hash(&attested).unwrap()
        );
        for offset in [7, 11, 75, 107, 139, 143, 183, 215, 247, 279, 311] {
            let mut changed = bytes(&attested).unwrap();
            changed[offset] ^= 1;
            assert!(
                validate_attested_message(&source, &m, &hex_bytes(&changed)).is_err(),
                "offset {offset}"
            );
        }
    }
    #[test]
    fn fast_can_finalize_standard_cannot_downgrade_and_fee_cannot_grow() {
        let (intent, m) = fixture(1, 8453, CctpTransferMode::Fast);
        let source = raw_message(&intent, &m, false);
        let mut attested = bytes(&raw_message(&intent, &m, true)).unwrap();
        attested[144..148].copy_from_slice(&2000u32.to_be_bytes());
        assert!(validate_attested_message(&source, &m, &hex_bytes(&attested)).is_ok());
        attested[312..344].copy_from_slice(&U256::from(131).to_be_bytes::<32>());
        assert!(validate_attested_message(&source, &m, &hex_bytes(&attested)).is_err());
        let (intent, m) = fixture(1, 8453, CctpTransferMode::Standard);
        let mut attested = bytes(&raw_message(&intent, &m, true)).unwrap();
        attested[144..148].copy_from_slice(&1000u32.to_be_bytes());
        assert!(
            validate_attested_message(&raw_message(&intent, &m, false), &m, &hex_bytes(&attested))
                .is_err()
        );
    }
    #[test]
    fn malformed_padding_versions_hooks_and_nonce_are_rejected() {
        let (intent, m) = fixture(1, 8453, CctpTransferMode::Fast);
        let raw = raw_message(&intent, &m, true);
        for offset in [0, 44, 76, 108, 148, 152, 184, 248] {
            let mut b = bytes(&raw).unwrap();
            b[offset] = 2;
            assert!(decode_message(&hex_bytes(&b)).is_err());
        }
        let mut b = bytes(&raw).unwrap();
        b.push(0);
        assert!(decode_message(&hex_bytes(&b)).is_err());
        let source = raw_message(&intent, &m, false);
        assert!(validate_attested_message(&source, &m, &source).is_err());
    }
    #[test]
    fn fees_round_up_exactly_without_floating_math() {
        assert_eq!(
            fee_from_bps(&Quantity::from_u64(1_000_000), "1.3").unwrap(),
            Quantity::from_u64(130)
        );
        assert_eq!(
            fee_from_bps(&Quantity::from_u64(1), "1.3").unwrap(),
            Quantity::from_u64(1)
        );
        assert_eq!(
            fee_from_bps(
                &Quantity::new("1000000000000000000000000000001").unwrap(),
                "0.0001"
            )
            .unwrap(),
            Quantity::new("10000000000000000000001").unwrap()
        );
        for invalid in ["-1", "NaN", "1e3", "1.2.3", ""] {
            assert!(fee_from_bps(&Quantity::from_u64(1), invalid).is_err());
        }
    }
    #[test]
    fn source_events_must_match_emitter_order_and_every_burn_field() {
        let (intent, m) = fixture(8453, 42161, CctpTransferMode::Fast);
        let logs = source_logs(&intent, &m);
        assert!(verify_source_logs(&logs, &intent, &m).is_ok());
        let mut wrong = logs.clone();
        wrong[0].address = RECIPIENT.into();
        assert!(verify_source_logs(&wrong, &intent, &m).is_err());
        let mut wrong = logs.clone();
        wrong.push(logs[0].clone());
        assert!(verify_source_logs(&wrong, &intent, &m).is_err());
        let mut wrong = logs.clone();
        wrong[1].log_index = 2;
        assert!(verify_source_logs(&wrong, &intent, &m).is_err());
        let mut wrong = logs;
        let mut data = bytes(&wrong[1].data).unwrap();
        data[31] ^= 1;
        wrong[1].data = hex_bytes(&data);
        assert!(verify_source_logs(&wrong, &intent, &m).is_err());
    }
    #[test]
    fn destination_requires_exact_receive_and_net_native_usdc_mint() {
        let (intent, m) = fixture(1, 8453, CctpTransferMode::Fast);
        let raw = raw_message(&intent, &m, true);
        let decoded = decode_message(&raw).unwrap();
        let receive = log(
            MESSAGE_TRANSMITTER,
            5,
            Transmitter::MessageReceived {
                caller: address(SENDER).unwrap(),
                sourceDomain: m.source_domain,
                nonce: hash(&decoded.nonce).unwrap(),
                sender: address(TOKEN_MESSENGER).unwrap().into_word(),
                finalityThresholdExecuted: 1000,
                messageBody: Bytes::from(bytes(&raw).unwrap()[148..].to_vec()),
            },
        );
        let token = usdc_asset(8453).unwrap().token.unwrap();
        let mint = log(
            &token,
            4,
            Token::Transfer {
                from: Address::ZERO,
                to: address(RECIPIENT).unwrap(),
                value: U256::from(999_870),
            },
        );
        assert_eq!(
            verify_destination_logs(&[mint.clone(), receive.clone()], &raw).unwrap(),
            5
        );
        assert!(verify_destination_logs(std::slice::from_ref(&receive), &raw).is_err());
        let mut wrong = mint.clone();
        wrong.address = TOKEN_MESSENGER.into();
        assert!(verify_destination_logs(&[wrong, receive.clone()], &raw).is_err());
        let mut wrong = mint;
        wrong.data = hex_bytes(&U256::from(1_000_000).abi_encode());
        assert!(verify_destination_logs(&[wrong, receive], &raw).is_err());
    }

    #[test]
    fn solana_source_receive_binds_raw_program_sender_and_forwarder_mint() {
        let proxy: Address = "0x1234567890123456789012345678901234567890"
            .parse()
            .unwrap();
        let mut raw = vec![0u8; MESSAGE_LEN + 56];
        let put_u32 = |raw: &mut [u8], offset: usize, value: u32| {
            raw[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
        };
        put_u32(&mut raw, 0, 1);
        put_u32(&mut raw, 4, solana::DOMAIN);
        put_u32(&mut raw, 8, hypercore::DOMAIN);
        raw[12..44].copy_from_slice(&[7; 32]);
        raw[44..76].copy_from_slice(&[8; 32]);
        raw[76..108].copy_from_slice(&address(TOKEN_MESSENGER).unwrap().into_word().0);
        raw[108..140].copy_from_slice(&address(hypercore::FORWARDER).unwrap().into_word().0);
        put_u32(&mut raw, 140, 1000);
        put_u32(&mut raw, 144, 1000);
        put_u32(&mut raw, 148, 1);
        raw[152..184].copy_from_slice(&[9; 32]);
        raw[184..216].copy_from_slice(&address(hypercore::FORWARDER).unwrap().into_word().0);
        raw[216..248].copy_from_slice(&U256::from(1_000_000u64).to_be_bytes::<32>());
        raw[248..280].copy_from_slice(&[10; 32]);
        raw[280..312].copy_from_slice(&U256::from(1_000u64).to_be_bytes::<32>());
        raw[312..344].copy_from_slice(&U256::from(200u64).to_be_bytes::<32>());
        raw[MESSAGE_LEN..].copy_from_slice(hypercore::forward_hook(proxy).unwrap().as_ref());
        let raw = hex_bytes(&raw);
        let message = decode_message(&raw).unwrap();
        let receive = log(
            MESSAGE_TRANSMITTER,
            5,
            Transmitter::MessageReceived {
                caller: address(hypercore::FORWARDER).unwrap(),
                sourceDomain: solana::DOMAIN,
                nonce: hash(&message.nonce).unwrap(),
                sender: B256::from([8; 32]),
                finalityThresholdExecuted: 1000,
                messageBody: Bytes::from(bytes(&raw).unwrap()[148..].to_vec()),
            },
        );
        let mint = log(
            hypercore::NATIVE_USDC,
            4,
            Token::Transfer {
                from: Address::ZERO,
                to: address(hypercore::FORWARDER).unwrap(),
                value: U256::from(999_800u64),
            },
        );
        assert_eq!(
            verify_destination_logs(&[mint.clone(), receive.clone()], &raw).unwrap(),
            5
        );
        let mut wrong = receive;
        let mut event_data = bytes(&wrong.data).unwrap();
        event_data[63] ^= 1;
        wrong.data = hex_bytes(&event_data);
        assert!(verify_destination_logs(&[mint, wrong], &raw).is_err());
    }

    #[test]
    fn hypercore_destination_accepts_only_fixed_hooked_forwarder_mint() {
        let (intent, m) = fixture(42161, 999, CctpTransferMode::Fast);
        let proxy: Address = "0x1234567890123456789012345678901234567890"
            .parse()
            .unwrap();
        let mut raw = bytes(&raw_message(&intent, &m, true)).unwrap();
        raw[76..108].copy_from_slice(&address(TOKEN_MESSENGER).unwrap().into_word().0);
        raw[108..140].copy_from_slice(&address(hypercore::FORWARDER).unwrap().into_word().0);
        raw[184..216].copy_from_slice(&address(hypercore::FORWARDER).unwrap().into_word().0);
        raw.extend_from_slice(hypercore::forward_hook(proxy).unwrap().as_ref());
        let raw = hex_bytes(&raw);
        let message = decode_message(&raw).unwrap();
        let receive = log(
            MESSAGE_TRANSMITTER,
            5,
            Transmitter::MessageReceived {
                caller: address(hypercore::FORWARDER).unwrap(),
                sourceDomain: m.source_domain,
                nonce: hash(&message.nonce).unwrap(),
                sender: address(TOKEN_MESSENGER).unwrap().into_word(),
                finalityThresholdExecuted: 1000,
                messageBody: Bytes::from(bytes(&raw).unwrap()[148..].to_vec()),
            },
        );
        let mint = log(
            hypercore::NATIVE_USDC,
            4,
            Token::Transfer {
                from: Address::ZERO,
                to: address(hypercore::FORWARDER).unwrap(),
                value: U256::from(999_870),
            },
        );
        assert_eq!(
            verify_destination_logs(&[mint.clone(), receive.clone()], &raw).unwrap(),
            5
        );
        let mut wrong = bytes(&raw).unwrap();
        let last = wrong.len() - 1;
        wrong[last] ^= 1;
        assert!(decode_message(&hex_bytes(&wrong)).is_err());
        let mut wrong = mint;
        wrong.address = usdc_asset(42161).unwrap().token.unwrap();
        assert!(verify_destination_logs(&[wrong, receive], &raw).is_err());
    }
    #[test]
    fn mint_is_exact_destination_contract_and_signature_payload() {
        let (intent, m) = fixture(1, 42161, CctpTransferMode::Fast);
        let raw = raw_message(&intent, &m, true);
        let attestation = hex_bytes(&[1u8; 65]);
        let call = mint_call(42161, &raw, &attestation).unwrap();
        assert_eq!(call.to, MESSAGE_TRANSMITTER);
        assert!(call.value.is_zero());
        assert_eq!(
            decode_mint_call(42161, MESSAGE_TRANSMITTER, &call.data).unwrap(),
            (raw.clone(), attestation.clone())
        );
        assert!(decode_mint_call(8453, MESSAGE_TRANSMITTER, &call.data).is_err());
        assert!(decode_mint_call(42161, TOKEN_MESSENGER, &call.data).is_err());
        assert!(decode_mint_call(42161, MESSAGE_TRANSMITTER, &format!("{}00", call.data)).is_err());
        let mut wrong = bytes(&call.data).unwrap();
        wrong[0] ^= 1;
        assert!(decode_mint_call(42161, MESSAGE_TRANSMITTER, &hex_bytes(&wrong)).is_err());
        assert!(mint_call(8453, &raw, &attestation).is_err());
        assert!(mint_call(42161, &raw, "0x").is_err());
    }
    #[test]
    fn config_rejects_unreviewed_network_origins() {
        assert!(CctpConfig::reviewed().validate().is_ok());
        assert!(
            CctpConfig {
                iris_url: "http://127.0.0.1:8888".into()
            }
            .validate()
            .is_ok()
        );
        for url in [
            "http://iris-api.circle.com",
            "https://example.com",
            "https://iris-api.circle.com.evil.example",
        ] {
            assert!(
                CctpConfig {
                    iris_url: url.into()
                }
                .validate()
                .is_err()
            );
        }
    }

    fn saved_quote(intent: &BridgeIntent, metadata: CctpQuoteMetadata) -> BridgeQuote {
        let receipt = usdc_asset(intent.destination_chain_id.unwrap())
            .unwrap()
            .amount(intent.amount.checked_sub(&metadata.max_fee).unwrap());
        BridgeQuote {
            quote_id: "fixture".into(),
            provider: PROVIDER_ID.into(),
            route_id: route_id(intent).unwrap(),
            intent: intent.clone(),
            gross_debit: intent
                .source
                .as_ref()
                .unwrap()
                .amount(intent.amount.clone()),
            expected_receipt: receipt.clone(),
            minimum_receipt: receipt,
            fees: vec![
                BridgeFee {
                    amount: EvmAsset {
                        chain_id: intent.source.as_ref().unwrap().chain_id,
                        token: None,
                        decimals: 18,
                    }
                    .amount(Quantity::from_u64(20)),
                    kind: FeeKind::Gas,
                    payer: SENDER.into(),
                },
                BridgeFee {
                    amount: EvmAsset {
                        chain_id: intent.destination_chain_id.unwrap(),
                        token: None,
                        decimals: 18,
                    }
                    .amount(Quantity::from_u64(30)),
                    kind: FeeKind::Gas,
                    payer: SENDER.into(),
                },
                BridgeFee {
                    amount: usdc_asset(intent.destination_chain_id.unwrap())
                        .unwrap()
                        .amount(metadata.max_fee.clone()),
                    kind: FeeKind::WithheldFromOutput,
                    payer: SENDER.into(),
                },
            ],
            approval: None,
            protocol_steps: vec![
                step(
                    intent,
                    0,
                    BridgeStepKind::Deposit,
                    burn_call(intent, &metadata).unwrap(),
                ),
                step(
                    intent,
                    1,
                    BridgeStepKind::Mint,
                    mint_template(intent).unwrap(),
                ),
            ],
            estimated_arrival_seconds: Some(2400),
            observed_at: Utc::now().to_rfc3339(),
            valid_until: (Utc::now() + Duration::seconds(90)).to_rfc3339(),
            class: QuoteClass::Executable,
            refund_paths: vec![],
            order_solver: None,
            protocol_order_id: None,
            provider_request_id: None,
            cctp: Some(metadata),
        }
    }
    #[test]
    fn fee_refresh_within_cap_preserves_scope_and_rejects_tampered_topology() {
        let (intent, mut metadata) = fixture(1, 8453, CctpTransferMode::Fast);
        let bounds = BridgeBounds {
            fee_caps: vec![FeeCap {
                asset: usdc_asset(8453).unwrap().asset_id(),
                kind: FeeKind::WithheldFromOutput,
                maximum: Quantity::from_u64(200),
                maximum_usd_micros: None,
            }],
            ..BridgeBounds::default()
        };
        metadata.max_fee = bounded_max_fee(
            &intent,
            &bounds,
            &fee_from_bps(&intent.amount, "1.3").unwrap(),
        )
        .unwrap();
        let first = saved_quote(&intent, metadata.clone());
        let digest = scope_digest(&first, &bounds, SENDER).unwrap();
        metadata.max_fee = bounded_max_fee(
            &intent,
            &bounds,
            &fee_from_bps(&intent.amount, "1.7").unwrap(),
        )
        .unwrap();
        let mut refreshed = saved_quote(&intent, metadata);
        refreshed.quote_id = "refreshed".into();
        refreshed.fees[0].amount.atoms = Quantity::from_u64(25);
        assert_eq!(scope_digest(&refreshed, &bounds, SENDER).unwrap(), digest);
        assert!(bounded_max_fee(&intent, &bounds, &Quantity::from_u64(201)).is_err());
        let mut changed = refreshed.clone();
        changed.protocol_steps[1].evm_call_mut().unwrap().data = "0x1234".into();
        assert!(scope_digest(&changed, &bounds, SENDER).is_err());
        let mut changed = refreshed.clone();
        changed.protocol_steps[0].evm_call_mut().unwrap().to = RECIPIENT.into();
        assert!(scope_digest(&changed, &bounds, SENDER).is_err());
        let mut changed = refreshed;
        changed.minimum_receipt.atoms = intent.amount.clone();
        assert!(scope_digest(&changed, &bounds, SENDER).is_err());
    }

    struct DeploymentRpc {
        chain: u64,
        wrong_pair: bool,
    }
    #[async_trait]
    impl gofin_evm::EvmRpc for DeploymentRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            let block = if self.chain == 1 { "0x20" } else { "0x30" };
            Ok(match method {
                "eth_chainId" => json!(format!("{:#x}", self.chain)),
                "eth_blockNumber" => json!(block),
                "eth_getCode" => {
                    assert_eq!(params[1], block);
                    json!("0x6000")
                }
                "eth_call" => {
                    assert_eq!(params[1], block);
                    let data = bytes(params[0]["data"].as_str().unwrap()).unwrap();
                    let encoded = if data
                        .starts_with(&Messenger::localMessageTransmitterCall::SELECTOR)
                    {
                        address(MESSAGE_TRANSMITTER)?.abi_encode()
                    } else if data.starts_with(&Messenger::localMinterCall::SELECTOR) {
                        address(TOKEN_MINTER)?.abi_encode()
                    } else if data.starts_with(&Messenger::messageBodyVersionCall::SELECTOR)
                        || data.starts_with(&Transmitter::versionCall::SELECTOR)
                    {
                        1u32.abi_encode()
                    } else if data.starts_with(&Messenger::remoteTokenMessengersCall::SELECTOR) {
                        address(TOKEN_MESSENGER)?.into_word().abi_encode()
                    } else if data.starts_with(&Transmitter::localDomainCall::SELECTOR) {
                        domain(self.chain)?.abi_encode()
                    } else if data.starts_with(&Token::decimalsCall::SELECTOR) {
                        U256::from(6).abi_encode()
                    } else if data.starts_with(&Minter::localTokenMessengerCall::SELECTOR) {
                        address(TOKEN_MESSENGER)?.abi_encode()
                    } else if data.starts_with(&Minter::getLocalTokenCall::SELECTOR) {
                        if self.wrong_pair {
                            Address::ZERO.abi_encode()
                        } else {
                            address(usdc_asset(self.chain)?.token.as_deref().unwrap())?.abi_encode()
                        }
                    } else {
                        panic!("Unexpected deployment call")
                    };
                    json!(hex_bytes(&encoded))
                }
                other => panic!("Unexpected deployment RPC {other}"),
            })
        }
    }

    #[derive(Default)]
    struct DeploymentConcurrency {
        active: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
        total: std::sync::atomic::AtomicUsize,
    }
    struct DelayedDeploymentRpc {
        inner: DeploymentRpc,
        chain_counts: Arc<DeploymentConcurrency>,
        all_counts: Arc<DeploymentConcurrency>,
    }
    #[async_trait]
    impl gofin_evm::EvmRpc for DelayedDeploymentRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            use std::sync::atomic::Ordering::SeqCst;
            for counts in [&self.chain_counts, &self.all_counts] {
                let active = counts.active.fetch_add(1, SeqCst) + 1;
                counts.peak.fetch_max(active, SeqCst);
                counts.total.fetch_add(1, SeqCst);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let result = self.inner.request(method, params).await;
            for counts in [&self.chain_counts, &self.all_counts] {
                counts.active.fetch_sub(1, SeqCst);
            }
            result
        }
    }
    #[tokio::test]
    async fn deployment_transport_delays_overlap_with_bounded_same_block_reads() {
        use std::sync::atomic::Ordering::SeqCst;
        let all_counts = Arc::new(DeploymentConcurrency::default());
        let chain_counts =
            [1, 8453].map(|chain| (chain, Arc::new(DeploymentConcurrency::default())));
        let runtimes = chain_counts
            .iter()
            .map(|(chain, counts)| {
                (
                    *chain,
                    Arc::new(EvmRuntime::new(
                        Arc::new(DelayedDeploymentRpc {
                            inner: DeploymentRpc {
                                chain: *chain,
                                wrong_pair: false,
                            },
                            chain_counts: counts.clone(),
                            all_counts: all_counts.clone(),
                        }),
                        *chain,
                        None,
                    )),
                )
            })
            .collect();
        let provider = CctpProvider::new(runtimes, CctpConfig::reviewed()).unwrap();
        let (intent, _) = fixture(1, 8453, CctpTransferMode::Standard);
        provider.verify_deployments(&intent).await.unwrap();
        // Measure overlap instead of wall-clock duration so busy CI hosts
        // cannot make the latency regression flaky. The inner RPC checks the
        // distinct captured block for every code and relationship read.
        for (_, counts) in chain_counts {
            assert_eq!(counts.total.load(SeqCst), 15);
            assert_eq!(counts.peak.load(SeqCst), 4);
            assert_eq!(counts.active.load(SeqCst), 0);
        }
        assert!(all_counts.peak.load(SeqCst) > 4);
        assert!(all_counts.peak.load(SeqCst) <= 8);
        assert_eq!(all_counts.total.load(SeqCst), 30);
    }
    #[tokio::test]
    async fn deployed_token_pair_and_protocol_bindings_are_checked_at_fixed_blocks() {
        let (intent, _) = fixture(1, 8453, CctpTransferMode::Standard);
        for wrong_pair in [false, true] {
            let runtimes = [1, 8453]
                .into_iter()
                .map(|chain| {
                    (
                        chain,
                        Arc::new(EvmRuntime::new(
                            Arc::new(DeploymentRpc {
                                chain,
                                wrong_pair: wrong_pair && chain == 8453,
                            }),
                            chain,
                            None,
                        )),
                    )
                })
                .collect();
            let provider = CctpProvider::new(runtimes, CctpConfig::reviewed()).unwrap();
            assert_eq!(
                provider.verify_deployments(&intent).await.is_err(),
                wrong_pair
            );
        }
    }

    struct AttesterRpc {
        chain: u64,
        signers: Vec<Address>,
        threshold: u64,
    }
    #[async_trait]
    impl gofin_evm::EvmRpc for AttesterRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            Ok(match method {
                "eth_chainId" => json!(format!("{:#x}", self.chain)),
                "eth_blockNumber" => json!(if self.chain == 1 { "0x100" } else { "0x100000" }),
                "eth_call" => {
                    let raw = bytes(params[0]["data"].as_str().unwrap()).unwrap();
                    if raw.starts_with(&Transmitter::signatureThresholdCall::SELECTOR) {
                        json!(hex_bytes(&U256::from(self.threshold).abi_encode()))
                    } else if raw.starts_with(&Transmitter::isEnabledAttesterCall::SELECTOR) {
                        let call = Transmitter::isEnabledAttesterCall::abi_decode(&raw).unwrap();
                        json!(hex_bytes(
                            &self.signers.contains(&call.attester).abi_encode()
                        ))
                    } else {
                        panic!("Unexpected fixture eth_call")
                    }
                }
                other => panic!("Unexpected fixture RPC {other}"),
            })
        }
    }
    #[tokio::test]
    async fn signatures_use_onchain_threshold_order_membership_and_raw_message_hash() {
        use alloy::signers::{SignerSync, local::PrivateKeySigner};
        let key1: PrivateKeySigner = format!("{:064x}", 1).parse().unwrap();
        let key2: PrivateKeySigner = format!("{:064x}", 2).parse().unwrap();
        let (intent, m) = fixture(1, 8453, CctpTransferMode::Fast);
        let raw = raw_message(&intent, &m, true);
        let digest = keccak256(bytes(&raw).unwrap());
        let mut signed = [
            (
                key1.address(),
                key1.sign_hash_sync(&digest).unwrap().as_bytes(),
            ),
            (
                key2.address(),
                key2.sign_hash_sync(&digest).unwrap().as_bytes(),
            ),
        ];
        signed.sort_by_key(|s| s.0);
        let rpc = Arc::new(AttesterRpc {
            chain: 8453,
            signers: signed.iter().map(|s| s.0).collect(),
            threshold: 2,
        });
        let provider = CctpProvider::new(
            BTreeMap::from([(8453, Arc::new(EvmRuntime::new(rpc, 8453, None)))]),
            CctpConfig::reviewed(),
        )
        .unwrap();
        let correct: Vec<u8> = signed.iter().flat_map(|s| s.1).collect();
        provider
            .verify_attestation(8453, &raw, &hex_bytes(&correct))
            .await
            .unwrap();
        let reversed: Vec<u8> = signed.iter().rev().flat_map(|s| s.1).collect();
        assert!(
            provider
                .verify_attestation(8453, &raw, &hex_bytes(&reversed))
                .await
                .is_err()
        );
        let duplicate: Vec<u8> = [signed[0].1, signed[0].1].into_iter().flatten().collect();
        assert!(
            provider
                .verify_attestation(8453, &raw, &hex_bytes(&duplicate))
                .await
                .is_err()
        );
        assert!(
            provider
                .verify_attestation(8453, &raw, &hex_bytes(&correct[..65]))
                .await
                .is_err()
        );
        let mut altered = bytes(&raw).unwrap();
        altered[247] ^= 1;
        assert!(
            provider
                .verify_attestation(8453, &hex_bytes(&altered), &hex_bytes(&correct))
                .await
                .is_err()
        );
        let mut parity = correct;
        parity[64] -= 27;
        assert!(
            provider
                .verify_attestation(8453, &raw, &hex_bytes(&parity))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn arbitrum_expiry_reads_l1_height() {
        let mut runtimes = BTreeMap::new();
        for chain in [1, 42161] {
            runtimes.insert(
                chain,
                Arc::new(EvmRuntime::new(
                    Arc::new(AttesterRpc {
                        chain,
                        signers: vec![],
                        threshold: 1,
                    }),
                    chain,
                    None,
                )),
            );
        }
        let provider = CctpProvider::new(runtimes, CctpConfig::reviewed()).unwrap();
        assert_eq!(provider.expiration_block_number(42161).await.unwrap(), 256);
    }
    fn http_fixture(status: u16, body: String) -> (CctpProvider, std::thread::JoinHandle<String>) {
        http_fixture_notifying(status, body, None)
    }
    fn http_fixture_notifying(
        status: u16,
        body: String,
        received: Option<Arc<tokio::sync::Notify>>,
    ) -> (CctpProvider, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = vec![0; 4096];
            let n = socket.read(&mut request).unwrap();
            write!(socket,"HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
            if let Some(received) = received {
                received.notify_one();
            }
            String::from_utf8(request[..n].to_vec()).unwrap()
        });
        (
            CctpProvider::new(
                BTreeMap::new(),
                CctpConfig {
                    iris_url: format!("http://{address}"),
                },
            )
            .unwrap(),
            server,
        )
    }
    struct QuoteConcurrencyRpc {
        deployment: DeploymentRpc,
        iris_received: Arc<tokio::sync::Notify>,
        deployment_started: std::sync::atomic::AtomicBool,
        gas_overlap: Arc<tokio::sync::Barrier>,
        estimates: std::sync::atomic::AtomicUsize,
        destination_fees: std::sync::atomic::AtomicUsize,
    }
    #[async_trait]
    impl gofin_evm::EvmRpc for QuoteConcurrencyRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            use std::sync::atomic::Ordering::SeqCst;
            if self.deployment.chain == 1
                && method == "eth_chainId"
                && !self.deployment_started.swap(true, SeqCst)
            {
                // Sequential deployment-then-Iris cannot pass this gate.
                self.iris_received.notified().await;
            }
            Ok(match method {
                "eth_call" if params[1] == "latest" => {
                    let data = bytes(params[0]["data"].as_str().unwrap()).unwrap();
                    if data.starts_with(&Token::allowanceCall::SELECTOR) {
                        json!(hex_bytes(&U256::ZERO.abi_encode()))
                    } else {
                        json!("0x")
                    }
                }
                "eth_estimateGas" => {
                    if self.estimates.fetch_add(1, SeqCst) == 0 {
                        // First source simulation must overlap destination gas.
                        self.gas_overlap.wait().await;
                    }
                    json!("0x186a0")
                }
                "eth_getBlockByNumber" => {
                    if self.deployment.chain == 42161 {
                        self.destination_fees.fetch_add(1, SeqCst);
                        self.gas_overlap.wait().await;
                    }
                    json!({"baseFeePerGas":"0x64"})
                }
                "eth_maxPriorityFeePerGas" => json!("0x989680"),
                "eth_getTransactionCount" => json!("0x0"),
                "eth_getBalance" => json!("0xffffffffffffffffffffffff"),
                _ => return self.deployment.request(method, params).await,
            })
        }
    }

    #[tokio::test]
    async fn quote_overlaps_independent_reads_and_keeps_validation_and_fresh_expiry() {
        use std::sync::atomic::Ordering::SeqCst;
        for wrong_pair in [false, true] {
            let iris_received = Arc::new(tokio::sync::Notify::new());
            let (mut provider, server) = http_fixture_notifying(
                200,
                json!([{"finalityThreshold":2000,"minimumFee":0}]).to_string(),
                Some(iris_received.clone()),
            );
            let gas_overlap = Arc::new(tokio::sync::Barrier::new(2));
            let runtimes = [1, 42161].map(|chain| {
                Arc::new(QuoteConcurrencyRpc {
                    deployment: DeploymentRpc {
                        chain,
                        wrong_pair: wrong_pair && chain == 1,
                    },
                    iris_received: iris_received.clone(),
                    deployment_started: Default::default(),
                    gas_overlap: gas_overlap.clone(),
                    estimates: Default::default(),
                    destination_fees: Default::default(),
                })
            });
            for rpc in &runtimes {
                let chain = rpc.deployment.chain;
                provider
                    .runtimes
                    .insert(chain, Arc::new(EvmRuntime::new(rpc.clone(), chain, None)));
            }
            let (intent, _) = fixture(1, 42161, CctpTransferMode::Standard);
            // A deadlock guard, not a wall-clock speed assertion: the gates
            // above prove concurrent polling of each independent pair.
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                provider.quote(&BridgeQuoteRequest {
                    cctp: Some(CctpOptions {
                        transfer_mode: CctpTransferMode::Standard,
                    }),
                    intent,
                    bounds: BridgeBounds::default(),
                }),
            )
            .await
            .expect("independent quote reads must overlap");
            assert!(server.join().unwrap().contains("/v2/burn/USDC/fees/0/3"));
            if wrong_pair {
                assert!(result.is_err());
                assert_eq!(runtimes[0].estimates.load(SeqCst), 0);
                continue;
            }
            let batch = result.unwrap();
            assert_eq!(runtimes[0].estimates.load(SeqCst), 2);
            assert_eq!(runtimes[1].destination_fees.load(SeqCst), 1);
            let quote = &batch.quotes[0];
            let observed = chrono::DateTime::parse_from_rfc3339(&quote.observed_at).unwrap();
            let expiry = chrono::DateTime::parse_from_rfc3339(&quote.valid_until).unwrap();
            assert_eq!(expiry - observed, Duration::seconds(90));
            provider
                .prepare(quote, &BridgeBounds::default(), SENDER)
                .unwrap();
            assert_eq!(quote.protocol_steps.len(), 3);
            assert_eq!(quote.minimum_receipt.atoms, Quantity::from_u64(1_000_000));
        }
    }

    #[tokio::test]
    async fn iris_transport_distinguishes_pending_ready_and_wrong_source() {
        let tx = format!("0x{}", "ab".repeat(32));
        let (provider, server) = http_fixture(404, "{}".into());
        assert_eq!(
            provider.fetch_attestation(0, &tx).await.unwrap(),
            CctpAttestationStatus::Pending
        );
        let request = server.join().unwrap();
        assert!(request.contains(&format!("GET /v2/messages/0?transactionHash={tx}")));
        let (provider, server) = http_fixture(404, "{}".into());
        assert_eq!(
            provider.fetch_attestation(19, &tx).await.unwrap(),
            CctpAttestationStatus::Pending
        );
        let request = server.join().unwrap();
        assert!(request.contains(&format!("GET /v2/messages/19?transactionHash={tx}")));
        let solana_signature = bs58::encode([7u8; 64]).into_string();
        let (provider, server) = http_fixture(404, "{}".into());
        assert_eq!(
            provider
                .fetch_attestation(5, &solana_signature)
                .await
                .unwrap(),
            CctpAttestationStatus::Pending
        );
        let request = server.join().unwrap();
        assert!(request.contains(&format!("transactionHash={solana_signature}")));
        let (intent, m) = fixture(1, 8453, CctpTransferMode::Fast);
        let raw = raw_message(&intent, &m, true);
        let forward_hash = format!("0x{}", "ef".repeat(32));
        let response = json!({"sourceTxHash":tx,"messages":[{"status":"complete","cctpVersion":2,"message":raw,"attestation":hex_bytes(&[1u8;65]),"forwardState":"COMPLETE","forwardTxHash":forward_hash}]});
        let (provider, server) = http_fixture(200, response.to_string());
        let result = provider.fetch_attestation(0, &tx).await.unwrap();
        server.join().unwrap();
        assert!(
            matches!(result,CctpAttestationStatus::Ready{messages} if messages.len()==1 && messages[0].forward_tx_hash.as_deref() == Some(forward_hash.as_str()))
        );
        let mut wrong = response;
        wrong["sourceTxHash"] = json!(format!("0x{}", "cd".repeat(32)));
        let (provider, server) = http_fixture(200, wrong.to_string());
        assert!(provider.fetch_attestation(0, &tx).await.is_err());
        server.join().unwrap();
        let (provider, server) = http_fixture(429, "{}".into());
        assert!(provider.fetch_attestation(0, &tx).await.is_err());
        server.join().unwrap();
    }
}
