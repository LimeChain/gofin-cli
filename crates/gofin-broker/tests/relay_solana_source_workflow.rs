//! Controlled Solana-to-EVM Relay source boundary. No live transport or key is used.
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bincode::Options;
use gofin_bridge::*;
use gofin_broker::{BridgeAdapter, SharedUsdValuator};
use gofin_domain::{
    Adapter, ApprovalAudit, AssetId, AuthorizationChallenge, Authorizer,
    BridgeStepKind as DomainStepKind, OperationRequest, Quantity, Result, Status,
    boundaries::BoundaryAuthorizationRequest, now,
};
use gofin_engine::{Engine, EngineOptions};
use gofin_evm::{EvmRpc, EvmRuntime};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use gofin_solana::transaction::NormalizedAccountMeta;
use gofin_solana::{NormalizedInstruction, SolanaRpc, SolanaSigner, SolanaWalletAdapter};
use gofin_store::{BridgeStateRepository, Store};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use solana_pubkey::Pubkey;
use solana_transaction::versioned::VersionedTransaction;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

const ORDER: &str = "0x7777777777777777777777777777777777777777777777777777777777777777";
const REQUEST: &str = "0x5555555555555555555555555555555555555555555555555555555555555555";
const RECIPIENT: &str = "0x2222222222222222222222222222222222222222";
const AMOUNT: u64 = 1_000_000;
const FEE: u64 = 5_000;
fn q(n: &str) -> Quantity {
    Quantity::new(n).unwrap()
}
fn signer() -> Arc<SolanaSigner> {
    let pair = ed25519_dalek::SigningKey::from_bytes(&[42; 32]).to_keypair_bytes();
    Arc::new(SolanaSigner::parse(&serde_json::to_string(&pair.to_vec()).unwrap()).unwrap())
}
fn source_asset() -> AssetId {
    AssetId {
        network: "solana:mainnet-beta".into(),
        reference: "native".into(),
        decimals: 9,
    }
}
fn destination_asset() -> AssetId {
    EvmAsset {
        chain_id: 1,
        token: None,
        decimals: 18,
    }
    .asset_id()
}
fn direct_deposit(wallet: &str, unsupported: bool) -> Vec<NormalizedInstruction> {
    let program = gofin_bridge::relay::solana::DEPOSITORY_PROGRAM;
    let depository =
        Pubkey::find_program_address(&[b"relay_depository"], &program.parse().unwrap())
            .0
            .to_string();
    let vault = gofin_bridge::relay::solana::relay_vault_address().unwrap();
    let meta = |address: &str, signer: bool, writable: bool| NormalizedAccountMeta {
        address: address.into(),
        signer,
        writable,
    };
    let mut data = Sha256::digest(b"global:deposit_native")[..8].to_vec();
    data.extend(AMOUNT.to_le_bytes());
    data.extend(hex::decode(&ORDER[2..]).unwrap());
    let mut instructions = vec![NormalizedInstruction {
        program_id: program.into(),
        accounts: vec![
            meta(&depository, false, false),
            meta(wallet, true, true),
            meta(wallet, true, true),
            meta(&vault, false, true),
            meta(gofin_bridge::relay::solana::SYSTEM_PROGRAM, false, false),
        ],
        data_base64: STANDARD.encode(data),
    }];
    if unsupported {
        instructions.push(NormalizedInstruction {
            program_id: gofin_bridge::relay::solana::SYSTEM_PROGRAM.into(),
            accounts: vec![],
            data_base64: STANDARD.encode([0u8; 4]),
        });
    }
    instructions
}

#[derive(Default)]
struct Approval(AtomicUsize);
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            source: Some(gofin_domain::AuthorizationSource::Development),
            yolo_provenance: None,
            mechanism: "synthetic fixture".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}
