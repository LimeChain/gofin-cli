//! Internal execution mechanics for the trusted Uniswap API adapter. Financial
//! intent is checked against provider quote metadata by the adapter; opaque
//! transaction calldata is never decoded or simulated here.
use super::*;
use alloy_dyn_abi::TypedData;

/// Provider-supplied EIP-712 data bound to the broker's wallet and quote expiry.
/// This is an internal adapter value, not an arbitrary-sign CLI operation.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TrustedUniswapTypedData {
    pub chain_id: u64,
    pub owner: String,
    pub expires_at: u64,
    pub typed_data: Value,
}

impl TrustedUniswapTypedData {
    fn parsed(&self) -> Result<TypedData> {
        let data: TypedData = serde_json::from_value(self.typed_data.clone())
            .map_err(|_| permit_error("Uniswap returned malformed EIP-712 typed data."))?;
        if self.chain_id == 0
            || data.domain.chain_id != Some(U256::from(self.chain_id))
            || data.domain.name.as_deref() != Some("Permit2")
            || data.domain.version.is_some()
            || data.domain.salt.is_some()
            || data.domain.verifying_contract
                != Some(address("0x000000000022D473030F116dDEE9F6B43aC78BA3")?)
            || !matches!(
                data.primary_type.as_str(),
                "PermitSingle" | "PermitWitnessTransferFrom"
            )
        {
            return Err(permit_error(
                "Uniswap typed data has an unsupported Permit2 domain, chain or primary type.",
            ));
        }
        let owner = nonzero_permit_address(&self.owner)?;
        if let Some(message_owner) = data.message.get("owner")
            && address(message_owner.as_str().unwrap_or(""))? != owner
        {
            return Err(permit_error(
                "Uniswap typed data owner differs from the configured wallet.",
            ));
        }
        let deadline = if data.primary_type == "PermitSingle" {
            &data.message["sigDeadline"]
        } else {
            &data.message["deadline"]
        };
        let deadline = decimal_u64(deadline)?;
        if self.expires_at == 0 || self.expires_at == u64::MAX || deadline != self.expires_at {
            return Err(permit_error(
                "Uniswap typed data must bind the exact finite signature expiry.",
            ));
        }
        Ok(data)
    }

    pub fn validate(&self, chain_id: u64, now: u64) -> Result<()> {
        if chain_id != self.chain_id || self.expires_at <= now {
            return Err(permit_error(
                "Uniswap typed data has expired or belongs to a different chain.",
            ));
        }
        self.parsed()?
            .eip712_signing_hash()
            .map_err(|_| permit_error("Uniswap typed data cannot be encoded as EIP-712."))?;
        Ok(())
    }

    /// Identity calculation is unsigned and safe to journal before dispatch.
    pub fn digest(&self) -> Result<B256> {
        self.parsed()?
            .eip712_signing_hash()
            .map_err(|_| permit_error("Uniswap typed data cannot be encoded as EIP-712."))
    }
}

fn decimal_u64(value: &Value) -> Result<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .ok_or_else(|| permit_error("Uniswap typed data expiry must be a finite uint64."))
}

/// A trusted provider's opaque payload and gas estimate. The broker supplies
/// the wallet and explicit total fee limit; the runtime owns nonce and fee bid.
#[derive(Clone, Debug)]
pub struct ProviderTransactionRequest {
    pub sender: Address,
    pub to: Address,
    pub value: U256,
    pub calldata: Bytes,
    pub gas_limit: u64,
    pub maximum_total_fee: U256,
}

