//! Provider-trusted intermediate-account regressions through the real broker.
//! Preparation trusts the order's transaction, while finalized receipts still
//! account for every wallet token delta and retain immutable submission identity.
use super::*;
use gofin_domain::{Adapter, ApprovalAudit, Authorizer, PreparedOperation};
use gofin_protocol::BrokerRequest;
use solana_instruction::AccountMeta;
use solana_pubkey::Pubkey;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
enum CandidateMode {
    Intermediate,
    UnreferencedSetup,
    WrongPda,
    ForeignAuthority,
    OmittedSetup,
}

struct IntermediateApi {
    inner: Arc<FixtureJupiterApi>,
    mode: CandidateMode,
    orders: AtomicUsize,
    drain_intermediate: bool,
}

fn intermediate_address(wallet: &str) -> String {
    gofin_solana::associated_token_address(
        wallet,
        SWAP_INPUT_MINT,
        gofin_solana::token::TOKEN_PROGRAM_ID,
    )
    .unwrap()
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
        let before = if path == "/swap/v2/execute" {
            let state = self.inner.rpc.0.lock().unwrap();
            let account = intermediate_address(&state.wallet);
            state
                .token_amounts
                .get(&account)
                .map(|amount| (state.wallet.clone(), account, *amount))
        } else {
            None
        };
        let mut response = self
            .inner
            .request(host, method, path, body, bucket, priority, max_wait)
            .await?;
        if let Some((wallet, account, amount_before)) = before {
            // The base fixture models endpoint execution. Add the intermediate
            // account's actual unchanged (or intentionally drained) receipt rows.
            let mut state = self.inner.rpc.0.lock().unwrap();
            let amount_after = if self.drain_intermediate {
                0
            } else {
                amount_before
            };
            state.token_amounts.insert(account.clone(), amount_after);
            let transaction = state
                .transactions
                .iter_mut()
                .find(|transaction| {
                    Some(transaction.signature.as_str()) == response["signature"].as_str()
                })
                .ok_or_else(fixture_state_error)?;
            let wire = decode_transaction(&transaction.wire)?;
            if let Some(index) = wire
                .message
                .static_account_keys()
                .iter()
                .position(|key| key.to_string() == account)
            {
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
        if !path.starts_with("/swap/v2/order?") {
            return Ok(response);
        }
        assert!(query_parameter(path, "slippageBps").is_none());
        assert!(query_parameter(path, "maxAccounts").is_none());
        assert!(query_parameter(path, "excludeRouters").is_none());
        self.orders.fetch_add(1, Ordering::SeqCst);
        let amount = query_parameter(path, "amount")
            .ok_or_else(fixture_state_error)?
            .parse::<u64>()
            .map_err(|_| fixture_state_error())?;
        let original = decode_transaction(
            response["transaction"]
                .as_str()
                .ok_or_else(fixture_state_error)?,
        )?;
        let mut route = self.inner.swap_instruction(
            amount,
            PYUSD_MINT,
            SWAP_OUTPUT_MINT,
            FixtureSwapLayout::RouteV2,
        )?;
        let wallet = self.inner.wallet.parse::<Pubkey>().unwrap();
        let account = intermediate_address(&self.inner.wallet);
        let intermediate = account.parse::<Pubkey>().unwrap();
        let mut setup = create_wsol_ata_instruction(&wallet, &intermediate)?;
        match self.mode {
            CandidateMode::WrongPda => setup.accounts[1].pubkey = Pubkey::new_from_array([71; 32]),
            CandidateMode::ForeignAuthority => {
                setup.accounts[2].pubkey = Pubkey::new_from_array([72; 32])
            }
            _ => {}
        }
        if !matches!(self.mode, CandidateMode::UnreferencedSetup) {
            route
                .accounts
                .push(AccountMeta::new(setup.accounts[1].pubkey, false));
        }
        // The mock route has two hops. GoFin must not need this envelope's IDL
        // or inspect its intermediate accounts to sign the provider's order.
        route.data.truncate(30);
        route.data.extend_from_slice(&2u32.to_le_bytes());
        for (input_index, output_index) in [(0, 1), (1, 2)] {
            route.data.push(38);
            route.data.extend_from_slice(&10_000u16.to_le_bytes());
            route.data.extend_from_slice(&[input_index, output_index]);
        }
        let mut instructions = vec![];
        if !matches!(self.mode, CandidateMode::OmittedSetup) {
            instructions.push(setup);
        }
        instructions.push(route);
        let message = solana_message::Message::new_with_blockhash(
            &instructions,
            Some(&wallet),
            original.message.recent_blockhash(),
        );
        let transaction = VersionedTransaction {
            signatures: vec![
                solana_signature::Signature::default();
                usize::from(message.header.num_required_signatures)
            ],
            message: solana_message::VersionedMessage::Legacy(message),
        };
        response["transaction"] = json!(
            STANDARD.encode(
                bincode::DefaultOptions::new()
                    .with_fixint_encoding()
                    .with_little_endian()
                    .serialize(&transaction)
                    .map_err(|_| fixture_state_error())?
            )
        );
        let absent = !self
            .inner
            .rpc
            .0
            .lock()
            .unwrap()
            .account_data
            .contains_key(&account);
        let rent = if absent && !matches!(self.mode, CandidateMode::OmittedSetup) {
            WSOL_ACCOUNT_RENT_LAMPORTS
        } else {
            0
        };
        response["rentFeeLamports"] = json!(rent);
        response["rentFeePayer"] = if rent == 0 {
            Value::Null
        } else {
            json!(self.inner.wallet)
        };
        Ok(response)
    }
}

fn decode_transaction(encoded: &str) -> Result<VersionedTransaction> {
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| fixture_state_error())?;
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .deserialize(&bytes)
        .map_err(|_| fixture_state_error())
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

fn harness(
    mode: CandidateMode,
    initial_amount: Option<u64>,
    fund_after_approval: bool,
    drain_intermediate: bool,
) -> Harness {
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
        orders: AtomicUsize::new(0),
        drain_intermediate,
    });
    let applications = gofin_broker::build_applications_with_network_transports(
        &config,
        credentials,
        Arc::new(FixturePrices),
        BTreeMap::new(),
        Some(rpc.clone()),
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
                .any(|definition| definition.name == "jupiter.swap")
        })
        .expect("real Jupiter adapter")
        .clone();
    let approvals = Arc::new(AtomicUsize::new(0));
    let directory = tempfile::tempdir().unwrap();
    let runtime = gofin_broker::compose_runtime(
        config,
        gofin_broker::RuntimeDependencies {
            store: gofin_store::Store::open(directory.path().join("operations.sqlite")).unwrap(),
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
        "wallet":"solana-rehearsal", "network":"solana", "cluster":"mainnet-beta",
        "input_mint":PYUSD_MINT, "output_mint":SWAP_OUTPUT_MINT,
        "amount_atoms":"1000000", "minimum_output_atoms":"995000",
        "maximum_slippage_bps":50, "maximum_platform_fee_bps":0,
        "maximum_network_fee_lamports":10000, "maximum_account_funding_lamports":funding_cap
    })
}

