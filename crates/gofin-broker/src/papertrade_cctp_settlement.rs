//! CCTP V2 evidence checks shared by the Papertrade funding routes.
//!
//! Dispatch identities and receipt discovery stay with the route host. This
//! verifier accepts only canonical EVM receipts and exact Circle message
//! artifacts; it cannot submit, sign, or replay a transaction.

use std::sync::Arc;

use gofin_bridge::{canonical::CanonicalRawLog, cctp};
use gofin_domain::{GoFinError, Quantity};
use gofin_evm::{CanonicalReceipt, alloy::primitives::U256};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CctpDestinationProof {
    pub transaction_hash: String,
    pub block_hash: String,
    pub block_number: u64,
    pub confirmations: u64,
    pub destination_domain: u32,
    pub recipient: String,
    pub gross_atoms: String,
    pub fee_atoms: String,
    pub net_atoms: String,
    pub nonce: String,
    pub message_received_log_index: u32,
}

/// Narrow route port for verifying an external EVM destination or Circle's
/// HyperCore forwarder receipt. Route hosts discover the exact receipt hash
/// and call these functions after observing configured finality.
#[async_trait::async_trait]
#[allow(clippy::too_many_arguments)] // Route-specific exact proof fields are intentionally explicit.
pub(crate) trait PapertradeCctpSettlement: Send + Sync {
    /// Fetch Circle's typed attestation state for an already canonical source
    /// transaction. The argument is a CCTP domain (not an EVM chain id), so
    /// Solana (5) and HyperCore (19) remain explicit and unambiguous.
    async fn fetch_attestation(
        &self,
        source_domain: u32,
        source_tx_hash: &str,
    ) -> Result<cctp::CctpAttestationStatus, GoFinError>;

    /// Verify a CoreWriter CCTP withdrawal settled by a finalized Solana mint.
    async fn verify_hypercore_outbound_solana(
        &self,
        quote: &cctp::CctpQuoteMetadata,
        source_transaction_hash: &str,
        attested_message: &str,
        attestation: &str,
        forward_tx_hash: &str,
        destination: &gofin_bridge::SolanaDestination,
        hook_owner: &str,
        amount_atoms: u64,
        core_nonce: u64,
        proof: &gofin_solana::FinalizedReceiptProof,
    ) -> Result<SolanaCctpDestinationProof, GoFinError>;

    /// Verify the destination-side proof for a CoreWriter CCTP withdrawal.
    /// The sibling route service independently binds the source action and
    /// ledger; this method proves Circle's exact attested message and canonical
    /// EVM mint receipt. Solana destination receipts use Solana's finalized
    /// receipt verifier instead.
    async fn verify_hypercore_outbound_evm(
        &self,
        quote: &cctp::CctpQuoteMetadata,
        attested_message: &str,
        attestation: &str,
        chain_id: u64,
        owner: &str,
        amount_atoms: u64,
        core_nonce: u64,
        receipt: &CanonicalReceipt,
    ) -> Result<CctpDestinationProof, GoFinError>;

    async fn verify_hypercore_forward(
        &self,
        source_message: &str,
        quote: &cctp::CctpQuoteMetadata,
        attested_message: &str,
        attestation: &str,
        source_chain_id: u64,
        source_owner: &str,
        amount_atoms: u64,
        proxy: &str,
        receipt: &CanonicalReceipt,
    ) -> Result<CctpDestinationProof, GoFinError>;

    async fn verify_solana_hypercore_forward(
        &self,
        source_message: &str,
        request: &cctp::solana::hypercore_source::BurnRequest,
        quote: &cctp::CctpQuoteMetadata,
        attested_message: &str,
        attestation: &str,
        proxy: &str,
        receipt: &CanonicalReceipt,
    ) -> Result<CctpDestinationProof, GoFinError>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SolanaCctpDestinationProof {
    pub signature: String,
    pub slot: u64,
    pub destination_domain: u32,
    pub recipient_token_account: String,
    pub gross_atoms: String,
    pub fee_atoms: String,
    pub net_atoms: String,
    pub nonce: String,
}

pub(crate) struct PapertradeCctpSettlementVerifier {
    provider: Arc<cctp::CctpProvider>,
}

impl PapertradeCctpSettlementVerifier {
    pub(crate) fn new(provider: Arc<cctp::CctpProvider>) -> Self {
        Self { provider }
    }

