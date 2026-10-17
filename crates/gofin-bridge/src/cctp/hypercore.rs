//! Reviewed CCTP V2 native-USDC deposit into a HyperCore spot account.
//!
//! HyperCore is CCTP domain 19. Circle mints to its deployed CctpForwarder on
//! HyperEVM, which forwards to an exact HyperCore proxy using the fixed
//! `cctp-forward` hook. The broker owns durable submission identity and proof
//! tracking; this module only constructs and validates protocol data.
use super::*;

pub const HYPEREVM_CHAIN_ID: u64 = 999;
pub const DOMAIN: u32 = 19;
pub const FORWARDER: &str = "0xb21d281deDB17AE5B501F6AA8256fe38C4e45757";
pub const CORE_DEPOSIT_WALLET: &str = "0x6B9E773128f453f5c2C60935Ee2DE2CBc5390A24";
pub const NATIVE_USDC: &str = "0xb88339CB7199b77E23DB6E890353E22632Ba630f";
pub const SPOT_DEX: u32 = u32::MAX;
const HOOK_MAGIC: &[u8; 24] = b"cctp-forward\0\0\0\0\0\0\0\0\0\0\0\0";
const OUTBOUND_HOOK_MAGIC: &[u8; 32] = b"cctp-forward\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";

fn immutable_hook_message_hash(raw: &str) -> Result<String> {
    let mut bytes = bytes(raw)?;
    check(
        bytes.len() == MESSAGE_LEN + 56,
        "HyperCore CCTP message must carry exactly its reviewed hook.",
    )?;
    for range in [12..44, 144..148, 312..344, 344..376] {
        bytes[range].fill(0);
    }
    Ok(format!("{:#x}", keccak256(bytes)))
}

sol! {
    interface HyperCoreMessenger {
        function depositForBurnWithHook(uint256 amount, uint32 destinationDomain, bytes32 mintRecipient, address burnToken, bytes32 destinationCaller, uint256 maxFee, uint32 minFinalityThreshold, bytes hookData) external;
    }
}

fn hypercore_error(message: &str) -> GoFinError {
    bridge_error(
        "CCTP_VALIDATION_FAILED",
        "bridge",
        message,
        "Inspect the original funding operation and its verified evidence; never repeat a burn.",
    )
}

/// Encode the single reviewed Circle forwarder hook. Trailing data is not
/// accepted because it would change the destination-side behavior.
pub fn forward_hook(proxy: Address) -> Result<Bytes> {
    ensure(
        proxy != Address::ZERO,
        "CCTP_VALIDATION_FAILED",
        "bridge",
        "HyperCore proxy must be nonzero.",
    )?;
    let mut hook = Vec::with_capacity(56);
    hook.extend_from_slice(HOOK_MAGIC);
    hook.extend_from_slice(&0u32.to_be_bytes()); // hook version
    hook.extend_from_slice(&24u32.to_be_bytes()); // encoded hook data length
    hook.extend_from_slice(proxy.as_slice());
    hook.extend_from_slice(&SPOT_DEX.to_be_bytes());
    Ok(Bytes::from(hook))
}

/// Verify an observed hook against the exact proxy and spot destination.
pub fn validate_forward_hook(hook: &[u8], proxy: Address) -> Result<()> {
    if hook.len() != 56
        || &hook[..24] != HOOK_MAGIC
        || hook[24..28] != [0, 0, 0, 0]
        || hook[28..32] != [0, 0, 0, 24]
        || &hook[32..52] != proxy.as_slice()
        || hook[52..56] != SPOT_DEX.to_be_bytes()
    {
        return Err(hypercore_error(
            "CCTP HyperCore hook differs from the reviewed proxy and spot route.",
        ));
    }
    Ok(())
}

/// Validate the fixed HyperCore hook envelope when its exact proxy is not yet
/// available (for example while parsing an Iris response). Route proof must
/// additionally call `validate_forward_hook` with its persisted proxy.
pub fn validate_forward_hook_shape(hook: &[u8]) -> Result<()> {
    if hook.len() != 56
        || &hook[..24] != HOOK_MAGIC
        || hook[24..28] != [0, 0, 0, 0]
        || hook[28..32] != [0, 0, 0, 24]
        || hook[32..52].iter().all(|byte| *byte == 0)
        || hook[52..56] != SPOT_DEX.to_be_bytes()
    {
        return Err(hypercore_error(
            "CCTP HyperCore hook envelope differs from the reviewed forwarder protocol.",
        ));
    }
    Ok(())
}

