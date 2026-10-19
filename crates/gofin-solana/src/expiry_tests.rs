use std::sync::{
    Mutex,
    atomic::{AtomicU64, AtomicUsize},
};

use super::*;
use crate::transaction::tests::{signed_wire, signer, signer_from_seed, v2_submission};

const FIRST_SLOT: u64 = 1000;
const LAST_SLOT: u64 = 1151;

struct HistoryRpc {
    blocks: Mutex<BTreeMap<u64, Value>>,
    requests: Mutex<Vec<(String, Value)>>,
    unavailable_before: AtomicU64,
    delay: std::time::Duration,
    head_slot: AtomicU64,
    head_height: AtomicU64,
    slots_override: Mutex<Option<Value>>,
    header_overrides: Mutex<BTreeMap<u64, Value>>,
}

fn block_hash(slot: u64) -> String {
    if slot == FIRST_SLOT {
        Hash::default().to_string()
    } else {
        Hash::new_from_array(Sha256::digest(slot.to_le_bytes()).into()).to_string()
    }
}

impl HistoryRpc {
    fn new() -> Self {
        Self {
            blocks: Mutex::new((FIRST_SLOT..=LAST_SLOT).map(|slot| (slot, json!({
                "blockhash":block_hash(slot), "previousBlockhash":block_hash(slot-1),
                "parentSlot":slot-1,"blockHeight":slot-900,"signatures":[]
            }))).collect()),
            requests: Default::default(),
            unavailable_before: AtomicU64::new(0),
            delay: std::time::Duration::ZERO,
            head_slot: AtomicU64::new(LAST_SLOT),
            head_height: AtomicU64::new(251),
            slots_override: Mutex::new(None),
            header_overrides: Default::default(),
        }
    }

    fn signature_reads(&self) -> Vec<u64> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, params)| {
                method == "getBlock" && params[1]["transactionDetails"] == "signatures"
            })
            .map(|(_, params)| params[0].as_u64().unwrap())
            .collect()
    }
}

#[async_trait]
impl SolanaRpc for HistoryRpc {
    async fn request_checked(
        &self,
        method: &str,
        params: Value,
        requirements: RpcReadRequirements,
    ) -> Result<Value> {
        // This in-memory fixture supplies synthetic corroborated observations.
        // The real two-origin requirement is covered by loopback pool tests.
        if method == "getBlock" && params[1]["transactionDetails"] == "signatures" {
            assert!(requirements.corroborate_finalized_block);
        } else {
            assert!(!requirements.corroborate_finalized_block);
        }
        self.request(method, params).await
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.requests
            .lock()
            .unwrap()
            .push((method.to_owned(), params.clone()));
        Ok(match method {
            "getSignatureStatuses" => {
                json!({"context":{"slot":self.head_slot.load(Ordering::Acquire)},"value":[null]})
            }
            "getTransaction" => Value::Null,
            "getSlot" => json!(self.head_slot.load(Ordering::Acquire)),
            "getBlockHeight" => json!(self.head_height.load(Ordering::Acquire)),
            "isBlockhashValid" => {
                json!({"context":{"slot":self.head_slot.load(Ordering::Acquire)},"value":false})
            }
            "getBlocks" => self
                .slots_override
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| {
                    json!(
                        self.blocks
                            .lock()
                            .unwrap()
                            .range(params[0].as_u64().unwrap()..=params[1].as_u64().unwrap())
                            .map(|(slot, _)| *slot)
                            .collect::<Vec<_>>()
                    )
                }),
            "getBlock" => {
                assert_eq!(params[1]["commitment"], "finalized");
                let slot = params[0].as_u64().unwrap();
                if params[1]["transactionDetails"] == "none"
                    && let Some(header) = self.header_overrides.lock().unwrap().get(&slot)
                {
                    return Ok(header.clone());
                }
                if params[1]["transactionDetails"] == "signatures" {
                    tokio::time::sleep(self.delay).await;
                    if slot < self.unavailable_before.load(Ordering::Acquire) {
                        return Err(failure(
                            "SOLANA_RPC_HISTORY_UNAVAILABLE",
                            "rpc",
                            "Synthetic missing block.",
                            "Keep pending.",
                        ));
                    }
                }
                self.blocks
                    .lock()
                    .unwrap()
                    .get(&slot)
                    .cloned()
                    .unwrap_or(Value::Null)
            }
            other => panic!(
                "Unexpected RPC {other}; reconciliation must never send or refresh a blockhash"
            ),
        })
    }
}

