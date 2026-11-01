//! Real broker/engine/store, Jupiter adapter, Kamino adapter and Solana signing/receipt
//! verification, with controlled Jupiter API/RPC and injected Kamino protocol graphs.
//! This tests exact authority and composition, not native Kamino codecs (covered in gofin-kamino).
//! GOFIN_DEV_BUILD_ID=synthetic-kamino-build cargo build -p gofin-cli --example gofin-dev
//! GOFIN_KAMINO_CLI="$PWD/target/debug/examples/gofin-dev" cargo test -p gofin-broker --test kamino_workflow

#[cfg(target_os = "macos")]
#[path = "support/native_approval.rs"]
mod native_approval;

#[path = "support/outcome_evidence.rs"]
mod outcome_evidence;

use async_trait::async_trait;
use gofin_broker::{
    BrokerConfigV2, BrokerPolicy, NetworkConfig, RuntimeDependencies, SharedUsdValuator,
    compose_runtime_with_clock,
};
use gofin_domain::*;
use gofin_kamino::protocol::VaultFamily;
use gofin_protocol::BrokerRequest;
use gofin_store::Store;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
#[path = "support/kamino_workflow.rs"]
mod fixture;
use fixture::*;
const CLIENT_IDENTITY: &str = "synthetic-kamino";
include!("support/kamino_client.rs");
#[derive(Default)]
struct Approval(AtomicUsize);
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "synthetic Kamino workflow".into(),
            source: None,
            yolo_provenance: None,
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
fn runtime(
    path: &Path,
    chain: Arc<Chain>,
    wallet: Arc<gofin_solana::SolanaWalletAdapter>,
    approval: Arc<Approval>,
) -> Arc<gofin_broker::BrokerRuntime> {
    runtime_with_clock(
        path,
        chain,
        wallet,
        approval,
        Arc::new(gofin_domain::clock::SystemClock),
    )
}
fn runtime_with_clock(
    path: &Path,
    chain: Arc<Chain>,
    wallet: Arc<gofin_solana::SolanaWalletAdapter>,
    approval: Arc<Approval>,
    clock: Arc<dyn gofin_domain::clock::Clock>,
) -> Arc<gofin_broker::BrokerRuntime> {
    compose_runtime_with_clock(
        BrokerConfigV2 {
            version: 2,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy: BrokerPolicy::beta(),
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        RuntimeDependencies {
            store: Store::open(path).unwrap(),
            adapters: vec![
                wallet.clone(),
                jupiter(chain.clone(), wallet.clone()),
                kamino(chain, wallet),
            ],
            authorizer: approval,
            valuator: Arc::new(SharedUsdValuator::with_source(Arc::new(Prices))),
            bridge: None,
        },
        clock,
    )
    .unwrap()
}
fn swap(input: &str, output: &str, amount: &str, minimum: &str) -> Value {
    json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","input_mint":input,"output_mint":output,"amount_atoms":amount,"minimum_output_atoms":minimum,"maximum_slippage_bps":100,"maximum_platform_fee_bps":0,"maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0})
}
fn deposit(family: VaultFamily, n: &str) -> Value {
    let mut amounts = vec![json!({"mint":UNDERLYING,"atoms":n})];
    if family == VaultFamily::Liquidity {
        amounts.push(json!({"mint":INPUT,"atoms":"10"}));
    }
    json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","family":family,"vault":vault(family),"share_mint":share(family),"amounts":amounts,"maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0})
}
fn withdrawal(family: VaultFamily) -> Value {
    json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","family":family,"vault":vault(family),"share_mint":share(family),"shares_atoms":"5","maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0})
}
// Public outcome fields are additive; inspect the original typed record separately.
fn typed_record(mut value: Value) -> serde_json::Result<OperationRecord> {
    let fields = [
        "id",
        "request_id",
        "fingerprint",
        "fingerprint_version",
        "operation",
        "input",
        "status",
        "created_at",
        "updated_at",
        "preview",
        "approval",
        "authorization_source",
        "submission",
        "dispatch_evidence",
        "position_baseline",
        "effects",
        "last_reconciliation_evidence",
        "error",
        "run_id",
    ];
    value
        .as_object_mut()
        .unwrap()
        .retain(|key, _| fields.contains(&key.as_str()));
    serde_json::from_value(value)
}
async fn execute(client: &Client, label: &str, operation: &str, input: Value) -> Value {
    let id = format!("kamino-{label}");
    client
        .call(
            args(&[
                "execute",
                operation,
                "--input",
                &input.to_string(),
                "--id",
                &id,
                "--json",
            ]),
            BrokerRequest::Execute {
                request: OperationRequest {
                    request_id: id,
                    operation: operation.into(),
                    input,
                    run_id: None,
                },
            },
            true,
        )
        .await
}
async fn status(client: &Client, record: &Value) -> Value {
    let id = record["id"].as_str().unwrap();
    client
        .call(
            args(&["status", id, "--json"]),
            BrokerRequest::Status { id: id.into() },
            true,
        )
        .await
}
async fn read_position(client: &Client, family: VaultFamily) -> Value {
    let input = json!({"wallet":"wallet","family":family,"vault":vault(family)});
    client
        .call(
            args(&[
                "read",
                "kamino.position",
                "--input",
                &input.to_string(),
                "--json",
            ]),
            BrokerRequest::Read {
                operation: "kamino.position".into(),
                input,
            },
            true,
        )
        .await["position"]
        .clone()
}

