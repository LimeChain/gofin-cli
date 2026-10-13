use async_trait::async_trait;
use fin_domain::{FinError, Quantity, Result, Submission, SubmissionJournal};
use fin_evm::{
    alloy::primitives::{Bytes, U256},
    *,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

// Unmistakably synthetic, exercised only against the in-memory RPC below.
const SYNTHETIC_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";
fn frozen() -> FrozenTransaction {
    let signer = LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY).unwrap();
    FrozenTransaction {
        rollup_fee: None,
        chain_id: 1,
        sender: lower_address(signer.address()),
        to: "0x0000000000000000000000000000000000000002".into(),
        value: Quantity::from_u64(7),
        calldata: "0x".into(),
        nonce: 3,
        gas: 30000,
        max_fee_per_gas: Quantity::from_u64(100_000_000),
        max_priority_fee_per_gas: Quantity::from_u64(10_000_000),
    }
}
#[tokio::test]
async fn rejects_every_changed_frozen_field_and_trailing_bytes() {
    let signer = LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY).unwrap();
    let tx = frozen();
    let raw = signer.sign_prepared(&tx).await.unwrap();
    verify_signed_envelope(&tx, &raw).unwrap();
    let mut changed = Vec::new();
    for i in 0..9 {
        let mut t = tx.clone();
        match i {
            0 => t.chain_id += 1,
            1 => t.nonce += 1,
            2 => t.gas += 1,
            3 => t.to = "0x0000000000000000000000000000000000000003".into(),
            4 => t.value = Quantity::from_u64(8),
            5 => t.calldata = "0x1234".into(),
            6 => t.max_fee_per_gas = Quantity::from_u64(101_000_000),
            7 => t.max_priority_fee_per_gas = Quantity::from_u64(11_000_000),
            _ => t.sender = "0x0000000000000000000000000000000000000004".into(),
        }
        changed.push(t);
    }
    for t in changed {
        assert_eq!(
            verify_signed_envelope(&t, &raw).unwrap_err().code,
            "SIGNED_TRANSACTION_MISMATCH"
        );
    }
    let mut extra = raw.to_vec();
    extra.push(0);
    assert_eq!(
        verify_signed_envelope(&tx, &extra).unwrap_err().code,
        "SIGNED_TRANSACTION_MISMATCH"
    );
}
struct Journal {
    saved: Mutex<Option<Submission>>,
    events: Arc<Mutex<Vec<String>>>,
    authorized: bool,
    fail_persist: bool,
}
impl SubmissionJournal for Journal {
    fn assert_authorized(&self) -> Result<()> {
        if self.authorized {
            Ok(())
        } else {
            Err(FinError::validation("Synthetic authorization denied."))
        }
    }
    fn persist_submission(&self, s: &Submission) -> Result<()> {
        self.events.lock().unwrap().push("persist".into());
        if self.fail_persist {
            return Err(FinError::validation("Synthetic durable write failed."));
        }
        *self.saved.lock().unwrap() = Some(s.clone());
        Ok(())
    }
}
struct Rpc {
    events: Arc<Mutex<Vec<String>>>,
    lost: bool,
    latest: u64,
    reorg: bool,
    wrong_chain: bool,
    nonce: u64,
    priority_fee: AtomicU64,
    base_fee: AtomicU64,
    gas_estimate: AtomicU64,
    balance: U256,
    simulation_failure: bool,
}

