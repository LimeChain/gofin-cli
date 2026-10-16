use super::*;
use gofin_domain::{ApprovalAudit, Authorizer};
use gofin_protocol::BrokerRequest;
use std::sync::atomic::{AtomicUsize, Ordering};

struct SlippageOrderApi {
    inner: Arc<FixtureJupiterApi>,
    paths: Mutex<Vec<String>>,
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
        let mut response = self
            .inner
            .request(host, method.clone(), path, body, bucket, priority, max_wait)
            .await?;
        if path.starts_with("/swap/v2/order?") && method == Method::GET {
            self.paths.lock().unwrap().push(path.to_owned());
            let slippage_bps = query_parameter(path, "slippageBps")
                .and_then(|value| value.parse::<u16>().ok())
                .ok_or_else(fixture_state_error)?;
            let encoded = response
                .get("transaction")
                .and_then(Value::as_str)
                .ok_or_else(fixture_state_error)?;
            response["transaction"] = json!(rewrite_order_slippage(encoded, slippage_bps)?);
        }
        Ok(response)
    }
}

fn rewrite_order_slippage(encoded: &str, slippage_bps: u16) -> Result<String> {
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
        return Err(GoFinError::validation(
            "The slippage rehearsal expects the full legacy Meta order fixture.",
        ));
    };
    let program = gofin_jupiter::swap::JUPITER_V6_PROGRAM
        .parse::<solana_pubkey::Pubkey>()
        .map_err(|_| fixture_state_error())?;
    let instruction = message
        .instructions
        .iter_mut()
        .find(|instruction| {
            message
                .account_keys
                .get(usize::from(instruction.program_id_index))
                == Some(&program)
        })
        .ok_or_else(fixture_state_error)?;
    let before = gofin_jupiter::swap::decode_swap_instruction(&instruction.data)
        .map_err(|_| fixture_state_error())?;
    let mut replacement = None;
    for offset in 8..instruction.data.len().saturating_sub(1) {
        let mut data = instruction.data.clone();
        data[offset..offset + 2].copy_from_slice(&slippage_bps.to_le_bytes());
        let Ok(after) = gofin_jupiter::swap::decode_swap_instruction(&data) else {
            continue;
        };
        if after.variant == before.variant
            && after.authority_id == before.authority_id
            && after.input_amount == before.input_amount
            && after.quoted_output == before.quoted_output
            && after.platform_fee_bps == before.platform_fee_bps
            && after.positive_slippage_bps == before.positive_slippage_bps
            && after.slippage_bps == slippage_bps
        {
            replacement = Some(data);
            break;
        }
    }
    instruction.data = replacement.ok_or_else(fixture_state_error)?;
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
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: gofin_domain::now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

fn slippage_runtime(
    directory: &tempfile::TempDir,
) -> (
    Arc<gofin_broker::BrokerRuntime>,
    Arc<FixtureRpc>,
    Arc<SlippageOrderApi>,
    Arc<AtomicUsize>,
) {
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
    let approval_calls = Arc::new(AtomicUsize::new(0));
    let runtime = gofin_broker::compose_runtime(
        config,
        gofin_broker::RuntimeDependencies {
            store: gofin_store::Store::open(&directory.path().join("operations.sqlite")).unwrap(),
            adapters: applications.adapters,
            authorizer: Arc::new(RehearsalAuthorizer {
                calls: approval_calls.clone(),
            }),
            valuator: applications.valuator,
            bridge: applications.bridge,
        },
    )
    .unwrap();
    (runtime, rpc, api, approval_calls)
}

#[tokio::test]
async fn meta_slippage_retry_settles_once_through_the_broker_with_original_input() {
    let _fixture = FIXTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    let (runtime, rpc, api, approvals) = slippage_runtime(&directory);
    let wallet = rpc.0.lock().unwrap().wallet.clone();
    let input_atoms = 20_000_000u64;
    let expected_output =
        swap_output_amount(input_atoms, SWAP_INPUT_MINT, SWAP_OUTPUT_MINT).unwrap();
    let minimum_output = expected_output * 9_990 / 10_000;
    let maximum_slippage_bps = 100u16;
    let safe_slippage = gofin_jupiter::swap::minimum_protecting_slippage_bps(
        expected_output,
        minimum_output,
        maximum_slippage_bps,
    )
    .unwrap();
    assert_eq!(safe_slippage, 10);

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
                "maximum_account_funding_lamports":0,
                "route":"meta_aggregator"
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
    assert!(
        effects.iter().any(|effect| {
            effect["kind"] == "conversion"
                && effect["spent"]["atoms"] == input_atoms.to_string()
                && effect["received"]["atoms"] == expected_output.to_string()
        }),
        "effects={effects:?}"
    );

    let paths = api.paths.lock().unwrap();
    assert_eq!(paths.len(), 2, "one initial and one tighter order only");
    for path in paths.iter() {
        assert!(path.contains(&format!("amount={input_atoms}&taker={wallet}")));
        assert!(path.contains(&format!("inputMint={SWAP_INPUT_MINT}")));
        assert!(path.contains(&format!("outputMint={SWAP_OUTPUT_MINT}")));
    }
    assert!(paths[0].contains("slippageBps=100"));
    assert!(paths[1].contains("slippageBps=10"));
    drop(paths);

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
        let decoded = gofin_jupiter::swap::decode_swap_instruction(&swap_ix.data).unwrap();
        assert_eq!(decoded.input_amount, Some(input_atoms));
        assert_eq!(decoded.quoted_output, expected_output);
        assert_eq!(decoded.slippage_bps, safe_slippage);
    }

    // The same operation identity is idempotent: no second quote, approval,
    // or dispatch is allowed after the broker has persisted settlement.
    let duplicate = runtime.handle(request).await;
    assert!(duplicate.ok, "duplicate response: {:?}", duplicate.error);
    assert_eq!(duplicate.result.unwrap()["id"], operation_id);
    assert_eq!(api.paths.lock().unwrap().len(), 2);
    assert_eq!(approvals.load(Ordering::SeqCst), 1);
    assert_eq!(rpc.0.lock().unwrap().transactions.len(), 1);
}
