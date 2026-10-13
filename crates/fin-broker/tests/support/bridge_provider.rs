//! Synthetic protocol candidates only: no HTTP, signing, credentials or fills.
use async_trait::async_trait;
use fin_bridge::*;
use fin_domain::{Quantity, Result, now};
use fin_evm::alloy::sol_types::SolCall;
use std::{
    collections::BTreeSet,
    sync::atomic::{AtomicUsize, Ordering},
};
const DEPOSITORY: &str = "0x4cd00e387622c35bddb9b4c962c136462338bc31";
const SOLVER: &str = "0xf70da97812cb96acdf810712aa562db8dfa3dbef";
fn q(s: &str) -> Quantity {
    Quantity::new(s).unwrap()
}
fn request_id() -> String {
    format!("0x{}", "55".repeat(32))
}
fn tx_hash() -> String {
    format!("0x{}", "77".repeat(32))
}
fin_evm::alloy::sol! { interface SyntheticRelayDepository { function depositNative(address depositor, bytes32 id) external payable; } }
pub(crate) struct SyntheticProvider {
    pub(crate) revisions: AtomicUsize,
}
#[async_trait]
impl BridgeProvider for SyntheticProvider {
    fn capabilities(&self) -> BridgeCapabilities {
        BridgeCapabilities {
            provider: "relay".into(),
            chains: BTreeSet::from([1, 42161]),
            native_eth: true,
            erc20: false,
            protocol_versions: BTreeSet::from(["v2".into()]),
        }
    }
    async fn quote(&self, r: &BridgeQuoteRequest) -> Result<BridgeQuoteBatch> {
        let revision = self.revisions.fetch_add(1, Ordering::SeqCst) + 1;
        let order = format!("0x{revision:064x}");
        let destination = &r.intent.destination_assets[0];
        let deposit = BridgeStep {
            ordinal: 0,
            kind: BridgeStepKind::Deposit,
            solana_mint: None,
            call: Some(EvmCall {
                chain_id: 1,
                to: DEPOSITORY.into(),
                value: r.intent.amount.clone(),
                data: format!(
                    "0x{}",
                    hex::encode(
                        SyntheticRelayDepository::depositNativeCall {
                            depositor: fin_evm::address(&r.intent.source_address)?,
                            id: fin_evm::hash(&order)?
                        }
                        .abi_encode()
                    )
                ),
            }),
            expected_sender: r.intent.source_address.clone(),
            maximum_debit: vec![r.intent.source.amount(r.intent.amount.clone())],
            dependencies: vec![],
        };
        Ok(BridgeQuoteBatch {
            failures: vec![],
            quotes: vec![BridgeQuote {
                cctp: None,
                quote_id: format!("synthetic-{revision}"),
                provider: "relay".into(),
                route_id: "synthetic-fixed-route".into(),
                intent: r.intent.clone(),
                gross_debit: r.intent.source.amount(r.intent.amount.clone()),
                expected_receipt: destination.amount(q("4490000000000000")),
                minimum_receipt: destination.amount(q("4480000000000000")),
                fees: vec![],
                approval: None,
                protocol_steps: vec![deposit],
                estimated_arrival_seconds: Some(1),
                observed_at: now(),
                valid_until: (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339(),
                class: QuoteClass::Executable,
                refund_paths: vec![RefundPath {
                    chain_id: 1,
                    asset: r.intent.source.asset_id(),
                    recipient: r.intent.refund_to.clone(),
                    minimum: q("1"),
                    maximum: r.intent.amount.clone(),
                }],
                order_solver: Some(SOLVER.into()),
                protocol_order_id: Some(order),
                provider_request_id: Some(request_id()),
            }],
        })
    }
    fn prepare(
        &self,
        quote: &BridgeQuote,
        bounds: &BridgeBounds,
        _: &str,
    ) -> Result<PreparedBridge> {
        Ok(PreparedBridge {
            quote: quote.clone(),
            bounds: bounds.clone(),
            steps: quote.protocol_steps.clone(),
            source_order_id: quote.protocol_order_id.clone(),
            fixed_scope_digest: "synthetic-fixed-digest".into(),
        })
    }
    async fn reconcile(&self, t: &BridgeTransferRef) -> Result<BridgeReconciliation> {
        Ok(BridgeReconciliation {
            status: "success".into(),
            failure: None,
            refund_fail_reason: None,
            candidates: vec![BridgeEvidenceCandidate {
                order_id: t.order_id.clone(),
                kind: EvidenceKind::DestinationFill,
                chain_id: 42161,
                transaction_hash: tx_hash(),
                correlation: t.provider_request_id.clone(),
            }],
        })
    }
}