struct CountedRpc {
    inner: Rpc,
    calls: Mutex<Vec<String>>,
}
#[async_trait]
impl EvmRpc for CountedRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.calls.lock().unwrap().push(method.into());
        self.inner.request(method, params).await
    }
}
fn validated_transaction(tx: &FrozenTransaction) -> fin_domain::RevalidatedOperation {
    fin_domain::RevalidatedOperation::after_revalidation(
        fin_domain::PreparedOperation {
            operation: "synthetic.transfer".into(),
            input: json!({}),
            binding: json!({"transaction":tx}),
            summary: "Isolated synthetic transfer".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: fin_domain::now(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            valuation: None,
            intent: fin_domain::FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        },
        Arc::new(fin_domain::clock::SystemClock),
    )
    .unwrap()
}

#[tokio::test]
async fn validated_submission_reuses_simulation_and_preserves_original_uncertain_identity() {
    let events = Arc::new(Mutex::new(vec![]));
    let mut fixture = rpc(events.clone());
    fixture.lost = true;
    let rpc = Arc::new(CountedRpc {
        inner: fixture,
        calls: Mutex::new(vec![]),
    });
    let runtime = EvmRuntime::new(
        rpc.clone(),
        1,
        Some(Arc::new(
            LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY).unwrap(),
        )),
    );
    let journal = Arc::new(Journal {
        saved: Mutex::new(None),
        events: events.clone(),
        authorized: true,
        fail_persist: false,
    });
    let tx = frozen();
    runtime.revalidate(&tx).await.unwrap();
    let validated = validated_transaction(&tx);
    assert_eq!(
        runtime
            .submit_revalidated(&tx, json!({}), journal.clone(), &validated)
            .await
            .unwrap_err()
            .code,
        "RPC_UNAVAILABLE"
    );
    let calls = rpc.calls.lock().unwrap();
    assert_eq!(calls.iter().filter(|s| *s == "eth_call").count(), 1);
    assert_eq!(calls.iter().filter(|s| *s == "eth_estimateGas").count(), 1);
    assert_eq!(
        calls
            .iter()
            .filter(|s| *s == "eth_getTransactionCount")
            .count(),
        2
    );
    assert_eq!(&*events.lock().unwrap(), &["persist", "send"]);
    let saved = journal.saved.lock().unwrap().clone().unwrap();
    assert_eq!(saved.data["transaction"], json!(tx));
}

