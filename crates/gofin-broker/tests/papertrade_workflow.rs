//! Named-plan principal quota regression through the real Papertrade adapter,
//! broker engine, store and protocol requests. All runtime evidence is synthetic.
use async_trait::async_trait;
use gofin_broker::{
    BrokerConfigV2, BrokerPolicy, NetworkConfig, RuntimeDependencies, SharedUsdValuator,
    compose_runtime_with_clock,
};
use gofin_domain::{clock::Clock, financial_plans::*, *};
use gofin_papertrade::*;
use gofin_protocol::BrokerRequest;
use gofin_store::Store;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

const OWNER: &str = "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf";
const AMOUNT: &str = "1000000000000000000000";
const STAKED_TOPIC: &str = "0x9e71bc8eea02a63969f509818f2dafb9254532904319f9dbda79b67bd34a5f3d";
const UNSTAKED_TOPIC: &str = "0x0f5bb82176feb1b5e747e28471aa92156a04d9f3ab9f45f28e2d704232b93f75";
const CLAIMED_TOPIC: &str = "0xd8138f8a3f377c5259ca548e70e4c2de94f129f5a11036a15b69513cba2b426a";
const CALL_EXECUTED_TOPIC: &str =
    "0x8a23ef1120f3e42e722a0c0a599a66d55ab2650ecc39716f57e625684e349cad";
const BATCH_IMPL: &str = "0xe0bc9cc7b067060aafccbaa3e250700c9cdd7262";
const BATCH_CODE_HASH: &str = "0xe926c659aca61daaaf9e68267817be4dae5152f11320a6ba970549dfdd15f62a";
const BATCH_EXECUTOR: &str = "0x233ff29b239abc1d7748605da9b205c3e008626e";
const PAPER_STAKING: &str = "0xaad6c7b0cc3014fc80ffedae0ed7ce5967b0f016";
const STAKING_IMPL: &str = "0xe6e3519455170913c5c3bfe541dd918a2d0cbb15";
const STAKING_CODE_HASH: &str =
    "0x54573c497e5109e353cf5330343323915e0c0f26e1b410fd3cb2cd8d1460f154";
const EXCHANGE: &str = "0x6cd5661646289fb6e65ea5c032310fded797d0a2";
const POSITION_OPENED_TOPIC: &str =
    "0x88451ea1189f73b554f0bf39c09868560ed149b3d788fcc933ff66ba926827f3";
const POSITION_CLOSED_TOPIC: &str =
    "0x87902ed2451849f465fc4d36a01d7f5c67b53af12f63ee85d2814a2d582f89bf";
const POSITIONS_CLOSED_CASH_TOPIC: &str =
    "0xa32e4800a59f03b131f5fea90951bdf00d8d505ad98f48ece29d8620a0bf09b0";
const EXCHANGE_IMPL: &str = "0x0f3febfc94876b0c895f7acecdc8f5c863b549ee";
const EXCHANGE_CODE_HASH: &str =
    "0x9d20287df886415035d53344a0dc8a1044949308cc3283ee0fe7ea5f3ea28d92";
const PAPER_TOKEN: &str = "0xe40f17915daa230324030003a197cdaef2261c0e";
const PAPER_TOKENOMICS: &str = "0x75174549dbbd44497d31a47e2e2d7406408d63d9";
const LIQUIDATED_TOPIC: &str = "0x0c859bc5db091ba3b9dc10c00f8dfc78b1f0b7e2566d107253ae67c069ae5de8";
const PAPER_LIQUIDATION_MINT_TOPIC: &str =
    "0x3050fba19ada85812db26b240c13c72cabec711cbf18c7c6b3554d511e2951ee";
const TRANSFER_TOPIC: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

