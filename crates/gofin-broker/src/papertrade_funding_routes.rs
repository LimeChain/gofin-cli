//! Protected Papertrade funding routes that are not handled by the direct
//! native HyperEVM/Core deposit service.
//!
//! Core withdrawals use the exact owner-signed `WithdrawToCore` EIP-712 intent
//! and canonical BatchExecutor receipt, never a direct EVM transaction. Core
//! CCTP withdrawals additionally require source-ledger, Circle attestation,
//! and finalized destination-mint evidence.

use std::time::Duration;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
    time::{SystemTime, UNIX_EPOCH},
};

use gofin_domain::GoFinError;
use gofin_evm::alloy::primitives::{U256, keccak256};
use gofin_hyperliquid::funding::{CctpWithdrawalTarget, cctp_withdrawal_action};
use gofin_papertrade::{
    FundingDirection, FundingIntent, FundingReceipt, FundingRoute, PapertradeRuntime,
    PapertradeTypedIntent,
};
use serde_json::{Value, json};

use crate::{papertrade_funding::*, papertrade_funding_host::PapertradeFundingServices};

const CORE_WITHDRAWAL_TOPIC: &str =
    "0x95d1a93901f24eeaa294588802f66fa008f9f3741060c9f79e5dab4709de5e75";
const CORE_CCTP_RETAINED_RESERVE_ATOMS: u64 = 100_000;
/// HYPE uses eight decimals; this caps a CoreWriter `sendAsset` fee at 0.001
/// HYPE while the separate 0.1 USDC reserve covers USDC-denominated fees.
const ERC20_TRANSFER_TOPIC: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

fn value_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(raw) if !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()) => {
            raw.parse().ok()
        }
        _ => None,
    }
}

fn decimal_hundredths(value: Option<&Value>) -> Result<u64, GoFinError> {
    let raw = match value {
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::String(raw)) => raw.clone(),
        _ => return Err(unavailable("Circle's CCTP fee quote omitted minimumFee.")),
    };
    let (whole, fraction) = raw.split_once('.').unwrap_or((&raw, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > 2
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(unavailable("Circle returned a malformed minimumFee."));
    }
    let whole = whole
        .parse::<u64>()
        .map_err(|_| unavailable("Circle minimumFee exceeds the supported range."))?;
    let fractional = match fraction.len() {
        0 => 0,
        1 => u64::from(fraction.as_bytes()[0] - b'0') * 10,
        2 => u64::from(
            fraction
                .parse::<u8>()
                .map_err(|_| unavailable("Circle minimumFee exceeds the supported range."))?,
        ),
        _ => unreachable!(),
    };
    whole
        .checked_mul(100)
        .and_then(|n| n.checked_add(fractional))
        .ok_or_else(|| unavailable("Circle minimumFee exceeds the supported range."))
}

fn canonical_log_index(log: &gofin_papertrade::CanonicalLog) -> Option<u64> {
    u64::from_str_radix(log.log_index.strip_prefix("0x")?, 16).ok()
}

fn valid_hash32(value: &str) -> bool {
    let Some(raw) = value.strip_prefix("0x") else {
        return false;
    };
    raw.len() == 64 && raw.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn topic_address(address: &str) -> String {
    format!("0x{}{}", "0".repeat(24), address.trim_start_matches("0x"))
}

fn hyperevm_core_wallet_transfer_matches(
    transaction: &Value,
    owner: &str,
    amount_atoms: u64,
) -> bool {
    let Some(input) = transaction
        .get("input")
        .or_else(|| transaction.get("data"))
        .and_then(Value::as_str)
        .and_then(|value| value.strip_prefix("0x"))
    else {
        return false;
    };
    let mut expected = hex::encode(&keccak256(b"transfer(address,uint256)")[..4]);
    expected.push_str(&format!(
        "{}{}",
        "0".repeat(24),
        owner.trim_start_matches("0x").to_ascii_lowercase()
    ));
    expected.push_str(&format!("{:064x}", amount_atoms));
    transaction["from"].as_str().is_some_and(|value| {
        value.eq_ignore_ascii_case(gofin_hyperliquid::funding::USDC_SYSTEM_ADDRESS)
    }) && transaction["to"].as_str().is_some_and(|value| {
        value.eq_ignore_ascii_case(gofin_bridge::cctp::hyperevm::CORE_DEPOSIT_WALLET)
    }) && transaction["value"]
        .as_str()
        .is_some_and(|value| value.eq_ignore_ascii_case("0x0"))
        && input.eq_ignore_ascii_case(&expected)
}

fn hyperevm_core_wallet_receipt_logs(
    logs: &[Value],
    owner: &str,
    amount_atoms: u64,
) -> Option<(Value, Value)> {
    let wallet = gofin_bridge::cctp::hyperevm::CORE_DEPOSIT_WALLET;
    let token = gofin_bridge::cctp::hyperevm::NATIVE_USDC;
    let owner_topic = topic_address(owner);
    let wallet_topic = topic_address(wallet);
    let withdraw_topic = format!("0x{}", hex::encode(keccak256(b"Withdraw(address,uint256)")));
    let transfer = logs
        .iter()
        .filter(|log| {
            log["address"]
                .as_str()
                .is_some_and(|address| address.eq_ignore_ascii_case(token))
                && log["topics"]
                    .as_array()
                    .is_some_and(|topics| topics.len() == 3)
                && log["topics"][0]
                    .as_str()
                    .is_some_and(|topic| topic.eq_ignore_ascii_case(ERC20_TRANSFER_TOPIC))
                && log["topics"][1]
                    .as_str()
                    .is_some_and(|topic| topic.eq_ignore_ascii_case(&wallet_topic))
                && log["topics"][2]
                    .as_str()
                    .is_some_and(|topic| topic.eq_ignore_ascii_case(&owner_topic))
                && log["data"].as_str().is_some_and(|data| {
                    U256::from_str_radix(data.trim_start_matches("0x"), 16)
                        .is_ok_and(|value| value == U256::from(amount_atoms))
                })
        })
        .collect::<Vec<_>>();
    let withdrawn = logs
        .iter()
        .filter(|log| {
            log["address"]
                .as_str()
                .is_some_and(|address| address.eq_ignore_ascii_case(wallet))
                && log["topics"]
                    .as_array()
                    .is_some_and(|topics| topics.len() == 2)
                && log["topics"][0]
                    .as_str()
                    .is_some_and(|topic| topic.eq_ignore_ascii_case(&withdraw_topic))
                && log["topics"][1]
                    .as_str()
                    .is_some_and(|topic| topic.eq_ignore_ascii_case(&owner_topic))
                && log["data"].as_str().is_some_and(|data| {
                    U256::from_str_radix(data.trim_start_matches("0x"), 16)
                        .is_ok_and(|value| value == U256::from(amount_atoms))
                })
        })
        .collect::<Vec<_>>();
    if transfer.len() != 1 || withdrawn.len() != 1 {
        return None;
    }
    let transfer_position = rpc_u64(&transfer[0]["logIndex"])?;
    let withdraw_position = rpc_u64(&withdrawn[0]["logIndex"])?;
    (transfer_position < withdraw_position)
        .then(|| ((*transfer[0]).clone(), (*withdrawn[0]).clone()))
}

fn rpc_u64(value: &Value) -> Option<u64> {
    let raw = value.as_str()?.strip_prefix("0x")?;
    u64::from_str_radix(raw, 16).ok()
}

fn rpc_result_rows(value: &Value) -> Option<&[Value]> {
    value
        .as_array()
        .or_else(|| value.get("receipts").and_then(Value::as_array))
        .or_else(|| value.get("transactions").and_then(Value::as_array))
        .map(Vec::as_slice)
}

fn has_hyperevm_system_receipt_capability(receipts: &Value, transactions: &Value) -> bool {
    rpc_result_rows(receipts).is_some_and(|rows| rows.len() <= 10_000)
        && rpc_result_rows(transactions).is_some_and(|rows| rows.len() <= 10_000)
}

fn event_position(value: &Value) -> Option<(u64, u64)> {
    Some((
        rpc_u64(value.get("blockNumber")?)?,
        rpc_u64(value.get("transactionIndex")?)?,
    ))
}

fn owner_topic(owner: &str) -> Result<String, GoFinError> {
    let address = gofin_evm::address(owner)?;
    Ok(format!(
        "0x{}{}",
        "00".repeat(12),
        hex::encode(address.as_slice())
    ))
}

fn sweep_finalize_matches(transaction: &Value, owner: &str) -> bool {
    let owner_word = owner
        .strip_prefix("0x")
        .filter(|value| value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let Some(owner_word) = owner_word else {
        return false;
    };
    let input = transaction
        .get("input")
        .and_then(Value::as_str)
        .unwrap_or_default();
    transaction
        .get("to")
        .and_then(Value::as_str)
        .is_some_and(|to| to.eq_ignore_ascii_case(gofin_papertrade::EXCHANGE))
        && transaction
            .get("from")
            .and_then(Value::as_str)
            .is_some_and(|from| from.eq_ignore_ascii_case(owner))
        && input.len() == 74
        && input[..10].eq_ignore_ascii_case("0x6d142da1")
        && input[34..].eq_ignore_ascii_case(owner_word)
}

fn cctp_spot_transfer_matches(
    row: &Value,
    tx_detail: &Value,
    user: &str,
    destination: &str,
    atoms: u64,
    earliest_ms: u64,
    latest_ms: u64,
) -> bool {
    let delta = &row["delta"];
    let tx = &tx_detail["tx"];
    row["time"]
        .as_u64()
        .is_some_and(|time| (earliest_ms..=latest_ms).contains(&time))
        && row["hash"].as_str().is_some_and(valid_hash32)
        && delta["type"].as_str() == Some("spotTransfer")
        && delta["token"].as_str() == Some("USDC")
        && delta["user"]
            .as_str()
            .is_some_and(|value| value.eq_ignore_ascii_case(user))
        && delta["destination"]
            .as_str()
            .is_some_and(|value| value.eq_ignore_ascii_case(destination))
        && delta["amount"]
            .as_str()
            .is_some_and(|amount| decimal_usdc_atoms(amount) == Some(atoms))
        && delta["fee"]
            .as_str()
            .is_some_and(|fee| decimal_usdc_atoms(fee).is_some())
        && tx["hash"]
            .as_str()
            .zip(row["hash"].as_str())
            .is_some_and(|(actual, expected)| actual.eq_ignore_ascii_case(expected))
        && tx["time"].as_u64() == row["time"].as_u64()
        && tx["error"].is_null()
        && tx["block"].as_u64().is_some_and(|block| block > 0)
        && tx["user"]
            .as_str()
            .is_some_and(|value| value.eq_ignore_ascii_case(user))
        && tx["action"]["type"].as_str() == Some("SystemSpotSendAction")
        && tx["action"]["destination"]
            .as_str()
            .is_some_and(|value| value.eq_ignore_ascii_case(destination))
        && tx["action"]["token"].as_u64() == Some(0)
        && atoms
            .checked_mul(100)
            .is_some_and(|wei| tx["action"]["wei"].as_u64() == Some(wei))
}

fn core_owner_credit_matches(
    ledger_row: &Value,
    tx_detail: &Value,
    owner: &str,
    expected_atoms: u64,
    start_ms: u64,
    evm_block_time_ms: u64,
    latest_ms: u64,
) -> bool {
    let time = ledger_row["time"].as_u64();
    let hash = ledger_row["hash"].as_str();
    let delta = &ledger_row["delta"];
    let tx = &tx_detail["tx"];
    let amount_matches = delta["amount"].as_str().is_some_and(|amount| {
        gofin_domain::Quantity::from_decimal(amount, 6)
            .is_ok_and(|quantity| quantity == gofin_domain::Quantity::from_u64(expected_atoms))
    });
    time.is_some_and(|time| {
        time >= start_ms && time / 1_000 >= evm_block_time_ms / 1_000 && time <= latest_ms
    }) && hash.is_some_and(valid_hash32)
        && delta["type"].as_str() == Some("spotTransfer")
        && delta["token"].as_str() == Some("USDC")
        && delta["user"]
            .as_str()
            .is_some_and(|sender| sender.eq_ignore_ascii_case(gofin_papertrade::EXCHANGE))
        && delta["destination"]
            .as_str()
            .is_some_and(|destination| destination.eq_ignore_ascii_case(owner))
        && amount_matches
        && tx["hash"]
            .as_str()
            .zip(hash)
            .is_some_and(|(actual, expected)| actual.eq_ignore_ascii_case(expected))
        && tx["time"].as_u64() == time
        && tx["error"].is_null()
        && tx["block"].as_u64().is_some_and(|block| block > 0)
        && tx["action"]["type"].as_str() == Some("SystemSpotSendAction")
        && tx["action"]["destination"]
            .as_str()
            .is_some_and(|destination| destination.eq_ignore_ascii_case(owner))
        && tx["action"]["token"].as_u64() == Some(0)
        && expected_atoms
            .checked_mul(100)
            .is_some_and(|wei| tx["action"]["wei"].as_u64() == Some(wei))
        && tx["user"]
            .as_str()
            .is_some_and(|sender| sender.eq_ignore_ascii_case(gofin_papertrade::EXCHANGE))
}

fn now_ms() -> Result<u64, GoFinError> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| GoFinError::validation("System clock is invalid."))?
            .as_millis(),
    )
    .map_err(|_| GoFinError::validation("System clock exceeds the supported range."))
}

fn amount_decimal(atoms: u64) -> String {
    format!("{}.{:06}", atoms / 1_000_000, atoms % 1_000_000)
}

fn decimal_usdc_atoms(value: &str) -> Option<u64> {
    gofin_domain::Quantity::from_decimal(value, 6)
        .ok()?
        .to_string()
        .parse()
        .ok()
}

fn expected_bridge_amount(plan: &BoundFunding) -> Result<u64, GoFinError> {
    plan.intent
        .amount_usdc_atoms
        .checked_sub(100_000)
        .filter(|amount| *amount > 0)
        .ok_or_else(|| GoFinError::validation("Core CCTP withdrawal is below its reserve."))
}

fn word_address(word: &[u8]) -> Option<String> {
    (word.len() == 32 && word[..12].iter().all(|byte| *byte == 0))
        .then(|| format!("0x{}", hex::encode(&word[12..])))
}

fn core_withdrawal_action_in_batch(
    transaction: &gofin_papertrade::CanonicalTransaction,
    verified: &gofin_papertrade::VerifiedPapertradeCall,
    owner: &str,
    core_usdc_8dp: u64,
) -> bool {
    const CORE_WRITER: &str = "0x3333333333333333333333333333333333333333";
    const CORE_WRITER_TOPIC: &str =
        "0x8c7f585fb295f7eb1e6aeb8fba61b23a4fe60beda405f0045073b185c74412e3";
    const BATCH_EXECUTOR: &str = gofin_papertrade::BATCH_EXECUTOR;
    const CALL_EXECUTED: &str =
        "0x8a23ef1120f3e42e722a0c0a599a66d55ab2650ecc39716f57e625684e349cad";
    const CALL_REJECTED: &str =
        "0x6002fcbae88fcbb8133c69c91df90bbc0b4464af94df2a56f8bb843f1cc5a193";
    const CALL_SKIPPED: &str = "0xaaf3cbad9087f9956efae61847c185478efbe673d0f8591fdaf1eb97a897aa7c";
    let same_batch_terminal = |log: &&gofin_papertrade::CanonicalLog| {
        log.address.eq_ignore_ascii_case(BATCH_EXECUTOR)
            && log.topics.first().is_some_and(|topic| {
                [CALL_EXECUTED, CALL_REJECTED, CALL_SKIPPED]
                    .iter()
                    .any(|known| topic.eq_ignore_ascii_case(known))
            })
            && log.topics.get(1).is_some_and(|topic| {
                topic
                    .trim_start_matches("0x")
                    .eq_ignore_ascii_case(&format!(
                        "{:0>64}",
                        verified.batch_id.trim_start_matches("0x")
                    ))
            })
    };
    let Some(call_log) = transaction.logs.iter().find(|log| {
        same_batch_terminal(log)
            && log
                .topics
                .first()
                .is_some_and(|topic| topic.eq_ignore_ascii_case(CALL_EXECUTED))
            && log.topics.get(2).is_some_and(|topic| {
                u64::from_str_radix(topic.trim_start_matches("0x"), 16).ok()
                    == Some(verified.batch_item_index)
            })
            && log
                .topics
                .get(3)
                .is_some_and(|topic| topic.eq_ignore_ascii_case(&verified.intent_id))
    }) else {
        return false;
    };
    let Some(call_index) = canonical_log_index(call_log) else {
        return false;
    };
    // BatchExecutor emits a terminal item marker after the callee returns, so
    // the target call's logs precede CallExecuted. Bound them below by the
    // previous item marker for this batch and above by this exact marker.
    let previous_item_marker = transaction
        .logs
        .iter()
        .filter(|log| same_batch_terminal(log))
        .filter(|log| {
            log.topics
                .get(2)
                .and_then(|topic| u64::from_str_radix(topic.trim_start_matches("0x"), 16).ok())
                .is_some_and(|index| index < verified.batch_item_index)
        })
        .filter_map(canonical_log_index)
        .max();
    let matching_withdrawals = transaction
        .logs
        .iter()
        .filter(|log| {
            log.address.eq_ignore_ascii_case(gofin_papertrade::EXCHANGE)
                && log
                    .topics
                    .first()
                    .is_some_and(|topic| topic.eq_ignore_ascii_case(CORE_WITHDRAWAL_TOPIC))
                && log.topics.get(1).is_some_and(|topic| {
                    word_address(&hex::decode(topic.trim_start_matches("0x")).unwrap_or_default())
                        .is_some_and(|address| address.eq_ignore_ascii_case(owner))
                })
                && hex::decode(log.data.trim_start_matches("0x"))
                    .ok()
                    .is_some_and(|data| {
                        data.len() >= 64
                            && U256::from_be_slice(&data[32..64]) == U256::from(core_usdc_8dp)
                    })
                && canonical_log_index(log).is_some_and(|index| {
                    previous_item_marker.is_none_or(|previous| index > previous)
                        && index < call_index
                })
        })
        .collect::<Vec<_>>();
    if matching_withdrawals.len() != 1 {
        return false;
    }
    let withdrawal_log = matching_withdrawals[0];
    let Some(withdrawal_index) = canonical_log_index(withdrawal_log) else {
        return false;
    };
    if withdrawal_index >= call_index {
        return false;
    }
    let matching_writer_logs = transaction
        .logs
        .iter()
        .filter(|log| {
            canonical_log_index(log).is_some_and(|index| {
                previous_item_marker.is_none_or(|previous| index > previous)
                    && index < withdrawal_index
            }) && log.address.eq_ignore_ascii_case(CORE_WRITER)
                && log
                    .topics
                    .first()
                    .is_some_and(|topic| topic.eq_ignore_ascii_case(CORE_WRITER_TOPIC))
                && log.topics.get(1).is_some_and(|topic| {
                    word_address(&hex::decode(topic.trim_start_matches("0x")).unwrap_or_default())
                        .is_some_and(|address| {
                            address.eq_ignore_ascii_case(gofin_papertrade::EXCHANGE)
                        })
                })
        })
        .collect::<Vec<_>>();
    if matching_writer_logs.len() != 1 {
        return false;
    }
    let Ok(raw) = hex::decode(
        matching_writer_logs[0]
            .data
            .strip_prefix("0x")
            .unwrap_or(&matching_writer_logs[0].data),
    ) else {
        return false;
    };
    if raw.len() < 64
        || U256::from_be_slice(&raw[..32]) != U256::from(32u64)
        || U256::from_be_slice(&raw[32..64]) != U256::from(100u64)
    {
        return false;
    }
    let payload = &raw[64..];
    payload.len() == 128
        && payload[..4] == [1, 0, 0, 6]
        && word_address(&payload[4..36]).is_some_and(|value| value.eq_ignore_ascii_case(owner))
        && payload[36..68].iter().all(|byte| *byte == 0)
        && U256::from_be_slice(&payload[68..100]) == U256::from(core_usdc_8dp)
        && payload[100..].iter().all(|byte| *byte == 0)
}

pub(crate) struct PapertradeFundingRoutes {
    credentials: Arc<dyn crate::BrokerCredentialProvider>,
    wallet_credential: gofin_credentials::CredentialRef,
    owner: String,
    evm_runtimes: BTreeMap<u64, Arc<gofin_evm::EvmRuntime>>,
    hyperliquid_transport: Arc<dyn gofin_hyperliquid::HyperliquidTransport>,
    hyperliquid_account: gofin_hyperliquid::HyperliquidAccount,
    cctp_settlement: Arc<dyn crate::papertrade_cctp_settlement::PapertradeCctpSettlement>,
    solana_runtime: Option<Arc<gofin_solana::SolanaRuntime>>,
    inbound_source: Arc<crate::papertrade_funding_inbound::PapertradeInboundCctpSource>,
    solana_credential: Option<gofin_credentials::CredentialRef>,
    papertrade_http: Arc<crate::papertrade_runtime::PapertradeHttp>,
    confirmations: BTreeMap<u64, u64>,
    http: reqwest::Client,
    runtime: Mutex<Option<Weak<crate::papertrade_runtime::BrokerPapertradeRuntime>>>,
}