#[tokio::test]
async fn validated_submission_rejects_changed_envelope_nonce_authority_and_journal_failure() {
    for scenario in ["binding", "nonce", "authority", "persist"] {
        let events = Arc::new(Mutex::new(vec![]));
        let mut fixture = rpc(events.clone());
        if scenario == "nonce" {
            fixture.nonce += 1;
        }
        let runtime = EvmRuntime::new(
            Arc::new(fixture),
            1,
            Some(Arc::new(AlteringSigner {
                mutation: None,
                events: events.clone(),
            })),
        );
        let original = frozen();
        let validated = validated_transaction(&original);
        let mut tx = original;
        if scenario == "binding" {
            tx.value = Quantity::from_u64(9);
        }
        let journal = Arc::new(Journal {
            saved: Mutex::new(None),
            events: events.clone(),
            authorized: scenario != "authority",
            fail_persist: scenario == "persist",
        });
        assert!(
            runtime
                .submit_revalidated(&tx, json!({}), journal.clone(), &validated)
                .await
                .is_err(),
            "{scenario}"
        );
        assert!(journal.saved.lock().unwrap().is_none(), "{scenario}");
        assert!(
            !events.lock().unwrap().contains(&"send".into()),
            "{scenario}"
        );
        assert_eq!(
            events.lock().unwrap().contains(&"sign".into()),
            scenario == "persist",
            "{scenario}"
        );
    }
}
#[async_trait]
impl EvmRpc for Rpc {
    async fn request(&self, method: &str, _: Value) -> Result<Value> {
        let tx = frozen();
        Ok(match method {
            "eth_chainId" => json!(if self.wrong_chain { "0x2" } else { "0x1" }),
            "eth_getTransactionCount" => json!(format!("{:#x}", self.nonce)),
            "eth_getBalance" => json!(hex_quantity(self.balance)),
            "eth_call" => {
                if self.simulation_failure {
                    return Err(FinError::new(
                        "SYNTHETIC_SIMULATION_FAILED",
                        "simulation",
                        "Isolated contract simulation failed.",
                        "Inspect the fixture.",
                    ));
                }
                json!("0x")
            }
            "eth_estimateGas" => json!(format!("{:#x}", self.gas_estimate.load(Ordering::SeqCst))),
            "eth_maxPriorityFeePerGas" => {
                json!(format!("{:#x}", self.priority_fee.load(Ordering::SeqCst)))
            }
            "eth_blockNumber" => json!(format!("{:#x}", self.latest)),
            "eth_getBlockByNumber" => {
                json!({"hash":format!("0x{}",if self.reorg{"22"}else{"11"}.repeat(32)),"number":"0xa","baseFeePerGas":format!("{:#x}",self.base_fee.load(Ordering::SeqCst))})
            }
            "eth_sendRawTransaction" => {
                self.events.lock().unwrap().push("send".into());
                if self.lost {
                    return Err(FinError::new(
                        "RPC_UNAVAILABLE",
                        "rpc",
                        "Synthetic response lost.",
                        "Reconcile original hash.",
                    ));
                }
                json!(format!("0x{}", "33".repeat(32)))
            }
            "eth_getTransactionReceipt" => {
                json!({"transactionHash":format!("0x{}","33".repeat(32)),"blockHash":format!("0x{}","11".repeat(32)),"blockNumber":"0xa","status":"0x0","gasUsed":"0x5208","effectiveGasPrice":"0x989680","logs":[]})
            }
            "eth_getTransactionByHash" => {
                json!({"hash":format!("0x{}","33".repeat(32)),"blockHash":format!("0x{}","11".repeat(32)),"blockNumber":"0xa","from":tx.sender,"to":tx.to,"chainId":"0x1","type":"0x2","nonce":"0x3","gas":format!("{:#x}",tx.gas),"value":"0x7","input":tx.calldata,"maxFeePerGas":hex_quantity(u256(&tx.max_fee_per_gas).unwrap()),"maxPriorityFeePerGas":hex_quantity(u256(&tx.max_priority_fee_per_gas).unwrap()),"accessList":[]})
            }
            _ => return Err(FinError::validation("Unexpected synthetic RPC method.")),
        })
    }
}
fn rpc(events: Arc<Mutex<Vec<String>>>) -> Rpc {
    Rpc {
        events,
        lost: false,
        latest: 11,
        reorg: false,
        wrong_chain: false,
        nonce: 3,
        priority_fee: AtomicU64::new(10_000_000),
        base_fee: AtomicU64::new(1),
        gas_estimate: AtomicU64::new(21000),
        balance: U256::from(u64::MAX),
        simulation_failure: false,
    }
}

fn runtime_with_rpc(transport: Arc<Rpc>) -> EvmRuntime {
    EvmRuntime::new(
        transport,
        1,
        Some(Arc::new(
            LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY).unwrap(),
        )),
    )
}