impl EvmRuntime {
    /// Sign provider Permit2 data only while the operation journal authorizes it.
    pub async fn sign_uniswap_typed_data(
        &self,
        permit: &TrustedUniswapTypedData,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Bytes> {
        journal.assert_authorized()?;
        self.assert_chain().await?;
        let latest = self
            .rpc
            .request("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        permit.validate(self.chain_id, rpc_u64(&latest["timestamp"])?)?;
        let signer = self.signer.as_ref().ok_or_else(|| {
            error(
                "PROTECTED_SIGNER_REQUIRED",
                "signing",
                "No protected signing provider is configured.",
            )
        })?;
        journal.assert_authorized()?;
        let bytes = signer.sign_uniswap_typed_data(permit).await?;
        if bytes.len() != 65 || !matches!(bytes[64], 27 | 28) {
            return Err(permit_error(
                "Uniswap signer returned a noncanonical signature.",
            ));
        }
        let signature = Signature::from_raw(&bytes)
            .map_err(|_| permit_error("Uniswap signer returned an invalid signature."))?;
        let recovered = signature
            .recover_address_from_prehash(&permit.digest()?)
            .map_err(|_| permit_error("Uniswap signature owner recovery failed."))?;
        if recovered != address(&permit.owner)? {
            return Err(permit_error(
                "Uniswap signature does not recover the authorized owner.",
            ));
        }
        journal.assert_authorized()?;
        self.assert_chain().await?;
        let latest = self
            .rpc
            .request("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        permit.validate(self.chain_id, rpc_u64(&latest["timestamp"])?)?;
        journal.assert_authorized()?;
        Ok(bytes)
    }

    /// Freeze provider calldata without local simulation or ABI interpretation.
    pub async fn prepare_provider_transaction(
        &self,
        request: ProviderTransactionRequest,
    ) -> Result<FrozenTransaction> {
        let ProviderTransactionRequest {
            sender,
            to,
            value,
            calldata,
            gas_limit,
            maximum_total_fee,
        } = request;
        if gas_limit == 0 || to.is_zero() || sender.is_zero() {
            return Err(error(
                "INVALID_ENVELOPE",
                "preview",
                "Provider transaction requires a nonzero sender, recipient and gas limit.",
            ));
        }
        self.assert_chain().await?;
        let ((required, priority), nonce) =
            tokio::try_join!(self.fees(), self.pending_nonce(sender))?;
        let affordable = maximum_total_fee / U256::from(gas_limit);
        if affordable < required {
            return Err(error(
                "FEE_LIMIT",
                "preview",
                "Provider transaction exceeds the explicit total fee bound.",
            ));
        }
        let max_fee = ceil_percent(required, 150)
            .ok_or_else(|| error("U256_OVERFLOW", "preview", "Fee headroom overflowed."))?
            .min(affordable);
        let max_priority = ceil_percent(priority, 150)
            .ok_or_else(|| {
                error(
                    "U256_OVERFLOW",
                    "preview",
                    "Priority fee headroom overflowed.",
                )
            })?
            .min(max_fee);
        let mut transaction = FrozenTransaction {
            chain_id: self.chain_id,
            sender: lower_address(sender),
            to: lower_address(to),
            value: quantity(value),
            calldata: hex_bytes(&calldata),
            nonce,
            gas: gas_limit,
            max_fee_per_gas: quantity(max_fee),
            max_priority_fee_per_gas: quantity(max_priority),
            rollup_fee: None,
        };
        transaction.consensus()?;
        transaction.rollup_fee = self.rollup_fee_bound(&transaction).await?;
        let execution_budget = maximum_total_fee
            .checked_sub(fees::bound_cost(&transaction)?)
            .ok_or_else(|| {
                error(
                    "FEE_LIMIT",
                    "preview",
                    "Provider transaction cannot fund rollup fees within its total limit.",
                )
            })?;
        let max_fee = max_fee.min(execution_budget / U256::from(gas_limit));
        if max_fee < required {
            return Err(error(
                "FEE_LIMIT",
                "preview",
                "Provider transaction cannot fund execution within its total fee limit.",
            ));
        }
        transaction.max_fee_per_gas = quantity(max_fee);
        let observed_base = (required - priority) / U256::from(2);
        transaction.max_priority_fee_per_gas = quantity(max_priority.min(max_fee - observed_base));
        transaction.consensus()?;
        let debit = value
            .checked_add(transaction.maximum_cost()?)
            .ok_or_else(|| {
                error(
                    "U256_OVERFLOW",
                    "preview",
                    "Provider transaction debit overflowed.",
                )
            })?;
        if self.balance(sender, None).await? < debit {
            return Err(error(
                "INSUFFICIENT_GAS_BALANCE",
                "preview",
                "Sender cannot fund provider transaction value and bounded gas.",
            ));
        }
        Ok(transaction)
    }

    /// Recheck the immutable envelope's execution prerequisites, without
    /// simulating or decoding the provider payload.
    pub async fn revalidate_provider_transaction(
        &self,
        transaction: &FrozenTransaction,
    ) -> Result<()> {
        transaction.consensus()?;
        if transaction.chain_id != self.chain_id
            || address(&transaction.to)?.is_zero()
            || address(&transaction.sender)?.is_zero()
        {
            return Err(error(
                "WRONG_CHAIN",
                "submission",
                "Provider transaction has invalid chain or wallet identities.",
            ));
        }
        self.assert_chain().await?;
        let sender = address(&transaction.sender)?;
        let (latest, nonce, balance, ()) = tokio::try_join!(
            self.rpc
                .request("eth_getBlockByNumber", json!(["latest", false])),
            self.pending_nonce(sender),
            self.balance(sender, None),
            self.revalidate_rollup_fee(transaction)
        )?;
        validate_frozen_fee_bid(transaction, rpc_u256(&latest["baseFeePerGas"])?)?;
        if nonce != transaction.nonce {
            return Err(error(
                "NONCE_CHANGED",
                "submission",
                "Provider transaction pending nonce changed.",
            ));
        }
        let debit = u256(&transaction.value)?
            .checked_add(transaction.maximum_cost()?)
            .ok_or_else(|| {
                error(
                    "U256_OVERFLOW",
                    "submission",
                    "Provider transaction debit overflowed.",
                )
            })?;
        if balance < debit {
            return Err(error(
                "INSUFFICIENT_GAS_BALANCE",
                "submission",
                "Sender cannot fund the approved provider transaction.",
            ));
        }
        Ok(())
    }
}

impl EvmRuntime {
    pub async fn submit_provider_transaction_revalidated(
        &self,
        transaction: &FrozenTransaction,
        metadata: Value,
        journal: Arc<dyn SubmissionJournal>,
        validated: &RevalidatedOperation,
    ) -> Result<Submission> {
        self.submit_provider_at_binding(transaction, metadata, journal, validated, "")
            .await
    }

    pub async fn submit_nested_provider_transaction_revalidated(
        &self,
        transaction: &FrozenTransaction,
        metadata: Value,
        journal: Arc<dyn SubmissionJournal>,
        validated: &RevalidatedOperation,
        binding_pointer: &str,
    ) -> Result<Submission> {
        if !binding_pointer.starts_with('/') {
            return Err(GoFinError::validation(
                "A nested provider binding is required.",
            ));
        }
        self.submit_provider_at_binding(transaction, metadata, journal, validated, binding_pointer)
            .await
    }

    async fn submit_provider_at_binding(
        &self,
        transaction: &FrozenTransaction,
        metadata: Value,
        journal: Arc<dyn SubmissionJournal>,
        validated: &RevalidatedOperation,
        binding_pointer: &str,
    ) -> Result<Submission> {
        validated.assert_fresh()?;
        let binding = validated
            .prepared()
            .binding
            .pointer(binding_pointer)
            .ok_or_else(|| {
                error(
                    "BINDING_CHANGED",
                    "submission",
                    "The validated candidate has no provider execution binding.",
                )
            })?;
        validate_provider_binding(transaction, binding)?;
        journal.assert_candidate(validated.prepared())?;
        self.revalidate_provider_transaction(transaction).await?;
        let latest = self
            .rpc
            .request("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        let now = rpc_u64(&latest["timestamp"])?;
        if binding["deadline"]
            .as_u64()
            .is_none_or(|deadline| deadline <= now)
        {
            return Err(error(
                "PREVIEW_EXPIRED",
                "submission",
                "The provider quote has expired before dispatch.",
            ));
        }
        self.submit_validated_envelope(transaction, metadata, journal, validated)
            .await
    }
}

fn validate_provider_binding(transaction: &FrozenTransaction, binding: &Value) -> Result<()> {
    let invalid = || {
        error(
            "BINDING_CHANGED",
            "submission",
            "Provider transaction differs from the final approved quote and execution bounds.",
        )
    };
    if binding["provider"] != "uniswap_api_v1"
        || binding["chain_id"].as_u64() != Some(transaction.chain_id)
        || address(binding["owner"].as_str().unwrap_or(""))? != address(&transaction.sender)?
        || binding["token"]["network"].as_str()
            != Some(format!("ethereum:{}", transaction.chain_id).as_str())
    {
        return Err(invalid());
    }
    let maximum_fee: Quantity =
        serde_json::from_value(binding["maximum_fee"].clone()).map_err(|_| invalid())?;
    if transaction.maximum_cost()? > u256(&maximum_fee)? {
        return Err(invalid());
    }
    let maximum_value = if binding["token"]["reference"] == "native" {
        let units: Quantity =
            serde_json::from_value(binding["units"].clone()).map_err(|_| invalid())?;
        u256(&units)?
    } else {
        U256::ZERO
    };
    if u256(&transaction.value)? > maximum_value {
        return Err(invalid());
    }
    if let Some(expected) = binding.get("transaction").filter(|t| !t.is_null())
        && expected != &serde_json::to_value(transaction).map_err(|_| invalid())?
    {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    const KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    fn owner() -> String {
        lower_address(
            LocalEnvelopeSigner::from_private_key(KEY)
                .unwrap()
                .address(),
        )
    }
    fn typed() -> TrustedUniswapTypedData {
        TrustedUniswapTypedData {
            chain_id: 1,
            owner: owner(),
            expires_at: 200,
            typed_data: json!({
                "domain":{"name":"Permit2","chainId":1,"verifyingContract":"0x000000000022D473030F116dDEE9F6B43aC78BA3"},
                "primaryType":"PermitSingle",
                "types":{
                    "PermitSingle":[{"name":"details","type":"PermitDetails"},{"name":"spender","type":"address"},{"name":"sigDeadline","type":"uint256"}],
                    "PermitDetails":[{"name":"token","type":"address"},{"name":"amount","type":"uint160"},{"name":"expiration","type":"uint48"},{"name":"nonce","type":"uint48"}]
                },
                "message":{"details":{"token":"0x0000000000000000000000000000000000000002","amount":"100","expiration":200,"nonce":7},"spender":"0x0000000000000000000000000000000000000003","sigDeadline":200}
            }),
        }
    }

    #[test]
    fn provider_permit_hash_matches_existing_canonical_permit2() {
        let data = typed();
        data.validate(1, 100).unwrap();
        let canonical = Permit2PermitSingle {
            chain_id: 1,
            owner: owner(),
            verifying_contract: "0x000000000022D473030F116dDEE9F6B43aC78BA3".into(),
            token: "0x0000000000000000000000000000000000000002".into(),
            amount: Quantity::from_u64(100),
            expiration: 200,
            nonce: 7,
            spender: "0x0000000000000000000000000000000000000003".into(),
            sig_deadline: 200,
        };
        assert_eq!(data.digest().unwrap(), canonical.digest().unwrap());
        assert!(data.validate(2, 100).is_err());
        assert!(data.validate(1, 200).is_err());
        for (pointer, replacement) in [
            ("/domain/name", json!("Other")),
            ("/domain/chainId", json!(2)),
            (
                "/domain/verifyingContract",
                json!("0x0000000000000000000000000000000000000002"),
            ),
            ("/primaryType", json!("PermitBatch")),
            ("/message/sigDeadline", json!(201)),
        ] {
            let mut changed = data.clone();
            *changed.typed_data.pointer_mut(pointer).unwrap() = replacement;
            assert!(changed.digest().is_err(), "accepted {pointer}");
        }
    }

    #[test]
    fn provider_witness_signature_commits_to_order_data() {
        let mut data = typed();
        data.typed_data["primaryType"] = json!("PermitWitnessTransferFrom");
        data.typed_data["types"] = json!({
            "PermitWitnessTransferFrom":[{"name":"permitted","type":"TokenPermissions"},{"name":"spender","type":"address"},{"name":"nonce","type":"uint256"},{"name":"deadline","type":"uint256"},{"name":"witness","type":"Order"}],
            "TokenPermissions":[{"name":"token","type":"address"},{"name":"amount","type":"uint256"}],
            "Order":[{"name":"recipient","type":"address"},{"name":"minimumOutput","type":"uint256"}]
        });
        data.typed_data["message"] = json!({"permitted":{"token":"0x0000000000000000000000000000000000000002","amount":"100"},"spender":"0x0000000000000000000000000000000000000003","nonce":"7","deadline":"200","witness":{"recipient":owner(),"minimumOutput":"90"}});
        data.validate(1, 100).unwrap();
        let original = data.digest().unwrap();
        data.typed_data["message"]["witness"]["minimumOutput"] = json!("91");
        assert_ne!(original, data.digest().unwrap());
    }

    struct Rpc {
        timestamp: AtomicU64,
        nonce: AtomicU64,
        methods: Mutex<Vec<String>>,
    }
    impl Default for Rpc {
        fn default() -> Self {
            Self {
                timestamp: AtomicU64::new(100),
                nonce: AtomicU64::new(7),
                methods: Mutex::new(vec![]),
            }
        }
    }
    #[async_trait]
    impl EvmRpc for Rpc {
        async fn request(&self, method: &str, _params: Value) -> Result<Value> {
            self.methods.lock().unwrap().push(method.to_owned());
            Ok(match method {
                "eth_chainId" => json!("0x1"),
                "eth_getBlockByNumber" => {
                    json!({"timestamp":format!("0x{:x}",self.timestamp.load(Ordering::SeqCst)),"baseFeePerGas":"0xa"})
                }
                "eth_getTransactionCount" => {
                    json!(format!("0x{:x}", self.nonce.load(Ordering::SeqCst)))
                }
                "eth_maxPriorityFeePerGas" => json!("0x1"),
                "eth_getBalance" => json!("0xffffffffffffffff"),
                _ => panic!("unexpected provider execution RPC: {method}"),
            })
        }
    }
    struct Journal(AtomicBool);
    impl SubmissionJournal for Journal {
        fn assert_authorized(&self) -> Result<()> {
            if self.0.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(error(
                    "DENIED",
                    "signing",
                    "Synthetic authorization expired.",
                ))
            }
        }
        fn persist_submission(&self, _: &Submission) -> Result<()> {
            panic!("no submission expected")
        }
    }
    struct Signer {
        rpc: Arc<Rpc>,
        journal: Arc<Journal>,
        expire: bool,
    }
    #[async_trait]
    impl EnvelopeSigner for Signer {
        async fn sign_prepared(&self, _: &FrozenTransaction) -> Result<Bytes> {
            panic!("no transaction signature expected")
        }
        async fn sign_uniswap_typed_data(&self, permit: &TrustedUniswapTypedData) -> Result<Bytes> {
            let signature = LocalEnvelopeSigner::from_private_key(KEY)
                .unwrap()
                .sign_uniswap_typed_data(permit)
                .await?;
            if self.expire {
                self.rpc.timestamp.store(200, Ordering::SeqCst);
            } else {
                self.journal.0.store(false, Ordering::SeqCst);
            }
            Ok(signature)
        }
    }

    #[tokio::test]
    async fn provider_signatures_require_authority_and_freshness_after_signing() {
        for expire in [false, true] {
            let rpc = Arc::new(Rpc::default());
            let journal = Arc::new(Journal(AtomicBool::new(true)));
            let signer = Arc::new(Signer {
                rpc: rpc.clone(),
                journal: journal.clone(),
                expire,
            });
            let runtime = EvmRuntime::new(rpc, 1, Some(signer));
            assert!(
                runtime
                    .sign_uniswap_typed_data(&typed(), journal)
                    .await
                    .is_err()
            );
        }
        let runtime = EvmRuntime::new(
            Arc::new(Rpc::default()),
            1,
            Some(Arc::new(
                LocalEnvelopeSigner::from_private_key(KEY).unwrap(),
            )),
        );
        let signature = runtime
            .sign_uniswap_typed_data(&typed(), Arc::new(Journal(AtomicBool::new(true))))
            .await
            .unwrap();
        assert_eq!(
            Signature::from_raw(&signature)
                .unwrap()
                .recover_address_from_prehash(&typed().digest().unwrap())
                .unwrap(),
            address(&owner()).unwrap()
        );
        let mut wrong_owner = typed();
        wrong_owner.owner = "0x0000000000000000000000000000000000000002".into();
        assert!(
            runtime
                .sign_uniswap_typed_data(&wrong_owner, Arc::new(Journal(AtomicBool::new(true))))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn opaque_provider_transaction_preserves_fee_nonce_and_binding_without_simulation() {
        let rpc = Arc::new(Rpc::default());
        let runtime = EvmRuntime::new(rpc.clone(), 1, None);
        let tx = runtime
            .prepare_provider_transaction(ProviderTransactionRequest {
                sender: address(&owner()).unwrap(),
                to: address("0x0000000000000000000000000000000000000002").unwrap(),
                value: U256::ZERO,
                calldata: Bytes::from_static(b"opaque provider data"),
                gas_limit: 30_000,
                maximum_total_fee: U256::from(1_000_000_000_000u64),
            })
            .await
            .unwrap();
        runtime.revalidate_provider_transaction(&tx).await.unwrap();
        assert!(tx.maximum_cost().unwrap() <= U256::from(1_000_000_000_000u64));
        let mut binding = json!({"provider":"uniswap_api_v1","chain_id":1,"owner":owner(),"token":{"network":"ethereum:1","reference":"0x0000000000000000000000000000000000000002"},"units":"100","maximum_fee":"1000000000000","transaction":null});
        validate_provider_binding(&tx, &binding).unwrap();
        binding["transaction"] = serde_json::to_value(&tx).unwrap();
        validate_provider_binding(&tx, &binding).unwrap();
        let mut changed = tx.clone();
        changed.calldata = "0x11".into();
        assert!(validate_provider_binding(&changed, &binding).is_err());
        binding["transaction"] = Value::Null;
        changed.value = Quantity::from_u64(1);
        assert!(validate_provider_binding(&changed, &binding).is_err());
        binding["maximum_fee"] = json!("1");
        assert!(validate_provider_binding(&tx, &binding).is_err());
        rpc.nonce.store(8, Ordering::SeqCst);
        assert_eq!(
            runtime
                .revalidate_provider_transaction(&tx)
                .await
                .unwrap_err()
                .code,
            "NONCE_CHANGED"
        );
        assert!(
            !rpc.methods
                .lock()
                .unwrap()
                .iter()
                .any(|m| m == "eth_call" || m == "eth_estimateGas")
        );
    }
}
