use async_trait::async_trait;
use gofin_domain::{GoFinError, Quantity, Result, Submission, SubmissionJournal};
use gofin_evm::{
    alloy::primitives::{Address, Bytes, U256},
    *,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

const SYNTHETIC_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";
fn q(text: &str) -> Quantity {
    Quantity::new(text).unwrap()
}
fn eip_permit(owner: String) -> Eip2612Permit {
    Eip2612Permit {
        chain_id: 1,
        token: "0x0000000000000000000000000000000000000002".into(),
        owner,
        spender: "0x0000000000000000000000000000000000000003".into(),
        value: q("100"),
        nonce: q("7"),
        deadline: 200,
        domain_separator: format!("0x{}", "11".repeat(32)),
    }
}
fn permit2(owner: String) -> Permit2PermitSingle {
    Permit2PermitSingle {
        chain_id: 1,
        verifying_contract: "0x0000000000000000000000000000000000000004".into(),
        owner,
        token: "0x0000000000000000000000000000000000000002".into(),
        amount: q("100"),
        expiration: 200,
        nonce: 7,
        spender: "0x0000000000000000000000000000000000000003".into(),
        sig_deadline: 200,
    }
}
fn unsigned_permit_tx(sender: Address, calldata: Bytes) -> UnsignedPermitTransaction {
    UnsignedPermitTransaction {
        sender,
        to: "0x0000000000000000000000000000000000000002"
            .parse()
            .unwrap(),
        value: U256::ZERO,
        calldata,
        gas_limit: 30_000,
        maximum_total_fee: U256::from(1_000_000_000_000u64),
    }
}

#[test]
fn eip2612_digest_binds_every_replay_sensitive_field_and_rejects_unbounded_inputs() {
    let owner = LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY).unwrap();
    let p = eip_permit(lower_address(owner.address()));
    p.validate(1, 100).unwrap();
    let first = p.digest().unwrap();
    let mut changed_nonce = p.clone();
    changed_nonce.nonce = q("8");
    assert_ne!(first, changed_nonce.digest().unwrap());
    let mut changed_domain = p.clone();
    changed_domain.domain_separator = format!("0x{}", "22".repeat(32));
    assert_ne!(first, changed_domain.digest().unwrap());
    assert_eq!(
        p.validate(2, 100).unwrap_err().code,
        "INVALID_AUTHORIZED_PERMIT"
    );
    assert_eq!(
        p.validate(1, 200).unwrap_err().code,
        "INVALID_AUTHORIZED_PERMIT"
    );
    let mut max_value = p.clone();
    max_value.value =
        q("115792089237316195423570985008687907853269984665640564039457584007913129639935");
    assert!(max_value.digest().is_err());
    let mut zero_owner = p;
    zero_owner.owner = "0x0000000000000000000000000000000000000000".into();
    assert!(zero_owner.digest().is_err());
}

#[test]
fn permit2_digest_binds_chain_nonce_expiry_spender_and_amount_cap() {
    let owner = LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY).unwrap();
    let p = permit2(lower_address(owner.address()));
    p.validate(1, 100).unwrap();
    let digest = p.digest().unwrap();
    let mut changed = p.clone();
    changed.nonce += 1;
    assert_ne!(digest, changed.digest().unwrap());
    changed = p.clone();
    changed.chain_id = 2;
    assert_ne!(digest, changed.digest().unwrap());
    changed = p.clone();
    changed.expiration += 1;
    assert_ne!(digest, changed.digest().unwrap());
    changed = p.clone();
    changed.amount = q("1461501637330902918203684832716283019655932542975"); // uint160 max
    assert!(changed.digest().is_err());
    assert_eq!(
        p.validate(1, 200).unwrap_err().code,
        "INVALID_AUTHORIZED_PERMIT"
    );
}

#[tokio::test]
async fn local_signer_returns_only_recoverable_typed_permit_signature_and_checks_owner() {
    let signer = LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY).unwrap();
    let permit = AuthorizedPermit::Eip2612(eip_permit(lower_address(signer.address())));
    let signature = signer.sign_authorized_permit(&permit).await.unwrap();
    assert_eq!(signature.len(), 65);
    let permit2 = AuthorizedPermit::Permit2Single(permit2(lower_address(signer.address())));
    assert_eq!(
        signer.sign_authorized_permit(&permit2).await.unwrap().len(),
        65
    );
    let mut wrong_owner = match permit {
        AuthorizedPermit::Eip2612(p) => p,
        _ => unreachable!(),
    };
    wrong_owner.owner = "0x0000000000000000000000000000000000000005".into();
    assert_eq!(
        signer
            .sign_authorized_permit(&AuthorizedPermit::Eip2612(wrong_owner))
            .await
            .unwrap_err()
            .code,
        "SIGNER_MISMATCH"
    );
}