fn old_history(distance: u64, spacing: u64) -> HistoryRpc {
    let rpc = HistoryRpc::new();
    let end_slot = FIRST_SLOT + 151 * spacing;
    let head_slot = end_slot + distance + 20;
    let mut blocks = rpc.blocks.lock().unwrap();
    blocks.clear();
    for index in 0..=171 {
        let slot = FIRST_SLOT + index * spacing;
        let parent = if index == 0 {
            FIRST_SLOT - 1
        } else {
            slot - spacing
        };
        blocks.insert(
            slot,
            json!({"blockhash":block_hash(slot),
            "previousBlockhash":block_hash(parent),"parentSlot":parent,
            "blockHeight":100+index,"signatures":[]}),
        );
    }
    // Only the required lifetime and locator neighborhoods need complete
    // fixture bodies. The distant finalized head is a search-bound header.
    blocks.insert(
        head_slot,
        json!({"blockhash":block_hash(head_slot),
        "previousBlockhash":block_hash(head_slot-1),"parentSlot":head_slot-1,
        "blockHeight":251+distance,"signatures":[]}),
    );
    drop(blocks);
    rpc.head_slot.store(head_slot, Ordering::Release);
    rpc.head_height.store(251 + distance, Ordering::Release);
    rpc
}

#[tokio::test]
async fn old_submissions_locate_exact_expiry_height_without_scanning_their_age() {
    for (distance, spacing) in [(96_036, 1), (10_000_000, 3)] {
        let rpc = Arc::new(old_history(distance, spacing));
        let runtime = SolanaRuntime::new(rpc.clone());
        let original = submission();
        let ReceiptOutcome::ExpiredWithoutInclusion { proof, .. } =
            runtime.reconcile_submission(&original, None).await.unwrap()
        else {
            panic!("complete old lifetime must reconcile");
        };
        assert_eq!(proof.last_slot, FIRST_SLOT + 151 * spacing);
        assert_eq!(proof.first_slot, FIRST_SLOT);
        assert_eq!(proof.last_block_height, 251);
        assert_eq!(proof.checked_blocks, 152);
        assert_eq!(rpc.signature_reads().len(), 152);
        let requests = rpc.requests.lock().unwrap();
        let windows: Vec<_> = requests
            .iter()
            .filter(|(method, _)| method == "getBlocks")
            .map(|(_, params)| (params[0].as_u64().unwrap(), params[1].as_u64().unwrap()))
            .collect();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].1, FIRST_SLOT + 151 * spacing + 20);
        assert!(windows[0].1 < rpc.head_slot.load(Ordering::Acquire) - SEARCH_SLOT_WINDOW);
        assert!(
            windows
                .iter()
                .all(|(start, end)| end - start <= SEARCH_SLOT_WINDOW)
        );
        assert_eq!(original, submission());
    }
}