struct TestClock;
impl Clock for TestClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }
}
struct Approval(AtomicUsize);
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        assert!(
            challenge.binding["plan"]["steps"].is_array() || challenge.purpose == "financial_write"
        );
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "synthetic Papertrade plan approval".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: TestClock.now().to_rfc3339(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

struct ConfigApproval;
#[async_trait]
impl gofin_engine::ConfigurationAuthorizer for ConfigApproval {
    async fn authorize_configuration(
        &self,
        challenge: &AuthorizationChallenge,
    ) -> Result<ApprovalAudit> {
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "synthetic human configuration approval".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: TestClock.now().to_rfc3339(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

struct Runtime {
    dispatches: AtomicUsize,
    registration_dispatches: AtomicUsize,
    funding_dispatches: AtomicUsize,
    funding_key_failures_remaining: AtomicUsize,
    funding_key_failure_seen: std::sync::atomic::AtomicBool,
    funding_operation_ids: std::sync::Mutex<Vec<String>>,
    signer: gofin_evm::LocalEnvelopeSigner,
    canonical: std::sync::Mutex<Option<CanonicalTransaction>>,
    positions: std::sync::Mutex<Vec<Value>>,
    paper_balance: std::sync::Mutex<u128>,
    staked_balance: std::sync::Mutex<u128>,
    pending_reward: std::sync::Mutex<u128>,
    reward_reserve: std::sync::Mutex<u128>,
    liquidation: std::sync::Mutex<Option<CanonicalTransaction>>,
    liquidation_credit_applied: std::sync::atomic::AtomicBool,
    session_registration: std::sync::Mutex<Option<BoundIntent>>,
    activated_registration: std::sync::Mutex<Option<String>>,
    cancel_observation: std::sync::Mutex<Option<CancelStatusProof>>,
    core_withdrawal_expiry: std::sync::Mutex<Option<CoreWithdrawalExpiryProof>>,
    funding_reconcile_calls: AtomicUsize,
}

fn to_signing_intent(
    intent: &PapertradeTypedIntent,
) -> gofin_evm::papertrade_signing::PapertradeTypedIntent {
    use gofin_evm::papertrade_signing::PapertradeTypedIntent as T;
    match intent {
        PapertradeTypedIntent::RegisterSessionKey {
            user,
            key,
            expiry,
            fee_address,
            nonce,
            deadline,
        } => T::RegisterSessionKey {
            user: user.clone(),
            key: key.clone(),
            expiry: *expiry,
            fee_address: fee_address.clone(),
            nonce: nonce.clone(),
            deadline: *deadline,
        },
        PapertradeTypedIntent::OpenPosition {
            user,
            instrument_id,
            is_long,
            size,
            leverage,
            use_debt,
            nonce,
            deadline,
        } => T::OpenPosition {
            user: user.clone(),
            instrument_id: *instrument_id,
            is_long: *is_long,
            size: *size,
            leverage: *leverage,
            use_debt: *use_debt,
            nonce: nonce.clone(),
            deadline: *deadline,
        },
        PapertradeTypedIntent::Close {
            user,
            position_ids,
            nonce,
            deadline,
        } => T::Close {
            user: user.clone(),
            position_ids: position_ids.clone(),
            nonce: nonce.clone(),
            deadline: *deadline,
        },
        PapertradeTypedIntent::Stake {
            user,
            amount,
            nonce,
            deadline,
        } => T::Stake {
            user: user.clone(),
            amount: amount.clone(),
            nonce: nonce.clone(),
            deadline: *deadline,
        },
        PapertradeTypedIntent::Unstake {
            user,
            amount,
            nonce,
            deadline,
        } => T::Unstake {
            user: user.clone(),
            amount: amount.clone(),
            nonce: nonce.clone(),
            deadline: *deadline,
        },
        PapertradeTypedIntent::Claim {
            user,
            nonce,
            deadline,
        } => T::Claim {
            user: user.clone(),
            nonce: nonce.clone(),
            deadline: *deadline,
        },
        PapertradeTypedIntent::CancelIntent {
            user,
            intent_id,
            deadline,
        } => T::CancelIntent {
            user: user.clone(),
            intent_id: intent_id.clone(),
            deadline: deadline.clone(),
        },
        _ => panic!("fixture supports only reviewed Papertrade intents"),
    }
}

fn word_u128(value: u128) -> [u8; 32] {
    let mut word = [0; 32];
    word[16..].copy_from_slice(&value.to_be_bytes());
    word
}

fn push_word(bytes: &mut Vec<u8>, value: u128) {
    bytes.extend_from_slice(&word_u128(value));
}

fn address_word(address: &str) -> [u8; 32] {
    let bytes = hex::decode(address.trim_start_matches("0x")).unwrap();
    assert_eq!(bytes.len(), 20);
    let mut word = [0; 32];
    word[12..].copy_from_slice(&bytes);
    word
}

fn stake_call(intent: &BoundIntent, v: &str, r: &str, s: &str) -> Result<Vec<u8>> {
    let PapertradeTypedIntent::Stake {
        user,
        amount,
        nonce,
        deadline,
    } = &intent.typed
    else {
        return Err(GoFinError::validation("expected fixture stake"));
    };
    let mut call = hex::decode("83115825").unwrap();
    call.extend_from_slice(&address_word(user));
    call.extend_from_slice(&word_u128(amount.parse().unwrap()));
    call.extend_from_slice(&word_u128(nonce.parse().unwrap()));
    call.extend_from_slice(&word_u128(u128::from(*deadline)));
    call.extend_from_slice(&word_u128(v.parse().unwrap()));
    call.extend_from_slice(&hex::decode(r.trim_start_matches("0x")).unwrap());
    call.extend_from_slice(&hex::decode(s.trim_start_matches("0x")).unwrap());
    Ok(call)
}

fn unstake_call(intent: &BoundIntent, v: &str, r: &str, s: &str) -> Result<Vec<u8>> {
    let PapertradeTypedIntent::Unstake {
        user,
        amount,
        nonce,
        deadline,
    } = &intent.typed
    else {
        return Err(GoFinError::validation("expected fixture unstake"));
    };
    let mut call = hex::decode("fa2871b5").unwrap();
    call.extend_from_slice(&address_word(user));
    for word in [
        amount
            .parse::<u128>()
            .map_err(|_| GoFinError::validation("bad unstake amount"))?,
        nonce
            .parse::<u128>()
            .map_err(|_| GoFinError::validation("bad nonce"))?,
        u128::from(*deadline),
        v.parse::<u128>()
            .map_err(|_| GoFinError::validation("bad v"))?,
    ] {
        push_word(&mut call, word);
    }
    call.extend_from_slice(&hex::decode(r.trim_start_matches("0x")).unwrap());
    call.extend_from_slice(&hex::decode(s.trim_start_matches("0x")).unwrap());
    Ok(call)
}

fn claim_call(intent: &BoundIntent, v: &str, r: &str, s: &str) -> Result<Vec<u8>> {
    let PapertradeTypedIntent::Claim {
        user,
        nonce,
        deadline,
    } = &intent.typed
    else {
        return Err(GoFinError::validation("expected fixture claim"));
    };
    let mut call = hex::decode("9c96d4a1").unwrap();
    call.extend_from_slice(&address_word(user));
    for word in [
        nonce
            .parse::<u128>()
            .map_err(|_| GoFinError::validation("bad nonce"))?,
        u128::from(*deadline),
        v.parse::<u128>()
            .map_err(|_| GoFinError::validation("bad v"))?,
    ] {
        push_word(&mut call, word);
    }
    call.extend_from_slice(&hex::decode(r.trim_start_matches("0x")).unwrap());
    call.extend_from_slice(&hex::decode(s.trim_start_matches("0x")).unwrap());
    Ok(call)
}

fn register_session_call(intent: &BoundIntent, v: &str, r: &str, s: &str) -> Result<Vec<u8>> {
    let PapertradeTypedIntent::RegisterSessionKey {
        user,
        key,
        expiry,
        fee_address,
        nonce,
        deadline,
    } = &intent.typed
    else {
        return Err(GoFinError::validation("expected fixture registration"));
    };
    let mut call = hex::decode("5f76a1d6").unwrap();
    call.extend_from_slice(&address_word(user));
    call.extend_from_slice(&address_word(key));
    push_word(&mut call, u128::from(*expiry));
    call.extend_from_slice(&address_word(fee_address));
    push_word(
        &mut call,
        nonce
            .parse()
            .map_err(|_| GoFinError::validation("bad nonce"))?,
    );
    push_word(&mut call, u128::from(*deadline));
    push_word(
        &mut call,
        v.parse().map_err(|_| GoFinError::validation("bad v"))?,
    );
    call.extend_from_slice(&hex::decode(r.trim_start_matches("0x")).unwrap());
    call.extend_from_slice(&hex::decode(s.trim_start_matches("0x")).unwrap());
    Ok(call)
}

fn registration_receipt(intent: &BoundIntent, call: Vec<u8>) -> Result<CanonicalTransaction> {
    let PapertradeTypedIntent::RegisterSessionKey {
        user, key, expiry, ..
    } = &intent.typed
    else {
        return Err(GoFinError::validation("expected fixture registration"));
    };
    let event = CanonicalLog {
        address: gofin_papertrade::SESSION_KEY_MANAGER.into(),
        topics: vec![
            "0x3cec0566535d67d6c92668517af0bf8a66cdc91d622dbb830fcc08e1bcf5d1df".into(),
            format!("0x{}", hex::encode(address_word(user))),
            format!("0x{}", hex::encode(address_word(key))),
        ],
        data: format!("0x{}", hex::encode(word_u128(u128::from(*expiry)))),
        log_index: "0x0".into(),
    };
    let executed = execution_log(
        &intent.intent_id,
        [word_u128(1), word_u128(64), word_u128(0)].concat(),
        1,
    );
    let mut tx = pinned_tx(
        encode_batch_to(
            &call,
            &intent.intent_id,
            gofin_papertrade::SESSION_KEY_MANAGER,
        ),
        vec![event, executed],
        "cf",
    );
    tx.exchange_implementation = None;
    tx.exchange_code_hash = None;
    Ok(tx)
}

fn encode_batch_to(call: &[u8], intent_id: &str, target: &str) -> String {
    let mut bytes = hex::decode("251fb27d").unwrap();
    let intent = hex::decode(intent_id.trim_start_matches("0x")).unwrap();
    assert_eq!(intent.len(), 32);
    push_word(&mut bytes, 32);
    push_word(&mut bytes, 1);
    push_word(&mut bytes, 32);
    bytes.extend_from_slice(&address_word(target));
    push_word(&mut bytes, 160);
    push_word(&mut bytes, 1);
    push_word(&mut bytes, 0);
    bytes.extend_from_slice(&intent);
    push_word(&mut bytes, call.len() as u128);
    bytes.extend_from_slice(call);
    while bytes.len() % 32 != 4 {
        bytes.push(0);
    }
    format!("0x{}", hex::encode(bytes))
}

fn open_call(intent: &BoundIntent, v: &str, r: &str, s: &str) -> Result<Vec<u8>> {
    let PapertradeTypedIntent::OpenPosition {
        user,
        instrument_id,
        is_long,
        size,
        leverage,
        use_debt,
        nonce,
        deadline,
    } = &intent.typed
    else {
        return Err(GoFinError::validation("expected fixture open"));
    };
    let mut call = hex::decode("ec6783f0").unwrap();
    call.extend_from_slice(&address_word(user));
    for word in [
        u128::from(*instrument_id),
        u128::from(*is_long),
        *size,
        u128::from(*leverage),
        u128::from(*use_debt),
        nonce.parse().unwrap(),
        u128::from(*deadline),
        v.parse().unwrap(),
    ] {
        push_word(&mut call, word);
    }
    call.extend_from_slice(&hex::decode(r.trim_start_matches("0x")).unwrap());
    call.extend_from_slice(&hex::decode(s.trim_start_matches("0x")).unwrap());
    Ok(call)
}

fn close_call(intent: &BoundIntent, v: &str, r: &str, s: &str) -> Result<Vec<u8>> {
    let PapertradeTypedIntent::Close {
        user,
        position_ids,
        nonce,
        deadline,
    } = &intent.typed
    else {
        return Err(GoFinError::validation("expected fixture close"));
    };
    let mut call = hex::decode("2792e706").unwrap();
    call.extend_from_slice(&address_word(user));
    let dynamic_offset = 7 * 32;
    for word in [
        dynamic_offset as u128,
        nonce.parse().unwrap(),
        u128::from(*deadline),
        v.parse().unwrap(),
    ] {
        push_word(&mut call, word);
    }
    call.extend_from_slice(&hex::decode(r.trim_start_matches("0x")).unwrap());
    call.extend_from_slice(&hex::decode(s.trim_start_matches("0x")).unwrap());
    push_word(&mut call, position_ids.len() as u128);
    for id in position_ids {
        push_word(&mut call, id.parse().unwrap());
    }
    Ok(call)
}

fn stake_receipt(
    intent: &BoundIntent,
    call: Vec<u8>,
    intent_id: &str,
) -> Result<CanonicalTransaction> {
    let PapertradeTypedIntent::Stake { user, amount, .. } = &intent.typed else {
        return Err(GoFinError::validation("expected fixture stake"));
    };
    let owner = hex::encode(address_word(user));
    let mut returned = Vec::new();
    returned.extend_from_slice(&word_u128(1));
    returned.extend_from_slice(&word_u128(64));
    returned.extend_from_slice(&word_u128(0));
    let call_log = CanonicalLog {
        address: BATCH_EXECUTOR.into(),
        topics: vec![
            CALL_EXECUTED_TOPIC.into(),
            format!("0x{}", "11".repeat(32)),
            format!("0x{}", "00".repeat(32)),
            intent_id.into(),
        ],
        data: format!("0x{}", hex::encode(returned)),
        log_index: "0x1".into(),
    };
    let staked = CanonicalLog {
        address: PAPER_STAKING.into(),
        topics: vec![STAKED_TOPIC.into(), format!("0x{}", owner)],
        data: format!("0x{}", hex::encode(word_u128(amount.parse().unwrap()))),
        log_index: "0x0".into(),
    };
    let mut tx = pinned_tx(
        encode_batch_to(&call, intent_id, PAPER_STAKING),
        vec![staked, call_log],
        "bb",
    );
    tx.exchange_implementation = None;
    tx.exchange_code_hash = None;
    tx.staking_implementation = Some(STAKING_IMPL.into());
    tx.staking_code_hash = Some(STAKING_CODE_HASH.into());
    Ok(tx)
}

fn unstake_receipt(intent: &BoundIntent, call: Vec<u8>) -> Result<CanonicalTransaction> {
    let PapertradeTypedIntent::Unstake { user, amount, .. } = &intent.typed else {
        return Err(GoFinError::validation("expected fixture unstake"));
    };
    let returned = [word_u128(1), word_u128(64), word_u128(0)].concat();
    let executed = execution_log(&intent.intent_id, returned, 1);
    let event = CanonicalLog {
        address: PAPER_STAKING.into(),
        topics: vec![
            UNSTAKED_TOPIC.into(),
            format!("0x{}", hex::encode(address_word(user))),
        ],
        data: format!("0x{}", hex::encode(word_u128(amount.parse().unwrap()))),
        log_index: "0x0".into(),
    };
    let mut tx = pinned_tx(
        encode_batch_to(&call, &intent.intent_id, PAPER_STAKING),
        vec![event, executed],
        "bc",
    );
    tx.exchange_implementation = None;
    tx.exchange_code_hash = None;
    tx.staking_implementation = Some(STAKING_IMPL.into());
    tx.staking_code_hash = Some(STAKING_CODE_HASH.into());
    Ok(tx)
}

fn claim_receipt(intent: &BoundIntent, call: Vec<u8>, paid: u128) -> Result<CanonicalTransaction> {
    let PapertradeTypedIntent::Claim { user, .. } = &intent.typed else {
        return Err(GoFinError::validation("expected fixture claim"));
    };
    let (mut returned, logs) = if paid == 0 {
        // BatchExecutor CallExecuted returns (true, abi.encode(uint256(0))).
        (
            [word_u128(1), word_u128(64), word_u128(32), word_u128(0)].concat(),
            vec![],
        )
    } else {
        let event = CanonicalLog {
            address: PAPER_STAKING.into(),
            topics: vec![
                CLAIMED_TOPIC.into(),
                format!("0x{}", hex::encode(address_word(user))),
            ],
            data: format!("0x{}", hex::encode(word_u128(paid))),
            log_index: "0x0".into(),
        };
        (
            [word_u128(1), word_u128(64), word_u128(32), word_u128(paid)].concat(),
            vec![event],
        )
    };
    let call_log = execution_log(
        &intent.intent_id,
        std::mem::take(&mut returned),
        logs.len() as u64,
    );
    let mut logs = logs;
    logs.push(call_log);
    let mut tx = pinned_tx(
        encode_batch_to(&call, &intent.intent_id, PAPER_STAKING),
        logs,
        if paid == 0 { "bd" } else { "be" },
    );
    tx.exchange_implementation = None;
    tx.exchange_code_hash = None;
    tx.staking_implementation = Some(STAKING_IMPL.into());
    tx.staking_code_hash = Some(STAKING_CODE_HASH.into());
    Ok(tx)
}

fn execution_log(intent_id: &str, data: Vec<u8>, index: u64) -> CanonicalLog {
    CanonicalLog {
        address: BATCH_EXECUTOR.into(),
        topics: vec![
            CALL_EXECUTED_TOPIC.into(),
            format!("0x{}", "11".repeat(32)),
            format!("0x{:064x}", 0),
            intent_id.into(),
        ],
        data: format!("0x{}", hex::encode(data)),
        log_index: format!("0x{index:x}"),
    }
}

fn pinned_tx(input: String, logs: Vec<CanonicalLog>, hash_byte: &str) -> CanonicalTransaction {
    CanonicalTransaction {
        transaction_hash: format!("0x{}", hash_byte.repeat(32)),
        transaction_from: None,
        transaction_to: BATCH_EXECUTOR.into(),
        transaction_input: input,
        block_hash: format!("0x{}", "cc".repeat(32)),
        block_number: 124,
        canonical: true,
        success: true,
        gas_used: None,
        effective_gas_price: None,
        logs,
        exchange_implementation: Some(EXCHANGE_IMPL.into()),
        exchange_code_hash: Some(EXCHANGE_CODE_HASH.into()),
        staking_implementation: None,
        staking_code_hash: None,
        batch_implementation: Some(BATCH_IMPL.into()),
        batch_code_hash: Some(BATCH_CODE_HASH.into()),
    }
}

fn liquidation_receipt(position_id: u64, paper_amount: u128) -> CanonicalTransaction {
    let owner_topic = format!("0x{}", hex::encode(address_word(OWNER)));
    let zero_topic = format!("0x{}", "00".repeat(32));
    let mut transfer_data = Vec::new();
    push_word(&mut transfer_data, paper_amount);
    let transfer = CanonicalLog {
        address: PAPER_TOKEN.into(),
        topics: vec![TRANSFER_TOPIC.into(), zero_topic, owner_topic.clone()],
        data: format!("0x{}", hex::encode(transfer_data)),
        log_index: "0x1".into(),
    };
    let mut mint_data = Vec::new();
    for value in [1, 25, 0, 25, 0, paper_amount] {
        push_word(&mut mint_data, value);
    }
    let mint = CanonicalLog {
        address: PAPER_TOKENOMICS.into(),
        topics: vec![
            PAPER_LIQUIDATION_MINT_TOPIC.into(),
            owner_topic.clone(),
            format!("0x{:064x}", position_id),
        ],
        data: format!("0x{}", hex::encode(mint_data)),
        log_index: "0x2".into(),
    };
    let mut liquidation_data = Vec::new();
    for value in [100, 101, 0, 0, 0] {
        push_word(&mut liquidation_data, value);
    }
    let liquidated = CanonicalLog {
        address: EXCHANGE.into(),
        topics: vec![
            LIQUIDATED_TOPIC.into(),
            format!("0x{:064x}", position_id),
            owner_topic,
        ],
        data: format!("0x{}", hex::encode(liquidation_data)),
        log_index: "0x3".into(),
    };
    CanonicalTransaction {
        transaction_hash: format!("0x{}", "a1".repeat(32)),
        transaction_from: None,
        transaction_to: EXCHANGE.into(),
        transaction_input: "0x".into(),
        block_hash: format!("0x{}", "b2".repeat(32)),
        block_number: 42,
        canonical: true,
        success: true,
        gas_used: None,
        effective_gas_price: None,
        logs: vec![transfer, mint, liquidated],
        exchange_implementation: Some(EXCHANGE_IMPL.into()),
        exchange_code_hash: Some(EXCHANGE_CODE_HASH.into()),
        staking_implementation: None,
        staking_code_hash: None,
        batch_implementation: None,
        batch_code_hash: None,
    }
}

fn open_receipt(
    intent: &BoundIntent,
    call: Vec<u8>,
    position_id: u64,
    hash_byte: &str,
) -> Result<CanonicalTransaction> {
    let PapertradeTypedIntent::OpenPosition {
        user,
        instrument_id,
        is_long,
        size,
        leverage,
        use_debt,
        ..
    } = &intent.typed
    else {
        return Err(GoFinError::validation("expected fixture open"));
    };
    let mut executed = Vec::new();
    push_word(&mut executed, 1);
    push_word(&mut executed, 64);
    push_word(&mut executed, 0);
    let mut opened = Vec::new();
    for word in [
        u128::from(*instrument_id),
        u128::from(*is_long),
        u128::from(*use_debt),
        50_000_000,
        *size,
        u128::from(*leverage),
    ] {
        push_word(&mut opened, word);
    }
    let position = CanonicalLog {
        address: EXCHANGE.into(),
        topics: vec![
            POSITION_OPENED_TOPIC.into(),
            format!("0x{:064x}", position_id),
            format!("0x{}", hex::encode(address_word(user))),
        ],
        data: format!("0x{}", hex::encode(opened)),
        log_index: "0x1".into(),
    };
    let call_log = execution_log(&intent.intent_id, executed, 1);
    let position = CanonicalLog {
        log_index: "0x0".into(),
        ..position
    };
    let tx = pinned_tx(
        encode_batch_to(&call, &intent.intent_id, EXCHANGE),
        vec![position, call_log],
        hash_byte,
    );
    Ok(tx)
}

fn close_receipt(
    intent: &BoundIntent,
    call: Vec<u8>,
    margin: u128,
    hash_byte: &str,
) -> Result<CanonicalTransaction> {
    let PapertradeTypedIntent::Close {
        user, position_ids, ..
    } = &intent.typed
    else {
        return Err(GoFinError::validation("expected fixture close"));
    };
    let mut executed = Vec::new();
    push_word(&mut executed, 1);
    push_word(&mut executed, 64);
    push_word(&mut executed, 0);
    let position_id = position_ids
        .first()
        .ok_or_else(|| GoFinError::validation("fixture close has no position"))?;
    let mut closed = Vec::new();
    for word in [50_000_000u128, 0, 0, 0, 0] {
        push_word(&mut closed, word);
    }
    let position = CanonicalLog {
        address: EXCHANGE.into(),
        topics: vec![
            POSITION_CLOSED_TOPIC.into(),
            format!("0x{:064x}", position_id.parse::<u128>().unwrap()),
            format!("0x{}", hex::encode(address_word(user))),
        ],
        data: format!("0x{}", hex::encode(closed)),
        log_index: "0x1".into(),
    };
    let mut cash = Vec::new();
    for word in [margin, margin, 0, 0] {
        push_word(&mut cash, word);
    }
    let cash_log = CanonicalLog {
        address: EXCHANGE.into(),
        topics: vec![
            POSITIONS_CLOSED_CASH_TOPIC.into(),
            format!("0x{}", hex::encode(address_word(user))),
        ],
        data: format!("0x{}", hex::encode(cash)),
        log_index: "0x2".into(),
    };
    let call_log = execution_log(&intent.intent_id, executed, 2);
    let position = CanonicalLog {
        log_index: "0x0".into(),
        ..position
    };
    let cash_log = CanonicalLog {
        log_index: "0x1".into(),
        ..cash_log
    };
    let tx = pinned_tx(
        encode_batch_to(&call, &intent.intent_id, EXCHANGE),
        vec![position, cash_log, call_log],
        hash_byte,
    );
    Ok(tx)
}
#[async_trait]
impl PapertradeRuntime for Runtime {
    async fn read(&self, request: ReadRequest) -> Result<Value> {
        match request {
            ReadRequest::TradingState => Ok(json!({
                "indexedThroughBlock":"123",
                "intakeControls":["1791673267280300","1000000000000000000000",[]]
            })),
            ReadRequest::Positions { owner } => Ok(
                json!({"owner":owner,"indexedThroughBlock":"123","positions":self.positions.lock().unwrap().clone()}),
            ),
            ReadRequest::StakingState { owner: _ } => Ok(json!({
                "paper": {
                    "balance": self.paper_balance.lock().unwrap().to_string(),
                    "staked": self.staked_balance.lock().unwrap().to_string(),
                    "pendingBase": self.pending_reward.lock().unwrap().to_string(),
                    "rewardDebt": "0"
                },
                "accRewardPerShareRaw":"0",
                "rewardReserveRaw":self.reward_reserve.lock().unwrap().to_string(),
                "claimableRewardsRaw":self.pending_reward.lock().unwrap().to_string()
            })),
            _ => Ok(json!({})),
        }
    }
    async fn valuation(&self, request: &IntentRequest) -> Result<Option<Valuation>> {
        if matches!(
            request,
            IntentRequest::Stake { .. } | IntentRequest::Unstake { .. }
        ) {
            let asset = paper_asset();
            let source = gofin_domain::unpriced::papertrade_principal_source(&asset)
                .unwrap()
                .to_owned();
            return Ok(Some(Valuation {
                movement_usd_micros: Quantity::zero(),
                maximum_fee_usd_micros: Quantity::zero(),
                additional_allocation_usd_micros: Quantity::zero(),
                observed_at: TestClock.now().to_rfc3339(),
                source: source.clone(),
                prices: vec![AssetPrice {
                    asset,
                    usd_micros_per_unit: Quantity::zero(),
                    unit_atoms: Quantity::new("1000000000000000000")?,
                    observed_at: TestClock.now().to_rfc3339(),
                    source,
                }],
            }));
        }
        if matches!(
            request,
            IntentRequest::Open { .. } | IntentRequest::Close { .. }
        ) {
            let asset = trading_usdc_asset();
            return Ok(Some(Valuation {
                movement_usd_micros: Quantity::new("10000000")?,
                maximum_fee_usd_micros: Quantity::zero(),
                additional_allocation_usd_micros: Quantity::zero(),
                observed_at: TestClock.now().to_rfc3339(),
                source: "synthetic exact internal USDC fixture at $1".into(),
                prices: vec![AssetPrice {
                    asset,
                    usd_micros_per_unit: Quantity::new("1000000")?,
                    unit_atoms: Quantity::new("1000000000000000000")?,
                    observed_at: TestClock.now().to_rfc3339(),
                    source: "synthetic exact internal USDC fixture at $1".into(),
                }],
            }));
        }
        if matches!(request, IntentRequest::Cancel { .. }) {
            return Ok(Some(Valuation {
                movement_usd_micros: Quantity::zero(),
                maximum_fee_usd_micros: Quantity::zero(),
                additional_allocation_usd_micros: Quantity::zero(),
                observed_at: TestClock.now().to_rfc3339(),
                source: "synthetic cancellation fixture; no financial effects".into(),
                prices: vec![],
            }));
        }
        if matches!(request, IntentRequest::Claim { .. }) {
            let amount = *self.pending_reward.lock().unwrap();
            let asset = trading_usdc_asset();
            return Ok(Some(Valuation {
                movement_usd_micros: Quantity::new((amount / 1_000_000_000_000).to_string())?,
                maximum_fee_usd_micros: Quantity::zero(),
                additional_allocation_usd_micros: Quantity::zero(),
                observed_at: TestClock.now().to_rfc3339(),
                source: "synthetic exact internal USDC claim fixture at $1".into(),
                prices: vec![AssetPrice {
                    asset,
                    usd_micros_per_unit: Quantity::new("1000000")?,
                    unit_atoms: Quantity::new("1000000000000000000")?,
                    observed_at: TestClock.now().to_rfc3339(),
                    source: "synthetic exact internal USDC claim fixture at $1".into(),
                }],
            }));
        }
        Ok(None)
    }
    async fn prepare_intent(
        &self,
        request: IntentRequest,
        _operation_id: &str,
    ) -> Result<BoundIntent> {
        let nonce = (self.dispatches.load(Ordering::SeqCst) + 7).to_string();
        let cancel_target_action =
            matches!(&request, IntentRequest::Cancel { .. }).then(|| "openPosition".into());
        let typed = match request {
            IntentRequest::Open {
                owner,
                instrument_id,
                is_long,
                margin_wad,
                leverage,
            } => PapertradeTypedIntent::OpenPosition {
                user: owner,
                instrument_id,
                is_long,
                size: margin_wad,
                leverage,
                use_debt: false,
                nonce,
                deadline: (TestClock.now().timestamp() as u64).saturating_add(600),
            },
            IntentRequest::Close {
                owner,
                position_ids,
            } => PapertradeTypedIntent::Close {
                user: owner,
                position_ids,
                nonce,
                deadline: (TestClock.now().timestamp() as u64).saturating_add(600),
            },
            IntentRequest::Stake { owner, amount } => PapertradeTypedIntent::Stake {
                user: owner,
                amount: amount.to_string(),
                nonce,
                deadline: (TestClock.now().timestamp() as u64).saturating_add(600),
            },
            IntentRequest::Unstake { owner, amount } => PapertradeTypedIntent::Unstake {
                user: owner,
                amount: amount.to_string(),
                nonce,
                deadline: (TestClock.now().timestamp() as u64).saturating_add(600),
            },
            IntentRequest::Claim { owner } => PapertradeTypedIntent::Claim {
                user: owner,
                nonce,
                deadline: (TestClock.now().timestamp() as u64).saturating_add(600),
            },
            IntentRequest::Cancel { owner, intent_id } => PapertradeTypedIntent::CancelIntent {
                user: owner,
                intent_id,
                deadline: (TestClock.now().timestamp() as u64)
                    .saturating_add(600)
                    .to_string(),
            },
            _ => return Err(GoFinError::validation("unused test intent")),
        };
        let signing_intent = to_signing_intent(&typed);
        let signer = gofin_evm::LocalEnvelopeSigner::from_private_key(&format!("0x{:064x}", 1))?;
        let signed =
            signer.sign_papertrade_intent(&signing_intent, TestClock.now().timestamp() as u64)?;
        Ok(BoundIntent {
            action: typed.action().map(str::to_owned),
            typed,
            intent_id: signed.intent_id,
            signer: OWNER.into(),
            registration_id: None,
            cancel_target_action,
        })
    }
    async fn prepare_session_registration(
        &self,
        owner: &str,
        _registration_id: &str,
        expiry: u64,
        _operation_id: &str,
    ) -> Result<BoundIntent> {
        let session_signer =
            gofin_evm::LocalEnvelopeSigner::from_private_key(&format!("0x{:064x}", 2))?;
        let typed = PapertradeTypedIntent::RegisterSessionKey {
            user: owner.to_ascii_lowercase(),
            key: format!("{:#x}", session_signer.address()),
            expiry,
            fee_address: "0x0000000000000000000000000000000000000000".into(),
            nonce: "1".into(),
            deadline: (TestClock.now().timestamp() as u64).saturating_add(600),
        };
        let signing_intent = to_signing_intent(&typed);
        let signed = self
            .signer
            .sign_papertrade_intent(&signing_intent, TestClock.now().timestamp() as u64)?;
        let intent = BoundIntent {
            action: typed.action().map(str::to_owned),
            typed,
            intent_id: signed.intent_id,
            signer: owner.to_ascii_lowercase(),
            registration_id: Some(_registration_id.to_owned()),
            cancel_target_action: None,
        };
        *self.session_registration.lock().unwrap() = Some(intent.clone());
        Ok(intent)
    }
    async fn revalidate_intent(
        &self,
        _request: &IntentRequest,
        _bound: Option<&BoundIntent>,
    ) -> Result<()> {
        Ok(())
    }
    async fn prepare_session_revocation(
        &self,
        _owner: &str,
        _registration_id: &str,
        _operation_id: &str,
    ) -> Result<BoundSessionRevoke> {
        Err(GoFinError::validation(
            "unused session revocation preparation",
        ))
    }
    async fn revalidate_session_configuration(
        &self,
        _binding: &SessionConfigurationBinding,
    ) -> Result<()> {
        Ok(())
    }
    async fn dispatch(&self, intent: &BoundIntent) -> Result<Receipt> {
        let dispatch_index = self.dispatches.fetch_add(1, Ordering::SeqCst);
        let signing_intent = to_signing_intent(&intent.typed);
        let now = TestClock.now().timestamp() as u64;
        let signed = self.signer.sign_papertrade_intent(&signing_intent, now)?;
        if matches!(intent.typed, PapertradeTypedIntent::CancelIntent { .. }) {
            // Model endpoint acceptance only; cancel_status remains empty
            // until the authenticated target-state fixture is advanced.
            return Ok(Receipt {
                intent_id: intent.intent_id.clone(),
                action: "cancelIntent".into(),
                status: "accepted".into(),
                transaction_hash: None,
            });
        }
        let (tx, hash_byte) = match &intent.typed {
            PapertradeTypedIntent::OpenPosition { .. } => {
                let call = open_call(intent, &signed.v, &signed.r, &signed.s)?;
                let position_id = 42 + dispatch_index as u64;
                let hash_byte = format!("{:02x}", 0xaa + dispatch_index as u8);
                let tx = open_receipt(intent, call, position_id, &hash_byte)?;
                let row = match &intent.typed {
                    PapertradeTypedIntent::OpenPosition {
                        instrument_id,
                        is_long,
                        use_debt,
                        size,
                        leverage,
                        ..
                    } => json!({
                        "positionId":position_id.to_string(), "instrumentId":instrument_id, "isLong":is_long, "useDebt":use_debt,
                        "entryPriceRaw":"50000000", "marginRaw":size.to_string(), "leverage":leverage,
                        "bustPriceRaw":"45000000", "openedAtMs":123, "intentId":intent.intent_id
                    }),
                    _ => unreachable!(),
                };
                self.positions.lock().unwrap().push(row);
                (tx, hash_byte)
            }
            PapertradeTypedIntent::Close { .. } => {
                let selected = self.positions.lock().unwrap().clone();
                let PapertradeTypedIntent::Close { position_ids, .. } = &intent.typed else {
                    unreachable!()
                };
                let selected = selected
                    .into_iter()
                    .filter(|row| {
                        row.get("positionId")
                            .and_then(Value::as_str)
                            .is_some_and(|id| position_ids.contains(&id.to_owned()))
                    })
                    .collect::<Vec<_>>();
                let margin = selected
                    .iter()
                    .map(|p| {
                        p.get("marginRaw")
                            .and_then(Value::as_str)
                            .and_then(|v| v.parse::<u128>().ok())
                            .ok_or_else(|| GoFinError::validation("missing fixture position"))
                    })
                    .try_fold(0u128, |sum, margin| {
                        sum.checked_add(margin?)
                            .ok_or_else(|| GoFinError::validation("fixture margin overflow"))
                    })?;
                let call = close_call(intent, &signed.v, &signed.r, &signed.s)?;
                let hash_byte = format!("{:02x}", 0xdd + dispatch_index as u8);
                let tx = close_receipt(intent, call, margin, &hash_byte)?;
                self.positions.lock().unwrap().retain(|row| {
                    !row.get("positionId")
                        .and_then(Value::as_str)
                        .is_some_and(|id| position_ids.contains(&id.to_owned()))
                });
                (tx, hash_byte)
            }
            PapertradeTypedIntent::Stake { .. } => {
                let call = stake_call(intent, &signed.v, &signed.r, &signed.s)?;
                let amount = match &intent.typed {
                    PapertradeTypedIntent::Stake { amount, .. } => amount.parse::<u128>().unwrap(),
                    _ => unreachable!(),
                };
                let mut wallet = self.paper_balance.lock().unwrap();
                *wallet = wallet
                    .checked_sub(amount)
                    .ok_or_else(|| GoFinError::validation("fixture PAPER balance underflow"))?;
                *self.staked_balance.lock().unwrap() += amount;
                (
                    stake_receipt(intent, call, &signed.intent_id)?,
                    "bb".to_owned(),
                )
            }
            PapertradeTypedIntent::Unstake { amount, .. } => {
                let amount = amount
                    .parse::<u128>()
                    .map_err(|_| GoFinError::validation("fixture unstake amount is malformed"))?;
                let call = unstake_call(intent, &signed.v, &signed.r, &signed.s)?;
                let mut staked = self.staked_balance.lock().unwrap();
                *staked = staked
                    .checked_sub(amount)
                    .ok_or_else(|| GoFinError::validation("fixture staked balance underflow"))?;
                *self.paper_balance.lock().unwrap() += amount;
                (unstake_receipt(intent, call)?, "bc".to_owned())
            }
            PapertradeTypedIntent::Claim { .. } => {
                let call = claim_call(intent, &signed.v, &signed.r, &signed.s)?;
                let paid = *self.pending_reward.lock().unwrap();
                *self.pending_reward.lock().unwrap() = 0;
                *self.reward_reserve.lock().unwrap() = 0;
                (
                    claim_receipt(intent, call, paid)?,
                    if paid == 0 {
                        "bd".to_owned()
                    } else {
                        "be".to_owned()
                    },
                )
            }
            _ => return Err(GoFinError::validation("unsupported fixture intent")),
        };
        *self.canonical.lock().unwrap() = Some(tx);
        Ok(Receipt {
            intent_id: intent.intent_id.clone(),
            action: intent.action.clone().unwrap_or_default(),
            status: "submitted".into(),
            transaction_hash: Some(format!("0x{}", hash_byte.repeat(32))),
        })
    }
    async fn dispatch_session_registration(
        &self,
        binding: &SessionRegistrationBinding,
    ) -> Result<Receipt> {
        let intent = self
            .session_registration
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| GoFinError::validation("missing protected session intent"))?;
        if intent.intent_id != binding.intent_id
            || intent.registration_id.as_deref() != Some(binding.registration_id.as_str())
        {
            return Err(GoFinError::validation("session public binding mismatch"));
        }
        let signing_intent = to_signing_intent(&intent.typed);
        let signed = self
            .signer
            .sign_papertrade_intent(&signing_intent, TestClock.now().timestamp() as u64)?;
        let call = register_session_call(&intent, &signed.v, &signed.r, &signed.s)?;
        let tx = registration_receipt(&intent, call)?;
        self.registration_dispatches.fetch_add(1, Ordering::SeqCst);
        let hash = tx.transaction_hash.clone();
        *self.canonical.lock().unwrap() = Some(tx);
        Ok(Receipt {
            intent_id: intent.intent_id,
            action: "registerSessionKey".into(),
            status: "submitted".into(),
            transaction_hash: Some(hash),
        })
    }
    async fn session_registration_intent(
        &self,
        binding: &SessionRegistrationBinding,
    ) -> Result<Option<BoundIntent>> {
        let intent = self.session_registration.lock().unwrap().clone();
        Ok(intent.filter(|intent| {
            intent.intent_id == binding.intent_id
                && intent.registration_id.as_deref() == Some(binding.registration_id.as_str())
        }))
    }
    async fn activate_session_registration(
        &self,
        binding: &SessionRegistrationBinding,
        proof: &VerifiedPapertradeCall,
    ) -> Result<()> {
        if proof.intent_id != binding.intent_id || proof.owner != binding.owner.to_ascii_lowercase()
        {
            return Err(GoFinError::validation(
                "canonical registration proof mismatch",
            ));
        }
        *self.activated_registration.lock().unwrap() = Some(binding.registration_id.clone());
        Ok(())
    }
    async fn dispatch_session_revocation(
        &self,
        _binding: &SessionRevocationBinding,
        _journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Receipt> {
        Err(GoFinError::validation("unused session revocation dispatch"))
    }
    async fn session_revocation_transaction(
        &self,
        _binding: &SessionRevocationBinding,
    ) -> Result<Option<BoundSessionRevoke>> {
        Ok(None)
    }
    async fn deactivate_session_registration(
        &self,
        _binding: &SessionRevocationBinding,
        _proof: &VerifiedSessionRevocation,
    ) -> Result<()> {
        Err(GoFinError::validation(
            "unused session revocation deactivation",
        ))
    }
    async fn canonical_transaction(&self, hash: &str) -> Result<Option<CanonicalTransaction>> {
        Ok(self
            .canonical
            .lock()
            .unwrap()
            .as_ref()
            .filter(|tx| tx.transaction_hash == hash)
            .cloned())
    }
    async fn canonical_intent_transaction(
        &self,
        intent_id: &str,
        _from_block: u64,
    ) -> Result<Option<CanonicalTransaction>> {
        Ok(self
            .canonical
            .lock()
            .unwrap()
            .as_ref()
            .filter(|tx| {
                tx.logs
                    .iter()
                    .any(|log| log.topics.get(3).is_some_and(|topic| topic == intent_id))
            })
            .cloned())
    }
    async fn cancel_status(
        &self,
        owner: &str,
        target_intent_id: &str,
        _from_block: u64,
    ) -> Result<Option<CancelStatusProof>> {
        Ok(self
            .cancel_observation
            .lock()
            .unwrap()
            .as_ref()
            .filter(|proof| {
                proof.owner.eq_ignore_ascii_case(owner)
                    && proof
                        .target_intent_id
                        .eq_ignore_ascii_case(target_intent_id)
            })
            .cloned())
    }
    async fn liquidation_transaction(
        &self,
        owner: &str,
        position_id: &str,
        _since_block: Option<u64>,
    ) -> Result<Option<CanonicalTransaction>> {
        if !owner.eq_ignore_ascii_case(OWNER) || position_id != "77" {
            return Ok(None);
        }
        if !self.liquidation_credit_applied.swap(true, Ordering::SeqCst) {
            *self.paper_balance.lock().unwrap() += 1_000_000_000_000_000_000_000;
        }
        Ok(self.liquidation.lock().unwrap().clone())
    }
    async fn funding_valuation(&self, intent: &FundingIntent) -> Result<Option<Valuation>> {
        if intent.route != FundingRoute::Core {
            return Ok(None);
        }
        let asset = AssetId {
            network: "hyperliquid:mainnet".into(),
            reference: "USDC".into(),
            decimals: 36,
        };
        let source = "synthetic controlled Core USDC funding fixture at $1";
        Ok(Some(Valuation {
            movement_usd_micros: Quantity::new(
                (u128::from(intent.amount_usdc_atoms) / 1_000_000 * 1_000_000).to_string(),
            )?,
            maximum_fee_usd_micros: Quantity::zero(),
            additional_allocation_usd_micros: Quantity::zero(),
            observed_at: TestClock.now().to_rfc3339(),
            source: source.into(),
            prices: vec![AssetPrice {
                asset,
                usd_micros_per_unit: Quantity::new("1000000")?,
                unit_atoms: Quantity::new(format!("1{}", "0".repeat(36)))?,
                observed_at: TestClock.now().to_rfc3339(),
                source: source.into(),
            }],
        }))
    }
    async fn prepare_funding(
        &self,
        intent: FundingIntent,
        operation_id: &str,
    ) -> Result<FundingBinding> {
        self.funding_operation_ids
            .lock()
            .unwrap()
            .push(operation_id.to_owned());
        if intent.route != FundingRoute::Core || intent.direction != FundingDirection::Deposit {
            return Err(GoFinError::validation(
                "fixture supports Core deposits only",
            ));
        }
        Ok(FundingBinding {
            version: 1,
            operation_id: operation_id.into(),
            intent: intent.clone(),
            source_identity: format!("hyperliquid:mainnet:{}:spot:primary", intent.owner),
            destination_identity: format!("papertrade:mainnet:{}", intent.owner),
            bounds: FundingFinancialBounds {
                source_usdc_atoms: intent.amount_usdc_atoms,
                maximum_source_fee_usdc_atoms: 0,
                destination_min_usdc_atoms: intent.amount_usdc_atoms,
                maximum_cctp_fee_usdc_atoms: 0,
                maximum_activation_fee_atoms: 0,
                native_fee_asset: None,
                maximum_native_fee_atoms: None,
                maximum_native_source_debit_atoms: None,
                maximum_refundable_native_funding_atoms: None,
            },
            transaction: None,
            solana_transaction: None,
            solana_message_event_account: None,
            papertrade_intent: None,
        })
    }
    async fn dispatch_funding(
        &self,
        binding: &FundingBinding,
        _submission: &Submission,
        _validated: &RevalidatedOperation,
        _journal: Arc<dyn SubmissionJournal>,
    ) -> Result<FundingProgress> {
        self.funding_dispatches.fetch_add(1, Ordering::SeqCst);
        if self
            .funding_key_failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            self.funding_key_failure_seen.store(true, Ordering::SeqCst);
            return Err(GoFinError::new(
                "INVALID_INPUT",
                "validation",
                "Invalid development Papertrade runtime key.",
                "The controlled fixture models local validation before any leg submission.",
            ));
        }
        Ok(fixture_funding_progress(binding))
    }
    async fn reconcile_funding(&self, binding: &FundingBinding) -> Result<FundingProgress> {
        self.funding_reconcile_calls.fetch_add(1, Ordering::SeqCst);
        if self.funding_key_failure_seen.load(Ordering::SeqCst)
            && self.funding_key_failures_remaining.load(Ordering::SeqCst) == 0
            && self.funding_dispatches.load(Ordering::SeqCst) == 1
        {
            return Ok(FundingProgress {
                status: FundingProgressStatus::Unknown,
                receipt: None,
                evidence: json!({
                    "operation_id":binding.operation_id,
                    "legs":[{"ordinal":0,"status":"not_submitted","submission_id":format!("papertrade-funding:{}:0",binding.operation_id)}]
                }),
            });
        }
        Ok(fixture_funding_progress(binding))
    }
    async fn core_withdrawal_expiry_proof(
        &self,
        _bound: &BoundIntent,
    ) -> Result<Option<CoreWithdrawalExpiryProof>> {
        Ok(self.core_withdrawal_expiry.lock().unwrap().clone())
    }
}

fn fixture_funding_progress(binding: &FundingBinding) -> FundingProgress {
    let atoms = binding.bounds.source_usdc_atoms;
    FundingProgress {
        status: FundingProgressStatus::Settled,
        receipt: Some(FundingReceipt {
            stage: FundingReceiptStage::FinalDestination,
            status: "settled".into(),
            amount_usdc_atoms: atoms,
            source_debit_usdc_atoms: atoms,
            destination_credit_usdc_atoms: atoms,
            actual_source_fee_usdc_atoms: 0,
            actual_cctp_fee_usdc_atoms: 0,
            retained_source_credit_usdc_atoms: 0,
            activation_fee_usdc_atoms: 0,
            actual_native_fee_atoms: None,
            actual_native_fee_asset: None,
            retained_source_native_atoms: None,
            retained_source_native_asset: None,
            source_identity: binding.source_identity.clone(),
            destination_identity: binding.destination_identity.clone(),
            evidence_id: format!("synthetic-core-deposit:{}", binding.operation_id),
        }),
        evidence: json!({"source":"synthetic controlled funding transport","operation_id":binding.operation_id}),
    }
}

fn core_withdrawal_expiry_fixture(
    owner: &str,
    route: FundingRoute,
    expired: bool,
) -> (FundingBinding, CoreWithdrawalExpiryProof) {
    let deadline = chrono::Utc::now().timestamp().max(1) as u64 - if expired { 60 } else { 0 };
    let intent_id = format!("0x{}", "ab".repeat(32));
    let bound = BoundIntent {
        typed: PapertradeTypedIntent::WithdrawToCore {
            user: owner.to_owned(),
            amount: "19000000".into(),
            nonce: "17".into(),
            deadline,
        },
        intent_id: intent_id.clone(),
        signer: owner.to_owned(),
        action: Some("withdrawToCore".into()),
        registration_id: None,
        cancel_target_action: None,
    };
    let operation_id = "papertrade-withdrawal-expiry-fixture";
    let destination_identity = match &route {
        FundingRoute::Core => format!("hyperliquid:mainnet:{owner}:spot:primary"),
        FundingRoute::Hyperevm => format!("ethereum:999:{owner}"),
        FundingRoute::Ethereum => format!("ethereum:1:{owner}"),
        FundingRoute::Arbitrum => format!("ethereum:42161:{owner}"),
        FundingRoute::Solana => format!("solana:mainnet-beta:{owner}"),
    };
    let binding = FundingBinding {
        version: 1,
        operation_id: operation_id.into(),
        intent: FundingIntent {
            direction: FundingDirection::Withdraw,
            route,
            owner: owner.to_owned(),
            amount_usdc_atoms: 19_000_000,
            request_id: operation_id.into(),
        },
        source_identity: format!("papertrade:mainnet:{owner}"),
        destination_identity,
        bounds: FundingFinancialBounds {
            source_usdc_atoms: 19_000_000,
            maximum_source_fee_usdc_atoms: 0,
            destination_min_usdc_atoms: 19_000_000,
            maximum_cctp_fee_usdc_atoms: 0,
            maximum_activation_fee_atoms: 0,
            native_fee_asset: None,
            maximum_native_fee_atoms: None,
            maximum_native_source_debit_atoms: None,
            maximum_refundable_native_funding_atoms: None,
        },
        transaction: None,
        solana_transaction: None,
        solana_message_event_account: None,
        papertrade_intent: Some(bound),
    };
    let now = chrono::Utc::now().timestamp().max(0) as u64;
    let proof = CoreWithdrawalExpiryProof {
        chain_id: CHAIN_ID,
        owner: owner.to_ascii_lowercase(),
        intent_id,
        nonce: "17".into(),
        deadline,
        canonical_block_number: 48_718_516,
        canonical_block_hash: format!("0x{}", "cd".repeat(32)),
        canonical_block_timestamp: if expired { now } else { deadline },
        exchange_proxy: EXCHANGE.into(),
        exchange_implementation: EXCHANGE_IMPLEMENTATION.into(),
        exchange_code_hash: EXCHANGE_CODE_HASH.into(),
        session_key_manager_proxy: SESSION_KEY_MANAGER.into(),
        session_key_manager_implementation: SESSION_KEY_MANAGER_IMPLEMENTATION.into(),
        session_key_manager_code_hash: SESSION_KEY_MANAGER_CODE_HASH.into(),
        nonce_unused: true,
        intent_absent: true,
    };
    (binding, proof)
}

fn funding_withdrawal_record(binding: &FundingBinding) -> OperationRecord {
    let submission = Submission {
        kind: "papertrade_funding".into(),
        reference: binding.operation_id.clone(),
        data: json!({
            "operation_id":binding.operation_id,
            "request_id":binding.intent.request_id,
            "direction":binding.intent.direction,
            "route":binding.intent.route,
            "amount_usdc_atoms":binding.intent.amount_usdc_atoms
        }),
    };
    OperationRecord {
        id: "00000000-0000-4000-8000-000000000011".into(),
        request_id: "papertrade-withdraw-expiry-request".into(),
        fingerprint: "0xfixture".into(),
        fingerprint_version: 1,
        operation: "papertrade.withdraw".into(),
        input: json!({"route":"core","amount_usdc_atoms":19000000}),
        status: Status::Unknown,
        created_at: TestClock.now().to_rfc3339(),
        updated_at: TestClock.now().to_rfc3339(),
        preview: Some(PreparedOperation {
            operation: "papertrade.withdraw".into(),
            input: json!({"route":"core","amount_usdc_atoms":19000000}),
            binding: json!({"funding_binding":binding}),
            summary: "Synthetic Core withdrawal expiry test".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: TestClock.now().to_rfc3339(),
            expires_at: TestClock.now().to_rfc3339(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        }),
        approval: None,
        authorization_source: None,
        submission: Some(submission),
        dispatch_evidence: None,
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    }
}

fn plan(asset: AssetId, resource: ResourceId) -> FinancialPlanSpec {
    let debit = ResourceAmount {
        resource: resource.clone(),
        amount: Amount {
            asset: asset.clone(),
            atoms: Quantity::new(AMOUNT).unwrap(),
        },
    };
    FinancialPlanSpec {
        version: 1,
        name: "Stake exact PAPER principal".into(),
        summary: None,
        steps: vec![FinancialPlanStepSpec {
            step_id: "stake".into(),
            operation: "papertrade.stake".into(),
            input: json!({"amount":"1000"}),
            permitted_variations: json!({
                "mode":"exact",
                "maximum_debits":[debit],
                "maximum_costs":[],
                "possible_credits":[{"resource":format!("papertrade:mainnet:{OWNER}:paper_staked"),"asset":asset}]
            }),
            completion: FinancialStepCompletion {
                operation_succeeded: true,
                principal_spend: None,
                minimum_net_receipt: None,
            },
            funding_dependencies: vec![],
            action_deadline_seconds: None,
        }],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: vec![debit],
            maximum_costs: vec![],
        },
    }
}

fn exact_operation_plan(
    name: &str,
    step_id: &str,
    operation: &str,
    input: Value,
    maximum_debits: Vec<ResourceAmount>,
    possible_credits: Vec<FinancialPlanFundingIdentity>,
) -> FinancialPlanSpec {
    FinancialPlanSpec {
        version: 1,
        name: name.into(),
        summary: None,
        steps: vec![FinancialPlanStepSpec {
            step_id: step_id.into(),
            operation: operation.into(),
            input,
            permitted_variations: json!({
                "mode":"exact",
                "maximum_debits":maximum_debits.clone(),
                "maximum_costs":[],
                "possible_credits":possible_credits.clone()
            }),
            completion: FinancialStepCompletion {
                operation_succeeded: true,
                principal_spend: None,
                minimum_net_receipt: None,
            },
            funding_dependencies: vec![],
            action_deadline_seconds: None,
        }],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits,
            maximum_costs: vec![],
        },
    }
}

async fn authorize_plan(
    client: &SocketClient,
    request_id: &str,
    plan: FinancialPlanSpec,
) -> String {
    let mut request = FinancialPlanAuthorizationRequest {
        request_id: request_id.into(),
        plan,
        valid_for_seconds: Some(600),
        replaces_revision_id: None,
        expected_review_hash: None,
    };
    let preview = client
        .call(BrokerRequest::PreviewPlan {
            request: request.clone(),
        })
        .await;
    assert!(preview.ok, "{:?}", preview.error);
    request.expected_review_hash = Some(
        preview.result.unwrap()["review_hash"]
            .as_str()
            .unwrap()
            .into(),
    );
    let authorized = client.call(BrokerRequest::AuthorizePlan { request }).await;
    assert!(authorized.ok, "{:?}", authorized.error);
    authorized.result.unwrap()["plan_id"]
        .as_str()
        .unwrap()
        .into()
}

async fn execute_plan_step(
    client: &SocketClient,
    plan_id: &str,
    step_id: &str,
    operation: &str,
    request_id: &str,
    input: Value,
) -> gofin_protocol::BrokerResponse {
    client
        .call(BrokerRequest::ExecutePlanStep {
            plan_id: plan_id.into(),
            step_id: step_id.into(),
            request: OperationRequest {
                request_id: request_id.into(),
                operation: operation.into(),
                input,
                run_id: None,
            },
        })
        .await
}

async fn run_named_step(
    client: &SocketClient,
    plan_request_id: &str,
    step_id: &str,
    operation: &str,
    request_id: &str,
    input: Value,
    plan: FinancialPlanSpec,
) -> Value {
    let plan_id = authorize_plan(client, plan_request_id, plan).await;
    let preview = client
        .call(BrokerRequest::PreviewPlanStep {
            plan_id: plan_id.clone(),
            step_id: step_id.into(),
            operation: operation.into(),
            input: input.clone(),
        })
        .await;
    assert!(preview.ok, "{operation} preview: {:?}", preview.error);
    let mut result =
        execute_plan_step(client, &plan_id, step_id, operation, request_id, input).await;
    assert!(result.ok, "{operation} execution: {:?}", result.error);
    if result
        .result
        .as_ref()
        .is_some_and(|row| row["status"] == "pending")
    {
        let operation_id = result.result.as_ref().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        result = client
            .call(BrokerRequest::Status { id: operation_id })
            .await;
    }
    assert!(result.ok, "{operation} reconciliation: {:?}", result.error);
    assert_eq!(
        result.result.as_ref().unwrap()["status"],
        "completed",
        "{operation}: {:?}",
        result.result
    );
    result.result.unwrap()
}

fn make_runtime(
    store_path: &std::path::Path,
    approval: Arc<Approval>,
) -> (
    Arc<gofin_broker::BrokerRuntime>,
    Arc<PapertradeAdapter<Runtime>>,
    Arc<Runtime>,
) {
    make_runtime_with_movement_cap(store_path, approval, "1")
}

fn make_runtime_with_movement_cap(
    store_path: &std::path::Path,
    approval: Arc<Approval>,
    movement_cap: &str,
) -> (
    Arc<gofin_broker::BrokerRuntime>,
    Arc<PapertradeAdapter<Runtime>>,
    Arc<Runtime>,
) {
    let mut policy = BrokerPolicy::beta();
    // One microdollar cap proves that exact PAPER principal is excluded from
    // USD movement accounting; the asset quantity remains fully exact.
    policy.max_movement_usd_micros = movement_cap.into();
    policy.max_run_allocation_usd_micros = movement_cap.into();
    let papertrade_runtime = Arc::new(Runtime {
        dispatches: AtomicUsize::new(0),
        registration_dispatches: AtomicUsize::new(0),
        funding_dispatches: AtomicUsize::new(0),
        funding_key_failures_remaining: AtomicUsize::new(0),
        funding_key_failure_seen: std::sync::atomic::AtomicBool::new(false),
        funding_operation_ids: std::sync::Mutex::new(Vec::new()),
        signer: gofin_evm::LocalEnvelopeSigner::from_private_key(&format!("0x{:064x}", 1)).unwrap(),
        canonical: std::sync::Mutex::new(None),
        positions: std::sync::Mutex::new(Vec::new()),
        paper_balance: std::sync::Mutex::new(1_000_000_000_000_000_000_000),
        staked_balance: std::sync::Mutex::new(0),
        pending_reward: std::sync::Mutex::new(2_000_000_000_000_000_000),
        reward_reserve: std::sync::Mutex::new(2_000_000_000_000_000_000),
        liquidation: std::sync::Mutex::new(Some(liquidation_receipt(
            77,
            1_000_000_000_000_000_000_000,
        ))),
        liquidation_credit_applied: std::sync::atomic::AtomicBool::new(false),
        session_registration: std::sync::Mutex::new(None),
        activated_registration: std::sync::Mutex::new(None),
        cancel_observation: std::sync::Mutex::new(None),
        core_withdrawal_expiry: std::sync::Mutex::new(None),
        funding_reconcile_calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(
        PapertradeAdapter::new(
            papertrade_runtime.clone(),
            PapertradeAccount {
                name: "synthetic-papertrade".into(),
                owner: OWNER.into(),
                signing_enabled: true,
            },
        )
        .unwrap(),
    );
    let broker = compose_runtime_with_clock(
        BrokerConfigV2 {
            version: 2,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy,
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        RuntimeDependencies {
            store: Store::open(store_path).unwrap(),
            adapters: vec![adapter.clone()],
            authorizer: approval,
            valuator: Arc::new(SharedUsdValuator::with_source(Arc::new(NoPrices))),
            bridge: None,
        },
        Arc::new(TestClock),
    )
    .unwrap();
    (broker, adapter, papertrade_runtime)
}
struct NoPrices;
#[async_trait]
impl gofin_pricing::UsdPriceSource for NoPrices {
    async fn quote(&self, asset: &AssetId) -> Result<gofin_pricing::UsdQuote> {
        panic!("unexpected USD quote for already-valued exact asset {asset:?}")
    }
}

struct SocketClient {
    path: std::path::PathBuf,
    server: tokio::task::JoinHandle<()>,
}
impl SocketClient {
    async fn new_cli(runtime: Arc<gofin_broker::BrokerRuntime>, dir: &std::path::Path) -> Self {
        let binary = std::env::var_os("GOFIN_PAPERTRADE_CLI")
            .expect("required Papertrade CLI rehearsal needs GOFIN_PAPERTRADE_CLI");
        let binary = std::path::PathBuf::from(binary)
            .canonicalize()
            .expect("GOFIN_PAPERTRADE_CLI must name an existing development CLI binary");
        assert!(binary.is_file(), "GOFIN_PAPERTRADE_CLI must be a file");
        Self::new(runtime, dir).await
    }

    async fn new(runtime: Arc<gofin_broker::BrokerRuntime>, dir: &std::path::Path) -> Self {
        let path = dir.join("papertrade-workflow.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let cli_mode = std::env::var_os("GOFIN_PAPERTRADE_CLI").is_some();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let runtime = runtime.clone();
                tokio::spawn(async move {
                    if !cli_mode {
                        gofin_broker::serve_connection(stream, &runtime, unsafe {
                            libc::geteuid()
                        })
                        .await
                        .unwrap();
                        return;
                    }
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    let hello: Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(hello["identity"], "synthetic-papertrade-workflow");
                    let reply = json!({"kind":"hello","identity":hello["identity"],"build_id":hello["build_id"],"mode":"synthetic"});
                    reader
                        .get_mut()
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .unwrap();
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    let input: Value = serde_json::from_str(&line).unwrap();
                    let request: BrokerRequest =
                        serde_json::from_value(input["request"].clone()).unwrap();
                    let response = gofin_protocol::progress::observe(
                        reader.get_mut(),
                        input["progress"].as_bool().unwrap_or(false),
                        runtime.handle(request),
                    )
                    .await;
                    let reply = json!({"identity":hello["identity"],"build_id":hello["build_id"],"mode":"synthetic","response":response});
                    reader
                        .get_mut()
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .unwrap();
                });
            }
        });
        Self { path, server }
    }
    async fn call(&self, request: BrokerRequest) -> gofin_protocol::BrokerResponse {
        if std::env::var_os("GOFIN_PAPERTRADE_CLI").is_some() {
            let binary = std::path::PathBuf::from(std::env::var("GOFIN_PAPERTRADE_CLI").unwrap())
                .canonicalize()
                .expect("GOFIN_PAPERTRADE_CLI must name the built development CLI");
            let input = |value: &Value| serde_json::to_string(value).unwrap();
            let mut args = vec!["--json".to_owned()];
            match request {
                BrokerRequest::PreviewPlan { request } => {
                    args.extend([
                        "plans".into(),
                        "preview".into(),
                        "--input".into(),
                        input(&json!(request.plan)),
                    ]);
                    if let Some(seconds) = request.valid_for_seconds {
                        args.extend(["--valid-for".into(), format!("{seconds}s")]);
                    }
                }
                BrokerRequest::AuthorizePlan { request } => {
                    args.extend([
                        "plans".into(),
                        "authorize".into(),
                        "--input".into(),
                        input(&json!(request.plan)),
                        "--id".into(),
                        request.request_id,
                    ]);
                    if let Some(seconds) = request.valid_for_seconds {
                        args.extend(["--valid-for".into(), format!("{seconds}s")]);
                    }
                    if let Some(hash) = request.expected_review_hash {
                        args.extend(["--review-hash".into(), hash]);
                    }
                    if let Some(revision) = request.replaces_revision_id {
                        args.extend(["--replaces".into(), revision]);
                    }
                }
                BrokerRequest::PreviewPlanStep {
                    plan_id,
                    step_id,
                    operation,
                    input: value,
                } => {
                    args.extend([
                        "execute".into(),
                        operation,
                        "--input".into(),
                        input(&value),
                        "--plan".into(),
                        plan_id,
                        "--step".into(),
                        step_id,
                        "--dry-run".into(),
                    ]);
                }
                BrokerRequest::ExecutePlanStep {
                    plan_id,
                    step_id,
                    request,
                } => {
                    args.extend([
                        "execute".into(),
                        request.operation,
                        "--input".into(),
                        input(&request.input),
                        "--id".into(),
                        request.request_id,
                        "--plan".into(),
                        plan_id,
                        "--step".into(),
                        step_id,
                    ]);
                }
                BrokerRequest::PlanStatus { plan_id } => {
                    args.extend(["plans".into(), "status".into(), plan_id]);
                }
                BrokerRequest::Status { id } => args.extend(["status".into(), id]),
                BrokerRequest::Execute { request } => args.extend([
                    "execute".into(),
                    request.operation,
                    "--input".into(),
                    input(&request.input),
                    "--id".into(),
                    request.request_id,
                ]),
                BrokerRequest::DevelopmentConfigure { request } => args.extend([
                    "configure".into(),
                    request.operation,
                    "--input".into(),
                    input(&request.input),
                    "--id".into(),
                    request.request_id,
                ]),
                other => panic!("CLI workflow mapping is not defined for {other:?}"),
            }
            let output = tokio::task::spawn_blocking({
                let socket = self.path.clone();
                move || {
                    std::process::Command::new(binary)
                        .args([
                            "--identity",
                            "synthetic-papertrade-workflow",
                            "--build-id",
                            "synthetic-papertrade-workflow-build",
                        ])
                        .args(args)
                        .env("GOFIN_DEV_IDENTITY", "synthetic-papertrade-workflow")
                        .env("GOFIN_DEV_BUILD_ID", "synthetic-papertrade-workflow-build")
                        .env("GOFIN_DEV_SOCKET", socket)
                        .output()
                        .unwrap()
                }
            })
            .await
            .unwrap();
            assert!(
                output.stderr.is_empty(),
                "machine CLI must keep stderr quiet: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            if output.status.success() {
                return gofin_protocol::BrokerResponse::success(value);
            }
            let error = serde_json::from_value(value["error"].clone()).unwrap_or_else(|_| {
                GoFinError::validation("Papertrade workflow CLI returned an unstructured error.")
            });
            return gofin_protocol::BrokerResponse::failure(error);
        }
        let mut stream = UnixStream::connect(&self.path).await.unwrap();
        let envelope = gofin_protocol::progress::RequestEnvelope {
            request,
            progress: false,
            cancel_on_disconnect: false,
        };
        let mut bytes = serde_json::to_vec(&envelope).unwrap();
        bytes.push(b'\n');
        stream.write_all(&bytes).await.unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }
}
impl Drop for SocketClient {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

#[test]
fn real_papertrade_adapter_named_plan_accepts_exact_principal_but_rejects_asset_substitution() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let approval = Arc::new(Approval(AtomicUsize::new(0)));
            let (runtime, adapter, papertrade_runtime) =
                make_runtime(&dir.path().join("papertrade-plan.sqlite"), approval.clone());
            runtime.engine.recover_interrupted().unwrap();
            let client = SocketClient::new(runtime.clone(), dir.path()).await;
            let prepared = adapter
                .prepare("papertrade.stake", &json!({"amount":"1000"}))
                .await
                .unwrap();
            let trading = adapter
                .read("papertrade.trading_state", &json!({}))
                .await
                .unwrap();
            assert_eq!(trading["minimum_paper_stake_raw"], "1000000000000000000000");
            assert_eq!(
                prepared.valuation.as_ref().unwrap().movement_usd_micros,
                Quantity::zero()
            );
            assert_eq!(
                prepared.warnings,
                vec![gofin_domain::unpriced::PAPERTRADE_PRINCIPAL_WARNING]
            );
            let _: BoundIntent =
                serde_json::from_value(prepared.binding["bound_intent"].clone()).unwrap();
            assert!(
                adapter
                    .prepare(
                        "papertrade.stake",
                        &json!({"amount":"999.999999999999999999"})
                    )
                    .await
                    .is_err(),
                "the exact current protocol minimum must be enforced"
            );

            let valid_plan = plan(
                paper_asset(),
                ResourceId(format!("papertrade:mainnet:{OWNER}:paper_wallet")),
            );
            let mut request = FinancialPlanAuthorizationRequest {
                request_id: "papertrade-principal-quota".into(),
                plan: valid_plan.clone(),
                valid_for_seconds: Some(600),
                replaces_revision_id: None,
                expected_review_hash: None,
            };
            let preview = client
                .call(BrokerRequest::PreviewPlan {
                    request: request.clone(),
                })
                .await;
            assert!(preview.ok, "{:?}", preview.error);
            request.expected_review_hash = Some(
                preview.result.unwrap()["review_hash"]
                    .as_str()
                    .unwrap()
                    .into(),
            );
            let authorized = client.call(BrokerRequest::AuthorizePlan { request }).await;
            assert!(authorized.ok, "{:?}", authorized.error);
            assert_eq!(approval.0.load(Ordering::SeqCst), 1);
            let plan_id = authorized.result.unwrap()["plan_id"]
                .as_str()
                .unwrap()
                .to_owned();
            let step = client
                .call(BrokerRequest::PreviewPlanStep {
                    plan_id: plan_id.clone(),
                    step_id: "stake".into(),
                    operation: "papertrade.stake".into(),
                    input: json!({"amount":"1000"}),
                })
                .await;
            assert!(
                step.ok,
                "exact 1000 PAPER step is below the configured $0.000001 movement ceiling: {:?}",
                step.error
            );
            assert_eq!(
                step.result.unwrap()["valuation"]["movement_usd_micros"],
                "0"
            );

            let execute_request = OperationRequest {
                request_id: "papertrade-stake-attempt".into(),
                operation: "papertrade.stake".into(),
                input: json!({"amount":"1000"}),
                run_id: None,
            };
            adapter.revalidate(&prepared).await.unwrap();
            let execute = || BrokerRequest::ExecutePlanStep {
                plan_id: plan_id.clone(),
                step_id: "stake".into(),
                request: execute_request.clone(),
            };
            let first = client.call(execute()).await;
            if let Some(encoded) = first
                .result
                .as_ref()
                .and_then(|r| r.get("preview"))
                .and_then(|p| p.get("binding"))
                .and_then(|b| b.get("bound_intent"))
            {
                serde_json::from_value::<BoundIntent>(encoded.clone()).unwrap();
            }
            assert!(first.ok, "{:?}", first.error);
            assert_eq!(
                papertrade_runtime.dispatches.load(Ordering::SeqCst),
                1,
                "first execute did not dispatch: {:?}",
                first.result
            );
            let recovered = client.call(execute()).await;
            assert!(recovered.ok, "{:?}", recovered.error);
            assert_eq!(
                papertrade_runtime.dispatches.load(Ordering::SeqCst),
                1,
                "uncertain submission recovery must not replay a write"
            );

            let counterfeit = AssetId {
                network: "ethereum:999".into(),
                reference: "0x000000000000000000000000000000000000dead".into(),
                decimals: 18,
            };
            let counterfeit_plan = plan(
                counterfeit,
                ResourceId(format!("papertrade:mainnet:{OWNER}:paper_wallet")),
            );
            let mut bad_request = FinancialPlanAuthorizationRequest {
                request_id: "papertrade-counterfeit-principal".into(),
                plan: counterfeit_plan,
                valid_for_seconds: Some(600),
                replaces_revision_id: None,
                expected_review_hash: None,
            };
            let bad_preview = client
                .call(BrokerRequest::PreviewPlan {
                    request: bad_request.clone(),
                })
                .await;
            // The user-supplied exact scope may be reviewed, but cannot authorize
            // an adapter candidate that actually debits the reviewed PAPER token.
            assert!(bad_preview.ok, "{:?}", bad_preview.error);
            bad_request.expected_review_hash = Some(
                bad_preview.result.unwrap()["review_hash"]
                    .as_str()
                    .unwrap()
                    .into(),
            );
            let bad_auth = client
                .call(BrokerRequest::AuthorizePlan {
                    request: bad_request,
                })
                .await;
            assert!(bad_auth.ok, "{:?}", bad_auth.error);
            let bad_step = client
                .call(BrokerRequest::PreviewPlanStep {
                    plan_id: bad_auth.result.unwrap()["plan_id"].as_str().unwrap().into(),
                    step_id: "stake".into(),
                    operation: "papertrade.stake".into(),
                    input: json!({"amount":"1000"}),
                })
                .await;
            assert!(
                !bad_step.ok,
                "counterfeit principal must not match exact Papertrade effect identity"
            );
        });
}

#[test]
#[ignore = "requires the built development CLI; run make test-papertrade-rehearsal"]
fn papertrade_cli_session_registration_reconciles_exact_canonical_receipt() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let (broker, _adapter, runtime) = make_runtime_with_movement_cap(
                &dir.path().join("papertrade-session-registration.sqlite"),
                Arc::new(Approval(AtomicUsize::new(0))),
                "5000000",
            );
            broker
                .attach_development_configuration_authorizer(Arc::new(ConfigApproval))
                .unwrap();
            broker.engine.recover_interrupted().unwrap();
            let client = SocketClient::new_cli(broker, dir.path()).await;
            let expiry = (TestClock.now().timestamp() as u64) + 3600;
            let request = OperationRequest {
                request_id: "papertrade-session-register-canonical".into(),
                operation: "papertrade.session_register".into(),
                input: json!({"registration_id":"fixture-session-1","expiry":expiry}),
                run_id: None,
            };
            let response = client
                .call(BrokerRequest::DevelopmentConfigure {
                    request: request.clone(),
                })
                .await;
            assert!(response.ok, "configuration execution: {:?}", response.error);
            let record = response.result.unwrap();
            assert_eq!(record["status"], "completed", "record: {record:?}");
            assert_eq!(record["submission"]["kind"], "papertrade_session_register");
            assert_eq!(
                record["dispatch_evidence"]["receipt"]["status"],
                "submitted"
            );
            assert_eq!(runtime.registration_dispatches.load(Ordering::SeqCst), 1);
            assert_eq!(
                runtime.activated_registration.lock().unwrap().as_deref(),
                Some("fixture-session-1"),
                "credential activation must follow exact canonical proof",
            );
            let serialized = serde_json::to_string(&record).unwrap();
            let protected_key = match &runtime
                .session_registration
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .typed
            {
                PapertradeTypedIntent::RegisterSessionKey { key, .. } => key.clone(),
                _ => unreachable!(),
            };
            assert!(
                !serialized.contains(&protected_key),
                "generated session key leaked: {serialized}"
            );

            // The actual CLI recovery/status route is idempotent: replaying the
            // same logical request returns the stored canonical result only.
            let duplicate = client
                .call(BrokerRequest::DevelopmentConfigure { request })
                .await;
            assert!(
                duplicate.ok,
                "idempotent config retry: {:?}",
                duplicate.error
            );
            assert_eq!(duplicate.result.unwrap()["status"], "completed");
            assert_eq!(runtime.registration_dispatches.load(Ordering::SeqCst), 1);
        });
}