struct Journal {
    authorized: bool,
    deny_on_assertion: Option<usize>,
    assertions: AtomicUsize,
    writes: Mutex<Vec<Submission>>,
}
impl SubmissionJournal for Journal {
    fn assert_authorized(&self) -> Result<()> {
        let call = self.assertions.fetch_add(1, Ordering::SeqCst) + 1;
        if self.authorized && self.deny_on_assertion != Some(call) {
            Ok(())
        } else {
            Err(GoFinError::validation("Synthetic authorization denied."))
        }
    }
    fn persist_submission(&self, submission: &Submission) -> Result<()> {
        self.writes.lock().unwrap().push(submission.clone());
        Ok(())
    }
}
struct Rpc;
#[async_trait]
impl EvmRpc for Rpc {
    async fn request(&self, method: &str, _: Value) -> Result<Value> {
        match method {
            "eth_chainId" => Ok(json!("0x1")),
            "eth_getBlockByNumber" => Ok(json!({"timestamp":"0x64"})),
            _ => Err(GoFinError::validation("Unexpected permit fixture RPC.")),
        }
    }
}
struct CountingSigner(AtomicUsize);
#[async_trait]
impl EnvelopeSigner for CountingSigner {
    async fn sign_prepared(&self, _: &FrozenTransaction) -> Result<Bytes> {
        Ok(Bytes::new())
    }
    async fn sign_authorized_permit(&self, _: &AuthorizedPermit) -> Result<Bytes> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Bytes::from(vec![0u8; 65]))
    }
}
struct ForwardingSigner {
    inner: LocalEnvelopeSigner,
    calls: AtomicUsize,
}
#[async_trait]
impl EnvelopeSigner for ForwardingSigner {
    async fn sign_prepared(&self, tx: &FrozenTransaction) -> Result<Bytes> {
        self.inner.sign_prepared(tx).await
    }
    async fn sign_authorized_permit(&self, permit: &AuthorizedPermit) -> Result<Bytes> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.sign_authorized_permit(permit).await
    }
}
fn journal(authorized: bool) -> Arc<Journal> {
    Arc::new(Journal {
        authorized,
        deny_on_assertion: None,
        assertions: AtomicUsize::new(0),
        writes: Mutex::new(Vec::new()),
    })
}