#[tokio::test]
async fn old_locator_hints_and_missing_history_cannot_replace_exact_quorum_coverage() {
    for fault in [
        "empty",
        "out_of_range",
        "duplicate",
        "wrong_header",
        "missing_target",
        "pruned_origin",
        "missing_lifetime_block",
    ] {
        let rpc = Arc::new(old_history(96_036, 1));
        match fault {
            "empty" => *rpc.slots_override.lock().unwrap() = Some(json!([])),
            "out_of_range" => *rpc.slots_override.lock().unwrap() = Some(json!([1172])),
            "duplicate" => *rpc.slots_override.lock().unwrap() = Some(json!([1171, 1171])),
            "wrong_header" => {
                let mut header = rpc.blocks.lock().unwrap()[&1171].clone();
                header["blockHeight"] = json!(251);
                rpc.header_overrides.lock().unwrap().insert(1171, header);
            }
            "missing_target" => {
                rpc.blocks.lock().unwrap().remove(&LAST_SLOT);
            }
            "pruned_origin" => {
                rpc.blocks.lock().unwrap().retain(|slot, _| *slot > 1050);
            }
            "missing_lifetime_block" => {
                rpc.blocks.lock().unwrap().remove(&1050);
            }
            _ => unreachable!(),
        }
        let runtime = SolanaRuntime::new(rpc.clone());
        let ReceiptOutcome::Pending { reason, .. } = runtime
            .reconcile_submission(&submission(), None)
            .await
            .unwrap()
        else {
            panic!("{fault} cannot prove absence");
        };
        assert!(
            reason.contains("historical")
                || reason.contains("Historical")
                || reason.contains("Corroborated"),
            "{fault}: {reason}"
        );
        assert!(
            !reason.contains(
                submission().data["signed_transaction_base64"]
                    .as_str()
                    .unwrap()
            )
        );
        assert!(rpc.signature_reads().len() <= MAX_COVERED_BLOCKS);
    }
}

#[tokio::test]
async fn disproven_locator_hint_is_discarded_before_retry() {
    let rpc = Arc::new(old_history(96_036, 1));
    let runtime = SolanaRuntime::new(rpc);
    let original = submission();
    let key = (
        original.data["chain_signature"]
            .as_str()
            .unwrap()
            .to_owned(),
        251,
    );
    runtime
        .expiry_cache
        .lock()
        .unwrap()
        .locator_upper
        .insert(key.clone(), 1100);
    assert!(matches!(
        runtime.reconcile_submission(&original, None).await.unwrap(),
        ReceiptOutcome::Pending { .. }
    ));
    assert!(
        !runtime
            .expiry_cache
            .lock()
            .unwrap()
            .locator_upper
            .contains_key(&key)
    );
    assert!(matches!(
        runtime.reconcile_submission(&original, None).await.unwrap(),
        ReceiptOutcome::ExpiredWithoutInclusion { .. }
    ));
}

/// Each locator header deliberately advances only one slot. These hints need
/// not describe a real chain: they must never manufacture a terminal proof.
struct SlowLocatorRpc {
    rpc: HistoryRpc,
    delay: std::time::Duration,
    windows: Mutex<Vec<u64>>,
}

#[async_trait]
impl SolanaRpc for SlowLocatorRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "getBlocks" {
            let upper = params[1].as_u64().unwrap();
            self.windows.lock().unwrap().push(upper);
            return Ok(json!([upper]));
        }
        self.rpc.request(method, params).await
    }
    async fn request_checked(
        &self,
        method: &str,
        params: Value,
        requirements: RpcReadRequirements,
    ) -> Result<Value> {
        if method == "getBlock"
            && params[0].as_u64() != Some(self.rpc.head_slot.load(Ordering::Acquire))
        {
            assert_eq!(params[1]["transactionDetails"], "none");
            assert!(!requirements.corroborate_finalized_block);
            tokio::time::sleep(self.delay).await;
            let slot = params[0].as_u64().unwrap();
            return Ok(
                json!({"blockhash":block_hash(slot),"previousBlockhash":block_hash(slot-1),"parentSlot":slot-1,"blockHeight":252}),
            );
        }
        self.rpc.request_checked(method, params, requirements).await
    }
}