impl PapertradeFundingRoutes {
    // This protected composition point intentionally accepts each capability
    // explicitly; callers cannot substitute arbitrary transports per request.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        credentials: Arc<dyn crate::BrokerCredentialProvider>,
        wallet_credential: gofin_credentials::CredentialRef,
        owner: &str,
        evm_runtimes: BTreeMap<u64, Arc<gofin_evm::EvmRuntime>>,
        hyperliquid_transport: Arc<dyn gofin_hyperliquid::HyperliquidTransport>,
        hyperliquid_account: gofin_hyperliquid::HyperliquidAccount,
        cctp_settlement: Arc<dyn crate::papertrade_cctp_settlement::PapertradeCctpSettlement>,
        solana_runtime: Option<Arc<gofin_solana::SolanaRuntime>>,
        inbound_source: Arc<crate::papertrade_funding_inbound::PapertradeInboundCctpSource>,
        solana_credential: Option<gofin_credentials::CredentialRef>,
        papertrade_http: Arc<crate::papertrade_runtime::PapertradeHttp>,
        confirmations: BTreeMap<u64, u64>,
    ) -> Result<Self, GoFinError> {
        let owner = gofin_evm::address(owner)
            .map_err(|_| GoFinError::validation("Papertrade funding owner is malformed."))?;
        let http = reqwest::Client::builder()
            .user_agent("GoFin/0.1 (Papertrade integration)")
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(8))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| unavailable("Circle quote transport could not be configured."))?;
        if !hyperliquid_account
            .owner
            .eq_ignore_ascii_case(&format!("{owner:#x}"))
            || !hyperliquid_account.signing_enabled
            || !matches!(
                hyperliquid_account.execution_scope,
                gofin_hyperliquid::HyperliquidExecutionScope::Owner
            )
        {
            return Err(GoFinError::validation(
                "Funding routes require the exact signing Hyperliquid owner account.",
            ));
        }
        Ok(Self {
            credentials,
            wallet_credential,
            owner: format!("{owner:#x}").to_ascii_lowercase(),
            evm_runtimes,
            hyperliquid_transport,
            hyperliquid_account,
            cctp_settlement,
            solana_runtime,
            inbound_source,
            solana_credential,
            papertrade_http,
            confirmations,
            http,
            runtime: Mutex::new(None),
        })
    }

    fn runtime(
        &self,
    ) -> Result<Arc<crate::papertrade_runtime::BrokerPapertradeRuntime>, GoFinError> {
        self.runtime
            .lock()
            .map_err(|_| unavailable("Papertrade funding runtime handle is poisoned."))?
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or_else(|| unavailable("Papertrade funding runtime is not attached."))
    }

    fn unavailable<T>() -> Result<T, GoFinError> {
        Err(GoFinError::new(
            "PAPERTRADE_FUNDING_ROUTE_UNAVAILABLE",
            "preflight",
            "This Papertrade funding route is not available in the configured broker.",
            "Use a configured, supported funding route and retry preparation.",
        ))
    }

    fn plan_key(operation_id: &str) -> String {
        format!("papertrade-funding-plan:{operation_id}")
    }

    fn leg_key(submission_id: &str, suffix: &str) -> String {
        format!(
            "papertrade-funding-leg:{}:{suffix}",
            hex::encode(submission_id)
        )
    }

    fn native_fee_policy(
        source_domain: u32,
    ) -> Result<(Option<String>, Option<String>), GoFinError> {
        // These are finite dispatch caps, not promised fee estimates. They
        // are disclosed in the funding plan and independently valued by the
        // configured pricing/policy path before authorization.
        match source_domain {
            0 => Ok((
                Some("ethereum:1:native".into()),
                Some("1000000000000000".into()),
            )),
            3 => Ok((
                Some("ethereum:42161:native".into()),
                Some("100000000000000".into()),
            )),
            5 => Ok((
                Some("solana:mainnet-beta:native".into()),
                // Event-account rent is a separately bounded refundable
                // balance; this cap covers only transaction/network fees.
                Some("500000".into()),
            )),
            19 => Ok((
                Some("hyperliquid:mainnet:HYPE".into()),
                Some(crate::papertrade_funding::MAX_CORE_ONWARD_NATIVE_FEE_ATOMS.to_string()),
            )),
            _ => Err(unavailable(
                "No reviewed native fee policy exists for this CCTP source.",
            )),
        }
    }

    fn verify_core_cctp_fee_row(
        delta: &Value,
        owner: &str,
        amount_atoms: u64,
        nonce: u64,
        maximum_usdc_fee_atoms: u64,
        maximum_hype_fee_atoms: u64,
    ) -> Option<(u64, u64)> {
        let amount = delta["amount"]
            .as_str()
            .and_then(|value| gofin_domain::Quantity::from_decimal(value, 6).ok())?;
        let usdc_value = delta["usdcValue"]
            .as_str()
            .and_then(|value| gofin_domain::Quantity::from_decimal(value, 6).ok())?;
        let usdc_fee = delta["fee"]
            .as_str()
            .and_then(|value| gofin_domain::Quantity::from_decimal(value, 6).ok())?;
        let usdc_fee_atoms = u64::try_from(usdc_fee.integer()).ok()?;
        let hype_fee_atoms = delta["nativeTokenFee"]
            .as_str()
            .and_then(|value| gofin_domain::Quantity::from_decimal(value, 8).ok())
            .and_then(|value| u64::try_from(value.integer()).ok())?;
        let amount_atoms = gofin_domain::Quantity::from_u64(amount_atoms);
        let fee_token = delta["feeToken"].as_str()?;
        if delta["type"].as_str() != Some("send")
            || delta["user"]
                .as_str()
                .is_none_or(|value| !value.eq_ignore_ascii_case(owner))
            || delta["destination"].as_str().is_none_or(|value| {
                !value.eq_ignore_ascii_case(gofin_hyperliquid::funding::USDC_SYSTEM_ADDRESS)
            })
            || delta["sourceDex"].as_str() != Some("spot")
            || delta["destinationDex"].as_str() != Some("spot")
            || delta["token"].as_str() != Some("USDC")
            || delta["nonce"].as_u64() != Some(nonce)
            || amount != amount_atoms
            || usdc_value != amount
            || usdc_fee_atoms > maximum_usdc_fee_atoms
            || hype_fee_atoms > maximum_hype_fee_atoms
            || (usdc_fee_atoms > 0 && (hype_fee_atoms > 0 || fee_token != "USDC"))
            || (hype_fee_atoms > 0 && (usdc_fee_atoms > 0 || !fee_token.is_empty()))
            || (usdc_fee_atoms == 0
                && hype_fee_atoms == 0
                && !fee_token.is_empty()
                && fee_token != "USDC")
        {
            return None;
        }
        Some((usdc_fee_atoms, hype_fee_atoms))
    }

    fn persist_immutable(&self, key: &str, value: &Value) -> Result<(), GoFinError> {
        match self.credentials.papertrade_runtime_record(key)? {
            Some(existing) if existing != *value => {
                let suffix = key.rsplit(':').next().unwrap_or("unknown");
                let changed = existing
                    .as_object()
                    .into_iter()
                    .flat_map(|object| object.keys())
                    .chain(
                        value
                            .as_object()
                            .into_iter()
                            .flat_map(|object| object.keys()),
                    )
                    .filter(|field| existing.get(*field) != value.get(*field))
                    .map(String::as_str)
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
                    .join(",");
                Err(GoFinError::validation(format!(
                    "Papertrade funding identity conflicts with durable record kind `{suffix}` (fields: {changed})."
                )))
            }
            Some(_) => Ok(()),
            None => self.credentials.put_papertrade_runtime_record(key, value),
        }
    }

    fn sanitized_withdrawal_error(error: &GoFinError) -> Value {
        let details = error.details.as_deref();
        let http_status = details
            .and_then(|value| value.get("http_status"))
            .and_then(Value::as_u64)
            .filter(|status| *status <= u16::MAX as u64);
        let exchange_code = details
            .and_then(|value| value.get("exchange_code"))
            .and_then(Value::as_str)
            .filter(|code| {
                !code.is_empty()
                    && code.len() <= 64
                    && code.bytes().all(|byte| {
                        byte.is_ascii_uppercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'_' | b'-')
                    })
            });
        let code = if !error.code.is_empty()
            && error.code.len() <= 64
            && error
                .code
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        {
            error.code.as_str()
        } else {
            "SANITIZED_TRANSPORT_ERROR"
        };
        let stage = match error.stage.as_str() {
            "submission" | "authentication" | "signing" | "read" | "validation" => {
                error.stage.as_str()
            }
            _ => "other",
        };
        json!({
            "version":1,
            "kind":"transport_error",
            "code":code,
            "stage":stage,
            "http_status":http_status,
            "exchange_code":exchange_code,
        })
    }

    async fn validate_hyperevm_continuation(
        &self,
        plan: &BoundFunding,
        leg: &FundingLeg,
        binding: &gofin_papertrade::FundingContinuationBinding,
        outer: &gofin_domain::Submission,
    ) -> Result<(), GoFinError> {
        binding.validate()?;
        if plan.intent.direction != FundingDirection::Withdraw
            || plan.intent.route != FundingRoute::Hyperevm
            || !plan.intent.owner.eq_ignore_ascii_case(&self.owner)
            || plan.legs.len() != 2
            || plan.legs[1] != *leg
            || leg.ordinal != 1
            || outer.kind != "papertrade_funding_continuation"
            || outer.reference != binding.operation_id
            || outer.data
                != json!({
                    "operation_id": binding.operation_id,
                    "route_id": binding.route_id,
                    "plan_fingerprint": binding.plan_fingerprint,
                    "stage1_proof_fingerprint": binding.stage1_proof_fingerprint
                })
            || binding.route_id != plan.operation_id
            || binding.intent != plan.intent
            || binding.destination_identity != plan.destination_identity
        {
            return Err(GoFinError::validation(
                "HyperEVM continuation differs from its exact stage-two operation and route.",
            ));
        }
        let stage1 = self.reconcile_persisted_leg(plan, &plan.legs[0]).await?;
        let proof = validate_stage1_core_credit(plan, &stage1)?;
        let expected = build_continuation_binding(
            plan,
            &binding.operation_id,
            &binding.parent_submission,
            proof,
        )?;
        if expected.version != binding.version
            || expected.operation_id != binding.operation_id
            || expected.route_id != binding.route_id
            || expected.parent_submission != binding.parent_submission
            || expected.plan_fingerprint != binding.plan_fingerprint
            || expected.stage1_proof_fingerprint != binding.stage1_proof_fingerprint
            || expected.intent != binding.intent
            || expected.source_identity != binding.source_identity
            || expected.destination_identity != binding.destination_identity
            || expected.bounds != binding.bounds
        {
            return Err(GoFinError::validation(
                "HyperEVM continuation no longer matches the immutable stage-one Core credit.",
            ));
        }
        Ok(())
    }

    async fn hyperevm_core_source_proof(
        &self,
        plan: &BoundFunding,
        leg: &FundingLeg,
        action: &gofin_hyperliquid::funding::SendAssetAction,
    ) -> Result<Option<Value>, GoFinError> {
        let hash_key = Self::leg_key(&leg.submission_id, "discovered_action_hash");
        let hash = if let Some(saved) = self.credentials.papertrade_runtime_record(&hash_key)? {
            let hash = saved.get("hash").and_then(Value::as_str).ok_or_else(|| {
                GoFinError::validation("Persisted HyperEVM source action hash is malformed.")
            })?;
            if !valid_hash32(hash) {
                return Err(GoFinError::validation(
                    "Persisted HyperEVM source action hash is malformed.",
                ));
            }
            hash.to_owned()
        } else {
            let history = self
                .hyperliquid_transport
                .explorer(
                    gofin_exchange::ccxt::HyperliquidExplorerRequest::UserDetails {
                        user: self.owner.clone(),
                    },
                )
                .await?;
            let transactions = history
                .get("txs")
                .and_then(Value::as_array)
                .filter(|rows| rows.len() <= 20_000)
                .ok_or_else(|| unavailable("Hyperliquid action history is malformed."))?;
            let amount = gofin_domain::Quantity::from_decimal(&action.amount, 6)?;
            let matches = transactions
                .iter()
                .filter(|row| {
                    let observed = &row["action"];
                    row["user"]
                        .as_str()
                        .is_some_and(|owner| owner.eq_ignore_ascii_case(&self.owner))
                        && observed["type"].as_str() == Some("sendAsset")
                        && observed["nonce"].as_u64() == Some(action.nonce)
                        && observed["hyperliquidChain"].as_str() == Some("Mainnet")
                        && observed["signatureChainId"].as_str() == Some("0x3e7")
                        && observed["token"].as_str() == Some("USDC")
                        && observed["destination"]
                            .as_str()
                            .is_some_and(|value| value.eq_ignore_ascii_case(&action.destination))
                        && observed["amount"]
                            .as_str()
                            .and_then(|value| gofin_domain::Quantity::from_decimal(value, 6).ok())
                            == Some(amount.clone())
                        && observed["sourceDex"].as_str() == Some("spot")
                        && observed["destinationDex"].as_str() == Some("spot")
                        && observed["fromSubAccount"].as_str() == Some("")
                })
                .collect::<Vec<_>>();
            if matches.len() != 1 {
                return Ok(None);
            }
            let hash = matches[0]["hash"]
                .as_str()
                .filter(|hash| valid_hash32(hash))
                .ok_or_else(|| {
                    unavailable("Hyperliquid action history omitted its canonical hash.")
                })?
                .to_owned();
            self.persist_immutable(&hash_key, &json!({"version":1,"hash":hash}))?;
            hash
        };
        let details = self
            .hyperliquid_transport
            .explorer(
                gofin_exchange::ccxt::HyperliquidExplorerRequest::TxDetails { hash: hash.clone() },
            )
            .await?;
        let tx = &details["tx"];
        let observed = &tx["action"];
        let amount = gofin_domain::Quantity::from_decimal(&action.amount, 6)?;
        let amount_atoms = amount
            .as_str()
            .parse::<u64>()
            .map_err(|_| GoFinError::validation("HyperEVM withdrawal principal is malformed."))?;
        if tx["hash"].as_str() != Some(hash.as_str())
            || tx["user"]
                .as_str()
                .is_none_or(|owner| !owner.eq_ignore_ascii_case(&self.owner))
            || !tx["error"].is_null()
            || observed["type"].as_str() != Some("sendAsset")
            || observed["nonce"].as_u64() != Some(action.nonce)
            || observed["hyperliquidChain"].as_str() != Some("Mainnet")
            || observed["signatureChainId"].as_str() != Some("0x3e7")
            || observed["token"].as_str() != Some("USDC")
            || observed["destination"]
                .as_str()
                .is_none_or(|value| !value.eq_ignore_ascii_case(&action.destination))
            || observed["amount"]
                .as_str()
                .and_then(|value| gofin_domain::Quantity::from_decimal(value, 6).ok())
                != Some(amount)
            || observed["sourceDex"].as_str() != Some("spot")
            || observed["destinationDex"].as_str() != Some("spot")
            || observed["fromSubAccount"].as_str() != Some("")
        {
            return Ok(None);
        }
        let action_time_ms = tx["time"]
            .as_u64()
            .ok_or_else(|| unavailable("Hyperliquid action omitted its canonical timestamp."))?;
        let start_time_ms = action_time_ms.saturating_sub(1_000);
        let end_time_ms = now_ms()?;
        let ledger = self
            .hyperliquid_transport
            .info(
                gofin_exchange::ccxt::HyperliquidInfoRequest::UserNonFundingLedgerUpdates {
                    owner: self.owner.clone(),
                    start_time_ms,
                    end_time_ms: Some(end_time_ms),
                },
            )
            .await?;
        let rows = ledger
            .as_array()
            .or_else(|| ledger.get("rows").and_then(Value::as_array))
            .filter(|rows| rows.len() <= 2_000)
            .ok_or_else(|| unavailable("Hyperliquid source ledger response is malformed."))?;
        let matching = rows
            .iter()
            .filter(|row| {
                row["hash"].as_str() == Some(hash.as_str())
                    && row["time"]
                        .as_u64()
                        .is_some_and(|time| time >= action_time_ms && time <= end_time_ms)
            })
            .collect::<Vec<_>>();
        if matching.len() != 1 {
            return Ok(None);
        }
        let native_fee_cap = plan
            .bounds
            .maximum_native_fee_atoms
            .as_deref()
            .filter(|_| plan.bounds.native_fee_asset.as_deref() == Some("hyperliquid:mainnet:HYPE"))
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        let effects =
            crate::papertrade_funding_host::verify_core_send_single_ledger_with_native_fee(
                &matching[0]["delta"],
                &self.owner,
                gofin_hyperliquid::funding::USDC_SYSTEM_ADDRESS,
                amount_atoms,
                plan.bounds.maximum_source_fee_usdc_atoms,
                native_fee_cap,
                "sendAsset",
                action.nonce,
            );
        let Some(effects) = effects else {
            return Ok(None);
        };
        if effects.source_debit_atoms != amount_atoms.saturating_add(effects.source_fee_atoms)
            || effects.source_fee_atoms > plan.bounds.maximum_source_fee_usdc_atoms
            || effects.destination_credit_atoms != amount_atoms
        {
            // Stage one credited the full withdrawal amount to Core. The
            // stage-two send principal is reduced by the fixed reserve and
            // its separate Core fee must fit inside that reserve.
            return Ok(None);
        }
        let proof = json!({
            "version":1,
            "kind":"canonical_hyperevm_core_send_asset",
            "action_hash":hash,
            "owner":self.owner,
            "nonce":action.nonce,
            "action_time_ms":action_time_ms,
            "amount_atoms":amount_atoms,
            "source_debit_atoms":effects.source_debit_atoms,
            "source_fee_atoms":effects.source_fee_atoms,
            "native_fee_atoms":effects.source_native_fee_atoms,
            "ledger_hash":matching[0]["hash"],
            "ledger_time_ms":matching[0]["time"]
        });
        self.persist_immutable(
            &Self::leg_key(&leg.submission_id, "hyperevm_core_source_proof"),
            &proof,
        )?;
        Ok(Some(proof))
    }

    async fn hyperevm_destination_proof(
        &self,
        plan: &BoundFunding,
        leg: &FundingLeg,
        action_time_ms: u64,
        amount_atoms: u64,
    ) -> Result<Option<Value>, GoFinError> {
        let runtime = self.evm_runtimes.get(&999).ok_or_else(|| {
            unavailable("The configured HyperEVM read runtime is missing for destination proof.")
        })?;
        let first_block = plan.core_sweep_from_block.ok_or_else(|| {
            GoFinError::validation("HyperEVM continuation omitted its EVM scan baseline.")
        })?;
        let latest = runtime.rpc.request("eth_blockNumber", json!([])).await?;
        let latest = latest
            .as_str()
            .and_then(|value| u64::from_str_radix(value.strip_prefix("0x")?, 16).ok())
            .ok_or_else(|| unavailable("HyperEVM returned an invalid latest block."))?;
        let mut cursor = first_block;
        let mut fetched = 0usize;
        let mut candidates = BTreeMap::<String, (Value, Value)>::new();
        while cursor <= latest {
            let key = Self::leg_key(
                &leg.submission_id,
                &format!("evm_system_receipt_scan_{cursor}"),
            );
            let page = if let Some(saved) = self.credentials.papertrade_runtime_record(&key)? {
                saved
            } else {
                if fetched >= 100 {
                    return Ok(None);
                }
                let receipt_result = runtime
                    .rpc
                    .request(
                        "eth_getBlockReceiptsWithSystemTx",
                        json!([format!("0x{cursor:x}")]),
                    )
                    .await?;
                let receipts = rpc_result_rows(&receipt_result)
                    .filter(|rows| rows.len() <= 10_000)
                    .ok_or_else(|| {
                        unavailable("HyperEVM returned an unbounded system-receipt page.")
                    })?;
                let system_result = runtime
                    .rpc
                    .request(
                        "eth_getSystemTxsByBlockNumber",
                        json!([format!("0x{cursor:x}")]),
                    )
                    .await?;
                let system_txs = rpc_result_rows(&system_result)
                    .filter(|rows| rows.len() <= 10_000)
                    .ok_or_else(|| {
                        unavailable("HyperEVM returned an unbounded system-transaction page.")
                    })?;
                let matching_txs = system_txs
                    .iter()
                    .filter(|tx| {
                        hyperevm_core_wallet_transfer_matches(tx, &self.owner, amount_atoms)
                    })
                    .filter_map(|tx| {
                        tx.get("hash")
                            .or_else(|| tx.get("transactionHash"))
                            .and_then(Value::as_str)
                            .filter(|hash| valid_hash32(hash))
                            .map(|hash| (hash.to_ascii_lowercase(), tx))
                    })
                    .collect::<BTreeMap<_, _>>();
                let mut matching_pairs = Vec::new();
                for receipt in receipts {
                    let Some(hash) = receipt
                        .get("transactionHash")
                        .and_then(Value::as_str)
                        .filter(|hash| valid_hash32(hash))
                    else {
                        continue;
                    };
                    if !matching_txs.contains_key(&hash.to_ascii_lowercase()) {
                        continue;
                    }
                    let Some(logs) = receipt.get("logs").and_then(Value::as_array) else {
                        continue;
                    };
                    if hyperevm_core_wallet_receipt_logs(logs, &self.owner, amount_atoms).is_none()
                    {
                        continue;
                    }
                    let tx = matching_txs[&hash.to_ascii_lowercase()];
                    matching_pairs.push(json!({
                        "hash":hash,
                        "receipt":receipt,
                        "transaction":{
                            "hash":tx.get("hash").or_else(|| tx.get("transactionHash")),
                            "from":tx.get("from"),
                            "to":tx.get("to"),
                            "value":tx.get("value"),
                            "input":tx.get("input").or_else(|| tx.get("data")),
                            "blockHash":tx.get("blockHash"),
                            "blockNumber":tx.get("blockNumber"),
                            "transactionIndex":tx.get("transactionIndex")
                        }
                    }));
                }
                let page = json!({"version":1,"block_number":cursor,"candidates":matching_pairs});
                self.persist_immutable(&key, &page)?;
                fetched += 1;
                page
            };
            let block_number = page["block_number"].as_u64().ok_or_else(|| {
                GoFinError::validation("HyperEVM destination scan page is malformed.")
            })?;
            if block_number != cursor || block_number > latest {
                return Err(GoFinError::validation(
                    "Persisted HyperEVM destination scan page conflicts with its block identity.",
                ));
            }
            let pairs = page["candidates"].as_array().ok_or_else(|| {
                GoFinError::validation("HyperEVM destination scan page omitted its candidates.")
            })?;
            for pair in pairs {
                let Some(hash) = pair["hash"].as_str().filter(|hash| valid_hash32(hash)) else {
                    continue;
                };
                if candidates
                    .insert(
                        hash.to_ascii_lowercase(),
                        (pair["receipt"].clone(), pair["transaction"].clone()),
                    )
                    .is_some()
                {
                    return Ok(None);
                }
            }
            cursor = cursor.saturating_add(1);
        }
        let mut proofs = Vec::new();
        for (hash, (receipt, transaction)) in candidates {
            let confirmations = self.confirmations.get(&999).copied().unwrap_or(0);
            if confirmations == 0 {
                return Err(unavailable("HyperEVM finality is not configured."));
            }
            let Some(block_number) = rpc_u64(&receipt["blockNumber"]) else {
                continue;
            };
            let Some(block_hash) = receipt["blockHash"]
                .as_str()
                .filter(|hash| valid_hash32(hash))
            else {
                continue;
            };
            if receipt["transactionHash"]
                .as_str()
                .is_none_or(|actual| !actual.eq_ignore_ascii_case(&hash))
                || receipt["status"].as_str() != Some("0x1")
                || !hyperevm_core_wallet_transfer_matches(&transaction, &self.owner, amount_atoms)
                || rpc_u64(&transaction["blockNumber"]) != Some(block_number)
                || transaction["blockHash"]
                    .as_str()
                    .is_none_or(|actual| !actual.eq_ignore_ascii_case(block_hash))
            {
                continue;
            }
            let receipt_logs = receipt["logs"].as_array().ok_or_else(|| {
                GoFinError::validation("Canonical HyperEVM receipt omitted logs.")
            })?;
            let Some((transfer_log, withdraw_log)) =
                hyperevm_core_wallet_receipt_logs(receipt_logs, &self.owner, amount_atoms)
            else {
                continue;
            };
            let block = runtime
                .rpc
                .request(
                    "eth_getBlockByNumber",
                    json!([format!("0x{block_number:x}"), false]),
                )
                .await?;
            if block["hash"]
                .as_str()
                .is_none_or(|actual| !actual.eq_ignore_ascii_case(block_hash))
            {
                continue;
            }
            let finalized = runtime
                .rpc
                .request("eth_getBlockByNumber", json!(["finalized", false]))
                .await?;
            let Some(finalized_number) = rpc_u64(&finalized["number"]) else {
                continue;
            };
            if finalized_number < block_number
                || latest.saturating_add(1) < block_number.saturating_add(confirmations)
            {
                continue;
            }
            let timestamp_secs = block["timestamp"]
                .as_str()
                .and_then(|value| u64::from_str_radix(value.strip_prefix("0x")?, 16).ok());
            if timestamp_secs.is_none_or(|seconds| seconds < action_time_ms / 1_000) {
                continue;
            }
            proofs.push(json!({
                "transaction_hash":hash,
                "block_hash":block_hash,
                "block_number":block_number,
                "confirmations":latest.saturating_sub(block_number).saturating_add(1),
                "withdraw_log_index":withdraw_log["logIndex"],
                "transfer_log_index":transfer_log["logIndex"],
                "amount_atoms":amount_atoms
            }));
        }
        if proofs.len() != 1 {
            return Ok(None);
        }
        Ok(proofs.pop())
    }

    fn load_bound_plan(&self, operation_id: &str) -> Result<BoundFunding, GoFinError> {
        let value = self
            .credentials
            .papertrade_runtime_record(&Self::plan_key(operation_id))?
            .ok_or_else(|| unavailable("Papertrade funding plan is not persisted."))?;
        serde_json::from_value(value)
            .map_err(|_| GoFinError::validation("Papertrade funding plan is malformed."))
    }

    async fn reconcile_core_cctp_withdrawal(
        &self,
        plan: &BoundFunding,
        leg: &FundingLeg,
        action: &gofin_hyperliquid::funding::SendToEvmWithDataAction,
    ) -> Result<FundingLegState, GoFinError> {
        let settled_key = Self::leg_key(&leg.submission_id, "settled_proof");
        if let Some(saved) = self.credentials.papertrade_runtime_record(&settled_key)? {
            if saved.get("operation_id").and_then(Value::as_str) != Some(&plan.operation_id)
                || value_u64(&saved["nonce"]) != Some(action.nonce)
                || !saved["source_hash"].as_str().is_some_and(valid_hash32)
                || !saved["core_action_hash"].as_str().is_some_and(valid_hash32)
                || !(saved["forward_transaction_hash"]
                    .as_str()
                    .is_some_and(valid_hash32)
                    || saved["forward_signature"]
                        .as_str()
                        .is_some_and(|signature| {
                            bs58::decode(signature).into_vec().is_ok_and(|bytes| {
                                bytes.len() == 64 && bs58::encode(&bytes).into_string() == signature
                            })
                        }))
                || saved["destination_credit_usdc_atoms"]
                    .as_str()
                    .and_then(|value| value.parse::<u64>().ok())
                    .is_none_or(|amount| amount < plan.minimum_received_usdc_atoms)
                || value_u64(&saved["core_action_fee_atoms"]).is_none_or(|fee| {
                    fee > plan
                        .bounds
                        .maximum_source_fee_usdc_atoms
                        .min(CORE_CCTP_RETAINED_RESERVE_ATOMS)
                })
                || value_u64(&saved["core_native_fee_atoms"]).is_none_or(|fee| {
                    fee > plan
                        .bounds
                        .maximum_native_fee_atoms
                        .as_deref()
                        .and_then(|value| value.parse::<u64>().ok())
                        .unwrap_or(0)
                })
                || (value_u64(&saved["core_native_fee_atoms"]) != Some(0)
                    && saved["core_native_fee_asset"].as_str()
                        != plan.bounds.native_fee_asset.as_deref())
                || value_u64(&saved["cctp_fee_usdc_atoms"])
                    .is_none_or(|fee| fee > plan.bounds.maximum_cctp_fee_usdc_atoms)
                || value_u64(&saved["retained_core_usdc_atoms"]).is_none_or(|retained| {
                    value_u64(&saved["core_action_fee_atoms"])
                        .and_then(|fee| CORE_CCTP_RETAINED_RESERVE_ATOMS.checked_sub(fee))
                        != Some(retained)
                })
            {
                return Err(GoFinError::validation(
                    "Persisted Core CCTP settlement evidence conflicts with its immutable route.",
                ));
            }
            return Ok(FundingLegState::Settled { evidence: saved });
        }
        let attempted_key = Self::leg_key(&leg.submission_id, "dispatch_attempted");
        let Some(attempt) = self.credentials.papertrade_runtime_record(&attempted_key)? else {
            return Ok(FundingLegState::NotSubmitted);
        };
        let attempt_ms = attempt
            .get("attempted_at_ms")
            .and_then(Value::as_u64)
            .ok_or_else(|| unavailable("Core CCTP dispatch timestamp is unavailable."))?;
        let end_ms = now_ms()?;
        let updates = self
            .hyperliquid_transport
            .info(
                gofin_exchange::ccxt::HyperliquidInfoRequest::UserNonFundingLedgerUpdates {
                    owner: self.hyperliquid_account.owner.to_ascii_lowercase(),
                    start_time_ms: attempt_ms,
                    end_time_ms: Some(end_ms),
                },
            )
            .await?;
        let rows = updates
            .as_array()
            .or_else(|| updates.get("rows").and_then(Value::as_array))
            .filter(|rows| rows.len() <= 1_000)
            .ok_or_else(|| unavailable("Core CCTP ledger response is malformed or oversized."))?;
        let candidates = rows
            .iter()
            .filter(|row| {
                let delta = &row["delta"];
                row["time"]
                    .as_u64()
                    .is_some_and(|time| time >= attempt_ms && time <= end_ms)
                    && delta["type"].as_str() == Some("send")
                    && delta["user"]
                        .as_str()
                        .is_some_and(|value| value.eq_ignore_ascii_case(&self.owner))
                    && delta["destination"].as_str().is_some_and(|value| {
                        value.eq_ignore_ascii_case(gofin_hyperliquid::funding::USDC_SYSTEM_ADDRESS)
                    })
                    && delta["sourceDex"].as_str() == Some("spot")
                    && delta["destinationDex"].as_str() == Some("spot")
                    && delta["token"].as_str() == Some("USDC")
                    && value_u64(&delta["nonce"]) == Some(action.nonce)
                    && delta["amount"].as_str().is_some_and(|value| {
                        gofin_domain::Quantity::from_decimal(value, 6).is_ok_and(|parsed| {
                            parsed
                                == gofin_domain::Quantity::from_decimal(&action.amount, 6)
                                    .unwrap_or_default()
                        })
                    })
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"original Core CCTP action is not yet observed in the owner ledger"}),
            });
        }
        if candidates.len() != 1 {
            return Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"multiple Core CCTP ledger actions match the immutable nonce"}),
            });
        }
        let ledger = candidates[0];
        let ledger_hash = ledger["hash"]
            .as_str()
            .ok_or_else(|| unavailable("Core CCTP ledger action omitted its transaction hash."))?;
        let explorer = self
            .hyperliquid_transport
            .explorer(
                gofin_exchange::ccxt::HyperliquidExplorerRequest::TxDetails {
                    hash: ledger_hash.to_owned(),
                },
            )
            .await?;
        if explorer["error"] != Value::Null
            || explorer["tx"]["hash"]
                .as_str()
                .is_none_or(|value| !value.eq_ignore_ascii_case(ledger_hash))
            || explorer["tx"]["user"]
                .as_str()
                .is_none_or(|value| !value.eq_ignore_ascii_case(&self.owner))
        {
            return Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"Core CCTP explorer evidence does not identify the exact owner ledger action"}),
            });
        }
        let explorer_action: gofin_hyperliquid::funding::SendToEvmWithDataAction =
            serde_json::from_value(explorer["tx"]["action"].clone())
                .map_err(|_| unavailable("Core CCTP explorer action schema is malformed."))?;
        if explorer_action != *action {
            return Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"Core CCTP explorer action differs from the exact persisted action"}),
            });
        }
        let native_fee_cap = plan
            .bounds
            .maximum_native_fee_atoms
            .as_deref()
            .filter(|_| plan.bounds.native_fee_asset.as_deref() == Some("hyperliquid:mainnet:HYPE"))
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        let Some((fee_atoms, native_fee_atoms)) = Self::verify_core_cctp_fee_row(
            &ledger["delta"],
            &self.owner,
            expected_bridge_amount(plan)?,
            action.nonce,
            plan.bounds
                .maximum_source_fee_usdc_atoms
                .min(CORE_CCTP_RETAINED_RESERVE_ATOMS),
            native_fee_cap,
        ) else {
            return Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"Core CCTP source ledger fee mode or exact amount is not proven within the bound"}),
            });
        };

        let source_credit = self
            .credentials
            .papertrade_runtime_record(&Self::leg_key(
                &format!("papertrade-funding:{}:0", plan.operation_id),
                "core_credit_proof",
            ))?
            .ok_or_else(|| unavailable("Core withdrawal source receipt proof is missing."))?;
        let expected_papertrade_hash = source_credit
            .get("transaction_hash")
            .and_then(Value::as_str)
            .ok_or_else(|| unavailable("Core withdrawal source receipt hash is missing."))?;
        let source_key = Self::leg_key(&leg.submission_id, "core_cctp_source_proof");
        let source_evidence = json!({
            "version":1,
            "operation_id":plan.operation_id,
            "nonce":action.nonce,
            "core_action_hash":ledger_hash,
            "core_action_time_ms":ledger["time"],
            "core_action_fee_atoms":fee_atoms,
            "core_native_fee_atoms":native_fee_atoms,
            "core_native_fee_asset":(native_fee_atoms > 0).then_some("hyperliquid:mainnet:HYPE"),
            "retained_core_usdc_atoms":CORE_CCTP_RETAINED_RESERVE_ATOMS.saturating_sub(fee_atoms),
            "action":action,
            "explorer_action":explorer_action,
            "papertrade_transaction_hash":expected_papertrade_hash
        });
        self.persist_immutable(&source_key, &source_evidence)?;
        let route_records = self.papertrade_http.funding_records(&self.owner).await?;
        let items = route_records
            .get("items")
            .and_then(Value::as_array)
            .filter(|items| items.len() <= 1_000)
            .ok_or_else(|| unavailable("Papertrade funding history is malformed or oversized."))?;
        let route_name = match plan.intent.route {
            FundingRoute::Ethereum => "ethereum",
            FundingRoute::Arbitrum => "arbitrum",
            FundingRoute::Solana => "solana",
            _ => return Err(GoFinError::validation("CCTP withdrawal route is invalid.")),
        };
        let records = items
            .iter()
            .filter(|item| {
                item["route"].as_str() == Some(route_name)
                    && item["destination"].as_str().is_some_and(|value| {
                        if plan.intent.route == FundingRoute::Solana {
                            value
                                == plan
                                    .destination_identity
                                    .strip_prefix("solana:mainnet-beta:")
                                    .unwrap_or("")
                        } else {
                            value.eq_ignore_ascii_case(&self.owner)
                        }
                    })
                    && value_u64(&item["amount"]) == Some(plan.intent.amount_usdc_atoms)
                    && item["exchangeTxHash"]
                        .as_str()
                        .is_some_and(|value| value.eq_ignore_ascii_case(expected_papertrade_hash))
                    && value_u64(&item["onward"]["nonce"]) == Some(action.nonce)
            })
            .collect::<Vec<_>>();
        if records.len() != 1 {
            return Ok(FundingLegState::Unknown {
                evidence: json!({"reason":if records.is_empty() {"Papertrade funding index does not yet corroborate the exact outbound Core nonce"} else {"multiple Papertrade funding records match the source nonce"}}),
            });
        }
        let burn_hash = records[0]["burnTxHash"]
            .as_str()
            .filter(|hash| valid_hash32(hash))
            .ok_or_else(|| {
                unavailable("Papertrade funding record omitted a canonical CCTP source hash.")
            })?;
        let attestation = self
            .cctp_settlement
            .fetch_attestation(gofin_bridge::cctp::hypercore::DOMAIN, burn_hash)
            .await?;
        let gofin_bridge::cctp::CctpAttestationStatus::Ready { messages } = attestation else {
            return Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"Circle has not completed attestation for the exact Core CCTP source"}),
            });
        };
        let matching_messages = messages
            .into_iter()
            .filter(|message| {
                message
                    .source_transaction_hash
                    .eq_ignore_ascii_case(burn_hash)
            })
            .collect::<Vec<_>>();
        if matching_messages.len() != 1 {
            return Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"Circle attestation did not uniquely identify the exact Core source hash"}),
            });
        }
        let message = &matching_messages[0];
        let quote = plan.cctp_quote.as_ref().ok_or_else(|| {
            GoFinError::validation("Outbound Core route omitted its immutable Circle quote.")
        })?;
        let message_body = gofin_bridge::cctp::decode_message(&message.message)?;
        let quote_fee = quote
            .metadata
            .max_fee
            .as_str()
            .parse::<u64>()
            .map_err(|_| GoFinError::validation("CCTP quote fee is malformed."))?;
        let expected_domain = match plan.intent.route {
            FundingRoute::Ethereum => 0,
            FundingRoute::Arbitrum => 3,
            FundingRoute::Solana => 5,
            _ => unreachable!(),
        };
        if message_body.source_domain != gofin_bridge::cctp::hypercore::DOMAIN
            || message_body.destination_domain != expected_domain
            || message_body.amount
                != gofin_domain::Quantity::from_u64(expected_bridge_amount(plan)?)
            || message_body.nonce != message.nonce
            || message_body.max_fee != quote.metadata.max_fee
            || value_u64(&records[0]["onward"]["maximumFee"]) != Some(quote_fee)
        {
            return Err(GoFinError::validation(
                "Circle's attested Core message differs from the exact approved destination amount or quote.",
            ));
        }
        let Some(forward_hash) = message.forward_tx_hash.as_deref() else {
            return Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"Circle has no completed destination-forward transaction hash yet"}),
            });
        };
        if expected_domain == 5 {
            let Some(runtime) = self.solana_runtime.as_ref() else {
                return Ok(FundingLegState::Unknown {
                    evidence: json!({"reason":"No enrolled Solana receipt runtime is available for destination proof"}),
                });
            };
            let recipient = plan
                .destination_identity
                .strip_prefix("solana:mainnet-beta:")
                .ok_or_else(|| {
                    GoFinError::validation("Solana destination identity is malformed.")
                })?;
            let recipient = gofin_solana::parse_address(recipient)?;
            let token_account = gofin_solana::associated_token_address(
                &recipient,
                gofin_solana::funding::MAINNET_USDC_MINT,
                gofin_solana::token::TOKEN_PROGRAM_ID,
            )?;
            let destination = gofin_bridge::SolanaDestination {
                network: gofin_solana::NETWORK.to_owned(),
                wallet: None,
                wallet_resource: gofin_domain::ResourceId(format!(
                    "{}:{recipient}",
                    gofin_solana::NETWORK
                )),
                wallet_address: recipient.clone(),
                recipient: recipient.clone(),
                asset: gofin_bridge::cctp::solana::asset(),
                recipient_token_account: token_account,
                destination_caller: recipient.clone(),
            };
            let proof = match runtime.observe_finalized_proof(forward_hash).await {
                Ok(proof) => proof,
                Err(_) => {
                    return Ok(FundingLegState::Unknown {
                        evidence: json!({"reason":"Circle forward signature is not yet finalized on Solana"}),
                    });
                }
            };
            let proof = self
                .cctp_settlement
                .verify_hypercore_outbound_solana(
                    &quote.metadata,
                    burn_hash,
                    &message.message,
                    &message.attestation,
                    forward_hash,
                    &destination,
                    &self.owner,
                    expected_bridge_amount(plan)?,
                    action.nonce,
                    &proof,
                )
                .await?;
            let evidence = json!({
                "kind":"settled_hypercore_cctp_withdrawal",
                "operation_id":plan.operation_id,
                "source_hash":burn_hash,
                "core_action_hash":ledger_hash,
                "core_action_time_ms":ledger["time"],
                "core_action_fee_atoms":fee_atoms,
                "core_native_fee_atoms":native_fee_atoms,
                "core_native_fee_asset":(native_fee_atoms > 0).then_some("hyperliquid:mainnet:HYPE"),
                "papertrade_transaction_hash":expected_papertrade_hash,
                "forward_signature":proof.signature,
                "destination_slot":proof.slot,
                "destination_credit_usdc_atoms":proof.net_atoms,
                "cctp_fee_usdc_atoms":proof.fee_atoms,
                "retained_core_usdc_atoms":CORE_CCTP_RETAINED_RESERVE_ATOMS - fee_atoms,
                "destination_token_account":proof.recipient_token_account,
                "core_nonce":action.nonce,
                "circle_nonce":proof.nonce
            });
            self.persist_immutable(
                &Self::leg_key(&leg.submission_id, "settled_proof"),
                &evidence,
            )?;
            return Ok(FundingLegState::Settled { evidence });
        }
        let chain_id = match expected_domain {
            0 => 1,
            3 => 42161,
            _ => {
                return Err(GoFinError::validation(
                    "Unsupported Papertrade CCTP destination domain.",
                ));
            }
        };
        let Some(runtime) = self.evm_runtimes.get(&chain_id) else {
            return Err(unavailable(
                "The configured destination-chain EVM runtime is missing.",
            ));
        };
        let confirmations = *self
            .confirmations
            .get(&chain_id)
            .ok_or_else(|| unavailable("Destination finality policy is not configured."))?;
        let Some(receipt) = runtime
            .observe_receipt(forward_hash, confirmations, true)
            .await?
        else {
            return Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"Circle forward transaction is not yet canonical and finalized"}),
            });
        };
        let proof = self
            .cctp_settlement
            .verify_hypercore_outbound_evm(
                &quote.metadata,
                &message.message,
                &message.attestation,
                chain_id,
                &self.owner,
                expected_bridge_amount(plan)?,
                action.nonce,
                &receipt,
            )
            .await?;
        let evidence = json!({
            "kind":"settled_hypercore_cctp_withdrawal",
            "operation_id":plan.operation_id,
            "source_hash":burn_hash,
            "core_action_hash":ledger_hash,
            "core_action_time_ms":ledger["time"],
            "core_action_fee_atoms":fee_atoms,
            "retained_core_usdc_atoms":CORE_CCTP_RETAINED_RESERVE_ATOMS - fee_atoms,
            "papertrade_transaction_hash":expected_papertrade_hash,
            "forward_transaction_hash":proof.transaction_hash,
            "destination_block_hash":proof.block_hash,
            "destination_block_number":proof.block_number,
            "destination_confirmations":proof.confirmations,
            "destination_credit_usdc_atoms":proof.net_atoms,
            "cctp_fee_usdc_atoms":proof.fee_atoms,
            "core_nonce":action.nonce,
            "circle_nonce":proof.nonce
        });
        self.persist_immutable(
            &Self::leg_key(&leg.submission_id, "settled_proof"),
            &evidence,
        )?;
        Ok(FundingLegState::Settled { evidence })
    }

    async fn prove_inbound_hypercore_deposit(
        &self,
        plan: &BoundFunding,
        source: &Value,
    ) -> Result<Option<FundingReceipt>, GoFinError> {
        if plan.proxy_core_usdc_before_atoms != Some(0)
            || plan.core_sweep_from_block.is_none()
            || source.get("amount_atoms").and_then(Value::as_u64)
                != Some(plan.intent.amount_usdc_atoms)
            || !source
                .get("operation_id")
                .and_then(Value::as_str)
                .is_some_and(|id| id == plan.operation_id)
        {
            return Err(GoFinError::validation(
                "Inbound CCTP source and empty HyperCore proxy baseline are not bound to this funding plan.",
            ));
        }
        let quote = plan.cctp_quote.as_ref().ok_or_else(|| {
            GoFinError::validation("Inbound CCTP plan omitted its persisted quote.")
        })?;
        let source_domain = match plan.intent.route {
            FundingRoute::Ethereum => 0,
            FundingRoute::Arbitrum => 3,
            FundingRoute::Solana => 5,
            _ => return Ok(None),
        };
        let source_hash = match plan.intent.route {
            FundingRoute::Solana => source.get("signature").and_then(Value::as_str),
            _ => source.get("transaction_hash").and_then(Value::as_str),
        }
        .ok_or_else(|| {
            GoFinError::validation("Canonical inbound CCTP source identity is missing.")
        })?;
        let source_message = source
            .pointer("/source_message/raw_message")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                GoFinError::validation("Canonical inbound CCTP source message is missing.")
            })?;
        let status = self
            .cctp_settlement
            .fetch_attestation(source_domain, source_hash)
            .await?;
        let gofin_bridge::cctp::CctpAttestationStatus::Ready { messages } = status else {
            return Ok(None);
        };
        let messages = messages
            .into_iter()
            .filter(|message| {
                if source_domain == 5 {
                    message.source_transaction_hash == source_hash
                } else {
                    message
                        .source_transaction_hash
                        .eq_ignore_ascii_case(source_hash)
                }
            })
            .collect::<Vec<_>>();
        if messages.len() != 1 {
            return Ok(None);
        }
        let message = &messages[0];
        let Some(forward_hash) = message.forward_tx_hash.as_deref() else {
            return Ok(None);
        };
        if !valid_hash32(forward_hash) {
            return Err(GoFinError::validation(
                "Circle's HyperCore forward transaction identity is not a canonical EVM hash.",
            ));
        }
        let runtime = self.evm_runtimes.get(&999).ok_or_else(|| {
            unavailable("The configured HyperEVM observer is missing for CCTP forwarding proof.")
        })?;
        let Some(forward_receipt) = runtime
            .observe_receipt(
                forward_hash,
                *self.confirmations.get(&999).unwrap_or(&1),
                true,
            )
            .await?
        else {
            return Ok(None);
        };
        let destination = if plan.intent.route == FundingRoute::Solana {
            let FundingAction::SolanaCctpBurn { request, .. } = &plan.legs[0].action else {
                return Err(GoFinError::validation(
                    "Solana inbound plan has no frozen burn request.",
                ));
            };
            self.cctp_settlement
                .verify_solana_hypercore_forward(
                    source_message,
                    request,
                    &quote.metadata,
                    &message.message,
                    &message.attestation,
                    &plan.proxy,
                    &forward_receipt,
                )
                .await?
        } else {
            let source_chain_id = if plan.intent.route == FundingRoute::Ethereum {
                1
            } else {
                42161
            };
            self.cctp_settlement
                .verify_hypercore_forward(
                    source_message,
                    &quote.metadata,
                    &message.message,
                    &message.attestation,
                    source_chain_id,
                    &self.owner,
                    plan.intent.amount_usdc_atoms,
                    &plan.proxy,
                    &forward_receipt,
                )
                .await?
        };
        let gross = destination.gross_atoms.parse::<u64>().map_err(|_| {
            GoFinError::validation("CCTP gross amount exceeds the supported range.")
        })?;
        let cctp_fee = destination
            .fee_atoms
            .parse::<u64>()
            .map_err(|_| GoFinError::validation("CCTP fee exceeds the supported range."))?;
        let net = destination
            .net_atoms
            .parse::<u64>()
            .map_err(|_| GoFinError::validation("CCTP net amount exceeds the supported range."))?;
        if gross != plan.intent.amount_usdc_atoms
            || cctp_fee > plan.bounds.maximum_cctp_fee_usdc_atoms
            || net.checked_add(cctp_fee) != Some(gross)
            || net < plan.bounds.maximum_activation_fee_atoms
            || net - plan.bounds.maximum_activation_fee_atoms
                < plan.bounds.destination_min_usdc_atoms
        {
            return Err(GoFinError::validation(
                "CCTP forward amount or bounded fee does not satisfy the exact HyperCore credit floor.",
            ));
        }
        let forward_block = runtime
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([format!("0x{:x}", forward_receipt.block_number), false]),
            )
            .await?;
        let forward_time_ms = forward_block
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|value| u64::from_str_radix(value.strip_prefix("0x")?, 16).ok())
            .and_then(|seconds| seconds.checked_mul(1_000))
            .ok_or_else(|| unavailable("Finalized CCTP forward block timestamp is unavailable."))?;
        let latest_time_ms = now_ms()?;
        let ledger = self
            .hyperliquid_transport
            .info(
                gofin_exchange::ccxt::HyperliquidInfoRequest::UserNonFundingLedgerUpdates {
                    owner: plan.proxy.to_ascii_lowercase(),
                    start_time_ms: forward_time_ms.saturating_sub(2_000),
                    end_time_ms: Some(latest_time_ms),
                },
            )
            .await?;
        let rows = ledger
            .as_array()
            .or_else(|| ledger.get("rows").and_then(Value::as_array))
            .filter(|rows| rows.len() <= 1_000)
            .ok_or_else(|| {
                unavailable("HyperCore proxy funding ledger is malformed or oversized.")
            })?;
        let system = "0x2000000000000000000000000000000000000000";
        let exchange = gofin_papertrade::EXCHANGE;
        let incoming = rows
            .iter()
            .filter(|row| {
                row["delta"]["type"].as_str() == Some("spotTransfer")
                    && row["delta"]["user"]
                        .as_str()
                        .is_some_and(|value| value.eq_ignore_ascii_case(system))
                    && row["delta"]["destination"]
                        .as_str()
                        .is_some_and(|value| value.eq_ignore_ascii_case(&plan.proxy))
                    && row["delta"]["token"].as_str() == Some("USDC")
                    && row["delta"]["amount"].as_str().and_then(decimal_usdc_atoms) == Some(net)
            })
            .collect::<Vec<_>>();
        if incoming.len() != 1 {
            return Ok(None);
        }
        let incoming = incoming[0];
        let incoming_hash = incoming["hash"]
            .as_str()
            .filter(|hash| valid_hash32(hash))
            .ok_or_else(|| {
                unavailable("HyperCore CCTP credit omitted its canonical ledger hash.")
            })?;
        let inbound_details = self
            .hyperliquid_transport
            .explorer(
                gofin_exchange::ccxt::HyperliquidExplorerRequest::TxDetails {
                    hash: incoming_hash.into(),
                },
            )
            .await?;
        if !cctp_spot_transfer_matches(
            incoming,
            &inbound_details,
            system,
            &plan.proxy,
            net,
            forward_time_ms.saturating_sub(2_000),
            latest_time_ms,
        ) {
            return Ok(None);
        }
        let activation_rows = rows
            .iter()
            .filter(|row| {
                row["hash"]
                    .as_str()
                    .is_some_and(|hash| hash.eq_ignore_ascii_case(incoming_hash))
                    && row["delta"]["type"].as_str() == Some("accountActivationGas")
                    && row["delta"]["token"].as_str() == Some("USDC")
            })
            .collect::<Vec<_>>();
        let activation_fee = match activation_rows.as_slice() {
            [] => 0,
            [row] => row["delta"]["amount"].as_str().and_then(decimal_usdc_atoms)
                .filter(|fee| *fee <= plan.bounds.maximum_activation_fee_atoms)
                .ok_or_else(|| GoFinError::validation("HyperCore activation fee exceeds its reviewed bound or has invalid identity."))?,
            _ => return Ok(None),
        };
        let ledger_fee = incoming["delta"]["fee"]
            .as_str()
            .and_then(decimal_usdc_atoms)
            .ok_or_else(|| unavailable("HyperCore CCTP credit fee is malformed."))?;
        if ledger_fee != activation_fee
            || incoming["delta"]["feeToken"].as_str().unwrap_or("")
                != if activation_fee == 0 { "" } else { "USDC" }
            || incoming["delta"]["nativeTokenFee"]
                .as_str()
                .and_then(decimal_usdc_atoms)
                .unwrap_or(u64::MAX)
                != 0
        {
            return Err(GoFinError::validation(
                "HyperCore CCTP activation fee does not match its exact USDC ledger charge.",
            ));
        }
        let swept_amount = net.checked_sub(activation_fee).ok_or_else(|| {
            GoFinError::validation("HyperCore activation fee exceeds the forwarded credit.")
        })?;
        let proxy_rows = rows
            .iter()
            .filter(|row| {
                row["delta"]["type"].as_str() == Some("spotTransfer")
                    && (row["delta"]["user"]
                        .as_str()
                        .is_some_and(|value| value.eq_ignore_ascii_case(&plan.proxy))
                        || row["delta"]["destination"]
                            .as_str()
                            .is_some_and(|value| value.eq_ignore_ascii_case(&plan.proxy)))
            })
            .collect::<Vec<_>>();
        if proxy_rows.len() != 2 {
            return Ok(None);
        }
        let outgoing = proxy_rows.iter().copied().find(|row| {
            row["delta"]["user"]
                .as_str()
                .is_some_and(|value| value.eq_ignore_ascii_case(&plan.proxy))
                && row["delta"]["destination"]
                    .as_str()
                    .is_some_and(|value| value.eq_ignore_ascii_case(exchange))
                && row["delta"]["token"].as_str() == Some("USDC")
                && row["delta"]["amount"].as_str().and_then(decimal_usdc_atoms)
                    == Some(swept_amount)
        });
        let Some(outgoing) = outgoing else {
            return Ok(None);
        };
        if outgoing["time"]
            .as_u64()
            .is_none_or(|time| time < incoming["time"].as_u64().unwrap_or(u64::MAX))
        {
            return Ok(None);
        }
        let outgoing_hash = outgoing["hash"]
            .as_str()
            .filter(|hash| valid_hash32(hash))
            .ok_or_else(|| {
                unavailable("HyperCore proxy sweep transfer omitted its canonical ledger hash.")
            })?;
        let outgoing_details = self
            .hyperliquid_transport
            .explorer(
                gofin_exchange::ccxt::HyperliquidExplorerRequest::TxDetails {
                    hash: outgoing_hash.into(),
                },
            )
            .await?;
        if !cctp_spot_transfer_matches(
            outgoing,
            &outgoing_details,
            &plan.proxy,
            exchange,
            swept_amount,
            incoming["time"].as_u64().unwrap_or(forward_time_ms),
            latest_time_ms,
        ) || outgoing["delta"]["fee"]
            .as_str()
            .and_then(decimal_usdc_atoms)
            != Some(0)
            || outgoing["delta"]["nativeTokenFee"]
                .as_str()
                .and_then(decimal_usdc_atoms)
                != Some(0)
        {
            return Ok(None);
        }
        let source_leg = plan
            .legs
            .first()
            .ok_or_else(|| GoFinError::validation("Inbound CCTP plan omitted its source leg."))?;
        let runtime = self.evm_runtimes.get(&999).ok_or_else(|| {
            unavailable("HyperEVM observer is unavailable for Papertrade settlement.")
        })?;
        let latest = runtime
            .rpc
            .request("eth_blockNumber", json!([]))
            .await?
            .as_str()
            .and_then(|raw| u64::from_str_radix(raw.strip_prefix("0x")?, 16).ok())
            .ok_or_else(|| unavailable("HyperEVM returned an invalid latest block."))?;
        let from = plan.core_sweep_from_block.unwrap();
        if from > latest {
            return Ok(None);
        }
        let topic_owner = owner_topic(&plan.intent.owner)?;
        let mut candidates = Vec::new();
        let mut page_start = from;
        let mut fetched_page_count = 0usize;
        while page_start <= latest {
            let query_end = latest.min(page_start.saturating_add(199));
            let page_key = Self::leg_key(
                &source_leg.submission_id,
                &format!("sweep_page_{page_start}"),
            );
            let matches = if let Some(saved) =
                self.credentials.papertrade_runtime_record(&page_key)?
            {
                if saved["from"].as_u64() != Some(page_start)
                    || saved["to"].as_u64() != Some(query_end)
                {
                    return Err(GoFinError::validation(
                        "Persisted CCTP sweep scan page conflicts with its range.",
                    ));
                }
                saved["matches"].as_array().cloned().unwrap_or_default()
            } else {
                if fetched_page_count >= 100 {
                    return Ok(None);
                }
                let response = runtime.rpc.request("eth_getLogs", json!([{
                    "address": exchange,
                    "fromBlock": format!("0x{page_start:x}"),
                    "toBlock": format!("0x{query_end:x}"),
                    "topics": ["0xc36b5179cb9c303b200074996eab2b3473eac370fdd7eba3bec636fe35109696", topic_owner]
                }])).await?;
                let logs = response.as_array().ok_or_else(|| {
                    unavailable("HyperEVM returned malformed Papertrade sweep logs.")
                })?;
                fetched_page_count += 1;
                let filtered = logs
                    .iter()
                    .filter(|log| {
                        log["data"]
                            .as_str()
                            .and_then(|data| crate::papertrade_funding::event_u128(data, 0))
                            .is_some_and(|wad| {
                                wad % 1_000_000_000_000 == 0
                                    && wad / 1_000_000_000_000 == u128::from(swept_amount)
                            })
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if query_end == page_start.saturating_add(199) {
                    let saved =
                        json!({"version":1,"from":page_start,"to":query_end,"matches":filtered});
                    self.persist_immutable(&page_key, &saved)?;
                    saved["matches"].as_array().cloned().unwrap_or_default()
                } else {
                    filtered
                }
            };
            candidates.extend(matches);
            if candidates.len() > 1 {
                break;
            }
            page_start = query_end.saturating_add(1);
        }
        candidates.retain(|row| event_position(row).is_some_and(|(block, _)| block >= from));
        if candidates.len() != 1 {
            return Ok(None);
        }
        let swept_log = &candidates[0];
        let Some((sweep_block, sweep_tx_index)) = event_position(swept_log) else {
            return Ok(None);
        };
        if sweep_block < forward_receipt.block_number
            || (sweep_block == forward_receipt.block_number
                && rpc_u64(&json!(swept_log["transactionIndex"])).is_some_and(|index| {
                    index
                        <= rpc_u64(&json!(forward_receipt.receipt["transactionIndex"])).unwrap_or(0)
                }))
        {
            return Ok(None);
        }
        let sweep_hash = swept_log["transactionHash"]
            .as_str()
            .filter(|hash| valid_hash32(hash))
            .ok_or_else(|| unavailable("Papertrade Swept log omitted its transaction hash."))?;
        let sweep_tx = runtime
            .rpc
            .request("eth_getTransactionByHash", json!([sweep_hash]))
            .await?;
        if !sweep_finalize_matches(&sweep_tx, &plan.intent.owner) {
            return Ok(None);
        }
        let Some(sweep_receipt) = runtime
            .observe_receipt(
                sweep_hash,
                *self.confirmations.get(&999).unwrap_or(&1),
                true,
            )
            .await?
        else {
            return Ok(None);
        };
        let swept = crate::papertrade_funding::verify_swept_event(
            &sweep_receipt,
            exchange,
            &plan.intent.owner,
            swept_amount,
        )?;
        if swept.usdc_atoms != swept_amount || swept_amount < plan.bounds.destination_min_usdc_atoms
        {
            return Err(GoFinError::validation(
                "Canonical Papertrade sweep differs from the exact CCTP Core credit.",
            ));
        }
        let native_fee = match plan.intent.route {
            FundingRoute::Solana => source
                .get("network_fee_lamports")
                .and_then(Value::as_u64)
                .map(|v| v.to_string()),
            _ => source
                .get("native_fee_atoms")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        };
        let native_asset = plan.bounds.native_fee_asset.clone();
        let retained_rent = if plan.intent.route == FundingRoute::Solana {
            source
                .get("retained_message_account_rent_lamports")
                .and_then(Value::as_u64)
                .map(|value| value.to_string())
        } else {
            None
        };
        let receipt = FundingReceipt {
            stage: gofin_papertrade::FundingReceiptStage::FinalDestination,
            status: "settled".into(),
            amount_usdc_atoms: swept_amount,
            source_debit_usdc_atoms: gross,
            destination_credit_usdc_atoms: swept_amount,
            actual_source_fee_usdc_atoms: 0,
            actual_cctp_fee_usdc_atoms: cctp_fee,
            activation_fee_usdc_atoms: activation_fee,
            actual_native_fee_atoms: native_fee,
            actual_native_fee_asset: native_asset,
            retained_source_credit_usdc_atoms: 0,
            retained_source_native_atoms: retained_rent.clone(),
            retained_source_native_asset: retained_rent
                .map(|_| "solana:mainnet-beta:native".to_owned()),
            source_identity: plan.source_identity.clone(),
            destination_identity: plan.destination_identity.clone(),
            evidence_id: format!(
                "papertrade-cctp-deposit:{}:{}:{}:{}",
                source_hash, destination.nonce, destination.transaction_hash, sweep_hash
            ),
        };
        self.persist_immutable(
            &Self::leg_key(&source_leg.submission_id, "inbound_hypercore_settled_proof"),
            &json!({
                "version":1,
                "source_hash":source_hash,
                "attested_nonce":destination.nonce,
                "forward_transaction_hash":destination.transaction_hash,
                "forward_block_hash":destination.block_hash,
                "forward_block_number":destination.block_number,
                "core_credit_hash":incoming_hash,
                "core_credit_amount_atoms":net,
                "activation_fee_atoms":activation_fee,
                "core_proxy_sweep_hash":outgoing_hash,
                "swept_event_hash":sweep_hash,
                "swept_event_block":sweep_block,
                "swept_event_transaction_index":sweep_tx_index,
                "destination_credit_atoms":swept_amount,
                "cctp_fee_atoms":cctp_fee
            }),
        )?;
        self.persist_immutable(
            &Self::leg_key(&source_leg.submission_id, "settled_proof"),
            &serde_json::to_value(&receipt)
                .map_err(|_| GoFinError::validation("Funding receipt could not be persisted."))?,
        )?;
        Ok(Some(receipt))
    }
}