#[test]
#[ignore = "requires the built development CLI; run make test-papertrade-rehearsal"]
fn papertrade_cli_stake_claim_zero_unstake_and_liquidation_award_roundtrip() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let (broker, adapter, runtime) = make_runtime_with_movement_cap(
                &dir.path().join("papertrade-staking-lifecycle.sqlite"),
                Arc::new(Approval(AtomicUsize::new(0))),
                "5000000",
            );
            broker.engine.recover_interrupted().unwrap();
            let client = SocketClient::new_cli(broker, dir.path()).await;
            let paper = paper_asset();
            let usdc = trading_usdc_asset();
            let wallet = ResourceId(format!("papertrade:mainnet:{OWNER}:paper_wallet"));
            let staked = ResourceId(format!("papertrade:mainnet:{OWNER}:paper_staked"));
            let wad = Quantity::new(AMOUNT).unwrap();

            let stake_result = run_named_step(
                &client,
                "staking-roundtrip-stake-plan",
                "stake",
                "papertrade.stake",
                "staking-roundtrip-stake",
                json!({"amount":"1000"}),
                plan(paper.clone(), wallet.clone()),
            )
            .await;
            assert_eq!(*runtime.paper_balance.lock().unwrap(), 0);
            assert_eq!(
                *runtime.staked_balance.lock().unwrap(),
                1_000_000_000_000_000_000_000
            );
            assert_eq!(stake_result["effects"]["effects"][0]["kind"], "transfer");

            let claim_plan = || {
                exact_operation_plan(
                    "Claim available Papertrade rewards",
                    "claim",
                    "papertrade.claim",
                    json!({}),
                    vec![],
                    vec![FinancialPlanFundingIdentity {
                        resource: ResourceId(format!("papertrade:mainnet:{OWNER}:trading_cash")),
                        asset: usdc.clone(),
                    }],
                )
            };
            let claimed = run_named_step(
                &client,
                "staking-roundtrip-positive-claim-plan",
                "claim",
                "papertrade.claim",
                "staking-roundtrip-positive-claim",
                json!({}),
                claim_plan(),
            )
            .await;
            assert_eq!(claimed["effects"]["effects"][0]["kind"], "settlement");
            assert_eq!(
                claimed["effects"]["effects"][0]["delta"],
                "2000000000000000000"
            );
            assert_eq!(*runtime.pending_reward.lock().unwrap(), 0);

            let zero_claim = run_named_step(
                &client,
                "staking-roundtrip-zero-claim-plan",
                "claim-zero",
                "papertrade.claim",
                "staking-roundtrip-zero-claim",
                json!({}),
                exact_operation_plan(
                    "Claim any available Papertrade rewards",
                    "claim-zero",
                    "papertrade.claim",
                    json!({}),
                    vec![],
                    vec![],
                ),
            )
            .await;
            assert_eq!(
                zero_claim["status"], "completed",
                "zero claim: {zero_claim:?}"
            );
            assert_eq!(runtime.dispatches.load(Ordering::SeqCst), 3);

            let unstake_debit = ResourceAmount {
                resource: staked.clone(),
                amount: Amount {
                    asset: paper.clone(),
                    atoms: wad.clone(),
                },
            };
            let unstaked = run_named_step(
                &client,
                "staking-roundtrip-unstake-plan",
                "unstake",
                "papertrade.unstake",
                "staking-roundtrip-unstake",
                json!({"amount":"1000"}),
                exact_operation_plan(
                    "Unstake exact PAPER principal",
                    "unstake",
                    "papertrade.unstake",
                    json!({"amount":"1000"}),
                    vec![unstake_debit],
                    vec![FinancialPlanFundingIdentity {
                        resource: wallet.clone(),
                        asset: paper.clone(),
                    }],
                ),
            )
            .await;
            assert_eq!(unstaked["status"], "completed");
            assert_eq!(*runtime.staked_balance.lock().unwrap(), 0);
            assert_eq!(
                *runtime.paper_balance.lock().unwrap(),
                1_000_000_000_000_000_000_000
            );

            // Keeper liquidation is read-only and carries no GoFin submission.
            // The fixture models the subsequent indexed wallet balance including
            // exactly the award proven in this receipt.
            let liquidation = adapter
                .read(
                    "papertrade.liquidation",
                    &json!({"position_id":"77","since_block":40}),
                )
                .await
                .unwrap();
            assert_eq!(liquidation["state"], "liquidated");
            assert_eq!(liquidation["paper_minted_raw"], AMOUNT);
            assert_eq!(
                liquidation["transaction_hash"],
                format!("0x{}", "a1".repeat(32))
            );
            assert_eq!(
                *runtime.paper_balance.lock().unwrap(),
                2_000_000_000_000_000_000_000
            );

            let restake = run_named_step(
                &client,
                "staking-roundtrip-liquidation-restake-plan",
                "restake",
                "papertrade.stake",
                "staking-roundtrip-liquidation-restake",
                json!({"amount":"1000"}),
                exact_operation_plan(
                    "Stake the canonical liquidation PAPER award",
                    "restake",
                    "papertrade.stake",
                    json!({"amount":"1000"}),
                    vec![ResourceAmount {
                        resource: wallet,
                        amount: Amount {
                            asset: paper.clone(),
                            atoms: wad,
                        },
                    }],
                    vec![FinancialPlanFundingIdentity {
                        resource: staked,
                        asset: paper,
                    }],
                ),
            )
            .await;
            assert_eq!(restake["status"], "completed");
            assert_eq!(
                *runtime.paper_balance.lock().unwrap(),
                1_000_000_000_000_000_000_000
            );
            assert_eq!(
                *runtime.staked_balance.lock().unwrap(),
                1_000_000_000_000_000_000_000
            );
            assert_eq!(runtime.dispatches.load(Ordering::SeqCst), 5);
        });
}