#[tokio::test(start_paused = true)]
async fn historical_locator_has_bounded_probes_and_resumes_disposable_progress() {
    for delay in [std::time::Duration::ZERO, std::time::Duration::from_secs(6)] {
        let rpc = Arc::new(SlowLocatorRpc {
            rpc: old_history(96_036, 1),
            delay,
            windows: Default::default(),
        });
        let runtime = SolanaRuntime::new(rpc.clone());
        let original = submission();
        let started = tokio::time::Instant::now();
        let ReceiptOutcome::Pending { reason, .. } =
            runtime.reconcile_submission(&original, None).await.unwrap()
        else {
            panic!("header hints cannot settle");
        };
        assert!(reason.contains("historical") && reason.contains("locator"));
        assert!(started.elapsed() <= RECONCILIATION_BUDGET);
        let count = rpc.windows.lock().unwrap().len();
        assert!(count <= MAX_LOCATOR_PROBES);
        if delay.is_zero() {
            assert_eq!(count, MAX_LOCATOR_PROBES);
        }
        assert!(runtime.reconcile_submission(&original, None).await.is_ok());
        {
            let windows = rpc.windows.lock().unwrap();
            assert!(
                windows[count] < windows[0],
                "next status must resume narrowed search"
            );
            assert!(windows.len() <= 2 * MAX_LOCATOR_PROBES);
            assert!(rpc.rpc.signature_reads().is_empty());
        }
        // Restart loses hints safely and starts from the same bounded seed.
        let restarted = SolanaRuntime::new(rpc.clone());
        let count = rpc.windows.lock().unwrap().len();
        assert!(matches!(
            restarted
                .reconcile_submission(&original, None)
                .await
                .unwrap(),
            ReceiptOutcome::Pending { .. }
        ));
        assert_eq!(rpc.windows.lock().unwrap()[count], 1171);
    }
}

fn submission() -> Submission {
    let signer = signer();
    let (tx, wire) = signed_wire(&signer);
    let mut submission = v2_submission(&signer, &tx, &wire);
    submission.data["last_valid_block_height"] = json!(250);
    submission
}

#[tokio::test]
async fn complete_finalized_coverage_proves_original_expired_without_inclusion() {
    let rpc = Arc::new(HistoryRpc::new());
    let runtime = SolanaRuntime::new(rpc.clone());
    let submission = submission();
    let original = submission.clone();
    let outcome = runtime
        .reconcile_submission(&submission, None)
        .await
        .unwrap();
    let ReceiptOutcome::ExpiredWithoutInclusion {
        signature,
        message_sha256,
        proof,
        ..
    } = &outcome
    else {
        panic!("expected complete absence proof, got {outcome:?}");
    };
    assert_eq!(
        signature,
        submission.data["chain_signature"].as_str().unwrap()
    );
    assert_eq!(
        message_sha256,
        submission.data["message_sha256"].as_str().unwrap()
    );
    assert_eq!(proof.first_slot, FIRST_SLOT);
    assert_eq!(proof.last_slot, LAST_SLOT);
    assert_eq!(proof.first_block_height, 100);
    assert_eq!(proof.last_block_height, 251);
    assert_eq!(proof.checked_blocks, 152);
    assert_eq!(proof.block_coverage_sha256.len(), 64);
    assert_eq!(rpc.signature_reads().len(), 152);
    let public = serde_json::to_string(&outcome).unwrap();
    assert!(!public.contains("no_network_send"));
    assert!(
        !public.contains(
            submission.data["signed_transaction_base64"]
                .as_str()
                .unwrap()
        )
    );
    assert_eq!(submission, original);
}

