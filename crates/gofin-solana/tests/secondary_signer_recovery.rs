//! Synthetic coverage of the broker-owned signer path and private v2 recovery.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use bincode::Options;
use gofin_domain::{GoFinError, PreparedOperation, Result, Submission, SubmissionJournal};
use gofin_solana::{
    ReceiptOutcome, SolanaRpc, SolanaRuntime, SolanaSigner, transaction::RpcTransactionSender,
};
use serde_json::{Value, json};
use solana_address::Address;
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_message::{Message, VersionedMessage};
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;

fn codec() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(1232)
}

fn signer(seed: u8) -> Arc<SolanaSigner> {
    let signing = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
    let mut key = [seed; 64];
    key[32..].copy_from_slice(signing.verifying_key().as_bytes());
    Arc::new(SolanaSigner::parse(&bs58::encode(key).into_string()).unwrap())
}

fn unsigned_wire(wallet: &SolanaSigner, secondary: &SolanaSigner) -> String {
    let message = VersionedMessage::Legacy(Message::new_with_blockhash(
        &[Instruction {
            program_id: Address::default(),
            accounts: vec![AccountMeta::new_readonly(
                secondary.address().parse().unwrap(),
                true,
            )],
            data: vec![0],
        }],
        Some(&wallet.address().parse().unwrap()),
        &Hash::default(),
    ));
    assert_eq!(message.header().num_required_signatures, 2);
    STANDARD.encode(
        codec()
            .serialize(&VersionedTransaction {
                signatures: vec![Signature::default(); 2],
                message,
            })
            .unwrap(),
    )
}

fn prepared(message_hash: &str) -> PreparedOperation {
    PreparedOperation {
        operation: "meteora.open".into(),
        input: json!({}),
        binding: json!({"message_sha256":message_hash}),
        summary: "Synthetic broker-owned secondary signer".into(),
        checks: vec![],
        warnings: vec![],
        observed_at: gofin_domain::now(),
        expires_at: (chrono::Utc::now() + chrono::Duration::minutes(1)).to_rfc3339(),
        valuation: None,
        intent: Default::default(),
        action_deadline: None,
        operation_version: 1,
    }
}

#[derive(Default)]
struct Journal(Mutex<Option<Submission>>);
impl SubmissionJournal for Journal {
    fn assert_authorized(&self) -> Result<()> {
        Ok(())
    }
    fn persist_submission(&self, submission: &Submission) -> Result<()> {
        let mut saved = self.0.lock().unwrap();
        assert!(
            saved.is_none(),
            "a second dispatch identity must never be persisted"
        );
        *saved = Some(submission.clone());
        Ok(())
    }
    fn persist_dispatch_evidence(&self, _submission: &Submission, _evidence: &Value) -> Result<()> {
        Ok(())
    }
}

struct DispatchRpc {
    journal: Arc<Journal>,
    preparation_failure: Option<GoFinError>,
    lose_response: bool,
    preparations: AtomicUsize,
    sends: AtomicUsize,
}
#[async_trait]
impl SolanaRpc for DispatchRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        match method {
            "getBlockHeight" => Ok(json!(1)),
            "sendTransaction" => {
                self.sends.fetch_add(1, Ordering::AcqRel);
                assert_eq!(params[1]["maxRetries"], 0);
                let encoded = params[0].as_str().unwrap();
                let saved = self
                    .journal
                    .0
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("persist before dispatch");
                assert_eq!(saved.data["signed_transaction_base64"], encoded);
                let tx: VersionedTransaction = codec()
                    .deserialize(&STANDARD.decode(encoded).unwrap())
                    .unwrap();
                if self.lose_response {
                    Err(GoFinError::new(
                        "SOLANA_RPC_UNAVAILABLE",
                        "rpc",
                        "Synthetic response loss.",
                        "Reconcile.",
                    ))
                } else {
                    Ok(json!(tx.signatures[0].to_string()))
                }
            }
            _ => panic!("unexpected synthetic dispatch RPC: {method}"),
        }
    }
    async fn prepare_send(&self) -> Result<()> {
        self.preparations.fetch_add(1, Ordering::AcqRel);
        self.preparation_failure.clone().map_or(Ok(()), Err)
    }
}

struct ReceiptRpc {
    wire: String,
    status_slot: u64,
    reads: AtomicUsize,
}
#[async_trait]
impl SolanaRpc for ReceiptRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.reads.fetch_add(1, Ordering::AcqRel);
        let tx: VersionedTransaction = codec()
            .deserialize(&STANDARD.decode(&self.wire).unwrap())
            .unwrap();
        let signature = tx.signatures[0].to_string();
        match method {
            "getSignatureStatuses" => {
                assert_eq!(params[0][0], signature);
                assert_eq!(params[1]["searchTransactionHistory"], true);
                Ok(
                    json!({"context":{"slot":99},"value":[{"slot":self.status_slot,"confirmationStatus":"finalized","err":null}]}),
                )
            }
            "getTransaction" => {
                assert_eq!(params[0], signature);
                assert_eq!(params[1]["commitment"], "finalized");
                Ok(
                    json!({"slot":99,"transaction":[self.wire,"base64"],"meta":{"fee":5000,"err":null,"preBalances":[10000,0,0],"postBalances":[5000,0,0],"preTokenBalances":[],"postTokenBalances":[],"loadedAddresses":{"writable":[],"readonly":[]},"innerInstructions":[],"logMessages":[]}}),
                )
            }
            _ => panic!("restart recovery must only read original identity: {method}"),
        }
    }
}