/// Parse the exact EVM-bound Core withdrawal hook. Circle's forwarder uses a
/// 32-byte padded `cctp-forward` tag followed by a 28-byte owner/action-nonce
/// payload. The route verifier binds both payload fields to the stored plan.
pub fn validate_outbound_hook_shape(hook: &[u8]) -> Result<()> {
    if hook.len() != 60 || &hook[..32] != OUTBOUND_HOOK_MAGIC {
        return Err(hypercore_error(
            "HyperCore outbound CCTP hook differs from the reviewed forwarder envelope.",
        ));
    }
    Ok(())
}

/// Validate a Circle-attested Core withdrawal message against the exact Core
/// action owner, amount, fee quote, and action nonce. This route is limited to
/// Ethereum, Arbitrum and Solana destination domains; Solana receipt proof is
/// performed by the Solana CCTP verifier rather than EVM logs.
pub fn validate_outbound_message(
    raw: &str,
    metadata: &CctpQuoteMetadata,
    destination_domain: u32,
    owner: Address,
    amount: &Quantity,
    core_nonce: u64,
) -> Result<CctpMessage> {
    let raw_bytes = bytes(raw)?;
    check(
        raw_bytes.len() == MESSAGE_LEN + 60,
        "HyperCore outbound CCTP message must contain its exact forwarder hook.",
    )?;
    validate_outbound_hook_shape(&raw_bytes[MESSAGE_LEN..])?;
    let hook = &raw_bytes[MESSAGE_LEN..];
    check(
        &hook[32..52] == owner.as_slice()
            && u64::from_be_bytes(hook[52..60].try_into().expect("checked hook")) == core_nonce,
        "HyperCore outbound hook owner or Core action nonce differs from the persisted route.",
    )?;
    check(
        metadata.protocol_version == 2
            && metadata.source_domain == DOMAIN
            && metadata.destination_domain == destination_domain
            && metadata.min_finality_threshold == metadata.transfer_mode.threshold()
            && matches!(destination_domain, 0 | 3 | 5)
            && !amount.is_zero(),
        "HyperCore outbound CCTP metadata differs from its exact destination route.",
    )?;
    let message = decode_message(raw)?;
    check(
        message.source_domain == DOMAIN
            && message.destination_domain == destination_domain
            && message.sender.eq_ignore_ascii_case(TOKEN_MESSENGER)
            && message.recipient.eq_ignore_ascii_case(TOKEN_MESSENGER)
            && message.destination_caller == format!("{:#x}", Address::ZERO)
            && message.burn_token.eq_ignore_ascii_case(NATIVE_USDC)
            && message
                .mint_recipient
                .eq_ignore_ascii_case(&format!("{owner:#x}"))
            && message
                .message_sender
                .eq_ignore_ascii_case(CORE_DEPOSIT_WALLET)
            && message.amount == *amount
            && message.max_fee == metadata.max_fee
            && message.fee_executed <= message.max_fee
            && message.fee_executed < message.amount
            && message.min_finality_threshold == metadata.min_finality_threshold
            && message.finality_threshold_executed == metadata.min_finality_threshold
            && message.expiration_block.is_zero(),
        "Attested HyperCore outbound CCTP message differs from the exact approved route.",
    )?;
    Ok(message)
}

/// Validate the Core withdrawal CCTP message when its external destination is
/// Solana. The 60-byte Core forwarder hook still binds the enrolled EVM owner
/// and exact CoreWriter nonce; the message body additionally binds the exact
/// Solana CCTP program, destination caller and USDC ATA.
pub fn validate_outbound_solana_message(
    raw: &str,
    metadata: &CctpQuoteMetadata,
    destination: &SolanaDestination,
    hook_owner: Address,
    amount: &Quantity,
    core_nonce: u64,
) -> Result<CctpMessage> {
    solana::validate_destination(destination)?;
    let raw_bytes = bytes(raw)?;
    check(
        raw_bytes.len() == MESSAGE_LEN + 60,
        "HyperCore outbound Solana CCTP message must contain its exact forwarder hook.",
    )?;
    validate_outbound_hook_shape(&raw_bytes[MESSAGE_LEN..])?;
    let hook = &raw_bytes[MESSAGE_LEN..];
    check(
        &hook[32..52] == hook_owner.as_slice()
            && u64::from_be_bytes(hook[52..60].try_into().expect("checked hook")) == core_nonce,
        "HyperCore outbound hook owner or Core action nonce differs from the persisted route.",
    )?;
    check(
        metadata.protocol_version == 2
            && metadata.source_domain == DOMAIN
            && metadata.destination_domain == solana::DOMAIN
            && metadata.min_finality_threshold == metadata.transfer_mode.threshold()
            && !amount.is_zero(),
        "HyperCore outbound CCTP metadata differs from its exact Solana route.",
    )?;
    let message = decode_message(raw)?;
    check(
        message.source_domain == DOMAIN
            && message.destination_domain == solana::DOMAIN
            && message.sender.eq_ignore_ascii_case(TOKEN_MESSENGER)
            && message.recipient == solana::TOKEN_MESSENGER_MINTER
            && message.destination_caller == destination.destination_caller
            && message.burn_token.eq_ignore_ascii_case(NATIVE_USDC)
            && message.mint_recipient == destination.recipient_token_account
            && message
                .message_sender
                .eq_ignore_ascii_case(CORE_DEPOSIT_WALLET)
            && message.amount == *amount
            && message.max_fee == metadata.max_fee
            && message.fee_executed <= message.max_fee
            && message.fee_executed < message.amount
            && message.min_finality_threshold == metadata.min_finality_threshold
            && message.finality_threshold_executed == metadata.min_finality_threshold
            && message.expiration_block.is_zero(),
        "Attested HyperCore outbound Solana CCTP message differs from the exact approved route.",
    )?;
    Ok(message)
}

