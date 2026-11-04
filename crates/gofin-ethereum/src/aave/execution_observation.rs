//! Aave execution observation: transport acquisition followed by pure interpretation.
use super::super::*;
use super::aave_observation::AaveObservation;
use gofin_evm::{CanonicalReceipt, quantity};

#[derive(Clone, Debug)]
struct BlockFact {
    number: u64,
    hash: gofin_evm::alloy::primitives::B256,
}
impl BlockFact {
    fn decode(value: &Value) -> Result<Option<Self>> {
        if value.is_null() {
            return Ok(None);
        }
        Ok(Some(Self {
            number: gofin_evm::rpc_u64(&value["number"])?,
            hash: gofin_evm::hash(value["hash"].as_str().unwrap_or(""))?,
        }))
    }
    fn matches(&self, receipt: &CanonicalReceipt) -> bool {
        self.number == receipt.block_number && self.hash == receipt.block_hash
    }
}
struct InclusionFacts {
    receipt: CanonicalReceipt,
    finalized: Option<BlockFact>,
    canonical_before: Option<BlockFact>,
    financial: Option<Box<AaveObservation>>,
    canonical_after: Option<BlockFact>,
}
enum ObservationState {
    Missing {
        replacement: Option<Box<gofin_evm::FinalizedNonceReplacement>>,
    },
    Included(Box<InclusionFacts>),
}

pub(crate) struct LifecycleObservation {
    original: OperationRecord,
    binding: Binding,
    native_asset: AssetId,
    finality: String,
    requires_finalized: bool,
    confirmations: u64,
    chain_id: u64,
    pool: String,
    observed_at: String,
    next_check_after: String,
    state: ObservationState,
}

fn original_binding(record: &OperationRecord) -> Result<Binding> {
    require(
        matches!(
            record.operation.as_str(),
            "aave.supply" | "aave.borrow" | "aave.repay" | "aave.withdraw" | "aave.collateral"
        ),
        "OPERATION_UNAVAILABLE",
        "Aave observation requires a supported Aave write.",
    )?;
    let submission = record.submission.as_ref().ok_or_else(|| {
        error(
            "SUBMISSION_REQUIRED",
            "reconciliation",
            "Original durable submission identity is missing.",
        )
    })?;
    let preview = record.preview.as_ref().ok_or_else(|| {
        error(
            "BINDING_REQUIRED",
            "reconciliation",
            "Original preview is missing.",
        )
    })?;
    require(
        preview.operation == record.operation && preview.input == record.input,
        "BINDING_IDENTITY_MISMATCH",
        "Record operation/input differs from original preview.",
    )?;
    let binding: Binding = serde_json::from_value(preview.binding.clone()).map_err(|_| {
        error(
            "BINDING_INVALID",
            "reconciliation",
            "Original transaction binding is malformed.",
        )
    })?;
    let pool = binding.pool.as_deref().ok_or_else(|| {
        error(
            "BINDING_INVALID",
            "reconciliation",
            "Original Aave Pool is missing.",
        )
    })?;
    require(
        address(pool)? == address(&binding.transaction.to)?
            && record.input.get("market").is_none_or(|market| {
                market.as_str().and_then(|value| address(value).ok()) == address(pool).ok()
            }),
        "BINDING_IDENTITY_MISMATCH",
        "Original Pool differs from the frozen destination or explicit market.",
    )?;
    require(
        binding.permit_template.is_none()
            && submission.data["transaction"]
                == serde_json::to_value(&binding.transaction).expect("serializable")
            && submission.data["metadata"]["operation"] == json!(record.operation)
            && submission.data["metadata"]["resource"] == json!(binding.resource),
        "SUBMISSION_IDENTITY_MISMATCH",
        "Durable submission differs from the original financial binding.",
    )?;
    // Decode independently as well: an arbitrary JSON object must never be
    // accepted as a durable execution envelope.
    serde_json::from_value::<FrozenTransaction>(submission.data["transaction"].clone()).map_err(
        |_| {
            error(
                "SUBMISSION_IDENTITY_MISMATCH",
                "reconciliation",
                "Durable transaction envelope is malformed.",
            )
        },
    )?;
    Ok(binding)
}

