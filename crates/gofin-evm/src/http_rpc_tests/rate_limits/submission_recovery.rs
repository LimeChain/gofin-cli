use super::*;
use async_trait::async_trait;
use std::path::PathBuf;

// Publicly known test key, used only by the two loopback RPC fixtures.
const SYNTHETIC_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";

struct CountingSigner {
    inner: LocalEnvelopeSigner,
    events: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait]
impl EnvelopeSigner for CountingSigner {
    async fn sign_prepared(&self, transaction: &FrozenTransaction) -> Result<Bytes> {
        self.events.lock().unwrap().push("sign");
        self.inner.sign_prepared(transaction).await
    }
}

// A synthetic approval and a real temporary file exercise the journal callback.
// This is not the protected broker's SQLite journal or real Touch ID.
struct TestJournal {
    path: PathBuf,
    events: Arc<Mutex<Vec<&'static str>>>,
}

impl TestJournal {
    fn load(&self) -> Submission {
        serde_json::from_slice(&std::fs::read(&self.path).unwrap()).unwrap()
    }
}

impl SubmissionJournal for TestJournal {
    fn assert_authorized(&self) -> Result<()> {
        Ok(())
    }

    fn persist_submission(&self, submission: &Submission) -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)
            .map_err(|_| GoFinError::validation("Synthetic journal could not be created."))?;
        serde_json::to_writer(&mut file, submission)
            .map_err(|_| GoFinError::validation("Synthetic submission could not be stored."))?;
        file.sync_all()
            .map_err(|_| GoFinError::validation("Synthetic journal could not be synced."))?;
        self.events.lock().unwrap().push("persist");
        Ok(())
    }
}

fn original_transaction(signer: &LocalEnvelopeSigner) -> FrozenTransaction {
    FrozenTransaction {
        rollup_fee: None,
        chain_id: 1,
        sender: lower_address(signer.address()),
        to: "0x0000000000000000000000000000000000000002".into(),
        value: Quantity::from_u64(7),
        calldata: "0x".into(),
        nonce: 3,
        gas: 30_000,
        max_fee_per_gas: Quantity::from_u64(100_000_000),
        max_priority_fee_per_gas: Quantity::from_u64(10_000_000),
    }
}

fn revalidation_reply(request: &Value) -> Reply {
    match request["method"].as_str().unwrap() {
        "eth_chainId" => Reply::result("0x1"),
        "eth_getBlockByNumber" => Reply::computed(|_| {
            json!({"baseFeePerGas":"0x1", "number":"0xb",
                "hash":format!("0x{}", "22".repeat(32)),
                "timestamp":format!("0x{:x}", SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs())})
        }),
        "eth_getTransactionCount" => Reply::result("0x3"),
        "eth_call" => Reply::result("0x"),
        "eth_estimateGas" => Reply::result("0x5208"),
        "eth_getBalance" => Reply::result("0xffffffffffffffff"),
        method => panic!("unexpected revalidation read: {method}"),
    }
}