/// Build an EVM source burn into the reviewed HyperCore forwarder route.
/// `intent` must identify native USDC on Ethereum, Arbitrum or HyperEVM's
/// supported EVM source route and the enrolled EVM source wallet.
pub fn burn_call(
    source_chain_id: u64,
    amount: &Quantity,
    metadata: &CctpQuoteMetadata,
    proxy: Address,
) -> Result<EvmCall> {
    // This native Core deposit route intentionally supports Ethereum,
    // Arbitrum and HyperEVM EVM sources. Solana source burns have a separate
    // Solana instruction path and must never be coerced through this builder.
    check(
        matches!(source_chain_id, 1 | 42161),
        "HyperCore CCTP EVM deposits are reviewed only from Ethereum or Arbitrum One.",
    )?;
    let source_asset = usdc_asset(source_chain_id)?;
    ensure(
        proxy != Address::ZERO
            && metadata.protocol_version == 2
            && metadata.source_domain == domain(source_chain_id)?
            && metadata.destination_domain == DOMAIN
            && metadata.min_finality_threshold == metadata.transfer_mode.threshold()
            && metadata.max_fee < *amount
            && !amount.is_zero(),
        "CCTP_VALIDATION_FAILED",
        "bridge",
        "CCTP HyperCore metadata differs from the exact native-USDC route or fee bound.",
    )?;
    let hook = forward_hook(proxy)?;
    Ok(EvmCall {
        chain_id: source_chain_id,
        to: TOKEN_MESSENGER.into(),
        value: Quantity::from_u64(0),
        data: hex_bytes(
            &HyperCoreMessenger::depositForBurnWithHookCall {
                amount: u256(amount)?,
                destinationDomain: DOMAIN,
                mintRecipient: address(FORWARDER)?.into_word(),
                burnToken: address(
                    source_asset
                        .token
                        .as_deref()
                        .ok_or_else(|| hypercore_error("Missing native USDC source token."))?,
                )?,
                destinationCaller: address(FORWARDER)?.into_word(),
                maxFee: u256(&metadata.max_fee)?,
                minFinalityThreshold: metadata.min_finality_threshold,
                hookData: hook,
            }
            .abi_encode(),
        ),
    })
}

/// Require a decoded CCTP message's fixed destination fields to match the
/// approved HyperCore transfer. Call this together with exact event/attestation
/// verification and the proxy's independently observed credit.
pub fn validate_message(
    raw_message: &str,
    metadata: &CctpQuoteMetadata,
    source_chain_id: u64,
    source_owner: Address,
    amount: &Quantity,
    proxy: Address,
) -> Result<()> {
    let raw = bytes(raw_message)?;
    check(
        raw.len() == MESSAGE_LEN + 56,
        "CCTP HyperCore message must carry exactly the reviewed 56-byte hook.",
    )?;
    let hook = &raw[MESSAGE_LEN..];
    validate_forward_hook(hook, proxy)?;
    let message = decode_message(&hex_bytes(&raw[..MESSAGE_LEN]))?;
    let expected = format!("{:#x}", address(FORWARDER)?);
    let source_messenger = TOKEN_MESSENGER;
    let source_token = usdc_asset(source_chain_id)?
        .token
        .ok_or_else(|| hypercore_error("Missing native USDC identity for source network."))?;
    let gross = u256(amount)?;
    let actual_fee = u256(&message.fee_executed)?;
    ensure(
        metadata.protocol_version == 2
            && matches!(source_chain_id, 1 | 42161)
            && metadata.source_domain == domain(source_chain_id)?
            && metadata.destination_domain == DOMAIN
            && message.source_domain == metadata.source_domain
            && message.destination_domain == DOMAIN
            && message.amount == *amount
            && message.sender.eq_ignore_ascii_case(source_messenger)
            && message
                .message_sender
                .eq_ignore_ascii_case(&format!("{source_owner:#x}"))
            && message.recipient.eq_ignore_ascii_case(TOKEN_MESSENGER)
            && message.destination_caller.eq_ignore_ascii_case(&expected)
            && message.mint_recipient.eq_ignore_ascii_case(&expected)
            && message.burn_token.eq_ignore_ascii_case(&source_token)
            && message.max_fee == metadata.max_fee
            && actual_fee <= u256(&metadata.max_fee)?
            && gross > actual_fee
            && message.min_finality_threshold == metadata.min_finality_threshold
            && message.finality_threshold_executed == metadata.min_finality_threshold,
        "CCTP_VALIDATION_FAILED",
        "bridge",
        "CCTP message destination domain, forwarder, caller or fee differs from the approved route.",
    )?;
    Ok(())
}