#[test]
fn papertrade_cli_local_funding_key_failure_is_terminal_without_replay() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let (broker, adapter, runtime) = make_runtime_with_movement_cap(
                &dir.path().join("papertrade-funding-key-recovery.sqlite"),
                Arc::new(Approval(AtomicUsize::new(0))),
                "20000000",
            );
            runtime
                .funding_key_failures_remaining
                .store(1, Ordering::SeqCst);
            broker.engine.recover_interrupted().unwrap();
            let client = SocketClient::new(broker.clone(), dir.path()).await;
            let input = json!({"route":"core","amount":"10"});
            let source_asset = AssetId {
                network: "hyperliquid:mainnet".into(),
                reference: "USDC".into(),
                decimals: 36,
            };
            let source_resource = ResourceId(format!("hyperliquid:mainnet:{OWNER}:spot:primary"));
            let plan = exact_operation_plan(
                "Recover a Core USDC deposit after a local runtime configuration error",
                "deposit",
                "papertrade.deposit",
                input.clone(),
                vec![ResourceAmount {
                    resource: source_resource,
                    amount: Amount {
                        asset: source_asset,
                        atoms: Quantity::new(format!("{}{}", 10_000_000u64, "0".repeat(30)))
                            .unwrap(),
                    },
                }],
                vec![FinancialPlanFundingIdentity {
                    resource: adapter.account().trading_cash_resource(),
                    asset: trading_usdc_asset(),
                }],
            );
            let plan_id = authorize_plan(&client, "papertrade-core-key-recovery-plan", plan).await;
            let first = execute_plan_step(
                &client,
                &plan_id,
                "deposit",
                "papertrade.deposit",
                "papertrade-core-key-invalid-1",
                input.clone(),
            )
            .await;
            assert!(first.ok, "initial local failure record: {:?}", first.error);
            let first = first.result.unwrap();
            let first_id = first["id"].as_str().unwrap().to_owned();
            let recovered = client
                .call(BrokerRequest::Status {
                    id: first_id.clone(),
                })
                .await;
            assert!(recovered.ok, "status recovery: {:?}", recovered.error);
            let recovered = recovered.result.unwrap();
            assert_eq!(
                recovered["status"], "failed",
                "recovered failure: {recovered:?}"
            );
            assert_eq!(recovered["effects"]["effects"], json!([]));
            assert_eq!(
                recovered["last_reconciliation_evidence"]["state"],
                "not_dispatched"
            );
            assert_eq!(
                recovered["last_reconciliation_evidence"]["financial_effects"],
                json!([])
            );
            assert_eq!(runtime.funding_dispatches.load(Ordering::SeqCst), 1);

            // Replaying the same logical request returns its immutable terminal
            // record and never calls the funding transport again.
            let duplicate = execute_plan_step(
                &client,
                &plan_id,
                "deposit",
                "papertrade.deposit",
                "papertrade-core-key-invalid-1",
                input.clone(),
            )
            .await;
            assert!(
                duplicate.ok,
                "idempotent failed retry: {:?}",
                duplicate.error
            );
            assert_eq!(duplicate.result.unwrap()["id"], first_id);
            assert_eq!(runtime.funding_dispatches.load(Ordering::SeqCst), 1);

            // A deliberate new attempt has a new request and durable route id.
            let second = execute_plan_step(
                &client,
                &plan_id,
                "deposit",
                "papertrade.deposit",
                "papertrade-core-key-invalid-2",
                input,
            )
            .await;
            assert!(second.ok, "new explicit attempt: {:?}", second.error);
            let second = second.result.unwrap();
            assert_ne!(second["id"], first_id);
            assert_eq!(second["status"], "completed", "new attempt: {second:?}");
            assert_eq!(runtime.funding_dispatches.load(Ordering::SeqCst), 2);
            let funding_ids = runtime.funding_operation_ids.lock().unwrap();
            assert_eq!(funding_ids.len(), 2);
            assert_ne!(funding_ids[0], funding_ids[1]);
        });
}

