//! Direct CCTP V2 destination support, pinned to Circle's deployed Solana V2
//! interfaces (b37d577fc1dc317ce9bc0316c5063afe38744ce3).
use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use gofin_solana::{
    FinalizedReceiptProof, SolanaRuntime,
    token::TOKEN_PROGRAM_ID,
    transaction::{NormalizedAccountMeta, NormalizedInstruction},
};
use sha2::{Digest, Sha256};
use solana_pubkey3::Pubkey;

pub const MESSAGE_TRANSMITTER: &str = "CCTPV2Sm4AdWt5296sk4P66VBZ7bEhcARwFaaS9YPbeC";
pub const TOKEN_MESSENGER_MINTER: &str = "CCTPV2vPZJS2u2BBsUoscuikbYjnpFmbFsvVuJdgUMQe";
pub const USDC_MINT: &str = gofin_solana::funding::MAINNET_USDC_MINT;
pub const DOMAIN: u32 = 5;
const SYSTEM: &str = "11111111111111111111111111111111";
const LOADER: &str = "BPFLoaderUpgradeab1e11111111111111111111111";
const EVENT_CPI: [u8; 8] = [228, 69, 165, 46, 81, 203, 154, 29];

/// Circle's Solana V2 source path for a native-USDC burn to HyperCore.
/// The on-chain destination domain is HyperEVM (19); the fixed CctpForwarder
/// then routes the mint into HyperCore spot using the reviewed hook.
pub mod hypercore_source {
    use super::*;
    use crate::cctp::CctpQuoteMetadata;

    pub const DESTINATION_DOMAIN: u32 = super::super::hypercore::DOMAIN;
    pub const FORWARDER: &str = super::super::hypercore::FORWARDER;

