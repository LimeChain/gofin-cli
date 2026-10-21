//! Preparation and approval-boundary regressions using real broker/adapters.
//! The swap transport models the Jupiter envelope and account observations,
//! not AMM execution. Absent intermediate accounts are preparation-only;
//! settlement coverage uses an existing zero account with its unchanged
//! token/rent balance rows explicitly included in the synthetic receipt.
use super::*;
use gofin_domain::{Adapter, ApprovalAudit, Authorizer, PreparedOperation};
use gofin_protocol::BrokerRequest;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
enum CandidateMode {
    Intermediate,
    UnreferencedSetup,
    WrongPda,
    ForeignAuthority,
    DirectWhenConstrained,
    OmittedSetup,
    UnsyncedNative { omit_setup: bool },
}

struct IntermediateApi {
    inner: Arc<FixtureJupiterApi>,
    mode: CandidateMode,
    limits: Mutex<Vec<Option<u8>>>,
}

fn api_instruction(instruction: &Instruction) -> Value {
    json!({
        "programId":instruction.program_id.to_string(),
        "accounts":instruction.accounts.iter().map(|account| json!({
            "pubkey":account.pubkey.to_string(),
            "isSigner":account.is_signer,
            "isWritable":account.is_writable
        })).collect::<Vec<_>>(),
        "data":STANDARD.encode(&instruction.data)
    })
}

#[async_trait]
impl gofin_jupiter::JupiterApi for IntermediateApi {
    async fn request(
        &self,
        host: gofin_jupiter::JupiterHost,
        method: Method,
        path: &str,
        body: Option<Value>,
        bucket: gofin_jupiter::RateBucket,
        priority: gofin_jupiter::RequestPriority,
        max_wait: Duration,
    ) -> Result<Value> {
        let mut response = self
            .inner
            .request(host, method, path, body, bucket, priority, max_wait)
            .await?;
        if !path.starts_with("/swap/v2/build?") {
            return Ok(response);
        }
        let limit = query_parameter(path, "maxAccounts")
            .map(str::parse::<u8>)
            .transpose()
            .map_err(|_| fixture_state_error())?;
        self.limits.lock().unwrap().push(limit);
        let amount = query_parameter(path, "amount")
            .ok_or_else(fixture_state_error)?
            .parse::<u64>()
            .map_err(|_| fixture_state_error())?;
        let mut route = self.inner.swap_instruction(
            amount,
            PYUSD_MINT,
            SWAP_OUTPUT_MINT,
            FixtureSwapLayout::RouteV2,
        )?;
        if matches!(self.mode, CandidateMode::DirectWhenConstrained) && limit.is_some() {
            response["swapInstruction"] = api_instruction(&route);
            return Ok(response);
        }

        let wallet = self.inner.wallet.parse::<Pubkey>().unwrap();
        let intermediate = intermediate_address(&self.inner.wallet)
            .parse::<Pubkey>()
            .unwrap();
        let mut setup = create_wsol_ata_instruction(&wallet, &intermediate)?;
        match self.mode {
            CandidateMode::WrongPda => setup.accounts[1].pubkey = Pubkey::new_from_array([71; 32]),
            CandidateMode::ForeignAuthority => {
                setup.accounts[2].pubkey = Pubkey::new_from_array([72; 32]);
            }
            _ => {}
        }
        if !matches!(self.mode, CandidateMode::UnreferencedSetup) {
            route
                .accounts
                .push(AccountMeta::new(setup.accounts[1].pubkey, false));
        }
        // Keep a real route_v2 exact-input envelope, with two consecutive
        // no-payload MeteoraDlmm steps (input -> intermediate -> output).
        // This fixture deliberately does not simulate those AMMs.
        route.data.truncate(30);
        route.data.extend_from_slice(&2u32.to_le_bytes());
        for (input_index, output_index) in [(0, 1), (1, 2)] {
            route.data.push(38);
            route.data.extend_from_slice(&10_000u16.to_le_bytes());
            route.data.extend_from_slice(&[input_index, output_index]);
        }
        response["swapInstruction"] = api_instruction(&route);
        if !matches!(
            self.mode,
            CandidateMode::OmittedSetup | CandidateMode::UnsyncedNative { omit_setup: true }
        ) {
            response["setupInstructions"]
                .as_array_mut()
                .ok_or_else(fixture_state_error)?
                .push(api_instruction(&setup));
        }
        Ok(response)
    }
}