#[async_trait::async_trait]
impl PapertradeFundingServices for PapertradeFundingRoutes {
    fn attach_runtime(
        &self,
        runtime: Weak<crate::papertrade_runtime::BrokerPapertradeRuntime>,
    ) -> Result<(), GoFinError> {
        let mut handle = self
            .runtime
            .lock()
            .map_err(|_| unavailable("Papertrade funding runtime handle is poisoned."))?;
        if handle.as_ref().and_then(Weak::upgrade).is_some() {
            return Err(GoFinError::validation(
                "Papertrade funding runtime was already attached.",
            ));
        }
        if runtime.upgrade().is_none() {
            return Err(GoFinError::validation(
                "Cannot attach an expired Papertrade funding runtime.",
            ));
        }
        *handle = Some(runtime);
        Ok(())
    }

    async fn preflight(&self, intent: &FundingIntent) -> Result<FundingPreflight, GoFinError> {
        let inbound_cctp = intent.direction == FundingDirection::Deposit
            && matches!(
                intent.route,
                FundingRoute::Ethereum | FundingRoute::Arbitrum | FundingRoute::Solana
            );
        let hyperevm_withdrawal = intent.direction == FundingDirection::Withdraw
            && intent.route == FundingRoute::Hyperevm;
        if !(inbound_cctp || hyperevm_withdrawal) || !intent.owner.eq_ignore_ascii_case(&self.owner)
        {
            return Self::unavailable();
        }
        let account_mode = gofin_hyperliquid::read_account_mode(
            self.hyperliquid_transport.as_ref(),
            &self.hyperliquid_account,
        )
        .await?;
        let account_mode = match account_mode.mode {
            gofin_hyperliquid::AccountMode::Separate => {
                gofin_hyperliquid::funding::HyperliquidAccountMode::Default
            }
            gofin_hyperliquid::AccountMode::Unified => {
                gofin_hyperliquid::funding::HyperliquidAccountMode::UnifiedAccount
            }
            gofin_hyperliquid::AccountMode::PortfolioMargin
            | gofin_hyperliquid::AccountMode::DexAbstraction => {
                return Err(GoFinError::validation(
                    "Papertrade inbound CCTP deposits do not support this Hyperliquid account mode.",
                ));
            }
        };
        let runtime = self.evm_runtimes.get(&999).ok_or_else(|| {
            unavailable("The configured HyperEVM read runtime is missing for proxy preflight.")
        })?;
        let factory_result = runtime
            .rpc
            .request(
                "eth_call",
                json!([{
                    "to": crate::papertrade_funding::PAPERTRADE_PROXY_FACTORY,
                    "data": crate::papertrade_funding::compute_proxy_address_call(&self.owner)?.data,
                }, "latest"]),
            )
            .await?;
        let factory_proxy = crate::papertrade_funding::decode_proxy_address_result(
            factory_result.as_str().ok_or_else(|| {
                GoFinError::validation("Papertrade factory proxy response is malformed.")
            })?,
        )?;
        let proxy_balance = if inbound_cctp {
            let proxy_state = self
                .hyperliquid_transport
                .info(
                    gofin_exchange::ccxt::HyperliquidInfoRequest::SpotClearinghouseState {
                        owner: factory_proxy.to_ascii_lowercase(),
                    },
                )
                .await?;
            let balances = proxy_state
                .get("balances")
                .and_then(Value::as_array)
                .filter(|rows| rows.len() <= 256)
                .ok_or_else(|| unavailable("Papertrade proxy Core spot state is malformed."))?;
            let balance = balances
                .iter()
                .find(|row| row["coin"].as_str() == Some("USDC"))
                .map(|row| {
                    row["total"]
                        .as_str()
                        .and_then(decimal_usdc_atoms)
                        .ok_or_else(|| unavailable("Papertrade proxy USDC balance is malformed."))
                })
                .transpose()?
                .unwrap_or(0);
            if balance != 0 {
                return Err(GoFinError::new(
                    "PAPERTRADE_PROXY_BALANCE_AMBIGUOUS",
                    "preview",
                    "The Papertrade proxy already has Core USDC awaiting sweep.",
                    "Resolve that existing proxy balance before starting another deposit.",
                ));
            }
            Some(balance)
        } else {
            None
        };
        let latest = runtime.rpc.request("eth_blockNumber", json!([])).await?;
        let core_sweep_from_block = latest
            .as_str()
            .and_then(|value| u64::from_str_radix(value.strip_prefix("0x")?, 16).ok())
            .ok_or_else(|| unavailable("HyperEVM returned an invalid latest block."))?;
        if hyperevm_withdrawal {
            // System-address sendAsset execution is not exposed by standard
            // eth_getLogs/eth_getTransactionReceipt on every HyperEVM RPC.
            // Refuse to begin a route unless this provider supports canonical
            // receipts that include system transactions; otherwise a real
            // source debit could never be independently settled.
            let receipts = runtime
                .rpc
                .request(
                    "eth_getBlockReceiptsWithSystemTx",
                    json!([format!("0x{core_sweep_from_block:x}")]),
                )
                .await
                .map_err(|_| {
                    unavailable(
                        "The configured HyperEVM RPC cannot provide canonical system-transaction receipts required for this withdrawal route.",
                    )
                })?;
            let system_transactions = runtime
                .rpc
                .request(
                    "eth_getSystemTxsByBlockNumber",
                    json!([format!("0x{core_sweep_from_block:x}")]),
                )
                .await
                .map_err(|_| {
                    unavailable(
                        "The configured HyperEVM RPC cannot expose canonical HyperCore system transactions required for this withdrawal route.",
                    )
                })?;
            if !has_hyperevm_system_receipt_capability(&receipts, &system_transactions) {
                return Err(unavailable(
                    "The configured HyperEVM RPC did not return bounded system receipt and transaction lists for the withdrawal route.",
                ));
            }
        }
        let solana_owner = if inbound_cctp && intent.route == FundingRoute::Solana {
            let credential = self.solana_credential.as_ref().ok_or_else(|| {
                unavailable("No explicitly enrolled Solana source wallet is configured.")
            })?;
            Some(
                self.credentials
                    .solana_signer(credential)?
                    .address()
                    .to_owned(),
            )
        } else {
            None
        };
        let source_domain = if hyperevm_withdrawal {
            19
        } else {
            match intent.route {
                FundingRoute::Ethereum => 0,
                FundingRoute::Arbitrum => 3,
                FundingRoute::Solana => 5,
                _ => unreachable!(),
            }
        };
        let (native_fee_asset, maximum_native_fee_atoms) = if hyperevm_withdrawal {
            (
                Some("hyperliquid:mainnet:HYPE".to_owned()),
                Some(crate::papertrade_funding::MAX_CORE_ONWARD_NATIVE_FEE_ATOMS.to_string()),
            )
        } else {
            Self::native_fee_policy(source_domain)?
        };
        Ok(FundingPreflight {
            owner: self.owner.clone(),
            factory_proxy,
            account_mode,
            hyperevm_usdc_allowance_atoms: None,
            native_fee_asset,
            maximum_native_fee_atoms,
            proxy_core_usdc_before_atoms: proxy_balance,
            core_owner_usdc_before_atoms: None,
            core_sweep_from_block: Some(core_sweep_from_block),
            solana_owner,
        })
    }

