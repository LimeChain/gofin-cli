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
            .map_err(|_| FinError::validation("Synthetic journal could not be created."))?;
        serde_json::to_writer(&mut file, submission)
            .map_err(|_| FinError::validation("Synthetic submission could not be stored."))?;
        file.sync_all()
            .map_err(|_| FinError::validation("Synthetic journal could not be synced."))?;
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

fn accepted_hash(accepted: &Mutex<Option<String>>, request: &Value) -> String {
    let original = accepted.lock().unwrap().clone().unwrap();
    assert_eq!(request["params"], json!([original]));
    original
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
    let primary = Server::new(vec![
        Reply::result("0x1"), // HttpRpc endpoint identity.
        Reply::result("0x1"), // EvmRuntime revalidation.
        Reply::computed(|_| {
            json!({"baseFeePerGas":"0x1", "number":"0xb",
            "hash":format!("0x{}", "22".repeat(32)),
            "timestamp":format!("0x{:x}", SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs())})
        }),
        Reply::result("0x3"),
        Reply::result("0x"),
        Reply::result("0x5208"),
        Reply::result("0xffffffffffffffff"),
        Reply::lost_response(move |request| {
            assert_eq!(request["method"], "eth_sendRawTransaction");
            let raw = rpc_bytes(&request["params"][0]).unwrap();
            let original_hash = format!("{:#x}", verify_signed_envelope(&tx, &raw).unwrap());
            // The original identity must already be on disk when A accepts it.
            assert_eq!(saved.load().reference, original_hash);
            *received.lock().unwrap() = Some(original_hash);
            trace.lock().unwrap().push("accept");
            Value::Null
        }),
        // Subsequent reads fail over to B; broadcasts must never do so.
        Reply::rate_limit(Some("120")),
    ]);

    let receipt_hash = accepted.clone();
    let receipt_block = block_hash.clone();
    let reported_hash = accepted.clone();
    let reported_block = block_hash.clone();
    let tx = transaction.clone();
    let backup = Server::new(vec![
        Reply::result("0x1"), // Independently validate B's chain.
        Reply::result("0x1"),
        Reply::computed(|_| Value::Null), // B has not indexed the receipt yet.
        Reply::result("0x1"),
        Reply::computed(move |request| {
            let original = accepted_hash(&receipt_hash, request);
            json!({
                "transactionHash": if corrupt_receipt { format!("0x{}", "ff".repeat(32)) } else { original },
                "blockHash":receipt_block,"blockNumber":"0xa","status":"0x1",
                "gasUsed":"0x5208","effectiveGasPrice":"0x989680","logs":[]
            })
        }),
        Reply::computed(move |request| {
            let original = accepted_hash(&reported_hash, request);
            json!({
                "hash":original,"blockHash":reported_block,"blockNumber":"0xa",
                "from":tx.sender,"to":tx.to,"chainId":"0x1","type":"0x2",
                "nonce":"0x3","gas":"0x7530","value":"0x7","input":"0x",
                "maxFeePerGas":"0x5f5e100","maxPriorityFeePerGas":"0x989680","accessList":[]
            })
        }),
        Reply::computed(move |_| json!({"hash":block_hash,"number":"0xa"})),
        Reply::result("0xb"),
        Reply::computed(|_| {
            json!({"number":"0xb", "hash":format!("0x{}", "22".repeat(32)),
            "timestamp":format!("0x{:x}", SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs())})
        }),
    ]);
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
        assert_eq!(result.unwrap_err().code, "RECEIPT_TRANSACTION_MISMATCH");
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
    assert_eq!(requests.len(), 9);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["method"] == "eth_sendRawTransaction")
            .count(),
        1
    );
    let requests = backup.requests();
    assert_eq!(requests.len(), if corrupt_receipt { 6 } else { 9 });
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
