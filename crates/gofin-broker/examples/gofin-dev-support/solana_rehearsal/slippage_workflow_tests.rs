use super::*;
use gofin_domain::{ApprovalAudit, Authorizer};
use gofin_protocol::BrokerRequest;
use std::sync::atomic::{AtomicUsize, Ordering};

struct SlippageOrderApi {
    inner: Arc<FixtureJupiterApi>,
    paths: Mutex<Vec<String>>,
    response_overrides: Mutex<BTreeMap<String, Value>>,
    token_metadata: Mutex<Value>,
    token_requests: AtomicUsize,
}

#[async_trait]
impl gofin_jupiter::JupiterApi for SlippageOrderApi {
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
        if path.starts_with("/tokens/v2/search?query=") {
            self.token_requests.fetch_add(1, Ordering::SeqCst);
            return Ok(self.token_metadata.lock().unwrap().clone());
        }
        let mut response = self
            .inner
            .request(host, method.clone(), path, body, bucket, priority, max_wait)
            .await?;
        if path.starts_with("/swap/v2/order?") && method == Method::GET {
            self.paths.lock().unwrap().push(path.to_owned());
            assert!(
                query_parameter(path, "slippageBps").is_none(),
                "RTSE must select slippage"
            );
            assert!(
                query_parameter(path, "excludeRouters").is_none(),
                "provider engines are trusted"
            );
            let encoded = response["transaction"]
                .as_str()
                .ok_or_else(fixture_state_error)?;
            response["transaction"] = json!(append_opaque_provider_instruction(encoded)?);
            for (key, value) in self.response_overrides.lock().unwrap().iter() {
                response[key] = value.clone();
            }
        }
        Ok(response)
    }
}

// The fixture models a future provider program unknown to GoFin. Its no-op is
// deliberately opaque: the adapter must sign the returned message unchanged.
fn append_opaque_provider_instruction(encoded: &str) -> Result<String> {
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| fixture_state_error())?;
    let mut transaction: VersionedTransaction = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .deserialize(&bytes)
        .map_err(|_| fixture_state_error())?;
    let solana_message::VersionedMessage::Legacy(message) = &mut transaction.message else {
        return Err(fixture_state_error());
    };
    let program_id_index =
        u8::try_from(message.account_keys.len()).map_err(|_| fixture_state_error())?;
    message
        .account_keys
        .push(solana_pubkey::Pubkey::new_from_array([87; 32]));
    message.header.num_readonly_unsigned_accounts += 1;
    message
        .instructions
        .push(solana_message::compiled_instruction::CompiledInstruction {
            program_id_index,
            accounts: vec![],
            data: vec![19, 88, 42],
        });
    let bytes = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .serialize(&transaction)
        .map_err(|_| fixture_state_error())?;
    Ok(STANDARD.encode(bytes))
}