#[tokio::test]
async fn broker_signed_v2_submission_recovers_after_restart_and_response_loss() {
    let wallet = signer(61);
    let secondary = signer(62);
    for lose_response in [false, true] {
        let journal = Arc::new(Journal::default());
        let rpc = Arc::new(DispatchRpc {
            journal: journal.clone(),
            preparation_failure: None,
            lose_response,
            preparations: AtomicUsize::new(0),
            sends: AtomicUsize::new(0),
        });
        let runtime = SolanaRuntime::new(rpc.clone());
        let frozen = runtime
            .freeze_transaction(
                &unsigned_wire(&wallet, &secondary),
                &wallet,
                100,
                10000,
                None,
                &[secondary.address().to_owned()],
            )
            .await
            .unwrap();
        let result = runtime
            .dispatch_with_secondary_signers(
                &frozen,
                &wallet,
                std::slice::from_ref(&secondary),
                &prepared(frozen.message_sha256()),
                journal.clone(),
                &RpcTransactionSender::new(rpc.clone()),
            )
            .await;
        if lose_response {
            assert_eq!(result.unwrap_err().code, "SOLANA_SUBMISSION_UNCERTAIN");
        } else {
            result.unwrap();
        }
        assert_eq!(rpc.sends.load(Ordering::Acquire), 1);
        assert_eq!(rpc.preparations.load(Ordering::Acquire), 2);
        let submission = journal.0.lock().unwrap().clone().unwrap();
        assert_eq!(submission.data["schema_version"], 2);
        assert_eq!(submission.data["missing_cosigners"], json!([]));
        let wire = submission.data["signed_transaction_base64"]
            .as_str()
            .unwrap()
            .to_owned();
        let tx: VersionedTransaction = codec()
            .deserialize(&STANDARD.decode(&wire).unwrap())
            .unwrap();
        let message = codec().serialize(&tx.message).unwrap();
        for (index, signature) in tx.signatures.iter().enumerate() {
            assert!(signature.verify(tx.message.static_account_keys()[index].as_ref(), &message));
            assert_eq!(
                submission.data["existing_signatures"][index]["signature"],
                signature.to_string()
            );
        }
        let receipt_rpc = Arc::new(ReceiptRpc {
            wire: wire.clone(),
            status_slot: 99,
            reads: AtomicUsize::new(0),
        });
        let restarted = SolanaRuntime::new(receipt_rpc.clone());
        let ReceiptOutcome::Finalized { proof, .. } = restarted
            .reconcile_submission(&submission, None)
            .await
            .unwrap()
        else {
            panic!("expected exact finalized proof")
        };
        assert_eq!(proof.slot, 99);
        assert_eq!(proof.message_sha256, frozen.message_sha256());
        assert_eq!(
            proof.required_signatures,
            tx.signatures
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );
        assert_eq!(receipt_rpc.reads.load(Ordering::Acquire), 2);
        let mut stale_missing = submission.clone();
        stale_missing.data["missing_cosigners"] = json!([secondary.address()]);
        assert_eq!(
            restarted
                .reconcile_submission(&stale_missing, None)
                .await
                .unwrap_err()
                .code,
            "SOLANA_RECEIPT_IDENTITY_MISMATCH"
        );
        assert_eq!(
            receipt_rpc.reads.load(Ordering::Acquire),
            2,
            "stale missing-signer data must reject before RPC"
        );
        let conflicting = SolanaRuntime::new(Arc::new(ReceiptRpc {
            wire,
            status_slot: 98,
            reads: AtomicUsize::new(0),
        }));
        assert_eq!(
            conflicting
                .reconcile_submission(&submission, None)
                .await
                .unwrap_err()
                .code,
            "SOLANA_RECEIPT_IDENTITY_MISMATCH"
        );
        assert_eq!(
            rpc.sends.load(Ordering::Acquire),
            1,
            "recovery must not dispatch again"
        );
    }
}

#[tokio::test]
async fn secondary_entry_point_preparation_failures_do_not_persist_or_send() {
    let wallet = signer(63);
    let secondary = signer(64);
    for code in [
        "SOLANA_NETWORK_MISMATCH",
        "SOLANA_RPC_RATE_LIMITED",
        "SOLANA_RPC_UNAVAILABLE",
    ] {
        let expected = GoFinError::new(code, "rpc", "Synthetic primary preparation failed.", "Retry preparation.").with_details(json!({"stage":if code == "SOLANA_RPC_UNAVAILABLE" {"transport_timeout"} else {"primary_identity"}}));
        let journal = Arc::new(Journal::default());
        let rpc = Arc::new(DispatchRpc {
            journal: journal.clone(),
            preparation_failure: Some(expected.clone()),
            lose_response: false,
            preparations: AtomicUsize::new(0),
            sends: AtomicUsize::new(0),
        });
        let runtime = SolanaRuntime::new(rpc.clone());
        let frozen = runtime
            .freeze_transaction(
                &unsigned_wire(&wallet, &secondary),
                &wallet,
                100,
                10000,
                None,
                &[secondary.address().to_owned()],
            )
            .await
            .unwrap();
        let error = runtime
            .dispatch_with_secondary_signers(
                &frozen,
                &wallet,
                std::slice::from_ref(&secondary),
                &prepared(frozen.message_sha256()),
                journal.clone(),
                &RpcTransactionSender::new(rpc.clone()),
            )
            .await
            .unwrap_err();
        assert_eq!(
            serde_json::to_value(error).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert!(journal.0.lock().unwrap().is_none());
        assert_eq!(rpc.sends.load(Ordering::Acquire), 0);
        assert_eq!(rpc.preparations.load(Ordering::Acquire), 1);
    }
}
