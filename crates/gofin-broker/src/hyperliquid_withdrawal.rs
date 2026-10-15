//! Proof observer for Hyperliquid's fixed Arbitrum One Bridge2 withdrawals.
//!
//! A venue response or balance movement is never settlement evidence. This
//! observer requires both Bridge2 request/finalization events, a matching
//! USDC transfer, and canonical sequencer-included receipts for the event transactions.

use gofin_domain::{GoFinError, Quantity};
use gofin_ethereum::EthereumAdapter;
use gofin_evm::alloy::{
    primitives::{Address, B256, U256},
    sol_types::SolEvent,
};
use gofin_evm::{alloy::sol, rpc_u64, u256};
use gofin_hyperliquid::account_actions::{
    HyperliquidWithdrawalProof, WithdrawalProofRequest, WithdrawalReceiptProof,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

const ARBITRUM_ONE: u64 = 42161;
const BRIDGE2: &str = "0x2df1c51e09aecf9cacb7bc98cb1742757f163df7";
const USDC: &str = "0xaf88d065e77c8cc2239327c5edb3a432268e5831";

sol! {
    interface HyperliquidBridge2 {
        event RequestedWithdrawal(
            address indexed user,
            address destination,
            uint64 usd,
            uint64 nonce,
            bytes32 message,
            uint64 requestedTime
        );
        event FinalizedWithdrawal(
            address indexed user,
            address destination,
            uint64 usd,
            uint64 nonce,
            bytes32 message
        );
    }
    interface HyperliquidUsdc {
        event Transfer(address indexed from, address indexed to, uint256 value);
    }
}

#[derive(Clone)]
pub struct HyperliquidArbitrumWithdrawalProof {
    ethereum: Arc<EthereumAdapter>,
}

impl HyperliquidArbitrumWithdrawalProof {
    pub fn new(ethereum: Arc<EthereumAdapter>) -> Self {
        Self { ethereum }
    }

    fn check_request(&self, request: &WithdrawalProofRequest) -> Result<(), GoFinError> {
        if self.ethereum.config.chain_id != ARBITRUM_ONE
            || !request.token.eq_ignore_ascii_case(USDC)
            || request.owner.len() != 42
            || request.destination.len() != 42
            || !request.owner.starts_with("0x")
            || !request.destination.starts_with("0x")
            || request.native_tx_hash.parse::<B256>().is_err()
            || request.ledger_hash.parse::<B256>().is_err()
            || request.nonce_ms == 0
            || request.nonce_ms.checked_mul(1000) != Some(request.ledger_nonce)
            || request.net_amount.is_zero()
            || request.net_amount > request.gross_amount
            || u256(&request.gross_amount).is_err()
            || request.gross_amount.is_zero()
        {
            return Err(GoFinError::new(
                "HYPERLIQUID_WITHDRAWAL_PROOF_REQUEST_INVALID",
                "reconciliation",
                "Withdrawal proof request does not identify a valid Arbitrum USDC action.",
                "Inspect the original withdrawal record; do not submit it again.",
            ));
        }
        Ok(())
    }

    async fn sequenced_receipt(&self, hash: &str) -> Result<Option<Value>, GoFinError> {
        let observed = self
            .ethereum
            .runtime
            .observe_receipt(hash, 1, false)
            .await?;
        let Some(observed) = observed else {
            return Ok(None);
        };
        if !observed.success {
            return Ok(None);
        }
        Ok(Some(observed.receipt))
    }
}

#[async_trait::async_trait]
impl HyperliquidWithdrawalProof for HyperliquidArbitrumWithdrawalProof {
    async fn prove_sequenced_arbitrum_withdrawal(
        &self,
        request: &WithdrawalProofRequest,
    ) -> Result<Option<WithdrawalReceiptProof>, GoFinError> {
        self.check_request(request)?;

        // The ledger identifies the payout transaction. Its canonical receipt
        // bounds every historical query, including recovery long after approval.
        let Some(final_receipt) = self.sequenced_receipt(&request.ledger_hash).await? else {
            return Ok(None);
        };
        let end_block = rpc_u64(&final_receipt["blockNumber"])?;
        // The signed action uses milliseconds; the ledger/Bridge2 nonce uses
        // microseconds. The timestamp narrows candidates, never proves a payout.
        let bridge = gofin_evm::address(BRIDGE2)?;
        let usdc = gofin_evm::address(USDC)?;
        let owner = gofin_evm::address(&request.owner)?;
        let destination = gofin_evm::address(&request.destination)?;
        let start_block = self
            .start_block(request.nonce_ms.saturating_sub(600_000) / 1000, end_block)
            .await?;
        let topics = vec![
            Some(format!(
                "{:#x}",
                HyperliquidBridge2::RequestedWithdrawal::SIGNATURE_HASH
            )),
            Some(withdrawal_owner_topic(owner)),
        ];
        let candidates = self.logs(bridge, start_block, end_block, topics).await?;
        let net_micro = native_to_micro(&request.net_amount)?;

        let mut matching_requests = Vec::new();
        for candidate in candidates {
            let Some(event) =
                decode_event::<HyperliquidBridge2::RequestedWithdrawal>(&candidate, bridge)?
            else {
                continue;
            };
            if requested_event_matches(&event, owner, destination, request.ledger_nonce, net_micro)
            {
                matching_requests.push((candidate, event));
            }
        }
        if matching_requests.is_empty() {
            return Ok(None);
        }
        if matching_requests.len() != 1 {
            return Err(proof_error(
                "More than one Bridge2 request matches this withdrawal nonce.",
            ));
        }
        let (request_log, requested) = matching_requests.remove(0);
        let request_hash = request_log["transactionHash"]
            .as_str()
            .ok_or_else(|| proof_error("Bridge2 request log lacks a transaction hash."))?;
        let Some(request_receipt) = self.sequenced_receipt(request_hash).await? else {
            return Ok(None);
        };
        require_exact_event::<HyperliquidBridge2::RequestedWithdrawal>(
            &request_receipt,
            bridge,
            |event| same_request(event, &requested),
        )?;

        let final_topics = vec![
            Some(format!(
                "{:#x}",
                HyperliquidBridge2::FinalizedWithdrawal::SIGNATURE_HASH
            )),
            Some(withdrawal_owner_topic(owner)),
        ];
        let finalized_logs = self
            .logs(bridge, start_block, end_block, final_topics)
            .await?;
        let mut matching_finalizations = Vec::new();
        for candidate in finalized_logs {
            let Some(event) =
                decode_event::<HyperliquidBridge2::FinalizedWithdrawal>(&candidate, bridge)?
            else {
                continue;
            };
            if event.user == requested.user
                && event.destination == requested.destination
                && event.usd == requested.usd
                && event.nonce == requested.nonce
                && event.message == requested.message
            {
                matching_finalizations.push((candidate, event));
            }
        }
        if matching_finalizations.is_empty() {
            return Ok(None);
        }
        if matching_finalizations.len() != 1 {
            return Err(proof_error(
                "More than one Bridge2 finalization matches the exact request event.",
            ));
        }
        let (final_log, finalized) = matching_finalizations.remove(0);
        let final_hash = final_log["transactionHash"]
            .as_str()
            .ok_or_else(|| proof_error("Bridge2 finalization log lacks a transaction hash."))?;
        if !final_hash.eq_ignore_ascii_case(&request.ledger_hash) {
            return Err(proof_error(
                "Bridge2 payout transaction differs from the exact native withdrawal ledger hash.",
            ));
        }
        require_exact_event::<HyperliquidBridge2::FinalizedWithdrawal>(
            &final_receipt,
            bridge,
            |event| same_finalization(event, &finalized),
        )?;
        let matching_transfers = transfer_amounts(&final_receipt, usdc, bridge, destination)?;
        if matching_transfers.len() != 1 || matching_transfers[0] != U256::from(finalized.usd) {
            return Err(proof_error(
                "Finalized Bridge2 event is not backed by one exact USDC transfer to the reviewed destination.",
            ));
        }

        // Payout proofs retain Arbitrum USDC's native six-decimal atoms.
        // The adapter compares these to its 36-decimal venue ledger exactly.
        let event_digest = event_digest(request, final_hash, &requested, &finalized);
        Ok(Some(WithdrawalReceiptProof {
            transaction_hash: final_hash.to_ascii_lowercase(),
            sender: BRIDGE2.to_owned(),
            recipient: request.destination.to_ascii_lowercase(),
            token: USDC.into(),
            net_amount: gofin_evm::quantity(U256::from(finalized.usd)),
            sequenced: true,
            block_number: rpc_u64(&final_receipt["blockNumber"])?,
            event_digest,
        }))
    }
}

impl HyperliquidArbitrumWithdrawalProof {
    async fn start_block(&self, timestamp: u64, end_block: u64) -> Result<u64, GoFinError> {
        // Binary-search canonical block timestamps to avoid an unbounded
        // from-genesis log query. The timestamp only narrows the RPC search;
        // it is not accepted as proof of the withdrawal.
        let latest_value = self
            .ethereum
            .runtime
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([format!("{end_block:#x}"), false]),
            )
            .await?;
        let latest = rpc_u64(&latest_value["number"])?;
        let latest_time = rpc_u64(&latest_value["timestamp"])?;
        if latest_time <= timestamp {
            return Ok(latest);
        }
        let mut low = 0u64;
        let mut high = latest;
        while low < high {
            let mid = low + (high - low) / 2;
            let block = self
                .ethereum
                .runtime
                .rpc
                .request("eth_getBlockByNumber", json!([format!("{mid:#x}"), false]))
                .await?;
            if block.is_null() {
                return Err(proof_error(
                    "RPC could not resolve a canonical timestamp-search block.",
                ));
            }
            if rpc_u64(&block["timestamp"])? < timestamp {
                low = mid.saturating_add(1);
            } else {
                high = mid;
            }
        }
        Ok(low.saturating_sub(1))
    }

    async fn logs(
        &self,
        address: Address,
        from_block: u64,
        to_block: u64,
        topics: Vec<Option<String>>,
    ) -> Result<Vec<Value>, GoFinError> {
        let topic_values = topics
            .into_iter()
            .map(|value| value.map_or(Value::Null, Value::String))
            .collect::<Vec<_>>();
        if from_block > to_block {
            return Err(proof_error(
                "Arbitrum withdrawal search starts after its verified payout block.",
            ));
        }
        // Disjoint bounded pages stay below public RPC range limits. A later
        // status check uses the same payout block instead of an expanding latest.
        let mut rows = Vec::new();
        let mut page_start = from_block;
        while page_start <= to_block {
            let page_end = page_start.saturating_add(511).min(to_block);
            let result = self.ethereum.runtime.rpc.request(
                "eth_getLogs",
                json!([{"address":gofin_evm::lower_address(address),"fromBlock":format!("{page_start:#x}"),"toBlock":format!("{page_end:#x}"),"topics":topic_values}]),
            ).await?;
            let page = result.as_array().ok_or_else(|| {
                proof_error("Arbitrum RPC returned an invalid event-log response.")
            })?;
            rows.extend(page.iter().cloned());
            if page_end == to_block {
                break;
            }
            page_start = page_end
                .checked_add(1)
                .ok_or_else(|| proof_error("Arbitrum log page overflow."))?;
        }
        Ok(rows)
    }
}