    async fn check_proxy_registration(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<ProxyRegistrationProof, GoFinError> {
        Self::unavailable()
    }

    async fn cctp_quote(&self, request: &CctpQuoteRequest) -> Result<FundingCctpQuote, GoFinError> {
        if request.amount_atoms == 0
            || !matches!(
                (request.source_domain, request.destination_domain),
                (0 | 3 | 5, 19) | (19, 0 | 3 | 5)
            )
        {
            return Err(GoFinError::validation(
                "Papertrade CCTP quote uses an unsupported domain pair or zero amount.",
            ));
        }
        let url = format!(
            "https://iris-api.circle.com/v2/burn/USDC/fees/{}/{}?forward=true&hyperCoreDeposit=true",
            request.source_domain, request.destination_domain
        );
        let response = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::ORIGIN, "https://exchange.papertrade.xyz")
            .send()
            .await
            .map_err(|_| unavailable("Circle's reviewed CCTP fee quote could not be fetched."))?;
        if !response.status().is_success() {
            return Err(unavailable(
                "Circle's reviewed CCTP fee quote returned an unsuccessful status.",
            ));
        }
        if response
            .content_length()
            .is_some_and(|size| size > 64 * 1024)
        {
            return Err(unavailable("Circle's CCTP fee quote was too large."));
        }
        let mut bytes = Vec::new();
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| unavailable("Circle's CCTP fee quote response could not be read."))?
        {
            if bytes.len().saturating_add(chunk.len()) > 64 * 1024 {
                return Err(unavailable("Circle's CCTP fee quote was too large."));
            }
            bytes.extend_from_slice(&chunk);
        }
        let rows: Vec<Value> = serde_json::from_slice(&bytes)
            .map_err(|_| unavailable("Circle's CCTP fee quote schema was invalid."))?;
        let row = rows
            .iter()
            .find(|row| row["finalityThreshold"].as_u64() == Some(2000))
            .ok_or_else(|| unavailable("Circle did not return a standard-finality fee quote."))?;
        let minimum_fee = decimal_hundredths(row.get("minimumFee"))?;
        let forward_fee = row
            .get("forwardFee")
            .and_then(|v| v.get("med").or_else(|| v.get("medium")))
            .and_then(value_u64)
            .ok_or_else(|| unavailable("Circle omitted the medium forwarding fee."))?;
        // Circle's minimumFee is expressed in hundredths of a basis point.
        // The official route computes ceil(amountAtoms * Ct / 1_000_000).
        let proportional = u128::from(request.amount_atoms)
            .checked_mul(u128::from(minimum_fee))
            .and_then(|n| n.checked_add(999_999))
            .map(|n| n / 1_000_000)
            .and_then(|n| u64::try_from(n).ok())
            .ok_or_else(|| unavailable("Circle's proportional fee exceeds the supported range."))?;
        let total_fee = proportional
            .checked_add(forward_fee)
            .ok_or_else(|| unavailable("Circle's CCTP fee exceeds the supported range."))?;

