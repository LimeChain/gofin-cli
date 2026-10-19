//! Real broker/engine/store, Jupiter adapter, Kamino adapter and Solana signing/receipt
//! verification, with controlled Jupiter API/RPC and injected Kamino protocol graphs.
//! This tests exact authority and composition, not native Kamino codecs (covered in gofin-kamino).
//! GOFIN_DEV_BUILD_ID=synthetic-kamino-build cargo build -p gofin-cli --example gofin-dev
//! GOFIN_KAMINO_CLI="$PWD/target/debug/examples/gofin-dev" cargo test -p gofin-broker --test kamino_workflow
use async_trait::async_trait;
use gofin_broker::{
    BrokerConfigV2, BrokerPolicy, NetworkConfig, RuntimeDependencies, SharedUsdValuator,
    compose_runtime_with_clock,
};
use gofin_domain::{financial_plans::*, *};
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
    json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","input_mint":input,"output_mint":output,"amount_atoms":amount,"minimum_output_atoms":minimum,"maximum_slippage_bps":100,"maximum_platform_fee_bps":0,"maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0,"route":"router"})
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
fn swap_bounds() -> Value {
    json!({"maximum_input_atoms":"100","minimum_output_per_input_numerator":"1","minimum_output_per_input_denominator":"1","maximum_slippage_bps":100,"maximum_platform_fee_bps":0,"maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0,"maximum_quote_lifetime_seconds":120})
}
fn dependency(id: &str, resource: &ResourceId, mint: &str) -> FinancialPlanFundingDependency {
    FinancialPlanFundingDependency {
        producing_step_id: id.into(),
        resource: resource.clone(),
        asset: asset(mint),
        funding_scope: None,
        maximum_fraction_bps: None,
    }
}
fn step(
    id: &str,
    operation: &str,
    input: Value,
    bounds: Value,
    deps: Vec<FinancialPlanFundingDependency>,
) -> FinancialPlanStepSpec {
    FinancialPlanStepSpec {
        step_id: id.into(),
        operation: operation.into(),
        input,
        permitted_variations: bounds,
        funding_dependencies: deps,
        action_deadline_seconds: None,
        completion: FinancialStepCompletion {
            operation_succeeded: true,
            principal_spend: None,
            minimum_net_receipt: None,
        },
    }
}
fn plan(chain: &Chain, family: VaultFamily) -> FinancialPlanSpec {
    let r = chain.resource();
    let mut deposit_caps = vec![json!({"mint":UNDERLYING,"atoms":"125"})];
    if family == VaultFamily::Liquidity {
        deposit_caps.push(json!({"mint":INPUT,"atoms":"10"}));
    }
    let mut exit_bounds = swap_bounds();
    exit_bounds["maximum_input_atoms"] = json!("50");
    exit_bounds["minimum_output_per_input_numerator"] = json!("4");
    exit_bounds["minimum_output_per_input_denominator"] = json!("5");
    FinancialPlanSpec{version:1,name:format!("Controlled {} vault roundtrip",family.as_str()),summary:Some("Swap 100 USDC atoms; deposit up to 125 asset atoms; redeem 5 shares; swap native proceeds; retain dust.".into()),steps:vec![step("entry-swap","jupiter.swap",swap(INPUT,UNDERLYING,"100","100"),swap_bounds(),vec![]),step("deposit","kamino.deposit",deposit(family,"125"),json!({"maximum_amounts":deposit_caps,"maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0}),vec![dependency("entry-swap",&r,UNDERLYING)]),step("withdraw","kamino.withdraw",withdrawal(family),json!({"maximum_shares_atoms":"5","maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0}),vec![]),step("exit-swap","jupiter.swap",swap(UNDERLYING,INPUT,"50","40"),exit_bounds,vec![dependency("withdraw",&r,UNDERLYING)])],aggregate_bounds:FinancialPlanBounds{maximum_debits:vec![ResourceAmount{resource:r.clone(),amount:Amount{asset:asset(INPUT),atoms:Quantity::from_u64(if family==VaultFamily::Liquidity{110}else{100})}},ResourceAmount{resource:r.clone(),amount:Amount{asset:asset(UNDERLYING),atoms:Quantity::from_u64(175)}}],maximum_costs:vec![CostBound{location:AccountingLocation::Cash{resource:r},amount:Amount{asset:native(),atoms:Quantity::from_u64(40000)},treatment:CostTreatment::AdditionalDebit}]}}
}
async fn authorize(client: &Client, spec: FinancialPlanSpec) -> String {
    let mut request = FinancialPlanAuthorizationRequest {
        request_id: "kamino-workflow-plan".into(),
        plan: spec.clone(),
        valid_for_seconds: Some(900),
        replaces_revision_id: None,
        expected_review_hash: None,
    };
    let preview = client
        .call(
            args(&[
                "plans",
                "preview",
                "--input",
                &json!(spec).to_string(),
                "--valid-for",
                "15m",
                "--json",
            ]),
            BrokerRequest::PreviewPlan {
                request: request.clone(),
            },
            true,
        )
        .await;
    request.expected_review_hash = Some(preview["review_hash"].as_str().unwrap().into());
    let result = client
        .call(
            args(&[
                "plans",
                "authorize",
                "--input",
                &json!(spec).to_string(),
                "--id",
                "kamino-workflow-plan",
                "--review-hash",
                request.expected_review_hash.as_deref().unwrap(),
                "--valid-for",
                "15m",
                "--json",
            ]),
            BrokerRequest::AuthorizePlan { request },
            true,
        )
        .await;
    result["plan_id"].as_str().unwrap().into()
}
async fn execute(
    client: &Client,
    plan: &str,
    step: &str,
    operation: &str,
    input: Value,
    ok: bool,
) -> Value {
    let id = format!("kamino-{step}");
    client
        .call(
            args(&[
                "execute",
                operation,
                "--input",
                &input.to_string(),
                "--id",
                &id,
                "--plan",
                plan,
                "--step",
                step,
                "--json",
            ]),
            BrokerRequest::ExecutePlanStep {
                plan_id: plan.into(),
                step_id: step.into(),
                request: OperationRequest {
                    request_id: id,
                    operation: operation.into(),
                    input,
                    run_id: None,
                },
            },
            ok,
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
        let record: OperationRecord = serde_json::from_value(result.clone()).unwrap();
        assert_eq!(record.status, Status::Completed, "{result}");
        assert_eq!(chain.balance(&share(family)), 10);
        assert_eq!(chain.lamports(), initial_lamports - ACCOUNT_RENT - 5000);
        assert_eq!(approval.0.load(Ordering::SeqCst), 1);
        let durable: OperationRecord = serde_json::from_value(
            Store::open(&path)
                .unwrap()
                .get_record(&record.id)
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
            let record: OperationRecord = serde_json::from_value(result.clone()).unwrap();
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
            let durable: OperationRecord = serde_json::from_value(
                Store::open(&path)
                    .unwrap()
                    .get_record(&record.id)
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
            let recovered: OperationRecord = serde_json::from_value(status).unwrap();
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
async fn separate_jupiter_and_native_kamino_steps_preserve_receipt_floors_across_restart() {
    for family in [VaultFamily::Lending, VaultFamily::Liquidity] {
        let temp = tempfile::tempdir().unwrap();
        let (chain, wallet) = Chain::new();
        let approval = Arc::new(Approval::default());
        let mut client = Client::new(
            runtime(
                &temp.path().join("journal.sqlite"),
                chain.clone(),
                wallet.clone(),
                approval.clone(),
            ),
            temp.path(),
        )
        .await;
        let mut spec = plan(&chain, family);
        // Shares are a position source, not a cash debit. A receipt target must
        // remain executable under the finite share permission approved here.
        let receipt_floor = ResourceAmount {
            resource: chain.resource(),
            amount: Amount {
                asset: asset(UNDERLYING),
                atoms: Quantity::from_u64(50),
            },
        };
        spec.steps[2].completion.minimum_net_receipt = Some(receipt_floor.clone());
        let id = authorize(&client, spec).await;
        for (step, op, input) in [
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
        ] {
            if step == "withdraw" {
                let before = client
                    .call(
                        args(&["plans", "status", &id, "--json"]),
                        BrokerRequest::PlanStatus {
                            plan_id: id.clone(),
                        },
                        true,
                    )
                    .await;
                assert_eq!(before["next_eligible_step_id"], "withdraw", "{before}");
                assert_eq!(before["steps"][2]["state"], "not_started", "{before}");
                assert_eq!(chain.balance(&share(family)), 10);
                drop(client);
                client = Client::new(
                    runtime(
                        &temp.path().join("journal.sqlite"),
                        chain.clone(),
                        wallet.clone(),
                        approval.clone(),
                    ),
                    temp.path(),
                )
                .await;
                let after = client
                    .call(
                        args(&["plans", "status", &id, "--json"]),
                        BrokerRequest::PlanStatus {
                            plan_id: id.clone(),
                        },
                        true,
                    )
                    .await;
                assert_eq!(after["next_eligible_step_id"], "withdraw", "{after}");
                assert_eq!(after["revision"], before["revision"]);
                assert_eq!(after["usage"], before["usage"]);
                assert_eq!(chain.send_count.load(Ordering::SeqCst), 2);
                assert_eq!(approval.0.load(Ordering::SeqCst), 1);
            }
            let result = execute(&client, &id, step, op, input, true).await;
            assert_eq!(result["status"], "completed", "{result}");
        }
        assert_eq!(chain.balance(UNDERLYING), 10);
        assert_eq!(chain.balance(&share(family)), 5);
        assert_eq!(chain.send_count.load(Ordering::SeqCst), 4);
        assert_eq!(approval.0.load(Ordering::SeqCst), 1);
        let status = client
            .call(
                args(&["plans", "status", &id, "--json"]),
                BrokerRequest::PlanStatus { plan_id: id },
                true,
            )
            .await;
        assert!(status["next_eligible_step_id"].is_null(), "{status}");
        let status: FinancialPlanStatus = serde_json::from_value(status).unwrap();
        let withdraw = &status.steps[2];
        assert_eq!(withdraw.state, FinancialPlanStepProgressState::Completed);
        assert_eq!(withdraw.attempts.len(), 1);
        assert_eq!(withdraw.attempts[0].status, Status::Completed);
        assert!(withdraw.blockers.is_empty());
        assert!(withdraw.verified_receipts.iter().any(|receipt| {
            receipt.resource == receipt_floor.resource
                && receipt.asset == receipt_floor.amount.asset
                && receipt.available == receipt_floor.amount.atoms
        }));
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
    let id = authorize(&client, plan(&chain, VaultFamily::Lending)).await;
    let swap_result = execute(
        &client,
        &id,
        "entry-swap",
        "jupiter.swap",
        swap(INPUT, UNDERLYING, "100", "100"),
        true,
    )
    .await;
    assert_eq!(swap_result["status"], "completed");
    chain.reject_deposit.store(true, Ordering::SeqCst);
    let failure = execute(
        &client,
        &id,
        "deposit",
        "kamino.deposit",
        deposit(VaultFamily::Lending, "115"),
        false,
    )
    .await;
    assert!(failure["message"].as_str().unwrap().contains("unavailable"));
    assert_eq!(chain.balance(UNDERLYING), 125);
    assert_eq!(chain.send_count.load(Ordering::SeqCst), 1);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn restart_reconciles_original_swap_and_does_not_auto_execute_deposit() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("journal.sqlite");
    let (chain, wallet) = Chain::new();
    let approval = Arc::new(Approval::default());
    let client = Client::new(
        runtime(&path, chain.clone(), wallet.clone(), approval.clone()),
        temp.path(),
    )
    .await;
    let id = authorize(&client, plan(&chain, VaultFamily::Lending)).await;
    chain.lose_response.store(true, Ordering::SeqCst);
    chain.hide_receipts.store(true, Ordering::SeqCst);
    let result = execute(
        &client,
        &id,
        "entry-swap",
        "jupiter.swap",
        swap(INPUT, UNDERLYING, "100", "100"),
        true,
    )
    .await;
    assert_ne!(result["status"], "completed");
    assert_eq!(chain.send_count.load(Ordering::SeqCst), 1);
    drop(client);
    chain.hide_receipts.store(false, Ordering::SeqCst);
    chain.lose_response.store(false, Ordering::SeqCst);
    let client = Client::new(
        runtime(&path, chain.clone(), wallet, approval.clone()),
        temp.path(),
    )
    .await;
    let status = client
        .call(
            args(&["plans", "status", &id, "--json"]),
            BrokerRequest::PlanStatus {
                plan_id: id.clone(),
            },
            true,
        )
        .await;
    assert_eq!(status["next_eligible_step_id"], "deposit", "{status}");
    assert_eq!(chain.balance(&share(VaultFamily::Lending)), 0);
    assert_eq!(chain.send_count.load(Ordering::SeqCst), 1);
    let result = execute(
        &client,
        &id,
        "deposit",
        "kamino.deposit",
        deposit(VaultFamily::Lending, "115"),
        true,
    )
    .await;
    assert_eq!(result["status"], "completed", "{result}");
    assert_eq!(chain.send_count.load(Ordering::SeqCst), 2);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn lost_kamino_response_reconciles_original_submission_after_broker_restart() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("journal.sqlite");
    let (chain, wallet) = Chain::new();
    let approval = Arc::new(Approval::default());
    let client = Client::new(
        runtime(&path, chain.clone(), wallet.clone(), approval.clone()),
        temp.path(),
    )
    .await;
    let id = authorize(&client, plan(&chain, VaultFamily::Lending)).await;
    let entry = execute(
        &client,
        &id,
        "entry-swap",
        "jupiter.swap",
        swap(INPUT, UNDERLYING, "100", "100"),
        true,
    )
    .await;
    assert_eq!(entry["status"], "completed", "{entry}");
    chain.lose_response.store(true, Ordering::SeqCst);
    chain.hide_receipts.store(true, Ordering::SeqCst);
    let result = execute(
        &client,
        &id,
        "deposit",
        "kamino.deposit",
        deposit(VaultFamily::Lending, "115"),
        true,
    )
    .await;
    assert_ne!(result["status"], "completed");
    let original_submission = result["submission"].clone();
    assert_eq!(chain.balance(&share(VaultFamily::Lending)), 10);
    assert_eq!(chain.send_count.load(Ordering::SeqCst), 2);
    drop(client);
    chain.hide_receipts.store(false, Ordering::SeqCst);
    chain.lose_response.store(false, Ordering::SeqCst);
    let client = Client::new(
        runtime(&path, chain.clone(), wallet, approval.clone()),
        temp.path(),
    )
    .await;
    let status = client
        .call(
            args(&["plans", "status", &id, "--json"]),
            BrokerRequest::PlanStatus {
                plan_id: id.clone(),
            },
            true,
        )
        .await;
    assert_eq!(status["next_eligible_step_id"], "withdraw", "{status}");
    let replay = execute(
        &client,
        &id,
        "deposit",
        "kamino.deposit",
        deposit(VaultFamily::Lending, "115"),
        true,
    )
    .await;
    assert_eq!(replay["status"], "completed", "{replay}");
    assert_eq!(replay["submission"], original_submission);
    assert_eq!(
        chain.send_count.load(Ordering::SeqCst),
        2,
        "recovery must not rebroadcast or replay the deposit"
    );
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn durably_denied_native_submission_releases_reservation_and_survives_restart() {
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
    let record: OperationRecord = serde_json::from_value(result.clone()).unwrap();
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
    let durable: OperationRecord = serde_json::from_value(
        Store::open(&path)
            .unwrap()
            .get_record(&record.id)
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
    let recovered: OperationRecord = serde_json::from_value(status).unwrap();
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
        let original: OperationRecord = serde_json::from_value(result.clone()).unwrap();
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
        assert!(
            Store::open(&path)
                .unwrap()
                .reservation_count(&original.id)
                .unwrap()
                > 0
        );
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
        let pending: OperationRecord = serde_json::from_value(unresolved.clone()).unwrap();
        assert!(
            matches!(pending.status, Status::Pending | Status::Unknown),
            "Missing block history cannot prove absence: {unresolved}"
        );
        assert!(pending.effects.is_none());
        assert!(
            Store::open(&path)
                .unwrap()
                .reservation_count(&original.id)
                .unwrap()
                > 0
        );
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
        let pending: OperationRecord = serde_json::from_value(uncorroborated.clone()).unwrap();
        assert!(
            matches!(pending.status, Status::Pending | Status::Unknown),
            "One provider cannot release the reservation: {uncorroborated}"
        );
        assert!(pending.effects.is_none());
        assert!(
            Store::open(&path)
                .unwrap()
                .reservation_count(&original.id)
                .unwrap()
                > 0
        );
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
        let settled: OperationRecord = serde_json::from_value(result.clone()).unwrap();
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
        let durable: OperationRecord = serde_json::from_value(
            Store::open(&path)
                .unwrap()
                .get_record(&original.id)
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
        assert_eq!(
            serde_json::from_value::<OperationRecord>(recovered).unwrap(),
            settled
        );
        let replay = client
            .call(execute_args, BrokerRequest::Execute { request }, true)
            .await;
        assert_eq!(
            serde_json::from_value::<OperationRecord>(replay).unwrap(),
            settled
        );
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

struct Clock(std::sync::atomic::AtomicI64);
impl gofin_domain::clock::Clock for Clock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now() + chrono::Duration::seconds(self.0.load(Ordering::SeqCst))
    }
}
#[tokio::test]
async fn expired_native_permission_cannot_consume_settled_predecessor_proceeds() {
    let temp = tempfile::tempdir().unwrap();
    let (chain, wallet) = Chain::new();
    let approval = Arc::new(Approval::default());
    let clock = Arc::new(Clock(std::sync::atomic::AtomicI64::new(0)));
    let runtime = runtime_with_clock(
        &temp.path().join("journal.sqlite"),
        chain.clone(),
        wallet,
        approval.clone(),
        clock.clone(),
    );
    let client = Client::new(runtime, temp.path()).await;
    let id = authorize(&client, plan(&chain, VaultFamily::Lending)).await;
    let entry = execute(
        &client,
        &id,
        "entry-swap",
        "jupiter.swap",
        swap(INPUT, UNDERLYING, "100", "100"),
        true,
    )
    .await;
    assert_eq!(entry["status"], "completed", "{entry}");
    clock.0.store(901, Ordering::SeqCst);
    let error = execute(
        &client,
        &id,
        "deposit",
        "kamino.deposit",
        deposit(VaultFamily::Lending, "115"),
        false,
    )
    .await;
    assert!(
        error["code"].as_str().unwrap().contains("EXPIRED")
            || error["code"].as_str().unwrap().contains("INACTIVE"),
        "{error}"
    );
    assert_eq!(chain.balance(UNDERLYING), 125);
    assert_eq!(chain.send_count.load(Ordering::SeqCst), 1);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
}