#[tokio::test]
async fn new_share_account_funding_settles_with_incomplete_inventory() {
    for family in [VaultFamily::Lending, VaultFamily::Liquidity] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("journal.sqlite");
        let (chain, wallet) = Chain::new();
        chain.seed_balance(UNDERLYING, 200);
        chain.inventory_unavailable.store(true, Ordering::SeqCst);
        let account = chain.remove_empty_share_account(family);
        let initial_lamports = chain.lamports();
        let approval = Arc::new(Approval::default());
        let client = Client::new(
            runtime(&path, chain.clone(), wallet, approval.clone()),
            temp.path(),
        )
        .await;
        let mut input = deposit(family, "100");
        input["maximum_account_funding_lamports"] = json!(ACCOUNT_RENT);
        let preview = client
            .call(
                args(&[
                    "preview",
                    "kamino.deposit",
                    "--input",
                    &input.to_string(),
                    "--json",
                ]),
                BrokerRequest::Preview {
                    operation: "kamino.deposit".into(),
                    input: input.clone(),
                },
                true,
            )
            .await;
        assert_eq!(
            preview["binding"][BASELINE_ANCHORS_KEY]
                .as_array()
                .unwrap()
                .len(),
            1,
            "{preview}"
        );
        assert!(
            preview["intent"]["expected"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["type"] == "optional_account_funding"
                    || row["kind"] == "optional_account_funding"),
            "{preview}"
        );
        assert_eq!(chain.send_count.load(Ordering::SeqCst), 0);
        let id = format!("new-share-account-{}", family.as_str());
        let result = client
            .call(
                args(&[
                    "execute",
                    "kamino.deposit",
                    "--input",
                    &input.to_string(),
                    "--id",
                    &id,
                    "--json",
                ]),
                BrokerRequest::Execute {
                    request: OperationRequest {
                        request_id: id,
                        operation: "kamino.deposit".into(),
                        input,
                        run_id: None,
                    },
                },
                true,
            )
            .await;
        let record: OperationRecord = typed_record(result.clone()).unwrap();
        assert_eq!(record.status, Status::Completed, "{result}");
        assert_eq!(chain.balance(&share(family)), 10);
        assert_eq!(chain.lamports(), initial_lamports - ACCOUNT_RENT - 5000);
        assert_eq!(approval.0.load(Ordering::SeqCst), 1);
        let durable: OperationRecord = typed_record(
            Store::open(&path)
                .unwrap()
                .get_typed_record(&record.id)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(durable.position_baseline.as_ref().unwrap().anchors.len(), 1);
        let funding = durable
            .effects
            .as_ref()
            .unwrap()
            .effects
            .iter()
            .find_map(|effect| match effect {
                FinancialEffect::PositionChange {
                    position,
                    delta,
                    anchor: Some(anchor),
                    ..
                } if position == &format!("account-funding:{account}") => Some((delta, anchor)),
                _ => None,
            })
            .unwrap();
        assert_eq!(funding.0.as_str(), ACCOUNT_RENT.to_string());
        assert_eq!(
            funding.1.position_kind,
            PositionKind::RefundableAccountFunding
        );
        assert_eq!(funding.1.principal.atoms, Quantity::from_u64(ACCOUNT_RENT));
        assert_eq!(
            read_position(&client, family).await["wallet_shares_atoms"],
            "10"
        );
    }
}