#[test]
fn real_papertrade_adapter_cli_core_deposit_effects_match_controlled_receipt() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let approval = Arc::new(Approval(AtomicUsize::new(0)));
            let (broker, adapter, runtime) = make_runtime_with_movement_cap(
                &dir.path().join("papertrade-core-deposit.sqlite"),
                approval,
                "20000000",
            );
            broker.engine.recover_interrupted().unwrap();
            let client = SocketClient::new(broker.clone(), dir.path()).await;
            let atoms = 10_000_000u64;
            let source_asset = AssetId {
                network: "hyperliquid:mainnet".into(),
                reference: "USDC".into(),
                decimals: 36,
            };
            let source_resource = ResourceId(format!("hyperliquid:mainnet:{OWNER}:spot:primary"));
            let deposit_input = json!({"route":"core","amount":"10"});
            let plan = exact_operation_plan(
                "Deposit $10 from the enrolled Hyperliquid Core USDC account",
                "deposit",
                "papertrade.deposit",
                deposit_input.clone(),
                vec![ResourceAmount {
                    resource: source_resource.clone(),
                    amount: Amount {
                        asset: source_asset.clone(),
                        atoms: Quantity::new(format!("{}{}", atoms, "0".repeat(30))).unwrap(),
                    },
                }],
                vec![FinancialPlanFundingIdentity {
                    resource: adapter.account().trading_cash_resource(),
                    asset: trading_usdc_asset(),
                }],
            );
            let plan_id = authorize_plan(&client, "papertrade-core-deposit", plan).await;
            let preview = client
                .call(BrokerRequest::PreviewPlanStep {
                    plan_id: plan_id.clone(),
                    step_id: "deposit".into(),
                    operation: "papertrade.deposit".into(),
                    input: deposit_input.clone(),
                })
                .await;
            assert!(preview.ok, "{:?}", preview.error);
            let preview_value = preview.result.unwrap();
            assert!(
                preview_value["summary"]
                    .as_str()
                    .is_some_and(|summary| summary.contains("exact 10 USDC Deposit"))
            );
            assert_eq!(
                preview_value["valuation"]["movement_usd_micros"],
                "10000000"
            );
            let executed = execute_plan_step(
                &client,
                &plan_id,
                "deposit",
                "papertrade.deposit",
                "papertrade-core-deposit-attempt",
                deposit_input.clone(),
            )
            .await;
            assert!(executed.ok, "{:?}", executed.error);
            assert_eq!(
                executed.result.as_ref().unwrap()["status"],
                "completed",
                "deposit did not settle: {:?}",
                executed.result
            );
            let effects = executed.result.as_ref().unwrap()["effects"]["effects"]
                .as_array()
                .unwrap();
            assert!(effects.iter().any(|effect| {
                effect["kind"] == "cash_debit"
                    && effect["resource"] == source_resource.0
                    && effect["amount"]["atoms"] == format!("{}{}", atoms, "0".repeat(30))
            }));
            assert!(effects.iter().any(|effect| {
                effect["kind"] == "settlement" && effect["delta"] == "10000000000000000000"
            }));
            assert_eq!(runtime.funding_dispatches.load(Ordering::SeqCst), 1);
            let recovered = execute_plan_step(
                &client,
                &plan_id,
                "deposit",
                "papertrade.deposit",
                "papertrade-core-deposit-attempt",
                deposit_input,
            )
            .await;
            assert!(recovered.ok, "{:?}", recovered.error);
            assert_eq!(runtime.funding_dispatches.load(Ordering::SeqCst), 1);
        });
}