    fn logs(receipt: &CanonicalReceipt) -> Result<Vec<CanonicalRawLog>, GoFinError> {
        let logs = receipt
            .receipt
            .get("logs")
            .and_then(Value::as_array)
            .filter(|logs| logs.len() <= 512)
            .ok_or_else(|| GoFinError::validation("CCTP receipt logs are missing or oversized."))?;
        logs.iter()
            .map(|log| {
                let address = log
                    .get("address")
                    .and_then(Value::as_str)
                    .filter(|v| v.len() == 42 && v.starts_with("0x"))
                    .ok_or_else(|| GoFinError::validation("CCTP log address is invalid."))?
                    .to_owned();
                let topics = log
                    .get("topics")
                    .and_then(Value::as_array)
                    .filter(|topics| !topics.is_empty() && topics.len() <= 4)
                    .ok_or_else(|| GoFinError::validation("CCTP log topics are invalid."))?
                    .iter()
                    .map(|topic| {
                        let topic = topic.as_str().ok_or_else(|| {
                            GoFinError::validation("CCTP log topic is malformed.")
                        })?;
                        if topic.len() != 66 || !topic.starts_with("0x") {
                            return Err(GoFinError::validation(
                                "CCTP log topic is not one 32-byte hash.",
                            ));
                        }
                        Ok(topic.to_owned())
                    })
                    .collect::<Result<Vec<_>, GoFinError>>()?;
                let data = log
                    .get("data")
                    .and_then(Value::as_str)
                    .filter(|v| v.starts_with("0x") && v.len() <= 131_074)
                    .ok_or_else(|| GoFinError::validation("CCTP log data is malformed."))?
                    .to_owned();
                let log_index = log
                    .get("logIndex")
                    .and_then(Value::as_str)
                    .and_then(|v| u32::from_str_radix(v.strip_prefix("0x")?, 16).ok())
                    .ok_or_else(|| GoFinError::validation("CCTP log index is malformed."))?;
                Ok(CanonicalRawLog {
                    log_index,
                    address,
                    topics,
                    data,
                })
            })
            .collect()
    }

    fn canonical_receipt(receipt: &CanonicalReceipt) -> Result<(), GoFinError> {
        let reported = receipt
            .receipt
            .get("transactionHash")
            .and_then(Value::as_str)
            .ok_or_else(|| GoFinError::validation("CCTP receipt omitted its transaction hash."))?;
        if !receipt.success
            || receipt.confirmations == 0
            || receipt.block_number == 0
            || gofin_evm::hash(reported)? != receipt.hash
        {
            return Err(GoFinError::validation(
                "CCTP destination receipt is unsuccessful, noncanonical, or below finality.",
            ));
        }
        Ok(())
    }

    fn proof(
        receipt: &CanonicalReceipt,
        message: &cctp::CctpMessage,
        domain: u32,
        recipient: &str,
        log_index: u32,
    ) -> Result<CctpDestinationProof, GoFinError> {
        let gross = message.amount.integer();
        let fee = message.fee_executed.integer();
        if fee > gross {
            return Err(GoFinError::validation("CCTP fee exceeds its gross amount."));
        }
        let net = gross - fee;
        Ok(CctpDestinationProof {
            transaction_hash: format!("{:#x}", receipt.hash),
            block_hash: format!("{:#x}", receipt.block_hash),
            block_number: receipt.block_number,
            confirmations: receipt.confirmations,
            destination_domain: domain,
            recipient: recipient.to_owned(),
            gross_atoms: message.amount.to_string(),
            fee_atoms: message.fee_executed.to_string(),
            net_atoms: net.to_string(),
            nonce: message.nonce.clone(),
            message_received_log_index: log_index,
        })
    }