struct Prices;
#[async_trait]
impl UsdPriceSource for Prices {
    async fn quote(&self, _: &AssetId) -> Result<UsdQuote> {
        Ok(UsdQuote {
            usd_per_unit: "100".into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic fixed price".into(),
        })
    }
}
struct Evm;
#[async_trait]
impl EvmRpc for Evm {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        Ok(match method {
            "eth_chainId" => json!("0x1"),
            "eth_getCode" => json!("0x"),
            "eth_blockNumber" => json!("0x64"),
            other => panic!("unexpected synthetic EVM RPC {other}: {params}"),
        })
    }
}
#[derive(Default)]
struct RpcState {
    sends: usize,
    transaction: Option<Value>,
    signature: Option<String>,
}
struct Rpc {
    state: Mutex<RpcState>,
    lose_response: AtomicBool,
}
impl Rpc {
    fn new(lose_response: bool) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(RpcState::default()),
            lose_response: AtomicBool::new(lose_response),
        })
    }
    fn sends(&self) -> usize {
        self.state.lock().unwrap().sends
    }
    fn finalized(wire: &str, tx: &VersionedTransaction) -> Value {
        let keys: Vec<_> = tx
            .message
            .static_account_keys()
            .iter()
            .map(ToString::to_string)
            .collect();
        let wallet = keys[0].clone();
        let vault = gofin_bridge::relay::solana::relay_vault_address().unwrap();
        let wallet_index = keys.iter().position(|key| key == &wallet).unwrap();
        let vault_index = keys.iter().position(|key| key == &vault).unwrap();
        let mut pre = vec![0u64; keys.len()];
        let mut post = vec![0u64; keys.len()];
        pre[wallet_index] = 1_000_000_000;
        post[wallet_index] = 1_000_000_000 - AMOUNT - FEE;
        post[vault_index] = AMOUNT;
        json!({"slot":99,"transaction":[wire,"base64"],"meta":{"fee":FEE,"err":null,
            "preBalances":pre,"postBalances":post,"preTokenBalances":[],"postTokenBalances":[],
            "loadedAddresses":{"writable":[],"readonly":[]},"innerInstructions":[],"logMessages":[]}})
    }
}
#[async_trait]
impl SolanaRpc for Rpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        Ok(match method {
            "getGenesisHash" => json!(gofin_solana::MAINNET_GENESIS_HASH),
            "getSlot" => json!(100),
            "getBlockHeight" => json!(0),
            "getBalance" => json!({"context":{"slot":100},"value":1_000_000_000u64}),
            "getLatestBlockhash" => json!({"context":{"slot":100},"value":{
                "blockhash":bs58::encode([1u8;32]).into_string(),"lastValidBlockHeight":150}}),
            "isBlockhashValid" => json!({"context":{"slot":100},"value":true}),
            "getFeeForMessage" => json!({"context":{"slot":100},"value":FEE}),
            "simulateTransaction" => {
                json!({"context":{"slot":100},"value":{"err":null,"unitsConsumed":100_000}})
            }
            "sendTransaction" => {
                let wire = params[0].as_str().unwrap();
                let tx: VersionedTransaction = bincode::DefaultOptions::new()
                    .with_fixint_encoding()
                    .reject_trailing_bytes()
                    .deserialize(&STANDARD.decode(wire).unwrap())
                    .unwrap();
                tx.verify_and_hash_message().unwrap();
                let signature = tx.signatures[0].to_string();
                let mut state = self.state.lock().unwrap();
                state.sends += 1;
                state.transaction = Some(Self::finalized(wire, &tx));
                state.signature = Some(signature.clone());
                if self.lose_response.swap(false, Ordering::SeqCst) {
                    return Err(gofin_domain::GoFinError::validation(
                        "SOLANA_RPC_UNAVAILABLE",
                    ));
                }
                json!(signature)
            }
            "getSignatureStatuses" => {
                let state = self.state.lock().unwrap();
                json!({"value":[if state.signature.is_some() { json!({"slot":99,"confirmationStatus":"finalized","err":null}) } else { Value::Null }]})
            }
            "getTransaction" => self
                .state
                .lock()
                .unwrap()
                .transaction
                .clone()
                .unwrap_or(Value::Null),
            "getSignaturesForAddress" => {
                let state = self.state.lock().unwrap();
                json!(
                    state
                        .signature
                        .as_ref()
                        .map(|signature| vec![json!({"signature":signature,"slot":99,
                    "err":null,"confirmationStatus":"finalized"})])
                        .unwrap_or_default()
                )
            }
            other => panic!("unexpected synthetic Solana RPC {other}: {params}"),
        })
    }
}
struct Relay {
    unsupported: bool,
    complete: bool,
}
#[async_trait]
impl BridgeProvider for Relay {
    fn capabilities(&self) -> BridgeCapabilities {
        BridgeCapabilities {
            provider: "relay".into(),
            chains: BTreeSet::from([1, 792703809]),
            native_eth: true,
            erc20: false,
            protocol_versions: BTreeSet::from(["v2".into()]),
        }
    }
    async fn discover_routes(&self, _: &BridgeRouteRequest) -> Result<BridgeRouteDiscovery> {
        unreachable!()
    }
    async fn quote(&self, request: &BridgeQuoteRequest) -> Result<BridgeQuoteBatch> {
        let source = request.intent.solana_source.as_ref().unwrap();
        let step = BridgeStep {
            ordinal: 0,
            kind: BridgeStepKind::Deposit,
            call: None,
            solana_mint: None,
            solana_transaction: Some(SolanaSourceTransaction {
                transaction_base64: None,
                instructions: direct_deposit(&request.intent.source_address, self.unsupported),
                lookup_tables: vec![],
                source_address: request.intent.source_address.clone(),
                order_id: ORDER.into(),
                request_id: REQUEST.into(),
                mint: None,
                amount_atoms: request.intent.amount.clone(),
                max_network_fee_lamports: FEE,
                max_rent_lamports: 0,
                last_valid_block_height: Some(150),
                expire_at: Some((chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339()),
            }),
            expected_sender: request.intent.source_address.clone(),
            maximum_debit: vec![source.amount(request.intent.amount.clone())],
            dependencies: vec![],
        };
        Ok(BridgeQuoteBatch {
            failures: vec![],
            quotes: vec![BridgeQuote {
                cctp: None,
                quote_id: "synthetic-solana-source".into(),
                provider: "relay".into(),
                route_id: "direct-native-deposit".into(),
                intent: request.intent.clone(),
                gross_debit: source.amount(request.intent.amount.clone()),
                expected_receipt: gofin_domain::Amount {
                    asset: destination_asset(),
                    atoms: q("1000000000000000"),
                },
                minimum_receipt: gofin_domain::Amount {
                    asset: destination_asset(),
                    atoms: q("900000000000000"),
                },
                fees: vec![],
                approval: None,
                protocol_steps: vec![step],
                estimated_arrival_seconds: Some(1),
                observed_at: now(),
                valid_until: (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339(),
                class: QuoteClass::Executable,
                refund_paths: vec![RefundPath {
                    chain_id: 792703809,
                    network: Some("solana:mainnet-beta".into()),
                    asset: source.asset_id(),
                    recipient: request.intent.refund_to.clone(),
                    minimum: q("1"),
                    maximum: request.intent.amount.clone(),
                }],
                order_solver: Some("0xf70da97812cb96acdf810712aa562db8dfa3dbef".into()),
                protocol_order_id: Some(ORDER.into()),
                provider_request_id: Some(REQUEST.into()),
            }],
        })
    }
    fn prepare(
        &self,
        quote: &BridgeQuote,
        bounds: &BridgeBounds,
        _: &str,
    ) -> Result<PreparedBridge> {
        Ok(PreparedBridge {
            quote: quote.clone(),
            bounds: bounds.clone(),
            steps: quote.protocol_steps.clone(),
            source_order_id: quote.protocol_order_id.clone(),
            fixed_scope_digest: "synthetic-solana-source-scope".into(),
        })
    }
    async fn reconcile(&self, _: &BridgeTransferRef) -> Result<BridgeReconciliation> {
        Ok(BridgeReconciliation {
            status: if self.complete { "success" } else { "pending" }.into(),
            failure: None,
            refund_fail_reason: None,
            candidates: if self.complete {
                vec![BridgeEvidenceCandidate {
                    order_id: Some(ORDER.into()),
                    kind: EvidenceKind::DestinationFill,
                    chain_id: 1,
                    transaction_hash: PAYOUT_HASH.into(),
                    correlation: None,
                }]
            } else {
                vec![]
            },
        })
    }
}
fn compact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, value| !value.is_null());
            for value in map.values_mut() {
                compact(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                compact(value);
            }
        }
        _ => {}
    }
}
async fn harness(
    unsupported: bool,
    lose_response: bool,
) -> (
    tempfile::TempDir,
    BridgeStateRepository,
    Arc<BridgeAdapter>,
    Arc<Engine>,
    Arc<Approval>,
    Arc<Rpc>,
    String,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("journal.sqlite"))
        .unwrap()
        .share();
    let state = BridgeStateRepository::new(store.clone());
    let signer = signer();
    let wallet_address = signer.address().to_owned();
    let rpc = Rpc::new(lose_response);
    let prices = Arc::new(SharedUsdValuator::with_source(Arc::new(Prices)));
    let wallet = Arc::new(
        SolanaWalletAdapter::new(
            "source",
            &wallet_address,
            Some(signer),
            rpc.clone(),
            Arc::new(Prices),
        )
        .unwrap(),
    );
    let bridge = Arc::new(
        BridgeAdapter::with_provider_and_observers(
            Arc::new(Relay {
                unsupported,
                complete: false,
            }),
            BTreeMap::new(),
            BTreeMap::from([(1, Arc::new(EvmRuntime::new(Arc::new(Evm), 1, None)))]),
            prices,
        )
        .unwrap()
        .with_solana_wallets(BTreeMap::from([("source".into(), wallet.clone())])),
    );
    bridge.attach_state(state.clone()).unwrap();
    let approval = Arc::new(Approval::default());
    let engine = Arc::new(
        Engine::new_with_shared_store(
            store,
            vec![wallet, bridge.clone()],
            approval.clone(),
            EngineOptions {
                max_movement_and_cost_usd_micros: "100000000".into(),
                max_run_allocation_usd_micros: "200000000".into(),
                allow_unpriced_exchange_assets: false,
            },
        )
        .unwrap(),
    );
    bridge.attach_engine(Arc::downgrade(&engine)).unwrap();
    (dir, state, bridge, engine, approval, rpc, wallet_address)
}
fn bounds() -> BridgeBounds {
    BridgeBounds {
        maximum_source_debit: Some(q("1000000")),
        minimum_net_receipt: Some(gofin_domain::Amount {
            asset: destination_asset(),
            atoms: q("900000000000000"),
        }),
        fee_caps: vec![
            FeeCap {
                asset: source_asset(),
                kind: FeeKind::Gas,
                maximum: q("5000"),
                maximum_usd_micros: None,
            },
            FeeCap {
                asset: source_asset(),
                kind: FeeKind::AdditionalDebit,
                maximum: q("1"),
                maximum_usd_micros: None,
            },
        ],
        ..Default::default()
    }
}
fn quote_input(wallet: &str) -> Value {
    json!({"provider":"relay","intent":{
    "wallet":"source","solanaSource":{"network":"solana:mainnet-beta","mint":null},
    "destinationChainId":1,"destinationAssets":[{"chainId":1,"token":null}],
    "recipient":RECIPIENT,"refundTo":wallet,"allowedRefundAssets":[],
    "allowedSolanaRefundAssets":[{"network":"solana:mainnet-beta","mint":null}],
    "amount":AMOUNT.to_string()},"bounds":bounds()})
}