#[tokio::test]
async fn journal_denial_precedes_all_permit_signer_calls() {
    let signer = Arc::new(CountingSigner(AtomicUsize::new(0)));
    let runtime = EvmRuntime::new(Arc::new(Rpc), 1, Some(signer.clone()));
    let permit = AuthorizedPermit::Eip2612(eip_permit(
        "0x0000000000000000000000000000000000000001".into(),
    ));
    let journal = journal(false);
    assert!(
        runtime
            .sign_authorized_permit(&permit, journal)
            .await
            .is_err()
    );
    assert_eq!(signer.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn runtime_rejects_a_custom_signers_wrong_permit_signature() {
    let signer = Arc::new(CountingSigner(AtomicUsize::new(0)));
    let runtime = EvmRuntime::new(Arc::new(Rpc), 1, Some(signer.clone()));
    let owner = LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY).unwrap();
    let permit = AuthorizedPermit::Eip2612(eip_permit(lower_address(owner.address())));
    assert_eq!(
        runtime
            .sign_authorized_permit(&permit, journal(true))
            .await
            .unwrap_err()
            .code,
        "PERMIT_SIGNATURE_MISMATCH"
    );
    assert_eq!(signer.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn revocation_during_signing_discards_the_valid_signature() {
    let owner = LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY).unwrap();
    let signer = Arc::new(ForwardingSigner {
        inner: owner,
        calls: AtomicUsize::new(0),
    });
    let runtime = EvmRuntime::new(Arc::new(Rpc), 1, Some(signer.clone()));
    let permit = AuthorizedPermit::Eip2612(eip_permit(lower_address(signer.inner.address())));
    let journal = Arc::new(Journal {
        authorized: true,
        deny_on_assertion: Some(2),
        assertions: AtomicUsize::new(0),
        writes: Mutex::new(Vec::new()),
    });
    assert!(
        runtime
            .sign_authorized_permit(&permit, journal.clone())
            .await
            .is_err()
    );
    assert_eq!(signer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(journal.assertions.load(Ordering::SeqCst), 2);
}

struct MaterializeRpc;
#[async_trait]
impl EvmRpc for MaterializeRpc {
    async fn request(&self, method: &str, _: Value) -> Result<Value> {
        match method {
            "eth_chainId" => Ok(json!("0x1")),
            "eth_getBlockByNumber" => Ok(json!({"timestamp":"0x64", "baseFeePerGas":"0x1"})),
            "eth_maxPriorityFeePerGas" => Ok(json!("0x989680")),
            "eth_getTransactionCount" => Ok(json!("0x3")),
            "eth_getBalance" => Ok(json!(hex_quantity(U256::MAX))),
            "eth_call" => Ok(json!("0x")),
            "eth_estimateGas" => Ok(json!("0x5208")),
            _ => Err(GoFinError::validation("Unexpected materialization RPC.")),
        }
    }
}

struct CountingMaterializeRpc {
    calls: Mutex<Vec<String>>,
    fail_simulation: bool,
}
#[async_trait]
impl EvmRpc for CountingMaterializeRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.calls.lock().unwrap().push(method.into());
        if method == "eth_call" && self.fail_simulation {
            return Err(GoFinError::validation(
                "Synthetic materialized simulation failed.",
            ));
        }
        if method == "eth_sendRawTransaction" {
            let bytes = rpc_bytes(&params[0])?;
            return Ok(json!(format!("{:#x}", alloy::primitives::keccak256(bytes))));
        }
        MaterializeRpc.request(method, params).await
    }
}

#[tokio::test]
async fn revalidated_permit_submission_simulates_signed_calldata_once_and_blocks_failed_simulation()
{
    for fail_simulation in [false, true] {
        let signer = Arc::new(ForwardingSigner {
            inner: LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY).unwrap(),
            calls: AtomicUsize::new(0),
        });
        let owner = signer.inner.address();
        let rpc = Arc::new(CountingMaterializeRpc {
            calls: Mutex::new(vec![]),
            fail_simulation,
        });
        let runtime = EvmRuntime::new(rpc.clone(), 1, Some(signer.clone()));
        let mut calldata = vec![0x12, 0x34, 0x56, 0x78];
        calldata.extend_from_slice(&[0; 65]);
        let template = runtime
            .prepare_permit_template(
                unsigned_permit_tx(owner, Bytes::from(calldata)),
                4,
                AuthorizedPermit::Eip2612(eip_permit(lower_address(owner))),
            )
            .await
            .unwrap();
        let candidate = gofin_domain::PreparedOperation {
            operation: "synthetic.permit.swap".into(),
            input: json!({}),
            binding: json!({"transaction":template.transaction(), "permit_template":template}),
            summary: "Isolated synthetic permit".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: gofin_domain::now(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            valuation: None,
            intent: gofin_domain::FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        };
        let validated = gofin_domain::RevalidatedOperation::after_revalidation(
            candidate,
            Arc::new(gofin_domain::clock::SystemClock),
        )
        .unwrap();
        let journal = journal(true);
        let result = runtime
            .submit_permit_template_revalidated(&template, json!({}), journal.clone(), &validated)
            .await;
        assert_eq!(result.is_ok(), !fail_simulation);
        let calls = rpc.calls.lock().unwrap();
        assert_eq!(calls.iter().filter(|m| *m == "eth_call").count(), 1);
        assert_eq!(
            calls.iter().filter(|m| *m == "eth_estimateGas").count(),
            usize::from(!fail_simulation)
        );
        assert_eq!(
            calls
                .iter()
                .filter(|m| *m == "eth_sendRawTransaction")
                .count(),
            usize::from(!fail_simulation)
        );
        assert_eq!(signer.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            journal.writes.lock().unwrap().len(),
            usize::from(!fail_simulation)
        );
    }
}

#[tokio::test]
async fn serialized_template_materializes_once_into_a_revalidated_concrete_call() {
    let owner = LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY).unwrap();
    let owner_address = owner.address();
    let runtime = EvmRuntime::new(Arc::new(MaterializeRpc), 1, Some(Arc::new(owner)));
    let mut template_bytes = vec![0x12, 0x34, 0x56, 0x78];
    template_bytes.extend_from_slice(&[0; 65]);
    let template = runtime
        .prepare_permit_template(
            unsigned_permit_tx(owner_address, Bytes::from(template_bytes)),
            4,
            AuthorizedPermit::Eip2612(eip_permit(lower_address(owner_address))),
        )
        .await
        .unwrap();
    let serialized = serde_json::to_vec(&template).unwrap();
    let restored: PreparedPermitTemplate = serde_json::from_slice(&serialized).unwrap();
    assert_eq!(restored.transaction(), template.transaction());
    let prepared = runtime
        .materialize_permit_template(&restored, journal(true))
        .await
        .unwrap();
    let calldata = rpc_bytes(&json!(prepared.transaction.calldata)).unwrap();
    assert_eq!(&calldata[..4], &[0x12, 0x34, 0x56, 0x78]);
    assert_ne!(&calldata[4..69], &[0; 65]);
    assert!(prepared.simulation.is_empty());
    validate_materialized_permit_template(&restored, &prepared.transaction).unwrap();
}