fn withdrawal_owner_topic(owner: Address) -> String {
    // Address's fixed-byte formatter ignores width. ABI indexed addresses are
    // explicitly left-padded to one 32-byte word, rather than formatted as 20 bytes.
    format!("{:#x}", owner.into_word())
}

fn decode_event<E: SolEvent>(
    log: &Value,
    expected_emitter: Address,
) -> Result<Option<E>, GoFinError> {
    if gofin_evm::address(log["address"].as_str().unwrap_or(""))? != expected_emitter {
        return Ok(None);
    }
    let topics = log["topics"]
        .as_array()
        .ok_or_else(|| proof_error("Arbitrum event has invalid topics."))?
        .iter()
        .map(|value| gofin_evm::hash(value.as_str().unwrap_or("")))
        .collect::<Result<Vec<B256>, _>>()?;
    if topics.first() != Some(&E::SIGNATURE_HASH) {
        return Ok(None);
    }
    let data = gofin_evm::rpc_bytes(&log["data"])?;
    let event = E::decode_raw_log_validate(topics.iter().copied(), &data)
        .map_err(|_| proof_error("Arbitrum event does not match the reviewed Bridge2 ABI."))?;
    Ok(Some(event))
}

fn require_exact_event<E: SolEvent>(
    receipt: &Value,
    emitter: Address,
    matches_expected: impl Fn(&E) -> bool,
) -> Result<(), GoFinError> {
    let logs = receipt["logs"]
        .as_array()
        .ok_or_else(|| proof_error("Canonical receipt lacks a log list."))?;
    let mut count = 0usize;
    for log in logs {
        let log_matches_receipt = log["transactionHash"] == receipt["transactionHash"]
            && log["blockHash"] == receipt["blockHash"]
            && log["blockNumber"] == receipt["blockNumber"]
            && log["removed"].as_bool() != Some(true);
        if !log_matches_receipt {
            continue;
        }
        if let Some(event) = decode_event::<E>(log, emitter)?
            && matches_expected(&event)
        {
            count += 1;
        }
    }
    if count != 1 {
        return Err(proof_error(
            "Canonical receipt does not contain exactly one matching Bridge2 event.",
        ));
    }
    Ok(())
}