/// Verify the canonical source-side CCTP events for a HyperCore-targeted
/// `depositForBurnWithHook`. This binds the full emitted message (including
/// the fixed hook) to the paired DepositForBurn event; callers must also bind
/// these logs to the exact finalized transaction and enrolled source signer.
pub fn verify_source_logs(
    logs: &[CanonicalRawLog],
    metadata: &CctpQuoteMetadata,
    source_chain_id: u64,
    source_owner: Address,
    amount: &Quantity,
    proxy: Address,
) -> Result<CctpSourceMessage> {
    let message_log = unique_log::<Transmitter::MessageSent>(logs, MESSAGE_TRANSMITTER)?;
    let message_event = event::<Transmitter::MessageSent>(message_log)?;
    let raw_message = hex_bytes(&message_event.message);
    let body = hex_bytes(&message_event.message[..MESSAGE_LEN]);
    let decoded = decode_message(&body)?;
    let expected_token = usdc_asset(source_chain_id)?
        .token
        .ok_or_else(|| hypercore_error("Missing native USDC source token."))?;
    ensure(
        metadata.protocol_version == 2
            && matches!(source_chain_id, 1 | 42161)
            && metadata.source_domain == domain(source_chain_id)?
            && metadata.destination_domain == DOMAIN
            && metadata.min_finality_threshold == metadata.transfer_mode.threshold()
            && metadata.max_fee < *amount
            && decoded.source_domain == metadata.source_domain
            && decoded.destination_domain == DOMAIN
            && decoded.sender.eq_ignore_ascii_case(TOKEN_MESSENGER)
            && decoded.recipient.eq_ignore_ascii_case(TOKEN_MESSENGER)
            && decoded.destination_caller.eq_ignore_ascii_case(FORWARDER)
            && decoded.min_finality_threshold == metadata.min_finality_threshold
            && decoded.finality_threshold_executed == 0
            && decoded.burn_token.eq_ignore_ascii_case(&expected_token)
            && decoded.mint_recipient.eq_ignore_ascii_case(FORWARDER)
            && decoded.amount == *amount
            && decoded
                .message_sender
                .eq_ignore_ascii_case(&format!("{source_owner:#x}"))
            && decoded.max_fee == metadata.max_fee
            && decoded.fee_executed == Quantity::from_u64(0)
            && decoded.expiration_block == Quantity::from_u64(0),
        "CCTP_VALIDATION_FAILED",
        "bridge",
        "Emitted source message differs from the exact HyperCore-forward CCTP route.",
    )?;
    let burn_log = unique_log::<Messenger::DepositForBurn>(logs, TOKEN_MESSENGER)?;
    let burn = event::<Messenger::DepositForBurn>(burn_log)?;
    let expected_hook = forward_hook(proxy)?;
    let source_token = address(
        usdc_asset(source_chain_id)?
            .token
            .as_deref()
            .ok_or_else(|| hypercore_error("Missing native USDC source token."))?,
    )?;
    ensure(
        burn.burnToken == source_token
            && burn.amount == u256(amount)?
            && burn.depositor == source_owner
            && burn.mintRecipient == address(FORWARDER)?.into_word()
            && burn.destinationDomain == DOMAIN
            && burn.destinationTokenMessenger == address(TOKEN_MESSENGER)?.into_word()
            && burn.destinationCaller == address(FORWARDER)?.into_word()
            && burn.maxFee == u256(&metadata.max_fee)?
            && burn.minFinalityThreshold == metadata.min_finality_threshold
            && burn.hookData.as_ref() == expected_hook.as_ref()
            && burn_log.log_index > message_log.log_index
            && decoded
                .message_sender
                .eq_ignore_ascii_case(&format!("{source_owner:#x}")),
        "CCTP_VALIDATION_FAILED",
        "bridge",
        "DepositForBurn event differs from the exact emitted HyperCore-forward message.",
    )?;
    Ok(CctpSourceMessage {
        message_hash: format!("{:#x}", keccak256(bytes(&raw_message)?)),
        immutable_message_hash: immutable_hook_message_hash(&raw_message)?,
        raw_message,
        log_index: message_log.log_index,
    })
}