    /// The Solana source burn writes Circle's `MessageSent` account instead
    /// of emitting the EVM `MessageSent` log. Keep its raw source message
    /// separately because source-domain Pubkeys are not EVM address words.
    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct SourceMessageProof {
        pub raw_message: String,
        pub message_hash: String,
        pub immutable_message_hash: String,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct BurnRequest {
        pub owner: String,
        pub message_sent_event_data: String,
        pub amount_atoms: u64,
        pub max_fee_atoms: u64,
        pub min_finality_threshold: u32,
        pub hook_data: String,
        /// Exact finalized rent-exemption quote for the 484-byte MessageSent
        /// account. This is local funding evidence, not encoded into Circle's
        /// burn instruction or CCTP message.
        #[serde(default)]
        pub message_sent_event_rent_lamports: u64,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct BurnProof {
        pub signature: String,
        pub slot: u64,
        pub owner: String,
        pub amount_atoms: u64,
        pub source_usdc_debit_atoms: u64,
        pub network_fee_lamports: u64,
        pub destination_domain: u32,
        pub hook_data: String,
    }

    pub(super) fn source_pda(program: &str, seeds: &[&[u8]]) -> Result<String> {
        pda(program, seeds)
    }

    pub(super) fn evm_word_as_pubkey(value: &str) -> Result<[u8; 32]> {
        Ok(address(value)?.into_word().into())
    }

    fn borsh_bytes(output: &mut Vec<u8>, value: &[u8]) -> Result<()> {
        let len = u32::try_from(value.len()).map_err(|_| invalid("CCTP hook data is too long."))?;
        output.extend_from_slice(&len.to_le_bytes());
        output.extend_from_slice(value);
        Ok(())
    }

    fn source_ix_data(
        request: &BurnRequest,
        metadata: &CctpQuoteMetadata,
        proxy: Address,
    ) -> Result<Vec<u8>> {
        check(
            request.amount_atoms > 0,
            "CCTP Solana burn amount must be positive.",
        )?;
        check(
            request.max_fee_atoms < request.amount_atoms,
            "CCTP Solana maximum fee must be below the burn amount.",
        )?;
        check(
            metadata.protocol_version == 2
                && metadata.source_domain == DOMAIN
                && metadata.destination_domain == DESTINATION_DOMAIN
                && metadata.max_fee == Quantity::from_u64(request.max_fee_atoms)
                && metadata.min_finality_threshold == request.min_finality_threshold
                && request.min_finality_threshold == metadata.transfer_mode.threshold(),
            "CCTP Solana source metadata differs from the reviewed HyperCore route.",
        )?;
        let hook = super::super::hypercore::forward_hook(proxy)?;
        check(
            request.hook_data == hex_bytes(&hook),
            "CCTP Solana source hook differs from the exact HyperCore proxy route.",
        )?;
        let mut data = discriminator("global", "deposit_for_burn_with_hook").to_vec();
        data.extend_from_slice(&request.amount_atoms.to_le_bytes());
        data.extend_from_slice(&DESTINATION_DOMAIN.to_le_bytes());
        data.extend_from_slice(&evm_word_as_pubkey(FORWARDER)?);
        data.extend_from_slice(&evm_word_as_pubkey(FORWARDER)?); // destinationCaller
        data.extend_from_slice(&request.max_fee_atoms.to_le_bytes());
        data.extend_from_slice(&request.min_finality_threshold.to_le_bytes());
        borsh_bytes(&mut data, &hook)?;
        Ok(data)
    }

    /// Build Circle's exact `deposit_for_burn_with_hook` instruction. The
    /// owner pays event-account rent, as in Circle's official deposit flow;
    /// the message event-data key is fresh per signed transaction.
    pub fn burn_instruction(
        request: &BurnRequest,
        metadata: &CctpQuoteMetadata,
        proxy: Address,
    ) -> Result<NormalizedInstruction> {
        let owner_key = pubkey_bytes(&request.owner)?;
        let owner = request.owner.as_str();
        let message_event = request.message_sent_event_data.as_str();
        pubkey_bytes(message_event)?;
        let local = source_pda(
            TOKEN_MESSENGER_MINTER,
            &[b"local_token", &pubkey_bytes(USDC_MINT)?],
        )?;
        let denylist = source_pda(TOKEN_MESSENGER_MINTER, &[b"denylist_account", &owner_key])?;
        let transmitter = source_pda(MESSAGE_TRANSMITTER, &[b"message_transmitter"])?;
        let messenger = source_pda(TOKEN_MESSENGER_MINTER, &[b"token_messenger"])?;
        let sender_authority = source_pda(TOKEN_MESSENGER_MINTER, &[b"sender_authority"])?;
        let messenger_event = source_pda(TOKEN_MESSENGER_MINTER, &[b"__event_authority"])?;
        let remote = source_pda(TOKEN_MESSENGER_MINTER, &[b"remote_token_messenger", b"19"])?;
        let minter = source_pda(TOKEN_MESSENGER_MINTER, &[b"token_minter"])?;
        let burn_account =
            gofin_solana::associated_token_address(owner, USDC_MINT, TOKEN_PROGRAM_ID)?;
        let accounts = vec![
            meta(owner, true, true),
            meta(owner, true, true),
            meta(sender_authority, false, false),
            meta(burn_account, false, true),
            meta(denylist, false, false),
            meta(transmitter, false, true),
            meta(messenger, false, false),
            meta(remote, false, false),
            meta(minter, false, false),
            meta(local, false, true),
            meta(USDC_MINT, false, true),
            meta(message_event, true, true),
            meta(MESSAGE_TRANSMITTER, false, false),
            meta(TOKEN_MESSENGER_MINTER, false, false),
            meta(TOKEN_PROGRAM_ID, false, false),
            meta(SYSTEM, false, false),
            meta(messenger_event, false, false),
            meta(TOKEN_MESSENGER_MINTER, false, false),
        ];
        Ok(NormalizedInstruction {
            program_id: TOKEN_MESSENGER_MINTER.into(),
            accounts,
            data_base64: STANDARD.encode(source_ix_data(request, metadata, proxy)?),
        })
    }

    /// Parse and validate a prepared burn instruction before signing. This
    /// prevents caller substitution of the recipient, caller, amount, fee or
    /// hook between preparation and dispatch.
    pub fn validate_burn_instruction(
        instruction: &NormalizedInstruction,
        request: &BurnRequest,
        metadata: &CctpQuoteMetadata,
        proxy: Address,
    ) -> Result<()> {
        let expected = burn_instruction(request, metadata, proxy)?;
        check(
            instruction == &expected,
            "Prepared Solana CCTP burn differs from its exact HyperCore funding request.",
        )
    }

    /// Prove the finalized source burn from the exact prepared instruction and
    /// the owner's native-USDC balance delta. The broker binds `signature`
    /// durably before dispatch and must never replay a burn after an uncertain
    /// result.
    pub fn verify_burn_receipt(
        proof: &FinalizedReceiptProof,
        request: &BurnRequest,
        metadata: &CctpQuoteMetadata,
        proxy: Address,
    ) -> Result<BurnProof> {
        let expected = burn_instruction(request, metadata, proxy)?;
        let cctp_instructions: Vec<_> = proof
            .message_instructions
            .iter()
            .filter(|instruction| instruction.program_id == TOKEN_MESSENGER_MINTER)
            .collect();
        check(
            !proof.failed
                && proof.error.is_none()
                && proof.slot > 0
                && !proof.signature.is_empty()
                && proof
                    .account_keys
                    .first()
                    .is_some_and(|key| key == &request.owner)
                && !proof.required_signatures.is_empty()
                && proof.message_instructions.len() == 1
                && cctp_instructions.len() == 1
                && cctp_instructions[0] == &expected,
            "Finalized Solana CCTP receipt does not bind the exact approved HyperCore burn.",
        )?;
        let debit = proof.token_delta_verified(&request.owner, USDC_MINT, TOKEN_PROGRAM_ID, 6)?;
        check(
            debit
                == gofin_domain::SignedQuantity::negative(&Quantity::from_u64(
                    request.amount_atoms,
                )),
            "Finalized Solana CCTP receipt has no exact owner native-USDC burn debit.",
        )?;
        Ok(BurnProof {
            signature: proof.signature.clone(),
            slot: proof.slot,
            owner: request.owner.clone(),
            amount_atoms: request.amount_atoms,
            source_usdc_debit_atoms: request.amount_atoms,
            network_fee_lamports: proof.fee_lamports,
            destination_domain: DESTINATION_DOMAIN,
            hook_data: request.hook_data.clone(),
        })
    }

    /// Validate the exact raw Circle message persisted to the MessageSent
    /// account created by the approved finalized burn. Solana source Pubkeys
    /// occupy full 32-byte fields, unlike the EVM word decoder.
    pub fn verify_source_message(
        raw_message: &str,
        request: &BurnRequest,
        metadata: &CctpQuoteMetadata,
        proxy: Address,
    ) -> Result<SourceMessageProof> {
        let raw = bytes(raw_message)?;
        check(
            raw.len() == MESSAGE_LEN + 56
                && raw[4..8] == DOMAIN.to_be_bytes()
                && raw[8..12] == DESTINATION_DOMAIN.to_be_bytes()
                && raw[140..144] == request.min_finality_threshold.to_be_bytes()
                && raw[144..148] == 0u32.to_be_bytes()
                && raw[148..152] == 1u32.to_be_bytes(),
            "Solana CCTP source message has an unexpected domain, body version, or finality.",
        )?;
        let owner = pubkey_bytes(&request.owner)?;
        let remote = source_pda(TOKEN_MESSENGER_MINTER, &[b"remote_token_messenger", b"19"])?;
        let remote = pubkey_bytes(&remote)?;
        let forwarder = evm_word_as_pubkey(FORWARDER)?;
        let source_messenger = pubkey_bytes(TOKEN_MESSENGER_MINTER)?;
        let source_usdc = pubkey_bytes(USDC_MINT)?;
        let hook = super::super::hypercore::forward_hook(proxy)?;
        check(
            raw[44..76] == source_messenger
                && raw[76..108] == remote
                && raw[108..140] == forwarder
                && raw[376..] == hook
                && raw[152..184] == source_usdc
                && raw[184..216] == forwarder
                && raw[216..248] == U256::from(request.amount_atoms).to_be_bytes::<32>()
                && raw[248..280] == owner
                && raw[280..312] == U256::from(request.max_fee_atoms).to_be_bytes::<32>()
                && raw[312..344] == U256::ZERO.to_be_bytes::<32>()
                && raw[344..376] == U256::ZERO.to_be_bytes::<32>()
                && metadata.protocol_version == 2
                && metadata.source_domain == DOMAIN
                && metadata.destination_domain == DESTINATION_DOMAIN
                && metadata.max_fee == Quantity::from_u64(request.max_fee_atoms)
                && metadata.min_finality_threshold == request.min_finality_threshold,
            "Solana Circle MessageSent account differs from its exact HyperCore burn instruction.",
        )?;
        let message_hash = format!("{:#x}", keccak256(&raw));
        // Match CCTP's immutable-message projection without passing through
        // the EVM-only source-address decoder. Circle excludes nonce and the
        // mutable finality/fee/expiration fields from this stable hash.
        let mut immutable = raw.clone();
        for range in [12..44, 144..148, 312..344, 344..376] {
            immutable[range].fill(0);
        }
        let immutable_message_hash = format!("{:#x}", keccak256(&immutable));
        Ok(SourceMessageProof {
            raw_message: hex_bytes(&raw),
            message_hash,
            immutable_message_hash,
        })
    }

    /// Bind Circle's finalized attestation to the exact Solana MessageSent
    /// account and reviewed HyperCore forwarding hook. Solana source addresses
    /// are full Pubkeys, so this deliberately avoids the EVM-only CCTP address
    /// decoder. The attestation signature itself is verified by CctpProvider.
    pub fn validate_attested_message(
        source_raw: &str,
        attested_raw: &str,
        request: &BurnRequest,
        metadata: &CctpQuoteMetadata,
        proxy: Address,
    ) -> Result<(u64, u64)> {
        let source = bytes(source_raw)?;
        let attested = bytes(attested_raw)?;
        check(
            source.len() == MESSAGE_LEN + 56
                && attested.len() == MESSAGE_LEN + 56
                && source[376..] == super::super::hypercore::forward_hook(proxy)?.as_ref()[..]
                && attested[376..] == source[376..],
            "Attested Solana CCTP message changed its exact HyperCore forward hook.",
        )?;
        // Confirm the stored source payload is itself the exact one approved
        // by the immutable request before comparing Circle's mutable fields.
        let source_proof = verify_source_message(source_raw, request, metadata, proxy)?;
        let mut immutable = attested.clone();
        for range in [12..44, 144..148, 312..344, 344..376] {
            immutable[range].fill(0);
        }
        check(
            format!("{:#x}", keccak256(&immutable)) == source_proof.immutable_message_hash
                && attested[4..8] == DOMAIN.to_be_bytes()
                && attested[8..12] == DESTINATION_DOMAIN.to_be_bytes()
                && attested[140..144] == request.min_finality_threshold.to_be_bytes()
                && attested[144..148] == request.min_finality_threshold.to_be_bytes()
                && attested[152..184] == pubkey_bytes(USDC_MINT)?
                && attested[184..216] == evm_word_as_pubkey(FORWARDER)?
                && attested[216..248] == U256::from(request.amount_atoms).to_be_bytes::<32>()
                && attested[248..280] == pubkey_bytes(&request.owner)?
                && attested[280..312] == U256::from(request.max_fee_atoms).to_be_bytes::<32>()
                && attested[344..376] == U256::ZERO.to_be_bytes::<32>()
                && metadata.protocol_version == 2
                && metadata.source_domain == DOMAIN
                && metadata.destination_domain == DESTINATION_DOMAIN
                && metadata.max_fee == Quantity::from_u64(request.max_fee_atoms)
                && metadata.min_finality_threshold == request.min_finality_threshold,
            "Circle's Solana attestation differs from its exact immutable burn fields.",
        )?;
        let nonce = U256::from_be_slice(&attested[12..44]);
        let fee = U256::from_be_slice(&attested[312..344]);
        check(
            nonce != U256::ZERO && fee <= U256::from(request.max_fee_atoms),
            "Circle's Solana attestation has an invalid nonce or exceeds its exact fee cap.",
        )?;
        let fee = u64::try_from(fee).map_err(|_| invalid("CCTP fee exceeds u64."))?;
        Ok((request.amount_atoms, fee))
    }
}

/// Reviewed direct legacy receive has at most 18 unique keys, 20 account
/// references, one wallet signature and no lookup table or extra instruction.
/// Reject an attester-set expansion before the source burn can be dispatched.
pub fn mint_wire_size(signature_threshold: u32) -> Result<usize> {
    check(
        (1..=32).contains(&signature_threshold),
        "Invalid CCTP Solana signature threshold.",
    )?;
    let data = 8 + 4 + MESSAGE_LEN + 4 + 65 * signature_threshold as usize;
    let size = 1 + 64 + 3 + 1 + 18 * 32 + 32 + 1 + 1 + 1 + 20 + 2 + data;
    check(
        size <= 1232,
        "CCTP Solana receive exceeds packet size for the deployed attester threshold; do not burn.",
    )?;
    Ok(size)
}

pub fn pubkey_bytes(value: &str) -> Result<[u8; 32]> {
    gofin_solana::parse_address(value)?;
    value
        .parse::<Pubkey>()
        .map(|v| v.to_bytes())
        .map_err(|_| invalid("Invalid Solana public key."))
}
pub fn public_key(raw: &[u8]) -> Result<String> {
    check(raw.len() == 32, "Solana public key must be 32 bytes.")?;
    Ok(bs58::encode(raw).into_string())
}
fn pda(program: &str, seeds: &[&[u8]]) -> Result<String> {
    let program = Pubkey::new_from_array(pubkey_bytes(program)?);
    Pubkey::try_find_program_address(seeds, &program)
        .map(|v| v.0.to_string())
        .ok_or_else(|| invalid("Invalid CCTP PDA seeds."))
}
fn discriminator(namespace: &str, name: &str) -> [u8; 8] {
    Sha256::digest(format!("{namespace}:{name}").as_bytes())[..8]
        .try_into()
        .expect("digest")
}
pub fn asset() -> AssetId {
    AssetId {
        network: gofin_solana::NETWORK.into(),
        reference: USDC_MINT.into(),
        decimals: 6,
    }
}
pub fn validate_destination(d: &SolanaDestination) -> Result<()> {
    check(
        d.network == gofin_solana::NETWORK && d.asset == asset(),
        "CCTP requires Solana mainnet-beta native USDC.",
    )?;
    pubkey_bytes(&d.wallet_address)?;
    pubkey_bytes(&d.recipient)?;
    check(
        d.destination_caller == d.wallet_address
            && d.recipient == d.wallet_address
            && d.wallet_resource.0 == format!("{}:{}", gofin_solana::NETWORK, d.wallet_address)
            && d.recipient_token_account
                == gofin_solana::associated_token_address(
                    &d.recipient,
                    USDC_MINT,
                    TOKEN_PROGRAM_ID,
                )?,
        "Solana destination must bind the enrolled signer, recipient and exact USDC ATA.",
    )
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SolanaCctpState {
    pub slot: u64,
    pub source_chain: u64,
    pub source_domain: u32,
    pub used_nonce: bool,
    pub nonce: Option<String>,
    pub used_nonce_address: Option<String>,
    pub account_funding_lamports: u64,
    pub recipient_exists: bool,
    pub signature_threshold: u32,
    pub enabled_attesters: Vec<String>,
    pub fee_recipient: String,
    pub fee_recipient_token_account: String,
    pub custody_balance: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SolanaMintProof {
    pub net_amount: Quantity,
    pub fee_executed: Quantity,
    pub nonce: String,
    pub receive_instruction_index: u32,
    pub actual_nonce_rent_lamports: u64,
}
struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .ok_or_else(|| invalid("CCTP account length overflow."))?;
        let b = self
            .data
            .get(self.at..end)
            .ok_or_else(|| invalid("Truncated CCTP account."))?;
        self.at = end;
        Ok(b)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("length"),
        ))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("length"),
        ))
    }
    fn key(&mut self) -> Result<String> {
        public_key(self.take(32)?)
    }
    fn bool(&mut self) -> Result<bool> {
        match self.take(1)?[0] {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(invalid("Malformed CCTP bool.")),
        }
    }
}
fn account_data(value: &Value, owner: &str, name: &str) -> Result<Vec<u8>> {
    check(
        value["owner"] == owner && value["executable"] == false,
        "CCTP state account owner or executable flag differs.",
    )?;
    let raw = gofin_solana::token::decode_rpc_account_data(&value["data"])?;
    check(
        raw.get(..8) == Some(discriminator("account", name).as_slice()),
        "CCTP account discriminator differs.",
    )?;
    Ok(raw[8..].to_vec())
}
fn token_account(
    value: &Value,
    key: &str,
    owner: &str,
) -> Result<gofin_solana::token::TokenAccountInfo> {
    check(
        value["owner"] == TOKEN_PROGRAM_ID && value["executable"] == false,
        "CCTP USDC account has wrong program owner.",
    )?;
    let info = gofin_solana::token::parse_token_account(
        TOKEN_PROGRAM_ID,
        key,
        &gofin_solana::token::decode_rpc_account_data(&value["data"])?,
    )?;
    check(
        info.mint == USDC_MINT && info.token_owner == owner && info.state == "initialized",
        "CCTP USDC account has wrong mint, owner or state.",
    )?;
    Ok(info)
}
fn messenger_fee(value: &Value) -> Result<String> {
    let b = account_data(value, TOKEN_MESSENGER_MINTER, "TokenMessenger")?;
    let mut r = Reader::new(&b);
    r.take(96)?;
    check(r.u32()? == 1, "CCTP Solana message body version differs.")?;
    r.take(1)?;
    r.key()
}
#[derive(Clone)]
struct Pdas {
    transmitter: String,
    messenger: String,
    remote: String,
    minter: String,
    local: String,
    pair: String,
    custody: String,
    authority: String,
    transmitter_event: String,
    messenger_event: String,
}
fn pdas(source_chain: u64) -> Result<Pdas> {
    let remote = domain(source_chain)?.to_string();
    let remote_token = address(
        usdc_asset(source_chain)?
            .token
            .as_deref()
            .unwrap_or_default(),
    )?
    .into_word();
    let mint = pubkey_bytes(USDC_MINT)?;
    let receiver = pubkey_bytes(TOKEN_MESSENGER_MINTER)?;
    Ok(Pdas {
        transmitter: pda(MESSAGE_TRANSMITTER, &[b"message_transmitter"])?,
        messenger: pda(TOKEN_MESSENGER_MINTER, &[b"token_messenger"])?,
        remote: pda(
            TOKEN_MESSENGER_MINTER,
            &[b"remote_token_messenger", remote.as_bytes()],
        )?,
        minter: pda(TOKEN_MESSENGER_MINTER, &[b"token_minter"])?,
        local: pda(TOKEN_MESSENGER_MINTER, &[b"local_token", &mint])?,
        pair: pda(
            TOKEN_MESSENGER_MINTER,
            &[b"token_pair", remote.as_bytes(), remote_token.as_slice()],
        )?,
        custody: pda(TOKEN_MESSENGER_MINTER, &[b"custody", &mint])?,
        authority: pda(
            MESSAGE_TRANSMITTER,
            &[b"message_transmitter_authority", &receiver],
        )?,
        transmitter_event: pda(MESSAGE_TRANSMITTER, &[b"__event_authority"])?,
        messenger_event: pda(TOKEN_MESSENGER_MINTER, &[b"__event_authority"])?,
    })
}
pub fn used_nonce_address(nonce: &str) -> Result<String> {
    let n = hash(nonce)?;
    check(n != B256::ZERO, "Missing CCTP attested nonce.")?;
    pda(MESSAGE_TRANSMITTER, &[b"used_nonce", n.as_slice()])
}