fn same_request(
    actual: &HyperliquidBridge2::RequestedWithdrawal,
    expected: &HyperliquidBridge2::RequestedWithdrawal,
) -> bool {
    actual.user == expected.user
        && actual.destination == expected.destination
        && actual.usd == expected.usd
        && actual.nonce == expected.nonce
        && actual.message == expected.message
}

fn requested_event_matches(
    event: &HyperliquidBridge2::RequestedWithdrawal,
    owner: Address,
    destination: Address,
    ledger_nonce: u64,
    net_micro: U256,
) -> bool {
    event.user == owner
        && event.destination == destination
        && event.nonce == ledger_nonce
        && event.usd > 0
        && U256::from(event.usd) == net_micro
}

fn same_finalization(
    actual: &HyperliquidBridge2::FinalizedWithdrawal,
    expected: &HyperliquidBridge2::FinalizedWithdrawal,
) -> bool {
    actual.user == expected.user
        && actual.destination == expected.destination
        && actual.usd == expected.usd
        && actual.nonce == expected.nonce
        && actual.message == expected.message
}

fn transfer_amounts(
    receipt: &Value,
    token: Address,
    from: Address,
    to: Address,
) -> Result<Vec<U256>, GoFinError> {
    let logs = receipt["logs"]
        .as_array()
        .ok_or_else(|| proof_error("Canonical receipt lacks a log list."))?;
    let mut values = Vec::new();
    for log in logs {
        if log["transactionHash"] != receipt["transactionHash"]
            || log["blockHash"] != receipt["blockHash"]
            || log["blockNumber"] != receipt["blockNumber"]
            || log["removed"].as_bool() == Some(true)
        {
            continue;
        }
        let Some(event) = decode_event::<HyperliquidUsdc::Transfer>(log, token)? else {
            continue;
        };
        if event.from == from && event.to == to {
            values.push(event.value);
        }
    }
    Ok(values)
}