#[test]
fn real_papertrade_adapter_named_plan_settles_open_and_full_close_without_replay() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let approval = Arc::new(Approval(AtomicUsize::new(0)));
            let (broker, adapter, runtime) = make_runtime_with_movement_cap(
                &dir.path().join("papertrade-trade.sqlite"),
                approval.clone(),
                "200000000",
            );
            broker.engine.recover_interrupted().unwrap();
            let client = SocketClient::new(broker.clone(), dir.path()).await;
            let cash_asset = trading_usdc_asset();
            let cash_resource = adapter.account().trading_cash_resource();
            let margin = Quantity::new("10000000000000000000").unwrap();
            let open_input = json!({"instrument_id":0,"is_long":true,"margin":"10","leverage":100});
            let open_plan = exact_operation_plan(
                "Open BTC with $10 isolated cash margin",
                "open",
                "papertrade.open",
                open_input.clone(),
                vec![ResourceAmount {
                    resource: cash_resource.clone(),
                    amount: Amount {
                        asset: cash_asset.clone(),
                        atoms: margin.clone(),
                    },
                }],
                vec![],
            );
            let open_plan_id = authorize_plan(&client, "papertrade-open-plan", open_plan).await;
            let opened = execute_plan_step(
                &client,
                &open_plan_id,
                "open",
                "papertrade.open",
                "papertrade-open-attempt",
                open_input.clone(),
            )
            .await;
            assert!(opened.ok, "{:?}", opened.error);
            assert_eq!(
                runtime.dispatches.load(Ordering::SeqCst),
                1,
                "open execute response: {:?}",
                opened.result
            );
            assert_eq!(
                opened
                    .result
                    .as_ref()
                    .and_then(|v| v.get("status"))
                    .and_then(Value::as_str),
                Some("completed"),
                "open did not settle: {:?}",
                opened.result
            );
            assert_eq!(runtime.positions.lock().unwrap().len(), 1);
            let open_effects = opened.result.as_ref().unwrap()["effects"]["effects"]
                .as_array()
                .unwrap();
            assert!(open_effects.iter().any(|effect| {
                effect["kind"] == "cash_debit"
                    && effect["amount"]["atoms"] == "10000000000000000000"
            }));
            assert!(
                open_effects.iter().any(|effect| {
                    effect["kind"] == "position_change"
                        && effect["anchor"]["position_kind"] == "margin_long"
                        && effect["delta"] == "10000000000000000000"
                }),
                "open effects: {open_effects:?}"
            );
            assert!(open_effects.iter().any(|effect| {
                effect["kind"] == "position_change"
                    && effect["anchor"]["position_kind"] == "derivative_long"
                    && effect["delta"] == "1000000000000000000000"
            }));
            let opened_status = client
                .call(BrokerRequest::PlanStatus {
                    plan_id: open_plan_id.clone(),
                })
                .await;
            assert!(opened_status.ok, "{:?}", opened_status.error);

            // A second same-market open must use its own ledger key; closing
            // the first exact position must leave the second exposure intact.
            let second_open_plan = exact_operation_plan(
                "Open a second isolated BTC position",
                "open",
                "papertrade.open",
                open_input.clone(),
                vec![ResourceAmount {
                    resource: adapter.account().trading_cash_resource(),
                    amount: Amount {
                        asset: trading_usdc_asset(),
                        atoms: margin.clone(),
                    },
                }],
                vec![],
            );
            let second_open_plan_id =
                authorize_plan(&client, "papertrade-open-second-plan", second_open_plan).await;
            let second_opened = execute_plan_step(
                &client,
                &second_open_plan_id,
                "open",
                "papertrade.open",
                "papertrade-open-second-attempt",
                open_input.clone(),
            )
            .await;
            assert!(second_opened.ok, "{:?}", second_opened.error);
            assert_eq!(
                runtime.positions.lock().unwrap().len(),
                2,
                "second open response {:?}, dispatches {}",
                second_opened.result,
                runtime.dispatches.load(Ordering::SeqCst)
            );
            assert_eq!(runtime.positions.lock().unwrap()[1]["positionId"], "43");
            runtime.positions.lock().unwrap().push(json!({
                "positionId":"44", "instrumentId":0, "isLong":false, "useDebt":false,
                "entryPriceRaw":"50000000", "marginRaw":"3000000000000000000", "leverage":5,
                "bustPriceRaw":"55000000", "openedAtMs":124, "intentId":null
            }));

            let close_input = json!({"position_ids":["42"]});
            let close_plan = exact_operation_plan(
                "Close the exact BTC position",
                "close",
                "papertrade.close",
                close_input.clone(),
                vec![],
                vec![
                    FinancialPlanFundingIdentity {
                        resource: cash_resource,
                        asset: cash_asset,
                    },
                    FinancialPlanFundingIdentity {
                        resource: ResourceId(format!("papertrade:mainnet:{OWNER}:paper_wallet")),
                        asset: paper_asset(),
                    },
                    FinancialPlanFundingIdentity {
                        resource: ResourceId(format!("papertrade:mainnet:{OWNER}:payout_queue")),
                        asset: trading_usdc_asset(),
                    },
                ],
            );
            let close_plan_id = authorize_plan(&client, "papertrade-close-plan", close_plan).await;
            let closed = execute_plan_step(
                &client,
                &close_plan_id,
                "close",
                "papertrade.close",
                "papertrade-close-attempt",
                close_input,
            )
            .await;
            assert!(closed.ok, "{:?}", closed.error);
            assert_eq!(
                runtime.dispatches.load(Ordering::SeqCst),
                3,
                "close execute response: {:?}",
                closed.result
            );
            assert_eq!(
                closed
                    .result
                    .as_ref()
                    .and_then(|v| v.get("status"))
                    .and_then(Value::as_str),
                Some("completed"),
                "close did not settle: {:?}",
                closed.result
            );
            let close_effects = closed.result.as_ref().unwrap()["effects"]["effects"]
                .as_array()
                .unwrap();
            assert!(close_effects.iter().any(|effect| {
                effect["kind"] == "settlement" && effect["delta"] == "10000000000000000000"
            }));
            assert!(close_effects.iter().any(|effect| {
                effect["kind"] == "position_change"
                    && effect["anchor"]["position_kind"] == "margin_long"
                    && effect["delta"] == "-10000000000000000000"
                    && effect["anchor"]["principal"]["atoms"] == "0"
            }));
            assert!(close_effects.iter().any(|effect| {
                effect["kind"] == "position_change"
                    && effect["anchor"]["position_kind"] == "derivative_long"
                    && effect["delta"] == "-1000000000000000000000"
                    && effect["anchor"]["principal"]["atoms"] == "0"
            }));
            assert_eq!(runtime.positions.lock().unwrap().len(), 2);
            assert!(
                runtime
                    .positions
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|row| row["positionId"] == "43")
            );
            let repeat = execute_plan_step(
                &client,
                &close_plan_id,
                "close",
                "papertrade.close",
                "papertrade-close-attempt",
                json!({"position_ids":["42"]}),
            )
            .await;
            assert!(repeat.ok, "{:?}", repeat.error);
            assert_eq!(
                runtime.dispatches.load(Ordering::SeqCst),
                3,
                "repeated completed status must not submit again"
            );
            let close_second_input = json!({"position_ids":["43"]});
            let close_second_plan = exact_operation_plan(
                "Close only the remaining BTC position",
                "close-second",
                "papertrade.close",
                close_second_input.clone(),
                vec![],
                vec![
                    FinancialPlanFundingIdentity {
                        resource: adapter.account().trading_cash_resource(),
                        asset: trading_usdc_asset(),
                    },
                    FinancialPlanFundingIdentity {
                        resource: ResourceId(format!("papertrade:mainnet:{OWNER}:paper_wallet")),
                        asset: paper_asset(),
                    },
                    FinancialPlanFundingIdentity {
                        resource: ResourceId(format!("papertrade:mainnet:{OWNER}:payout_queue")),
                        asset: trading_usdc_asset(),
                    },
                ],
            );
            let close_second_plan_id =
                authorize_plan(&client, "papertrade-close-second-plan", close_second_plan).await;
            let closed_second = execute_plan_step(
                &client,
                &close_second_plan_id,
                "close-second",
                "papertrade.close",
                "papertrade-close-second-attempt",
                close_second_input,
            )
            .await;
            assert!(closed_second.ok, "{:?}", closed_second.error);
            assert_eq!(runtime.positions.lock().unwrap().len(), 1);
            assert_eq!(runtime.positions.lock().unwrap()[0]["positionId"], "44");

            // Exercise the other reviewed instrument and a short exposure in
            // the same public CLI/engine path, then settle its exact position.
            let eth_short_input =
                json!({"instrument_id":1,"is_long":false,"margin":"10","leverage":5});
            let eth_short_plan = exact_operation_plan(
                "Open a cash-collateralized ETH short",
                "eth-short",
                "papertrade.open",
                eth_short_input.clone(),
                vec![ResourceAmount {
                    resource: adapter.account().trading_cash_resource(),
                    amount: Amount {
                        asset: trading_usdc_asset(),
                        atoms: margin.clone(),
                    },
                }],
                vec![],
            );
            let eth_short_plan_id =
                authorize_plan(&client, "papertrade-eth-short-plan", eth_short_plan).await;
            let eth_short = execute_plan_step(
                &client,
                &eth_short_plan_id,
                "eth-short",
                "papertrade.open",
                "papertrade-eth-short-attempt",
                eth_short_input,
            )
            .await;
            assert!(eth_short.ok, "{:?}", eth_short.error);
            assert_eq!(
                eth_short.result.as_ref().unwrap()["status"],
                "completed",
                "ETH short did not settle: {:?}",
                eth_short.result
            );
            let eth_open_effects = eth_short.result.as_ref().unwrap()["effects"]["effects"]
                .as_array()
                .unwrap();
            assert!(eth_open_effects.iter().any(|effect| {
                effect["kind"] == "position_change"
                    && effect["anchor"]["position_kind"] == "derivative_short"
                    && effect["delta"] == "50000000000000000000"
            }));
            assert_eq!(
                runtime.positions.lock().unwrap().last().unwrap()["instrumentId"],
                1
            );
            assert_eq!(
                runtime.positions.lock().unwrap().last().unwrap()["isLong"],
                false
            );

            let eth_close_input = json!({"position_ids":["46"]});
            let eth_close_plan = exact_operation_plan(
                "Close the exact ETH short position",
                "eth-short-close",
                "papertrade.close",
                eth_close_input.clone(),
                vec![],
                vec![
                    FinancialPlanFundingIdentity {
                        resource: adapter.account().trading_cash_resource(),
                        asset: trading_usdc_asset(),
                    },
                    FinancialPlanFundingIdentity {
                        resource: ResourceId(format!("papertrade:mainnet:{OWNER}:paper_wallet")),
                        asset: paper_asset(),
                    },
                    FinancialPlanFundingIdentity {
                        resource: ResourceId(format!("papertrade:mainnet:{OWNER}:payout_queue")),
                        asset: trading_usdc_asset(),
                    },
                ],
            );
            let eth_close_plan_id =
                authorize_plan(&client, "papertrade-eth-short-close-plan", eth_close_plan).await;
            let eth_closed = execute_plan_step(
                &client,
                &eth_close_plan_id,
                "eth-short-close",
                "papertrade.close",
                "papertrade-eth-short-close-attempt",
                eth_close_input,
            )
            .await;
            assert!(eth_closed.ok, "{:?}", eth_closed.error);
            assert_eq!(
                eth_closed.result.as_ref().unwrap()["status"],
                "completed",
                "ETH short close did not settle: {:?}",
                eth_closed.result
            );
            let eth_close_effects = eth_closed.result.as_ref().unwrap()["effects"]["effects"]
                .as_array()
                .unwrap();
            assert!(eth_close_effects.iter().any(|effect| {
                effect["kind"] == "position_change"
                    && effect["anchor"]["position_kind"] == "derivative_short"
                    && effect["delta"] == "-50000000000000000000"
                    && effect["anchor"]["principal"]["atoms"] == "0"
            }));
            assert_eq!(runtime.positions.lock().unwrap().len(), 1);
            assert_eq!(runtime.positions.lock().unwrap()[0]["positionId"], "44");

            let legacy_close_input = json!({"position_ids":["44"]});
            let legacy_close_plan = exact_operation_plan(
                "Close externally opened legacy position using observed native baseline",
                "close-legacy",
                "papertrade.close",
                legacy_close_input.clone(),
                vec![],
                vec![
                    FinancialPlanFundingIdentity {
                        resource: adapter.account().trading_cash_resource(),
                        asset: trading_usdc_asset(),
                    },
                    FinancialPlanFundingIdentity {
                        resource: ResourceId(format!("papertrade:mainnet:{OWNER}:paper_wallet")),
                        asset: paper_asset(),
                    },
                    FinancialPlanFundingIdentity {
                        resource: ResourceId(format!("papertrade:mainnet:{OWNER}:payout_queue")),
                        asset: trading_usdc_asset(),
                    },
                ],
            );
            let legacy_close_plan_id =
                authorize_plan(&client, "papertrade-close-legacy-plan", legacy_close_plan).await;
            let legacy_preview = adapter
                .prepare("papertrade.close", &legacy_close_input)
                .await
                .unwrap();
            let legacy_anchors = legacy_preview.binding[gofin_domain::BASELINE_ANCHORS_KEY]
                .as_array()
                .expect("legacy close must bind observed baseline anchors");
            assert_eq!(legacy_anchors.len(), 2);
            assert!(
                legacy_anchors
                    .iter()
                    .all(|anchor| anchor["position"] == "papertrade:position_id:44")
            );
            let legacy_closed = execute_plan_step(
                &client,
                &legacy_close_plan_id,
                "close-legacy",
                "papertrade.close",
                "papertrade-close-legacy-attempt",
                legacy_close_input,
            )
            .await;
            assert!(legacy_closed.ok, "{:?}", legacy_closed.error);
            assert_eq!(runtime.positions.lock().unwrap().len(), 0);
            assert_eq!(runtime.dispatches.load(Ordering::SeqCst), 7);
            assert_eq!(approval.0.load(Ordering::SeqCst), 7);
            // Complete the supported market/direction matrix through the same
            // named-plan and canonical-settlement path. BTC long and ETH short
            // were exercised above; cover BTC short and ETH long here.
            for (label, instrument_id, is_long, leverage, position_kind) in [
                ("btc-short", 0, false, 100, "derivative_short"),
                ("eth-long", 1, true, 5, "derivative_long"),
            ] {
                let open_input = json!({
                    "instrument_id": instrument_id,
                    "is_long": is_long,
                    "margin": "10",
                    "leverage": leverage
                });
                let open_step = format!("{label}-open");
                let open_plan = exact_operation_plan(
                    &format!("Open a cash-collateralized {label} position"),
                    &open_step,
                    "papertrade.open",
                    open_input.clone(),
                    vec![ResourceAmount {
                        resource: adapter.account().trading_cash_resource(),
                        amount: Amount {
                            asset: trading_usdc_asset(),
                            atoms: margin.clone(),
                        },
                    }],
                    vec![],
                );
                let open_plan_id =
                    authorize_plan(&client, &format!("papertrade-{label}-open-plan"), open_plan)
                        .await;
                let opened = execute_plan_step(
                    &client,
                    &open_plan_id,
                    &open_step,
                    "papertrade.open",
                    &format!("papertrade-{label}-open-attempt"),
                    open_input,
                )
                .await;
                assert!(opened.ok, "{label} open: {:?}", opened.error);
                assert_eq!(opened.result.as_ref().unwrap()["status"], "completed");
                let position = runtime.positions.lock().unwrap().last().unwrap().clone();
                assert_eq!(position["instrumentId"], instrument_id);
                assert_eq!(position["isLong"], is_long);
                assert_eq!(position["useDebt"], false);
                let open_effects = opened.result.as_ref().unwrap()["effects"]["effects"]
                    .as_array()
                    .unwrap();
                assert!(
                    open_effects.iter().any(|effect| {
                        effect["kind"] == "position_change"
                            && effect["anchor"]["position_kind"] == position_kind
                            && effect["delta"]
                                == if is_long {
                                    "50000000000000000000"
                                } else {
                                    "1000000000000000000000"
                                }
                    }),
                    "{label} open effects: {open_effects:?}"
                );

                let position_id = position["positionId"].as_str().unwrap().to_owned();
                let close_input = json!({"position_ids":[position_id]});
                let close_step = format!("{label}-close");
                let close_plan = exact_operation_plan(
                    &format!("Close the exact {label} position"),
                    &close_step,
                    "papertrade.close",
                    close_input.clone(),
                    vec![],
                    vec![
                        FinancialPlanFundingIdentity {
                            resource: adapter.account().trading_cash_resource(),
                            asset: trading_usdc_asset(),
                        },
                        FinancialPlanFundingIdentity {
                            resource: ResourceId(format!(
                                "papertrade:mainnet:{OWNER}:paper_wallet"
                            )),
                            asset: paper_asset(),
                        },
                        FinancialPlanFundingIdentity {
                            resource: ResourceId(format!(
                                "papertrade:mainnet:{OWNER}:payout_queue"
                            )),
                            asset: trading_usdc_asset(),
                        },
                    ],
                );
                let close_plan_id = authorize_plan(
                    &client,
                    &format!("papertrade-{label}-close-plan"),
                    close_plan,
                )
                .await;
                let closed = execute_plan_step(
                    &client,
                    &close_plan_id,
                    &close_step,
                    "papertrade.close",
                    &format!("papertrade-{label}-close-attempt"),
                    close_input,
                )
                .await;
                assert!(closed.ok, "{label} close: {:?}", closed.error);
                assert_eq!(
                    closed.result.as_ref().unwrap()["status"],
                    "completed",
                    "{label} close response: {:?}",
                    closed.result
                );
                let close_effects = closed.result.as_ref().unwrap()["effects"]["effects"]
                    .as_array()
                    .unwrap();
                assert!(
                    close_effects.iter().any(|effect| {
                        effect["kind"] == "position_change"
                            && effect["anchor"]["position_kind"] == position_kind
                            && effect["anchor"]["principal"]["atoms"] == "0"
                            && effect["delta"]
                                .as_str()
                                .is_some_and(|delta| delta.starts_with('-'))
                    }),
                    "{label} close effects: {close_effects:?}"
                );
                assert!(
                    runtime
                        .positions
                        .lock()
                        .unwrap()
                        .iter()
                        .all(|row| row["positionId"] != position["positionId"])
                );
            }
            let status = client
                .call(BrokerRequest::PlanStatus {
                    plan_id: close_plan_id,
                })
                .await;
            assert!(status.ok, "{:?}", status.error);
        });
}