struct IntermediateRpc {
    inner: Arc<FixtureRpc>,
    unsynced_native_lamports: bool,
}

#[async_trait]
impl SolanaRpc for IntermediateRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let before = if method == "sendTransaction" {
            let state = self.inner.0.lock().unwrap();
            let account = intermediate_address(&state.wallet);
            state.account_data.contains_key(&account).then(|| {
                (
                    state.wallet.clone(),
                    account.clone(),
                    state.token_amounts[&account],
                )
            })
        } else {
            None
        };
        let mut response = self.inner.request(method, params.clone()).await?;
        if self.unsynced_native_lamports {
            let account = intermediate_address(&self.inner.0.lock().unwrap().wallet);
            let add_lamport = |value: &mut Value| -> Result<()> {
                let lamports = value["lamports"].as_u64().ok_or_else(fixture_state_error)?;
                value["lamports"] = json!(lamports + 1);
                Ok(())
            };
            if method == "getAccountInfo" && params[0] == account {
                add_lamport(&mut response["value"])?;
            } else if method == "getMultipleAccounts" {
                for (index, address) in params[0]
                    .as_array()
                    .ok_or_else(fixture_state_error)?
                    .iter()
                    .enumerate()
                {
                    if address == &account {
                        add_lamport(&mut response["value"][index])?;
                    }
                }
            }
        }
        if let Some((wallet, account, amount_before)) = before {
            let mut state = self.inner.0.lock().unwrap();
            let amount_after = state.token_amounts[&account];
            let transaction = state
                .transactions
                .iter_mut()
                .find(|transaction| Some(transaction.signature.as_str()) == response.as_str())
                .ok_or_else(fixture_state_error)?;
            let bytes = STANDARD
                .decode(&transaction.wire)
                .map_err(|_| fixture_state_error())?;
            let wire: VersionedTransaction = bincode::DefaultOptions::new()
                .with_fixint_encoding()
                .with_little_endian()
                .reject_trailing_bytes()
                .with_limit(128 * 1024)
                .deserialize(&bytes)
                .map_err(|_| fixture_state_error())?;
            if let Some(index) = wire
                .message
                .static_account_keys()
                .iter()
                .position(|key| key.to_string() == account)
            {
                // The base ledger changes only endpoint balances. For an
                // existing intermediate ATA its idempotent create is a no-op.
                // Include the actual before/after values instead of allowing
                // omission of the intermediate from the settlement proof.
                transaction.pre_balances[index] = WSOL_ACCOUNT_RENT_LAMPORTS + amount_before;
                transaction.post_balances[index] = WSOL_ACCOUNT_RENT_LAMPORTS + amount_after;
                transaction.pre_token_balances.push(token_balance(
                    index,
                    SWAP_INPUT_MINT,
                    &wallet,
                    gofin_solana::token::TOKEN_PROGRAM_ID,
                    9,
                    amount_before,
                ));
                transaction.post_token_balances.push(token_balance(
                    index,
                    SWAP_INPUT_MINT,
                    &wallet,
                    gofin_solana::token::TOKEN_PROGRAM_ID,
                    9,
                    amount_after,
                ));
            }
        }
        Ok(response)
    }
}

struct IntermediateAuthorizer {
    calls: Arc<AtomicUsize>,
    fund_after_approval: bool,
    rpc: Arc<FixtureRpc>,
}

