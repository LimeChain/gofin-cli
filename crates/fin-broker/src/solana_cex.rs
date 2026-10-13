//! Composite recipient proof observer for Binance and other CEX withdrawals.
//!
//! EVM routing remains delegated to the existing observer. Binance's canonical
//! `SOL` route is independently resolved to exact Solana asset identities and
//! checked against finalized transaction effects.

use async_trait::async_trait;
use fin_binance::{BinanceRecipientVerifier, PayoutRequest, PayoutResult};
use fin_domain::{AssetId, FinError, Quantity, Status};
use fin_solana::{
    NETWORK, SolanaWalletAdapter,
    funding::{AmountBounds, ExternalPayoutRequest, ExternalPayoutStatus, resolve_external_asset},
};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};

pub struct SolanaCexPayoutVerifier {
    evm: Option<Arc<dyn BinanceRecipientVerifier>>,
    observers: BTreeMap<String, Arc<SolanaWalletAdapter>>,
}

impl SolanaCexPayoutVerifier {
    pub fn new(
        evm: Option<Arc<dyn BinanceRecipientVerifier>>,
        observers: BTreeMap<String, Arc<SolanaWalletAdapter>>,
    ) -> Result<Self, FinError> {
        if observers.is_empty() && evm.is_none() {
            return Err(FinError::new(
                "PAYOUT_RPC_MISSING",
                "configuration",
                "No configured Solana observer is available for payout verification.",
                "Configure a protected Solana mainnet-beta wallet observer.",
            ));
        }
        Ok(Self { evm, observers })
    }

    fn runtime(&self) -> Option<Arc<fin_solana::SolanaRuntime>> {
        self.observers
            .values()
            .next()
            .map(|observer| observer.runtime())
    }
}

#[async_trait]
impl BinanceRecipientVerifier for SolanaCexPayoutVerifier {
    fn supports_chain(&self, chain_id: u64) -> bool {
        self.evm
            .as_ref()
            .is_some_and(|evm| evm.supports_chain(chain_id))
    }

    async fn resolve_asset(
        &self,
        chain_id: u64,
        coin: &str,
        contract: Option<&str>,
    ) -> Result<Option<AssetId>, FinError> {
        match &self.evm {
            Some(evm) => evm.resolve_asset(chain_id, coin, contract).await,
            None => Ok(None),
        }
    }

    fn supports_asset(&self, network: &str, asset: &AssetId) -> bool {
        if fin_binance::binance_solana_network(network) {
            return asset.network == NETWORK
                && ((asset.reference == "native" && asset.decimals == 9)
                    || (asset.reference == fin_solana::funding::MAINNET_USDC_MINT
                        && asset.decimals == 6));
        }
        self.evm
            .as_ref()
            .is_some_and(|evm| evm.supports_asset(network, asset))
    }

    fn valid_recipient(&self, network: &str, recipient: &str) -> bool {
        if fin_binance::binance_solana_network(network) {
            fin_solana::parse_address(recipient).is_ok()
        } else {
            self.evm
                .as_ref()
                .is_some_and(|evm| evm.valid_recipient(network, recipient))
        }
    }

    async fn resolve_route_asset(
        &self,
        network: &str,
        coin: &str,
        contract: Option<&str>,
    ) -> Result<Option<AssetId>, FinError> {
        if fin_binance::binance_solana_network(network) {
            if coin != "SOL" && coin != "USDC" {
                return Ok(None);
            }
            let Some(runtime) = self.runtime() else {
                return Ok(None);
            };
            let asset = resolve_external_asset(&runtime, coin, contract).await?;
            return Ok(Some(asset));
        }
        match &self.evm {
            Some(evm) => evm.resolve_route_asset(network, coin, contract).await,
            None => Ok(None),
        }
    }

    async fn verify(&self, request: PayoutRequest) -> PayoutResult {
        if !fin_binance::binance_solana_network(&request.network) {
            return match &self.evm {
                Some(evm) => evm.verify(request).await,
                None => PayoutResult {
                    status: Status::Pending,
                    amount_atoms: None,
                    evidence: json!({"state":"unverified","reason":"No observer is configured for this network."}),
                },
            };
        }
        let fail = |reason: &str| PayoutResult {
            status: Status::Pending,
            amount_atoms: None,
            evidence: json!({"network":"SOL","state":"unverified","reason":reason}),
        };
        if request.asset.network != NETWORK {
            return fail("Asset identity is not canonical Solana mainnet-beta.");
        }
        let Some(runtime) = self.runtime() else {
            return fail("No Solana observer is configured.");
        };
        let Some(minimum) = to_atoms(&request.minimum_amount, request.asset.decimals) else {
            return fail("Minimum payout bound is invalid.");
        };
        let Some(maximum) = to_atoms(&request.maximum_amount, request.asset.decimals) else {
            return fail("Maximum payout bound is invalid.");
        };
        let recipient = match fin_solana::parse_address(&request.recipient) {
            Ok(v) => v,
            Err(_) => return fail("Recipient is not an exact Solana address."),
        };
        let payout = ExternalPayoutRequest {
            signature: request.tx_hash.clone(),
            asset: request.asset.clone(),
            recipient: recipient.clone(),
            recipient_owner: None,
            source_owner: None,
            amount: AmountBounds {
                minimum_atoms: minimum,
                maximum_atoms: maximum,
            },
        };
        match runtime.verify_external_payout(&payout).await {
            Ok(ExternalPayoutStatus::Finalized { evidence }) => PayoutResult {
                status: Status::Completed,
                amount_atoms: Some(Quantity::from_u64(evidence.credited_atoms)),
                evidence: json!({"network":"SOL","state":"finalized","signature":evidence.signature,"slot":evidence.slot,"asset":evidence.asset,"recipient":recipient,"recipientAccounts":evidence.recipient_accounts,"creditedAtoms":evidence.credited_atoms,"debitedAtoms":evidence.debited_atoms,"transferInstructionCount":evidence.transfer_instruction_count}),
            },
            Ok(ExternalPayoutStatus::Pending { reason }) => fail(&reason),
            Err(error) => fail(&error.message),
        }
    }
}

fn to_atoms(decimal: &str, decimals: u8) -> Option<u64> {
    Quantity::from_decimal(decimal, decimals)
        .ok()?
        .as_str()
        .parse()
        .ok()
}
