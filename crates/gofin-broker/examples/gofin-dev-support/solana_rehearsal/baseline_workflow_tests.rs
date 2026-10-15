use super::*;
use gofin_domain::{
    Adapter, ApprovalAudit, ConnectionMetadata, Definition, ExecutionActor, OperationKind,
    OperationRecord, PreparedOperation, Reconciliation, ResourceId, SnapshotScope, StateSnapshot,
    Submission, SubmissionJournal,
};
use gofin_protocol::BrokerRequest;
use std::sync::atomic::{AtomicUsize, Ordering};

struct WalletInventoryTripwire {
    identity: ConnectionMetadata,
    scoped_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Adapter for WalletInventoryTripwire {
    fn definitions(&self) -> Vec<Definition> {
        vec![Definition {
            name: "synthetic.wallet-inventory-tripwire".into(),
            description: "Fails if an exact prepared baseline falls back to wallet inventory."
                .into(),
            kind: OperationKind::Read,
            execution_actor: ExecutionActor::AgentInvocable,
            input_schema: json!({"type":"object"}),
            output_schema: json!({"type":"object"}),
            error_schema: json!({"type":"object"}),
            examples: vec![],
            coverage: "synthetic tripwire".into(),
            completion: "synthetic tripwire".into(),
            semantics: json!({}),
        }]
    }

    fn snapshot_identities(&self) -> Vec<ConnectionMetadata> {
        vec![self.identity.clone()]
    }

    async fn snapshot(&self) -> Result<StateSnapshot> {
        self.scoped_calls.fetch_add(1, Ordering::SeqCst);
        Err(GoFinError::new(
            "WHOLE_WALLET_INVENTORY_USED",
            "snapshot",
            "Prepared account evidence should have satisfied this rent baseline.",
            "Inspect the broker-owned prepared baseline.",
        ))
    }

    async fn snapshot_scoped(&self, scope: &SnapshotScope) -> Result<StateSnapshot> {
        if scope.resources != vec![self.identity.resource.clone()] {
            return Err(GoFinError::validation("Unexpected inventory scope."));
        }
        self.snapshot().await
    }

    async fn read(&self, _: &str, _: &Value) -> Result<Value> {
        Err(GoFinError::validation("Unused inventory read."))
    }

    async fn prepare(&self, _: &str, _: &Value) -> Result<PreparedOperation> {
        Err(GoFinError::validation("Unused inventory prepare."))
    }

    async fn revalidate(&self, _: &PreparedOperation) -> Result<PreparedOperation> {
        Err(GoFinError::validation("Unused inventory revalidate."))
    }

    async fn submit(
        &self,
        _: &PreparedOperation,
        _: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        Err(GoFinError::validation("Unused inventory submit."))
    }

    async fn reconcile(&self, _: &OperationRecord) -> Result<Reconciliation> {
        Err(GoFinError::validation("Unused inventory reconcile."))
    }
}

struct CountingAuthorizer {
    approvals: Arc<AtomicUsize>,
}

#[async_trait]
impl gofin_domain::Authorizer for CountingAuthorizer {
    async fn authorize(
        &self,
        challenge: &gofin_domain::AuthorizationChallenge,
    ) -> Result<gofin_domain::ApprovalAudit> {
        self.approvals.fetch_add(1, Ordering::SeqCst);
        crate::validate_development_challenge(challenge)?;
        Ok(ApprovalAudit {
            mechanism: "development-auto-approval-no-os-authentication".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: gofin_domain::now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

fn runtime_with_inventory_tripwire(
    config: gofin_broker::BrokerConfigV2,
    applications: gofin_broker::ProtectedApplications,
    directory: &tempfile::TempDir,
    wallet: &str,
) -> (
    Arc<gofin_broker::BrokerRuntime>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let observer_calls = Arc::new(AtomicUsize::new(0));
    let approval_calls = Arc::new(AtomicUsize::new(0));
    let resource = ResourceId(format!("solana:mainnet-beta:{wallet}"));
    let mut adapters = applications.adapters;
    adapters.push(Arc::new(WalletInventoryTripwire {
        identity: ConnectionMetadata {
            name: "synthetic-whole-wallet-inventory".into(),
            provider: "synthetic".into(),
            resource,
            role: Some("wallet".into()),
            parent: None,
            network: Some("solana".into()),
            address: Some(wallet.to_owned()),
            credential_ref: None,
            signing_enabled: false,
            details: json!({"cluster":"mainnet-beta"}),
        },
        scoped_calls: observer_calls.clone(),
    }));
    let runtime = gofin_broker::compose_runtime(
        config,
        gofin_broker::RuntimeDependencies {
            store: gofin_store::Store::open(&directory.path().join("operations.sqlite")).unwrap(),
            adapters,
            authorizer: Arc::new(CountingAuthorizer {
                approvals: approval_calls.clone(),
            }),
            valuator: applications.valuator,
            bridge: applications.bridge,
        },
    )
    .unwrap();
    (runtime, observer_calls, approval_calls)
}

#[tokio::test]
async fn prepared_existing_wsol_rent_anchor_skips_full_wallet_inventory_and_settles_once() {
    let _fixture = FIXTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (config, applications, rpc, _) = applications_with_fixture().unwrap();
    let wallet = rpc.0.lock().unwrap().wallet.clone();
    let resource = ResourceId(format!("solana:mainnet-beta:{wallet}"));
    let directory = tempfile::tempdir().unwrap();
    let (runtime, observer_calls, approval_calls) =
        runtime_with_inventory_tripwire(config, applications, &directory, &wallet);

    let response = runtime
        .handle(BrokerRequest::Execute {
            request: gofin_domain::OperationRequest {
                request_id: "prepared-wsol-rent-baseline".into(),
                operation: "jupiter.swap".into(),
                input: json!({
                    "wallet":"solana-rehearsal",
                    "network":"solana",
                    "cluster":"mainnet-beta",
                    "input_mint":SWAP_OUTPUT_MINT,
                    "output_mint":"native",
                    "amount_atoms":"1000000",
                    "minimum_output_atoms":"9950000",
                    "maximum_slippage_bps":50,
                    "maximum_platform_fee_bps":0,
                    "maximum_network_fee_lamports":10000,
                    "maximum_account_funding_lamports":0,
                    "route":"router"
                }),
                run_id: None,
            },
        })
        .await;
    assert!(response.ok, "execution response: {:?}", response.error);
    let result = response.result.expect("settled operation result");
    assert_eq!(result["status"], "completed", "{result}");
    let operation_id = result["id"].as_str().expect("operation id");
    let status = runtime
        .handle(BrokerRequest::Status {
            id: operation_id.to_owned(),
        })
        .await;
    assert!(status.ok, "status response: {:?}", status.error);
    let record = status.result.expect("operation record");
    let prepared = &record["preview"]["binding"][gofin_domain::BASELINE_ANCHORS_KEY];
    let anchors: Vec<gofin_domain::PositionAnchor> =
        serde_json::from_value(prepared.clone()).expect("exact broker-prepared baseline rows");
    assert_eq!(anchors.len(), 1);
    assert_eq!(anchors[0].resource, resource);
    assert_eq!(
        anchors[0].position_kind,
        gofin_domain::PositionKind::RefundableAccountFunding
    );
    assert_eq!(
        anchors[0].principal.atoms,
        gofin_domain::Quantity::from_u64(2_039_280)
    );
    assert_eq!(
        anchors[0].evidence["observation"]["commitment"],
        "finalized"
    );
    assert_eq!(anchors[0].evidence["observation"]["account_existed"], true);
    assert_eq!(
        anchors[0].evidence["observation"]["account_absence_proven"],
        false
    );
    assert_eq!(
        anchors[0].observed_at,
        anchors[0].evidence["observed_at"].as_str().unwrap()
    );
    assert_eq!(anchors[0].evidence["observation"]["context_slot"], 100);
    assert_eq!(observer_calls.load(Ordering::SeqCst), 0);
    assert_eq!(approval_calls.load(Ordering::SeqCst), 1);
    let state = rpc.0.lock().unwrap();
    assert_eq!(state.transactions.len(), 1);
    let wsol_ata = gofin_solana::associated_token_address(
        &wallet,
        SWAP_INPUT_MINT,
        gofin_solana::token::TOKEN_PROGRAM_ID,
    )
    .unwrap();
    assert_eq!(
        state
            .get_account_info_addresses
            .iter()
            .filter(|address| *address == &wsol_ata)
            .count(),
        2,
        "read exact finalized WSOL state once during prepare and once during fresh revalidation"
    );
    let effects = record["effects"]["effects"].as_array().unwrap();
    assert_eq!(
        effects.len(),
        5,
        "all wallet effects are explicitly accounted"
    );
    assert!(effects.iter().any(|effect| {
        effect["kind"] == "conversion"
            && effect["spent"]["atoms"] == "1000000"
            && effect["received"]["atoms"] == "10000000"
    }));
    assert!(effects.iter().any(|effect| {
        effect["kind"] == "cost"
            && effect["category"] == "solana_network_fee"
            && effect["amount"]["atoms"] == "5000"
    }));
    assert!(effects.iter().any(|effect| {
        effect["kind"] == "usable_credit" && effect["amount"]["atoms"] == "2039280"
    }));
    assert!(effects.iter().any(|effect| {
        effect["kind"] == "position_change"
            && effect["position"] == anchors[0].position
            && effect["delta"] == "-2039280"
    }));
    drop(state);
    let repeated_status = runtime
        .handle(BrokerRequest::Status {
            id: operation_id.to_owned(),
        })
        .await;
    assert!(repeated_status.ok);
    assert_eq!(rpc.0.lock().unwrap().transactions.len(), 1);
    assert_eq!(approval_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn wallet_paid_new_output_ata_has_exact_zero_anchor_without_inventory_scan() {
    let _fixture = FIXTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (config, applications, rpc, _) = applications_with_fixture().unwrap();
    let wallet = rpc.0.lock().unwrap().wallet.clone();
    let output_ata = gofin_solana::associated_token_address(
        &wallet,
        SWAP_OUTPUT_MINT,
        gofin_solana::token::TOKEN_PROGRAM_ID,
    )
    .unwrap();
    let input_ata = gofin_solana::associated_token_address(
        &wallet,
        PYUSD_MINT,
        gofin_solana::token::TOKEN_2022_PROGRAM_ID,
    )
    .unwrap();
    let (native_before, input_before, usdc_before, wsol_before) = {
        let state = rpc.0.lock().unwrap();
        (
            state.lamports,
            state.token_amounts[&input_ata],
            state.token_amounts[&output_ata],
            state.token_amounts[&gofin_solana::associated_token_address(
                &wallet,
                SWAP_INPUT_MINT,
                gofin_solana::token::TOKEN_PROGRAM_ID,
            )
            .unwrap()],
        )
    };
    {
        let mut state = rpc.0.lock().unwrap();
        state.account_data.remove(&output_ata);
        state.token_amounts.remove(&output_ata);
    }
    let directory = tempfile::tempdir().unwrap();
    let (runtime, observer_calls, approval_calls) =
        runtime_with_inventory_tripwire(config, applications, &directory, &wallet);
    let response = runtime
        .handle(BrokerRequest::Execute {
            request: gofin_domain::OperationRequest {
                request_id: "prepared-new-ata-rent-baseline".into(),
                operation: "jupiter.swap".into(),
                input: json!({
                    "wallet":"solana-rehearsal",
                    "network":"solana",
                    "cluster":"mainnet-beta",
                    "input_mint":PYUSD_MINT,
                    "output_mint":SWAP_OUTPUT_MINT,
                    "amount_atoms":"1000000",
                    "minimum_output_atoms":"995000",
                    "maximum_slippage_bps":50,
                    "maximum_platform_fee_bps":0,
                    "maximum_network_fee_lamports":10000,
                    "maximum_account_funding_lamports":2039280,
                    "route":"router"
                }),
                run_id: None,
            },
        })
        .await;
    assert!(response.ok, "execution response: {:?}", response.error);
    let result = response.result.expect("completed operation result");
    assert_eq!(result["status"], "completed", "{result}");
    let status = runtime
        .handle(BrokerRequest::Status {
            id: result["id"].as_str().unwrap().to_owned(),
        })
        .await;
    assert!(status.ok);
    let record = status.result.unwrap();
    let anchors: Vec<gofin_domain::PositionAnchor> = serde_json::from_value(
        record["preview"]["binding"][gofin_domain::BASELINE_ANCHORS_KEY].clone(),
    )
    .unwrap();
    assert_eq!(anchors.len(), 1);
    assert_eq!(anchors[0].position, format!("account-funding:{output_ata}"));
    assert_eq!(anchors[0].principal.atoms, gofin_domain::Quantity::zero());
    assert_eq!(anchors[0].evidence["observation"]["account_existed"], false);
    assert_eq!(
        anchors[0].evidence["observation"]["account_absence_proven"],
        true
    );
    assert_eq!(
        anchors[0].evidence["observation"]["commitment"],
        "finalized"
    );
    assert_eq!(anchors[0].evidence["observation"]["context_slot"], 100);
    assert_eq!(anchors[0].evidence["observation"]["payer"], wallet);
    assert_eq!(
        anchors[0].evidence["observation"]["rent_lamports"],
        2_039_280
    );
    assert_eq!(observer_calls.load(Ordering::SeqCst), 0);
    assert_eq!(approval_calls.load(Ordering::SeqCst), 1);
    let state = rpc.0.lock().unwrap();
    assert_eq!(state.transactions.len(), 1);
    assert_eq!(state.token_amounts[&input_ata], input_before - 1_000_000);
    assert_eq!(usdc_before, 100_000_000);
    assert_eq!(state.token_amounts[&output_ata], 1_000_000);
    assert_eq!(state.lamports, native_before - 2_044_280);
    assert_eq!(
        state.token_amounts[&gofin_solana::associated_token_address(
            &wallet,
            SWAP_INPUT_MINT,
            gofin_solana::token::TOKEN_PROGRAM_ID,
        )
        .unwrap()],
        wsol_before,
        "the unrelated wrapped-SOL position remains unchanged"
    );
    let effects = record["effects"]["effects"].as_array().unwrap();
    assert!(effects.iter().any(|effect| {
        effect["kind"] == "conversion"
            && effect["spent"]["atoms"] == "1000000"
            && effect["received"]["atoms"] == "1000000"
    }));
    assert!(effects.iter().any(|effect| {
        effect["kind"] == "position_change"
            && effect["position"] == anchors[0].position
            && effect["delta"] == "2039280"
    }));
    assert!(effects.iter().any(|effect| {
        effect["kind"] == "cost"
            && effect["category"] == "solana_network_fee"
            && effect["amount"]["atoms"] == "5000"
    }));
    assert_eq!(
        state
            .get_account_info_addresses
            .iter()
            .filter(|address| *address == &output_ata)
            .count(),
        2,
        "read exact finalized ATA state once during prepare and once during fresh revalidation"
    );
}