#[async_trait]
impl Authorizer for IntermediateAuthorizer {
    async fn authorize(
        &self,
        challenge: &gofin_domain::AuthorizationChallenge,
    ) -> Result<ApprovalAudit> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        crate::validate_development_challenge(challenge)?;
        if self.fund_after_approval {
            let mut state = self.rpc.0.lock().unwrap();
            let account = intermediate_address(&state.wallet);
            state.token_amounts.insert(account, 1);
        }
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

struct Harness {
    runtime: Arc<gofin_broker::BrokerRuntime>,
    adapter: Arc<dyn Adapter>,
    rpc: Arc<FixtureRpc>,
    api: Arc<IntermediateApi>,
    approvals: Arc<AtomicUsize>,
    _directory: tempfile::TempDir,
}

fn intermediate_address(wallet: &str) -> String {
    gofin_solana::associated_token_address(
        wallet,
        SWAP_INPUT_MINT,
        gofin_solana::token::TOKEN_PROGRAM_ID,
    )
    .unwrap()
}

fn harness(mode: CandidateMode, initial_amount: Option<u64>, fund_after_approval: bool) -> Harness {
    let (config, _, rpc, fixture_api) = applications_with_fixture().unwrap();
    {
        let mut state = rpc.0.lock().unwrap();
        let account = intermediate_address(&state.wallet);
        match initial_amount {
            Some(amount) => {
                state.token_amounts.insert(account, amount);
            }
            None => {
                state.account_data.remove(&account);
                state.token_amounts.remove(&account);
            }
        }
    }
    let credentials: Arc<dyn BrokerCredentialProvider> = Arc::new(FixtureCredentials {
        reference: "00000000-0000-4000-8000-000000000007",
        signer: Arc::new(SolanaSigner::parse(SYNTHETIC_KEYPAIR).unwrap()),
        auxiliary_signers: Mutex::new(BTreeMap::new()),
    });
    let api = Arc::new(IntermediateApi {
        inner: fixture_api,
        mode,
        limits: Mutex::new(vec![]),
    });
    let applications = gofin_broker::build_applications_with_network_transports(
        &config,
        credentials,
        Arc::new(FixturePrices),
        BTreeMap::new(),
        Some(Arc::new(IntermediateRpc {
            inner: rpc.clone(),
            unsynced_native_lamports: matches!(mode, CandidateMode::UnsyncedNative { .. }),
        })),
        Some(api.clone()),
    )
    .unwrap();
    let adapter = applications
        .adapters
        .iter()
        .find(|adapter| {
            adapter
                .definitions()
                .iter()
                .any(|d| d.name == "jupiter.swap")
        })
        .expect("real Jupiter adapter")
        .clone();
    let approvals = Arc::new(AtomicUsize::new(0));
    let directory = tempfile::tempdir().unwrap();
    let runtime = gofin_broker::compose_runtime(
        config,
        gofin_broker::RuntimeDependencies {
            store: gofin_store::Store::open(&directory.path().join("operations.sqlite")).unwrap(),
            adapters: applications.adapters,
            authorizer: Arc::new(IntermediateAuthorizer {
                calls: approvals.clone(),
                fund_after_approval,
                rpc: rpc.clone(),
            }),
            valuator: applications.valuator,
            bridge: applications.bridge,
        },
    )
    .unwrap();
    Harness {
        runtime,
        adapter,
        rpc,
        api,
        approvals,
        _directory: directory,
    }
}

fn swap_input(funding_cap: u64) -> Value {
    json!({
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
        "maximum_account_funding_lamports":funding_cap,
        "route":"router"
    })
}

async fn preview(harness: &Harness, funding_cap: u64) -> gofin_protocol::BrokerResponse {
    harness
        .runtime
        .handle(BrokerRequest::Preview {
            operation: "jupiter.swap".into(),
            input: swap_input(funding_cap),
        })
        .await
}

#[tokio::test]
async fn absent_intermediate_ata_prepares_with_exact_rent_and_frozen_revalidation() {
    let _fixture = FIXTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let harness = harness(CandidateMode::Intermediate, None, false);
    let response = preview(&harness, WSOL_ACCOUNT_RENT_LAMPORTS).await;
    assert!(response.ok, "preview failed: {:?}", response.error);
    let prepared: PreparedOperation = serde_json::from_value(response.result.unwrap()).unwrap();
    let account = intermediate_address(&harness.api.inner.wallet);
    assert_eq!(
        prepared.binding["account_funding_lamports"],
        WSOL_ACCOUNT_RENT_LAMPORTS
    );
    let funding = prepared.binding["funding_accounts"].as_array().unwrap();
    assert_eq!(funding.len(), 1);
    assert_eq!(funding[0]["address"], account);
    assert_eq!(funding[0]["lamports"], WSOL_ACCOUNT_RENT_LAMPORTS);
    let anchors = prepared.binding[gofin_domain::BASELINE_ANCHORS_KEY]
        .as_array()
        .unwrap();
    assert_eq!(anchors.len(), 1);
    assert_eq!(anchors[0]["position"], format!("account-funding:{account}"));
    assert_eq!(anchors[0]["principal"]["atoms"], "0");
    assert_eq!(
        anchors[0]["evidence"]["observation"]["account_absence_proven"],
        true
    );
    let refreshed = harness.adapter.revalidate(&prepared).await.unwrap();
    assert_eq!(
        refreshed.binding["message_sha256"],
        prepared.binding["message_sha256"]
    );
    assert_eq!(harness.api.limits.lock().unwrap().as_slice(), &[None]);
    assert_eq!(harness.approvals.load(Ordering::SeqCst), 0);
    assert!(harness.rpc.0.lock().unwrap().transactions.is_empty());
}

#[tokio::test]
async fn zero_intermediate_ata_prepares_without_new_rent_or_wallet_debit() {
    let _fixture = FIXTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let harness = harness(CandidateMode::Intermediate, Some(0), false);
    let response = preview(&harness, 0).await;
    assert!(response.ok, "preview failed: {:?}", response.error);
    let prepared: PreparedOperation = serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(prepared.binding["account_funding_lamports"], 0);
    assert!(
        prepared.binding["funding_accounts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    harness.adapter.revalidate(&prepared).await.unwrap();
    assert_eq!(harness.api.limits.lock().unwrap().as_slice(), &[None]);
    assert_eq!(harness.approvals.load(Ordering::SeqCst), 0);
    assert!(harness.rpc.0.lock().unwrap().transactions.is_empty());
}

#[tokio::test]
async fn funded_intermediate_rejects_all_bounded_router_candidates_before_approval() {
    let _fixture = FIXTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let harness = harness(CandidateMode::Intermediate, Some(1), false);
    let response = preview(&harness, WSOL_ACCOUNT_RENT_LAMPORTS).await;
    assert!(!response.ok);
    assert_eq!(
        response.error.unwrap().code,
        "JUPITER_INTERMEDIATE_BALANCE_UNSUPPORTED"
    );
    assert_eq!(
        harness.api.limits.lock().unwrap().as_slice(),
        &[None, Some(48), Some(32)]
    );
    assert_eq!(harness.approvals.load(Ordering::SeqCst), 0);
    assert!(harness.rpc.0.lock().unwrap().transactions.is_empty());
}

#[tokio::test]
async fn funded_intermediate_cannot_bypass_balance_guard_by_omitting_setup() {
    let _fixture = FIXTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let harness = harness(CandidateMode::OmittedSetup, Some(1), false);
    let response = preview(&harness, 0).await;
    assert!(!response.ok);
    assert_eq!(
        response.error.unwrap().code,
        "JUPITER_INTERMEDIATE_BALANCE_UNSUPPORTED"
    );
    assert_eq!(
        harness.api.limits.lock().unwrap().as_slice(),
        &[None, Some(48), Some(32)]
    );
    assert_eq!(harness.approvals.load(Ordering::SeqCst), 0);
    assert!(harness.rpc.0.lock().unwrap().transactions.is_empty());
}

#[tokio::test]
async fn zero_recorded_wsol_with_unsynced_sol_rejects_with_or_without_setup() {
    let _fixture = FIXTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for omit_setup in [false, true] {
        let harness = harness(CandidateMode::UnsyncedNative { omit_setup }, Some(0), false);
        let response = preview(&harness, 0).await;
        assert!(
            !response.ok,
            "unsynced SOL accepted, omit_setup={omit_setup}"
        );
        assert_eq!(
            response.error.unwrap().code,
            "JUPITER_INTERMEDIATE_BALANCE_UNSUPPORTED"
        );
        assert_eq!(
            harness.api.limits.lock().unwrap().as_slice(),
            &[None, Some(48), Some(32)]
        );
        assert_eq!(harness.approvals.load(Ordering::SeqCst), 0);
        let state = harness.rpc.0.lock().unwrap();
        assert_eq!(state.token_amounts[&intermediate_address(&state.wallet)], 0);
        assert!(state.transactions.is_empty());
    }
}

#[tokio::test]
async fn cached_zero_intermediate_dispatch_settles_once_with_unchanged_balance_proof() {
    let _fixture = FIXTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let harness = harness(CandidateMode::Intermediate, Some(0), false);
    let request = BrokerRequest::Execute {
        request: gofin_domain::OperationRequest {
            request_id: "intermediate-existing-zero-settlement".into(),
            operation: "jupiter.swap".into(),
            input: swap_input(0),
            run_id: None,
        },
    };
    let response = harness.runtime.handle(request.clone()).await;
    assert!(response.ok, "execution failed: {:?}", response.error);
    let result = response.result.unwrap();
    assert_eq!(result["status"], "completed");
    let effects = result["effects"]["effects"].as_array().unwrap();
    assert!(effects.iter().any(|effect| {
        effect["kind"] == "conversion"
            && effect["spent"]["atoms"] == "1000000"
            && effect["received"]["atoms"] == "1000000"
    }));
    let repeated = harness.runtime.handle(request).await;
    assert!(repeated.ok);
    assert_eq!(repeated.result.unwrap()["id"], result["id"]);
    assert_eq!(harness.api.limits.lock().unwrap().as_slice(), &[None]);
    assert_eq!(harness.approvals.load(Ordering::SeqCst), 1);
    let state = harness.rpc.0.lock().unwrap();
    assert_eq!(state.transactions.len(), 1);
    assert_eq!(state.token_amounts[&intermediate_address(&state.wallet)], 0);
    for rows in [
        &state.transactions[0].pre_token_balances,
        &state.transactions[0].post_token_balances,
    ] {
        assert!(rows.iter().any(|row| {
            row["mint"] == SWAP_INPUT_MINT && row["uiTokenAmount"]["amount"] == "0"
        }));
    }
}

#[tokio::test]
async fn constrained_router_can_avoid_existing_intermediate_holdings_without_changing_intent() {
    let _fixture = FIXTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let harness = harness(CandidateMode::DirectWhenConstrained, Some(1), false);
    let response = preview(&harness, 0).await;
    assert!(response.ok, "preview failed: {:?}", response.error);
    let prepared: PreparedOperation = serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(prepared.input, swap_input(0));
    assert_eq!(
        harness.api.limits.lock().unwrap().as_slice(),
        &[None, Some(48)]
    );
    harness.adapter.revalidate(&prepared).await.unwrap();
    let state = harness.rpc.0.lock().unwrap();
    assert_eq!(state.token_amounts[&intermediate_address(&state.wallet)], 1);
    assert!(state.transactions.is_empty());
}

#[tokio::test]
async fn funding_intermediate_during_approval_blocks_dispatch_without_requote() {
    let _fixture = FIXTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let harness = harness(CandidateMode::Intermediate, Some(0), true);
    let response = harness
        .runtime
        .handle(BrokerRequest::Execute {
            request: gofin_domain::OperationRequest {
                request_id: "intermediate-funded-during-approval".into(),
                operation: "jupiter.swap".into(),
                input: swap_input(0),
                run_id: None,
            },
        })
        .await;
    // A rejection after approval is a durable failed operation, returned in
    // the broker's successful request envelope; it must not have a submission.
    assert!(response.ok, "execute request failed: {:?}", response.error);
    let record = response.result.unwrap();
    assert_eq!(record["status"], "failed");
    assert_eq!(
        record["error"]["code"],
        "JUPITER_INTERMEDIATE_BALANCE_UNSUPPORTED"
    );
    assert!(record["submission"].is_null());
    assert_eq!(harness.approvals.load(Ordering::SeqCst), 1);
    assert_eq!(harness.api.limits.lock().unwrap().as_slice(), &[None]);
    assert!(harness.rpc.0.lock().unwrap().transactions.is_empty());
}

#[tokio::test]
async fn intermediate_setup_retains_rent_cap_route_membership_and_account_graph_checks() {
    let _fixture = FIXTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let harness = harness(CandidateMode::Intermediate, None, false);
    let response = preview(&harness, WSOL_ACCOUNT_RENT_LAMPORTS - 1).await;
    assert!(!response.ok);
    assert_eq!(
        response.error.unwrap().code,
        "JUPITER_ACCOUNT_FUNDING_LIMIT_EXCEEDED"
    );
    assert_eq!(harness.api.limits.lock().unwrap().as_slice(), &[None]);
    for mode in [
        CandidateMode::UnreferencedSetup,
        CandidateMode::WrongPda,
        CandidateMode::ForeignAuthority,
    ] {
        let harness = self::harness(mode, None, false);
        let response = preview(&harness, WSOL_ACCOUNT_RENT_LAMPORTS).await;
        assert!(!response.ok);
        assert_eq!(
            response.error.unwrap().code,
            "JUPITER_INSTRUCTION_UNREVIEWED"
        );
        assert_eq!(harness.api.limits.lock().unwrap().as_slice(), &[None]);
        assert_eq!(harness.approvals.load(Ordering::SeqCst), 0);
        assert!(harness.rpc.0.lock().unwrap().transactions.is_empty());
    }
}