    fn verify_hypercore_forward_logs(
        receipt: &CanonicalReceipt,
        proxy: &str,
        expected_net_atoms: u64,
        message_received_index: u32,
    ) -> Result<(), GoFinError> {
        const FORWARDER: &str = "0xb21d281dedb17ae5b501f6aa8256fe38c4e45757";
        const CORE_DEPOSIT_WALLET: &str = "0x6b9e773128f453f5c2c60935ee2de2cbc5390a24";
        const NATIVE_USDC: &str = "0xb88339cb7199b77e23db6e890353e22632ba630f";
        const CORE_SYSTEM: &str = "0x2000000000000000000000000000000000000000";
        const TRANSFER_TOPIC: &str =
            "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
        const FORWARD_TOPIC: &str =
            "0x7c77c294abdf00fb72b21272f440e7f664b15929dfe5e3689c33fb1456be0583";
        let logs = Self::logs(receipt)?;
        let word = |address: &str| -> Result<String, GoFinError> {
            let parsed = gofin_evm::address(address)?;
            Ok(format!(
                "0x{}{}",
                "00".repeat(12),
                hex::encode(parsed.as_slice())
            ))
        };
        gofin_evm::address(NATIVE_USDC)?;
        let transfer_matches = |emitter: &str, from: &str, to: &str| -> Vec<&CanonicalRawLog> {
            let from = word(from).ok();
            let to = word(to).ok();
            logs.iter()
                .filter(|log| {
                    log.address.eq_ignore_ascii_case(emitter)
                        && log
                            .topics
                            .first()
                            .is_some_and(|topic| topic.eq_ignore_ascii_case(TRANSFER_TOPIC))
                        && log.topics.len() == 3
                        && Some(&log.topics[1]) == from.as_ref()
                        && Some(&log.topics[2]) == to.as_ref()
                        && log.data.len() == 66
                        && log.data.starts_with("0x")
                        && U256::from_str_radix(&log.data[2..], 16).ok()
                            == Some(U256::from(expected_net_atoms))
                })
                .collect()
        };
        let forwarder_to_wallet = transfer_matches(NATIVE_USDC, FORWARDER, CORE_DEPOSIT_WALLET);
        let proxy_to_system = transfer_matches(CORE_DEPOSIT_WALLET, proxy, CORE_SYSTEM);
        let proxy_topic = word(proxy)?;
        let wallet_topic = word(CORE_DEPOSIT_WALLET)?;
        let usdc_topic = word(NATIVE_USDC)?;
        let forward_events = logs
            .iter()
            .filter(|log| {
                log.address.eq_ignore_ascii_case(FORWARDER)
                    && log.topics.len() == 4
                    && log.topics[0].eq_ignore_ascii_case(FORWARD_TOPIC)
                    && log.topics[1].eq_ignore_ascii_case(&proxy_topic)
                    && log.topics[2].eq_ignore_ascii_case(&wallet_topic)
                    && log.topics[3].eq_ignore_ascii_case(&usdc_topic)
                    && log.data.len() == 130
                    && log.data.starts_with("0x")
                    && U256::from_str_radix(&log.data[2..66], 16).ok() == Some(U256::from(u32::MAX))
                    && U256::from_str_radix(&log.data[66..130], 16).ok()
                        == Some(U256::from(expected_net_atoms))
            })
            .collect::<Vec<_>>();
        if forwarder_to_wallet.len() != 1
            || proxy_to_system.len() != 1
            || forward_events.len() != 1
            || forwarder_to_wallet[0].log_index <= message_received_index
            || proxy_to_system[0].log_index <= forwarder_to_wallet[0].log_index
            || forward_events[0].log_index <= proxy_to_system[0].log_index
        {
            return Err(GoFinError::validation(
                "CCTP forwarder did not prove the exact native-USDC Core spot deposit to the bound proxy.",
            ));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl PapertradeCctpSettlement for PapertradeCctpSettlementVerifier {
    async fn fetch_attestation(
        &self,
        source_domain: u32,
        source_tx_hash: &str,
    ) -> Result<cctp::CctpAttestationStatus, GoFinError> {
        self.provider
            .fetch_attestation(source_domain, source_tx_hash)
            .await
    }

    async fn verify_hypercore_outbound_solana(
        &self,
        quote: &cctp::CctpQuoteMetadata,
        source_transaction_hash: &str,
        attested_message: &str,
        attestation: &str,
        forward_tx_hash: &str,
        destination: &gofin_bridge::SolanaDestination,
        hook_owner: &str,
        amount_atoms: u64,
        core_nonce: u64,
        proof: &gofin_solana::FinalizedReceiptProof,
    ) -> Result<SolanaCctpDestinationProof, GoFinError> {
        let owner = gofin_evm::address(hook_owner)?;
        let message = cctp::hypercore::validate_outbound_solana_message(
            attested_message,
            quote,
            destination,
            owner,
            &Quantity::from_u64(amount_atoms),
            core_nonce,
        )?;
        let source_hash = gofin_evm::hash(source_transaction_hash)?;
        if source_hash == gofin_evm::alloy::primitives::B256::ZERO {
            return Err(GoFinError::validation(
                "Core CCTP source transaction hash is invalid.",
            ));
        }
        let attested = cctp::CctpAttestation {
            source_transaction_hash: format!("{source_hash:#x}"),
            message: attested_message.to_owned(),
            attestation: attestation.to_owned(),
            nonce: message.nonce.clone(),
            forward_tx_hash: Some(forward_tx_hash.to_owned()),
        };
        let state = self
            .provider
            .solana_state(999, destination, Some(&message.nonce))
            .await?;
        cctp::solana::verify_solana_attestation(attested_message, attestation, &state)?;
        let verified = cctp::solana::verify_solana_receipt(proof, destination, &attested)?;
        if proof.signature != forward_tx_hash {
            return Err(GoFinError::validation(
                "Canonical Solana receipt does not match Circle's completed-forward transaction hash.",
            ));
        }
        if verified.nonce != message.nonce || verified.fee_executed != message.fee_executed {
            return Err(GoFinError::validation(
                "Finalized Solana CCTP receipt differs from the exact attested message.",
            ));
        }
        let gross = message.amount.to_string();
        let fee = verified.fee_executed.to_string();
        Ok(SolanaCctpDestinationProof {
            signature: proof.signature.clone(),
            slot: proof.slot,
            destination_domain: cctp::solana::DOMAIN,
            recipient_token_account: destination.recipient_token_account.clone(),
            gross_atoms: gross,
            fee_atoms: fee,
            net_atoms: verified.net_amount.to_string(),
            nonce: message.nonce,
        })
    }

    /// Verify the destination-side proof for a CoreWriter CCTP withdrawal.
    /// The sibling route service independently binds the source action and
    /// ledger; this method proves Circle's exact attested message and canonical
    /// EVM mint receipt. Solana destination receipts use Solana's finalized
    /// receipt verifier instead.
    async fn verify_hypercore_outbound_evm(
        &self,
        quote: &cctp::CctpQuoteMetadata,
        attested_message: &str,
        attestation: &str,
        chain_id: u64,
        owner: &str,
        amount_atoms: u64,
        core_nonce: u64,
        receipt: &CanonicalReceipt,
    ) -> Result<CctpDestinationProof, GoFinError> {
        Self::canonical_receipt(receipt)?;
        let domain = cctp::domain(chain_id)?;
        if !matches!(domain, 0 | 3) {
            return Err(GoFinError::validation(
                "HyperCore outbound EVM settlement supports only Ethereum and Arbitrum receipts.",
            ));
        }
        self.provider
            .verify_attestation(chain_id, attested_message, attestation)
            .await?;
        let owner = gofin_evm::address(owner)?;
        let message = cctp::hypercore::validate_outbound_message(
            attested_message,
            quote,
            domain,
            owner,
            &Quantity::from_u64(amount_atoms),
            core_nonce,
        )?;
        let log_index = cctp::verify_destination_logs(&Self::logs(receipt)?, attested_message)?;
        Self::proof(receipt, &message, domain, &format!("{owner:#x}"), log_index)
    }

    async fn verify_hypercore_forward(
        &self,
        source_message: &str,
        quote: &cctp::CctpQuoteMetadata,
        attested_message: &str,
        attestation: &str,
        source_chain_id: u64,
        source_owner: &str,
        amount_atoms: u64,
        proxy: &str,
        receipt: &CanonicalReceipt,
    ) -> Result<CctpDestinationProof, GoFinError> {
        Self::canonical_receipt(receipt)?;
        self.provider
            .verify_attestation(999, attested_message, attestation)
            .await?;
        let owner = gofin_evm::address(source_owner)?;
        let proxy = gofin_evm::address(proxy)?;
        let amount = Quantity::from_u64(amount_atoms);
        let message = cctp::hypercore::validate_attested_message(
            source_message,
            quote,
            source_chain_id,
            owner,
            &amount,
            proxy,
            attested_message,
        )?;
        let recipient = cctp::hypercore::FORWARDER;
        let log_index = cctp::verify_destination_logs(&Self::logs(receipt)?, attested_message)?;
        let gross = message.amount.integer();
        let fee = message.fee_executed.integer();
        if fee > gross {
            return Err(GoFinError::validation("CCTP fee exceeds its gross amount."));
        }
        let net = gross - fee;
        let net_atoms = u64::try_from(net)
            .map_err(|_| GoFinError::validation("CCTP net amount exceeds supported range."))?;
        Self::verify_hypercore_forward_logs(receipt, &format!("{proxy:#x}"), net_atoms, log_index)?;
        Self::proof(
            receipt,
            &message,
            cctp::hypercore::DOMAIN,
            recipient,
            log_index,
        )
    }

    async fn verify_solana_hypercore_forward(
        &self,
        source_message: &str,
        request: &cctp::solana::hypercore_source::BurnRequest,
        quote: &cctp::CctpQuoteMetadata,
        attested_message: &str,
        attestation: &str,
        proxy: &str,
        receipt: &CanonicalReceipt,
    ) -> Result<CctpDestinationProof, GoFinError> {
        Self::canonical_receipt(receipt)?;
        self.provider
            .verify_attestation(999, attested_message, attestation)
            .await?;
        let proxy_address = gofin_evm::address(proxy)?;
        let (gross, fee) = cctp::solana::hypercore_source::validate_attested_message(
            source_message,
            attested_message,
            request,
            quote,
            proxy_address,
        )?;
        let message = cctp::decode_message(attested_message)?;
        if gross != request.amount_atoms || fee > request.max_fee_atoms {
            return Err(GoFinError::validation(
                "Solana CCTP attested amount or fee differs from the approved source burn.",
            ));
        }
        let logs = Self::logs(receipt)?;
        let log_index = cctp::verify_destination_logs(&logs, attested_message)?;
        let net = gross
            .checked_sub(fee)
            .ok_or_else(|| GoFinError::validation("CCTP fee exceeds its gross amount."))?;
        Self::verify_hypercore_forward_logs(receipt, proxy, net, log_index)?;
        Self::proof(
            receipt,
            &message,
            cctp::hypercore::DOMAIN,
            cctp::hypercore::FORWARDER,
            log_index,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PROXY: &str = "0x3bd71e4e21e2097124a95812c0fc7a122a014578";
    const FORWARDER: &str = "0xb21d281dedb17ae5b501f6aa8256fe38c4e45757";
    const CORE_DEPOSIT_WALLET: &str = "0x6b9e773128f453f5c2c60935ee2de2cbc5390a24";
    const NATIVE_USDC: &str = "0xb88339cb7199b77e23db6e890353e22632ba630f";
    const CORE_SYSTEM: &str = "0x2000000000000000000000000000000000000000";
    const TRANSFER_TOPIC: &str =
        "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
    const FORWARD_TOPIC: &str =
        "0x7c77c294abdf00fb72b21272f440e7f664b15929dfe5e3689c33fb1456be0583";

    fn address_topic(address: &str) -> String {
        format!("0x{}{}", "00".repeat(12), address[2..].to_ascii_lowercase())
    }

    fn log(address: &str, topics: Vec<String>, data: String, index: u64) -> Value {
        json!({
            "address":address,
            "topics":topics,
            "data":data,
            "logIndex":format!("0x{index:x}")
        })
    }

    fn core_forward_receipt(amount: u64) -> CanonicalReceipt {
        let amount_word = format!("{:064x}", amount);
        let transfer = |emitter: &str, from: &str, to: &str, index| {
            log(
                emitter,
                vec![
                    TRANSFER_TOPIC.into(),
                    address_topic(from),
                    address_topic(to),
                ],
                format!("0x{amount_word}"),
                index,
            )
        };
        let logs = vec![
            transfer(NATIVE_USDC, FORWARDER, CORE_DEPOSIT_WALLET, 6),
            transfer(CORE_DEPOSIT_WALLET, PROXY, CORE_SYSTEM, 7),
            log(
                FORWARDER,
                vec![
                    FORWARD_TOPIC.into(),
                    address_topic(PROXY),
                    address_topic(CORE_DEPOSIT_WALLET),
                    address_topic(NATIVE_USDC),
                ],
                format!("0x{:064x}{amount_word}", u32::MAX),
                8,
            ),
        ];
        let hash = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        CanonicalReceipt {
            hash: gofin_evm::hash(hash).unwrap(),
            transaction_sender: FORWARDER.into(),
            block_hash: gofin_evm::hash(
                "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            )
            .unwrap(),
            block_number: 10,
            confirmations: 5,
            success: true,
            fee: U256::ZERO,
            receipt: json!({"transactionHash":hash,"logs":logs}),
        }
    }

    #[test]
    fn hypercore_forward_requires_exact_core_wallet_and_proxy_events() {
        let receipt = core_forward_receipt(999_800_000);
        assert!(
            PapertradeCctpSettlementVerifier::verify_hypercore_forward_logs(
                &receipt,
                PROXY,
                999_800_000,
                5,
            )
            .is_ok()
        );
        assert!(
            PapertradeCctpSettlementVerifier::verify_hypercore_forward_logs(
                &receipt,
                "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                999_800_000,
                5,
            )
            .is_err()
        );
        assert!(
            PapertradeCctpSettlementVerifier::verify_hypercore_forward_logs(
                &receipt,
                PROXY,
                999_800_001,
                5,
            )
            .is_err()
        );
        assert!(
            PapertradeCctpSettlementVerifier::verify_hypercore_forward_logs(
                &receipt,
                PROXY,
                999_800_000,
                8,
            )
            .is_err()
        );
    }
}