pub async fn inspect_state(
    runtime: &SolanaRuntime,
    source_chain: u64,
    d: &SolanaDestination,
    nonce: Option<&str>,
) -> Result<SolanaCctpState> {
    validate_destination(d)?;
    let genesis = runtime.rpc().request("getGenesisHash", json!([])).await?;
    check(
        genesis == gofin_solana::MAINNET_GENESIS_HASH,
        "CCTP Solana RPC genesis mismatch.",
    )?;
    let p = pdas(source_chain)?;
    let initial = runtime
        .rpc()
        .request(
            "getAccountInfo",
            json!([p.messenger,{"encoding":"base64","commitment":"confirmed"}]),
        )
        .await?;
    let fee_recipient = messenger_fee(&initial["value"])?;
    let fee_ata =
        gofin_solana::associated_token_address(&fee_recipient, USDC_MINT, TOKEN_PROGRAM_ID)?;
    let mut keys = vec![
        MESSAGE_TRANSMITTER.to_owned(),
        TOKEN_MESSENGER_MINTER.to_owned(),
        p.transmitter.clone(),
        p.messenger.clone(),
        p.remote.clone(),
        p.minter.clone(),
        p.local.clone(),
        p.pair.clone(),
        p.custody.clone(),
        USDC_MINT.into(),
        d.recipient_token_account.clone(),
        fee_ata.clone(),
    ];
    if let Some(n) = nonce {
        keys.push(used_nonce_address(n)?);
    }
    let snapshot=runtime.rpc().request("getMultipleAccounts",json!([keys,{"encoding":"base64","commitment":"confirmed","minContextSlot":initial["context"]["slot"].as_u64().ok_or_else(||invalid("Missing Solana context slot."))?}])).await?;
    let slot = snapshot["context"]["slot"]
        .as_u64()
        .ok_or_else(|| invalid("Missing Solana context slot."))?;
    check(
        slot >= initial["context"]["slot"].as_u64().unwrap_or(u64::MAX),
        "CCTP Solana snapshot predates its required context.",
    )?;
    let a = snapshot["value"]
        .as_array()
        .filter(|a| a.len() == keys.len())
        .ok_or_else(|| invalid("Missing CCTP Solana accounts."))?;
    for program in &a[..2] {
        check(
            program["executable"] == true && program["owner"] == LOADER,
            "Reviewed CCTP Solana program is not executable under the upgradeable loader.",
        )?;
    }
    let b = account_data(&a[2], MESSAGE_TRANSMITTER, "MessageTransmitter")?;
    let mut r = Reader::new(&b);
    r.take(128)?;
    check(
        !r.bool()? && r.u32()? == DOMAIN && r.u32()? == 1,
        "CCTP Solana transmitter paused or wrong domain/version.",
    )?;
    let threshold = r.u32()?;
    let count = r.u32()?;
    check(
        threshold > 0 && threshold <= 32 && count >= threshold && count <= 128,
        "Invalid Solana attester threshold.",
    )?;
    mint_wire_size(threshold)?;
    let mut attesters = Vec::new();
    for _ in 0..count {
        let raw = r.take(32)?;
        check(
            raw[..12].iter().all(|v| *v == 0),
            "Non-EVM CCTP attester key.",
        )?;
        attesters.push(hex_bytes(&raw[12..]));
    }
    check(r.u64()? >= 228, "CCTP message size limit is too small.")?;
    check(
        messenger_fee(&a[3])? == fee_recipient,
        "CCTP Solana fee recipient changed during snapshot.",
    )?;
    let b = account_data(&a[4], TOKEN_MESSENGER_MINTER, "RemoteTokenMessenger")?;
    let mut r = Reader::new(&b);
    check(
        r.u32()? == domain(source_chain)?
            && r.take(32)? == address(super::TOKEN_MESSENGER)?.into_word().as_slice(),
        "CCTP Solana remote messenger differs.",
    )?;
    let b = account_data(&a[5], TOKEN_MESSENGER_MINTER, "TokenMinter")?;
    let mut r = Reader::new(&b);
    r.take(64)?;
    check(!r.bool()?, "CCTP Solana minter paused.")?;
    let b = account_data(&a[6], TOKEN_MESSENGER_MINTER, "LocalToken")?;
    let mut r = Reader::new(&b);
    check(
        r.key()? == p.custody && r.key()? == USDC_MINT,
        "CCTP Solana local token/custody differs.",
    )?;
    let b = account_data(&a[7], TOKEN_MESSENGER_MINTER, "TokenPair")?;
    let mut r = Reader::new(&b);
    check(
        r.u32()? == domain(source_chain)?
            && r.take(32)?
                == address(
                    usdc_asset(source_chain)?
                        .token
                        .as_deref()
                        .unwrap_or_default(),
                )?
                .into_word()
                .as_slice()
            && r.key()? == p.local,
        "CCTP Solana token pair differs.",
    )?;
    let custody = token_account(&a[8], &p.custody, &p.minter)?;
    check(
        a[9]["owner"] == TOKEN_PROGRAM_ID && a[9]["executable"] == false,
        "USDC mint program differs.",
    )?;
    let mint = gofin_solana::token::parse_mint(
        TOKEN_PROGRAM_ID,
        &gofin_solana::token::decode_rpc_account_data(&a[9]["data"])?,
        0,
    )?;
    check(mint.decimals == 6, "USDC mint precision differs.")?;
    let recipient_exists = !a[10].is_null();
    check(
        recipient_exists,
        "Create the recipient native-USDC ATA before requesting CCTP.",
    )?;
    token_account(&a[10], &d.recipient_token_account, &d.recipient)?;
    token_account(&a[11], &fee_ata, &fee_recipient)?;
    let used_nonce = if nonce.is_some() && !a[12].is_null() {
        let b = account_data(&a[12], MESSAGE_TRANSMITTER, "UsedNonce")?;
        check(b == [1], "CCTP nonce account has malformed state.")?;
        true
    } else {
        false
    };
    let nonce_rent = runtime
        .rpc()
        .request(
            "getMinimumBalanceForRentExemption",
            json!([9,{"commitment":"confirmed"}]),
        )
        .await?
        .as_u64()
        .ok_or_else(|| invalid("Invalid nonce rent response."))?;
    Ok(SolanaCctpState {
        slot,
        source_chain,
        source_domain: domain(source_chain)?,
        used_nonce,
        nonce: nonce.map(str::to_owned),
        used_nonce_address: nonce.map(used_nonce_address).transpose()?,
        account_funding_lamports: nonce_rent,
        recipient_exists,
        signature_threshold: threshold,
        enabled_attesters: attesters,
        fee_recipient,
        fee_recipient_token_account: fee_ata,
        custody_balance: custody.amount_atoms,
    })
}