#[tokio::test]
async fn revalidation_accepts_only_changes_inside_the_frozen_fee_and_gas_envelope() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let transport = Arc::new(rpc(events.clone()));
    transport.base_fee.store(1_000_000, Ordering::SeqCst);
    let runtime = runtime_with_rpc(transport.clone());
    let sender = LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY)
        .unwrap()
        .address();
    let prepared = runtime
        .prepare(
            sender,
            "0x0000000000000000000000000000000000000002"
                .parse()
                .unwrap(),
            U256::from(7),
            Bytes::new(),
            U256::from(1_000_000_000_000u64),
        )
        .await
        .unwrap();
    let frozen = prepared.transaction.clone();
    assert_eq!(
        u256(&frozen.max_fee_per_gas).unwrap(),
        U256::from(18_000_000)
    );
    assert_eq!(
        u256(&frozen.max_priority_fee_per_gas).unwrap(),
        U256::from(15_000_000)
    );

    transport.base_fee.store(2_000_000, Ordering::SeqCst);
    transport.priority_fee.store(14_000_000, Ordering::SeqCst);
    transport.gas_estimate.store(23_000, Ordering::SeqCst);
    runtime.revalidate(&prepared.transaction).await.unwrap();
    assert_eq!(
        prepared.transaction, frozen,
        "revalidation never mutates the signed envelope"
    );

    // A new market recommendation cannot alter or invalidate the frozen tip.
    transport.base_fee.store(1_000_000, Ordering::SeqCst);
    transport.priority_fee.store(15_000_001, Ordering::SeqCst);
    runtime.revalidate(&prepared.transaction).await.unwrap();
    assert_eq!(prepared.transaction, frozen);
    // Reject when current base plus the full frozen 15m tip exceeds the 18m cap.
    transport.base_fee.store(3_000_001, Ordering::SeqCst);
    assert_eq!(
        runtime
            .revalidate(&prepared.transaction)
            .await
            .unwrap_err()
            .code,
        "FEE_BOUND_CHANGED"
    );
    transport.base_fee.store(2_000_000, Ordering::SeqCst);
    transport
        .gas_estimate
        .store(frozen.gas + 1, Ordering::SeqCst);
    assert_eq!(
        runtime
            .revalidate(&prepared.transaction)
            .await
            .unwrap_err()
            .code,
        "GAS_BOUND_CHANGED"
    );
}

#[tokio::test]
async fn total_fee_cap_clips_priority_headroom() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let transport = Arc::new(rpc(events));
    transport.base_fee.store(0, Ordering::SeqCst);
    let runtime = runtime_with_rpc(transport.clone());
    let sender = LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY)
        .unwrap()
        .address();
    let gas = 25_200u64;
    let cap = U256::from(10_000_000u64) * U256::from(gas);
    let prepared = runtime
        .prepare(
            sender,
            "0x0000000000000000000000000000000000000002"
                .parse()
                .unwrap(),
            U256::from(7),
            Bytes::new(),
            cap,
        )
        .await
        .unwrap();
    assert_eq!(
        u256(&prepared.transaction.max_fee_per_gas).unwrap(),
        U256::from(10_000_000)
    );
    assert_eq!(
        u256(&prepared.transaction.max_priority_fee_per_gas).unwrap(),
        U256::from(10_000_000),
        "priority headroom is clipped by the approved max fee"
    );
    assert_eq!(prepared.transaction.maximum_cost().unwrap(), cap);
    transport.priority_fee.store(10_000_001, Ordering::SeqCst);
    runtime.revalidate(&prepared.transaction).await.unwrap();
    assert_eq!(prepared.transaction.maximum_cost().unwrap(), cap);
    transport.base_fee.store(1, Ordering::SeqCst);
    assert_eq!(
        runtime
            .revalidate(&prepared.transaction)
            .await
            .unwrap_err()
            .code,
        "FEE_BOUND_CHANGED"
    );
}