fn event_digest(
    request: &WithdrawalProofRequest,
    payout_hash: &str,
    requested: &HyperliquidBridge2::RequestedWithdrawal,
    finalized: &HyperliquidBridge2::FinalizedWithdrawal,
) -> String {
    let stable = format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        request.native_tx_hash.to_ascii_lowercase(),
        request.owner.to_ascii_lowercase(),
        request.destination.to_ascii_lowercase(),
        request.nonce_ms,
        requested.usd,
        format_args!("{:#x}", requested.message),
        payout_hash.to_ascii_lowercase(),
        finalized.usd,
        request.gross_amount,
        request.ledger_nonce,
        request.ledger_hash.to_ascii_lowercase(),
        request.net_amount,
    );
    format!("0x{}", hex::encode(Sha256::digest(stable.as_bytes())))
}

fn proof_error(message: &str) -> GoFinError {
    GoFinError::new(
        "HYPERLIQUID_WITHDRAWAL_PROOF_INVALID",
        "reconciliation",
        message,
        "Inspect the original withdrawal evidence; never resubmit an unresolved withdrawal.",
    )
}

fn native_to_micro(amount: &Quantity) -> Result<U256, GoFinError> {
    let native = u256(amount)?;
    let scale = scale_6_to_36();
    if native.is_zero() || native % scale != U256::ZERO {
        return Err(proof_error(
            "Native USDC amount is not exactly representable in Arbitrum micro-units.",
        ));
    }
    Ok(native / scale)
}