#[tokio::test]
async fn solana_source_auto_provider_quotes_without_approval_or_submission() {
    for omit in [true, false] {
        let (_dir, state, bridge, _engine, approval, rpc, wallet) = harness(false, false).await;
        let mut input = quote_input(&wallet);
        if omit {
            input.as_object_mut().unwrap().remove("provider");
        } else {
            input["provider"] = Value::Null;
        }
        let quote = bridge.read("bridge.quotes", &input).await.unwrap();
        assert_eq!(quote["coverage"]["providers"], json!(["relay"]));
        assert_eq!(quote["transfers"].as_array().unwrap().len(), 1);
        assert_eq!(state.transfers().unwrap().len(), 1);
        assert_eq!(approval.0.load(Ordering::SeqCst), 0);
        assert_eq!(rpc.sends(), 0);
    }
}

#[tokio::test]
async fn solana_source_explicit_provider_is_never_replaced() {
    let (_dir, state, bridge, _engine, approval, rpc, wallet) = harness(false, false).await;
    let mut input = quote_input(&wallet);
    input["provider"] = json!("cctp");
    let error = bridge.read("bridge.quotes", &input).await.unwrap_err();
    assert_eq!(error.code, "BRIDGE_PROVIDER_UNAVAILABLE");
    assert!(state.transfers().unwrap().is_empty());
    assert_eq!(approval.0.load(Ordering::SeqCst), 0);
    assert_eq!(rpc.sends(), 0);
}