struct AlteringSigner {
    mutation: Option<usize>,
    events: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl EnvelopeSigner for AlteringSigner {
    async fn sign_prepared(&self, expected: &FrozenTransaction) -> Result<Bytes> {
        self.events.lock().unwrap().push("sign".into());
        let mut changed = expected.clone();
        let mut key = SYNTHETIC_KEY;
        match self.mutation {
            Some(0) => changed.chain_id += 1,
            Some(1) => changed.nonce += 1,
            Some(2) => changed.gas += 1,
            Some(3) => changed.to = "0x0000000000000000000000000000000000000003".into(),
            Some(4) => changed.value = Quantity::from_u64(8),
            Some(5) => changed.calldata = "0x1234".into(),
            Some(6) => changed.max_fee_per_gas = Quantity::from_u64(101_000_000),
            Some(7) => changed.max_priority_fee_per_gas = Quantity::from_u64(11_000_000),
            Some(8) => {
                // A second unmistakably synthetic in-memory key tests recovered
                // sender identity, independently of all transaction fields.
                key = "0000000000000000000000000000000000000000000000000000000000000002";
                changed.sender =
                    lower_address(LocalEnvelopeSigner::from_private_key(key)?.address());
            }
            None => {}
            _ => unreachable!("isolated mutation table"),
        }
        LocalEnvelopeSigner::from_private_key(key)?
            .sign_prepared(&changed)
            .await
    }
}

#[tokio::test]
async fn altered_signer_output_never_persists_or_broadcasts() {
    for mutation in 0..9 {
        let events = Arc::new(Mutex::new(Vec::new()));
        let runtime = EvmRuntime::new(
            Arc::new(rpc(events.clone())),
            1,
            Some(Arc::new(AlteringSigner {
                mutation: Some(mutation),
                events: events.clone(),
            })),
        );
        let journal = Arc::new(Journal {
            saved: Mutex::new(None),
            events: events.clone(),
            authorized: true,
            fail_persist: false,
        });
        assert_eq!(
            runtime
                .submit(&frozen(), json!({}), journal.clone())
                .await
                .unwrap_err()
                .code,
            "SIGNED_TRANSACTION_MISMATCH",
            "mutation {mutation}"
        );
        assert_eq!(*events.lock().unwrap(), vec!["sign"], "mutation {mutation}");
        assert!(journal.saved.lock().unwrap().is_none());
    }
}

#[tokio::test]
async fn wrong_frozen_chain_is_rejected_before_signing() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let runtime = EvmRuntime::new(
        Arc::new(rpc(events.clone())),
        1,
        Some(Arc::new(AlteringSigner {
            mutation: None,
            events: events.clone(),
        })),
    );
    let journal = Arc::new(Journal {
        saved: Mutex::new(None),
        events: events.clone(),
        authorized: true,
        fail_persist: false,
    });
    let mut tx = frozen();
    tx.chain_id = 2;
    assert_eq!(
        runtime
            .submit(&tx, json!({}), journal.clone())
            .await
            .unwrap_err()
            .code,
        "WRONG_CHAIN"
    );
    assert!(events.lock().unwrap().is_empty());
    assert!(journal.saved.lock().unwrap().is_none());
}