fn scale_6_to_36() -> U256 {
    U256::from(10).pow(U256::from(30))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SequencedRpc {
        reorg: bool,
        success: bool,
        calls: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl gofin_evm::EvmRpc for SequencedRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value, GoFinError> {
            self.calls.lock().unwrap().push(method.to_owned());
            let tx_hash = format!("0x{}", "11".repeat(32));
            let block_hash = format!("0x{}", "22".repeat(32));
            Ok(match method {
                "eth_chainId" => json!("0xa4b1"),
                "eth_getTransactionReceipt" => json!({
                    "transactionHash":tx_hash,"blockHash":block_hash,"blockNumber":"0x10",
                    "status":if self.success { "0x1" } else { "0x0" },
                    "gasUsed":"0x5208","effectiveGasPrice":"0x2","logs":[]
                }),
                "eth_getTransactionByHash" => json!({
                    "hash":tx_hash,"blockHash":block_hash,"blockNumber":"0x10","from":BRIDGE2
                }),
                "eth_getBlockByNumber" => {
                    assert_ne!(
                        params[0],
                        json!("finalized"),
                        "Rollup settlement must not query Ethereum finality"
                    );
                    json!({"number":"0x10","hash":if self.reorg {format!("0x{}","33".repeat(32))} else {block_hash}})
                }
                "eth_blockNumber" => json!("0x10"),
                _ => panic!("Unexpected proof request: {method}"),
            })
        }
    }

    #[tokio::test]
    async fn withdrawal_observer_accepts_one_sequenced_block_and_rejects_revert_or_reorg() {
        for (reorg, success, accepted) in [
            (false, true, true),
            (false, false, false),
            (true, true, false),
        ] {
            let rpc = Arc::new(SequencedRpc {
                reorg,
                success,
                calls: Default::default(),
            });
            let ethereum = Arc::new(
                EthereumAdapter::new(
                    gofin_ethereum::EthereumConfig::wallet_only(ARBITRUM_ONE, 1, vec![]),
                    vec![gofin_ethereum::WalletConnection {
                        name: "synthetic".into(),
                        address: "0x1111111111111111111111111111111111111111".into(),
                        signing_enabled: false,
                    }],
                    Arc::new(gofin_evm::EvmRuntime::new(rpc.clone(), ARBITRUM_ONE, None)),
                )
                .unwrap(),
            );
            let observer = HyperliquidArbitrumWithdrawalProof::new(ethereum);
            let receipt = observer
                .sequenced_receipt(&format!("0x{}", "11".repeat(32)))
                .await
                .unwrap();
            assert_eq!(receipt.is_some(), accepted);
            assert!(
                rpc.calls
                    .lock()
                    .unwrap()
                    .contains(&"eth_getBlockByNumber".into())
            );
        }
    }

    fn request() -> WithdrawalProofRequest {
        WithdrawalProofRequest {
            owner: "0x1111111111111111111111111111111111111111".into(),
            destination: "0x2222222222222222222222222222222222222222".into(),
            token: USDC.into(),
            gross_amount: Quantity::new("1000000000000000000000000000000000000000").unwrap(),
            nonce_ms: 1_700_000_000_000,
            ledger_nonce: 1_700_000_000_000_000,
            ledger_hash: format!("0x{}", "cd".repeat(32)),
            net_amount: Quantity::new("995000000000000000000000000000000000000").unwrap(),
            native_tx_hash: format!("0x{}", "ab".repeat(32)),
        }
    }

    #[test]
    fn bridge2_events_are_distinct_from_native_action_hash() {
        let request = request();
        let message = B256::repeat_byte(0x33);
        let requested = HyperliquidBridge2::RequestedWithdrawal {
            user: gofin_evm::address(&request.owner).unwrap(),
            destination: gofin_evm::address(&request.destination).unwrap(),
            usd: 995_000_000,
            nonce: request.ledger_nonce,
            message,
            requestedTime: request.nonce_ms,
        };
        let finalized = HyperliquidBridge2::FinalizedWithdrawal {
            user: requested.user,
            destination: requested.destination,
            usd: requested.usd,
            nonce: requested.nonce,
            message: requested.message,
        };
        let digest = event_digest(
            &request,
            &format!("0x{}", "cd".repeat(32)),
            &requested,
            &finalized,
        );
        assert_ne!(request.native_tx_hash, format!("0x{}", "cd".repeat(32)));
        assert_eq!(digest.len(), 66);
        assert_eq!(
            gofin_evm::quantity(U256::from(finalized.usd) * scale_6_to_36()).as_str(),
            "995000000000000000000000000000000000000"
        );
        assert_eq!(
            native_to_micro(&request.gross_amount).unwrap(),
            U256::from(1_000_000_000)
        );
        assert!(requested_event_matches(
            &requested,
            requested.user,
            requested.destination,
            requested.nonce,
            U256::from(995_000_000),
        ));
        assert!(!requested_event_matches(
            &requested,
            requested.user,
            requested.destination,
            requested.nonce + 1,
            U256::from(995_000_000),
        ));
        assert!(!requested_event_matches(
            &requested,
            requested.user,
            requested.destination,
            requested.nonce,
            U256::from(1),
        ));
    }

    #[test]
    fn withdrawal_owner_filter_matches_abi_indexed_address_word() {
        for address in [
            "0x1111111111111111111111111111111111111111",
            "0x0000000000000000000000000000000000000001",
        ] {
            let owner = gofin_evm::address(address).unwrap();
            let requested = HyperliquidBridge2::RequestedWithdrawal {
                user: owner,
                destination: owner,
                usd: 4_000_000,
                nonce: 123_000,
                message: B256::repeat_byte(0x33),
                requestedTime: 123,
            };
            let finalized = HyperliquidBridge2::FinalizedWithdrawal {
                user: owner,
                destination: owner,
                usd: requested.usd,
                nonce: requested.nonce,
                message: requested.message,
            };
            let topic = withdrawal_owner_topic(owner);
            assert_eq!(topic.len(), 66);
            assert_eq!(
                topic,
                format!("{:#x}", requested.encode_log_data().topics()[1])
            );
            assert_eq!(
                topic,
                format!("{:#x}", finalized.encode_log_data().topics()[1])
            );
            assert_eq!(&topic[2..26], "000000000000000000000000");
            assert_eq!(&topic[26..], &address[2..]);
        }
    }

    struct WithdrawalRpc {
        request: WithdrawalProofRequest,
        event_nonce: u64,
        event_net: u64,
        transfer_net: u64,
        duplicate_request: bool,
        wrong_ledger_hash: bool,
        wrong_message: bool,
    }

    impl WithdrawalRpc {
        fn event_log<E: SolEvent>(&self, event: &E, hash: &str) -> Value {
            let encoded = event.encode_log_data();
            json!({
                "address": BRIDGE2,
                "topics": encoded.topics().iter().map(|v| format!("{v:#x}")).collect::<Vec<_>>(),
                "data": format!("0x{}",hex::encode(&encoded.data)),
                "transactionHash":hash,"blockHash":format!("0x{}","22".repeat(32)),
                "blockNumber":"0x10","removed":false
            })
        }

        fn request_log(&self) -> Value {
            self.event_log(
                &HyperliquidBridge2::RequestedWithdrawal {
                    user: gofin_evm::address(&self.request.owner).unwrap(),
                    destination: gofin_evm::address(&self.request.destination).unwrap(),
                    usd: self.event_net,
                    nonce: self.event_nonce,
                    message: B256::repeat_byte(0x33),
                    requestedTime: self.request.nonce_ms / 1000,
                },
                &format!("0x{}", "11".repeat(32)),
            )
        }

        fn final_hash(&self) -> String {
            format!(
                "0x{}",
                if self.wrong_ledger_hash { "ee" } else { "cd" }.repeat(32)
            )
        }

        fn final_log(&self) -> Value {
            self.event_log(
                &HyperliquidBridge2::FinalizedWithdrawal {
                    user: gofin_evm::address(&self.request.owner).unwrap(),
                    destination: gofin_evm::address(&self.request.destination).unwrap(),
                    usd: self.event_net,
                    nonce: self.event_nonce,
                    message: B256::repeat_byte(if self.wrong_message { 0x44 } else { 0x33 }),
                },
                &self.final_hash(),
            )
        }

        fn transfer_log(&self) -> Value {
            let mut log = self.event_log(
                &HyperliquidUsdc::Transfer {
                    from: gofin_evm::address(BRIDGE2).unwrap(),
                    to: gofin_evm::address(&self.request.destination).unwrap(),
                    value: U256::from(self.transfer_net),
                },
                &self.final_hash(),
            );
            log["address"] = json!(USDC);
            log
        }
    }

    #[async_trait::async_trait]
    impl gofin_evm::EvmRpc for WithdrawalRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value, GoFinError> {
            let block_hash = format!("0x{}", "22".repeat(32));
            Ok(match method {
                "eth_chainId" => json!("0xa4b1"),
                "eth_getLogs" => {
                    let expected_owner_topic = self.request_log()["topics"][1].clone();
                    assert_eq!(params[0]["topics"][1], expected_owner_topic);
                    if params[0]["topics"][0]
                        == json!(format!(
                            "{:#x}",
                            HyperliquidBridge2::RequestedWithdrawal::SIGNATURE_HASH
                        ))
                    {
                        if self.duplicate_request {
                            json!([self.request_log(), self.request_log()])
                        } else {
                            json!([self.request_log()])
                        }
                    } else {
                        json!([self.final_log()])
                    }
                }
                "eth_getBlockByNumber" => {
                    assert_ne!(params[0], json!("finalized"));
                    assert_ne!(
                        params[0],
                        json!("latest"),
                        "Recovery bounds come from the known payout block"
                    );
                    json!({"number":"0x10","hash":block_hash,"timestamp":format!("{:#x}",self.request.nonce_ms/1000-600)})
                }
                "eth_getTransactionReceipt" => {
                    let hash = params[0].as_str().unwrap();
                    let logs = if hash == self.final_hash() {
                        vec![self.final_log(), self.transfer_log()]
                    } else {
                        vec![self.request_log()]
                    };
                    json!({"transactionHash":hash,"blockHash":block_hash,"blockNumber":"0x10","status":"0x1","gasUsed":"0x5208","effectiveGasPrice":"0x2","logs":logs})
                }
                "eth_getTransactionByHash" => {
                    json!({"hash":params[0],"blockHash":block_hash,"blockNumber":"0x10","from":BRIDGE2})
                }
                "eth_blockNumber" => json!("0x10"),
                _ => panic!("Unexpected withdrawal proof RPC: {method}"),
            })
        }
    }

    #[tokio::test]
    async fn withdrawal_correlates_microsecond_nonce_exact_net_and_ledger_payout_hash() {
        // All identities and funds here are synthetic; this fixture mirrors the
        // venue's native units and independent action/ledger/payout meanings.
        for case in 0..7 {
            let request = request();
            let rpc = Arc::new(WithdrawalRpc {
                request: request.clone(),
                event_nonce: if case == 1 {
                    request.nonce_ms
                } else {
                    request.ledger_nonce
                },
                event_net: if case == 2 { 994_000_000 } else { 995_000_000 },
                transfer_net: if case == 3 { 994_000_000 } else { 995_000_000 },
                duplicate_request: case == 4,
                wrong_ledger_hash: case == 5,
                wrong_message: case == 6,
            });
            let ethereum = Arc::new(
                EthereumAdapter::new(
                    gofin_ethereum::EthereumConfig::wallet_only(ARBITRUM_ONE, 1, vec![]),
                    vec![gofin_ethereum::WalletConnection {
                        name: "synthetic".into(),
                        address: request.owner.clone(),
                        signing_enabled: false,
                    }],
                    Arc::new(gofin_evm::EvmRuntime::new(rpc, ARBITRUM_ONE, None)),
                )
                .unwrap(),
            );
            let result = HyperliquidArbitrumWithdrawalProof::new(ethereum)
                .prove_sequenced_arbitrum_withdrawal(&request)
                .await;
            match case {
                0 => {
                    let proof = result.unwrap().unwrap();
                    assert_eq!(
                        proof.net_amount,
                        Quantity::from_decimal(&request.net_amount.to_decimal(36), 6).unwrap()
                    );
                    assert_eq!(proof.transaction_hash, request.ledger_hash);
                    assert!(proof.sequenced);
                }
                1 | 2 | 6 => assert!(result.unwrap().is_none()),
                _ => assert!(result.is_err()),
            }
        }
    }
    #[test]
    fn withdrawal_net_rescaling_rejects_sub_token_precision_and_zero() {
        let exact = Quantity::from_decimal("4", 36).unwrap();
        assert_eq!(native_to_micro(&exact).unwrap(), U256::from(4_000_000));
        let sub_atom = exact.checked_add(&Quantity::new("1").unwrap()).unwrap();
        assert!(native_to_micro(&sub_atom).is_err());
        assert!(native_to_micro(&Quantity::zero()).is_err());
    }

    struct PagedLogsRpc(std::sync::Mutex<Vec<(u64, u64)>>);

    #[async_trait::async_trait]
    impl gofin_evm::EvmRpc for PagedLogsRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value, GoFinError> {
            assert_eq!(method, "eth_getLogs");
            let from = rpc_u64(&params[0]["fromBlock"])?;
            let to = rpc_u64(&params[0]["toBlock"])?;
            assert!(from <= to && to - from < 512);
            assert!(
                to <= 1300,
                "Recovery queries must stay below their known payout block"
            );
            self.0.lock().unwrap().push((from, to));
            Ok(json!([]))
        }
    }

    #[tokio::test]
    async fn withdrawal_log_pages_are_disjoint_and_bounded_by_known_payout_block() {
        let rpc = Arc::new(PagedLogsRpc(Default::default()));
        let ethereum = Arc::new(
            EthereumAdapter::new(
                gofin_ethereum::EthereumConfig::wallet_only(ARBITRUM_ONE, 1, vec![]),
                vec![gofin_ethereum::WalletConnection {
                    name: "synthetic".into(),
                    address: "0x1111111111111111111111111111111111111111".into(),
                    signing_enabled: false,
                }],
                Arc::new(gofin_evm::EvmRuntime::new(rpc.clone(), ARBITRUM_ONE, None)),
            )
            .unwrap(),
        );
        let result = HyperliquidArbitrumWithdrawalProof::new(ethereum)
            .logs(gofin_evm::address(BRIDGE2).unwrap(), 100, 1300, vec![])
            .await
            .unwrap();
        assert!(result.is_empty());
        assert_eq!(
            *rpc.0.lock().unwrap(),
            vec![(100, 611), (612, 1123), (1124, 1300)]
        );
        let observer = HyperliquidArbitrumWithdrawalProof::new(Arc::new(
            EthereumAdapter::new(
                gofin_ethereum::EthereumConfig::wallet_only(ARBITRUM_ONE, 1, vec![]),
                vec![gofin_ethereum::WalletConnection {
                    name: "synthetic".into(),
                    address: "0x1111111111111111111111111111111111111111".into(),
                    signing_enabled: false,
                }],
                Arc::new(gofin_evm::EvmRuntime::new(rpc.clone(), ARBITRUM_ONE, None)),
            )
            .unwrap(),
        ));
        assert!(
            observer
                .logs(gofin_evm::address(BRIDGE2).unwrap(), 1300, 1300, vec![])
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(rpc.0.lock().unwrap().last(), Some(&(1300, 1300)));
        assert!(
            observer
                .logs(gofin_evm::address(BRIDGE2).unwrap(), 1301, 1300, vec![])
                .await
                .is_err()
        );
        assert_eq!(rpc.0.lock().unwrap().len(), 4);
    }
}