#[tokio::test]
async fn abi_signature_words_materialize_and_only_those_words_may_change() {
    let owner = LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY).unwrap();
    let owner_address = owner.address();
    let runtime = EvmRuntime::new(Arc::new(MaterializeRpc), 1, Some(Arc::new(owner)));
    // A nested multicall selfPermit payload may start on a different byte
    // alignment than the outer function selector's top-level ABI words.
    let mut template_bytes = vec![0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0];
    template_bytes.extend_from_slice(&[0; 96]);
    let template = runtime
        .prepare_permit_template_with_layout(
            unsigned_permit_tx(owner_address, Bytes::from(template_bytes)),
            SignatureLayout::Eip2612Abi {
                v_offset: 8,
                r_offset: 40,
                s_offset: 72,
            },
            AuthorizedPermit::Eip2612(eip_permit(lower_address(owner_address))),
        )
        .await
        .unwrap();
    let prepared = runtime
        .materialize_permit_template(&template, journal(true))
        .await
        .unwrap();
    let calldata = rpc_bytes(&json!(prepared.transaction.calldata)).unwrap();
    assert_eq!(
        &calldata[..8],
        &[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0]
    );
    assert!(calldata[8..39].iter().all(|byte| *byte == 0));
    assert!(matches!(calldata[39], 27 | 28));
    assert_ne!(&calldata[40..104], &[0; 64]);
    validate_materialized_permit_template(&template, &prepared.transaction).unwrap();

    let mut changed = prepared.transaction.clone();
    let mut bytes = calldata.to_vec();
    bytes[0] ^= 1;
    changed.calldata = hex_bytes(&bytes);
    assert_eq!(
        validate_materialized_permit_template(&template, &changed)
            .unwrap_err()
            .code,
        "PERMIT_TRANSACTION_MISMATCH"
    );
    changed = prepared.transaction;
    changed.max_fee_per_gas = q("999");
    assert_eq!(
        validate_materialized_permit_template(&template, &changed)
            .unwrap_err()
            .code,
        "PERMIT_TRANSACTION_MISMATCH"
    );
}