#[tokio::test]
async fn changed_nonce_fees_gas_balance_and_failed_simulation_stop_before_signing() {
    for (case, expected) in [
        (0, "WRONG_CHAIN"),
        (1, "NONCE_CHANGED"),
        (2, "FEE_BOUND_CHANGED"),
        (3, "GAS_BOUND_CHANGED"),
        (4, "INSUFFICIENT_GAS_BALANCE"),
        (5, "SYNTHETIC_SIMULATION_FAILED"),
    ] {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut transport = rpc(events.clone());
        match case {
            0 => transport.wrong_chain = true,
            1 => transport.nonce = 4,
            2 => transport.base_fee.store(90_000_001, Ordering::SeqCst),
            3 => transport.gas_estimate.store(30001, Ordering::SeqCst),
            4 => transport.balance = U256::from(6),
            5 => transport.simulation_failure = true,
            _ => unreachable!(),
        }
        let runtime = EvmRuntime::new(
            Arc::new(transport),
            1,
            Some(Arc::new(AlteringSigner {
                mutation: None,
                events: events.clone(),
            })),
        );
        let journal = Arc::new(Journal {
            saved: Mutex::new(None),
            events: events.clone(),
            authorized: true,
            fail_persist: false,
        });
        assert_eq!(
            runtime
                .submit(&frozen(), json!({}), journal.clone())
                .await
                .unwrap_err()
                .code,
            expected,
            "case {case}"
        );
        assert!(events.lock().unwrap().is_empty(), "case {case}");
        assert!(journal.saved.lock().unwrap().is_none());
    }
}
#[tokio::test]
async fn persists_original_identity_before_lost_dispatch_without_signed_bytes() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut transport = rpc(events.clone());
    transport.lost = true;
    let runtime = EvmRuntime::new(
        Arc::new(transport),
        1,
        Some(Arc::new(
            LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY).unwrap(),
        )),
    );
    let journal = Arc::new(Journal {
        saved: Mutex::new(None),
        events: events.clone(),
        authorized: true,
        fail_persist: false,
    });
    assert_eq!(
        runtime
            .submit(&frozen(), json!({"synthetic":true}), journal.clone())
            .await
            .unwrap_err()
            .code,
        "RPC_UNAVAILABLE"
    );
    assert_eq!(*events.lock().unwrap(), vec!["persist", "send"]);
    let saved = journal.saved.lock().unwrap().clone().unwrap();
    assert!(saved.reference.starts_with("0x"));
    assert!(saved.data.get("raw").is_none());
    assert!(saved.data.get("signature").is_none());
}
#[tokio::test]
async fn denied_authorization_and_failed_persistence_never_dispatch() {
    for (authorized, fail_persist) in [(false, false), (true, true)] {
        let events = Arc::new(Mutex::new(Vec::new()));
        let runtime = EvmRuntime::new(
            Arc::new(rpc(events.clone())),
            1,
            Some(Arc::new(
                LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY).unwrap(),
            )),
        );
        let journal = Arc::new(Journal {
            saved: Mutex::new(None),
            events: events.clone(),
            authorized,
            fail_persist,
        });
        assert!(runtime.submit(&frozen(), json!({}), journal).await.is_err());
        assert!(!events.lock().unwrap().contains(&"send".into()));
    }
}
#[tokio::test]
async fn reverted_receipt_requires_original_fields_canonical_block_and_confirmations() {
    let submission = Submission {
        kind: "ethereum".into(),
        reference: format!("0x{}", "33".repeat(32)),
        data: json!({}),
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut transport = rpc(events.clone());
    transport.latest = 10;
    let runtime = EvmRuntime::new(Arc::new(transport), 1, None);
    assert!(
        runtime
            .receipt(&submission, &frozen(), 2)
            .await
            .unwrap()
            .is_none()
    );
    let mut transport = rpc(events.clone());
    transport.reorg = true;
    let runtime = EvmRuntime::new(Arc::new(transport), 1, None);
    assert!(
        runtime
            .receipt(&submission, &frozen(), 2)
            .await
            .unwrap()
            .is_none()
    );
    let runtime = EvmRuntime::new(Arc::new(rpc(events.clone())), 1, None);
    let receipt = runtime
        .receipt(&submission, &frozen(), 2)
        .await
        .unwrap()
        .unwrap();
    assert!(!receipt.success);
    assert_eq!(receipt.fee, U256::from(21000) * U256::from(10_000_000));
    let mut wrong = frozen();
    wrong.nonce += 1;
    assert_eq!(
        runtime
            .receipt(&submission, &wrong, 2)
            .await
            .unwrap_err()
            .code,
        "RECEIPT_TRANSACTION_MISMATCH"
    );
    let mut transport = rpc(events);
    transport.wrong_chain = true;
    assert_eq!(
        EvmRuntime::new(Arc::new(transport), 1, None)
            .receipt(&submission, &frozen(), 2)
            .await
            .unwrap_err()
            .code,
        "WRONG_CHAIN"
    );
}
#[test]
fn production_transport_rejects_embedded_credentials_and_local_rpc() {
    for u in [
        "http://localhost:8545",
        "https://user:secret@example.com",
        "https://example.com?api-key=synthetic",
        "https://example.com#fragment",
    ] {
        assert!(HttpRpc::new(vec![u.into()], 1).is_err())
    }
    assert!(HttpRpc::isolated_localhost("http://127.0.0.1:8545".into(), 1).is_ok());
}