#[test]
fn papertrade_cancel_requires_terminal_target_state_and_rejects_execution_race() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let runtime_path = dir.path().join("papertrade-cancel.sqlite");
            let approval = Arc::new(Approval(AtomicUsize::new(0)));
            let (broker, adapter, runtime) =
                make_runtime_with_movement_cap(&runtime_path, approval, "100000000");
            broker.engine.recover_interrupted().unwrap();
            let client = SocketClient::new(broker.clone(), dir.path()).await;

            let cancel_step = |step: &str, intent_id: &str| {
                exact_operation_plan(
                    "Cancel one exact pending Papertrade intent",
                    step,
                    "papertrade.cancel",
                    json!({"intent_id":intent_id}),
                    vec![],
                    vec![],
                )
            };
            let cancelled_target = format!("0x{}", "11".repeat(32));
            let cancelled_plan = authorize_plan(
                &client,
                "papertrade-cancel-confirmed-plan",
                cancel_step("cancel-confirmed", &cancelled_target),
            )
            .await;
            let accepted_only = execute_plan_step(
                &client,
                &cancelled_plan,
                "cancel-confirmed",
                "papertrade.cancel",
                "papertrade-cancel-confirmed-attempt",
                json!({"intent_id":cancelled_target}),
            )
            .await;
            assert!(accepted_only.ok, "{:?}", accepted_only.error);
            let accepted_record = accepted_only.result.as_ref().unwrap();
            assert_eq!(accepted_record["status"], "pending");
            assert!(
                accepted_record["effects"].is_null(),
                "POST acceptance alone must not settle cancellation: {accepted_record:?}"
            );
            let operation_id = accepted_record["id"].as_str().unwrap().to_owned();
            assert!(
                !broker.engine.status_rpc_read_scope(&operation_id).unwrap(),
                "cancel status uses the authenticated Papertrade API and should retain the normal wait scope"
            );
            *runtime.cancel_observation.lock().unwrap() = Some(CancelStatusProof {
                owner: OWNER.into(),
                target_intent_id: cancelled_target,
                action: "openPosition".into(),
                state: CancelIntentState::Cancelled,
                source: CancelStatusSource::CancelEndpoint,
                account_revision: None,
                indexed_through_block: None,
            });
            let settled = client
                .call(BrokerRequest::Status { id: operation_id })
                .await;
            assert!(settled.ok, "{:?}", settled.error);
            assert_eq!(settled.result.as_ref().unwrap()["status"], "completed");
            assert_eq!(
                settled.result.as_ref().unwrap()["effects"]["effects"],
                json!([])
            );
            assert_eq!(runtime.dispatches.load(Ordering::SeqCst), 1);

            // A competing execution can win the race after cancellation was
            // accepted. The target's exact terminal state is surfaced as a
            // rejection, never mistaken for a successful cancellation.
            let raced_target = format!("0x{}", "22".repeat(32));
            let raced_plan = authorize_plan(
                &client,
                "papertrade-cancel-race-plan",
                cancel_step("cancel-race", &raced_target),
            )
            .await;
            *runtime.cancel_observation.lock().unwrap() = Some(CancelStatusProof {
                owner: OWNER.into(),
                target_intent_id: raced_target.clone(),
                action: "openPosition".into(),
                state: CancelIntentState::Succeeded,
                source: CancelStatusSource::CancelEndpoint,
                account_revision: None,
                indexed_through_block: None,
            });
            let raced = execute_plan_step(
                &client,
                &raced_plan,
                "cancel-race",
                "papertrade.cancel",
                "papertrade-cancel-race-attempt",
                json!({"intent_id":raced_target}),
            )
            .await;
            assert!(raced.ok, "{:?}", raced.error);
            assert_eq!(
                raced.result.as_ref().unwrap()["status"],
                "failed",
                "raced cancellation response: {:?}",
                raced.result
            );
            assert_eq!(
                raced.result.as_ref().unwrap()["effects"]["effects"],
                json!([])
            );
            assert_eq!(runtime.dispatches.load(Ordering::SeqCst), 2);
            assert_eq!(adapter.account().owner, OWNER);
        });
}