#[tokio::test]
async fn direct_solana_source_persists_finalized_transit_after_response_loss() {
    let (dir, state, bridge, engine, approval, rpc, wallet) = harness(false, true).await;
    let quote = bridge
        .read("bridge.quotes", &quote_input(&wallet))
        .await
        .unwrap();
    let transfer = quote["transfers"][0]["transfer_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let step = quote["transfers"][0]["steps"][0]["step_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let saved = state.transfer(&transfer).unwrap().unwrap();
    let approved_bounds: BridgeBounds =
        serde_json::from_value(saved.quote_revisions.last().unwrap().payload["bounds"].clone())
            .unwrap();
    let mut input = serde_json::to_value(BridgeTransferInput {
        transfer_id: transfer.clone(),
        step_id: step,
        step_kind: DomainStepKind::Deposit,
        bounds: Some(approved_bounds.clone()),
    })
    .unwrap();
    compact(&mut input);
    let mut boundary = serde_json::to_value(&approved_bounds).unwrap();
    compact(&mut boundary);
    let grant = engine
        .authorize_boundary(BoundaryAuthorizationRequest {
            request_id: "approve-relay-solana-source".into(),
            operation: "bridge.transfer".into(),
            input: input.clone(),
            bounds: boundary,
            valid_for_seconds: 600,
            replaces: None,
        })
        .await;
    let grant = grant.unwrap();
    let request = OperationRequest {
        request_id: "relay-solana-source".into(),
        operation: "bridge.transfer".into(),
        input,
        run_id: None,
    };
    let first = engine.execute_boundary(&grant.id, request.clone()).await;
    let first = first.unwrap();
    assert_eq!(first.status, Status::Unknown);
    assert_eq!(
        first.error.as_ref().unwrap().code,
        "SOLANA_SUBMISSION_UNCERTAIN"
    );
    assert!(
        first.submission.is_some(),
        "submission identity must persist before dispatch"
    );
    assert_eq!(rpc.sends(), 1);
    // Rebuild both broker adapter and engine over a new SQLite handle. The
    // original process's in-memory journal and active grant state are unused.
    let resumed_store = Store::open(dir.path().join("journal.sqlite"))
        .unwrap()
        .share();
    let reopened = BridgeStateRepository::new(resumed_store.clone());
    let resumed_wallet = Arc::new(
        SolanaWalletAdapter::new(
            "source",
            &wallet,
            Some(signer()),
            rpc.clone(),
            Arc::new(Prices),
        )
        .unwrap(),
    );
    let resumed_bridge = Arc::new(
        BridgeAdapter::with_provider_and_observers(
            Arc::new(Relay {
                unsupported: false,
                complete: false,
            }),
            BTreeMap::new(),
            BTreeMap::from([(1, Arc::new(EvmRuntime::new(Arc::new(Evm), 1, None)))]),
            Arc::new(SharedUsdValuator::with_source(Arc::new(Prices))),
        )
        .unwrap()
        .with_solana_wallets(BTreeMap::from([("source".into(), resumed_wallet.clone())])),
    );
    resumed_bridge.attach_state(reopened.clone()).unwrap();
    let resumed_engine = Arc::new(
        Engine::new_with_shared_store(
            resumed_store,
            vec![resumed_wallet, resumed_bridge.clone()],
            approval.clone(),
            EngineOptions {
                max_movement_and_cost_usd_micros: "100000000".into(),
                max_run_allocation_usd_micros: "200000000".into(),
                allow_unpriced_exchange_assets: false,
            },
        )
        .unwrap(),
    );
    resumed_bridge
        .attach_engine(Arc::downgrade(&resumed_engine))
        .unwrap();
    assert!(reopened.transfer(&transfer).unwrap().is_some());
    let recovered = resumed_engine
        .execute_boundary(&grant.id, request)
        .await
        .unwrap();
    assert_eq!(recovered.id, first.id);
    assert_eq!(recovered.status, Status::Completed);
    assert_eq!(
        rpc.sends(),
        1,
        "uncertain Solana dispatch must never replay"
    );
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    let status = resumed_bridge
        .read("bridge.status", &json!({"transferId":transfer}))
        .await
        .unwrap();
    assert_ne!(
        status["state"], "completed",
        "source receipt alone is not destination settlement"
    );
    let durable = reopened.transfer(&transfer).unwrap().unwrap();
    assert!(
        durable.transit.is_some(),
        "finalized vault credit must produce durable transit"
    );
}

#[tokio::test]
async fn unsupported_solana_source_instruction_rejected_before_approval() {
    let (_dir, state, bridge, _engine, approval, rpc, wallet) = harness(true, false).await;
    let error = bridge
        .read("bridge.quotes", &quote_input(&wallet))
        .await
        .unwrap_err();
    assert!(
        format!("{error:?}").contains("unsupported instruction"),
        "{error:?}"
    );
    assert!(state.transfers().unwrap().is_empty());
    assert_eq!(approval.0.load(Ordering::SeqCst), 0);
    assert_eq!(rpc.sends(), 0);
}

const PAYOUT_HASH: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PAYOUT_BLOCK: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
struct PayoutEvm;
#[async_trait]
impl EvmRpc for PayoutEvm {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let sender = "0xf70da97812cb96acdf810712aa562db8dfa3dbef";
        Ok(match method {
            "eth_chainId" => json!("0x1"),
            "eth_getCode" => json!("0x"),
            "eth_blockNumber" => json!("0x70"),
            "eth_getBlockByNumber" | "eth_getBlockByHash" => {
                json!({"number":if params[0]=="finalized" {"0x70"} else {"0x64"},"hash":PAYOUT_BLOCK,"timestamp":"0x2000"})
            }
            "eth_getTransactionByHash" => {
                json!({"hash":PAYOUT_HASH,"from":sender,"to":RECIPIENT,"value":"0x38d7ea4c68000","input":ORDER,"nonce":"0x1","gas":"0x186a0","chainId":"0x1","type":"0x2","maxFeePerGas":"0x10","maxPriorityFeePerGas":"0x1","accessList":[],"blockNumber":"0x64","blockHash":PAYOUT_BLOCK})
            }
            "eth_getTransactionReceipt" => {
                json!({"transactionHash":PAYOUT_HASH,"blockNumber":"0x64","blockHash":PAYOUT_BLOCK,"status":"0x1","gasUsed":"0x7530","effectiveGasPrice":"0x3b9aca00","logs":[]})
            }
            other => panic!("unexpected synthetic payout RPC {other}: {params}"),
        })
    }
}