#[tokio::test]
async fn intermediate_shapes_and_balances_are_trusted_without_requote_or_account_graph_validation()
{
    let _fixture = FIXTURE_LOCK.lock().await;
    for mode in [
        CandidateMode::Intermediate,
        CandidateMode::UnreferencedSetup,
        CandidateMode::WrongPda,
        CandidateMode::ForeignAuthority,
        CandidateMode::OmittedSetup,
    ] {
        for initial in [None, Some(0), Some(1)] {
            let harness = harness(mode, initial, false, false);
            let response = harness
                .runtime
                .handle(BrokerRequest::Preview {
                    operation: "jupiter.swap".into(),
                    input: swap_input(WSOL_ACCOUNT_RENT_LAMPORTS),
                })
                .await;
            assert!(
                response.ok,
                "provider order was rejected: {:?}",
                response.error
            );
            let prepared: PreparedOperation =
                serde_json::from_value(response.result.unwrap()).unwrap();
            let refreshed = harness.adapter.revalidate(&prepared).await.unwrap();
            assert_eq!(
                refreshed.binding["message_sha256"],
                prepared.binding["message_sha256"]
            );
            assert!(prepared.binding.get("funding_accounts").is_none());
            assert_eq!(harness.api.orders.load(Ordering::SeqCst), 1);
            assert_eq!(harness.approvals.load(Ordering::SeqCst), 0);
            let state = harness.rpc.0.lock().unwrap();
            assert!(state.transactions.is_empty());
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
            assert!(
                !state
                    .get_account_info_addresses
                    .contains(&intermediate_address(&state.wallet))
            );
        }
    }
}

