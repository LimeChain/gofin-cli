use super::*;
use gofin_domain::ApprovalAudit;
use gofin_protocol::BrokerRequest;
use std::sync::atomic::{AtomicUsize, Ordering};

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
            source: Some(gofin_domain::AuthorizationSource::Development),
            yolo_provenance: None,
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: gofin_domain::now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

fn settlement_runtime(
    config: gofin_broker::BrokerConfigV2,
    applications: gofin_broker::ProtectedApplications,
    directory: &tempfile::TempDir,
) -> (Arc<gofin_broker::BrokerRuntime>, Arc<AtomicUsize>) {
    let approval_calls = Arc::new(AtomicUsize::new(0));
    let runtime = gofin_broker::compose_runtime(
        config,
        gofin_broker::RuntimeDependencies {
            store: gofin_store::Store::open(directory.path().join("operations.sqlite")).unwrap(),
            adapters: applications.adapters,
            authorizer: Arc::new(CountingAuthorizer {
                approvals: approval_calls.clone(),
            }),
            valuator: applications.valuator,
            bridge: applications.bridge,
        },
    )
    .unwrap();
    (runtime, approval_calls)
}

#[tokio::test]
async fn existing_wsol_rent_refund_is_accounted_from_finalized_proof_once() {
    let _fixture = FIXTURE_LOCK.lock().await;
    let (config, applications, rpc, _) = applications_with_fixture().unwrap();
    let wallet = rpc.0.lock().unwrap().wallet.clone();
    let directory = tempfile::tempdir().unwrap();
    let (runtime, approval_calls) = settlement_runtime(config, applications, &directory);

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
                    "maximum_account_funding_lamports":0
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
    assert_eq!(
        record["preview"]["binding"]["wallet_account_funding_lamports"],
        0
    );
    assert_eq!(approval_calls.load(Ordering::SeqCst), 1);
    assert_eq!(rpc.0.lock().unwrap().transactions.len(), 1);
    let wsol_ata = gofin_solana::associated_token_address(
        &wallet,
        SWAP_INPUT_MINT,
        gofin_solana::token::TOKEN_PROGRAM_ID,
    )
    .unwrap();
    let effects = record["effects"]["effects"].as_array().unwrap();
    assert_eq!(
        effects.len(),
        5,
        "all wallet effects are explicitly accounted: {effects:?}"
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
            && effect["position"] == format!("account-funding:{wsol_ata}")
            && effect["delta"] == "-2039280"
    }));
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
async fn wallet_paid_new_output_ata_is_accounted_from_finalized_proof() {
    let _fixture = FIXTURE_LOCK.lock().await;
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
    let (runtime, approval_calls) = settlement_runtime(config, applications, &directory);
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
                    "maximum_account_funding_lamports":2039280
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
    assert_eq!(
        record["preview"]["binding"]["wallet_account_funding_lamports"],
        2_039_280
    );
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
            && effect["position"] == format!("account-funding:{output_ata}")
            && effect["delta"] == "2039280"
    }));
    assert!(effects.iter().any(|effect| {
        effect["kind"] == "cost"
            && effect["category"] == "solana_network_fee"
            && effect["amount"]["atoms"] == "5000"
    }));
    assert_eq!(
        state
            .rpc_method_counts
            .get("simulateTransaction")
            .copied()
            .unwrap_or(0),
        0
    );
    assert_eq!(
        state
            .rpc_method_counts
            .get("getFeeForMessage")
            .copied()
            .unwrap_or(0),
        0
    );
}