use gofin_protocol::BrokerRequest;
include!("support/kraken_plan_client.rs");

#[tokio::test]
#[ignore = "requires an explicitly built development CLI"]
async fn cli_relay_solana_ethereum_positive() {
    assert!(
        std::env::var_os("GOFIN_KRAKEN_PLAN_CLI").is_some(),
        "real CLI required"
    );
    let dir = tempfile::tempdir().unwrap();
    let state = BridgeStateRepository::new(
        Store::open(dir.path().join("journal.sqlite"))
            .unwrap()
            .share(),
    );
    let signer = signer();
    let address = signer.address().to_owned();
    let rpc = Rpc::new(false);
    let prices = Arc::new(SharedUsdValuator::with_source(Arc::new(Prices)));
    let wallet = Arc::new(
        SolanaWalletAdapter::new(
            "source",
            &address,
            Some(signer),
            rpc.clone(),
            Arc::new(Prices),
        )
        .unwrap(),
    );
    let bridge = Arc::new(
        BridgeAdapter::with_provider_and_observers(
            Arc::new(Relay {
                unsupported: false,
                complete: true,
            }),
            BTreeMap::new(),
            BTreeMap::from([(1, Arc::new(EvmRuntime::new(Arc::new(PayoutEvm), 1, None)))]),
            prices.clone(),
        )
        .unwrap()
        .with_solana_wallets(BTreeMap::from([("source".into(), wallet.clone())])),
    );
    let mut policy = gofin_broker::BrokerPolicy::beta();
    policy.max_movement_usd_micros = "100000000".into();
    policy.max_run_allocation_usd_micros = "200000000".into();
    let broker = gofin_broker::compose_runtime(
        gofin_broker::BrokerConfigV2 {
            version: 2,
            networks: gofin_broker::NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy,
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        gofin_broker::RuntimeDependencies {
            store: Store::open(dir.path().join("journal.sqlite")).unwrap(),
            adapters: vec![wallet, bridge.clone()],
            authorizer: Arc::new(Approval::default()),
            valuator: prices,
            bridge: Some(bridge),
        },
    )
    .unwrap();
    let client = Client::new(broker, dir.path()).await;
    let quote_input = quote_input(&address);
    let quote = client
        .call(
            args(&[
                "read",
                "bridge.quotes",
                "--input",
                &quote_input.to_string(),
                "--json",
            ]),
            BrokerRequest::Read {
                operation: "bridge.quotes".into(),
                input: quote_input,
            },
            true,
        )
        .await;
    let transfer = quote["transfers"][0]["transfer_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let step = quote["transfers"][0]["steps"][0]["step_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let saved = state.transfer(&transfer).unwrap().unwrap();
    let frozen: BridgeBounds =
        serde_json::from_value(saved.quote_revisions.last().unwrap().payload["bounds"].clone())
            .unwrap();
    let mut input = serde_json::to_value(BridgeTransferInput {
        transfer_id: transfer.clone(),
        step_id: step,
        step_kind: DomainStepKind::Deposit,
        bounds: Some(frozen),
    })
    .unwrap();
    compact(&mut input);
    let request = OperationRequest {
        request_id: "relay-solana-positive".into(),
        operation: "bridge.transfer".into(),
        input: input.clone(),
        run_id: None,
    };
    let record = client
        .call(
            args(&[
                "execute",
                "bridge.transfer",
                "--input",
                &input.to_string(),
                "--id",
                &request.request_id,
                "--json",
            ]),
            BrokerRequest::Execute { request },
            true,
        )
        .await;
    assert_eq!(record["status"], "completed", "{record}");
    assert_eq!(rpc.sends(), 1);
    let status_input = json!({"transferId":transfer});
    let status = client
        .call(
            args(&[
                "read",
                "bridge.status",
                "--input",
                &status_input.to_string(),
                "--json",
            ]),
            BrokerRequest::Read {
                operation: "bridge.status".into(),
                input: status_input,
            },
            true,
        )
        .await;
    assert_eq!(status["state"], "completed", "{status}");
    let complete = state.transfer(&transfer).unwrap().unwrap();
    assert_eq!(complete.settlement_proofs.len(), 1);
    assert_eq!(
        complete.settlement_proofs[0].evidence["proof_method"],
        "direct_transaction_order_tag"
    );
    assert_eq!(
        status["settlement"]["cash_credits"][0]["amount_atoms"],
        "1000000000000000"
    );
    assert_eq!(
        status["settlement"]["cash_credits"][0]["asset"]["network"],
        "ethereum:1"
    );
}