#[test]
fn withdrawal_expiry_proof_precedes_funding_scan_for_every_route_and_must_be_valid() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for route in [
                FundingRoute::Core,
                FundingRoute::Hyperevm,
                FundingRoute::Ethereum,
                FundingRoute::Arbitrum,
                FundingRoute::Solana,
            ] {
                let dir = tempfile::tempdir().unwrap();
                let runtime_path = dir.path().join("papertrade-withdraw-expiry.sqlite");
                let approval = Arc::new(Approval(AtomicUsize::new(0)));
                let (_broker, adapter, runtime) =
                    make_runtime_with_movement_cap(&runtime_path, approval, "100000000");
                let (binding, mut proof) =
                    core_withdrawal_expiry_fixture(OWNER, route.clone(), true);
                *runtime.core_withdrawal_expiry.lock().unwrap() = Some(proof.clone());
                let record = funding_withdrawal_record(&binding);
                assert!(adapter.uses_rpc_reads("papertrade.withdraw", &record.input));
                assert!(!adapter.uses_rpc_reads("papertrade.account", &json!({"owner":OWNER})));
                assert!(!adapter.uses_rpc_reads("papertrade.trading_state", &json!({})));
                assert!(!adapter.uses_rpc_reads("papertrade.positions", &json!({"owner":OWNER})));
                assert!(!adapter.uses_rpc_reads("papertrade.history", &json!({"owner":OWNER})));
                assert!(
                    !adapter
                        .uses_rpc_reads("papertrade.cancel", &json!({"intent_id":proof.intent_id}))
                );

                let expired = adapter.reconcile(&record).await.unwrap();
                assert_eq!(expired.status, Status::Failed, "route: {route:?}");
                let verified = expired
                    .verified
                    .expect("proof binds no-effect terminal failure");
                assert!(verified.effects.is_empty());
                assert_eq!(verified.submission, record.submission.clone().unwrap());
                assert_eq!(runtime.funding_reconcile_calls.load(Ordering::SeqCst), 0);

                // A proof with a mismatched nonce cannot terminalize the
                // route; the adapter falls through to normal reconciliation.
                proof.nonce_unused = false;
                *runtime.core_withdrawal_expiry.lock().unwrap() = Some(proof);
                let unproven = adapter.reconcile(&record).await.unwrap();
                assert_ne!(unproven.status, Status::Failed, "route: {route:?}");
                assert_eq!(runtime.funding_reconcile_calls.load(Ordering::SeqCst), 1);
            }
        });
}