        // Native gas caps are supplied by route policy, never derived from a
        // USD conversion or accepted from the funding request. Until a chain
        // policy is configured, do not create a plan that lacks a finite cap.
        let (native_fee_asset, maximum_native_fee_atoms) =
            Self::native_fee_policy(request.source_domain)?;
        Ok(FundingCctpQuote {
            metadata: gofin_bridge::cctp::CctpQuoteMetadata {
                protocol_version: 2,
                source_domain: request.source_domain,
                destination_domain: request.destination_domain,
                transfer_mode: gofin_bridge::cctp::CctpTransferMode::Standard,
                max_fee: gofin_domain::Quantity::from_u64(total_fee),
                min_finality_threshold: 2000,
            },
            hypercore_gas_limit: 200_000,
            native_fee_asset,
            maximum_native_fee_atoms,
        })
    }

    async fn prepare_evm_call(
        &self,
        _: &str,
        intent: &FundingIntent,
        call: &gofin_bridge::EvmCall,
        maximum_total_fee: &str,
    ) -> Result<gofin_evm::FrozenTransaction, GoFinError> {
        if !intent.owner.eq_ignore_ascii_case(&self.owner) {
            return Err(GoFinError::validation(
                "EVM funding call owner differs from the enrolled Papertrade owner.",
            ));
        }
        let runtime = self
            .evm_runtimes
            .get(&call.chain_id)
            .ok_or_else(|| unavailable("The exact source-chain EVM runtime is not configured."))?;
        if runtime.chain_id != call.chain_id || !runtime.can_sign() {
            return Err(unavailable(
                "The exact source-chain runtime has no enrolled protected signer.",
            ));
        }
        let signer = self
            .credentials
            .papertrade_wallet_signer(&self.wallet_credential, &self.owner)?;
        if !format!("{:#x}", signer.address()).eq_ignore_ascii_case(&self.owner) {
            return Err(GoFinError::validation(
                "Source signer does not derive the enrolled owner.",
            ));
        }
        let address = gofin_evm::address(&self.owner)?;
        let target = gofin_evm::address(&call.to)?;
        let calldata = gofin_evm::alloy::primitives::Bytes::from(
            hex::decode(call.data.strip_prefix("0x").ok_or_else(|| {
                GoFinError::validation("Funding call calldata is not hex encoded.")
            })?)
            .map_err(|_| GoFinError::validation("Funding call calldata is malformed."))?,
        );
        let value = gofin_evm::alloy::primitives::U256::from_str_radix(&call.value.to_string(), 10)
            .map_err(|_| GoFinError::validation("Funding call value is malformed."))?;
        let fee = gofin_evm::alloy::primitives::U256::from_str_radix(maximum_total_fee, 10)
            .map_err(|_| GoFinError::validation("Funding native fee cap is malformed."))?;
        let prepared = runtime
            .prepare(address, target, value, calldata, fee)
            .await?;
        Ok(prepared.transaction)
    }

    async fn solana_burn_request(
        &self,
        operation_id: &str,
        intent: &FundingIntent,
        metadata: &gofin_bridge::cctp::CctpQuoteMetadata,
        proxy: &str,
    ) -> Result<gofin_bridge::cctp::solana::hypercore_source::BurnRequest, GoFinError> {
        if intent.route != FundingRoute::Solana
            || intent.direction != FundingDirection::Deposit
            || !intent.owner.eq_ignore_ascii_case(&self.owner)
        {
            return Self::unavailable();
        }
        let credential = self.solana_credential.as_ref().ok_or_else(|| {
            unavailable("No explicitly enrolled signing Solana connection is configured.")
        })?;
        let submission_id = format!("papertrade-funding:{operation_id}:0");
        let request = self
            .inbound_source
            .solana_burn_request(
                self.credentials.as_ref(),
                credential,
                &submission_id,
                intent.amount_usdc_atoms,
                metadata,
                proxy,
            )
            .await?;
        let value = serde_json::to_value(&request)
            .map_err(|_| GoFinError::validation("Solana CCTP request is malformed."))?;
        self.persist_immutable(
            &Self::leg_key(&submission_id, "solana_burn_request"),
            &value,
        )?;
        Ok(request)
    }

    async fn prepare_solana_burn_transaction(
        &self,
        submission_id: &str,
        instruction: &gofin_solana::NormalizedInstruction,
        maximum_network_fee_lamports: u64,
    ) -> Result<gofin_papertrade::SolanaFundingTransactionBinding, GoFinError> {
        let credential = self.solana_credential.as_ref().ok_or_else(|| {
            unavailable("No explicitly enrolled signing Solana connection is configured.")
        })?;
        self.inbound_source
            .prepare_solana_burn_transaction(
                self.credentials.as_ref(),
                credential,
                submission_id,
                instruction,
                maximum_network_fee_lamports,
            )
            .await
    }

    async fn reserve_hyperliquid_nonce(&self, _: &str) -> Result<u64, GoFinError> {
        Self::unavailable()
    }

    async fn papertrade_withdraw_to_core_intent(
        &self,
        operation_id: &str,
        intent: &FundingIntent,
    ) -> Result<gofin_papertrade::BoundIntent, GoFinError> {
        if !matches!(intent.route, FundingRoute::Core | FundingRoute::Hyperevm)
            || intent.direction != FundingDirection::Withdraw
            || !intent.owner.eq_ignore_ascii_case(&self.owner)
        {
            return Self::unavailable();
        }
        let runtime = self.runtime()?;
        let binding = runtime
            .prepare_funding_withdraw_to_core(operation_id, intent.amount_usdc_atoms)
            .await?;
        let key = Self::leg_key(
            &format!("papertrade-funding:{operation_id}:0"),
            "start_block",
        );
        if let Some(existing) = self.credentials.papertrade_runtime_record(&key)? {
            if existing.get("intent_id").and_then(Value::as_str) != Some(&binding.intent_id) {
                return Err(GoFinError::validation(
                    "Persisted withdrawal observation cursor belongs to another intent.",
                ));
            }
        } else {
            let from_block = runtime.funding_latest_block().await?;
            self.persist_immutable(
                &key,
                &json!({
                    "version":1,
                    "intent_id":binding.intent_id,
                    "from_block":from_block,
                    "start_time_ms":now_ms()?
                }),
            )?;
        }
        Ok(binding)
    }

    async fn persist_plan(&self, plan: &BoundFunding) -> Result<(), GoFinError> {
        let value = serde_json::to_value(plan)
            .map_err(|_| GoFinError::validation("Papertrade funding plan is malformed."))?;
        self.persist_immutable(&Self::plan_key(&plan.operation_id), &value)
    }

    async fn assert_persisted_plan(&self, plan: &BoundFunding) -> Result<(), GoFinError> {
        if self.load_bound_plan(&plan.operation_id)? != *plan {
            return Err(GoFinError::validation(
                "Papertrade funding plan differs from its durable immutable record.",
            ));
        }
        Ok(())
    }

    async fn dispatch_persisted_leg(
        &self,
        leg: &FundingLeg,
        outer_submission: &gofin_domain::Submission,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
        validated: &gofin_domain::RevalidatedOperation,
    ) -> Result<FundingLegState, GoFinError> {
        if let FundingAction::EvmCall { transaction, .. } = &leg.action {
            let operation_id = leg
                .submission_id
                .strip_prefix("papertrade-funding:")
                .and_then(|value| value.rsplit_once(':').map(|(operation, _)| operation))
                .ok_or_else(|| GoFinError::validation("Funding leg id is malformed."))?;
            let plan = self.load_bound_plan(operation_id)?;
            let authorized: gofin_papertrade::FundingBinding = serde_json::from_value(
                validated
                    .prepared()
                    .binding
                    .pointer("/funding_binding")
                    .cloned()
                    .ok_or_else(|| unavailable("Authorized funding binding is missing."))?,
            )
            .map_err(|_| unavailable("Authorized funding binding is malformed."))?;
            if !plan.legs.iter().any(|stored| stored == leg)
                || plan.intent.direction != FundingDirection::Deposit
                || !matches!(
                    plan.intent.route,
                    FundingRoute::Ethereum | FundingRoute::Arbitrum
                )
                || !plan.intent.owner.eq_ignore_ascii_case(&self.owner)
                || authorized.version != plan.version
                || authorized.operation_id != plan.operation_id
                || authorized.intent != plan.intent
                || authorized.source_identity != plan.source_identity
                || authorized.destination_identity != plan.destination_identity
                || authorized.bounds != plan.bounds
                || authorized.transaction.as_ref() != Some(transaction)
                || authorized.papertrade_intent.is_some()
                || authorized.solana_transaction.is_some()
                || transaction.chain_id
                    != if plan.intent.route == FundingRoute::Ethereum {
                        1
                    } else {
                        42161
                    }
                || outer_submission.kind != "papertrade_funding"
                || outer_submission.reference != plan.operation_id
            {
                return Err(GoFinError::validation(
                    "EVM CCTP burn differs from its immutable funding authorization.",
                ));
            }
            let attempted_key = Self::leg_key(&leg.submission_id, "dispatch_attempted");
            if self
                .credentials
                .papertrade_runtime_record(&attempted_key)?
                .is_some()
            {
                return Ok(FundingLegState::Unknown {
                    evidence: json!({"reason":"original EVM CCTP burn was already attempted; reconcile only"}),
                });
            }
            let Some(quote) = plan.cctp_quote.as_ref() else {
                return Err(GoFinError::validation(
                    "EVM CCTP funding plan omitted its immutable quote.",
                ));
            };
            let max_fee = plan
                .bounds
                .maximum_native_fee_atoms
                .as_deref()
                .ok_or_else(|| GoFinError::validation("EVM fee cap is missing."))?;
            let max_fee = max_fee
                .parse::<u128>()
                .map_err(|_| GoFinError::validation("EVM fee cap is malformed."))?;
            return match self
                .inbound_source
                .dispatch_evm_burn(
                    self.credentials.clone(),
                    &leg.submission_id,
                    transaction,
                    outer_submission,
                    journal,
                    validated,
                )
                .await
            {
                Ok(submission) => {
                    let evidence = json!({
                        "kind":"evm_cctp_burn_submitted",
                        "submission_id":leg.submission_id,
                        "source_chain_id":transaction.chain_id,
                        "transaction_hash":submission.reference,
                        "maximum_native_fee_atoms":max_fee.to_string(),
                        "quote":quote.metadata
                    });
                    self.persist_immutable(
                        &Self::leg_key(&leg.submission_id, "dispatch_response"),
                        &evidence,
                    )?;
                    Ok(FundingLegState::Submitted { evidence })
                }
                Err(_) => Ok(FundingLegState::Unknown {
                    evidence: json!({"reason":"EVM CCTP burn response was uncertain; reconcile the exact frozen transaction"}),
                }),
            };
        }
        if let FundingAction::SolanaCctpBurn {
            request,
            instruction,
            transaction,
        } = &leg.action
        {
            let operation_id = leg
                .submission_id
                .strip_prefix("papertrade-funding:")
                .and_then(|value| value.rsplit_once(':').map(|(operation, _)| operation))
                .ok_or_else(|| GoFinError::validation("Funding leg id is malformed."))?;
            let plan = self.load_bound_plan(operation_id)?;
            let authorized: gofin_papertrade::FundingBinding = serde_json::from_value(
                validated
                    .prepared()
                    .binding
                    .pointer("/funding_binding")
                    .cloned()
                    .ok_or_else(|| {
                        GoFinError::validation(
                            "Authorized funding operation omits its exact route binding.",
                        )
                    })?,
            )
            .map_err(|_| {
                GoFinError::validation("Authorized funding route binding is malformed.")
            })?;
            if !plan.legs.iter().any(|stored| stored == leg)
                || plan.intent.direction != FundingDirection::Deposit
                || plan.intent.route != FundingRoute::Solana
                || !plan.intent.owner.eq_ignore_ascii_case(&self.owner)
                || !request.owner.eq_ignore_ascii_case(
                    authorized
                        .source_identity
                        .strip_prefix("solana:mainnet-beta:")
                        .unwrap_or_default(),
                )
                || request.amount_atoms != plan.intent.amount_usdc_atoms
                || plan.cctp_quote.as_ref().is_none_or(|quote| {
                    gofin_bridge::cctp::solana::hypercore_source::validate_burn_instruction(
                        instruction,
                        request,
                        &quote.metadata,
                        plan.proxy.parse().unwrap_or_default(),
                    )
                    .is_err()
                })
                || authorized.version != plan.version
                || authorized.operation_id != plan.operation_id
                || authorized.intent != plan.intent
                || authorized.source_identity != plan.source_identity
                || authorized.destination_identity != plan.destination_identity
                || authorized.bounds != plan.bounds
                || authorized.solana_transaction.as_ref() != Some(transaction)
                || authorized.transaction.is_some()
                || authorized.papertrade_intent.is_some()
                || outer_submission.kind != "papertrade_funding"
                || outer_submission.reference != plan.operation_id
            {
                return Err(GoFinError::validation(
                    "Solana CCTP burn differs from its immutable route binding.",
                ));
            }
            let credential = self.solana_credential.as_ref().ok_or_else(|| {
                unavailable("No explicitly enrolled signing Solana connection is configured.")
            })?;
            let persisted_request = self
                .credentials
                .papertrade_runtime_record(&Self::leg_key(
                    &leg.submission_id,
                    "solana_burn_request",
                ))?
                .ok_or_else(|| unavailable("Solana CCTP request was not durably prepared."))?;
            if persisted_request
                != serde_json::to_value(request)
                    .map_err(|_| GoFinError::validation("Solana CCTP request is malformed."))?
            {
                return Err(GoFinError::validation(
                    "Solana CCTP request differs from its durable preparation record.",
                ));
            }
            let attempted_key = Self::leg_key(&leg.submission_id, "dispatch_attempted");
            if self
                .credentials
                .papertrade_runtime_record(&attempted_key)?
                .is_some()
            {
                return Ok(FundingLegState::Unknown {
                    evidence: json!({"reason":"original Solana CCTP burn was already attempted; reconcile only"}),
                });
            }
            let submission = self
                .inbound_source
                .dispatch_prepared_solana_burn(
                    self.credentials.clone(),
                    credential,
                    &leg.submission_id,
                    instruction,
                    outer_submission,
                    journal,
                    validated,
                )
                .await?;
            let evidence = json!({
                "kind":"solana_cctp_burn_submitted",
                "submission_id":leg.submission_id,
                "owner":request.owner,
                "amount_atoms":request.amount_atoms,
                "message_sha256":transaction.message_sha256,
                "signature":submission.reference
            });
            self.persist_immutable(
                &Self::leg_key(&leg.submission_id, "dispatch_response"),
                &evidence,
            )?;
            return Ok(FundingLegState::Submitted { evidence });
        }
        if let FundingAction::HyperCoreSendAsset { action } = &leg.action {
            let operation_id = leg
                .submission_id
                .strip_prefix("papertrade-funding:")
                .and_then(|value| value.rsplit_once(':').map(|(operation, _)| operation))
                .ok_or_else(|| GoFinError::validation("Funding leg id is malformed."))?;
            let plan = self.load_bound_plan(operation_id)?;
            if !plan.legs.iter().any(|stored| stored == leg)
                || action.action_type != "sendAsset"
                || action.hyperliquid_chain != "Mainnet"
                || !action
                    .signature_chain_id
                    .eq_ignore_ascii_case(gofin_hyperliquid::funding::HYPEREVM_SIGNATURE_CHAIN_ID)
                || action.token != gofin_hyperliquid::funding::USDC_TOKEN
                || !action
                    .destination
                    .eq_ignore_ascii_case(gofin_hyperliquid::funding::USDC_SYSTEM_ADDRESS)
                || action.source_dex != "spot"
                || action.destination_dex != "spot"
                || !action.from_sub_account.is_empty()
                || action.nonce == 0
            {
                return Err(GoFinError::validation(
                    "HyperEVM Core sendAsset differs from the reviewed same-owner route.",
                ));
            }
            let binding: gofin_papertrade::FundingContinuationBinding = serde_json::from_value(
                validated
                    .prepared()
                    .binding
                    .pointer("/funding_continuation")
                    .cloned()
                    .ok_or_else(|| {
                        GoFinError::validation(
                            "HyperEVM onward send requires its separate continuation authority.",
                        )
                    })?,
            )
            .map_err(|_| {
                GoFinError::validation("HyperEVM funding continuation binding is malformed.")
            })?;
            self.validate_hyperevm_continuation(&plan, leg, &binding, outer_submission)
                .await?;
            let principal = gofin_domain::Quantity::from_decimal(&action.amount, 6)?
                .integer()
                .to_string()
                .parse::<u64>()
                .map_err(|_| GoFinError::validation("HyperEVM withdrawal amount is too large."))?;
            if principal != binding.bounds.source_usdc_atoms
                || principal
                    != plan
                        .intent
                        .amount_usdc_atoms
                        .saturating_sub(CORE_CCTP_RETAINED_RESERVE_ATOMS)
            {
                return Err(GoFinError::validation(
                    "HyperEVM sendAsset amount differs from the approved continuation principal.",
                ));
            }
            let attempted_key = Self::leg_key(&leg.submission_id, "dispatch_attempted");
            if self
                .credentials
                .papertrade_runtime_record(&attempted_key)?
                .is_some()
            {
                return Ok(FundingLegState::Unknown {
                    evidence: json!({"reason":"the original HyperEVM sendAsset was already attempted; reconcile its exact nonce only"}),
                });
            }
            self.persist_immutable(
                &attempted_key,
                &json!({
                    "version":1,
                    "outer_submission":outer_submission,
                    "owner":self.owner,
                    "nonce":action.nonce,
                    "destination":action.destination,
                    "amount":action.amount,
                    "source_dex":action.source_dex,
                    "destination_dex":action.destination_dex,
                    "attempted_at_ms":now_ms()?
                }),
            )?;
            validated.assert_fresh()?;
            journal.assert_authorized()?;
            let envelope = gofin_exchange::ccxt::HyperliquidActionEnvelope {
                nonce_ms: action.nonce,
                vault_address: None,
                action: gofin_exchange::ccxt::HyperliquidAction::SendAsset {
                    signature_chain_id: action.signature_chain_id.clone(),
                    destination: action.destination.clone(),
                    source_dex: action.source_dex.clone(),
                    destination_dex: action.destination_dex.clone(),
                    token: action.token.clone(),
                    amount: action.amount.clone(),
                    from_sub_account: action.from_sub_account.clone(),
                    nonce: action.nonce,
                },
            };
            let response = self
                .hyperliquid_transport
                .dispatch(&self.hyperliquid_account, &envelope, journal)
                .await;
            let safe_outcome = match &response {
                Ok(value) => json!({
                    "version":1,
                    "kind":"venue_response",
                    "status":value.get("status").and_then(Value::as_str).filter(|value| value.len() <= 24).unwrap_or("other")
                }),
                Err(error) => json!({
                    "version":1,
                    "kind":"transport_error",
                    "code":if error.code.len() <= 64 {error.code.as_str()} else {"SANITIZED_TRANSPORT_ERROR"},
                    "stage":if ["submission","authentication","signing","read","validation"].contains(&error.stage.as_str()) {error.stage.as_str()} else {"other"}
                }),
            };
            self.persist_immutable(
                &Self::leg_key(&leg.submission_id, "dispatch_response"),
                &safe_outcome,
            )?;
            return match response {
                Ok(value) if value["status"] == "ok" => Ok(FundingLegState::Submitted {
                    evidence: json!({
                        "kind":"hyperevm_send_asset_submitted",
                        "owner":self.owner,
                        "nonce":action.nonce,
                        "amount_atoms":principal,
                        "destination":action.destination
                    }),
                }),
                Ok(_) | Err(_) => Ok(FundingLegState::Unknown {
                    evidence: json!({"reason":"HyperEVM sendAsset outcome is uncertain; reconcile its original nonce and destination"}),
                }),
            };
        }
        if let FundingAction::HyperCoreCctpWithdraw { action } = &leg.action {
            let operation_id = leg
                .submission_id
                .strip_prefix("papertrade-funding:")
                .and_then(|value| value.rsplit_once(':').map(|(operation, _)| operation))
                .ok_or_else(|| GoFinError::validation("Funding leg id is malformed."))?;
            let plan = self.load_bound_plan(operation_id)?;
            let continuation: Option<gofin_papertrade::FundingContinuationBinding> = validated
                .prepared()
                .binding
                .pointer("/funding_continuation")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|_| {
                    GoFinError::validation("Authorized funding continuation is malformed.")
                })?;
            let authorized: Option<gofin_papertrade::FundingBinding> = if continuation.is_none() {
                Some(
                    serde_json::from_value(
                        validated
                            .prepared()
                            .binding
                            .pointer("/funding_binding")
                            .cloned()
                            .ok_or_else(|| {
                                GoFinError::validation(
                                    "Authorized funding operation omits its exact route binding.",
                                )
                            })?,
                    )
                    .map_err(|_| {
                        GoFinError::validation("Authorized funding route binding is malformed.")
                    })?,
                )
            } else {
                None
            };
            let expected_domain = match plan.intent.route {
                FundingRoute::Ethereum => 0,
                FundingRoute::Arbitrum => 3,
                FundingRoute::Solana => 5,
                _ => {
                    return Err(GoFinError::validation(
                        "Core CCTP withdrawal has an unsupported destination route.",
                    ));
                }
            };
            let expected_withdrawal = plan.legs.iter().find_map(|stored| match &stored.action {
                FundingAction::PapertradeWithdrawToCore { bound } => Some(bound),
                _ => None,
            });
            let requested = plan.intent.amount_usdc_atoms;
            let expected_bridge_atoms = requested.checked_sub(100_000).ok_or_else(|| {
                GoFinError::validation("Core CCTP withdrawal amount is below its fixed reserve.")
            })?;
            let target = match plan.intent.route {
                FundingRoute::Ethereum => CctpWithdrawalTarget::Ethereum {
                    recipient: &plan.intent.owner,
                },
                FundingRoute::Arbitrum => CctpWithdrawalTarget::Arbitrum {
                    recipient: &plan.intent.owner,
                },
                FundingRoute::Solana => CctpWithdrawalTarget::Solana {
                    recipient_owner: plan
                        .destination_identity
                        .strip_prefix("solana:mainnet-beta:")
                        .ok_or_else(|| {
                            GoFinError::validation(
                                "Papertrade funding plan has a malformed Solana owner identity.",
                            )
                        })?,
                },
                _ => {
                    return Err(GoFinError::validation(
                        "Core CCTP withdrawal has an unsupported destination route.",
                    ));
                }
            };
            let expected_action = cctp_withdrawal_action(
                gofin_hyperliquid::funding::HYPEREVM_SIGNATURE_CHAIN_ID,
                &amount_decimal(expected_bridge_atoms),
                plan.cctp_quote
                    .as_ref()
                    .map(|quote| quote.hypercore_gas_limit)
                    .ok_or_else(|| {
                        GoFinError::validation("Funding plan omitted its CCTP quote.")
                    })?,
                action.nonce,
                target,
            )
            .map_err(|_| {
                GoFinError::validation("Frozen Core CCTP withdrawal action is invalid.")
            })?;
            let authorized_for_dispatch = if let Some(binding) = &continuation {
                binding.validate()?;
                if outer_submission.kind != "papertrade_funding_continuation"
                    || outer_submission.reference != binding.operation_id
                    || outer_submission.data
                        != json!({
                            "operation_id": binding.operation_id,
                            "route_id": binding.route_id,
                            "plan_fingerprint": binding.plan_fingerprint,
                            "stage1_proof_fingerprint": binding.stage1_proof_fingerprint
                        })
                    || binding.route_id != plan.operation_id
                    || binding.intent != plan.intent
                    || binding.destination_identity != plan.destination_identity
                    || plan.legs.len() != 2
                    || plan.legs[0].ordinal != 0
                    || plan.legs[1] != *leg
                {
                    false
                } else {
                    // Stage two is authorized only by the exact completed
                    // stage-one Core credit and original immutable plan. This
                    // read-only verifier performs no write and prevents a
                    // caller from reusing a continuation against another leg.
                    let stage1 = self.reconcile_persisted_leg(&plan, &plan.legs[0]).await?;
                    let proof = validate_stage1_core_credit(&plan, &stage1)?;
                    let expected = build_continuation_binding(
                        &plan,
                        &binding.operation_id,
                        &binding.parent_submission,
                        proof,
                    )?;
                    expected.version == binding.version
                        && expected.operation_id == binding.operation_id
                        && expected.route_id == binding.route_id
                        && expected.parent_submission == binding.parent_submission
                        && expected.plan_fingerprint == binding.plan_fingerprint
                        && expected.stage1_proof_fingerprint == binding.stage1_proof_fingerprint
                        && expected.intent == binding.intent
                        && expected.source_identity == binding.source_identity
                        && expected.destination_identity == binding.destination_identity
                        && expected.bounds == binding.bounds
                }
            } else {
                authorized.as_ref().is_some_and(|binding| {
                    binding.version == plan.version
                        && binding.operation_id == plan.operation_id
                        && binding.intent == plan.intent
                        && binding.source_identity == plan.source_identity
                        && binding.destination_identity == plan.destination_identity
                        && binding.bounds == plan.bounds
                        && binding.papertrade_intent.as_ref() == expected_withdrawal
                        && binding.transaction.is_none()
                        && binding.solana_transaction.is_none()
                        && outer_submission.kind == "papertrade_funding"
                        && outer_submission.reference == plan.operation_id
                })
            };
            if !plan.legs.iter().any(|stored| stored == leg)
                || !authorized_for_dispatch
                || plan.intent.direction != FundingDirection::Withdraw
                || !plan.intent.owner.eq_ignore_ascii_case(&self.owner)
                || plan.bounds.maximum_source_fee_usdc_atoms > CORE_CCTP_RETAINED_RESERVE_ATOMS
                || !matches!(
                    plan.intent.route,
                    FundingRoute::Ethereum | FundingRoute::Arbitrum | FundingRoute::Solana
                )
                || action.destination_chain_id != expected_domain
                || action != &expected_action
            {
                return Err(GoFinError::validation(
                    "Core CCTP withdrawal differs from its immutable route binding.",
                ));
            }
            let attempted_key = Self::leg_key(&leg.submission_id, "dispatch_attempted");
            if self
                .credentials
                .papertrade_runtime_record(&attempted_key)?
                .is_some()
            {
                return Ok(FundingLegState::Unknown {
                    evidence: json!({"reason":"original Core CCTP action was already attempted; reconcile only"}),
                });
            }
            let encoding = match action.address_encoding {
                gofin_hyperliquid::funding::AddressEncoding::Hex => "hex",
                gofin_hyperliquid::funding::AddressEncoding::Base58 => "base58",
            };
            let hl_action = gofin_exchange::ccxt::HyperliquidAction::SendToEvmWithData {
                signature_chain_id: action.signature_chain_id.clone(),
                token: action.token.clone(),
                amount: action.amount.clone(),
                source_dex: action.source_dex.clone(),
                destination_recipient: action.destination_recipient.clone(),
                address_encoding: encoding.to_owned(),
                destination_chain_id: action.destination_chain_id,
                gas_limit: action.gas_limit,
                data: action.data.clone(),
                nonce: action.nonce,
            };
            let envelope = gofin_exchange::ccxt::HyperliquidActionEnvelope {
                nonce_ms: action.nonce,
                vault_address: None,
                action: hl_action,
            };
            // A durable marker precedes the only write path. Any timeout or
            // lost response is reconciled against the exact nonce/action and
            // is never retried as a replacement action.
            self.persist_immutable(
                &attempted_key,
                &json!({
                    "version":1,
                    "outer_submission":outer_submission,
                    "attempted_at_ms":now_ms()?,
                    "nonce":action.nonce,
                    "destination_domain":action.destination_chain_id,
                    "recipient":action.destination_recipient,
                    "amount":action.amount,
                    "data":action.data
                }),
            )?;
            return match self
                .hyperliquid_transport
                .dispatch(&self.hyperliquid_account, &envelope, journal)
                .await
            {
                Ok(response) if response["status"] == "ok" => {
                    self.persist_immutable(
                        &Self::leg_key(&leg.submission_id, "dispatch_response"),
                        &json!({"version":1,"status":"ok"}),
                    )?;
                    Ok(FundingLegState::Submitted {
                        evidence: json!({
                            "kind":"hypercore_cctp_withdrawal_submitted",
                            "nonce":action.nonce,
                            "destination_domain":action.destination_chain_id,
                            "recipient":action.destination_recipient,
                            "amount":action.amount,
                            "gas_limit":action.gas_limit,
                            "data":action.data
                        }),
                    })
                }
                Ok(_) | Err(_) => Ok(FundingLegState::Unknown {
                    evidence: json!({"reason":"Core CCTP action outcome is unresolved; reconcile the exact original nonce/action"}),
                }),
            };
        }
        let FundingAction::PapertradeWithdrawToCore { bound } = &leg.action else {
            return Self::unavailable();
        };
        let operation_id = leg
            .submission_id
            .strip_prefix("papertrade-funding:")
            .and_then(|value| value.rsplit_once(':').map(|(operation, _)| operation))
            .ok_or_else(|| GoFinError::validation("Funding leg id is malformed."))?;
        let plan = self.load_bound_plan(operation_id)?;
        if !plan.legs.iter().any(|stored| stored == leg)
            || !matches!(
                plan.intent.route,
                FundingRoute::Core
                    | FundingRoute::Hyperevm
                    | FundingRoute::Ethereum
                    | FundingRoute::Arbitrum
                    | FundingRoute::Solana
            )
            || plan.intent.direction != FundingDirection::Withdraw
            || outer_submission.kind != "papertrade_funding"
            || outer_submission.reference != plan.operation_id
        {
            return Err(GoFinError::validation(
                "Core withdrawal leg differs from its authorized durable route.",
            ));
        }
        let authorized_intent: gofin_papertrade::BoundIntent = serde_json::from_value(
            validated
                .prepared()
                .binding
                .pointer("/funding_binding/papertrade_intent")
                .cloned()
                .ok_or_else(|| {
                    GoFinError::validation(
                        "Authorized Core withdrawal omits its exact typed Papertrade intent.",
                    )
                })?,
        )
        .map_err(|_| GoFinError::validation("Authorized Core withdrawal intent is malformed."))?;
        if authorized_intent != *bound {
            return Err(GoFinError::validation(
                "Core withdrawal intent differs from the authorized funding binding.",
            ));
        }
        let attempted_key = Self::leg_key(&leg.submission_id, "dispatch_attempted");
        if self
            .credentials
            .papertrade_runtime_record(&attempted_key)?
            .is_some()
        {
            return Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"original Core withdrawal dispatch was already attempted; reconcile only"}),
            });
        }
        self.persist_immutable(&attempted_key, &json!({"version":1,"attempted":true}))?;
        match self
            .runtime()?
            .dispatch_funding_intent(bound, outer_submission, validated, journal)
            .await
        {
            Ok(receipt) => {
                let evidence = json!({
                    "kind":"papertrade_withdrawal_accepted",
                    "intent_id":receipt.intent_id,
                    "action":receipt.action,
                    "status":receipt.status
                });
                self.persist_immutable(
                    &Self::leg_key(&leg.submission_id, "acceptance"),
                    &evidence,
                )?;
                Ok(FundingLegState::Submitted { evidence })
            }
            Err(error) => {
                let outcome = Self::sanitized_withdrawal_error(&error);
                self.persist_immutable(
                    &Self::leg_key(&leg.submission_id, "dispatch_response"),
                    &outcome,
                )?;
                Ok(FundingLegState::Unknown {
                    evidence: json!({
                        "reason":"withdrawal response was uncertain; locate the exact original intent",
                        "dispatch_outcome":outcome,
                    }),
                })
            }
        }
    }

    async fn reconcile_persisted_leg(
        &self,
        plan: &BoundFunding,
        leg: &FundingLeg,
    ) -> Result<FundingLegState, GoFinError> {
        if let FundingAction::EvmCall { transaction, .. } = &leg.action {
            if !plan.legs.iter().any(|stored| stored == leg)
                || plan.intent.direction != FundingDirection::Deposit
                || !matches!(
                    plan.intent.route,
                    FundingRoute::Ethereum | FundingRoute::Arbitrum
                )
                || transaction.chain_id
                    != if plan.intent.route == FundingRoute::Ethereum {
                        1
                    } else {
                        42161
                    }
            {
                return Err(GoFinError::validation(
                    "EVM CCTP source proof differs from its immutable funding plan.",
                ));
            }
            let proof_key = Self::leg_key(&leg.submission_id, "source_burn_proof");
            if let Some(saved) = self.credentials.papertrade_runtime_record(&proof_key)? {
                if saved.get("operation_id").and_then(Value::as_str) != Some(&plan.operation_id)
                    || saved
                        .get("transaction_hash")
                        .and_then(Value::as_str)
                        .is_none_or(|hash| !valid_hash32(hash))
                    || saved.get("amount_atoms").and_then(Value::as_u64)
                        != Some(plan.intent.amount_usdc_atoms)
                    || saved.get("source_chain_id").and_then(Value::as_u64)
                        != Some(transaction.chain_id)
                {
                    return Err(GoFinError::validation(
                        "Persisted EVM CCTP source proof conflicts with its immutable plan.",
                    ));
                }
                return Ok(FundingLegState::Settled { evidence: saved });
            }
            let Some(child) = self.credentials.papertrade_runtime_record(&format!(
                "papertrade-funding-leg:{}:child_submission",
                hex::encode(leg.submission_id.as_bytes())
            ))?
            else {
                let attempted = self
                    .credentials
                    .papertrade_runtime_record(&Self::leg_key(
                        &leg.submission_id,
                        "dispatch_attempted",
                    ))?
                    .is_some();
                return Ok(if attempted {
                    FundingLegState::Unknown {
                        evidence: json!({"reason":"EVM CCTP burn was attempted but its nested transaction identity is unavailable; reconcile only"}),
                    }
                } else {
                    FundingLegState::NotSubmitted
                });
            };
            let child: gofin_domain::Submission = serde_json::from_value(child)
                .map_err(|_| unavailable("Nested EVM funding identity is malformed."))?;
            let Some(hash) = child.reference.strip_prefix("0x").map(str::to_owned) else {
                return Err(GoFinError::validation(
                    "Nested EVM transaction hash is malformed.",
                ));
            };
            let hash = format!("0x{hash}");
            if !valid_hash32(&hash) {
                return Err(GoFinError::validation(
                    "Nested EVM transaction hash is malformed.",
                ));
            }
            let quote = plan.cctp_quote.as_ref().ok_or_else(|| {
                GoFinError::validation("EVM CCTP source plan omitted its persisted quote.")
            })?;
            let maximum_native_fee =
                plan.bounds
                    .maximum_native_fee_atoms
                    .as_deref()
                    .ok_or_else(|| {
                        GoFinError::validation("EVM CCTP source plan omitted its native fee cap.")
                    })?;
            let Some(proof) = self
                .inbound_source
                .observe_evm_burn(
                    transaction.chain_id,
                    &hash,
                    transaction,
                    &quote.metadata,
                    &self.owner,
                    plan.intent.amount_usdc_atoms,
                    &plan.proxy,
                    maximum_native_fee,
                )
                .await?
            else {
                return Ok(FundingLegState::Unknown {
                    evidence: json!({"reason":"EVM CCTP burn transaction is not yet canonical and finalized","transaction_hash":hash}),
                });
            };
            let evidence = json!({
                "kind":"settled_evm_cctp_source_burn",
                "operation_id":plan.operation_id,
                "transaction_hash":proof.transaction_hash,
                "block_hash":proof.block_hash,
                "block_number":proof.block_number,
                "confirmations":proof.confirmations,
                "source_chain_id":proof.source_chain_id,
                "source_owner":proof.source_owner,
                "amount_atoms":proof.amount_atoms,
                "native_fee_atoms":proof.native_fee_atoms,
                "source_message":proof.cctp_source
            });
            self.persist_immutable(&proof_key, &evidence)?;
            return Ok(FundingLegState::Settled { evidence });
        }
        if let FundingAction::SolanaCctpBurn {
            request,
            transaction,
            ..
        } = &leg.action
        {
            if !plan.legs.iter().any(|stored| stored == leg)
                || plan.intent.direction != FundingDirection::Deposit
                || plan.intent.route != FundingRoute::Solana
                || request.amount_atoms != plan.intent.amount_usdc_atoms
                || !request.owner.eq_ignore_ascii_case(&plan.intent.owner)
                || plan.source_identity != format!("solana:mainnet-beta:{}", request.owner)
            {
                return Err(GoFinError::validation(
                    "Solana CCTP source proof differs from its immutable funding plan.",
                ));
            }
            let proof_key = Self::leg_key(&leg.submission_id, "source_burn_proof");
            if let Some(saved) = self.credentials.papertrade_runtime_record(&proof_key)? {
                if saved.get("operation_id").and_then(Value::as_str) != Some(&plan.operation_id)
                    || saved.get("amount_atoms").and_then(Value::as_u64)
                        != Some(plan.intent.amount_usdc_atoms)
                    || saved
                        .get("signature")
                        .and_then(Value::as_str)
                        .is_none_or(|signature| bs58::decode(signature).into_vec().is_err())
                {
                    return Err(GoFinError::validation(
                        "Persisted Solana CCTP source proof conflicts with its immutable plan.",
                    ));
                }
                return Ok(FundingLegState::Settled { evidence: saved });
            }
            let response = self.credentials.papertrade_runtime_record(&Self::leg_key(
                &leg.submission_id,
                "dispatch_response",
            ))?;
            let signature = response
                .as_ref()
                .and_then(|value| value.get("signature"))
                .and_then(Value::as_str)
                .ok_or_else(|| unavailable("Solana CCTP dispatch signature is not persisted."))?;
            let quote = plan.cctp_quote.as_ref().ok_or_else(|| {
                GoFinError::validation("Solana CCTP source plan omitted its persisted quote.")
            })?;
            let fee_cap = transaction.maximum_network_fee_lamports;
            let proof = self
                .inbound_source
                .observe_solana_burn(signature, request, &quote.metadata, &plan.proxy, fee_cap)
                .await?;
            let evidence = json!({
                "kind":"settled_solana_cctp_source_burn",
                "operation_id":plan.operation_id,
                "signature":proof.signature,
                "slot":proof.slot,
                "source_owner":proof.source_owner,
                "amount_atoms":proof.amount_atoms,
                "network_fee_lamports":proof.network_fee_lamports,
                "message_sent_event_account":request.message_sent_event_data,
                "retained_message_account_rent_lamports":proof.retained_message_account_rent_lamports,
                "destination_domain":proof.destination_domain,
                "hook_data":proof.hook_data,
                "source_message":proof.source_message
            });
            self.persist_immutable(&proof_key, &evidence)?;
            return Ok(FundingLegState::Settled { evidence });
        }
        if let FundingAction::HyperCoreSendAsset { action } = &leg.action {
            if !plan.legs.iter().any(|stored| stored == leg)
                || plan.intent.direction != FundingDirection::Withdraw
                || plan.intent.route != FundingRoute::Hyperevm
                || leg.ordinal != 1
            {
                return Err(GoFinError::validation(
                    "HyperEVM continuation reconciliation differs from its immutable plan.",
                ));
            }
            let action_atoms = gofin_domain::Quantity::from_decimal(&action.amount, 6)?
                .as_str()
                .parse::<u64>()
                .map_err(|_| {
                    GoFinError::validation("HyperEVM continuation principal is malformed.")
                })?;
            if let Some(saved) = self.credentials.papertrade_runtime_record(&Self::leg_key(
                &leg.submission_id,
                "hyperevm_destination_proof",
            ))? {
                if saved["kind"].as_str() != Some("settled_hyperevm_withdrawal")
                    || saved["source"]["nonce"].as_u64() != Some(action.nonce)
                    || saved["destination"]["amount_atoms"].as_u64() != Some(action_atoms)
                    || saved["destination"]["transaction_hash"]
                        .as_str()
                        .is_none_or(|hash| !valid_hash32(hash))
                {
                    return Err(GoFinError::validation(
                        "Persisted HyperEVM continuation proof conflicts with its exact action.",
                    ));
                }
                return Ok(FundingLegState::Settled { evidence: saved });
            }
            let attempted = self.credentials.papertrade_runtime_record(&Self::leg_key(
                &leg.submission_id,
                "dispatch_attempted",
            ))?;
            if attempted.is_none() {
                return Ok(FundingLegState::NotSubmitted);
            }
            if attempted.as_ref().and_then(|value| value["nonce"].as_u64()) != Some(action.nonce)
                || attempted
                    .as_ref()
                    .and_then(|value| value["amount"].as_str())
                    != Some(action.amount.as_str())
                || attempted
                    .as_ref()
                    .and_then(|value| value["owner"].as_str())
                    .is_none_or(|owner| !owner.eq_ignore_ascii_case(&self.owner))
            {
                return Err(GoFinError::validation(
                    "HyperEVM continuation attempt differs from its frozen action.",
                ));
            }
            let Some(source) = self.hyperevm_core_source_proof(plan, leg, action).await? else {
                return Ok(FundingLegState::Unknown {
                    evidence: json!({"reason":"the exact Core action ledger proof is not yet available; reconcile only","nonce":action.nonce}),
                });
            };
            let action_time_ms = source["action_time_ms"].as_u64().ok_or_else(|| {
                GoFinError::validation("HyperEVM Core source proof omitted its exact action time.")
            })?;
            let Some(destination) = self
                .hyperevm_destination_proof(plan, leg, action_time_ms, action_atoms)
                .await?
            else {
                return Ok(FundingLegState::Unknown {
                    evidence: json!({"reason":"the exact finalized HyperEVM USDC system transfer is not uniquely proven yet; reconcile only","action_hash":source["action_hash"]}),
                });
            };
            let evidence = json!({
                "version":1,
                "kind":"settled_hyperevm_withdrawal",
                "owner":self.owner,
                "amount_atoms":action_atoms,
                "source":source,
                "destination":destination
            });
            self.persist_immutable(
                &Self::leg_key(&leg.submission_id, "hyperevm_destination_proof"),
                &evidence,
            )?;
            return Ok(FundingLegState::Settled { evidence });
        }
        if let FundingAction::HyperCoreCctpWithdraw { action } = &leg.action {
            if !plan.legs.iter().any(|stored| stored == leg)
                || plan.intent.direction != FundingDirection::Withdraw
                || !matches!(
                    plan.intent.route,
                    FundingRoute::Ethereum | FundingRoute::Arbitrum | FundingRoute::Solana
                )
            {
                return Err(GoFinError::validation(
                    "Core CCTP reconciliation differs from its immutable funding plan.",
                ));
            }
            return self.reconcile_core_cctp_withdrawal(plan, leg, action).await;
        }
        let FundingAction::PapertradeWithdrawToCore { bound } = &leg.action else {
            return Self::unavailable();
        };
        if !plan.legs.iter().any(|stored| stored == leg)
            || !matches!(
                plan.intent.route,
                FundingRoute::Core
                    | FundingRoute::Hyperevm
                    | FundingRoute::Ethereum
                    | FundingRoute::Arbitrum
                    | FundingRoute::Solana
            )
            || plan.intent.direction != FundingDirection::Withdraw
        {
            return Err(GoFinError::validation(
                "Core withdrawal reconciliation differs from its immutable funding plan.",
            ));
        }
        let Some(start) = self
            .credentials
            .papertrade_runtime_record(&Self::leg_key(&leg.submission_id, "start_block"))?
        else {
            return Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"withdrawal canonical search start block is unavailable"}),
            });
        };
        if start.get("intent_id").and_then(Value::as_str) != Some(&bound.intent_id) {
            return Err(GoFinError::validation(
                "Core withdrawal search cursor differs from its frozen intent.",
            ));
        }
        let from_block = start
            .get("from_block")
            .and_then(Value::as_u64)
            .ok_or_else(|| GoFinError::validation("Core withdrawal search cursor is malformed."))?;
        let tx = match self
            .runtime()?
            .canonical_intent_transaction(&bound.intent_id, from_block)
            .await
        {
            Ok(tx) => tx,
            Err(_) => {
                // A transient indexed-log/RPC failure must not suppress the
                // adapter's independent exact expiry-proof fallback. That
                // observer is tied to the same bound intent, owner, nonce,
                // deadline, and reviewed contract implementations. Returning
                // Unknown here lets reconciliation ask for that proof; it
                // never authorizes redispatch.
                return Ok(FundingLegState::Unknown {
                    evidence: json!({
                        "reason":"canonical exact-intent lookup is temporarily unavailable; reconcile only",
                        "intent_id":bound.intent_id,
                        "from_block":from_block
                    }),
                });
            }
        };
        let Some(tx) = tx else {
            let attempted = self
                .credentials
                .papertrade_runtime_record(&Self::leg_key(
                    &leg.submission_id,
                    "dispatch_attempted",
                ))?
                .is_some();
            return Ok(if attempted {
                FundingLegState::Unknown {
                    evidence: json!({"reason":"no canonical exact-intent receipt is indexed yet"}),
                }
            } else {
                FundingLegState::NotSubmitted
            });
        };
        let verified =
            gofin_papertrade::verify_canonical_call(&tx, &bound.typed, &bound.intent_id)?;
        let PapertradeTypedIntent::WithdrawToCore {
            amount,
            user,
            deadline,
            ..
        } = &bound.typed
        else {
            return Err(GoFinError::validation(
                "Funding intent is not WithdrawToCore.",
            ));
        };
        let requested = amount
            .parse::<u64>()
            .map_err(|_| GoFinError::validation("Core withdrawal amount is malformed."))?;
        let matching = verified.events.iter().find_map(|event| match event {
            gofin_papertrade::ProtocolSettlementEvent::CoreWithdrawal {
                owner,
                removed_wad,
                received_usdc_atoms,
            } if owner.eq_ignore_ascii_case(user) => Some((removed_wad, received_usdc_atoms)),
            _ => None,
        });
        let Some((removed_wad, received_core_8dp)) = matching else {
            return Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"canonical intent receipt lacks exact CoreWithdrawal event"}),
            });
        };
        let expected_wad = u128::from(requested)
            .checked_mul(1_000_000_000_000)
            .ok_or_else(|| GoFinError::validation("Core withdrawal amount overflowed."))?;
        let core_atoms = received_core_8dp
            .parse::<u64>()
            .map_err(|_| GoFinError::validation("CoreWithdrawal amount is malformed."))?;
        if removed_wad.parse::<u128>().ok() != Some(expected_wad)
            || core_atoms % 100 != 0
            || core_atoms / 100 != requested
        {
            return Err(GoFinError::validation(
                "Canonical CoreWithdrawal debit/credit does not match exact 6dp amount.",
            ));
        }
        if !core_withdrawal_action_in_batch(&tx, &verified, user, core_atoms) {
            return Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"canonical batch does not contain one exact matching CoreWriter SystemSpotSendAction"}),
            });
        }
        let Some(evm_block_time_ms) = self
            .runtime()?
            .canonical_block_timestamp_ms(verified.block_number, &verified.block_hash)
            .await?
        else {
            return Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"canonical Papertrade block timestamp is unavailable or its hash changed"}),
            });
        };
        let start_time_ms = start
            .get("start_time_ms")
            .and_then(Value::as_u64)
            .ok_or_else(|| GoFinError::validation("Core withdrawal ledger cursor is malformed."))?;
        let intent_time_limit = deadline.saturating_add(300).saturating_mul(1_000);
        let end_time_ms = now_ms()?.min(intent_time_limit);
        if end_time_ms < start_time_ms || end_time_ms / 1_000 < evm_block_time_ms / 1_000 {
            return Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"Core withdrawal owner-credit search window expired before a matching ledger record was observed"}),
            });
        }
        let credit_key = Self::leg_key(&leg.submission_id, "core_credit_proof");
        if let Some(saved) = self.credentials.papertrade_runtime_record(&credit_key)? {
            let mut mismatches = Vec::new();
            if saved.get("intent_id").and_then(Value::as_str) != Some(&verified.intent_id) {
                mismatches.push("intent_id");
            }
            if saved
                .get("owner")
                .and_then(Value::as_str)
                .is_none_or(|owner| !owner.eq_ignore_ascii_case(&plan.intent.owner))
            {
                mismatches.push("owner");
            }
            if saved
                .get("transaction_hash")
                .or_else(|| saved.get("papertrade_transaction_hash"))
                .and_then(Value::as_str)
                .is_none_or(|hash| !hash.eq_ignore_ascii_case(&verified.transaction_hash))
            {
                mismatches.push("transaction_hash");
            }
            if saved.get("received_usdc_atoms").and_then(Value::as_u64) != Some(requested) {
                mismatches.push("received_usdc_atoms");
            }
            if saved
                .get("core_credit_time_ms")
                .and_then(Value::as_u64)
                .is_none_or(|time| {
                    time < start_time_ms
                        || time / 1_000 < evm_block_time_ms / 1_000
                        || time > end_time_ms
                })
            {
                mismatches.push("core_credit_time_ms");
            }
            if saved
                .get("core_credit_transaction_hash")
                .and_then(Value::as_str)
                .is_none_or(|hash| !valid_hash32(hash))
            {
                mismatches.push("core_credit_transaction_hash");
            }
            if !mismatches.is_empty() {
                return Err(GoFinError::validation(format!(
                    "Persisted Core credit proof conflicts with the canonical withdrawal intent ({}).",
                    mismatches.join(", ")
                )));
            }
            return Ok(FundingLegState::Settled { evidence: saved });
        }
        let updates = self
            .hyperliquid_transport
            .info(
                gofin_exchange::ccxt::HyperliquidInfoRequest::UserNonFundingLedgerUpdates {
                    owner: self.hyperliquid_account.owner.to_ascii_lowercase(),
                    start_time_ms,
                    end_time_ms: Some(end_time_ms),
                },
            )
            .await?;
        let rows = updates
            .as_array()
            .or_else(|| updates.get("rows").and_then(Value::as_array))
            .ok_or_else(|| {
                GoFinError::validation("Core withdrawal ledger response is malformed.")
            })?;
        if rows.len() > 1_000 {
            return Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"Core withdrawal ledger response exceeded its bounded evidence window"}),
            });
        }
        let matching = rows
            .iter()
            .filter(|row| {
                let delta = &row["delta"];
                row["time"].as_u64().is_some_and(|time| {
                    time >= start_time_ms
                        && time / 1_000 >= evm_block_time_ms / 1_000
                        && time <= end_time_ms
                }) && delta["type"].as_str() == Some("spotTransfer")
                    && delta["token"].as_str() == Some("USDC")
                    && delta["user"].as_str().is_some_and(|sender| {
                        sender.eq_ignore_ascii_case(gofin_papertrade::EXCHANGE)
                    })
                    && delta["destination"]
                        .as_str()
                        .is_some_and(|destination| destination.eq_ignore_ascii_case(user))
                    && delta["amount"].as_str().is_some_and(|amount| {
                        gofin_domain::Quantity::from_decimal(amount, 6).is_ok_and(|quantity| {
                            quantity == gofin_domain::Quantity::from_u64(requested)
                        })
                    })
            })
            .collect::<Vec<_>>();
        if matching.len() != 1 {
            return Ok(FundingLegState::Unknown {
                evidence: json!({
                    "reason":if matching.is_empty() {"Papertrade Core withdrawal is queued but exact HyperCore owner credit is not observed"} else {"multiple exact HyperCore credits match the withdrawal and cannot be uniquely correlated"},
                    "papertrade_transaction_hash":verified.transaction_hash,
                    "intent_id":verified.intent_id
                }),
            });
        }
        let ledger_row = matching[0];
        let ledger_hash = ledger_row["hash"]
            .as_str()
            .ok_or_else(|| GoFinError::validation("Core withdrawal ledger omitted its hash."))?;
        let explorer = self
            .hyperliquid_transport
            .explorer(
                gofin_exchange::ccxt::HyperliquidExplorerRequest::TxDetails {
                    hash: ledger_hash.to_owned(),
                },
            )
            .await?;
        if !core_owner_credit_matches(
            ledger_row,
            &explorer,
            user,
            requested,
            start_time_ms,
            evm_block_time_ms,
            end_time_ms,
        ) || requested.checked_mul(100) != Some(core_atoms)
        {
            return Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"HyperCore ledger credit does not match its exact SystemSpotSendAction explorer proof"}),
            });
        }
        let evidence = json!({
            "kind":"canonical_papertrade_core_withdrawal",
            "transaction_hash":verified.transaction_hash,
            "block_hash":verified.block_hash,
            "block_number":verified.block_number,
            "intent_id":verified.intent_id,
            "owner":verified.owner,
            "removed_wad":removed_wad,
            "received_core_usdc_8dp":received_core_8dp,
            "received_usdc_atoms":requested,
            "core_credit_transaction_hash":ledger_hash,
            "core_credit_time_ms":ledger_row["time"]
        });
        self.persist_immutable(&credit_key, &evidence)?;
        Ok(FundingLegState::Settled { evidence })
    }

    async fn prove_destination(
        &self,
        plan: &BoundFunding,
        settled_legs: &[FundingLegState],
    ) -> Result<Option<FundingReceipt>, GoFinError> {
        if plan.intent.direction == FundingDirection::Deposit {
            if !matches!(
                plan.intent.route,
                FundingRoute::Ethereum | FundingRoute::Arbitrum | FundingRoute::Solana
            ) {
                return Ok(None);
            }
            if settled_legs.len() != 1 {
                return Ok(None);
            }
            let FundingLegState::Settled { evidence } = &settled_legs[0] else {
                return Ok(None);
            };
            let expected_kind = if plan.intent.route == FundingRoute::Solana {
                "settled_solana_cctp_source_burn"
            } else {
                "settled_evm_cctp_source_burn"
            };
            if evidence.get("kind").and_then(Value::as_str) != Some(expected_kind) {
                return Err(GoFinError::validation(
                    "Inbound CCTP destination proof received a different source leg.",
                ));
            }
            let proof_key = Self::leg_key(&plan.legs[0].submission_id, "settled_proof");
            if let Some(saved) = self.credentials.papertrade_runtime_record(&proof_key)? {
                let receipt: FundingReceipt = serde_json::from_value(saved).map_err(|_| {
                    GoFinError::validation("Persisted inbound CCTP receipt is malformed.")
                })?;
                if receipt.status != "settled"
                    || receipt.source_debit_usdc_atoms != plan.intent.amount_usdc_atoms
                    || receipt.source_identity != plan.source_identity
                    || receipt.destination_identity != plan.destination_identity
                    || receipt.destination_credit_usdc_atoms
                        < plan.bounds.destination_min_usdc_atoms
                {
                    return Err(GoFinError::validation(
                        "Persisted inbound CCTP receipt conflicts with its immutable route.",
                    ));
                }
                return Ok(Some(receipt));
            }
            return self.prove_inbound_hypercore_deposit(plan, evidence).await;
        }
        if plan.intent.direction != FundingDirection::Withdraw {
            return Ok(None);
        }
        if plan.intent.route == FundingRoute::Hyperevm {
            if settled_legs.len() != 2 {
                return Ok(None);
            }
            let (
                FundingLegState::Settled { evidence: stage1 },
                FundingLegState::Settled { evidence },
            ) = (&settled_legs[0], &settled_legs[1])
            else {
                return Ok(None);
            };
            if stage1["kind"].as_str() != Some("canonical_papertrade_core_withdrawal")
                || stage1["owner"]
                    .as_str()
                    .is_none_or(|owner| !owner.eq_ignore_ascii_case(&plan.intent.owner))
                || stage1["received_usdc_atoms"].as_u64() != Some(plan.intent.amount_usdc_atoms)
                || evidence["kind"].as_str() != Some("settled_hyperevm_withdrawal")
                || evidence["owner"]
                    .as_str()
                    .is_none_or(|owner| !owner.eq_ignore_ascii_case(&plan.intent.owner))
                || evidence["amount_atoms"].as_u64()
                    != plan
                        .intent
                        .amount_usdc_atoms
                        .checked_sub(CORE_CCTP_RETAINED_RESERVE_ATOMS)
                || evidence["source"]["amount_atoms"].as_u64() != evidence["amount_atoms"].as_u64()
                || evidence["source"]["source_fee_atoms"]
                    .as_u64()
                    .is_none_or(|fee| fee > CORE_CCTP_RETAINED_RESERVE_ATOMS)
                || evidence["source"]["native_fee_atoms"]
                    .as_u64()
                    .is_none_or(|fee| {
                        fee > plan
                            .bounds
                            .maximum_native_fee_atoms
                            .as_deref()
                            .and_then(|value| value.parse::<u64>().ok())
                            .unwrap_or(0)
                    })
                || evidence["source"]["source_debit_atoms"].as_u64()
                    != evidence["amount_atoms"].as_u64().and_then(|amount| {
                        evidence["source"]["source_fee_atoms"]
                            .as_u64()
                            .and_then(|fee| amount.checked_add(fee))
                    })
                || evidence["destination"]["amount_atoms"].as_u64()
                    != evidence["amount_atoms"].as_u64()
            {
                return Err(GoFinError::validation(
                    "HyperEVM continuation proof does not match the exact stage-one Core credit and stage-two transfer.",
                ));
            }
            let core_hash = stage1["core_credit_transaction_hash"]
                .as_str()
                .ok_or_else(|| GoFinError::validation("Stage-one Core credit hash is missing."))?;
            let source_hash = evidence["source"]["action_hash"]
                .as_str()
                .ok_or_else(|| GoFinError::validation("Core sendAsset hash is missing."))?;
            let destination_hash = evidence["destination"]["transaction_hash"]
                .as_str()
                .ok_or_else(|| GoFinError::validation("HyperEVM transfer hash is missing."))?;
            if !valid_hash32(core_hash)
                || !valid_hash32(source_hash)
                || !valid_hash32(destination_hash)
            {
                return Err(GoFinError::validation(
                    "HyperEVM continuation contains a malformed canonical transaction hash.",
                ));
            }
            let principal_atoms = plan
                .intent
                .amount_usdc_atoms
                .checked_sub(CORE_CCTP_RETAINED_RESERVE_ATOMS)
                .ok_or_else(|| {
                    GoFinError::validation("HyperEVM withdrawal is below its retained reserve.")
                })?;
            let source_fee_atoms = evidence["source"]["source_fee_atoms"]
                .as_u64()
                .ok_or_else(|| GoFinError::validation("Core send fee proof is malformed."))?;
            let retained = CORE_CCTP_RETAINED_RESERVE_ATOMS
                .checked_sub(source_fee_atoms)
                .ok_or_else(|| GoFinError::validation("Core send fee exceeds its reserve."))?;
            let native_fee_atoms = evidence["source"]["native_fee_atoms"]
                .as_u64()
                .ok_or_else(|| GoFinError::validation("Core native fee proof is malformed."))?;
            return Ok(Some(FundingReceipt {
                stage: gofin_papertrade::FundingReceiptStage::FinalDestination,
                status: "settled".into(),
                amount_usdc_atoms: principal_atoms,
                source_debit_usdc_atoms: plan.intent.amount_usdc_atoms,
                destination_credit_usdc_atoms: principal_atoms,
                actual_source_fee_usdc_atoms: source_fee_atoms,
                actual_cctp_fee_usdc_atoms: 0,
                retained_source_credit_usdc_atoms: retained,
                retained_source_native_atoms: None,
                retained_source_native_asset: None,
                activation_fee_usdc_atoms: 0,
                actual_native_fee_atoms: (native_fee_atoms > 0)
                    .then(|| native_fee_atoms.to_string()),
                actual_native_fee_asset: (native_fee_atoms > 0)
                    .then(|| plan.bounds.native_fee_asset.clone())
                    .flatten(),
                source_identity: plan.source_identity.clone(),
                destination_identity: plan.destination_identity.clone(),
                evidence_id: format!(
                    "papertrade-hyperevm-withdrawal:{core_hash}:{source_hash}:{destination_hash}"
                ),
            }));
        }
        if matches!(
            plan.intent.route,
            FundingRoute::Ethereum | FundingRoute::Arbitrum | FundingRoute::Solana
        ) {
            if settled_legs.len() != 2 {
                return Ok(None);
            }
            let (
                FundingLegState::Settled { evidence: source },
                FundingLegState::Settled { evidence },
            ) = (&settled_legs[0], &settled_legs[1])
            else {
                return Ok(None);
            };
            if source.get("kind").and_then(Value::as_str)
                != Some("canonical_papertrade_core_withdrawal")
                || source.get("received_usdc_atoms").and_then(Value::as_u64)
                    != Some(plan.intent.amount_usdc_atoms)
                || evidence.get("kind").and_then(Value::as_str)
                    != Some("settled_hypercore_cctp_withdrawal")
            {
                return Err(GoFinError::validation(
                    "Outbound CCTP proof does not match the exact Papertrade withdrawal and route.",
                ));
            }
            let destination_credit = evidence
                .get("destination_credit_usdc_atoms")
                .and_then(|value| {
                    value
                        .as_str()
                        .and_then(|raw| raw.parse::<u64>().ok())
                        .or_else(|| value.as_u64())
                })
                .ok_or_else(|| {
                    GoFinError::validation("CCTP destination credit amount is malformed.")
                })?;
            let source_fee = evidence
                .get("core_action_fee_atoms")
                .and_then(Value::as_u64)
                .ok_or_else(|| GoFinError::validation("Core CCTP action fee proof is missing."))?;
            let native_fee = evidence
                .get("core_native_fee_atoms")
                .and_then(Value::as_u64)
                .ok_or_else(|| GoFinError::validation("Core HYPE fee proof is missing."))?;
            let cctp_fee = evidence
                .get("cctp_fee_usdc_atoms")
                .and_then(|value| {
                    value
                        .as_str()
                        .and_then(|raw| raw.parse::<u64>().ok())
                        .or_else(|| value.as_u64())
                })
                .ok_or_else(|| {
                    GoFinError::validation("CCTP destination fee amount is malformed.")
                })?;
            if destination_credit < plan.minimum_received_usdc_atoms
                || source_fee > plan.bounds.maximum_source_fee_usdc_atoms
                || cctp_fee > plan.bounds.maximum_cctp_fee_usdc_atoms
                || native_fee
                    > plan
                        .bounds
                        .maximum_native_fee_atoms
                        .as_deref()
                        .and_then(|value| value.parse::<u64>().ok())
                        .unwrap_or(0)
                || (native_fee > 0
                    && (evidence
                        .get("core_native_fee_asset")
                        .and_then(Value::as_str)
                        != plan.bounds.native_fee_asset.as_deref()
                        || plan.bounds.native_fee_asset.as_deref()
                            != Some("hyperliquid:mainnet:HYPE")))
            {
                return Err(GoFinError::validation(
                    "Settled outbound CCTP fees or credit differ from the authorized bounds.",
                ));
            }
            let papertrade_hash = evidence
                .get("papertrade_transaction_hash")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    GoFinError::validation("Papertrade withdrawal receipt hash is missing.")
                })?;
            let core_action_hash = evidence
                .get("core_action_hash")
                .and_then(Value::as_str)
                .ok_or_else(|| GoFinError::validation("Core CCTP action hash is missing."))?;
            let destination_hash = evidence
                .get("forward_transaction_hash")
                .or_else(|| evidence.get("forward_signature"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    GoFinError::validation("CCTP destination receipt hash is missing.")
                })?;
            let source_debit = plan.intent.amount_usdc_atoms;
            let retained_source_credit = CORE_CCTP_RETAINED_RESERVE_ATOMS
                .checked_sub(source_fee)
                .ok_or_else(|| {
                GoFinError::validation("Core CCTP source fee exceeds its retained reserve.")
            })?;
            return Ok(Some(FundingReceipt {
                stage: gofin_papertrade::FundingReceiptStage::FinalDestination,
                status: "settled".into(),
                amount_usdc_atoms: destination_credit,
                source_debit_usdc_atoms: source_debit,
                destination_credit_usdc_atoms: destination_credit,
                actual_source_fee_usdc_atoms: source_fee,
                actual_cctp_fee_usdc_atoms: cctp_fee,
                retained_source_credit_usdc_atoms: retained_source_credit,
                retained_source_native_atoms: None,
                retained_source_native_asset: None,
                activation_fee_usdc_atoms: 0,
                actual_native_fee_atoms: (native_fee > 0).then(|| native_fee.to_string()),
                actual_native_fee_asset: (native_fee > 0)
                    .then(|| plan.bounds.native_fee_asset.clone())
                    .flatten(),
                source_identity: plan.source_identity.clone(),
                destination_identity: plan.destination_identity.clone(),
                evidence_id: format!(
                    "papertrade-cctp-withdrawal:{papertrade_hash}:{core_action_hash}:{destination_hash}"
                ),
            }));
        }
        if plan.intent.route != FundingRoute::Core || settled_legs.len() != 1 {
            return Ok(None);
        }
        let FundingLegState::Settled { evidence } = &settled_legs[0] else {
            return Ok(None);
        };
        if evidence.get("kind").and_then(Value::as_str)
            != Some("canonical_papertrade_core_withdrawal")
            || evidence.get("owner").and_then(Value::as_str)
                != Some(plan.intent.owner.to_ascii_lowercase().as_str())
            || evidence.get("received_usdc_atoms").and_then(Value::as_u64)
                != Some(plan.intent.amount_usdc_atoms)
        {
            return Err(GoFinError::validation(
                "Core withdrawal proof differs from its immutable owner and amount.",
            ));
        }
        let evidence_id = evidence
            .get("transaction_hash")
            .and_then(Value::as_str)
            .ok_or_else(|| GoFinError::validation("Canonical withdrawal hash is missing."))?;
        let core_credit_hash = evidence
            .get("core_credit_transaction_hash")
            .and_then(Value::as_str)
            .ok_or_else(|| GoFinError::validation("Canonical Core credit hash is missing."))?;
        let evidence_id = format!("papertrade-withdrawal:{evidence_id}:{core_credit_hash}");
        Ok(Some(FundingReceipt {
            stage: gofin_papertrade::FundingReceiptStage::FinalDestination,
            status: "settled".into(),
            amount_usdc_atoms: plan.intent.amount_usdc_atoms,
            source_debit_usdc_atoms: plan.intent.amount_usdc_atoms,
            destination_credit_usdc_atoms: plan.intent.amount_usdc_atoms,
            retained_source_credit_usdc_atoms: 0,
            retained_source_native_atoms: None,
            retained_source_native_asset: None,
            actual_source_fee_usdc_atoms: 0,
            actual_cctp_fee_usdc_atoms: 0,
            activation_fee_usdc_atoms: 0,
            actual_native_fee_atoms: None,
            actual_native_fee_asset: None,
            source_identity: plan.source_identity.clone(),
            destination_identity: plan.destination_identity.clone(),
            evidence_id,
        }))
    }
}