pub fn verify_solana_attestation(
    message: &str,
    attestation: &str,
    state: &SolanaCctpState,
) -> Result<()> {
    let m = decode_message(message)?;
    check(
        m.destination_domain == DOMAIN && m.source_domain == state.source_domain,
        "Wrong CCTP Solana attestation route.",
    )?;
    let sigs = bytes(attestation)?;
    check(
        sigs.len() == state.signature_threshold as usize * 65,
        "Solana CCTP signature count differs.",
    )?;
    let digest = keccak256(bytes(message)?);
    let mut previous = Address::ZERO;
    for sig in sigs.chunks_exact(65) {
        check(matches!(sig[64], 27 | 28), "Invalid CCTP recovery id.")?;
        let signature = Signature::from_raw(sig).map_err(|_| invalid("Invalid CCTP signature."))?;
        check(signature.normalize_s().is_none(), "High-s CCTP signature.")?;
        let signer = signature
            .recover_address_from_prehash(&digest)
            .map_err(|_| invalid("Invalid CCTP signer."))?;
        check(
            signer > previous
                && state
                    .enabled_attesters
                    .iter()
                    .any(|s| address(s).ok() == Some(signer)),
            "Disabled, duplicate or unordered Solana CCTP attester.",
        )?;
        previous = signer;
    }
    Ok(())
}
fn meta(key: impl Into<String>, signer: bool, writable: bool) -> NormalizedAccountMeta {
    NormalizedAccountMeta {
        address: key.into(),
        signer,
        writable,
    }
}
fn validate_message(d: &SolanaDestination, a: &CctpAttestation) -> Result<CctpMessage> {
    validate_destination(d)?;
    let m = decode_message(&a.message)?;
    check(
        m.destination_domain == DOMAIN
            && m.recipient == TOKEN_MESSENGER_MINTER
            && m.destination_caller == d.destination_caller
            && m.mint_recipient == d.recipient_token_account
            && m.nonce == a.nonce
            && m.fee_executed <= m.max_fee
            && m.fee_executed < m.amount,
        "Solana CCTP message differs from approved destination.",
    )?;
    u64::try_from(u256(&m.amount)?).map_err(|_| invalid("Solana CCTP amount exceeds u64."))?;
    Ok(m)
}
pub fn solana_mint_instructions(
    d: &SolanaDestination,
    a: &CctpAttestation,
    state: &SolanaCctpState,
) -> Result<Vec<NormalizedInstruction>> {
    let m = validate_message(d, a)?;
    mint_wire_size(state.signature_threshold)?;
    check(
        !state.used_nonce
            && state.nonce.as_deref() == Some(m.nonce.as_str())
            && state.source_domain == m.source_domain
            && (m.expiration_block.is_zero()
                || m.expiration_block > Quantity::from_u64(state.slot)),
        "CCTP Solana nonce used, state unbound or attestation expired.",
    )?;
    check(
        Quantity::from_u64(state.custody_balance) >= m.amount,
        "CCTP Solana custody cannot cover this mint.",
    )?;
    verify_solana_attestation(&a.message, &a.attestation, state)?;
    let p = pdas(state.source_chain)?;
    check(
        state.fee_recipient_token_account
            == gofin_solana::associated_token_address(
                &state.fee_recipient,
                USDC_MINT,
                TOKEN_PROGRAM_ID,
            )?,
        "CCTP fee ATA differs.",
    )?;
    let mut out = Vec::new();
    check(
        state.recipient_exists,
        "Create recipient native-USDC ATA before preparing CCTP mint.",
    )?;
    let accounts = vec![
        meta(&d.wallet_address, true, true),
        meta(&d.destination_caller, true, true),
        meta(p.authority, false, false),
        meta(p.transmitter, false, false),
        meta(used_nonce_address(&m.nonce)?, false, true),
        meta(TOKEN_MESSENGER_MINTER, false, false),
        meta(SYSTEM, false, false),
        meta(p.transmitter_event, false, false),
        meta(MESSAGE_TRANSMITTER, false, false),
        meta(p.messenger, false, false),
        meta(p.remote, false, false),
        meta(p.minter, false, true),
        meta(p.local, false, true),
        meta(p.pair, false, false),
        meta(&state.fee_recipient_token_account, false, true),
        meta(&d.recipient_token_account, false, true),
        meta(p.custody, false, true),
        meta(TOKEN_PROGRAM_ID, false, false),
        meta(p.messenger_event, false, false),
        meta(TOKEN_MESSENGER_MINTER, false, false),
    ];
    let mut data = discriminator("global", "receive_message").to_vec();
    for raw in [bytes(&a.message)?, bytes(&a.attestation)?] {
        data.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        data.extend_from_slice(&raw);
    }
    out.push(NormalizedInstruction {
        program_id: MESSAGE_TRANSMITTER.into(),
        accounts,
        data_base64: STANDARD.encode(data),
    });
    Ok(out)
}

/// Decode direct receive bytes. Callers must independently link its nonce to
/// the original source transaction before using an observed generation.
pub fn decode_receive_instruction(ix: &NormalizedInstruction) -> Result<(String, String)> {
    check(
        ix.program_id == MESSAGE_TRANSMITTER,
        "Wrong Solana CCTP receive program.",
    )?;
    let raw = STANDARD
        .decode(&ix.data_base64)
        .map_err(|_| invalid("Invalid Solana instruction encoding."))?;
    let mut r = Reader::new(&raw);
    check(
        r.take(8)? == discriminator("global", "receive_message"),
        "Wrong Solana CCTP instruction discriminator.",
    )?;
    let len = r.u32()? as usize;
    let message = hex_bytes(r.take(len)?);
    decode_message(&message)?;
    let len = r.u32()? as usize;
    check(
        len > 0 && len <= 65 * 32 && len.is_multiple_of(65),
        "Invalid CCTP attestation length.",
    )?;
    let attestation = hex_bytes(r.take(len)?);
    check(r.at == raw.len(), "Trailing Solana receive bytes.")?;
    Ok((message, attestation))
}