/// Bind an Iris-populated HyperCore hook message back to its exact source
/// burn. Only Circle's mutable nonce/finality/fee/expiration fields may
/// differ; the 56-byte forward hook and every recipient/source field remain
/// byte-for-byte fixed.
pub fn validate_attested_message(
    source_raw: &str,
    metadata: &CctpQuoteMetadata,
    source_chain_id: u64,
    source_owner: Address,
    amount: &Quantity,
    proxy: Address,
    attested_raw: &str,
) -> Result<CctpMessage> {
    let source = bytes(source_raw)?;
    let attested = bytes(attested_raw)?;
    check(
        source.len() == MESSAGE_LEN + 56 && attested.len() == MESSAGE_LEN + 56,
        "HyperCore CCTP message must carry exactly the reviewed hook.",
    )?;
    check(
        source[MESSAGE_LEN..] == attested[MESSAGE_LEN..]
            && source[MESSAGE_LEN..] == forward_hook(proxy)?.as_ref()[..],
        "Attestation changed the fixed HyperCore destination hook.",
    )?;
    let source_body = hex_bytes(&source[..MESSAGE_LEN]);
    let attested_body = hex_bytes(&attested[..MESSAGE_LEN]);
    let source_message = decode_message(&source_body)?;
    let result = decode_message(&attested_body)?;
    check(
        immutable_message_hash(&source_body)? == immutable_message_hash(&attested_body)?,
        "Attestation changed immutable HyperCore source-burn fields.",
    )?;
    check(
        hash(&source_message.nonce)? == B256::ZERO
            && source_message.finality_threshold_executed == 0
            && source_message.fee_executed == Quantity::from_u64(0)
            && source_message.expiration_block == Quantity::from_u64(0),
        "HyperCore source message already contains attester-populated fields.",
    )?;
    validate_message(
        attested_raw,
        metadata,
        source_chain_id,
        source_owner,
        amount,
        proxy,
    )?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_log<E: SolEvent>(emitter: &str, index: u32, event: E) -> CanonicalRawLog {
        CanonicalRawLog {
            log_index: index,
            address: emitter.into(),
            topics: event
                .encode_topics()
                .iter()
                .map(|topic| format!("{:#x}", topic.0))
                .collect(),
            data: hex_bytes(&event.encode_data()),
        }
    }

    fn source_logs(proxy: Address, owner: Address, amount: Quantity) -> Vec<CanonicalRawLog> {
        let metadata = CctpQuoteMetadata {
            protocol_version: 2,
            source_domain: 3,
            destination_domain: DOMAIN,
            transfer_mode: CctpTransferMode::Standard,
            max_fee: Quantity::from_u64(10),
            min_finality_threshold: 2000,
        };
        let token = address(usdc_asset(42161).unwrap().token.as_deref().unwrap()).unwrap();
        let hook = forward_hook(proxy).unwrap();
        let mut message = Vec::with_capacity(MESSAGE_LEN + hook.len());
        for value in [1u32, 3, DOMAIN] {
            message.extend_from_slice(&value.to_be_bytes());
        }
        message.extend_from_slice(&[0; 32]);
        for who in [
            address(TOKEN_MESSENGER).unwrap(),
            address(TOKEN_MESSENGER).unwrap(),
            address(FORWARDER).unwrap(),
        ] {
            message.extend_from_slice(&who.into_word().0);
        }
        message.extend_from_slice(&2000u32.to_be_bytes());
        message.extend_from_slice(&0u32.to_be_bytes());
        message.extend_from_slice(&1u32.to_be_bytes());
        message.extend_from_slice(&token.into_word().0);
        message.extend_from_slice(&address(FORWARDER).unwrap().into_word().0);
        message.extend_from_slice(&u256(&amount).unwrap().to_be_bytes::<32>());
        message.extend_from_slice(&owner.into_word().0);
        message.extend_from_slice(&u256(&metadata.max_fee).unwrap().to_be_bytes::<32>());
        message.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
        message.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
        message.extend_from_slice(&hook);
        vec![
            canonical_log(
                MESSAGE_TRANSMITTER,
                3,
                Transmitter::MessageSent {
                    message: Bytes::from(message),
                },
            ),
            canonical_log(
                TOKEN_MESSENGER,
                4,
                Messenger::DepositForBurn {
                    burnToken: token,
                    amount: u256(&amount).unwrap(),
                    depositor: owner,
                    mintRecipient: address(FORWARDER).unwrap().into_word(),
                    destinationDomain: DOMAIN,
                    destinationTokenMessenger: address(TOKEN_MESSENGER).unwrap().into_word(),
                    destinationCaller: address(FORWARDER).unwrap().into_word(),
                    maxFee: u256(&metadata.max_fee).unwrap(),
                    minFinalityThreshold: metadata.min_finality_threshold,
                    hookData: hook,
                },
            ),
        ]
    }

    fn outbound_message(owner: Address, amount: u64, fee: u64, nonce: u64) -> String {
        let mut message = Vec::with_capacity(MESSAGE_LEN + 60);
        for value in [1u32, DOMAIN, 3] {
            message.extend_from_slice(&value.to_be_bytes());
        }
        message.extend_from_slice(&[0x11; 32]); // Circle message nonce
        for who in [
            address(TOKEN_MESSENGER).unwrap(),
            address(TOKEN_MESSENGER).unwrap(),
            Address::ZERO,
        ] {
            message.extend_from_slice(&who.into_word().0);
        }
        message.extend_from_slice(&2000u32.to_be_bytes());
        message.extend_from_slice(&2000u32.to_be_bytes());
        message.extend_from_slice(&1u32.to_be_bytes());
        message.extend_from_slice(&address(NATIVE_USDC).unwrap().into_word().0);
        message.extend_from_slice(&owner.into_word().0);
        message.extend_from_slice(&U256::from(amount).to_be_bytes::<32>());
        message.extend_from_slice(&address(CORE_DEPOSIT_WALLET).unwrap().into_word().0);
        message.extend_from_slice(&U256::from(fee).to_be_bytes::<32>());
        message.extend_from_slice(&U256::from(fee).to_be_bytes::<32>());
        message.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
        message.extend_from_slice(OUTBOUND_HOOK_MAGIC);
        message.extend_from_slice(owner.as_slice());
        message.extend_from_slice(&nonce.to_be_bytes());
        hex_bytes(&message)
    }

    #[test]
    fn outbound_solana_message_binds_exact_ata_and_core_nonce() {
        let owner: Address = "0x5502f38ed8ad519c3b9635ba723dddb94e991ecd"
            .parse()
            .unwrap();
        let wallet = solana::public_key(&[7; 32]).unwrap();
        let ata = gofin_solana::associated_token_address(
            &wallet,
            solana::USDC_MINT,
            gofin_solana::token::TOKEN_PROGRAM_ID,
        )
        .unwrap();
        let destination = SolanaDestination {
            network: gofin_solana::NETWORK.into(),
            wallet: None,
            wallet_resource: ResourceId(format!("{}:{wallet}", gofin_solana::NETWORK)),
            wallet_address: wallet.clone(),
            recipient: wallet.clone(),
            asset: solana::asset(),
            recipient_token_account: ata.clone(),
            destination_caller: wallet,
        };
        let amount = 319_820_698u64;
        let fee = 200_000u64;
        let core_nonce = 1_792_085_987_467u64;
        let mut message = Vec::with_capacity(MESSAGE_LEN + 60);
        for value in [1u32, DOMAIN, solana::DOMAIN] {
            message.extend_from_slice(&value.to_be_bytes());
        }
        message.extend_from_slice(&[0x11; 32]);
        for raw in [
            address(TOKEN_MESSENGER).unwrap().into_word().0,
            solana::pubkey_bytes(solana::TOKEN_MESSENGER_MINTER).unwrap(),
            solana::pubkey_bytes(&destination.destination_caller).unwrap(),
        ] {
            message.extend_from_slice(&raw);
        }
        message.extend_from_slice(&2000u32.to_be_bytes());
        message.extend_from_slice(&2000u32.to_be_bytes());
        message.extend_from_slice(&1u32.to_be_bytes());
        message.extend_from_slice(&address(NATIVE_USDC).unwrap().into_word().0);
        message.extend_from_slice(&solana::pubkey_bytes(&ata).unwrap());
        message.extend_from_slice(&U256::from(amount).to_be_bytes::<32>());
        message.extend_from_slice(&address(CORE_DEPOSIT_WALLET).unwrap().into_word().0);
        message.extend_from_slice(&U256::from(fee).to_be_bytes::<32>());
        message.extend_from_slice(&U256::from(fee).to_be_bytes::<32>());
        message.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
        message.extend_from_slice(OUTBOUND_HOOK_MAGIC);
        message.extend_from_slice(owner.as_slice());
        message.extend_from_slice(&core_nonce.to_be_bytes());
        let raw = hex_bytes(&message);
        let metadata = CctpQuoteMetadata {
            protocol_version: 2,
            source_domain: DOMAIN,
            destination_domain: solana::DOMAIN,
            transfer_mode: CctpTransferMode::Standard,
            max_fee: Quantity::from_u64(fee),
            min_finality_threshold: 2000,
        };
        let parsed = validate_outbound_solana_message(
            &raw,
            &metadata,
            &destination,
            owner,
            &Quantity::from_u64(amount),
            core_nonce,
        )
        .unwrap();
        assert_eq!(parsed.mint_recipient, ata);
        assert_eq!(parsed.destination_domain, solana::DOMAIN);
        assert!(
            validate_outbound_solana_message(
                &raw,
                &metadata,
                &destination,
                owner,
                &Quantity::from_u64(amount),
                core_nonce + 1,
            )
            .is_err()
        );
    }

    #[test]
    fn outbound_attestation_binds_domain_owner_amount_fee_and_core_nonce() {
        let owner: Address = "0x5502f38ed8ad519c3b9635ba723dddb94e991ecd"
            .parse()
            .unwrap();
        let metadata = CctpQuoteMetadata {
            protocol_version: 2,
            source_domain: DOMAIN,
            destination_domain: 3,
            transfer_mode: CctpTransferMode::Standard,
            max_fee: Quantity::from_u64(200_000),
            min_finality_threshold: 2000,
        };
        let raw = outbound_message(owner, 319_820_698, 200_000, 1_792_085_987_467);
        let parsed = validate_outbound_message(
            &raw,
            &metadata,
            3,
            owner,
            &Quantity::from_u64(319_820_698),
            1_792_085_987_467,
        )
        .unwrap();
        assert_eq!(parsed.source_domain, DOMAIN);
        assert_eq!(parsed.destination_domain, 3);
        assert_eq!(parsed.fee_executed, Quantity::from_u64(200_000));
        assert!(
            validate_outbound_message(
                &raw,
                &metadata,
                3,
                owner,
                &Quantity::from_u64(319_820_698),
                1_792_085_987_468,
            )
            .is_err()
        );
        assert!(
            validate_outbound_message(
                &raw,
                &metadata,
                3,
                Address::ZERO,
                &Quantity::from_u64(319_820_698),
                1_792_085_987_467,
            )
            .is_err()
        );
    }

    #[test]
    fn forward_hook_binds_proxy_and_spot_only() {
        let proxy: Address = "0x1234567890123456789012345678901234567890"
            .parse()
            .unwrap();
        let hook = forward_hook(proxy).unwrap();
        assert_eq!(hook.len(), 56);
        validate_forward_hook(&hook, proxy).unwrap();
        assert!(validate_forward_hook(&hook, Address::ZERO).is_err());
        let mut wrong_id = hook.to_vec();
        wrong_id[55] = 0;
        assert!(validate_forward_hook(&wrong_id, proxy).is_err());
        let mut trailing = hook.to_vec();
        trailing.push(1);
        assert!(validate_forward_hook(&trailing, proxy).is_err());
    }

    #[test]
    fn message_proof_binds_domain_caller_recipient_amount_and_fees() {
        let proxy: Address = "0x1234567890123456789012345678901234567890"
            .parse()
            .unwrap();
        let metadata = CctpQuoteMetadata {
            protocol_version: 2,
            source_domain: 3,
            destination_domain: DOMAIN,
            transfer_mode: CctpTransferMode::Standard,
            max_fee: Quantity::from_u64(10),
            min_finality_threshold: 2000,
        };
        let amount = Quantity::from_u64(1_000_000);
        let hook = forward_hook(proxy).unwrap();
        let source_owner: Address = "0x9999999999999999999999999999999999999999"
            .parse()
            .unwrap();
        let source_token = address(usdc_asset(42161).unwrap().token.as_deref().unwrap()).unwrap();
        let mut raw = Vec::new();
        for value in [1u32, 3, DOMAIN] {
            raw.extend_from_slice(&value.to_be_bytes());
        }
        raw.extend_from_slice(&[1; 32]);
        for who in [
            address(TOKEN_MESSENGER).unwrap(),
            address(TOKEN_MESSENGER).unwrap(),
            address(FORWARDER).unwrap(),
        ] {
            raw.extend_from_slice(&who.into_word().0);
        }
        raw.extend_from_slice(&2000u32.to_be_bytes());
        raw.extend_from_slice(&2000u32.to_be_bytes());
        raw.extend_from_slice(&1u32.to_be_bytes());
        raw.extend_from_slice(&source_token.into_word().0);
        raw.extend_from_slice(&address(FORWARDER).unwrap().into_word().0);
        raw.extend_from_slice(&u256(&amount).unwrap().to_be_bytes::<32>());
        raw.extend_from_slice(&source_owner.into_word().0);
        raw.extend_from_slice(&u256(&metadata.max_fee).unwrap().to_be_bytes::<32>());
        raw.extend_from_slice(&u256(&Quantity::from_u64(7)).unwrap().to_be_bytes::<32>());
        raw.extend_from_slice(&u256(&Quantity::from_u64(999)).unwrap().to_be_bytes::<32>());
        raw.extend_from_slice(&hook);
        let encoded = hex_bytes(&raw);
        validate_message(&encoded, &metadata, 42161, source_owner, &amount, proxy).unwrap();

        for (offset, value) in [(11, 0), (108, 1), (184, 1), (312, 1), (248, 1)] {
            let mut wrong = raw.clone();
            wrong[offset] = value;
            assert!(
                validate_message(
                    &hex_bytes(&wrong),
                    &metadata,
                    42161,
                    source_owner,
                    &amount,
                    proxy
                )
                .is_err(),
                "message byte {offset}"
            );
        }
        assert!(validate_message(&encoded, &metadata, 8453, source_owner, &amount, proxy).is_err());
    }

    #[test]
    fn burn_targets_fixed_forwarder_and_binds_hook() {
        let proxy: Address = "0x1234567890123456789012345678901234567890"
            .parse()
            .unwrap();
        let metadata = CctpQuoteMetadata {
            protocol_version: 2,
            source_domain: 3,
            destination_domain: DOMAIN,
            transfer_mode: CctpTransferMode::Fast,
            max_fee: Quantity::from_u64(10),
            min_finality_threshold: 1000,
        };
        let amount = Quantity::from_u64(1_000_000);
        let call = burn_call(42161, &amount, &metadata, proxy).unwrap();
        assert_eq!(call.to, TOKEN_MESSENGER);
        assert_eq!(call.chain_id, 42161);
        let raw = gofin_evm::rpc_bytes(&serde_json::json!(call.data)).unwrap();
        let decoded =
            HyperCoreMessenger::depositForBurnWithHookCall::abi_decode_validate(&raw).unwrap();
        assert_eq!(decoded.destinationDomain, DOMAIN);
        assert_eq!(
            decoded.mintRecipient,
            address(FORWARDER).unwrap().into_word()
        );
        assert_eq!(
            decoded.destinationCaller,
            address(FORWARDER).unwrap().into_word()
        );
        assert_eq!(
            decoded.burnToken,
            address(usdc_asset(42161).unwrap().token.as_deref().unwrap()).unwrap()
        );
        assert_eq!(decoded.hookData, forward_hook(proxy).unwrap());
    }

    #[test]
    fn source_log_proof_binds_burn_message_owner_and_exact_hook() {
        let proxy: Address = "0x1234567890123456789012345678901234567890"
            .parse()
            .unwrap();
        let owner: Address = "0x9999999999999999999999999999999999999999"
            .parse()
            .unwrap();
        let amount = Quantity::from_u64(1_000_000);
        let metadata = CctpQuoteMetadata {
            protocol_version: 2,
            source_domain: 3,
            destination_domain: DOMAIN,
            transfer_mode: CctpTransferMode::Standard,
            max_fee: Quantity::from_u64(10),
            min_finality_threshold: 2000,
        };
        let logs = source_logs(proxy, owner, amount.clone());
        let proof = verify_source_logs(&logs, &metadata, 42161, owner, &amount, proxy).unwrap();
        assert_eq!(proof.log_index, 3);
        assert!(!proof.message_hash.is_empty());
        let sent = event::<Transmitter::MessageSent>(&logs[0]).unwrap();
        let source = hex_bytes(&sent.message);
        let mut attested = bytes(&source).unwrap();
        attested[12..44].fill(2);
        attested[144..148].copy_from_slice(&2000u32.to_be_bytes());
        attested[312..344]
            .copy_from_slice(&u256(&Quantity::from_u64(5)).unwrap().to_be_bytes::<32>());
        attested[344..376]
            .copy_from_slice(&u256(&Quantity::from_u64(500)).unwrap().to_be_bytes::<32>());
        let attested = hex_bytes(&attested);
        let decoded =
            validate_attested_message(&source, &metadata, 42161, owner, &amount, proxy, &attested)
                .unwrap();
        assert_eq!(decoded.fee_executed, Quantity::from_u64(5));

        let mut wrong_attested = bytes(&attested).unwrap();
        let last = wrong_attested.len() - 1;
        wrong_attested[last] = 0;
        assert!(
            validate_attested_message(
                &source,
                &metadata,
                42161,
                owner,
                &amount,
                proxy,
                &hex_bytes(&wrong_attested),
            )
            .is_err()
        );

        let mut wrong = logs;
        let mut burn: Messenger::DepositForBurn = event(&wrong[1]).unwrap();
        let wrong_proxy: Address = "0x1234567890123456789012345678901234567891"
            .parse()
            .unwrap();
        burn.hookData = forward_hook(wrong_proxy).unwrap();
        wrong[1] = canonical_log(TOKEN_MESSENGER, 4, burn);
        assert!(verify_source_logs(&wrong, &metadata, 42161, owner, &amount, proxy).is_err());
    }
}