#[tokio::test]
async fn exact_standalone_roundtrip_preserves_starting_shares_and_durable_settlement_for_both_families()
 {
    for family in [VaultFamily::Lending, VaultFamily::Liquidity] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("journal.sqlite");
        let (chain, wallet) = Chain::new();
        chain.inventory_unavailable.store(true, Ordering::SeqCst);
        let original_shares = 7;
        chain.seed_balance(&share(family), original_shares);
        chain.seed_balance(UNDERLYING, 200);
        let original_underlying = chain.balance(UNDERLYING);
        let original_other = chain.balance(INPUT);
        let approval = Arc::new(Approval::default());
        let client = Client::new(
            runtime(&path, chain.clone(), wallet.clone(), approval.clone()),
            temp.path(),
        )
        .await;
        let initial = read_position(&client, family).await;
        assert_eq!(initial["wallet_shares_atoms"], original_shares.to_string());
        assert_eq!(initial["accounting"]["historical_principal_known"], false);
        assert_eq!(approval.0.load(Ordering::SeqCst), 0);
        let mut partial = withdrawal(family);
        partial["shares_atoms"] = json!("4");
        let mut remainder = withdrawal(family);
        remainder["shares_atoms"] = json!("6");
        let mut entry = deposit(family, "100");
        if family == VaultFamily::Liquidity {
            entry["amounts"][1]["atoms"] = json!("50");
        }
        let mut records = Vec::new();
        for (index, (label, operation, input, delta, owned)) in [
            (
                "deposit",
                "kamino.deposit",
                entry,
                "10",
                original_shares + 10,
            ),
            (
                "partial-withdrawal",
                "kamino.withdraw",
                partial,
                "-4",
                original_shares + 6,
            ),
            (
                "withdraw-new-share-remainder",
                "kamino.withdraw",
                remainder,
                "-6",
                original_shares,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let preview = client
                .call(
                    args(&[
                        "preview",
                        operation,
                        "--input",
                        &input.to_string(),
                        "--json",
                    ]),
                    BrokerRequest::Preview {
                        operation: operation.into(),
                        input: input.clone(),
                    },
                    true,
                )
                .await;
            #[cfg(target_os = "macos")]
            native_approval::assert_prepared_renders(
                &serde_json::from_value(preview.clone()).expect("prepared preview"),
            );
            assert_eq!(preview["operation"], operation, "{preview}");
            assert_eq!(chain.send_count.load(Ordering::SeqCst), index);
            assert_eq!(approval.0.load(Ordering::SeqCst), index);
            let request_id = format!("standalone-{}-{label}", family.as_str());
            let result = client
                .call(
                    args(&[
                        "execute",
                        operation,
                        "--input",
                        &input.to_string(),
                        "--id",
                        &request_id,
                        "--json",
                    ]),
                    BrokerRequest::Execute {
                        request: OperationRequest {
                            request_id,
                            operation: operation.into(),
                            input,
                            run_id: None,
                        },
                    },
                    true,
                )
                .await;
            let record: OperationRecord = typed_record(result.clone()).unwrap();
            assert_eq!(record.status, Status::Completed, "{result}");
            assert!(record.approval.is_some());
            assert_ne!(
                record.authorization_source,
                Some(AuthorizationSource::NamedPlan)
            );
            assert_ne!(
                record.authorization_source,
                Some(AuthorizationSource::BoundedGrant)
            );
            let submission = record.submission.as_ref().unwrap();
            let verified = record.effects.as_ref().unwrap();
            assert_eq!(verified.submission, *submission);
            let durable: OperationRecord = typed_record(
                Store::open(&path)
                    .unwrap()
                    .get_typed_record(&record.id)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            let durable_effects = durable.effects.as_ref().unwrap();
            assert_eq!(durable.status, Status::Completed);
            assert_eq!(durable.approval, record.approval);
            assert_eq!(
                verified.proof_digest,
                verified_effect_digest(durable_effects).unwrap()
            );
            assert!(submission.data.get("signed_transaction_base64").is_none());
            assert_eq!(
                durable_effects.submission,
                *durable.submission.as_ref().unwrap()
            );
            assert_eq!(
                verified.evidence["source"],
                "finalized_kamino_vault_transaction"
            );
            assert_eq!(
                verified.evidence["proof"]["signature"],
                submission.data["wallet_signature"]
            );
            let position_effect = verified
                .effects
                .iter()
                .find_map(|effect| match effect {
                    FinancialEffect::PositionChange {
                        asset,
                        delta,
                        anchor: Some(anchor),
                        ..
                    } if asset.reference == share(family) => Some((delta, anchor)),
                    _ => None,
                })
                .unwrap();
            assert_eq!(position_effect.0.as_str(), delta);
            assert_eq!(position_effect.1.principal.atoms, Quantity::from_u64(owned));
            assert_eq!(
                position_effect.1.evidence["vault_position"]["historical_principal_known"],
                false
            );
            assert!(verified.effects.iter().any(|effect| match effect {
                FinancialEffect::CashDebit { amount, .. } if index == 0 =>
                    amount.asset.reference == UNDERLYING && amount.atoms == Quantity::from_u64(100),
                FinancialEffect::UsableCredit { amount, .. } if index > 0 =>
                    amount.asset.reference == UNDERLYING
                        && amount.atoms == Quantity::from_u64(if index == 1 { 40 } else { 60 }),
                _ => false,
            }));
            let observed = read_position(&client, family).await;
            assert_eq!(observed["wallet_shares_atoms"], owned.to_string());
            assert_eq!(
                observed["accounting"]["wallet_shares_atoms"],
                owned.to_string()
            );
            assert_eq!(chain.send_count.load(Ordering::SeqCst), index + 1);
            assert_eq!(approval.0.load(Ordering::SeqCst), index + 1);
            records.push(record);
        }
        assert_eq!(chain.balance(&share(family)), original_shares);
        assert_eq!(chain.balance(UNDERLYING), original_underlying);
        assert_eq!(chain.balance(INPUT), original_other);
        // A fresh broker must recover the immutable identities and settled
        // financial effects without another approval or transaction dispatch.
        drop(client);
        let client = Client::new(
            runtime(&path, chain.clone(), wallet, approval.clone()),
            temp.path(),
        )
        .await;
        for expected in records {
            let status = client
                .call(
                    args(&["status", &expected.id, "--json"]),
                    BrokerRequest::Status {
                        id: expected.id.clone(),
                    },
                    true,
                )
                .await;
            let recovered: OperationRecord = typed_record(status).unwrap();
            assert_eq!(recovered.status, Status::Completed);
            assert_eq!(recovered.submission, expected.submission);
            assert_eq!(recovered.effects, expected.effects);
        }
        assert_eq!(
            read_position(&client, family).await["wallet_shares_atoms"],
            original_shares.to_string()
        );
        assert_eq!(chain.send_count.load(Ordering::SeqCst), 3);
        assert_eq!(approval.0.load(Ordering::SeqCst), 3);
    }
}
#[tokio::test]
async fn separate_jupiter_and_native_kamino_actions_preserve_receipt_floors_across_restart() {
    for family in [VaultFamily::Lending, VaultFamily::Liquidity] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("journal.sqlite");
        let (chain, wallet) = Chain::new();
        let approval = Arc::new(Approval::default());
        let mut client = Client::new(
            runtime(&path, chain.clone(), wallet.clone(), approval.clone()),
            temp.path(),
        )
        .await;
        let mut records = vec![];
        for (index, (label, operation, input)) in [
            (
                "entry-swap",
                "jupiter.swap",
                swap(INPUT, UNDERLYING, "100", "100"),
            ),
            ("deposit", "kamino.deposit", deposit(family, "115")),
            ("withdraw", "kamino.withdraw", withdrawal(family)),
            (
                "exit-swap",
                "jupiter.swap",
                swap(UNDERLYING, INPUT, "50", "40"),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            if label == "withdraw" {
                assert_eq!(chain.balance(&share(family)), 10);
                drop(client);
                client = Client::new(
                    runtime(&path, chain.clone(), wallet.clone(), approval.clone()),
                    temp.path(),
                )
                .await;
                for record in &records {
                    let observed = status(&client, record).await;
                    assert_eq!(observed["submission"], record["submission"]);
                    assert_eq!(observed["outcome"]["replay_safe"], false);
                }
                assert_eq!(chain.send_count.load(Ordering::SeqCst), 2);
                assert_eq!(approval.0.load(Ordering::SeqCst), 2);
            }
            let result = execute(&client, label, operation, input.clone()).await;
            assert_eq!(result["status"], "completed", "{result}");
            let repeated = execute(&client, label, operation, input).await;
            assert_eq!(repeated["id"], result["id"]);
            assert_eq!(repeated["submission"], result["submission"]);
            assert_eq!(approval.0.load(Ordering::SeqCst), index + 1);
            records.push(result);
        }
        assert_eq!(chain.balance(UNDERLYING), 10);
        assert_eq!(chain.balance(&share(family)), 5);
        assert_eq!(chain.send_count.load(Ordering::SeqCst), 4);
        assert_eq!(approval.0.load(Ordering::SeqCst), 4);
        let withdraw = typed_record(records[2].clone()).unwrap();
        assert!(withdraw.effects.unwrap().effects.iter().any(
            |e| matches!(e,FinancialEffect::PositionChange {delta,..} if delta.as_str()=="-5")
        ));
    }
}
#[tokio::test]
async fn later_native_failure_keeps_verified_swap_assets_and_does_not_reverse() {
    let temp = tempfile::tempdir().unwrap();
    let (chain, wallet) = Chain::new();
    let approval = Arc::new(Approval::default());
    let client = Client::new(
        runtime(
            &temp.path().join("journal.sqlite"),
            chain.clone(),
            wallet,
            approval.clone(),
        ),
        temp.path(),
    )
    .await;
    let swap_result = execute(
        &client,
        "entry-swap",
        "jupiter.swap",
        swap(INPUT, UNDERLYING, "100", "100"),
    )
    .await;
    assert_eq!(swap_result["status"], "completed");
    chain.reject_deposit.store(true, Ordering::SeqCst);
    let failed = execute(
        &client,
        "deposit",
        "kamino.deposit",
        deposit(VaultFamily::Lending, "115"),
    )
    .await;
    assert_eq!(failed["status"], "failed");
    assert!(
        failed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unavailable")
    );
    assert_eq!(chain.balance(UNDERLYING), 125);
    assert_eq!(chain.send_count.load(Ordering::SeqCst), 1);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        status(&client, &swap_result).await["outcome"]["replay_safe"],
        false
    );
}
#[tokio::test]
async fn restart_reconciles_original_swap_and_does_not_auto_execute_deposit() {
    recover_lost_action(false).await;
}
#[tokio::test]
async fn lost_kamino_response_reconciles_original_submission_after_broker_restart() {
    recover_lost_action(true).await;
}
async fn recover_lost_action(lose_deposit: bool) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("journal.sqlite");
    let (chain, wallet) = Chain::new();
    let approval = Arc::new(Approval::default());
    let client = Client::new(
        runtime(&path, chain.clone(), wallet.clone(), approval.clone()),
        temp.path(),
    )
    .await;
    if lose_deposit {
        assert_eq!(
            execute(
                &client,
                "entry-swap",
                "jupiter.swap",
                swap(INPUT, UNDERLYING, "100", "100")
            )
            .await["status"],
            "completed"
        );
    }
    chain.lose_response.store(true, Ordering::SeqCst);
    chain.hide_receipts.store(true, Ordering::SeqCst);
    let (label, operation, input) = if lose_deposit {
        (
            "deposit",
            "kamino.deposit",
            deposit(VaultFamily::Lending, "115"),
        )
    } else {
        (
            "entry-swap",
            "jupiter.swap",
            swap(INPUT, UNDERLYING, "100", "100"),
        )
    };
    let result = execute(&client, label, operation, input.clone()).await;
    assert_ne!(result["status"], "completed");
    let count = if lose_deposit { 2 } else { 1 };
    assert_eq!(chain.send_count.load(Ordering::SeqCst), count);
    drop(client);
    chain.hide_receipts.store(false, Ordering::SeqCst);
    chain.lose_response.store(false, Ordering::SeqCst);
    let client = Client::new(
        runtime(&path, chain.clone(), wallet, approval.clone()),
        temp.path(),
    )
    .await;
    let observed = status(&client, &result).await;
    assert_eq!(observed["status"], "completed", "{observed}");
    let repeated = execute(&client, label, operation, input).await;
    assert_eq!(repeated["submission"], result["submission"]);
    assert_eq!(repeated["id"], result["id"]);
    assert_eq!(chain.send_count.load(Ordering::SeqCst), count);
    assert_eq!(approval.0.load(Ordering::SeqCst), count);
    if !lose_deposit {
        assert_eq!(chain.balance(&share(VaultFamily::Lending)), 0);
        assert_eq!(
            execute(
                &client,
                "deposit",
                "kamino.deposit",
                deposit(VaultFamily::Lending, "115")
            )
            .await["status"],
            "completed"
        );
        assert_eq!(chain.send_count.load(Ordering::SeqCst), 2);
        assert_eq!(approval.0.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn durably_denied_native_submission_proves_nonexecution_and_survives_restart() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("journal.sqlite");
    let (chain, wallet) = Chain::new();
    chain.seed_balance(&share(VaultFamily::Lending), 20);
    *chain.expire_after_submission.lock().unwrap() = Some(path.clone());
    let approval = Arc::new(Approval::default());
    let client = Client::new(
        runtime(&path, chain.clone(), wallet.clone(), approval.clone()),
        temp.path(),
    )
    .await;
    let input = withdrawal(VaultFamily::Lending);
    let result = client
        .call(
            args(&[
                "execute",
                "kamino.withdraw",
                "--input",
                &input.to_string(),
                "--id",
                "native-denied-after-journal",
                "--json",
            ]),
            BrokerRequest::Execute {
                request: OperationRequest {
                    request_id: "native-denied-after-journal".into(),
                    operation: "kamino.withdraw".into(),
                    input,
                    run_id: None,
                },
            },
            true,
        )
        .await;
    let record: OperationRecord = typed_record(result.clone()).unwrap();
    assert_eq!(record.status, Status::Failed, "{result}");
    assert!(record.submission.is_some());
    assert_eq!(
        record.dispatch_evidence.as_ref().unwrap()["no_network_send"],
        true
    );
    let verified = record.effects.as_ref().unwrap();
    assert!(verified.effects.is_empty());
    assert_eq!(verified.evidence["state"], "not_dispatched");
    // Public records redact the signed wire; validate the digest against the
    // complete immutable journal instead of that public projection.
    let durable: OperationRecord = typed_record(
        Store::open(&path)
            .unwrap()
            .get_typed_record(&record.id)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let verified = durable.effects.as_ref().unwrap();
    assert_eq!(
        verified.proof_digest,
        verified_effect_digest(verified).unwrap()
    );
    assert_eq!(
        Store::open(&path)
            .unwrap()
            .reservation_count(&record.id)
            .unwrap(),
        0
    );
    assert_eq!(chain.send_count.load(Ordering::SeqCst), 0);
    assert_eq!(chain.balance(&share(VaultFamily::Lending)), 20);
    drop(client);
    let client = Client::new(
        runtime(&path, chain.clone(), wallet, approval.clone()),
        temp.path(),
    )
    .await;
    let status = client
        .call(
            args(&["status", &record.id, "--json"]),
            BrokerRequest::Status {
                id: record.id.clone(),
            },
            true,
        )
        .await;
    let recovered: OperationRecord = typed_record(status).unwrap();
    assert_eq!(recovered, record);
    assert_eq!(
        Store::open(&path)
            .unwrap()
            .reservation_count(&record.id)
            .unwrap(),
        0
    );
    assert_eq!(chain.send_count.load(Ordering::SeqCst), 0);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn expired_unlanded_native_withdrawal_requires_full_history_and_never_replays() {
    for family in [VaultFamily::Lending, VaultFamily::Liquidity] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("journal.sqlite");
        let (chain, wallet) = Chain::new();
        let primary = RpcEndpoint::new(chain.clone()).await;
        let secondary = RpcEndpoint::new(chain.clone()).await;
        secondary.history_available.store(false, Ordering::SeqCst);
        let pooled = gofin_solana::rpc::HttpRpc::isolated_localhost_pool(vec![
            primary.url.clone(),
            secondary.url.clone(),
        ])
        .unwrap();
        let wallet = Arc::new(
            gofin_solana::SolanaWalletAdapter::new(
                "wallet",
                &chain.owner,
                wallet.signer(),
                pooled,
                Arc::new(Prices),
            )
            .unwrap(),
        );
        chain.seed_balance(&share(family), 20);
        chain.finalized_height.store(999, Ordering::SeqCst);
        chain.drop_before_landing.store(true, Ordering::SeqCst);
        let original_lamports = chain.lamports();
        let approval = Arc::new(Approval::default());
        let client = Client::new(
            runtime(&path, chain.clone(), wallet.clone(), approval.clone()),
            temp.path(),
        )
        .await;
        let input = withdrawal(family);
        let request_id = format!("unlanded-withdrawal-{}", family.as_str());
        let execute_args = args(&[
            "execute",
            "kamino.withdraw",
            "--input",
            &input.to_string(),
            "--id",
            &request_id,
            "--json",
        ]);
        let request = OperationRequest {
            request_id,
            operation: "kamino.withdraw".into(),
            input,
            run_id: None,
        };
        let result = client
            .call(
                execute_args.clone(),
                BrokerRequest::Execute {
                    request: request.clone(),
                },
                true,
            )
            .await;
        let original: OperationRecord = typed_record(result.clone()).unwrap();
        assert!(
            matches!(original.status, Status::Pending | Status::Unknown),
            "{result}"
        );
        assert!(original.submission.is_some());
        assert!(
            original
                .dispatch_evidence
                .as_ref()
                .is_none_or(|v| v["no_network_send"] != true)
        );
        assert_eq!(
            Store::open(&path)
                .unwrap()
                .reservation_count(&original.id)
                .unwrap(),
            0
        );
        assert_eq!(result["outcome"]["replay_safe"], false);
        assert_eq!(chain.send_count.load(Ordering::SeqCst), 1);

        chain.finalized_height.store(1001, Ordering::SeqCst);
        // Resume the uncertain submission through a fresh broker/RPC instance,
        // with no cached pre-expiry heads or partial proof progress.
        drop(client);
        let pooled = gofin_solana::rpc::HttpRpc::isolated_localhost_pool(vec![
            primary.url.clone(),
            secondary.url.clone(),
        ])
        .unwrap();
        let wallet = Arc::new(
            gofin_solana::SolanaWalletAdapter::new(
                "wallet",
                &chain.owner,
                wallet.signer(),
                pooled,
                Arc::new(Prices),
            )
            .unwrap(),
        );
        let client = Client::new(
            runtime(&path, chain.clone(), wallet.clone(), approval.clone()),
            temp.path(),
        )
        .await;
        let unresolved = client
            .call(
                args(&["status", &original.id, "--json"]),
                BrokerRequest::Status {
                    id: original.id.clone(),
                },
                true,
            )
            .await;
        let pending: OperationRecord = typed_record(unresolved.clone()).unwrap();
        assert!(
            matches!(pending.status, Status::Pending | Status::Unknown),
            "Missing block history cannot prove absence: {unresolved}"
        );
        assert!(pending.effects.is_none());
        assert_eq!(
            Store::open(&path)
                .unwrap()
                .reservation_count(&original.id)
                .unwrap(),
            0
        );
        assert_eq!(result["outcome"]["replay_safe"], false);
        assert_eq!(chain.send_count.load(Ordering::SeqCst), 1);

        chain.block_history_available.store(true, Ordering::SeqCst);
        let uncorroborated = client
            .call(
                args(&["status", &original.id, "--json"]),
                BrokerRequest::Status {
                    id: original.id.clone(),
                },
                true,
            )
            .await;
        let pending: OperationRecord = typed_record(uncorroborated.clone()).unwrap();
        assert!(
            matches!(pending.status, Status::Pending | Status::Unknown),
            "One provider cannot establish non-inclusion: {uncorroborated}"
        );
        assert!(pending.effects.is_none());
        assert_eq!(
            Store::open(&path)
                .unwrap()
                .reservation_count(&original.id)
                .unwrap(),
            0
        );
        assert_eq!(result["outcome"]["replay_safe"], false);
        secondary.history_available.store(true, Ordering::SeqCst);
        let result = client
            .call(
                args(&["status", &original.id, "--json"]),
                BrokerRequest::Status {
                    id: original.id.clone(),
                },
                true,
            )
            .await;
        let settled: OperationRecord = typed_record(result.clone()).unwrap();
        assert_eq!(
            settled.status,
            Status::Failed,
            "evidence={:?}; rpc={}; blocks={}/{}",
            settled.last_reconciliation_evidence,
            wallet.runtime().rpc().diagnostics(),
            primary.signature_blocks.load(Ordering::SeqCst),
            secondary.signature_blocks.load(Ordering::SeqCst)
        );
        assert_eq!(settled.submission, original.submission);
        assert_eq!(settled.dispatch_evidence, original.dispatch_evidence);
        assert_eq!(settled.preview, original.preview);
        assert_eq!(settled.approval, original.approval);
        let verified = settled.effects.as_ref().unwrap();
        assert!(verified.effects.is_empty());
        assert_eq!(verified.evidence["state"], "expired_without_inclusion");
        assert!(verified.evidence.get("no_network_send").is_none());
        assert_eq!(
            Store::open(&path)
                .unwrap()
                .reservation_count(&original.id)
                .unwrap(),
            0
        );
        let durable: OperationRecord = typed_record(
            Store::open(&path)
                .unwrap()
                .get_typed_record(&original.id)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let proof = durable.effects.as_ref().unwrap();
        assert_eq!(proof.proof_digest, verified_effect_digest(proof).unwrap());
        assert_eq!(proof.submission, durable.submission.clone().unwrap());
        assert!(
            kamino(chain.clone(), wallet.clone())
                .verify_non_dispatch_expiry(&durable)
                .await
                .unwrap()
                .is_none(),
            "A sent but unlanded transaction is not a no-send automatic retry."
        );
        assert_eq!(chain.balance(&share(family)), 20);
        assert_eq!(chain.balance(UNDERLYING), 0);
        assert_eq!(chain.lamports(), original_lamports);
        drop(client);

        chain.drop_before_landing.store(false, Ordering::SeqCst);
        let client = Client::new(
            runtime(&path, chain.clone(), wallet, approval.clone()),
            temp.path(),
        )
        .await;
        let recovered = client
            .call(
                args(&["status", &original.id, "--json"]),
                BrokerRequest::Status {
                    id: original.id.clone(),
                },
                true,
            )
            .await;
        assert_eq!(typed_record(recovered).unwrap(), settled);
        let replay = client
            .call(execute_args, BrokerRequest::Execute { request }, true)
            .await;
        assert_eq!(typed_record(replay).unwrap(), settled);
        assert_eq!(
            Store::open(&path)
                .unwrap()
                .reservation_count(&original.id)
                .unwrap(),
            0
        );
        assert_eq!(chain.send_count.load(Ordering::SeqCst), 1);
        assert_eq!(approval.0.load(Ordering::SeqCst), 1);
        assert!(primary.signature_blocks.load(Ordering::SeqCst) >= 152);
        assert!(secondary.signature_blocks.load(Ordering::SeqCst) >= 152);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires owned Surfpool and public development CLI"]
async fn public_cli_real_surfpool_blockhash_expiry_preserves_original_signature() {
    use std::time::{Duration, Instant};
    struct Node(std::process::Child);
    impl Drop for Node {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let binary =
        std::env::var("GOFIN_SURFPOOL_BIN").expect("pinned owned Surfpool binary required");
    assert!(
        std::env::var("GOFIN_KAMINO_CLI").is_ok(),
        "public CLI required"
    );
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("journal.sqlite");
    let rpc_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let ws_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let node = std::process::Command::new(binary)
        .args([
            "start",
            "--no-tui",
            "--no-studio",
            "--no-deploy",
            "--host",
            "127.0.0.1",
            "--offline",
            "--port",
            &rpc_port.to_string(),
            "--ws-port",
            &ws_port.to_string(),
            "--slot-time",
            "100",
            "--airdrop-amount",
            "0",
            "--airdrop-keypair-path",
            temp.path().join("no-airdrop.json").to_str().unwrap(),
            "--db",
            ":memory:",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let _node = Node(node);
    let url = format!("http://127.0.0.1:{rpc_port}");
    let http = reqwest::Client::new();
    let mut height = 0u64;
    for _ in 0..300 {
        if let Ok(response)=http.post(&url).json(&json!({"jsonrpc":"2.0","id":1,"method":"getBlockHeight","params":[{"commitment":"finalized"}]})).send().await && let Ok(value)=response.json::<Value>().await && let Some(h)=value["result"].as_u64() && h>0 {height=h;break}
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(height > 0, "owned Surfpool did not start");
    // The simulator first exposes the upstream bootstrap height before its
    // local bank is ready; let the local block-height clock settle.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let (chain, initial_wallet) = Chain::new();
    chain.drop_before_landing.store(true, Ordering::SeqCst);
    let primary = RpcEndpoint::new_with_history(chain.clone(), Some(url.clone())).await;
    let secondary = RpcEndpoint::new_with_history(chain.clone(), Some(url.clone())).await;
    let pooled = gofin_solana::rpc::HttpRpc::isolated_localhost_pool(vec![
        primary.url.clone(),
        secondary.url.clone(),
    ])
    .unwrap();
    let wallet = Arc::new(
        gofin_solana::SolanaWalletAdapter::new(
            "wallet",
            &chain.owner,
            initial_wallet.signer(),
            pooled,
            Arc::new(Prices),
        )
        .unwrap(),
    );
    let second_key = ed25519_dalek::SigningKey::from_bytes(&[2u8; 32]);
    let second_signer = Arc::new(
        gofin_solana::SolanaSigner::parse(
            &serde_json::to_string(&second_key.to_keypair_bytes().to_vec()).unwrap(),
        )
        .unwrap(),
    );
    let second_rpc = gofin_solana::rpc::HttpRpc::isolated_localhost_pool(vec![
        primary.url.clone(),
        secondary.url.clone(),
    ])
    .unwrap();
    let second_wallet = Arc::new(
        gofin_solana::SolanaWalletAdapter::new(
            "other-wallet",
            second_signer.address(),
            Some(second_signer.clone()),
            second_rpc,
            Arc::new(Prices),
        )
        .unwrap(),
    );
    let approval = Arc::new(Approval::default());
    let client = Client::new(
        expiry_fixture_runtime(&path, wallet.clone(), second_wallet, approval.clone()),
        temp.path(),
    )
    .await;
    let recipient = bs58::encode([7u8; 32]).into_string();
    let input = json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","requestId":"surfpool-expiry","recipient":recipient,"amount":"0.001","max_fee_sol":"0.001"});
    let req = OperationRequest {
        request_id: "surfpool-expiry".into(),
        operation: "wallet.transfer".into(),
        input: input.clone(),
        run_id: None,
    };
    let execute = args(&[
        "execute",
        "wallet.transfer",
        "--input",
        &input.to_string(),
        "--id",
        "surfpool-expiry",
        "--json",
    ]);
    let initial = client
        .call(
            execute.clone(),
            BrokerRequest::Execute {
                request: req.clone(),
            },
            true,
        )
        .await;
    let id = initial["id"].as_str().unwrap().to_owned();
    let submission = initial["submission"].clone();
    assert!(!submission.is_null(), "{initial}");
    let last_valid = submission["data"]["last_valid_block_height"]
        .as_u64()
        .unwrap();
    let started = Instant::now();
    loop {
        let value:Value=http.post(&url).json(&json!({"jsonrpc":"2.0","id":1,"method":"getBlockHeight","params":[{"commitment":"finalized"}]})).send().await.unwrap().json().await.unwrap();
        if value["result"].as_u64().is_some_and(|h| h > last_valid + 2) {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(90),
            "Surfpool did not advance past lifetime"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let mut result = Value::Null;
    for _ in 0..5 {
        result = client
            .call(
                args(&["status", &id, "--json"]),
                BrokerRequest::Status { id: id.clone() },
                true,
            )
            .await;
        if result["status"] == "failed" {
            break;
        }
    }
    assert_eq!(result["status"], "failed", "{result}");
    assert_eq!(result["outcome"]["replay_safe"], true);
    assert_eq!(result["outcome"]["result"], "failed");
    assert_eq!(result["outcome"]["retry_approval"], "required");
    assert_eq!(result["outcome"]["next_action"]["code"], "reauthorize");
    assert_eq!(
        result["last_reconciliation_evidence"]["state"],
        "expired_without_inclusion"
    );
    assert_eq!(result["submission"], submission);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(chain.send_count.load(Ordering::SeqCst), 1);
    let replay = client
        .call(execute, BrokerRequest::Execute { request: req }, true)
        .await;
    assert_eq!(replay["id"], id);
    assert_eq!(chain.send_count.load(Ordering::SeqCst), 1);
    let snapshot = || {
        (
            approval.0.load(Ordering::SeqCst),
            chain.send_count.load(Ordering::SeqCst),
        )
    };
    let main_counts = snapshot();
    let mut admission_probes = Vec::new();
    for (probe_id, wallet_name) in [("expiry-same", "wallet"), ("expiry-other", "other-wallet")] {
        let input = json!({"wallet":wallet_name,"network":"solana","cluster":"mainnet-beta","requestId":probe_id,"recipient":recipient,"amount":"0.001","max_fee_sol":"0.001"});
        let req = BrokerRequest::Execute {
            request: OperationRequest {
                request_id: probe_id.into(),
                operation: "wallet.transfer".into(),
                input: input.clone(),
                run_id: None,
            },
        };
        let before = snapshot();
        let result = client
            .call(
                args(&[
                    "execute",
                    "wallet.transfer",
                    "--input",
                    &input.to_string(),
                    "--id",
                    probe_id,
                    "--json",
                ]),
                req,
                true,
            )
            .await;
        admission_probes.push(outcome_evidence::admission_probe(
            if wallet_name == "wallet" {
                "same_account"
            } else {
                "different_account"
            },
            before,
            snapshot(),
            &result,
        ));
    }
    assert_eq!(approval.0.load(Ordering::SeqCst), 3);
    assert_eq!(chain.send_count.load(Ordering::SeqCst), 3);
    if let Ok(path) = std::env::var("GOFIN_OUTCOME_EVIDENCE") {
        let mut evidence = json!({"scenario":"outcome.a11","approval_count":main_counts.0,"external_submission_count":main_counts.1,"probe_approval_count":2,"probe_submission_count":2,"intervention_count":0,"remaining_external_uncertainty":true,"blocked_unrelated_activity":false,"rpc_evidence":"two local fixture endpoints relay one patched owned Surfpool; not independent production providers"});
        outcome_evidence::attach_probes(&mut evidence, &admission_probes);
        std::fs::write(path, serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
    }
}

struct ExpiryFixtureWalletRouter {
    first: Arc<gofin_solana::SolanaWalletAdapter>,
    second: Arc<gofin_solana::SolanaWalletAdapter>,
}
impl ExpiryFixtureWalletRouter {
    fn selected(&self, input: &Value) -> &gofin_solana::SolanaWalletAdapter {
        if input["wallet"] == "other-wallet" {
            &self.second
        } else {
            &self.first
        }
    }
}
#[async_trait]
impl Adapter for ExpiryFixtureWalletRouter {
    fn definitions(&self) -> Vec<Definition> {
        self.first.definitions()
    }
    fn connections(&self) -> Vec<ConnectionMetadata> {
        let mut c = self.first.connections();
        c.extend(self.second.connections());
        c
    }
    fn uses_rpc_reads(&self, operation: &str, input: &Value) -> bool {
        self.selected(input).uses_rpc_reads(operation, input)
    }
    fn supports_independent_submission(&self, p: &PreparedOperation) -> bool {
        self.selected(&p.input).supports_independent_submission(p)
    }
    fn preparation_reuse_window(&self, p: &PreparedOperation) -> Option<std::time::Duration> {
        self.selected(&p.input).preparation_reuse_window(p)
    }
    fn validate_approved_candidate(
        &self,
        a: &PreparedOperation,
        b: &PreparedOperation,
    ) -> Result<()> {
        self.selected(&a.input).validate_approved_candidate(a, b)
    }
    async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
        self.selected(input).read(operation, input).await
    }
    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        self.selected(input).prepare(operation, input).await
    }
    async fn revalidate(&self, p: &PreparedOperation) -> Result<PreparedOperation> {
        self.selected(&p.input).revalidate(p).await
    }
    async fn submit(
        &self,
        p: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        self.selected(&p.input).submit(p, journal).await
    }
    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
        self.selected(&record.input).reconcile(record).await
    }
}
fn expiry_fixture_runtime(
    path: &Path,
    first: Arc<gofin_solana::SolanaWalletAdapter>,
    second: Arc<gofin_solana::SolanaWalletAdapter>,
    approval: Arc<Approval>,
) -> Arc<gofin_broker::BrokerRuntime> {
    compose_runtime_with_clock(
        BrokerConfigV2 {
            version: 2,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy: BrokerPolicy::beta(),
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        RuntimeDependencies {
            store: Store::open(path).unwrap(),
            adapters: vec![Arc::new(ExpiryFixtureWalletRouter { first, second })],
            authorizer: approval,
            valuator: Arc::new(SharedUsdValuator::with_source(Arc::new(Prices))),
            bridge: None,
        },
        Arc::new(gofin_domain::clock::SystemClock),
    )
    .unwrap()
}

struct StalledSolanaRpc {
    chain: Arc<Chain>,
    first_accepted: std::sync::atomic::AtomicBool,
    release_first: std::sync::atomic::AtomicBool,
}
#[async_trait]
impl gofin_solana::SolanaRpc for StalledSolanaRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let result = self.chain.request(method, params).await;
        if method == "sendTransaction" && !self.first_accepted.swap(true, Ordering::SeqCst) {
            while !self.release_first.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
        result
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn solana_distinct_memo_bound_actions_advance_while_first_response_stalls() {
    Box::pin(async {
        let temp=tempfile::tempdir().unwrap();let path=temp.path().join("journal.sqlite");let (chain,original_wallet)=Chain::new();chain.drop_before_landing.store(true,Ordering::SeqCst);
        let rpc=Arc::new(StalledSolanaRpc{chain:chain.clone(),first_accepted:std::sync::atomic::AtomicBool::new(false),release_first:std::sync::atomic::AtomicBool::new(false)});
        let first=Arc::new(gofin_solana::SolanaWalletAdapter::new("wallet",&chain.owner,original_wallet.signer(),rpc.clone(),Arc::new(Prices)).unwrap());
        let key=ed25519_dalek::SigningKey::from_bytes(&[2u8;32]);let signer=Arc::new(gofin_solana::SolanaSigner::parse(&serde_json::to_string(&key.to_keypair_bytes().to_vec()).unwrap()).unwrap());
        let second=Arc::new(gofin_solana::SolanaWalletAdapter::new("other-wallet",signer.address(),Some(signer.clone()),rpc.clone(),Arc::new(Prices)).unwrap());
        let approval=Arc::new(Approval::default());let client=Arc::new(Client::new(expiry_fixture_runtime(&path,first,second,approval.clone()),temp.path()).await);
        let run=|id:&str,wallet:&str| {let id=id.to_owned();let wallet=wallet.to_owned();let client=client.clone();async move {let input=json!({"wallet":wallet,"network":"solana","cluster":"mainnet-beta","requestId":id,"recipient":bs58::encode([7u8;32]).into_string(),"amount":"0.001","max_fee_sol":"0.001"});let req=BrokerRequest::Execute{request:OperationRequest {request_id:id.clone(),operation:"wallet.transfer".into(),input:input.clone(),run_id:None}};client.call(args(&["execute","wallet.transfer","--input",&input.to_string(),"--id",&id,"--json"]),req,true).await}};
        let first_call=tokio::spawn(run("solana-first","wallet"));
        tokio::time::timeout(std::time::Duration::from_secs(10),async {while !rpc.first_accepted.load(Ordering::SeqCst) {tokio::time::sleep(std::time::Duration::from_millis(5)).await}}).await.unwrap();
        let snapshot=|| (approval.0.load(Ordering::SeqCst),chain.send_count.load(Ordering::SeqCst));
        let main_counts=snapshot();
        let b=tokio::time::timeout(std::time::Duration::from_secs(10),run("solana-same","wallet")).await.unwrap();
        let same_probe=outcome_evidence::admission_probe("same_account",main_counts,snapshot(),&b);
        let before=snapshot();
        let c=tokio::time::timeout(std::time::Duration::from_secs(10),run("solana-other","other-wallet")).await.unwrap();
        let other_probe=outcome_evidence::admission_probe("different_account",before,snapshot(),&c);
        rpc.release_first.store(true,Ordering::SeqCst);
        let a=first_call.await.unwrap();
        assert_ne!(a["id"],b["id"]);assert_ne!(a["id"],c["id"]);assert_ne!(a["submission"]["reference"],b["submission"]["reference"]);assert_ne!(a["submission"]["reference"],c["submission"]["reference"]);
        assert_eq!(approval.0.load(Ordering::SeqCst),3);assert_eq!(chain.send_count.load(Ordering::SeqCst),3);
        if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {
            let mut evidence:Value=serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(evidence["scenario"],"outcome.a6");
            evidence["approval_count"]=json!(evidence["approval_count"].as_u64().unwrap()+main_counts.0 as u64);
            evidence["external_submission_count"]=json!(evidence["external_submission_count"].as_u64().unwrap()+main_counts.1 as u64);
            let mut probes=evidence["admission_probes"].as_array().unwrap().clone();probes.extend([same_probe,other_probe]);
            outcome_evidence::attach_probes(&mut evidence,&probes);
            evidence["solana_fixture"]=json!({"approvals":snapshot().0,"submissions":snapshot().1,"same_wallet_independent":true,"different_wallet_independent":true});
            std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
        }
    }).await
}