#[tokio::test]
async fn incomplete_conflicting_or_positive_history_never_proves_absence() {
    for fault in [
        "missing_slot",
        "null_block",
        "parent",
        "previous_hash",
        "height",
        "origin",
        "short_bound",
        "target_present",
        "missing_signatures",
        "malformed_signature",
    ] {
        let rpc = Arc::new(HistoryRpc::new());
        let mut submission = submission();
        {
            let mut blocks = rpc.blocks.lock().unwrap();
            match fault {
                "missing_slot" => {
                    blocks.remove(&1050);
                }
                "null_block" => {
                    blocks.insert(1050, Value::Null);
                }
                "parent" => blocks.get_mut(&1051).unwrap()["parentSlot"] = json!(1049),
                "previous_hash" => {
                    blocks.get_mut(&1051).unwrap()["previousBlockhash"] = json!(block_hash(1049))
                }
                "height" => blocks.get_mut(&1050).unwrap()["blockHeight"] = json!(149),
                "origin" => {
                    blocks.get_mut(&FIRST_SLOT).unwrap()["blockhash"] = json!(block_hash(999))
                }
                "short_bound" => submission.data["last_valid_block_height"] = json!(249),
                "target_present" => {
                    blocks.get_mut(&1050).unwrap()["signatures"] =
                        json!([submission.data["chain_signature"]])
                }
                "missing_signatures" => {
                    blocks
                        .get_mut(&1050)
                        .unwrap()
                        .as_object_mut()
                        .unwrap()
                        .remove("signatures");
                }
                "malformed_signature" => {
                    blocks.get_mut(&1050).unwrap()["signatures"] = json!(["truncated"])
                }
                _ => unreachable!(),
            }
        }
        let runtime = SolanaRuntime::new(rpc);
        assert!(
            matches!(
                runtime
                    .reconcile_submission(&submission, None)
                    .await
                    .unwrap(),
                ReceiptOutcome::Pending { .. }
            ),
            "{fault}"
        );
    }
}

#[tokio::test]
async fn partial_finalized_coverage_is_cached_but_never_settles_before_complete() {
    let rpc = Arc::new(HistoryRpc::new());
    rpc.unavailable_before.store(1100, Ordering::Release);
    let runtime = SolanaRuntime::new(rpc.clone());
    let submission = submission();
    assert!(matches!(
        runtime
            .reconcile_submission(&submission, None)
            .await
            .unwrap(),
        ReceiptOutcome::Pending { .. }
    ));
    let first_reads = rpc.signature_reads();
    assert!(first_reads.contains(&LAST_SLOT));
    rpc.unavailable_before.store(0, Ordering::Release);
    assert!(matches!(
        runtime
            .reconcile_submission(&submission, None)
            .await
            .unwrap(),
        ReceiptOutcome::ExpiredWithoutInclusion { .. }
    ));
    assert_eq!(
        rpc.signature_reads()
            .iter()
            .filter(|slot| **slot == LAST_SLOT)
            .count(),
        1
    );
    // Restart discards the optimization: missing history cannot inherit a
    // terminal conclusion from incomplete or process-local facts.
    rpc.unavailable_before.store(1100, Ordering::Release);
    let restarted = SolanaRuntime::new(rpc.clone());
    assert!(matches!(
        restarted
            .reconcile_submission(&submission, None)
            .await
            .unwrap(),
        ReceiptOutcome::Pending { .. }
    ));
    assert_eq!(
        rpc.signature_reads()
            .iter()
            .filter(|slot| **slot == LAST_SLOT)
            .count(),
        2
    );
}

#[tokio::test(start_paused = true)]
async fn coverage_work_retains_twenty_second_budget_and_progress_across_status_calls() {
    let mut fixture = HistoryRpc::new();
    fixture.delay = std::time::Duration::from_secs(2);
    let rpc = Arc::new(fixture);
    let runtime = SolanaRuntime::new(rpc.clone());
    let submission = submission();
    let started = tokio::time::Instant::now();
    let ReceiptOutcome::Pending { reason, .. } = runtime
        .reconcile_submission(&submission, None)
        .await
        .unwrap()
    else {
        panic!("coverage must remain pending at its budget");
    };
    assert!(reason.contains("finalized blocks checked and cached"));
    assert_eq!(started.elapsed(), RECONCILIATION_BUDGET);
    assert!(rpc.signature_reads().len() < 152);
    let mut finished = false;
    for _ in 0..5 {
        if matches!(
            runtime
                .reconcile_submission(&submission, None)
                .await
                .unwrap(),
            ReceiptOutcome::ExpiredWithoutInclusion { .. }
        ) {
            finished = true;
            break;
        }
    }
    assert!(finished);
}