fn unavailable(message: &str) -> GoFinError {
    GoFinError::new(
        "PAPERTRADE_FUNDING_ROUTE_UNAVAILABLE",
        "transport",
        message,
        "Keep the original funding operation unresolved and reconcile it before retrying.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn withdrawal_transport_diagnostic_keeps_only_bounded_classification() {
        let error = GoFinError::new(
            "PAPERTRADE_HTTP_REJECTED",
            "submission",
            "redacted",
            "reconcile",
        )
        .with_details(json!({
            "http_status":422,
            "exchange_code":"INVALID_INTENT",
            "body":"signed payload must not persist",
            "signature":"secret",
        }));
        let diagnostic = PapertradeFundingRoutes::sanitized_withdrawal_error(&error);
        assert_eq!(diagnostic["http_status"], 422);
        assert_eq!(diagnostic["exchange_code"], "INVALID_INTENT");
        assert_eq!(diagnostic["code"], "PAPERTRADE_HTTP_REJECTED");
        assert!(diagnostic.get("body").is_none());
        assert!(diagnostic.get("signature").is_none());

        let unsafe_code = GoFinError::new(
            "PAPERTRADE_HTTP_REJECTED",
            "submission",
            "redacted",
            "reconcile",
        )
        .with_details(json!({"http_status":422,"exchange_code":"bad\nbody"}));
        assert_eq!(
            PapertradeFundingRoutes::sanitized_withdrawal_error(&unsafe_code)["exchange_code"],
            Value::Null
        );
    }

    #[test]
    fn hyperevm_destination_requires_system_wallet_call_for_exact_owner_and_amount() {
        let owner = "0x5502f38ed8ad519c3b9635ba723dddb94e991ecd";
        let mut input = hex::encode(&keccak256(b"transfer(address,uint256)")[..4]);
        input.push_str(&format!("{}{}", "0".repeat(24), &owner[2..]));
        input.push_str(&format!("{:064x}", 19_900_000u64));
        let tx = json!({
            "from":gofin_hyperliquid::funding::USDC_SYSTEM_ADDRESS,
            "to":gofin_bridge::cctp::hyperevm::CORE_DEPOSIT_WALLET,
            "value":"0x0",
            "input":format!("0x{input}")
        });
        assert!(hyperevm_core_wallet_transfer_matches(
            &tx, owner, 19_900_000
        ));

        assert!(!hyperevm_core_wallet_transfer_matches(
            &tx, owner, 19_900_001
        ));
        assert!(!hyperevm_core_wallet_transfer_matches(
            &json!({"from":owner,"to":gofin_bridge::cctp::hyperevm::CORE_DEPOSIT_WALLET,"value":"0x0","input":format!("0x{input}")}),
            owner,
            19_900_000
        ));
        assert!(!hyperevm_core_wallet_transfer_matches(
            &json!({"from":gofin_hyperliquid::funding::USDC_SYSTEM_ADDRESS,"to":gofin_bridge::cctp::hyperevm::NATIVE_USDC,"value":"0x0","input":format!("0x{input}")}),
            owner,
            19_900_000
        ));
    }

    #[test]
    fn hyperevm_destination_receipt_binds_wallet_withdraw_and_native_usdc_transfer() {
        let owner = "0x5502f38ed8ad519c3b9635ba723dddb94e991ecd";
        let wallet = gofin_bridge::cctp::hyperevm::CORE_DEPOSIT_WALLET;
        let amount = 19_900_000u64;
        let amount_word = format!("0x{:064x}", amount);
        let logs = vec![
            json!({
                "address":gofin_bridge::cctp::hyperevm::NATIVE_USDC,
                "topics":[ERC20_TRANSFER_TOPIC,topic_address(wallet),topic_address(owner)],
                "data":amount_word,"logIndex":"0x2"
            }),
            json!({
                "address":wallet,
                "topics":[format!("0x{}",hex::encode(keccak256(b"Withdraw(address,uint256)"))),topic_address(owner)],
                "data":amount_word,"logIndex":"0x3"
            }),
        ];
        assert!(hyperevm_core_wallet_receipt_logs(&logs, owner, amount).is_some());
        assert!(hyperevm_core_wallet_receipt_logs(&logs, owner, amount + 1).is_none());

        let mut wrong_recipient = logs.clone();
        wrong_recipient[0]["topics"][2] = json!(topic_address(wallet));
        assert!(hyperevm_core_wallet_receipt_logs(&wrong_recipient, owner, amount).is_none());

        let mut wrong_wallet_event = logs.clone();
        wrong_wallet_event[1]["address"] = json!(gofin_bridge::cctp::hyperevm::NATIVE_USDC);
        assert!(hyperevm_core_wallet_receipt_logs(&wrong_wallet_event, owner, amount).is_none());

        let mut wrong_order = logs;
        wrong_order[0]["logIndex"] = json!("0x4");
        wrong_order[1]["logIndex"] = json!("0x2");
        assert!(hyperevm_core_wallet_receipt_logs(&wrong_order, owner, amount).is_none());
    }
    use async_trait::async_trait;
    use gofin_domain::Quantity;
    use gofin_evm::{CanonicalReceipt, EvmRpc, alloy::primitives::U256};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const TEST_OWNER: &str = "0x5502f38ed8ad519c3b9635ba723dddb94e991ecd";
    const TEST_PROXY: &str = "0x18b9c4b188f556242edbfd5496d0840130f3ebb0";
    const TEST_SOURCE_HASH: &str =
        "0x1111111111111111111111111111111111111111111111111111111111111111";
    const TEST_FORWARD_HASH: &str =
        "0x2222222222222222222222222222222222222222222222222222222222222222";
    const TEST_SWEEP_HASH: &str =
        "0x3333333333333333333333333333333333333333333333333333333333333333";

    #[test]
    fn hyperevm_system_receipt_capability_requires_both_bounded_rpc_methods() {
        assert!(has_hyperevm_system_receipt_capability(
            &json!([]),
            &json!([])
        ));
        assert!(has_hyperevm_system_receipt_capability(
            &json!({"receipts": []}),
            &json!({"transactions": []})
        ));
        assert!(!has_hyperevm_system_receipt_capability(
            &json!({"error":{"code":-32601,"message":"method not found"}}),
            &json!([])
        ));
        assert!(!has_hyperevm_system_receipt_capability(
            &json!([]),
            &json!({"result": []})
        ));
        assert!(!has_hyperevm_system_receipt_capability(
            &json!({"receipts": "unbounded"}),
            &json!({"transactions": []})
        ));
    }

    struct FundingRouteTestCredentials(Mutex<BTreeMap<String, Value>>);

    impl gofin_exchange::ccxt::CredentialProvider for FundingRouteTestCredentials {
        fn credentials(
            &self,
            _: &str,
        ) -> Result<gofin_exchange::ccxt::Credentials, gofin_exchange::ccxt::CcxtError> {
            Err(gofin_exchange::ccxt::CcxtError {
                kind: gofin_exchange::ccxt::CcxtErrorKind::Unsupported,
                http_status: None,
                exchange_code: None,
            })
        }
    }

    impl crate::BrokerCredentialProvider for FundingRouteTestCredentials {
        fn envelope_signer(
            &self,
            _: &gofin_credentials::CredentialRef,
        ) -> Result<(String, Arc<dyn gofin_evm::EnvelopeSigner>), GoFinError> {
            Err(GoFinError::validation(
                "No signer expected in proof-only fixture.",
            ))
        }

        fn papertrade_runtime_record(&self, key: &str) -> Result<Option<Value>, GoFinError> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }

        fn put_papertrade_runtime_record(
            &self,
            key: &str,
            value: &Value,
        ) -> Result<(), GoFinError> {
            let mut records = self.0.lock().unwrap();
            if records.get(key).is_some_and(|existing| existing != value) {
                return Err(GoFinError::validation("Conflicting test durable record."));
            }
            records.insert(key.to_owned(), value.clone());
            Ok(())
        }
    }

    struct InboundProofRpc {
        forward_block_time_secs: u64,
    }

    fn test_receipt(hash: &str, block: u64, transaction_index: u64, swept: bool) -> Value {
        let block_hash = if block == 100 {
            format!("0x{}", "aa".repeat(32))
        } else {
            format!("0x{}", "bb".repeat(32))
        };
        let logs = if swept {
            let owner_topic = format!("0x{}{}", "00".repeat(12), &TEST_OWNER[2..]);
            vec![json!({
                "address":gofin_papertrade::EXCHANGE,
                "topics":["0xc36b5179cb9c303b200074996eab2b3473eac370fdd7eba3bec636fe35109696",owner_topic],
                "data":format!("0x{:064x}",18_800_000u128 * 1_000_000_000_000),
                "logIndex":"0x0",
                "blockNumber":format!("0x{block:x}"),
                "transactionIndex":format!("0x{transaction_index:x}"),
                "transactionHash":hash
            })]
        } else {
            vec![]
        };
        json!({
            "transactionHash":hash,
            "blockHash":block_hash,
            "blockNumber":format!("0x{block:x}"),
            "transactionIndex":format!("0x{transaction_index:x}"),
            "status":"0x1",
            "gasUsed":"0x5208",
            "effectiveGasPrice":"0x1",
            "logs":logs
        })
    }

    #[async_trait]
    impl EvmRpc for InboundProofRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value, GoFinError> {
            match method {
                "eth_chainId" => Ok(json!("0x3e7")),
                "eth_blockNumber" => Ok(json!("0x100")),
                "eth_getTransactionReceipt" => {
                    let hash = params[0].as_str().unwrap();
                    Ok(match hash {
                        TEST_FORWARD_HASH => test_receipt(hash, 100, 1, false),
                        TEST_SWEEP_HASH => test_receipt(hash, 112, 2, true),
                        _ => Value::Null,
                    })
                }
                "eth_getTransactionByHash" => {
                    let hash = params[0].as_str().unwrap();
                    Ok(match hash {
                        TEST_FORWARD_HASH => json!({
                            "hash":hash,"from":TEST_OWNER,"to":"0x0000000000000000000000000000000000000001",
                            "blockHash":format!("0x{}","aa".repeat(32)),"blockNumber":"0x64"
                        }),
                        TEST_SWEEP_HASH => json!({
                            "hash":hash,"from":TEST_OWNER,"to":gofin_papertrade::EXCHANGE,
                            "input":format!("0x6d142da1{}{}","0".repeat(24),&TEST_OWNER[2..]),
                            "blockHash":format!("0x{}","bb".repeat(32)),"blockNumber":"0x70"
                        }),
                        _ => Value::Null,
                    })
                }
                "eth_getBlockByNumber" => {
                    let tag = params[0].as_str().unwrap_or_default();
                    if tag == "finalized" || tag == "latest" {
                        Ok(json!({"number":"0x100","hash":format!("0x{}","cc".repeat(32))}))
                    } else {
                        let number =
                            u64::from_str_radix(tag.trim_start_matches("0x"), 16).unwrap_or(100);
                        let hash = if number == 100 {
                            format!("0x{}", "aa".repeat(32))
                        } else {
                            format!("0x{}", "bb".repeat(32))
                        };
                        Ok(
                            json!({"number":tag,"hash":hash,"timestamp":format!("0x{:x}",self.forward_block_time_secs)}),
                        )
                    }
                }
                "eth_getLogs" => Ok(json!([{
                    "address":gofin_papertrade::EXCHANGE,
                    "topics":["0xc36b5179cb9c303b200074996eab2b3473eac370fdd7eba3bec636fe35109696",format!("0x{}{}","00".repeat(12),&TEST_OWNER[2..])],
                    "data":format!("0x{:064x}",18_800_000u128 * 1_000_000_000_000),
                    "logIndex":"0x0","blockNumber":"0x70","transactionIndex":"0x2","transactionHash":TEST_SWEEP_HASH
                }])),
                _ => Err(GoFinError::validation(format!(
                    "Unexpected proof RPC method: {method} {params}"
                ))),
            }
        }
    }

    struct InboundLedgerTransport {
        incoming_time: u64,
        outgoing_time: u64,
    }

    #[async_trait]
    impl gofin_hyperliquid::HyperliquidTransport for InboundLedgerTransport {
        async fn info(
            &self,
            request: gofin_exchange::ccxt::HyperliquidInfoRequest,
        ) -> Result<Value, GoFinError> {
            let owner = match request {
                gofin_exchange::ccxt::HyperliquidInfoRequest::UserNonFundingLedgerUpdates {
                    owner,
                    ..
                } => owner,
                _ => return Err(GoFinError::validation("Unexpected route info request.")),
            };
            if owner != TEST_PROXY {
                return Ok(json!([]));
            }
            Ok(json!([
                {"time":self.incoming_time,"hash":TEST_FORWARD_HASH,"delta":{"type":"spotTransfer","user":"0x2000000000000000000000000000000000000000","destination":TEST_PROXY,"token":"USDC","amount":"19.8","fee":"1","feeToken":"USDC","nativeTokenFee":"0"}},
                {"time":self.incoming_time,"hash":TEST_FORWARD_HASH,"delta":{"type":"accountActivationGas","amount":"1","token":"USDC"}},
                {"time":self.outgoing_time,"hash":TEST_SWEEP_HASH,"delta":{"type":"spotTransfer","user":TEST_PROXY,"destination":gofin_papertrade::EXCHANGE,"token":"USDC","amount":"18.8","fee":"0","nativeTokenFee":"0"}}
            ]))
        }

        async fn explorer(
            &self,
            request: gofin_exchange::ccxt::HyperliquidExplorerRequest,
        ) -> Result<Value, GoFinError> {
            let gofin_exchange::ccxt::HyperliquidExplorerRequest::TxDetails { hash } = request
            else {
                return Err(GoFinError::validation("Unexpected route explorer request."));
            };
            let (time, user, destination, amount) = if hash == TEST_FORWARD_HASH {
                (
                    self.incoming_time,
                    "0x2000000000000000000000000000000000000000",
                    TEST_PROXY,
                    19_800_000u64,
                )
            } else {
                (
                    self.outgoing_time,
                    TEST_PROXY,
                    gofin_papertrade::EXCHANGE,
                    18_800_000u64,
                )
            };
            Ok(
                json!({"tx":{"hash":hash,"time":time,"error":null,"block":100,"user":user,"action":{"type":"SystemSpotSendAction","destination":destination,"token":0,"wei":amount*100}}}),
            )
        }

        async fn dispatch(
            &self,
            _: &gofin_hyperliquid::HyperliquidAccount,
            _: &gofin_exchange::ccxt::HyperliquidActionEnvelope,
            _: Arc<dyn gofin_domain::SubmissionJournal>,
        ) -> Result<Value, GoFinError> {
            Err(GoFinError::validation("Read-only route proof fixture."))
        }
    }

    struct InboundSettlement {
        attestations: AtomicUsize,
        verifications: AtomicUsize,
    }

    #[async_trait]
    impl crate::papertrade_cctp_settlement::PapertradeCctpSettlement for InboundSettlement {
        async fn fetch_attestation(
            &self,
            source_domain: u32,
            source_tx_hash: &str,
        ) -> Result<gofin_bridge::cctp::CctpAttestationStatus, GoFinError> {
            self.attestations.fetch_add(1, Ordering::SeqCst);
            assert_eq!(source_domain, 3);
            assert_eq!(source_tx_hash, TEST_SOURCE_HASH);
            Ok(gofin_bridge::cctp::CctpAttestationStatus::Ready {
                messages: vec![gofin_bridge::cctp::CctpAttestation {
                    source_transaction_hash: TEST_SOURCE_HASH.into(),
                    message: "attested-message".into(),
                    attestation: "circle-signature".into(),
                    nonce: "cctp-nonce".into(),
                    forward_tx_hash: Some(TEST_FORWARD_HASH.into()),
                }],
            })
        }
        async fn verify_hypercore_outbound_solana(
            &self,
            _: &gofin_bridge::cctp::CctpQuoteMetadata,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: &gofin_bridge::SolanaDestination,
            _: &str,
            _: u64,
            _: u64,
            _: &gofin_solana::FinalizedReceiptProof,
        ) -> Result<crate::papertrade_cctp_settlement::SolanaCctpDestinationProof, GoFinError>
        {
            Err(GoFinError::validation("Not used."))
        }
        async fn verify_hypercore_outbound_evm(
            &self,
            _: &gofin_bridge::cctp::CctpQuoteMetadata,
            _: &str,
            _: &str,
            _: u64,
            _: &str,
            _: u64,
            _: u64,
            _: &CanonicalReceipt,
        ) -> Result<crate::papertrade_cctp_settlement::CctpDestinationProof, GoFinError> {
            Err(GoFinError::validation("Not used."))
        }
        async fn verify_hypercore_forward(
            &self,
            source_message: &str,
            quote: &gofin_bridge::cctp::CctpQuoteMetadata,
            attested_message: &str,
            attestation: &str,
            source_chain_id: u64,
            source_owner: &str,
            amount_atoms: u64,
            proxy: &str,
            receipt: &CanonicalReceipt,
        ) -> Result<crate::papertrade_cctp_settlement::CctpDestinationProof, GoFinError> {
            self.verifications.fetch_add(1, Ordering::SeqCst);
            assert_eq!(source_message, "source-message");
            assert_eq!(quote.source_domain, 3);
            assert_eq!(attested_message, "attested-message");
            assert_eq!(attestation, "circle-signature");
            assert_eq!(source_chain_id, 42161);
            assert_eq!(source_owner, TEST_OWNER);
            assert_eq!(amount_atoms, 20_000_000);
            assert_eq!(proxy, TEST_PROXY);
            assert_eq!(format!("{:#x}", receipt.hash), TEST_FORWARD_HASH);
            Ok(crate::papertrade_cctp_settlement::CctpDestinationProof {
                transaction_hash: TEST_FORWARD_HASH.into(),
                block_hash: format!("0x{}", "aa".repeat(32)),
                block_number: 100,
                confirmations: 2,
                destination_domain: 19,
                recipient: TEST_PROXY.into(),
                gross_atoms: "20000000".into(),
                fee_atoms: "200000".into(),
                net_atoms: "19800000".into(),
                nonce: "cctp-nonce".into(),
                message_received_log_index: 3,
            })
        }
        async fn verify_solana_hypercore_forward(
            &self,
            _: &str,
            _: &gofin_bridge::cctp::solana::hypercore_source::BurnRequest,
            _: &gofin_bridge::cctp::CctpQuoteMetadata,
            _: &str,
            _: &str,
            _: &str,
            _: &CanonicalReceipt,
        ) -> Result<crate::papertrade_cctp_settlement::CctpDestinationProof, GoFinError> {
            Err(GoFinError::validation("Not used."))
        }
    }

    #[tokio::test]
    async fn inbound_arbitrum_route_connects_circle_forward_core_credit_and_sweep_once() {
        let now_ms = now_ms().unwrap();
        let credentials = Arc::new(FundingRouteTestCredentials(Mutex::new(BTreeMap::new())));
        let rpc = Arc::new(InboundProofRpc {
            forward_block_time_secs: now_ms / 1_000 - 2,
        });
        let runtime = Arc::new(gofin_evm::EvmRuntime::new(rpc, 999, None));
        let settlement = Arc::new(InboundSettlement {
            attestations: AtomicUsize::new(0),
            verifications: AtomicUsize::new(0),
        });
        let inbound_source = Arc::new(
            crate::papertrade_funding_inbound::PapertradeInboundCctpSource::new(
                BTreeMap::new(),
                BTreeMap::new(),
                None,
                None,
                BTreeMap::new(),
            )
            .unwrap(),
        );
        let papertrade_http = Arc::new(crate::papertrade_runtime::PapertradeHttp::new().unwrap());
        let routes = PapertradeFundingRoutes::new(
            credentials.clone(),
            gofin_credentials::CredentialRef::new("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap(),
            TEST_OWNER,
            BTreeMap::from([(999, runtime)]),
            Arc::new(InboundLedgerTransport {
                incoming_time: now_ms - 1_000,
                outgoing_time: now_ms - 900,
            }),
            gofin_hyperliquid::HyperliquidAccount {
                name: "test-papertrade-owner".into(),
                owner: TEST_OWNER.into(),
                credential_ref: Some("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".into()),
                signing_enabled: true,
                execution_scope: gofin_hyperliquid::HyperliquidExecutionScope::Owner,
            },
            settlement.clone(),
            None,
            inbound_source,
            None,
            papertrade_http,
            BTreeMap::from([(999, 2)]),
        )
        .unwrap();

        let amount_atoms = 20_000_000;
        let quote = FundingCctpQuote {
            metadata: gofin_bridge::cctp::CctpQuoteMetadata {
                protocol_version: 2,
                source_domain: 3,
                destination_domain: 19,
                transfer_mode: gofin_bridge::cctp::CctpTransferMode::Fast,
                max_fee: Quantity::from_u64(300_000),
                min_finality_threshold: 1_000,
            },
            hypercore_gas_limit: 200_000,
            native_fee_asset: Some("ethereum:42161:native".into()),
            maximum_native_fee_atoms: Some("100000000000000".into()),
        };
        let transaction = gofin_evm::FrozenTransaction {
            chain_id: 42161,
            sender: TEST_OWNER.into(),
            to: gofin_bridge::cctp::TOKEN_MESSENGER.into(),
            value: Quantity::from_u64(0),
            calldata: "0x12345678".into(),
            nonce: 1,
            gas: 200_000,
            max_fee_per_gas: Quantity::from_u64(2),
            max_priority_fee_per_gas: Quantity::from_u64(1),
            rollup_fee: None,
        };
        let plan = BoundFunding {
            version: 1,
            operation_id: "inbound-composite-test".into(),
            intent: FundingIntent {
                direction: FundingDirection::Deposit,
                route: FundingRoute::Arbitrum,
                owner: TEST_OWNER.into(),
                amount_usdc_atoms: amount_atoms,
                request_id: "source-request".into(),
            },
            proxy: TEST_PROXY.into(),
            source_identity: format!("ethereum:42161:{}", TEST_OWNER),
            destination_identity: format!("papertrade:mainnet:{}", TEST_OWNER),
            minimum_received_usdc_atoms: 10_000_000,
            bounds: gofin_papertrade::FundingFinancialBounds {
                source_usdc_atoms: amount_atoms,
                maximum_source_fee_usdc_atoms: 0,
                destination_min_usdc_atoms: 10_000_000,
                maximum_cctp_fee_usdc_atoms: 300_000,
                maximum_activation_fee_atoms: 1_000_000,
                native_fee_asset: Some("ethereum:42161:native".into()),
                maximum_native_fee_atoms: Some("100000000000000".into()),
                maximum_refundable_native_funding_atoms: None,
                maximum_native_source_debit_atoms: None,
            },
            cctp_quote: Some(quote),
            proxy_core_usdc_before_atoms: Some(0),
            core_owner_usdc_before_atoms: None,
            core_sweep_from_block: Some(100),
            proxy_registration: None,
            legs: vec![FundingLeg {
                ordinal: 0,
                submission_id: "inbound-composite-leg".into(),
                action: FundingAction::EvmCall {
                    call: gofin_bridge::EvmCall {
                        chain_id: 42161,
                        to: gofin_bridge::cctp::TOKEN_MESSENGER.into(),
                        value: Quantity::from_u64(0),
                        data: transaction.calldata.clone(),
                    },
                    transaction,
                },
            }],
        };
        let source = json!({
            "kind":"settled_evm_cctp_source_burn",
            "operation_id":plan.operation_id,
            "transaction_hash":TEST_SOURCE_HASH,
            "amount_atoms":amount_atoms,
            "native_fee_atoms":"10",
            "source_message":{"raw_message":"source-message"}
        });
        let settled = vec![FundingLegState::Settled { evidence: source }];

        let receipt = routes
            .prove_destination(&plan, &settled)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.status, "settled");
        assert_eq!(receipt.source_debit_usdc_atoms, amount_atoms);
        assert_eq!(receipt.actual_cctp_fee_usdc_atoms, 200_000);
        assert_eq!(receipt.activation_fee_usdc_atoms, 1_000_000);
        assert_eq!(receipt.destination_credit_usdc_atoms, 18_800_000);
        assert_eq!(
            receipt.evidence_id,
            format!(
                "papertrade-cctp-deposit:{TEST_SOURCE_HASH}:cctp-nonce:{TEST_FORWARD_HASH}:{TEST_SWEEP_HASH}"
            )
        );
        assert_eq!(settlement.attestations.load(Ordering::SeqCst), 1);
        assert_eq!(settlement.verifications.load(Ordering::SeqCst), 1);

        // A reconstructed route service with the same durable store sees the
        // immutable terminal receipt and does not query Circle or resubmit.
        let restarted = PapertradeFundingRoutes::new(
            credentials,
            gofin_credentials::CredentialRef::new("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap(),
            TEST_OWNER,
            BTreeMap::new(),
            Arc::new(InboundLedgerTransport {
                incoming_time: now_ms - 1_000,
                outgoing_time: now_ms - 900,
            }),
            gofin_hyperliquid::HyperliquidAccount {
                name: "test-papertrade-owner".into(),
                owner: TEST_OWNER.into(),
                credential_ref: Some("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".into()),
                signing_enabled: true,
                execution_scope: gofin_hyperliquid::HyperliquidExecutionScope::Owner,
            },
            settlement.clone(),
            None,
            Arc::new(
                crate::papertrade_funding_inbound::PapertradeInboundCctpSource::new(
                    BTreeMap::new(),
                    BTreeMap::new(),
                    None,
                    None,
                    BTreeMap::new(),
                )
                .unwrap(),
            ),
            None,
            Arc::new(crate::papertrade_runtime::PapertradeHttp::new().unwrap()),
            BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            restarted.prove_destination(&plan, &settled).await.unwrap(),
            Some(receipt)
        );
        assert_eq!(settlement.attestations.load(Ordering::SeqCst), 1);
        assert_eq!(settlement.verifications.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn circle_minimum_fee_is_parsed_as_exact_hundredths_of_a_basis_point() {
        assert_eq!(decimal_hundredths(Some(&json!(1))).unwrap(), 100);
        assert_eq!(decimal_hundredths(Some(&json!(1.25))).unwrap(), 125);
        assert_eq!(decimal_hundredths(Some(&json!("0.05"))).unwrap(), 5);
        assert!(decimal_hundredths(Some(&json!("1.001"))).is_err());
        assert!(decimal_hundredths(Some(&json!(-1))).is_err());
    }

    #[test]
    fn circle_quote_fee_rounding_uses_ceiling_and_adds_medium_forward_fee() {
        let amount_atoms = 10_000_000u64;
        let ct = 125u128;
        let proportional = (u128::from(amount_atoms) * ct).div_ceil(1_000_000);
        assert_eq!(proportional, 1_250);
        assert_eq!(u64::try_from(proportional).unwrap() + 258_363, 259_613);
    }

    #[test]
    fn cctp_native_route_caps_are_finite_and_chain_scoped() {
        assert_eq!(
            PapertradeFundingRoutes::native_fee_policy(0).unwrap(),
            (
                Some("ethereum:1:native".into()),
                Some("1000000000000000".into())
            )
        );
        assert_eq!(
            PapertradeFundingRoutes::native_fee_policy(3).unwrap(),
            (
                Some("ethereum:42161:native".into()),
                Some("100000000000000".into())
            )
        );
        assert_eq!(
            PapertradeFundingRoutes::native_fee_policy(5).unwrap(),
            (
                Some("solana:mainnet-beta:native".into()),
                Some("500000".into())
            )
        );
        assert_eq!(
            PapertradeFundingRoutes::native_fee_policy(19).unwrap(),
            (
                Some("hyperliquid:mainnet:HYPE".into()),
                Some("100000".into())
            )
        );
        assert!(PapertradeFundingRoutes::native_fee_policy(999).is_err());
    }

    #[test]
    fn core_cctp_send_fee_modes_are_independently_bounded() {
        let native_fee = json!({
            "type":"send",
            "user":TEST_OWNER,
            "destination":gofin_hyperliquid::funding::USDC_SYSTEM_ADDRESS,
            "sourceDex":"spot",
            "destinationDex":"spot",
            "token":"USDC",
            "amount":"19.9",
            "usdcValue":"19.9",
            "fee":"0.0",
            "feeToken":"",
            "nativeTokenFee":"0.00006755",
            "nonce":1234
        });
        assert_eq!(
            PapertradeFundingRoutes::verify_core_cctp_fee_row(
                &native_fee,
                TEST_OWNER,
                19_900_000,
                1234,
                100_000,
                100_000,
            ),
            Some((0, 6_755))
        );

        let usdc_fee = json!({
            "type":"send",
            "user":TEST_OWNER,
            "destination":gofin_hyperliquid::funding::USDC_SYSTEM_ADDRESS,
            "sourceDex":"spot",
            "destinationDex":"spot",
            "token":"USDC",
            "amount":"19.9",
            "usdcValue":"19.9",
            "fee":"0.016785",
            "feeToken":"USDC",
            "nativeTokenFee":"0.0",
            "nonce":1234
        });
        assert_eq!(
            PapertradeFundingRoutes::verify_core_cctp_fee_row(
                &usdc_fee, TEST_OWNER, 19_900_000, 1234, 100_000, 100_000,
            ),
            Some((16_785, 0))
        );

        let mut double_fee = native_fee.clone();
        double_fee["fee"] = json!("0.001");
        double_fee["feeToken"] = json!("USDC");
        assert!(
            PapertradeFundingRoutes::verify_core_cctp_fee_row(
                &double_fee,
                TEST_OWNER,
                19_900_000,
                1234,
                100_000,
                100_000,
            )
            .is_none()
        );

        assert!(
            PapertradeFundingRoutes::verify_core_cctp_fee_row(
                &native_fee,
                TEST_OWNER,
                19_900_000,
                1234,
                100_000,
                6_000,
            )
            .is_none()
        );
    }

    #[test]
    fn core_credit_action_must_be_unique_and_inside_the_exact_batch_item() {
        fn word(value: U256) -> String {
            format!("{:0>64x}", value)
        }
        fn address_word(value: &str) -> String {
            format!(
                "{:0>64}",
                value.trim_start_matches("0x").to_ascii_lowercase()
            )
        }
        let owner = "0x5502f38ed8ad519c3b9635ba723dddb94e991ecd";
        let exchange = gofin_papertrade::EXCHANGE;
        let intent_id = format!("0x{}", "ab".repeat(32));
        let batch_id = format!("0x{}", "cd".repeat(32));
        let amount_8dp = 31_992_069_800u64;
        let mut payload = vec![1, 0, 0, 6];
        payload.extend_from_slice(&[0; 12]);
        payload.extend_from_slice(&hex::decode(owner.trim_start_matches("0x")).unwrap());
        payload.extend_from_slice(&[0; 32]);
        payload.extend_from_slice(&U256::from(amount_8dp).to_be_bytes::<32>());
        assert_eq!(payload.len(), 100);
        let padded_payload = [&payload[..], &[0; 28][..]].concat();
        let writer_data = format!(
            "0x{}{}{}",
            word(U256::from(32u64)),
            word(U256::from(100u64)),
            hex::encode(padded_payload)
        );
        let transaction = gofin_papertrade::CanonicalTransaction {
            transaction_hash: format!("0x{}", "11".repeat(32)),
            transaction_from: None,
            transaction_to: gofin_papertrade::EXCHANGE.into(),
            transaction_input: "0x".into(),
            block_hash: format!("0x{}", "22".repeat(32)),
            block_number: 100,
            canonical: true,
            success: true,
            gas_used: None,
            effective_gas_price: None,
            logs: vec![
                gofin_papertrade::CanonicalLog {
                    address: "0x3333333333333333333333333333333333333333".into(),
                    topics: vec![
                        "0x8c7f585fb295f7eb1e6aeb8fba61b23a4fe60beda405f0045073b185c74412e3".into(),
                        format!("0x{}", address_word(exchange)),
                    ],
                    data: writer_data,
                    log_index: "0xa".into(),
                },
                gofin_papertrade::CanonicalLog {
                    address: exchange.into(),
                    topics: vec![
                        CORE_WITHDRAWAL_TOPIC.into(),
                        format!("0x{}", address_word(owner)),
                    ],
                    data: format!(
                        "0x{}{}",
                        word(U256::from(319_920_698_000_000_000_000u128)),
                        word(U256::from(amount_8dp))
                    ),
                    log_index: "0xb".into(),
                },
                gofin_papertrade::CanonicalLog {
                    address: gofin_papertrade::BATCH_EXECUTOR.into(),
                    topics: vec![
                        "0x8a23ef1120f3e42e722a0c0a599a66d55ab2650ecc39716f57e625684e349cad".into(),
                        batch_id.clone(),
                        format!("0x{}", word(U256::from(2u64))),
                        intent_id.clone(),
                    ],
                    data: "0x".into(),
                    log_index: "0xc".into(),
                },
            ],
            exchange_implementation: None,
            exchange_code_hash: None,
            staking_implementation: None,
            staking_code_hash: None,
            batch_implementation: None,
            batch_code_hash: None,
        };
        let verified = gofin_papertrade::VerifiedPapertradeCall {
            transaction_hash: transaction.transaction_hash.clone(),
            block_hash: transaction.block_hash.clone(),
            block_number: transaction.block_number,
            batch_id,
            batch_item_index: 2,
            intent_id,
            action: "withdrawToCore".into(),
            owner: owner.into(),
            target: exchange.into(),
            selector: "0x00000000".into(),
            call_return_data: None,
            event_ids: vec!["0x3".into()],
            events: vec![],
        };
        assert!(core_withdrawal_action_in_batch(
            &transaction,
            &verified,
            owner,
            amount_8dp
        ));
        let mut first_log_writer = transaction.clone();
        first_log_writer.logs[0].log_index = "0x0".into();
        assert!(core_withdrawal_action_in_batch(
            &first_log_writer,
            &verified,
            owner,
            amount_8dp
        ));
        let mut wrong_owner = transaction.clone();
        wrong_owner.logs[0].data = format!(
            "0x{}{}{}",
            word(U256::from(32u64)),
            word(U256::from(100u64)),
            hex::encode({
                let mut bytes = payload;
                bytes[4 + 12] ^= 1;
                [&bytes[..], &[0; 28][..]].concat()
            })
        );
        assert!(!core_withdrawal_action_in_batch(
            &wrong_owner,
            &verified,
            owner,
            amount_8dp
        ));
    }

    #[test]
    fn core_credit_requires_exchange_sender_and_time_after_canonical_withdrawal() {
        let owner = "0x5502f38ed8ad519c3b9635ba723dddb94e991ecd";
        let exchange = gofin_papertrade::EXCHANGE;
        let hash = format!("0x{}", "11".repeat(32));
        let ledger = json!({
            "time": 1_000u64,
            "hash": hash,
            "delta": {
                "type":"spotTransfer",
                "token":"USDC",
                "user":exchange,
                "destination":owner,
                "amount":"0.00001"
            }
        });
        let explorer = json!({"tx":{
            "hash":hash,
            "time":1_000u64,
            "error":null,
            "block":5,
            "user":exchange,
            "action":{
                "type":"SystemSpotSendAction",
                "destination":owner,
                "token":0,
                "wei":1_000u64
            }
        }});
        assert!(core_owner_credit_matches(
            &ledger, &explorer, owner, 10, 1_000, 1_000, 1_001
        ));
        let mut same_second_credit = ledger.clone();
        same_second_credit["time"] = json!(1_999u64);
        let mut same_second_explorer = explorer.clone();
        same_second_explorer["tx"]["time"] = json!(1_999u64);
        assert!(core_owner_credit_matches(
            &same_second_credit,
            &same_second_explorer,
            owner,
            10,
            1_000,
            1_000,
            2_001
        ));

        let mut forged_sender = ledger.clone();
        forged_sender["delta"]["user"] = json!(owner);
        assert!(!core_owner_credit_matches(
            &forged_sender,
            &explorer,
            owner,
            10,
            1_000,
            1_000,
            1_001
        ));

        let mut pre_event_credit = ledger;
        pre_event_credit["time"] = json!(999u64);
        let mut pre_event_explorer = explorer;
        pre_event_explorer["tx"]["time"] = json!(999u64);
        assert!(!core_owner_credit_matches(
            &pre_event_credit,
            &pre_event_explorer,
            owner,
            10,
            1_000,
            1_000,
            1_001
        ));
    }
}