async fn recover_lost_broadcast(corrupt_receipt: bool) {
    let directory = tempfile::tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let journal = Arc::new(TestJournal {
        path: directory.path().join("original-submission.json"),
        events: events.clone(),
    });
    let signer = Arc::new(CountingSigner {
        inner: LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY).unwrap(),
        events: events.clone(),
    });
    let transaction = original_transaction(&signer.inner);
    let accepted = Arc::new(Mutex::new(None));
    let block_hash = format!("0x{}", "11".repeat(32));

    let tx = transaction.clone();
    let saved = journal.clone();
    let received = accepted.clone();
    let trace = events.clone();
    let primary = Server::handling(move |request| {
        if request["method"] == "eth_sendRawTransaction" {
            let tx = tx.clone();
            let saved = saved.clone();
            let received = received.clone();
            let trace = trace.clone();
            return Reply::lost_response(move |request| {
                let raw = rpc_bytes(&request["params"][0]).unwrap();
                let original_hash = format!("{:#x}", verify_signed_envelope(&tx, &raw).unwrap());
                // The original identity must already be on disk when A accepts it.
                assert_eq!(saved.load().reference, original_hash);
                *received.lock().unwrap() = Some(original_hash);
                trace.lock().unwrap().push("accept");
                Value::Null
            });
        }
        if received.lock().unwrap().is_some() {
            // A is rate-limited after accepting the original broadcast. This
            // is observed before B returns recovery evidence, then respected.
            return Reply::rate_limit(Some("120"));
        }
        revalidation_reply(request)
    });

    let received = accepted.clone();
    let tx = transaction.clone();
    let mut receipt_reads = 0;
    let backup = Server::handling(move |request| {
        assert_ne!(
            request["method"], "eth_sendRawTransaction",
            "read racing must never send a financial broadcast to B"
        );
        let Some(original) = received.lock().unwrap().clone() else {
            // Both endpoints can receive read-only revalidation. Return the
            // same evidence, slower than A, without assuming request order.
            return revalidation_reply(request).delayed(Duration::from_millis(30));
        };
        match request["method"].as_str().unwrap() {
            "eth_chainId" => Reply::result("0x1"),
            "eth_getTransactionReceipt" => {
                assert_eq!(request["params"], json!([original]));
                receipt_reads += 1;
                if receipt_reads == 1 {
                    // Absence of evidence is pending, never proof of failure.
                    Reply::computed(|_| Value::Null)
                } else {
                    let reported_hash = if corrupt_receipt {
                        format!("0x{}", "ff".repeat(32))
                    } else {
                        original
                    };
                    let block_hash = block_hash.clone();
                    Reply::computed(move |_| {
                        json!({
                            "transactionHash":reported_hash,"blockHash":block_hash,
                            "blockNumber":"0xa","status":"0x1","gasUsed":"0x5208",
                            "effectiveGasPrice":"0x989680","logs":[]
                        })
                    })
                }
            }
            "eth_getTransactionByHash" => {
                assert_eq!(request["params"], json!([original]));
                let tx = tx.clone();
                let block_hash = block_hash.clone();
                Reply::computed(move |_| {
                    json!({
                        "hash":original,"blockHash":block_hash,"blockNumber":"0xa",
                        "from":tx.sender,"to":tx.to,"chainId":"0x1","type":"0x2",
                        "nonce":"0x3","gas":"0x7530","value":"0x7","input":"0x",
                        "maxFeePerGas":"0x5f5e100","maxPriorityFeePerGas":"0x989680","accessList":[]
                    })
                })
            }
            "eth_getBlockByNumber" if request["params"][0] == "0xa" => {
                let block_hash = block_hash.clone();
                Reply::computed(move |_| json!({"hash":block_hash,"number":"0xa"}))
            }
            "eth_blockNumber" => Reply::result("0xb"),
            "eth_getBlockByNumber" => revalidation_reply(request),
            "eth_getTransactionCount" | "eth_call" | "eth_estimateGas" | "eth_getBalance" => {
                // A cancelled loser may already be queued in the fixture when
                // A's fast revalidation completes and the broadcast arrives.
                revalidation_reply(request).delayed(Duration::from_millis(30))
            }
            method => panic!("unexpected recovery read: {method}"),
        }
    });
    let mut rpc = primary.rpc();
    rpc.urls.push(backup.url.clone());
    let rpc = Arc::new(rpc);
    let runtime = EvmRuntime::new(rpc.clone(), 1, Some(signer));
    let metadata = json!({"synthetic":true,"operation_id":"synthetic-response-loss"});

    let failure = runtime
        .submit(&transaction, metadata.clone(), journal.clone())
        .await
        .unwrap_err();
    assert!(
        !backup
            .requests()
            .iter()
            .any(|request| request["method"] == "eth_sendRawTransaction"),
        "an unanswered broadcast must not be resent through RPC B"
    );
    assert_eq!(failure.code, "RPC_UNAVAILABLE");
    assert_eq!(*events.lock().unwrap(), vec!["sign", "persist", "accept"]);
    let original = journal.load();
    assert_eq!(
        original.reference,
        accepted.lock().unwrap().clone().unwrap()
    );
    assert_eq!(
        original.data["transaction"],
        serde_json::to_value(&transaction).unwrap()
    );
    assert_eq!(original.data["metadata"], metadata);
    assert!(original.data.get("raw").is_none());
    assert!(original.data.get("signature").is_none());
    let original_bytes = std::fs::read(&journal.path).unwrap();
    drop(runtime);

    // Recover using the stored identity and a runtime with no signing provider.
    let recovery = EvmRuntime::new(rpc, 1, None);
    assert!(!recovery.can_sign());
    assert!(
        recovery
            .receipt(&original, &transaction, 2)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(*events.lock().unwrap(), vec!["sign", "persist", "accept"]);

    let result = recovery.receipt(&journal.load(), &transaction, 2).await;
    if corrupt_receipt {
        // Reject the wrong hash during transport validation, before this
        // receipt can win the race or reach the runtime's envelope checks.
        let failure = result.unwrap_err();
        assert_eq!(failure.code, "RPC_DATA_INVALID");
        assert!(
            failure.details.as_deref().unwrap()["rpc_health"]["last_read"]["attempts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|attempt| attempt["endpoint_id"] == "rpc-2"
                    && attempt["reason"] == "incompatible")
        );
    } else {
        let receipt = result.unwrap().unwrap();
        assert_eq!(receipt.hash, hash(&original.reference).unwrap());
        assert!(receipt.success);
        assert_eq!(receipt.block_number, 10);
        assert_eq!(receipt.confirmations, 2);
        assert_eq!(receipt.fee, U256::from(210_000_000_000_u64));
    }
    assert_eq!(std::fs::read(&journal.path).unwrap(), original_bytes);
    assert_eq!(*events.lock().unwrap(), vec!["sign", "persist", "accept"]);
    let requests = primary.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["method"] == "eth_sendRawTransaction")
            .count(),
        1
    );
    let requests = backup.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["method"] == "eth_getTransactionReceipt")
            .count(),
        2
    );
    assert!(
        !requests
            .iter()
            .any(|request| request["method"] == "eth_sendRawTransaction")
    );
    for request in requests.iter().filter(|request| {
        matches!(
            request["method"].as_str(),
            Some("eth_getTransactionReceipt" | "eth_getTransactionByHash")
        )
    }) {
        assert_eq!(request["params"], json!([original.reference]));
    }
}

#[tokio::test]
async fn lost_broadcast_response_reconciles_original_hash_through_backup_without_resending() {
    recover_lost_broadcast(false).await;
}

#[tokio::test]
async fn lost_broadcast_response_rejects_a_different_receipt_hash_without_resending() {
    recover_lost_broadcast(true).await;
}