#[tokio::test]
async fn legacy_partial_and_durable_nonce_submissions_cannot_use_recent_hash_expiry() {
    let rpc = Arc::new(HistoryRpc::new());
    let runtime = SolanaRuntime::new(rpc.clone());
    let mut legacy = submission();
    legacy.data["schema_version"] = json!(1);
    assert!(matches!(
        runtime.reconcile_submission(&legacy, None).await.unwrap(),
        ReceiptOutcome::Pending { .. }
    ));
    assert!(rpc.signature_reads().is_empty());

    let signer = signer();
    let payer = signer.address().parse::<Address>().unwrap();
    let instruction = Instruction {
        program_id: "11111111111111111111111111111111".parse().unwrap(),
        accounts: vec![AccountMeta::new(payer, true)],
        data: 4u32.to_le_bytes().to_vec(),
    };
    let message = VersionedMessage::Legacy(solana_message::Message::new_with_blockhash(
        &[instruction],
        Some(&payer),
        &Hash::default(),
    ));
    let signature = Signature::from(
        signer
            .sign_message(&codec().serialize(&message).unwrap())
            .unwrap(),
    );
    let tx = VersionedTransaction {
        signatures: vec![signature],
        message,
    };
    assert!(tx.uses_durable_nonce());
    let wire = STANDARD.encode(codec().serialize(&tx).unwrap());
    let mut nonce = v2_submission(&signer, &tx, &wire);
    nonce.data["last_valid_block_height"] = json!(250);
    assert!(matches!(
        runtime.reconcile_submission(&nonce, None).await.unwrap(),
        ReceiptOutcome::Pending { .. }
    ));
    assert!(rpc.signature_reads().is_empty());

    let (mut partial, _) = signed_wire(&signer);
    partial.signatures[0] = Signature::default();
    let data = json!({"schema_version":2,"last_valid_block_height":250,"signed_transaction_base64":STANDARD.encode(codec().serialize(&partial).unwrap())});
    assert!(
        runtime
            .prove_expired_without_inclusion(&data, "unknown")
            .await
            .unwrap()
            .is_none()
    );
}

struct RacingCoverageRpc {
    requests: AtomicUsize,
    started: tokio::sync::Barrier,
    first_cached: tokio::sync::Notify,
    rpc_conflict: bool,
}

#[async_trait]
impl SolanaRpc for RacingCoverageRpc {
    async fn request(&self, method: &str, _params: Value) -> Result<Value> {
        panic!("Unexpected unverified read or financial dispatch: {method}")
    }

    async fn request_checked(
        &self,
        method: &str,
        _params: Value,
        requirements: RpcReadRequirements,
    ) -> Result<Value> {
        assert_eq!(method, "getBlock");
        assert!(requirements.corroborate_finalized_block);
        let index = self.requests.fetch_add(1, Ordering::AcqRel);
        self.started.wait().await;
        let mut block = HistoryRpc::new().blocks.lock().unwrap()[&1050].clone();
        if index == 1 {
            self.first_cached.notified().await;
            if self.rpc_conflict {
                return Err(coverage_conflict());
            }
            block["signatures"] = json!([Signature::from([8; 64]).to_string()]);
        }
        Ok(block)
    }
}