#[tokio::test]
async fn restored_template_with_changed_owner_is_rejected_before_signing() {
    let owner = Arc::new(LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY).unwrap());
    let runtime = EvmRuntime::new(Arc::new(MaterializeRpc), 1, Some(owner.clone()));
    let template_bytes = vec![0; 65];
    let template = runtime
        .prepare_permit_template(
            unsigned_permit_tx(owner.address(), Bytes::from(template_bytes)),
            0,
            AuthorizedPermit::Eip2612(eip_permit(lower_address(owner.address()))),
        )
        .await
        .unwrap();
    let mut saved: Value = serde_json::to_value(template).unwrap();
    saved["transaction"]["sender"] = json!("0x0000000000000000000000000000000000000005");
    let restored: PreparedPermitTemplate = serde_json::from_value(saved).unwrap();
    let signer = Arc::new(CountingSigner(AtomicUsize::new(0)));
    let runtime = EvmRuntime::new(Arc::new(MaterializeRpc), 1, Some(signer.clone()));
    assert!(
        runtime
            .materialize_permit_template(&restored, journal(true))
            .await
            .is_err()
    );
    assert_eq!(signer.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn template_materialization_honors_journal_denial_before_and_after_signing() {
    let owner = LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY).unwrap();
    let owner_address = owner.address();
    let template_runtime = EvmRuntime::new(Arc::new(MaterializeRpc), 1, Some(Arc::new(owner)));
    let mut calldata = vec![0x12, 0x34, 0x56, 0x78];
    calldata.extend_from_slice(&[0; 65]);
    let template = template_runtime
        .prepare_permit_template(
            unsigned_permit_tx(owner_address, Bytes::from(calldata)),
            4,
            AuthorizedPermit::Eip2612(eip_permit(lower_address(owner_address))),
        )
        .await
        .unwrap();
    let signer = Arc::new(ForwardingSigner {
        inner: LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY).unwrap(),
        calls: AtomicUsize::new(0),
    });
    let runtime = EvmRuntime::new(Arc::new(MaterializeRpc), 1, Some(signer.clone()));
    assert!(
        runtime
            .materialize_permit_template(&template, journal(false))
            .await
            .is_err()
    );
    assert_eq!(signer.calls.load(Ordering::SeqCst), 0);

    let revoked = Arc::new(Journal {
        authorized: true,
        deny_on_assertion: Some(3),
        assertions: AtomicUsize::new(0),
        writes: Mutex::new(Vec::new()),
    });
    assert!(
        runtime
            .materialize_permit_template(&template, revoked.clone())
            .await
            .is_err()
    );
    assert_eq!(signer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(revoked.assertions.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn providers_without_typed_permit_support_fail_closed() {
    let unsupported = NoPermitSigner;
    let permit = AuthorizedPermit::Eip2612(eip_permit(
        "0x0000000000000000000000000000000000000001".into(),
    ));
    assert_eq!(
        unsupported
            .sign_authorized_permit(&permit)
            .await
            .unwrap_err()
            .code,
        "PERMIT_SIGNING_UNAVAILABLE"
    );
}
struct NoPermitSigner;
#[async_trait]
impl EnvelopeSigner for NoPermitSigner {
    async fn sign_prepared(&self, _: &FrozenTransaction) -> Result<Bytes> {
        Ok(Bytes::new())
    }
}

#[allow(dead_code)]
fn _address_type_is_publicly_usable(_: Address, _: U256) {}

/// Synthetic in-memory fee/nonce changes: no transport or signing is permitted.
struct MovingPermitRpc {
    base: std::sync::atomic::AtomicU64,
    priority: std::sync::atomic::AtomicU64,
    nonce: std::sync::atomic::AtomicU64,
    timestamp: std::sync::atomic::AtomicU64,
    funded: std::sync::atomic::AtomicBool,
    unsigned_simulations: AtomicUsize,
}
#[async_trait]
impl EvmRpc for MovingPermitRpc {
    async fn request(&self, method: &str, _: Value) -> Result<Value> {
        match method {
            "eth_chainId" => Ok(json!("0x1")),
            "eth_getBlockByNumber" => Ok(json!({
                "timestamp":format!("{:#x}", self.timestamp.load(Ordering::SeqCst)),
                "baseFeePerGas":format!("{:#x}", self.base.load(Ordering::SeqCst))
            })),
            "eth_maxPriorityFeePerGas" => Ok(json!(format!(
                "{:#x}",
                self.priority.load(Ordering::SeqCst)
            ))),
            "eth_getTransactionCount" => {
                Ok(json!(format!("{:#x}", self.nonce.load(Ordering::SeqCst))))
            }
            "eth_getBalance" => Ok(json!(if self.funded.load(Ordering::SeqCst) {
                hex_quantity(U256::MAX)
            } else {
                "0x0".into()
            })),
            "eth_call" | "eth_estimateGas" | "eth_sendRawTransaction" => {
                self.unsigned_simulations.fetch_add(1, Ordering::SeqCst);
                Err(GoFinError::validation(
                    "Unsigned permit test must never simulate or broadcast.",
                ))
            }
            _ => Err(GoFinError::validation(
                "Unexpected moving-permit fixture RPC.",
            )),
        }
    }
}

#[tokio::test]
async fn permit_revalidation_preserves_frozen_bid_through_base_changes_and_rejects_scope_or_funding_errors()
 {
    for permit2_mode in [false, true] {
        let signer = Arc::new(CountingSigner(AtomicUsize::new(0)));
        let owner = LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY).unwrap();
        let rpc = Arc::new(MovingPermitRpc {
            base: std::sync::atomic::AtomicU64::new(100_000_000),
            priority: std::sync::atomic::AtomicU64::new(10_000_000),
            nonce: std::sync::atomic::AtomicU64::new(3),
            timestamp: std::sync::atomic::AtomicU64::new(100),
            funded: std::sync::atomic::AtomicBool::new(true),
            unsigned_simulations: AtomicUsize::new(0),
        });
        let runtime = EvmRuntime::new(rpc.clone(), 1, Some(signer.clone()));
        let permit = if permit2_mode {
            AuthorizedPermit::Permit2Single(permit2(lower_address(owner.address())))
        } else {
            AuthorizedPermit::Eip2612(eip_permit(lower_address(owner.address())))
        };
        let request = || {
            let mut request = unsigned_permit_tx(owner.address(), Bytes::from(vec![0; 65]));
            request.maximum_total_fee = U256::from(100_000_000_000_000u64);
            request
        };
        let template = runtime
            .prepare_permit_template(request(), 0, permit.clone())
            .await
            .unwrap();
        let original = serde_json::to_value(&template).unwrap();
        // These changes used to either rebuild the bid or compare a new 2*base bid.
        rpc.base.store(105_000_000, Ordering::SeqCst);
        rpc.priority.store(90_000_000, Ordering::SeqCst);
        let fresh = runtime
            .revalidate_permit_template(
                &template,
                request(),
                SignatureLayout::Packed65 { offset: 0 },
                permit.clone(),
            )
            .await
            .unwrap();
        assert_eq!(serde_json::to_value(fresh).unwrap(), original);
        rpc.base.store(1_000_000_000, Ordering::SeqCst); // Base alone exceeds the frozen cap.
        assert_eq!(
            runtime
                .revalidate_permit_template(
                    &template,
                    request(),
                    SignatureLayout::Packed65 { offset: 0 },
                    permit.clone()
                )
                .await
                .unwrap_err()
                .code,
            "FEE_BOUND_CHANGED"
        );
        rpc.base.store(105_000_000, Ordering::SeqCst);
        rpc.nonce.store(4, Ordering::SeqCst);
        assert_eq!(
            runtime
                .revalidate_permit_template(
                    &template,
                    request(),
                    SignatureLayout::Packed65 { offset: 0 },
                    permit.clone()
                )
                .await
                .unwrap_err()
                .code,
            "NONCE_CHANGED"
        );
        rpc.nonce.store(3, Ordering::SeqCst);
        rpc.funded.store(false, Ordering::SeqCst);
        assert_eq!(
            runtime
                .revalidate_permit_template(
                    &template,
                    request(),
                    SignatureLayout::Packed65 { offset: 0 },
                    permit.clone()
                )
                .await
                .unwrap_err()
                .code,
            "INSUFFICIENT_GAS_BALANCE"
        );
        rpc.funded.store(true, Ordering::SeqCst);
        let mut reduced_cap = request();
        reduced_cap.maximum_total_fee =
            template.transaction().maximum_cost().unwrap() - U256::from(1);
        assert_eq!(
            runtime
                .revalidate_permit_template(
                    &template,
                    reduced_cap,
                    SignatureLayout::Packed65 { offset: 0 },
                    permit.clone()
                )
                .await
                .unwrap_err()
                .code,
            "FEE_LIMIT"
        );
        rpc.timestamp.store(200, Ordering::SeqCst);
        assert_eq!(
            runtime
                .revalidate_permit_template(
                    &template,
                    request(),
                    SignatureLayout::Packed65 { offset: 0 },
                    permit.clone()
                )
                .await
                .unwrap_err()
                .code,
            "INVALID_AUTHORIZED_PERMIT"
        );
        rpc.timestamp.store(100, Ordering::SeqCst);
        let mut tampered = request();
        tampered.calldata = Bytes::from(vec![1; 65]);
        assert_eq!(
            runtime
                .revalidate_permit_template(
                    &template,
                    tampered,
                    SignatureLayout::Packed65 { offset: 0 },
                    permit.clone()
                )
                .await
                .unwrap_err()
                .code,
            "PERMIT_TRANSACTION_MISMATCH"
        );
        assert_eq!(
            runtime
                .revalidate_permit_template(
                    &template,
                    request(),
                    SignatureLayout::Packed65 { offset: 1 },
                    permit
                )
                .await
                .unwrap_err()
                .code,
            "PERMIT_STATE_CHANGED"
        );
        assert_eq!(rpc.unsigned_simulations.load(Ordering::SeqCst), 0);
        assert_eq!(signer.0.load(Ordering::SeqCst), 0);
    }
}