/// Finalized success is necessary but not sufficient: exact source-linked
/// attestation bytes, the program's CPI events, and USDC ATA credit must agree.
pub fn verify_solana_receipt(
    proof: &FinalizedReceiptProof,
    d: &SolanaDestination,
    a: &CctpAttestation,
) -> Result<SolanaMintProof> {
    let m = validate_message(d, a)?;
    check(!proof.failed, "CCTP Solana transaction failed.")?;
    let mut expected = discriminator("global", "receive_message").to_vec();
    for raw in [bytes(&a.message)?, bytes(&a.attestation)?] {
        expected.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        expected.extend_from_slice(&raw);
    }
    let matches = proof
        .message_instructions
        .iter()
        .enumerate()
        .filter(|(_, i)| {
            i.program_id == MESSAGE_TRANSMITTER
                && STANDARD.decode(&i.data_base64).ok().as_deref() == Some(expected.as_slice())
        })
        .collect::<Vec<_>>();
    check(
        matches.len() == 1,
        "Missing or ambiguous CCTP Solana receive instruction.",
    )?;
    let (index, ix) = matches[0];
    check(
        ix.accounts
            .get(1)
            .is_some_and(|v| v.address == d.destination_caller && v.signer)
            && ix
                .accounts
                .get(4)
                .is_some_and(|v| v.address == used_nonce_address(&m.nonce).unwrap_or_default()),
        "CCTP Solana caller or used nonce account differs.",
    )?;
    let source_chain = match m.source_domain {
        0 => 1,
        3 => 42161,
        6 => 8453,
        _ => return Err(invalid("Unsupported CCTP source domain.")),
    };
    let p = pdas(source_chain)?;
    let nonce_key = used_nonce_address(&m.nonce)?;
    check(
        ix.accounts.len() == 20
            && proof.message_instructions.len() == 1
            && proof.required_signatures.len() == 1
            && proof.account_keys.first() == Some(&d.wallet_address),
        "CCTP receipt must bind its sole signer and direct receive instruction.",
    )?;
    let expected_keys = [
        d.wallet_address.as_str(),
        d.destination_caller.as_str(),
        &p.authority,
        &p.transmitter,
        &nonce_key,
        TOKEN_MESSENGER_MINTER,
        SYSTEM,
        &p.transmitter_event,
        MESSAGE_TRANSMITTER,
        &p.messenger,
        &p.remote,
        &p.minter,
        &p.local,
        &p.pair,
        ix.accounts[14].address.as_str(),
        &d.recipient_token_account,
        &p.custody,
        TOKEN_PROGRAM_ID,
        &p.messenger_event,
        TOKEN_MESSENGER_MINTER,
    ];
    check(
        ix.accounts
            .iter()
            .zip(expected_keys)
            .all(|(actual, expected)| actual.address == expected),
        "CCTP receive PDA topology differs.",
    )?;
    let mut received = 0;
    let mut minted = 0;
    let mut nonce_rent = None;
    for group in &proof.inner_instructions {
        if group["index"].as_u64() != Some(index as u64) {
            continue;
        }
        for inner in group["instructions"]
            .as_array()
            .ok_or_else(|| invalid("Malformed Solana inner instructions."))?
        {
            let program = inner["programIdIndex"]
                .as_u64()
                .and_then(|i| proof.account_keys.get(i as usize));
            let Some(program) = program else {
                return Err(invalid("Invalid Solana CPI program index."));
            };
            let raw = bs58::decode(
                inner["data"]
                    .as_str()
                    .ok_or_else(|| invalid("Missing Solana CPI bytes."))?,
            )
            .into_vec()
            .map_err(|_| invalid("Invalid Solana CPI encoding."))?;
            let accounts = inner["accounts"]
                .as_array()
                .ok_or_else(|| invalid("Missing Solana CPI accounts."))?
                .iter()
                .map(|v| {
                    v.as_u64()
                        .and_then(|i| proof.account_keys.get(i as usize))
                        .ok_or_else(|| invalid("Invalid CPI account index."))
                })
                .collect::<Result<Vec<_>>>()?;
            if program == SYSTEM && raw.get(..4) == Some(&0u32.to_le_bytes()) {
                let mut r = Reader::new(&raw[4..]);
                let rent = r.u64()?;
                let space = r.u64()?;
                let owner = r.key()?;
                if accounts.get(1).is_some_and(|v| v.as_str() == nonce_key) {
                    check(
                        accounts.len() == 2
                            && accounts[0] == &d.wallet_address
                            && owner == MESSAGE_TRANSMITTER
                            && space == 9
                            && rent > 0
                            && r.at == r.data.len()
                            && nonce_rent.is_none(),
                        "CCTP nonce funding CPI differs.",
                    )?;
                    nonce_rent = Some(rent);
                }
            }
            if program != MESSAGE_TRANSMITTER && program != TOKEN_MESSENGER_MINTER {
                continue;
            }
            if !raw.starts_with(&EVENT_CPI) {
                continue;
            }
            let expected_authority = if program == MESSAGE_TRANSMITTER {
                &p.transmitter_event
            } else {
                &p.messenger_event
            };
            check(
                accounts.len() == 1 && accounts[0] == expected_authority,
                "CCTP event CPI authority differs.",
            )?;
            let mut r = Reader::new(&raw[8..]);
            let disc = r.take(8)?;
            if program == MESSAGE_TRANSMITTER && disc == discriminator("event", "MessageReceived") {
                check(
                    r.key()? == d.destination_caller
                        && r.u32()? == m.source_domain
                        && r.take(32)? == hash(&m.nonce)?.as_slice()
                        && r.take(32)? == address(super::TOKEN_MESSENGER)?.into_word().as_slice()
                        && r.u32()? == m.finality_threshold_executed,
                    "Solana CCTP receive event mismatch.",
                )?;
                let len = r.u32()? as usize;
                check(
                    r.take(len)? == &bytes(&a.message)?[148..] && r.at == r.data.len(),
                    "Solana CCTP message body mismatch.",
                )?;
                received += 1;
            } else if program == TOKEN_MESSENGER_MINTER
                && disc == discriminator("event", "MintAndWithdraw")
            {
                check(
                    r.key()? == d.recipient_token_account
                        && Quantity::from_u64(r.u64()?) == m.amount.checked_sub(&m.fee_executed)?
                        && r.key()? == USDC_MINT
                        && Quantity::from_u64(r.u64()?) == m.fee_executed
                        && r.at == r.data.len(),
                    "Solana CCTP mint event mismatch.",
                )?;
                minted += 1;
            }
        }
    }
    let net = m.amount.checked_sub(&m.fee_executed)?;
    check(
        received == 1
            && minted == 1
            && proof.token_account_delta_verified(
                &d.recipient_token_account,
                &d.recipient,
                USDC_MINT,
                TOKEN_PROGRAM_ID,
                6,
            )? == gofin_domain::SignedQuantity::positive(&net),
        "CCTP Solana events or exact usable native-USDC credit missing.",
    )?;
    let fee_account = &ix.accounts[14].address;
    let fee_index = proof
        .account_keys
        .iter()
        .position(|v| v == fee_account)
        .ok_or_else(|| invalid("Missing fee token account."))?;
    let fee_owner = proof
        .post_token_balances
        .iter()
        .find(|v| v["accountIndex"].as_u64() == Some(fee_index as u64))
        .and_then(|v| v["owner"].as_str())
        .ok_or_else(|| invalid("Missing fee token owner."))?;
    check(
        fee_account
            == &gofin_solana::associated_token_address(fee_owner, USDC_MINT, TOKEN_PROGRAM_ID)?
            && proof.token_account_delta_verified(
                fee_account,
                fee_owner,
                USDC_MINT,
                TOKEN_PROGRAM_ID,
                6,
            )? == gofin_domain::SignedQuantity::positive(&m.fee_executed)
            && proof.token_account_delta_verified(
                &p.custody,
                &p.minter,
                USDC_MINT,
                TOKEN_PROGRAM_ID,
                6,
            )? == gofin_domain::SignedQuantity::negative(&m.amount),
        "CCTP fee credit or custody debit differs.",
    )?;
    let rent = nonce_rent.ok_or_else(|| invalid("Missing exact UsedNonce creation funding."))?;
    check(
        proof.lamport_delta(&nonce_key)?
            == gofin_domain::SignedQuantity::positive(&Quantity::from_u64(rent))
            && proof.lamport_delta(&d.wallet_address)?
                == gofin_domain::SignedQuantity::negative(&Quantity::from_u64(
                    rent.checked_add(proof.fee_lamports)
                        .ok_or_else(|| invalid("Solana fee overflow."))?,
                )),
        "CCTP nonce rent or signer SOL debit differs.",
    )?;
    Ok(SolanaMintProof {
        net_amount: net,
        fee_executed: m.fee_executed,
        nonce: m.nonce,
        receive_instruction_index: index as u32,
        actual_nonce_rent_lamports: rent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::signers::{SignerSync, local::PrivateKeySigner};

    fn fixture() -> (
        SolanaDestination,
        CctpAttestation,
        SolanaCctpState,
        gofin_solana::SolanaSigner,
    ) {
        let key = ed25519_dalek::SigningKey::from_bytes(&[1u8; 32]);
        let signer =
            gofin_solana::SolanaSigner::parse(&bs58::encode(key.to_keypair_bytes()).into_string())
                .unwrap();
        let wallet = signer.address().to_owned();
        let d = SolanaDestination {
            network: gofin_solana::NETWORK.into(),
            wallet: Some("fixture".into()),
            wallet_resource: ResourceId(format!("{}:{wallet}", gofin_solana::NETWORK)),
            wallet_address: wallet.clone(),
            recipient: wallet.clone(),
            asset: asset(),
            recipient_token_account: gofin_solana::associated_token_address(
                &wallet,
                USDC_MINT,
                TOKEN_PROGRAM_ID,
            )
            .unwrap(),
            destination_caller: wallet,
        };
        let mut raw = vec![0u8; MESSAGE_LEN];
        for (offset, n) in [
            (0, 1u32),
            (4, 6),
            (8, 5),
            (140, 1000),
            (144, 1000),
            (148, 1),
        ] {
            raw[offset..offset + 4].copy_from_slice(&n.to_be_bytes());
        }
        raw[12..44].fill(7);
        raw[44..76].copy_from_slice(
            address(super::super::TOKEN_MESSENGER)
                .unwrap()
                .into_word()
                .as_slice(),
        );
        raw[76..108].copy_from_slice(&pubkey_bytes(TOKEN_MESSENGER_MINTER).unwrap());
        raw[108..140].copy_from_slice(&pubkey_bytes(&d.destination_caller).unwrap());
        raw[152..184].copy_from_slice(
            address(usdc_asset(8453).unwrap().token.as_deref().unwrap())
                .unwrap()
                .into_word()
                .as_slice(),
        );
        raw[184..216].copy_from_slice(&pubkey_bytes(&d.recipient_token_account).unwrap());
        raw[216..248].copy_from_slice(&U256::from(1_000_000).to_be_bytes::<32>());
        raw[248..280].copy_from_slice(
            address("0x1111111111111111111111111111111111111111")
                .unwrap()
                .into_word()
                .as_slice(),
        );
        raw[280..312].copy_from_slice(&U256::from(500).to_be_bytes::<32>());
        raw[312..344].copy_from_slice(&U256::from(100).to_be_bytes::<32>());
        raw[344..376].copy_from_slice(&U256::from(200).to_be_bytes::<32>());
        let mut signed = (1..=2)
            .map(|n| {
                let key: PrivateKeySigner = format!("{n:064x}").parse().unwrap();
                (
                    key.address(),
                    key.sign_hash_sync(&keccak256(&raw)).unwrap().as_bytes(),
                )
            })
            .collect::<Vec<_>>();
        signed.sort_by_key(|v| v.0);
        let a = CctpAttestation {
            source_transaction_hash: format!("0x{}", "ab".repeat(32)),
            message: hex_bytes(&raw),
            attestation: hex_bytes(&signed.iter().flat_map(|v| v.1).collect::<Vec<_>>()),
            nonce: hex_bytes(&[7; 32]),
            forward_tx_hash: None,
        };
        let fee_recipient = public_key(&[2; 32]).unwrap();
        let state = SolanaCctpState {
            slot: 100,
            source_chain: 8453,
            source_domain: 6,
            used_nonce: false,
            nonce: Some(a.nonce.clone()),
            used_nonce_address: Some(used_nonce_address(&a.nonce).unwrap()),
            account_funding_lamports: 953520,
            recipient_exists: true,
            signature_threshold: 2,
            enabled_attesters: signed.iter().map(|v| format!("{:#x}", v.0)).collect(),
            fee_recipient_token_account: gofin_solana::associated_token_address(
                &fee_recipient,
                USDC_MINT,
                TOKEN_PROGRAM_ID,
            )
            .unwrap(),
            fee_recipient,
            custody_balance: 2_000_000,
        };
        (d, a, state, signer)
    }
    struct StateRpc {
        accounts: Vec<Value>,
        wrong_genesis: bool,
    }
    fn rpc_account(owner: &str, name: &str, body: Vec<u8>) -> Value {
        let mut raw = if name.is_empty() {
            vec![]
        } else {
            discriminator("account", name).to_vec()
        };
        raw.extend(body);
        json!({"owner":owner,"executable":false,"lamports":1_000_000,"data":[STANDARD.encode(raw),"base64"]})
    }
    fn state_rpc(d: &SolanaDestination, state: &SolanaCctpState) -> StateRpc {
        let p = pdas(8453).unwrap();
        let program = json!({"owner":LOADER,"executable":true,"data":["","base64"]});
        let mut mt = vec![0; 128];
        mt.push(0);
        mt.extend(5u32.to_le_bytes());
        mt.extend(1u32.to_le_bytes());
        mt.extend(2u32.to_le_bytes());
        mt.extend(2u32.to_le_bytes());
        for a in &state.enabled_attesters {
            mt.extend(address(a).unwrap().into_word());
        }
        mt.extend(8192u64.to_le_bytes());
        let mut messenger = vec![0; 96];
        messenger.extend(1u32.to_le_bytes());
        messenger.push(0);
        messenger.extend(pubkey_bytes(&state.fee_recipient).unwrap());
        messenger.extend([0; 36]);
        let mut remote = 6u32.to_le_bytes().to_vec();
        remote.extend(address(super::super::TOKEN_MESSENGER).unwrap().into_word());
        let minter = vec![0; 66];
        let mut local = pubkey_bytes(&p.custody).unwrap().to_vec();
        local.extend(pubkey_bytes(USDC_MINT).unwrap());
        local.extend([0; 58]);
        let mut pair = 6u32.to_le_bytes().to_vec();
        pair.extend(
            address(usdc_asset(8453).unwrap().token.as_deref().unwrap())
                .unwrap()
                .into_word(),
        );
        pair.extend(pubkey_bytes(&p.local).unwrap());
        pair.push(0);
        let token = |owner: &str, amount: u64| {
            let mut raw = vec![0; 165];
            raw[..32].copy_from_slice(&pubkey_bytes(USDC_MINT).unwrap());
            raw[32..64].copy_from_slice(&pubkey_bytes(owner).unwrap());
            raw[64..72].copy_from_slice(&amount.to_le_bytes());
            raw[108] = 1;
            rpc_account(TOKEN_PROGRAM_ID, "", raw)
        };
        let mut mint = vec![0; 82];
        mint[44] = 6;
        mint[45] = 1;
        StateRpc {
            accounts: vec![
                program.clone(),
                program,
                rpc_account(MESSAGE_TRANSMITTER, "MessageTransmitter", mt),
                rpc_account(TOKEN_MESSENGER_MINTER, "TokenMessenger", messenger),
                rpc_account(TOKEN_MESSENGER_MINTER, "RemoteTokenMessenger", remote),
                rpc_account(TOKEN_MESSENGER_MINTER, "TokenMinter", minter),
                rpc_account(TOKEN_MESSENGER_MINTER, "LocalToken", local),
                rpc_account(TOKEN_MESSENGER_MINTER, "TokenPair", pair),
                token(&p.minter, state.custody_balance),
                rpc_account(TOKEN_PROGRAM_ID, "", mint),
                token(&d.recipient, 0),
                token(&state.fee_recipient, 0),
                Value::Null,
            ],
            wrong_genesis: false,
        }
    }
    #[async_trait]
    impl gofin_solana::rpc::SolanaRpc for StateRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            Ok(match method {
                "getGenesisHash" => json!(if self.wrong_genesis {
                    "devnet"
                } else {
                    gofin_solana::MAINNET_GENESIS_HASH
                }),
                "getAccountInfo" => json!({"context":{"slot":99},"value":self.accounts[3]}),
                "getMultipleAccounts" => {
                    assert_eq!(params[1]["minContextSlot"], 99);
                    let n = params[0].as_array().unwrap().len();
                    json!({"context":{"slot":100},"value":self.accounts[..n]})
                }
                "getMinimumBalanceForRentExemption" => {
                    assert_eq!(params[0], 9);
                    json!(953520)
                }
                _ => panic!("unexpected RPC {method}"),
            })
        }
    }
    #[tokio::test]
    async fn onchain_snapshot_checks_pdas_programs_attesters_native_usdc_and_existing_ata() {
        let (d, a, state, _) = fixture();
        let actual = inspect_state(
            &SolanaRuntime::new(Arc::new(state_rpc(&d, &state))),
            8453,
            &d,
            Some(&a.nonce),
        )
        .await
        .unwrap();
        assert_eq!(actual, state);
        for n in 0..5 {
            let mut rpc = state_rpc(&d, &state);
            match n {
                0 => rpc.wrong_genesis = true,
                1 => rpc.accounts[10] = Value::Null,
                2 => rpc.accounts[0]["executable"] = json!(false),
                3 => rpc.accounts[7]["owner"] = json!(SYSTEM),
                _ => {
                    let mut raw = STANDARD
                        .decode(rpc.accounts[2]["data"][0].as_str().unwrap())
                        .unwrap();
                    raw[145..149].copy_from_slice(&3u32.to_le_bytes());
                    rpc.accounts[2]["data"][0] = json!(STANDARD.encode(raw));
                }
            }
            assert!(
                inspect_state(&SolanaRuntime::new(Arc::new(rpc)), 8453, &d, None)
                    .await
                    .is_err(),
                "case {n}"
            );
        }
    }
    struct NoRpc;
    #[async_trait]
    impl gofin_solana::rpc::SolanaRpc for NoRpc {
        async fn request(&self, method: &str, _: Value) -> Result<Value> {
            if matches!(method, "getSlot" | "getBlockHeight") {
                Ok(json!(100))
            } else {
                panic!("unexpected {method}")
            }
        }
    }
    #[tokio::test]
    async fn exact_two_attester_legacy_transaction_fits_packet_without_lookup_table() {
        let (d, a, state, signer) = fixture();
        let runtime = SolanaRuntime::new(Arc::new(NoRpc));
        let ix = solana_mint_instructions(&d, &a, &state).unwrap();
        assert_eq!(ix.len(), 1);
        assert_eq!(ix[0].accounts.len(), 20);
        assert_eq!(mint_wire_size(2).unwrap(), 1224);
        let frozen = runtime
            .freeze_instructions(
                &ix,
                &[],
                &public_key(&[9; 32]).unwrap(),
                &signer,
                1000,
                5000,
                None,
                &[],
            )
            .await
            .unwrap();
        assert_eq!(frozen.required_signers(), [d.wallet_address]);
        let mut too_large = ix;
        let mut raw = STANDARD.decode(&too_large[0].data_base64).unwrap();
        raw[388..392].copy_from_slice(&195u32.to_le_bytes());
        raw.extend_from_slice(&[0; 65]);
        too_large[0].data_base64 = STANDARD.encode(raw);
        assert!(
            runtime
                .freeze_instructions(
                    &too_large,
                    &[],
                    &public_key(&[9; 32]).unwrap(),
                    &signer,
                    1000,
                    5000,
                    None,
                    &[]
                )
                .await
                .is_err()
        );
        assert!(mint_wire_size(3).is_err());
    }
    #[test]
    fn solana_raw_words_and_direct_decode_preserve_case_and_exact_bytes() {
        let (d, a, state, _) = fixture();
        let m = decode_message(&a.message).unwrap();
        assert_eq!(m.mint_recipient, d.recipient_token_account);
        assert_eq!(m.recipient, TOKEN_MESSENGER_MINTER);
        let mut ix = solana_mint_instructions(&d, &a, &state).unwrap().remove(0);
        assert_eq!(
            decode_receive_instruction(&ix).unwrap(),
            (a.message, a.attestation)
        );
        let mut raw = STANDARD.decode(&ix.data_base64).unwrap();
        raw.push(0);
        ix.data_base64 = STANDARD.encode(raw);
        assert!(decode_receive_instruction(&ix).is_err());
    }
    #[test]
    fn mint_rejects_wrong_signer_nonce_expiry_missing_ata_and_insufficient_custody() {
        let (d, a, state, _) = fixture();
        for n in 0..7 {
            let mut changed = state.clone();
            match n {
                0 => changed.slot = 200,
                1 => changed.used_nonce = true,
                2 => changed.recipient_exists = false,
                3 => changed.nonce = Some(hex_bytes(&[8; 32])),
                4 => changed.custody_balance = 999999,
                5 => changed.enabled_attesters.clear(),
                _ => changed.signature_threshold = 3,
            };
            assert!(
                solana_mint_instructions(&d, &a, &changed).is_err(),
                "case {n}"
            );
        }
        let mut reversed = a.clone();
        let sig = bytes(&a.attestation).unwrap();
        reversed.attestation = hex_bytes(&[&sig[65..], &sig[..65]].concat());
        assert!(solana_mint_instructions(&d, &reversed, &state).is_err());
        let mut wrong = d;
        wrong.destination_caller = public_key(&[9; 32]).unwrap();
        assert!(solana_mint_instructions(&wrong, &a, &state).is_err());
    }
    fn receipt(
        d: &SolanaDestination,
        a: &CctpAttestation,
        state: &SolanaCctpState,
    ) -> FinalizedReceiptProof {
        let ix = solana_mint_instructions(d, a, state).unwrap().remove(0);
        let p = pdas(8453).unwrap();
        let mut keys = vec![d.wallet_address.clone()];
        for account in &ix.accounts {
            if !keys.contains(&account.address) {
                keys.push(account.address.clone());
            }
        }
        let idx = |key: &str| keys.iter().position(|k| k == key).unwrap();
        let mut pre = vec![1; keys.len()];
        let mut post = pre.clone();
        pre[0] = 2_000_000;
        post[0] = pre[0] - state.account_funding_lamports - 5000;
        let nonce = used_nonce_address(&a.nonce).unwrap();
        pre[idx(&nonce)] = 0;
        post[idx(&nonce)] = state.account_funding_lamports;
        let balance = |account: &str, owner: &str, amount: u64| json!({"accountIndex":idx(account),"mint":USDC_MINT,"owner":owner,"programId":TOKEN_PROGRAM_ID,"uiTokenAmount":{"amount":amount.to_string(),"decimals":6}});
        let mut create = 0u32.to_le_bytes().to_vec();
        create.extend(state.account_funding_lamports.to_le_bytes());
        create.extend(9u64.to_le_bytes());
        create.extend(pubkey_bytes(MESSAGE_TRANSMITTER).unwrap());
        let mut receive = EVENT_CPI.to_vec();
        receive.extend(discriminator("event", "MessageReceived"));
        receive.extend(pubkey_bytes(&d.destination_caller).unwrap());
        receive.extend(6u32.to_le_bytes());
        receive.extend([7; 32]);
        receive.extend(address(super::super::TOKEN_MESSENGER).unwrap().into_word());
        receive.extend(1000u32.to_le_bytes());
        receive.extend(228u32.to_le_bytes());
        receive.extend(&bytes(&a.message).unwrap()[148..]);
        let mut mint = EVENT_CPI.to_vec();
        mint.extend(discriminator("event", "MintAndWithdraw"));
        mint.extend(pubkey_bytes(&d.recipient_token_account).unwrap());
        mint.extend(999900u64.to_le_bytes());
        mint.extend(pubkey_bytes(USDC_MINT).unwrap());
        mint.extend(100u64.to_le_bytes());
        let inner = |program: &str, accounts: Vec<usize>, raw: Vec<u8>| json!({"programIdIndex":idx(program),"accounts":accounts,"data":bs58::encode(raw).into_string()});
        let inner = vec![
            inner(SYSTEM, vec![0, idx(&nonce)], create),
            inner(TOKEN_MESSENGER_MINTER, vec![idx(&p.messenger_event)], mint),
            inner(
                MESSAGE_TRANSMITTER,
                vec![idx(&p.transmitter_event)],
                receive,
            ),
        ];
        let pre_tokens = vec![
            balance(&d.recipient_token_account, &d.recipient, 10),
            balance(&state.fee_recipient_token_account, &state.fee_recipient, 20),
            balance(&p.custody, &p.minter, 2_000_000),
        ];
        let post_tokens = vec![
            balance(&d.recipient_token_account, &d.recipient, 999910),
            balance(
                &state.fee_recipient_token_account,
                &state.fee_recipient,
                120,
            ),
            balance(&p.custody, &p.minter, 1_000_000),
        ];
        FinalizedReceiptProof {
            signature: "synthetic".into(),
            slot: 150,
            fee_lamports: 5000,
            failed: false,
            message_sha256: "synthetic".into(),
            account_keys: keys,
            message_instructions: vec![ix],
            required_signatures: vec!["synthetic".into()],
            pre_balances_lamports: pre,
            post_balances_lamports: post,
            pre_token_balances: pre_tokens,
            post_token_balances: post_tokens,
            loaded_addresses: json!({}),
            inner_instructions: vec![json!({"index":0,"instructions":inner})],
            log_messages: vec![],
            error: None,
        }
    }
    #[test]
    fn finalized_proof_binds_message_cpi_credit_fee_and_permanent_nonce_rent() {
        let (d, a, state, _) = fixture();
        let proof = receipt(&d, &a, &state);
        let result = verify_solana_receipt(&proof, &d, &a).unwrap();
        assert_eq!(result.net_amount, Quantity::from_u64(999900));
        assert_eq!(result.actual_nonce_rent_lamports, 953520);
        for n in 0..9 {
            let mut bad = proof.clone();
            match n {
                0 => bad.failed = true,
                1 => bad.post_token_balances[0]["uiTokenAmount"]["amount"] = json!("999911"),
                2 => bad.post_token_balances[1]["uiTokenAmount"]["amount"] = json!("119"),
                3 => bad.post_token_balances[2]["uiTokenAmount"]["amount"] = json!("1000001"),
                4 => bad.post_balances_lamports[0] += 1,
                5 => bad.inner_instructions[0]["instructions"][0]["accounts"] = json!([1, 2]),
                6 => bad.inner_instructions[0]["instructions"][1]["accounts"] = json!([0]),
                7 => {
                    bad.message_instructions[0].accounts[4].address = public_key(&[3; 32]).unwrap()
                }
                _ => bad.inner_instructions.clear(),
            };
            assert!(verify_solana_receipt(&bad, &d, &a).is_err(), "case {n}");
        }
        let mut different = a;
        different.nonce = hex_bytes(&[8; 32]);
        assert!(verify_solana_receipt(&proof, &d, &different).is_err());
    }

    #[test]
    fn hypercore_source_burn_binds_forwarder_hook_and_domain() {
        let owner = public_key(&[8; 32]).unwrap();
        let message_sent_event_data = public_key(&[10; 32]).unwrap();
        let proxy: Address = "0x1234567890123456789012345678901234567890"
            .parse()
            .unwrap();
        let hook = super::super::hypercore::forward_hook(proxy).unwrap();
        let metadata = CctpQuoteMetadata {
            protocol_version: 2,
            source_domain: DOMAIN,
            destination_domain: super::super::hypercore::DOMAIN,
            transfer_mode: CctpTransferMode::Fast,
            max_fee: Quantity::from_u64(10),
            min_finality_threshold: 1000,
        };
        let request = hypercore_source::BurnRequest {
            owner: owner.clone(),
            message_sent_event_data,
            amount_atoms: 1_000_000,
            max_fee_atoms: 10,
            min_finality_threshold: 1000,
            hook_data: hex_bytes(&hook),
            message_sent_event_rent_lamports: 1,
        };
        let instruction = hypercore_source::burn_instruction(&request, &metadata, proxy).unwrap();
        assert_eq!(instruction.program_id, TOKEN_MESSENGER_MINTER);
        assert_eq!(instruction.accounts.len(), 18);
        assert_eq!(instruction.accounts[0].address, owner);
        assert_eq!(instruction.accounts[1].address, owner);
        assert_eq!(
            instruction.accounts[7].address,
            "3WG7G3vPxz4a6bdBb9Xic2YL4QM84ru1YcVYskcweVHY"
        );
        hypercore_source::validate_burn_instruction(&instruction, &request, &metadata, proxy)
            .unwrap();
        let raw = STANDARD.decode(&instruction.data_base64).unwrap();
        let mut reader = Reader::new(&raw);
        assert_eq!(
            reader.take(8).unwrap(),
            discriminator("global", "deposit_for_burn_with_hook")
        );
        assert_eq!(reader.u64().unwrap(), 1_000_000);
        assert_eq!(reader.u32().unwrap(), super::super::hypercore::DOMAIN);
        assert_eq!(
            reader.take(32).unwrap(),
            &address(super::super::hypercore::FORWARDER)
                .unwrap()
                .into_word()
                .0
        );
        assert_eq!(
            reader.take(32).unwrap(),
            &address(super::super::hypercore::FORWARDER)
                .unwrap()
                .into_word()
                .0
        );
        assert_eq!(reader.u64().unwrap(), 10);
        assert_eq!(reader.u32().unwrap(), 1000);
        let hook_len = reader.u32().unwrap() as usize;
        assert_eq!(reader.take(hook_len).unwrap(), hook.as_ref());
        assert_eq!(reader.at, raw.len());

        let mut wrong = request.clone();
        wrong.amount_atoms += 1;
        assert!(
            hypercore_source::validate_burn_instruction(&instruction, &wrong, &metadata, proxy)
                .is_err()
        );
        let wrong_metadata = CctpQuoteMetadata {
            destination_domain: 3,
            ..metadata
        };
        assert!(hypercore_source::burn_instruction(&request, &wrong_metadata, proxy).is_err());
    }

    #[test]
    fn hypercore_source_receipt_requires_exact_instruction_and_usdc_burn() {
        let owner = public_key(&[8; 32]).unwrap();
        let message_sent_event_data = public_key(&[10; 32]).unwrap();
        let proxy: Address = "0x1234567890123456789012345678901234567890"
            .parse()
            .unwrap();
        let hook = super::super::hypercore::forward_hook(proxy).unwrap();
        let metadata = CctpQuoteMetadata {
            protocol_version: 2,
            source_domain: DOMAIN,
            destination_domain: super::super::hypercore::DOMAIN,
            transfer_mode: CctpTransferMode::Fast,
            max_fee: Quantity::from_u64(10),
            min_finality_threshold: 1000,
        };
        let request = hypercore_source::BurnRequest {
            owner: owner.clone(),
            message_sent_event_data,
            amount_atoms: 1_000_000,
            max_fee_atoms: 10,
            min_finality_threshold: 1000,
            hook_data: hex_bytes(&hook),
            message_sent_event_rent_lamports: 1,
        };
        let instruction = hypercore_source::burn_instruction(&request, &metadata, proxy).unwrap();
        let mut account_keys: Vec<String> = instruction
            .accounts
            .iter()
            .map(|meta| meta.address.clone())
            .collect();
        account_keys.push(TOKEN_MESSENGER_MINTER.into());
        let burn_account =
            gofin_solana::associated_token_address(&owner, USDC_MINT, TOKEN_PROGRAM_ID).unwrap();
        let burn_index = account_keys
            .iter()
            .position(|key| key == &burn_account)
            .unwrap() as u64;
        let token_balance = |amount: &str| {
            json!({
                "accountIndex": burn_index,
                "mint": USDC_MINT,
                "owner": owner,
                "programId": TOKEN_PROGRAM_ID,
                "uiTokenAmount": {"amount": amount, "decimals": 6}
            })
        };
        let proof = FinalizedReceiptProof {
            signature: "solana-source-burn-signature".into(),
            slot: 123,
            fee_lamports: 7000,
            failed: false,
            message_sha256: "synthetic".into(),
            account_keys,
            message_instructions: vec![instruction],
            required_signatures: vec!["signature-bytes".into()],
            pre_balances_lamports: vec![1; 19],
            post_balances_lamports: vec![1; 19],
            pre_token_balances: vec![token_balance("1000000")],
            post_token_balances: vec![token_balance("0")],
            loaded_addresses: json!({}),
            inner_instructions: vec![],
            log_messages: vec![],
            error: None,
        };
        let result =
            hypercore_source::verify_burn_receipt(&proof, &request, &metadata, proxy).unwrap();
        assert_eq!(result.source_usdc_debit_atoms, 1_000_000);
        assert_eq!(result.destination_domain, 19);
        assert_eq!(result.network_fee_lamports, 7000);

        let mut failed = proof.clone();
        failed.failed = true;
        assert!(
            hypercore_source::verify_burn_receipt(&failed, &request, &metadata, proxy).is_err()
        );
        let mut wrong_debit = proof.clone();
        wrong_debit.post_token_balances[0]["uiTokenAmount"]["amount"] = json!("1");
        assert!(
            hypercore_source::verify_burn_receipt(&wrong_debit, &request, &metadata, proxy)
                .is_err()
        );
    }

    #[test]
    fn hypercore_source_message_decodes_raw_solana_pubkeys_and_binds_hook() {
        let owner = public_key(&[8; 32]).unwrap();
        let proxy: Address = "0x1234567890123456789012345678901234567890"
            .parse()
            .unwrap();
        let metadata = CctpQuoteMetadata {
            protocol_version: 2,
            source_domain: DOMAIN,
            destination_domain: super::super::hypercore::DOMAIN,
            transfer_mode: CctpTransferMode::Fast,
            max_fee: Quantity::from_u64(10),
            min_finality_threshold: 1000,
        };
        let request = hypercore_source::BurnRequest {
            owner: owner.clone(),
            message_sent_event_data: public_key(&[10; 32]).unwrap(),
            amount_atoms: 1_000_000,
            max_fee_atoms: 10,
            min_finality_threshold: 1000,
            hook_data: hex_bytes(&super::super::hypercore::forward_hook(proxy).unwrap()),
            message_sent_event_rent_lamports: 1,
        };
        let remote = hypercore_source::source_pda(
            TOKEN_MESSENGER_MINTER,
            &[b"remote_token_messenger", b"19"],
        )
        .unwrap()
        .parse::<Pubkey>()
        .unwrap();
        let owner_bytes = owner.parse::<Pubkey>().unwrap().to_bytes();
        let remote_bytes = remote.to_bytes();
        let messenger_bytes = TOKEN_MESSENGER_MINTER.parse::<Pubkey>().unwrap().to_bytes();
        let usdc_bytes = USDC_MINT.parse::<Pubkey>().unwrap().to_bytes();
        let forwarder_word =
            hypercore_source::evm_word_as_pubkey(hypercore_source::FORWARDER).unwrap();
        let mut raw = vec![0u8; MESSAGE_LEN + 56];
        let put_u32 = |raw: &mut [u8], offset: usize, value: u32| {
            raw[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
        };
        put_u32(&mut raw, 0, 1);
        put_u32(&mut raw, 4, DOMAIN);
        put_u32(&mut raw, 8, super::super::hypercore::DOMAIN);
        raw[44..76].copy_from_slice(&messenger_bytes);
        raw[76..108].copy_from_slice(&remote_bytes);
        raw[108..140].copy_from_slice(&forwarder_word);
        put_u32(&mut raw, 140, 1000);
        put_u32(&mut raw, 148, 1);
        raw[152..184].copy_from_slice(&usdc_bytes);
        raw[184..216].copy_from_slice(&forwarder_word);
        raw[216..248].copy_from_slice(&U256::from(1_000_000u64).to_be_bytes::<32>());
        raw[248..280].copy_from_slice(&owner_bytes);
        raw[280..312].copy_from_slice(&U256::from(10u64).to_be_bytes::<32>());
        raw[MESSAGE_LEN..].copy_from_slice(&super::super::hypercore::forward_hook(proxy).unwrap());

        let decoded =
            hypercore_source::verify_source_message(&hex_bytes(&raw), &request, &metadata, proxy)
                .unwrap();
        let generic = crate::cctp::decode_message(&hex_bytes(&raw)).unwrap();
        assert_eq!(generic.source_domain, DOMAIN);
        assert_eq!(generic.sender, public_key(&messenger_bytes).unwrap());
        assert_eq!(generic.recipient, public_key(&remote_bytes).unwrap());
        assert_eq!(generic.burn_token, public_key(&usdc_bytes).unwrap());
        assert_eq!(generic.message_sender, owner);
        assert_eq!(decoded.raw_message, hex_bytes(&raw));
        assert_eq!(decoded.message_hash, format!("{:#x}", keccak256(&raw)));
        let mut wrong = raw;
        wrong[248] ^= 1;
        assert!(
            hypercore_source::verify_source_message(&hex_bytes(&wrong), &request, &metadata, proxy)
                .is_err()
        );
    }
}