#[tokio::test]
async fn quoted_intermediate_rent_is_bounded_before_approval() {
    let _fixture = FIXTURE_LOCK.lock().await;
    let harness = harness(CandidateMode::Intermediate, None, false, false);
    let response = harness
        .runtime
        .handle(BrokerRequest::Preview {
            operation: "jupiter.swap".into(),
            input: swap_input(WSOL_ACCOUNT_RENT_LAMPORTS - 1),
        })
        .await;
    assert!(!response.ok);
    assert_eq!(
        response.error.unwrap().code,
        "JUPITER_ACCOUNT_FUNDING_LIMIT_EXCEEDED"
    );
    assert_eq!(harness.api.orders.load(Ordering::SeqCst), 1);
    assert_eq!(harness.approvals.load(Ordering::SeqCst), 0);
    assert!(harness.rpc.0.lock().unwrap().transactions.is_empty());
}

#[tokio::test]
async fn unchanged_intermediate_holdings_settle_once_even_when_funded_during_approval() {
    let _fixture = FIXTURE_LOCK.lock().await;
    for (initial, fund_after_approval, expected) in [(0, false, 0), (1, false, 1), (0, true, 1)] {
        let harness = harness(
            CandidateMode::Intermediate,
            Some(initial),
            fund_after_approval,
            false,
        );
        let request = BrokerRequest::Execute {
            request: gofin_domain::OperationRequest {
                request_id: "trusted-intermediate-settlement".into(),
                operation: "jupiter.swap".into(),
                input: swap_input(0),
                run_id: None,
            },
        };
        let response = harness.runtime.handle(request.clone()).await;
        assert!(
            response.ok,
            "execution request failed: {:?}",
            response.error
        );
        let record = response.result.unwrap();
        assert_eq!(record["status"], "completed", "{record}");
        assert!(
            record["effects"]["effects"]
                .as_array()
                .unwrap()
                .iter()
                .any(|effect| {
                    effect["kind"] == "conversion"
                        && effect["spent"]["atoms"] == "1000000"
                        && effect["received"]["atoms"] == "1000000"
                })
        );
        let duplicate = harness.runtime.handle(request).await;
        assert!(duplicate.ok);
        assert_eq!(duplicate.result.unwrap()["id"], record["id"]);
        assert_eq!(harness.api.orders.load(Ordering::SeqCst), 1);
        assert_eq!(harness.approvals.load(Ordering::SeqCst), 1);
        let state = harness.rpc.0.lock().unwrap();
        assert_eq!(state.transactions.len(), 1);
        assert_eq!(
            state.token_amounts[&intermediate_address(&state.wallet)],
            expected
        );
        let expected_text = expected.to_string();
        for rows in [
            &state.transactions[0].pre_token_balances,
            &state.transactions[0].post_token_balances,
        ] {
            assert!(rows.iter().any(|row| row["mint"] == SWAP_INPUT_MINT
                && row["uiTokenAmount"]["amount"] == expected_text));
        }
    }
}

#[tokio::test]
async fn unexpected_intermediate_debit_retains_submission_without_claiming_settlement_or_replaying()
{
    let _fixture = FIXTURE_LOCK.lock().await;
    let harness = harness(CandidateMode::Intermediate, Some(1), false, true);
    let request = BrokerRequest::Execute {
        request: gofin_domain::OperationRequest {
            request_id: "unexpected-intermediate-debit".into(),
            operation: "jupiter.swap".into(),
            input: swap_input(0),
            run_id: None,
        },
    };
    let response = harness.runtime.handle(request.clone()).await;
    assert!(
        response.ok,
        "execution request failed: {:?}",
        response.error
    );
    let record = response.result.unwrap();
    assert_eq!(record["status"], "pending", "{record}");
    assert_eq!(
        record["last_reconciliation_evidence"]["latestReconciliationError"]["code"],
        "JUPITER_SETTLEMENT_EFFECT_MISMATCH"
    );
    assert!(record["submission"].is_object());
    assert!(record["effects"].is_null());
    let duplicate = harness.runtime.handle(request).await;
    assert!(duplicate.ok);
    let duplicate = duplicate.result.unwrap();
    assert_eq!(duplicate["id"], record["id"]);
    assert_eq!(duplicate["submission"], record["submission"]);
    assert_eq!(harness.api.orders.load(Ordering::SeqCst), 1);
    assert_eq!(harness.approvals.load(Ordering::SeqCst), 1);
    assert_eq!(harness.rpc.0.lock().unwrap().transactions.len(), 1);
}