#[tokio::test]
async fn observed_concurrent_coverage_conflicts_remain_sticky_on_retry() {
    for rpc_conflict in [false, true] {
        let rpc = Arc::new(RacingCoverageRpc {
            requests: AtomicUsize::new(0),
            started: tokio::sync::Barrier::new(2),
            first_cached: tokio::sync::Notify::new(),
            rpc_conflict,
        });
        let runtime = SolanaRuntime::new(rpc.clone());
        let submission = submission();
        let signature = submission.data["chain_signature"].as_str().unwrap();
        let first = async {
            let result = runtime.finalized_absence_block(signature, 1050).await;
            rpc.first_cached.notify_one();
            result
        };
        let second = runtime.finalized_absence_block(signature, 1050);
        let (first, second) = tokio::join!(first, second);
        assert!(first.unwrap().is_some());
        assert_eq!(second.unwrap_err().code, "SOLANA_RPC_CONFLICT");
        assert_eq!(
            runtime
                .finalized_absence_block(signature, 1050)
                .await
                .unwrap_err()
                .code,
            "SOLANA_RPC_CONFLICT"
        );
        assert_eq!(
            runtime
                .prove_expired_without_inclusion(&submission.data, signature)
                .await
                .unwrap_err()
                .code,
            "SOLANA_RPC_CONFLICT"
        );
        assert_eq!(rpc.requests.load(Ordering::Acquire), 2);
    }
}

#[tokio::test]
async fn cached_absence_for_one_signed_transaction_cannot_hide_another_signature() {
    let rpc = Arc::new(HistoryRpc::new());
    let runtime = SolanaRuntime::new(rpc.clone());
    assert!(matches!(
        runtime
            .reconcile_submission(&submission(), None)
            .await
            .unwrap(),
        ReceiptOutcome::ExpiredWithoutInclusion { .. }
    ));
    let signer = signer_from_seed([31; 32]);
    let (tx, wire) = signed_wire(&signer);
    let mut other = v2_submission(&signer, &tx, &wire);
    other.data["last_valid_block_height"] = json!(250);
    rpc.blocks.lock().unwrap().get_mut(&1050).unwrap()["signatures"] =
        json!([other.data["chain_signature"]]);
    assert!(matches!(
        runtime.reconcile_submission(&other, None).await.unwrap(),
        ReceiptOutcome::Pending { .. }
    ));
}

#[tokio::test]
async fn generic_single_transport_cannot_claim_corroborated_history() {
    struct Single(HistoryRpc);
    #[async_trait]
    impl SolanaRpc for Single {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            self.0.request(method, params).await
        }
    }
    let rpc = Arc::new(Single(HistoryRpc::new()));
    let runtime = SolanaRuntime::new(rpc.clone());
    assert!(matches!(
        runtime
            .reconcile_submission(&submission(), None)
            .await
            .unwrap(),
        ReceiptOutcome::Pending { .. }
    ));
    assert!(runtime.expiry_cache.lock().unwrap().blocks.is_empty());
    assert!(rpc.0.signature_reads().is_empty());
}

#[tokio::test]
async fn observed_inclusion_remains_pending_after_block_cache_eviction() {
    let rpc = Arc::new(HistoryRpc::new());
    let runtime = SolanaRuntime::new(rpc.clone());
    let submission = submission();
    rpc.blocks.lock().unwrap().get_mut(&1050).unwrap()["signatures"] =
        json!([submission.data["chain_signature"]]);
    assert!(matches!(
        runtime
            .reconcile_submission(&submission, None)
            .await
            .unwrap(),
        ReceiptOutcome::Pending { .. }
    ));
    let reads = rpc.signature_reads().len();
    {
        let mut cache = runtime.expiry_cache.lock().unwrap();
        cache.blocks.clear();
        cache.order.clear();
    }
    rpc.blocks.lock().unwrap().get_mut(&1050).unwrap()["signatures"] = json!([]);
    assert!(matches!(
        runtime
            .reconcile_submission(&submission, None)
            .await
            .unwrap(),
        ReceiptOutcome::Pending { .. }
    ));
    assert_eq!(rpc.signature_reads().len(), reads);
}