struct RehearsalAuthorizer {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Authorizer for RehearsalAuthorizer {
    async fn authorize(
        &self,
        challenge: &gofin_domain::AuthorizationChallenge,
    ) -> Result<ApprovalAudit> {
        self.calls.fetch_add(1, Ordering::SeqCst);
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

type SlippageRuntimeFixture = (
    Arc<gofin_broker::BrokerRuntime>,
    Arc<FixtureRpc>,
    Arc<SlippageOrderApi>,
    Arc<AtomicUsize>,
    Arc<dyn gofin_domain::Adapter>,
);

fn slippage_runtime(directory: &tempfile::TempDir) -> SlippageRuntimeFixture {
    let (config, _, rpc, fixture_api) = applications_with_fixture().unwrap();
    let signer = Arc::new(SolanaSigner::parse(SYNTHETIC_KEYPAIR).unwrap());
    let credentials: Arc<dyn BrokerCredentialProvider> = Arc::new(FixtureCredentials {
        reference: "00000000-0000-4000-8000-000000000007",
        signer,
        auxiliary_signers: Mutex::new(BTreeMap::new()),
    });
    let api = Arc::new(SlippageOrderApi {
        inner: fixture_api,
        paths: Mutex::new(vec![]),
        response_overrides: Mutex::new(BTreeMap::new()),
        token_metadata: Mutex::new(json!([])),
        token_requests: AtomicUsize::new(0),
    });
    let api_transport: Arc<dyn gofin_jupiter::JupiterApi> = api.clone();
    let applications = gofin_broker::build_applications_with_network_transports(
        &config,
        credentials,
        Arc::new(FixturePrices),
        BTreeMap::new(),
        Some(rpc.clone()),
        Some(api_transport),
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
        .unwrap()
        .clone();
    let approval_calls = Arc::new(AtomicUsize::new(0));
    let runtime = gofin_broker::compose_runtime(
        config,
        gofin_broker::RuntimeDependencies {
            store: gofin_store::Store::open(directory.path().join("operations.sqlite")).unwrap(),
            adapters: applications.adapters,
            authorizer: Arc::new(RehearsalAuthorizer {
                calls: approval_calls.clone(),
            }),
            valuator: applications.valuator,
            bridge: applications.bridge,
        },
    )
    .unwrap();
    (runtime, rpc, api, approval_calls, adapter)
}

#[tokio::test]
#[cfg(target_os = "macos")]
async fn actual_jupiter_approval_binding_renders_in_native_review() {
    let _fixture = FIXTURE_LOCK.lock().await;
    let directory = tempfile::tempdir().unwrap();
    let main = directory.path().join("main.swift");
    std::fs::write(
        &main,
        r#"
import Foundation
let data = FileHandle.standardInput.readDataToEndOfFile()
let input = try JSONSerialization.jsonObject(with: data) as! [String: Any]
let canonical = input["canonicalBinding"] as! String
let binding = try JSONSerialization.jsonObject(with: Data(canonical.utf8)) as! [String: Any]
switch typedFinancialApprovalReason(binding) {
case .success(let reason): print(reason)
case .failure(let failure):
    FileHandle.standardError.write(Data(failure.rawValue.utf8))
    exit(1)
}
"#,
    )
    .unwrap();
    let binary = directory.path().join("native-jupiter-review");
    let presentation = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../native/ApprovalPresentation.swift");
    let compiled = std::process::Command::new("xcrun")
        .args(["swiftc"])
        .arg(presentation)
        .arg(main)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let (runtime, rpc, api, approvals, adapter) = slippage_runtime(&directory);
    let ore_mint = "oreoU2P8bN6jkk3jbaiVxYnG1dCXcYxwhwyK9jSybcp";
    {
        let mut state = rpc.0.lock().unwrap();
        let account = gofin_solana::associated_token_address(
            &state.wallet,
            ore_mint,
            gofin_solana::token::TOKEN_PROGRAM_ID,
        )
        .unwrap();
        let token_data = token_account_data(ore_mint, &state.wallet, 0);
        state.account_data.insert(
            ore_mint.into(),
            (gofin_solana::token::TOKEN_PROGRAM_ID.into(), mint_data(11)),
        );
        state.account_data.insert(
            account,
            (gofin_solana::token::TOKEN_PROGRAM_ID.into(), token_data),
        );
    }
    *api.token_metadata.lock().unwrap() =
        json!([{ "id": ore_mint, "symbol": "ORE", "decimals": 11 }]);
    for (cap, displayed) in [(0, "0 SOL"), (5_000_000, "0.005 SOL")] {
        let response = runtime
            .handle(BrokerRequest::Preview {
                operation: "jupiter.swap".into(),
                input: json!({
                    "wallet":"solana-rehearsal", "network":"solana", "cluster":"mainnet-beta",
                    "input_mint":"native", "output_mint":ore_mint,
                    "amount_atoms":"20000000", "minimum_output_atoms":"199000000000",
                    "maximum_slippage_bps":100, "maximum_platform_fee_bps":0,
                    "maximum_network_fee_lamports":10000, "maximum_account_funding_lamports":cap
                }),
            })
            .await;
        assert!(response.ok, "{:?}", response.error);
        let prepared: gofin_domain::PreparedOperation =
            serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(
            prepared.binding["approval_asset_metadata"][0],
            json!({
                "name":"ORE", "identifier":{"network":"solana:mainnet-beta", "reference":ore_mint, "decimals":11}
            })
        );
        let requests = api.token_requests.load(Ordering::SeqCst);
        *api.token_metadata.lock().unwrap() =
            json!([{ "id": ore_mint, "symbol": "CHANGED", "decimals": 11 }]);
        let revalidated = adapter.revalidate(&prepared).await.unwrap();
        assert_eq!(
            revalidated.binding["approval_asset_metadata"],
            prepared.binding["approval_asset_metadata"]
        );
        assert_eq!(api.token_requests.load(Ordering::SeqCst), requests);
        *api.token_metadata.lock().unwrap() =
            json!([{ "id": ore_mint, "symbol": "ORE", "decimals": 11 }]);
        let binding = gofin_domain::authorization_binding(&revalidated);
        assert!(serde_json::to_string(&binding).unwrap().contains(ore_mint));
        let challenge = gofin_domain::AuthorizationChallenge {
            review: Some(gofin_domain::approval_review::approval_review(&revalidated).unwrap()),
            purpose: "financial_write".into(),
            operation_id: "native-contract-fixture".into(),
            binding_hash: gofin_domain::binding_hash(&binding).unwrap(),
            binding,
            summary: prepared.summary.clone(),
            nonce: "a".repeat(64),
            expires_at: prepared.expires_at.clone(),
        };
        let input = gofin_authorization::helper_challenge(&challenge).unwrap();
        let mut child = std::process::Command::new(&binary)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(&serde_json::to_vec(&input).unwrap())
                .unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "Native formatter rejected adapter-generated funding cap {cap}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let reason = String::from_utf8(output.stdout).unwrap();
        assert!(reason.contains(displayed), "{reason}");
        assert!(reason.contains("0.02 SOL"), "{reason}");
        assert!(reason.contains("for at least 1.99 ORE"), "{reason}");
        assert!(reason.contains("solana-rehearsal (Solana)"), "{reason}");
        assert!(reason.contains("fees ≤0.00001 SOL"), "{reason}");
    }
    assert_eq!(approvals.load(Ordering::SeqCst), 0);
    assert!(rpc.0.lock().unwrap().transactions.is_empty());
}

#[tokio::test]
async fn unfunded_jupiter_quote_returns_pricing_and_blocker_then_fresh_funded_order_executes() {
    let _fixture = FIXTURE_LOCK.lock().await;
    let directory = tempfile::tempdir().unwrap();
    let (runtime, rpc, api, approvals, _) = slippage_runtime(&directory);
    {
        let mut overrides = api.response_overrides.lock().unwrap();
        overrides.insert("transaction".into(), json!(""));
        overrides.insert("lastValidBlockHeight".into(), Value::Null);
        overrides.insert("router".into(), json!("metis"));
        overrides.insert("errorCode".into(), json!(1));
        overrides.insert("errorMessage".into(), json!("Insufficient funds"));
    }
    let input = json!({
        "wallet":"solana-rehearsal", "network":"solana", "cluster":"mainnet-beta",
        "input_mint":"native", "output_mint":SWAP_OUTPUT_MINT,
        "amount_atoms":"20000000", "maximum_slippage_bps":100
    });
    let response = runtime
        .handle(BrokerRequest::Read {
            operation: "jupiter.quote".into(),
            input: input.clone(),
        })
        .await;
    assert!(response.ok, "{:?}", response.error);
    let quote = response.result.unwrap();
    assert_eq!(quote["expected_output_atoms"], "2000000");
    assert_eq!(quote["minimum_output_atoms"], "1990000");
    assert_eq!(quote["transaction_available"], false);
    let provider_error = json!({"router":"metis", "code":1, "message":"Insufficient funds"});
    assert_eq!(quote["provider_error"], provider_error);
    assert!(
        quote["next_action"]
            .as_str()
            .unwrap()
            .contains("Once the required input funds are available")
    );
    assert!(quote.get("transaction").is_none());
    let schema = gofin_jupiter::operation_definitions()
        .into_iter()
        .find(|definition| definition.name == "jupiter.quote")
        .unwrap()
        .output_schema;
    assert!(jsonschema::validator_for(&schema).unwrap().is_valid(&quote));

    let mut swap_input = input.clone();
    swap_input["minimum_output_atoms"] = json!("1990000");
    swap_input["maximum_platform_fee_bps"] = json!(0);
    swap_input["maximum_network_fee_lamports"] = json!(10000);
    swap_input["maximum_account_funding_lamports"] = json!(0);
    let execute = |id: &str| BrokerRequest::Execute {
        request: gofin_domain::OperationRequest {
            request_id: id.into(),
            operation: "jupiter.swap".into(),
            input: swap_input.clone(),
            run_id: None,
        },
    };
    let response = runtime.handle(execute("unfunded-order")).await;
    assert!(response.ok, "{:?}", response.error);
    let record = response.result.unwrap();
    assert_eq!(record["status"], "failed");
    assert_eq!(record["error"]["code"], "JUPITER_ORDER_EMPTY");
    assert_eq!(record["error"]["details"]["provider_error"], provider_error);
    assert!(record["approval"].is_null());
    assert!(record["submission"].is_null());
    assert_eq!(approvals.load(Ordering::SeqCst), 0);
    assert!(rpc.0.lock().unwrap().transactions.is_empty());

    // Model the provider building a fresh transaction after funds arrive.
    api.response_overrides.lock().unwrap().clear();
    let response = runtime
        .handle(BrokerRequest::Read {
            operation: "jupiter.quote".into(),
            input,
        })
        .await;
    assert!(response.ok, "{:?}", response.error);
    let quote = response.result.unwrap();
    assert_eq!(quote["transaction_available"], true);
    assert!(quote["provider_error"].is_null());
    assert_eq!(approvals.load(Ordering::SeqCst), 0);
    let response = runtime.handle(execute("fresh-funded-order")).await;
    assert!(response.ok, "{:?}", response.error);
    let record = response.result.unwrap();
    assert_eq!(record["status"], "completed", "{record}");
    assert_eq!(approvals.load(Ordering::SeqCst), 1);
    assert_eq!(rpc.0.lock().unwrap().transactions.len(), 1);
    assert_eq!(
        api.paths.lock().unwrap().len(),
        4,
        "each read and execution fetches a fresh order"
    );
}

#[tokio::test]
async fn trusted_meta_order_with_rtse_and_opaque_instruction_settles_once() {
    let _fixture = FIXTURE_LOCK.lock().await;
    let directory = tempfile::tempdir().unwrap();
    let (runtime, rpc, api, approvals, _) = slippage_runtime(&directory);
    api.response_overrides
        .lock()
        .unwrap()
        .insert("router".into(), json!("future-provider-engine"));
    let wallet = rpc.0.lock().unwrap().wallet.clone();
    let input_atoms = 20_000_000u64;
    let expected_output =
        swap_output_amount(input_atoms, SWAP_INPUT_MINT, SWAP_OUTPUT_MINT).unwrap();
    let minimum_output = expected_output * 9_950 / 10_000;
    let maximum_slippage_bps = 100u16;

    let request = BrokerRequest::Execute {
        request: gofin_domain::OperationRequest {
            request_id: "meta-absolute-floor-rehearsal".into(),
            operation: "jupiter.swap".into(),
            input: json!({
                "wallet":"solana-rehearsal",
                "network":"solana",
                "cluster":"mainnet-beta",
                "input_mint":"native",
                "output_mint":SWAP_OUTPUT_MINT,
                "amount_atoms":input_atoms.to_string(),
                "minimum_output_atoms":minimum_output.to_string(),
                "maximum_slippage_bps":maximum_slippage_bps,
                "maximum_platform_fee_bps":0,
                "maximum_network_fee_lamports":10000,
                "maximum_account_funding_lamports":0
            }),
            run_id: None,
        },
    };
    let response = runtime.handle(request.clone()).await;
    assert!(response.ok, "broker execution failed: {:?}", response.error);
    let result = response.result.expect("settled execution result");
    assert_eq!(result["status"], "completed", "{result}");
    let operation_id = result["id"]
        .as_str()
        .expect("operation identity")
        .to_owned();
    let status = runtime
        .handle(BrokerRequest::Status {
            id: operation_id.clone(),
        })
        .await;
    assert!(status.ok, "status read failed: {:?}", status.error);
    let record = status.result.expect("durable operation record");
    let effects = record["effects"]["effects"].as_array().expect("effects");
    let input_atoms_text = input_atoms.to_string();
    let expected_output_text = expected_output.to_string();
    assert!(
        effects.iter().any(|effect| {
            effect["kind"] == "conversion"
                && effect["spent"]["atoms"] == input_atoms_text
                && effect["received"]["atoms"] == expected_output_text
        }),
        "effects={effects:?}"
    );

    {
        let paths = api.paths.lock().unwrap();
        assert_eq!(paths.len(), 1, "one provider RTSE order only");
        for path in paths.iter() {
            assert!(path.contains(&format!("amount={input_atoms}&taker={wallet}")));
            assert!(path.contains(&format!("inputMint={SWAP_INPUT_MINT}")));
            assert!(path.contains(&format!("outputMint={SWAP_OUTPUT_MINT}")));
        }
        assert!(!paths[0].contains("slippageBps="));
    }

    assert_eq!(approvals.load(Ordering::SeqCst), 1);
    {
        let state = rpc.0.lock().unwrap();
        assert_eq!(state.transactions.len(), 1, "one finalized submission");
        assert!(transaction_swap_matches(
            &state.transactions,
            &state.transactions[0].signature,
            &wallet,
            input_atoms,
            SWAP_INPUT_MINT,
            SWAP_OUTPUT_MINT,
        ));
        let bytes = STANDARD.decode(&state.transactions[0].wire).unwrap();
        let transaction: VersionedTransaction = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .reject_trailing_bytes()
            .with_limit(128 * 1024)
            .deserialize(&bytes)
            .unwrap();
        let keys = transaction.message.static_account_keys();
        let swap_ix = transaction
            .message
            .instructions()
            .iter()
            .find(|ix| {
                keys.get(usize::from(ix.program_id_index))
                    .is_some_and(|key| key.to_string() == gofin_jupiter::swap::JUPITER_V6_PROGRAM)
            })
            .expect("submitted Jupiter swap instruction");
        let (actual_input, actual_output, _, _) = fixture_swap_fields(&swap_ix.data).unwrap();
        assert_eq!(actual_input, input_atoms);
        assert_eq!(actual_output, expected_output);
        assert!(
            transaction.message.instructions().iter().any(|ix| {
                keys.get(usize::from(ix.program_id_index))
                    == Some(&solana_pubkey::Pubkey::new_from_array([87; 32]))
                    && ix.data == [19, 88, 42]
            }),
            "opaque provider instruction must be preserved"
        );
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

    // The same operation identity is idempotent: no second quote, approval,
    // or dispatch is allowed after the broker has persisted settlement.
    let duplicate = runtime.handle(request).await;
    assert!(duplicate.ok, "duplicate response: {:?}", duplicate.error);
    assert_eq!(duplicate.result.unwrap()["id"], operation_id);
    assert_eq!(api.paths.lock().unwrap().len(), 1);
    assert_eq!(approvals.load(Ordering::SeqCst), 1);
    assert_eq!(rpc.0.lock().unwrap().transactions.len(), 1);
}

#[tokio::test]
async fn order_metadata_outside_financial_limits_is_refused_before_approval_or_dispatch() {
    let _fixture = FIXTURE_LOCK.lock().await;
    for (field, value) in [
        ("inAmount", json!("19999999")),
        ("inputMint", json!(SWAP_OUTPUT_MINT)),
        ("outputMint", json!(SWAP_INPUT_MINT)),
        ("taker", json!(bs58::encode([88; 32]).into_string())),
        ("swapMode", json!("ExactOut")),
        ("otherAmountThreshold", json!("1989999")),
        ("slippageBps", json!(51)),
        ("feeBps", json!(1)),
        ("signatureFeeLamports", json!(10001)),
        ("rentFeeLamports", json!(3000001)),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let (runtime, rpc, api, approvals, _) = slippage_runtime(&directory);
        api.response_overrides
            .lock()
            .unwrap()
            .insert(field.to_owned(), value);
        let response = runtime.handle(BrokerRequest::Execute {
            request: gofin_domain::OperationRequest {
                request_id: format!("invalid-order-{field}"),
                operation: "jupiter.swap".into(),
                input: json!({
                    "wallet":"solana-rehearsal", "network":"solana", "cluster":"mainnet-beta",
                    "input_mint":"native", "output_mint":SWAP_OUTPUT_MINT,
                    "amount_atoms":"20000000", "minimum_output_atoms":"1990000",
                    "maximum_slippage_bps":50, "maximum_platform_fee_bps":0,
                    "maximum_network_fee_lamports":10000, "maximum_account_funding_lamports":3000000
                }),
                run_id: None,
            },
        }).await;
        assert!(response.ok, "broker request failed: {:?}", response.error);
        let record = response.result.expect("durable refused operation");
        assert_eq!(record["status"], "failed", "out-of-bound {field}: {record}");
        assert_eq!(record["error"]["stage"], "prepare", "{field}: {record}");
        if field == "slippageBps" {
            assert_eq!(record["error"]["code"], "JUPITER_SLIPPAGE_LIMIT_EXCEEDED");
            assert_eq!(
                record["error"]["details"],
                json!({
                    "actual_slippage_bps": 51,
                    "maximum_slippage_bps": 50,
                })
            );
        }
        assert!(record["approval"].is_null(), "{field}");
        assert!(record["submission"].is_null(), "{field}");
        assert_eq!(approvals.load(Ordering::SeqCst), 0, "{field}");
        assert!(rpc.0.lock().unwrap().transactions.is_empty(), "{field}");
    }
}