fn validate_market(binding: &Binding, chain_id: u64, pool: &str) -> Result<()> {
    require(
        binding.transaction.chain_id == chain_id
            && binding.pool.as_deref().map(address).transpose()? == Some(address(pool)?)
            && address(&binding.transaction.to)? == address(pool)?,
        "BINDING_IDENTITY_MISMATCH",
        "Observation market or network differs from the immutable submitted Pool.",
    )
}

impl EthereumAdapter {
    pub(crate) async fn collect_aave_lifecycle_observation(
        &self,
        record: &OperationRecord,
    ) -> Result<LifecycleObservation> {
        let binding = original_binding(record)?;
        // A changed default must not redirect an already submitted operation.
        // Resolve the immutable Pool only within protected enabled markets.
        let mut selection = record.input.clone();
        selection["market"] = json!(binding.pool);
        let selected = self.selected_market(&selection)?;
        let adapter = selected.as_ref().unwrap_or(self);
        let pool = adapter.aave()?.pool.clone();
        validate_market(&binding, adapter.config.chain_id, &pool)?;
        let submission = record.submission.as_ref().expect("validated submission");
        let confirmations =
            if EthereumConfig::external_credit_uses_sequencer(adapter.config.chain_id) {
                1
            } else {
                adapter.config.confirmations
            };
        let state = match adapter
            .runtime
            .receipt(submission, &binding.transaction, confirmations)
            .await?
        {
            None => {
                let replacement = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    adapter
                        .runtime
                        .observe_finalized_nonce_replacement(submission, &binding.transaction),
                )
                .await
                .ok()
                .and_then(Result::ok)
                .flatten();
                ObservationState::Missing {
                    replacement: replacement.map(Box::new),
                }
            }
            Some(receipt) => ObservationState::Included(Box::new(
                adapter
                    .collect_aave_inclusion(record, &binding, receipt)
                    .await?,
            )),
        };
        let observed_at = adapter.clock.now();
        Ok(LifecycleObservation {
            original: record.clone(),
            binding,
            native_asset: adapter.native_asset(),
            finality: adapter.config.finality_label().to_owned(),
            requires_finalized: adapter.config.requires_rpc_finalized(),
            confirmations,
            chain_id: adapter.config.chain_id,
            pool,
            observed_at: observed_at.to_rfc3339(),
            next_check_after: (observed_at + chrono::Duration::seconds(15)).to_rfc3339(),
            state,
        })
    }

    async fn aave_inclusion_header(&self, block: u64) -> Result<Option<BlockFact>> {
        let header = self
            .runtime
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([gofin_evm::block_tag(Some(block)), false]),
            )
            .await?;
        BlockFact::decode(&header)
    }

    async fn collect_aave_inclusion(
        &self,
        record: &OperationRecord,
        binding: &Binding,
        receipt: CanonicalReceipt,
    ) -> Result<InclusionFacts> {
        let mut facts = InclusionFacts {
            receipt,
            finalized: None,
            canonical_before: None,
            financial: None,
            canonical_after: None,
        };
        // Acquisition can stop at a dependency boundary, but returns the actual
        // heights/hashes. Pure evaluation independently decides the outcome.
        if self.config.requires_rpc_finalized() {
            facts.finalized = self
                .runtime
                .rpc
                .request_checked(
                    "eth_getBlockByNumber",
                    json!(["finalized", false]),
                    gofin_evm::RpcReadRequirements {
                        minimum_block: Some(facts.receipt.block_number),
                        ..Default::default()
                    },
                )
                .await
                .ok()
                .and_then(|header| BlockFact::decode(&header).ok().flatten());
            if facts
                .finalized
                .as_ref()
                .is_none_or(|header| header.number < facts.receipt.block_number)
            {
                return Ok(facts);
            }
            facts.canonical_before = self
                .aave_inclusion_header(facts.receipt.block_number)
                .await?;
            if !facts
                .canonical_before
                .as_ref()
                .is_some_and(|header| header.matches(&facts.receipt))
            {
                return Ok(facts);
            }
        }
        if facts.receipt.success {
            facts.financial = Some(Box::new(
                self.collect_aave_receipt_facts(&record.operation, binding, &facts.receipt)
                    .await?,
            ));
        }
        facts.canonical_after = self
            .aave_inclusion_header(facts.receipt.block_number)
            .await?;
        Ok(facts)
    }

    pub(crate) fn evaluate_aave_lifecycle_observation(
        record: &OperationRecord,
        observation: &LifecycleObservation,
    ) -> Result<Reconciliation> {
        require(
            record == &observation.original,
            "BINDING_IDENTITY_MISMATCH",
            "Observation belongs to a different durable operation snapshot.",
        )?;
        let binding = original_binding(record)?;
        require(
            serde_json::to_value(&binding).expect("serializable")
                == serde_json::to_value(&observation.binding).expect("serializable"),
            "BINDING_IDENTITY_MISMATCH",
            "Observation binding differs from the durable financial binding.",
        )?;
        let submission = record.submission.as_ref().expect("validated submission");
        let pending = |canonical: bool, finality: Option<&str>, height: Option<u64>| {
            let mut evidence = json!({"original_hash":submission.reference,"canonical_confirmed":canonical,"next_check_after":observation.next_check_after});
            if let Some(finality) = finality {
                evidence["finality"] = json!(finality);
            }
            if finality == Some("awaiting-rpc-finalized") {
                evidence["finalized_height"] = json!(height);
            }
            Reconciliation {
                status: Status::Pending,
                evidence,
                verified: None,
            }
        };
        validate_market(&binding, observation.chain_id, &observation.pool)?;
        let inclusion = match &observation.state {
            ObservationState::Missing {
                replacement: Some(proof),
            } => {
                return proof.reconcile(
                    submission,
                    &binding.transaction,
                    observation.observed_at.clone(),
                );
            }
            ObservationState::Missing { replacement: None } => {
                return Ok(pending(false, None, None));
            }
            ObservationState::Included(inclusion) => inclusion,
        };
        let receipt = &inclusion.receipt;
        require(
            receipt.hash == gofin_evm::hash(&submission.reference)?
                && address(&receipt.transaction_sender)? == address(&binding.transaction.sender)?,
            "RECEIPT_IDENTITY_INVALID",
            "Observed receipt does not bind the original transaction identity.",
        )?;
        if receipt.confirmations < observation.confirmations {
            return Ok(pending(false, None, None));
        }
        if observation.requires_finalized {
            let height = inclusion.finalized.as_ref().map(|header| header.number);
            if height.is_none_or(|height| height < receipt.block_number) {
                return Ok(pending(true, Some("awaiting-rpc-finalized"), height));
            }
            if !inclusion
                .canonical_before
                .as_ref()
                .is_some_and(|header| header.matches(receipt))
            {
                return Ok(pending(false, Some("inclusion-changed"), None));
            }
        }
        if !inclusion
            .canonical_after
            .as_ref()
            .is_some_and(|header| header.matches(receipt))
        {
            return Ok(pending(false, Some("inclusion-changed"), None));
        }
        let facts = &inclusion.financial;
        if facts
            .as_ref()
            .and_then(|facts| facts.inclusion.as_ref())
            .is_some_and(|anchor| {
                anchor.block != receipt.block_number
                    || anchor.block_hash != format!("{:#x}", receipt.block_hash)
            })
        {
            return Ok(pending(false, Some("inclusion-changed"), None));
        }
        let mut effects = Vec::new();
        if let Some(fee) = receipt.fee {
            effects.push(FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: binding.resource.clone(),
                },
                amount: amount(&observation.native_asset, fee),
                category: "ethereum_gas".into(),
                treatment: CostTreatment::AdditionalDebit,
            });
        }
        let mut details = json!({"block_number":receipt.block_number,"block_hash":format!("{:#x}",receipt.block_hash),"confirmations":receipt.confirmations,"original_hash":submission.reference,"original_envelope_verified":true,"success":receipt.success,"fee_wei":receipt.fee.map(quantity),"finality":observation.finality});
        if receipt.fee.is_none() {
            details["outcome_reporting"] = json!("partial");
        }
        if receipt.success {
            facts
                .as_ref()
                .ok_or_else(|| {
                    error(
                        "AAVE_OBSERVATION_INCOMPLETE",
                        "reconciliation",
                        "Successful Aave receipt is missing financial proof facts.",
                    )
                })?
                .evaluate_success_with_semantics(
                    &record.operation,
                    &binding,
                    &mut effects,
                    &mut details,
                    record
                        .preview
                        .as_ref()
                        .is_some_and(|preview| preview.operation_version == 2),
                )?;
        } else {
            details["outcome_replay_safe"] = json!(true);
        }
        let mut verified = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects,
            evidence: details.clone(),
            observed_at: observation.observed_at.clone(),
            proof_digest: String::new(),
        };
        verified.proof_digest = verified_effect_digest(&verified)?;
        Ok(Reconciliation {
            status: if receipt.success {
                Status::Completed
            } else {
                Status::Failed
            },
            evidence: details,
            verified: Some(verified),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn original() -> (EthereumAdapter, Arc<crate::tests::Fixture>, OperationRecord) {
        let rpc = Arc::new(crate::tests::Fixture::new());
        let adapter = crate::tests::adapter(rpc.clone());
        let mut input = crate::tests::input();
        input["token"] = json!(lower_address(crate::tests::addr(101)));
        input["amount"] = json!("1");
        let preview = adapter.prepare("aave.supply", &input).await.unwrap();
        let binding: Binding = serde_json::from_value(preview.binding.clone()).unwrap();
        let submission = Submission {
            kind: "ethereum".into(),
            reference: format!("0x{}", "34".repeat(32)),
            data: json!({"transaction":binding.transaction,"metadata":{"operation":preview.operation,"resource":binding.resource}}),
        };
        let record = OperationRecord {
            id: "isolated-operation".into(),
            request_id: "isolated-request".into(),
            fingerprint: "isolated".into(),
            fingerprint_version: 1,
            operation: preview.operation.clone(),
            input: preview.input.clone(),
            status: Status::Submitted,
            created_at: now(),
            updated_at: now(),
            preview: Some(preview),
            approval: None,
            authorization_source: None,
            submission: Some(submission),
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        (adapter, rpc, record)
    }

    fn receipt(record: &OperationRecord, success: bool) -> CanonicalReceipt {
        CanonicalReceipt {
            hash: gofin_evm::hash(&record.submission.as_ref().unwrap().reference).unwrap(),
            transaction_sender: original_binding(record).unwrap().transaction.sender,
            block_hash: gofin_evm::hash(&format!("0x{}", "56".repeat(32))).unwrap(),
            block_number: 21,
            confirmations: 2,
            success,
            fee: Some(U256::from(123)),
            receipt: json!({"logs":[]}),
        }
    }
    fn header(receipt: &CanonicalReceipt) -> BlockFact {
        BlockFact {
            number: receipt.block_number,
            hash: receipt.block_hash,
        }
    }
    fn facts(
        adapter: &EthereumAdapter,
        original: &OperationRecord,
        state: ObservationState,
    ) -> LifecycleObservation {
        LifecycleObservation {
            original: original.clone(),
            binding: original_binding(original).unwrap(),
            native_asset: adapter.native_asset(),
            finality: adapter.config.finality_label().into(),
            requires_finalized: true,
            confirmations: 2,
            chain_id: adapter.config.chain_id,
            pool: adapter.aave().unwrap().pool.clone(),
            observed_at: "2026-11-03T09:00:00+00:00".into(),
            next_check_after: "2026-11-03T09:00:15+00:00".into(),
            state,
        }
    }
    fn included(receipt: CanonicalReceipt, height: Option<u64>) -> ObservationState {
        let canonical = header(&receipt);
        ObservationState::Included(Box::new(InclusionFacts {
            finalized: height.map(|number| BlockFact {
                number,
                hash: canonical.hash,
            }),
            canonical_before: Some(canonical.clone()),
            canonical_after: Some(canonical),
            financial: None,
            receipt,
        }))
    }
    #[tokio::test]
    async fn observation_uses_the_immutable_enabled_pool_after_default_changes() {
        let (mut adapter, rpc, record) = original().await;
        let original_market = adapter.config.aave.clone().unwrap();
        adapter.config.aave_markets.push(original_market.clone());
        adapter.config.aave.as_mut().unwrap().pool = lower_address(crate::tests::addr(999));
        let observed = adapter
            .collect_aave_lifecycle_observation(&record)
            .await
            .unwrap();
        assert_eq!(observed.pool, original_market.pool);
        adapter.config.aave_markets.clear();
        rpc.calls.lock().unwrap().clear();
        assert_eq!(
            adapter
                .collect_aave_lifecycle_observation(&record)
                .await
                .err()
                .unwrap()
                .code,
            "AAVE_MARKET_UNAVAILABLE"
        );
        assert!(rpc.calls.lock().unwrap().is_empty());
        let mut malformed = record.clone();
        malformed.input["market"] = json!(lower_address(crate::tests::addr(999)));
        malformed.preview.as_mut().unwrap().input = malformed.input.clone();
        assert_eq!(
            adapter
                .collect_aave_lifecycle_observation(&malformed)
                .await
                .err()
                .unwrap()
                .code,
            "BINDING_IDENTITY_MISMATCH"
        );
        assert!(rpc.calls.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn pure_finality_decision_checks_actual_heights_and_missing_headers_without_io() {
        let (adapter, rpc, record) = original().await;
        rpc.calls.lock().unwrap().clear();
        for height in [None, Some(20), Some(21), Some(22)] {
            let observation = facts(&adapter, &record, included(receipt(&record, false), height));
            for _ in 0..2 {
                let result =
                    EthereumAdapter::evaluate_aave_lifecycle_observation(&record, &observation)
                        .unwrap();
                assert_eq!(
                    result.status,
                    if height.is_some_and(|n| n >= 21) {
                        Status::Failed
                    } else {
                        Status::Pending
                    }
                );
                if result.status == Status::Pending {
                    assert!(result.verified.is_none());
                    assert_eq!(
                        result.evidence["next_check_after"],
                        observation.next_check_after
                    );
                }
            }
        }
        for value in [
            json!(null),
            json!({"number":"0x15","hash":"malformed"}),
            json!({"number":"malformed","hash":format!("0x{}","56".repeat(32))}),
        ] {
            assert!(BlockFact::decode(&value).ok().flatten().is_none());
        }
        assert!(rpc.calls.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn missing_reorg_and_changed_record_never_settle_or_replay() {
        let (adapter, rpc, record) = original().await;
        rpc.calls.lock().unwrap().clear();
        let missing = facts(
            &adapter,
            &record,
            ObservationState::Missing { replacement: None },
        );
        assert_eq!(
            EthereumAdapter::evaluate_aave_lifecycle_observation(&record, &missing)
                .unwrap()
                .status,
            Status::Pending
        );
        for before in [true, false] {
            let mut observation = facts(
                &adapter,
                &record,
                included(receipt(&record, false), Some(22)),
            );
            if let ObservationState::Included(inclusion) = &mut observation.state {
                let target = if before {
                    &mut inclusion.canonical_before
                } else {
                    &mut inclusion.canonical_after
                };
                target.as_mut().unwrap().hash =
                    gofin_evm::hash(&format!("0x{}", "77".repeat(32))).unwrap();
            }
            let result =
                EthereumAdapter::evaluate_aave_lifecycle_observation(&record, &observation)
                    .unwrap();
            assert_eq!(result.status, Status::Pending);
            assert!(result.verified.is_none());
            assert_eq!(result.evidence["finality"], "inclusion-changed");
        }
        let mut changed = record.clone();
        changed.request_id = "replacement-request".into();
        assert_eq!(
            EthereumAdapter::evaluate_aave_lifecycle_observation(&changed, &missing)
                .unwrap_err()
                .code,
            "BINDING_IDENTITY_MISMATCH"
        );
        assert!(rpc.calls.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn failed_receipt_records_only_actual_gas_and_checks_immutable_market() {
        let (adapter, rpc, record) = original().await;
        rpc.calls.lock().unwrap().clear();
        let mut observation = facts(
            &adapter,
            &record,
            included(receipt(&record, false), Some(22)),
        );
        let result =
            EthereumAdapter::evaluate_aave_lifecycle_observation(&record, &observation).unwrap();
        assert_eq!(result.status, Status::Failed);
        let verified = result.verified.unwrap();
        assert_eq!(verified.effects.len(), 1);
        assert!(
            matches!(&verified.effects[0], FinancialEffect::Cost { amount, .. } if amount.atoms == quantity(U256::from(123)))
        );
        assert_eq!(verified.submission, *record.submission.as_ref().unwrap());
        assert_eq!(
            verified.proof_digest,
            verified_effect_digest(&verified).unwrap()
        );
        observation.pool = lower_address(crate::tests::addr(999));
        assert_eq!(
            EthereumAdapter::evaluate_aave_lifecycle_observation(&record, &observation)
                .unwrap_err()
                .code,
            "BINDING_IDENTITY_MISMATCH"
        );
        assert!(rpc.calls.lock().unwrap().is_empty());
    }
}
