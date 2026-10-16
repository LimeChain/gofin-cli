//! Synthetic EVM-to-Solana Relay boundary: exact external recipient, one finite
//! authorization, durable source dispatch, and no settlement from API status.
//! The provider and EVM RPC are controlled fixtures; no network request is made.
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bincode::Options;
use ed25519_dalek::Signer as _;
use gofin_bridge::*;
use gofin_broker::{BridgeAdapter, SharedUsdValuator};
use gofin_domain::{
    Adapter, ApprovalAudit, AssetId, AuthorizationChallenge, Authorizer,
    BridgeStepKind as DomainStepKind, OperationRequest, Quantity, Result, Status,
    boundaries::BoundaryAuthorizationRequest, now,
};
use gofin_engine::{Engine, EngineOptions};
use gofin_ethereum::{EthereumAdapter, EthereumConfig, WalletConnection};
use gofin_evm::{
    EvmRpc, EvmRuntime, LocalEnvelopeSigner,
    alloy::{
        consensus::{Transaction, TxEnvelope},
        eips::eip2718::Decodable2718,
        primitives::{U256, keccak256},
        sol_types::SolCall,
    },
    lower_address,
};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use gofin_store::{BridgeStateRepository, Store};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use solana_instruction::{AccountMeta, Instruction};
use solana_message::{Message, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_transaction::versioned::VersionedTransaction;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

const DEPOSITORY: &str = "0x4cd00e387622c35bddb9b4c962c136462338bc31";
const RECIPIENT: &str = "GTtzwxqy67xx9DVESJjx28TgXqpc8xTqtiytgNMaQBTE";
const REQUEST_ID: &str = "0x5555555555555555555555555555555555555555555555555555555555555555";
const ORDER_ID: &str = "0x7777777777777777777777777777777777777777777777777777777777777777";
const HUB_HASH: &str = "0x9999999999999999999999999999999999999999999999999999999999999999";
const HUB_EXECUTOR: &str = "0x6747c7e64b90dccc0d54f865cbd9513156adbc0c";
const HUB_ALLOCATOR: &str = "0x613d3c588f6b8f89302b463f8f19f7241b2857e2";
const SOLANA_DEPOSITORY: &str = gofin_bridge::relay::solana::DEPOSITORY_PROGRAM;
const SOLANA_SYSTEM: &str = "11111111111111111111111111111111";
const ED25519_PROGRAM: &str = "Ed25519SigVerify111111111111111111111111111";
gofin_evm::alloy::sol! { interface Depository { function depositNative(address depositor, bytes32 id) external payable; } }

fn q(s: &str) -> Quantity {
    Quantity::new(s).unwrap()
}
fn hash() -> String {
    format!("0x{}", "44".repeat(32))
}
fn word_address(address: &str) -> String {
    format!("{:0>64}", &address[2..])
}
fn word(value: U256) -> String {
    format!("{value:064x}")
}
fn compact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, v| !v.is_null());
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

fn key(value: &str) -> Vec<u8> {
    bs58::decode(value).into_vec().unwrap()
}
fn vault() -> String {
    Pubkey::find_program_address(&[b"vault"], &SOLANA_DEPOSITORY.parse().unwrap())
        .0
        .to_string()
}
fn payout_payload() -> Vec<u8> {
    let mut bytes = vec![7; 32];
    bytes.extend(key(RECIPIENT));
    bytes.push(0);
    bytes.extend(5_000_000u64.to_le_bytes());
    bytes.extend(42u64.to_le_bytes());
    bytes.extend(1_800_000_000i64.to_le_bytes());
    bytes.extend(key(&vault()));
    bytes
}
fn hub_logs() -> Vec<Value> {
    let payload = payout_payload();
    let order_address =
        gofin_bridge::relay::solana::derive_order_address("ethereum", ORDER_ID).unwrap();
    let withdrawal = format!("0x{}", "09".repeat(32));
    let mut executed_data = vec![0; 96];
    executed_data[31] = 1;
    executed_data[63] = 2;
    executed_data[64..].copy_from_slice(&hex::decode(&withdrawal[2..]).unwrap());
    let padded = payload.len().div_ceil(32) * 32;
    let mut built_data = vec![0; 96 + padded + 64];
    built_data[31] = 64;
    built_data[56..64].copy_from_slice(&((96 + padded) as u64).to_be_bytes());
    built_data[95] = payload.len() as u8;
    built_data[96..96 + payload.len()].copy_from_slice(&payload);
    built_data[96 + padded + 31] = 1;
    built_data[96 + padded + 32..].copy_from_slice(&Sha256::digest(&payload));
    let base = |address: &str, topics: Vec<String>, data: Vec<u8>, index: u64| {
        json!({
        "address":address,"topics":topics,"data":format!("0x{}",hex::encode(data)),
        "logIndex":format!("{index:#x}"),"transactionHash":HUB_HASH,
        "blockNumber":"0x64","blockHash":hash(),"removed":false})
    };
    vec![
        base(
            HUB_EXECUTOR,
            vec![
                format!(
                    "{:#x}",
                    keccak256("Executed(address,uint256,uint256,uint256,uint256,bytes32)")
                ),
                format!("0x{:0>64}", &order_address[2..]),
                format!("0x{:064x}", 1),
                format!("0x{:064x}", 2),
            ],
            executed_data,
            0,
        ),
        base(
            HUB_ALLOCATOR,
            vec![
                format!("{:#x}", keccak256("PayloadBuilt(bytes32,bytes,bytes32[])")),
                withdrawal,
            ],
            built_data,
            1,
        ),
    ]
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
            usd_per_unit: "2000".into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic fixed price".into(),
        })
    }
}

struct Rpc {
    sender: String,
    sends: AtomicUsize,
    sent: Mutex<BTreeMap<String, (Value, Value)>>,
}
#[async_trait]
impl EvmRpc for Rpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        Ok(match method {
            "eth_chainId" => json!("0x1"),
            "eth_getCode" => json!("0x60006000"),
            "eth_getBalance" => json!("0xde0b6b3a7640000"),
            "eth_call" => json!("0x"),
            "eth_estimateGas" => json!("0x7530"),
            "eth_getTransactionCount" => json!(format!("{:#x}", self.sends.load(Ordering::SeqCst))),
            "eth_maxPriorityFeePerGas" => json!("0x3b9aca00"),
            "eth_blockNumber" => json!("0x64"),
            "eth_getBlockByNumber" => {
                json!({"hash":hash(),"number":"0x64","baseFeePerGas":"0x3b9aca00"})
            }
            "eth_sendRawTransaction" => {
                let raw =
                    hex::decode(params[0].as_str().unwrap().trim_start_matches("0x")).unwrap();
                let tx = TxEnvelope::decode_2718(&mut raw.as_slice()).unwrap();
                let tx_hash = format!("{:#x}", keccak256(&raw));
                let log = json!({"address":DEPOSITORY,
                    "topics":[format!("{:#x}",keccak256("RelayNativeDeposit(address,uint256,bytes32)"))],
                    "data":format!("0x{}{}{}",word_address(&self.sender),word(tx.value()),&ORDER_ID[2..]),
                    "logIndex":"0x0","transactionHash":tx_hash,"blockNumber":"0x64",
                    "blockHash":hash(),"removed":false});
                let transaction = json!({"hash":tx_hash,"from":self.sender,
                    "to":lower_address(tx.to().unwrap()),"value":format!("{:#x}",tx.value()),
                    "input":format!("0x{}",hex::encode(tx.input())),"nonce":format!("{:#x}",tx.nonce()),
                    "gas":format!("{:#x}",tx.gas_limit()),"chainId":"0x1","type":"0x2",
                    "maxFeePerGas":format!("{:#x}",tx.max_fee_per_gas()),
                    "maxPriorityFeePerGas":format!("{:#x}",tx.max_priority_fee_per_gas().unwrap()),
                    "accessList":[],"blockNumber":"0x64","blockHash":hash()});
                let receipt = json!({"transactionHash":tx_hash,"blockNumber":"0x64","blockHash":hash(),
                    "status":"0x1","gasUsed":"0x7d00","effectiveGasPrice":"0x3b9aca00","logs":[log]});
                self.sent
                    .lock()
                    .unwrap()
                    .insert(tx_hash.clone(), (transaction, receipt));
                self.sends.fetch_add(1, Ordering::SeqCst);
                json!(tx_hash)
            }
            "eth_getTransactionByHash" | "eth_getTransactionReceipt" => self
                .sent
                .lock()
                .unwrap()
                .get(params[0].as_str().unwrap())
                .map(|(tx, receipt)| {
                    if method == "eth_getTransactionReceipt" {
                        receipt.clone()
                    } else {
                        tx.clone()
                    }
                })
                .unwrap_or(Value::Null),
            other => panic!("unexpected synthetic EVM RPC {other}: {params}"),
        })
    }
}

struct HubRpc;
#[async_trait]
impl EvmRpc for HubRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        Ok(match method {
            "eth_chainId" => json!("0x83471"),
            "eth_blockNumber" => json!("0x65"),
            "eth_getBlockByNumber" if params[0] == "finalized" => {
                // The Hub receipt is in 0x64 and this workflow requires two
                // confirmations under the finalized head, hence at least 0x65.
                json!({"hash":format!("0x{}", "55".repeat(32)),"number":"0x65","baseFeePerGas":"0x1"})
            }
            "eth_getBlockByNumber" => json!({"hash":hash(),"number":"0x64","baseFeePerGas":"0x1"}),
            "eth_getTransactionByHash" => {
                assert_eq!(params[0], HUB_HASH);
                json!({"hash":HUB_HASH,"from":HUB_EXECUTOR,"blockNumber":"0x64","blockHash":hash()})
            }
            "eth_getTransactionReceipt" => {
                assert_eq!(params[0], HUB_HASH);
                json!({"transactionHash":HUB_HASH,"blockNumber":"0x64","blockHash":hash(),
                    "status":"0x1","gasUsed":"0x5208","effectiveGasPrice":"0x1","logs":hub_logs()})
            }
            other => panic!("unexpected synthetic Relay Hub RPC {other}: {params}"),
        })
    }
}

struct PayoutRpc {
    finalized: BTreeMap<String, Value>,
}
impl PayoutRpc {
    fn new() -> (Arc<Self>, String, String) {
        let mut finalized = BTreeMap::new();
        let bad = Self::transaction(3_999_999, 1);
        let good = Self::transaction(5_000_000, 2);
        finalized.insert(bad.0.clone(), bad.1);
        finalized.insert(good.0.clone(), good.1);
        (Arc::new(Self { finalized }), bad.0, good.0)
    }
    fn transaction(credit: u64, nonce: u8) -> (String, Value) {
        let signer = ed25519_dalek::SigningKey::from_bytes(&[42; 32]);
        let payer = Pubkey::new_from_array(signer.verifying_key().to_bytes());
        let meta = |address: &str| AccountMeta::new(address.parse().unwrap(), false);
        let mut accounts = vec![meta(SOLANA_SYSTEM); 12];
        accounts[2] = meta(RECIPIENT);
        accounts[3] = meta(&vault());
        let mut data = Sha256::digest(b"global:execute_transfer")[..8].to_vec();
        data.extend(payout_payload());
        let instructions = vec![
            Instruction {
                program_id: ED25519_PROGRAM.parse().unwrap(),
                accounts: vec![],
                data: vec![],
            },
            Instruction {
                program_id: SOLANA_DEPOSITORY.parse().unwrap(),
                accounts,
                data,
            },
        ];
        let message = VersionedMessage::Legacy(Message::new_with_blockhash(
            &instructions,
            Some(&payer),
            &solana_hash::Hash::new_from_array([nonce; 32]),
        ));
        let message_bytes = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .serialize(&message)
            .unwrap();
        let signature = solana_signature::Signature::from(signer.sign(&message_bytes).to_bytes());
        let tx = VersionedTransaction {
            signatures: vec![signature],
            message,
        };
        let keys = tx
            .message
            .static_account_keys()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let mut pre = vec![100u64; keys.len()];
        let mut post = pre.clone();
        let payer_index = keys
            .iter()
            .position(|key| *key == payer.to_string())
            .unwrap();
        let recipient_index = keys.iter().position(|key| key == RECIPIENT).unwrap();
        pre[payer_index] = 1_000_000_000;
        post[payer_index] = 999_995_000;
        post[recipient_index] += credit;
        let wire = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .serialize(&tx)
            .unwrap();
        (
            signature.to_string(),
            json!({"slot":99,"transaction":[STANDARD.encode(wire),"base64"],
            "meta":{"fee":5000,"err":null,"preBalances":pre,"postBalances":post,
                "preTokenBalances":[],"postTokenBalances":[],"loadedAddresses":{"writable":[],"readonly":[]},
                "innerInstructions":[],"logMessages":[]}}),
        )
    }
}
#[async_trait]
impl gofin_solana::SolanaRpc for PayoutRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        Ok(match method {
            "getSignatureStatuses" => {
                json!({"value":[self.finalized.get(params[0][0].as_str().unwrap())
                .map(|_| json!({"slot":99,"confirmationStatus":"finalized","err":null})).unwrap_or(Value::Null)]})
            }
            "getTransaction" => self
                .finalized
                .get(params[0].as_str().unwrap())
                .cloned()
                .unwrap_or(Value::Null),
            other => panic!("unexpected synthetic Solana payout RPC {other}: {params}"),
        })
    }
}

struct RelayFixture {
    phase: AtomicUsize,
    bad_signature: String,
    good_signature: String,
}
#[async_trait]
impl BridgeProvider for RelayFixture {
    fn capabilities(&self) -> BridgeCapabilities {
        BridgeCapabilities {
            provider: "relay".into(),
            chains: BTreeSet::from([1, 792703809]),
            native_eth: true,
            erc20: false,
            protocol_versions: BTreeSet::from(["v2".into()]),
        }
    }
    async fn discover_routes(&self, request: &BridgeRouteRequest) -> Result<BridgeRouteDiscovery> {
        assert!(request.source.is_some());
        assert_eq!(request.destination_chain_id, 792703809);
        let destination = request.solana_destination.as_ref().unwrap();
        Ok(BridgeRouteDiscovery {
            provider: "relay".into(),
            availability: RouteAvailability::Candidate,
            candidates: vec![BridgeRouteCandidate {
                destination: None,
                solana_destination: Some(SolanaAssetInput {
                    network: destination.network.clone(),
                    mint: destination.mint.clone(),
                }),
                reported_decimals: None,
            }],
            observed_at: now(),
            evidence_source: "controlled Relay route fixture".into(),
            complete: false,
            limitations: vec!["Executable quote required".into()],
            error: None,
        })
    }
    async fn quote(&self, request: &BridgeQuoteRequest) -> Result<BridgeQuoteBatch> {
        let source = request.intent.evm_source()?;
        let destination = request.intent.solana_destination.as_ref().unwrap();
        let call = Depository::depositNativeCall {
            depositor: gofin_evm::address(&request.intent.source_address)?,
            id: gofin_evm::hash(ORDER_ID)?,
        }
        .abi_encode();
        let step = BridgeStep {
            ordinal: 0,
            kind: BridgeStepKind::Deposit,
            call: Some(EvmCall {
                chain_id: 1,
                to: DEPOSITORY.into(),
                value: request.intent.amount.clone(),
                data: format!("0x{}", hex::encode(call)),
            }),
            solana_mint: None,
            solana_transaction: None,
            expected_sender: request.intent.source_address.clone(),
            maximum_debit: vec![source.amount(request.intent.amount.clone())],
            dependencies: vec![],
        };
        Ok(BridgeQuoteBatch {
            failures: vec![],
            quotes: vec![BridgeQuote {
                cctp: None,
                quote_id: "synthetic-solana-relay".into(),
                provider: "relay".into(),
                route_id: "synthetic-fixed-solana-route".into(),
                intent: request.intent.clone(),
                gross_debit: source.amount(request.intent.amount.clone()),
                expected_receipt: gofin_domain::Amount {
                    asset: destination.asset.clone(),
                    atoms: q("5000000"),
                },
                minimum_receipt: gofin_domain::Amount {
                    asset: destination.asset.clone(),
                    atoms: q("4000000"),
                },
                fees: vec![],
                approval: None,
                protocol_steps: vec![step],
                estimated_arrival_seconds: Some(1),
                observed_at: now(),
                valid_until: (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339(),
                class: QuoteClass::Executable,
                refund_paths: vec![RefundPath {
                    chain_id: 1,
                    network: None,
                    asset: source.asset_id(),
                    recipient: request.intent.refund_to.clone(),
                    minimum: q("1"),
                    maximum: request.intent.amount.clone(),
                }],
                order_solver: Some("0xf70da97812cb96acdf810712aa562db8dfa3dbef".into()),
                protocol_order_id: Some(ORDER_ID.into()),
                provider_request_id: Some(REQUEST_ID.into()),
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
            fixed_scope_digest: "synthetic-solana-scope".into(),
        })
    }
    async fn reconcile(&self, transfer: &BridgeTransferRef) -> Result<BridgeReconciliation> {
        let phase = self.phase.load(Ordering::SeqCst);
        let candidate = |kind, chain_id, transaction_hash: String| BridgeEvidenceCandidate {
            order_id: transfer.order_id.clone(),
            kind,
            chain_id,
            transaction_hash,
            correlation: None,
        };
        let mut candidates = vec![];
        if phase >= 1 {
            candidates.push(candidate(
                EvidenceKind::SettlementHub,
                537713,
                HUB_HASH.into(),
            ));
            candidates.push(candidate(
                EvidenceKind::DestinationFill,
                792703809,
                self.bad_signature.clone(),
            ));
        }
        if phase >= 2 {
            candidates.push(candidate(
                EvidenceKind::DestinationFill,
                792703809,
                self.good_signature.clone(),
            ));
        }
        Ok(BridgeReconciliation {
            status: "success".into(),
            failure: None,
            refund_fail_reason: None,
            candidates,
        })
    }
}

#[tokio::test]
async fn relay_external_solana_recipient_needs_one_grant_and_independent_payout_proof() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("journal.sqlite"))
        .unwrap()
        .share();
    let state = BridgeStateRepository::new(store.clone());
    let signer = Arc::new(LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap());
    let address = lower_address(signer.address());
    let rpc = Arc::new(Rpc {
        sender: address.clone(),
        sends: AtomicUsize::new(0),
        sent: Mutex::new(BTreeMap::new()),
    });
    let runtime = Arc::new(EvmRuntime::new(rpc.clone(), 1, Some(signer)));
    let hub = Arc::new(EvmRuntime::new(Arc::new(HubRpc), 537713, None));
    let (solana_rpc, bad_signature, good_signature) = PayoutRpc::new();
    let provider = Arc::new(RelayFixture {
        phase: AtomicUsize::new(0),
        bad_signature,
        good_signature,
    });
    let prices = Arc::new(SharedUsdValuator::with_source(Arc::new(Prices)));
    let wallet = Arc::new(
        EthereumAdapter::new(
            EthereumConfig::wallet_only(1, 1, vec![]),
            vec![WalletConnection {
                name: "source".into(),
                address: address.clone(),
                signing_enabled: true,
            }],
            runtime.clone(),
        )
        .unwrap()
        .with_wallet_valuation(prices.clone()),
    );
    let observer_address = bs58::encode([99u8; 32]).into_string();
    let solana_observer = Arc::new(
        gofin_solana::SolanaWalletAdapter::new(
            "observer",
            observer_address,
            None,
            solana_rpc,
            Arc::new(Prices),
        )
        .unwrap(),
    );
    let bridge = Arc::new(
        BridgeAdapter::with_provider_and_observers(
            provider.clone(),
            BTreeMap::from([(1, wallet.clone())]),
            BTreeMap::from([(1, runtime), (537713, hub)]),
            prices,
        )
        .unwrap()
        .with_solana_wallets(BTreeMap::from([("observer".into(), solana_observer)])),
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
    let bounds = BridgeBounds {
        maximum_source_debit: Some(q("5000000000000000")),
        minimum_net_receipt: Some(gofin_domain::Amount {
            asset: AssetId {
                network: "solana:mainnet-beta".into(),
                reference: "native".into(),
                decimals: 9,
            },
            atoms: q("4000000"),
        }),
        fee_caps: vec![FeeCap {
            asset: EvmAsset {
                chain_id: 1,
                token: None,
                decimals: 18,
            }
            .asset_id(),
            kind: FeeKind::Gas,
            maximum: q("200000000000000"),
            maximum_usd_micros: None,
        }],
        ..Default::default()
    };
    let routes = bridge
        .read(
            "bridge.routes",
            &json!({"source":{"chainId":1,"token":null},
        "solanaDestination":{"network":"solana:mainnet-beta","mint":null}}),
        )
        .await
        .unwrap();
    assert_eq!(
        routes["routes"][0]["solana_destination"]["mint"],
        Value::Null
    );
    let quote = bridge
        .read(
            "bridge.quotes",
            &json!({"provider":"relay","intent":{
        "wallet":"source","source":{"chainId":1,"token":null},
        "solanaDestination":{"network":"solana:mainnet-beta","wallet":null,"mint":null},
        "recipient":RECIPIENT,"refundTo":address,"allowedRefundAssets":[{"chainId":1,"token":null}],
        "amount":"5000000000000000"},"bounds":bounds}),
        )
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
    let saved_quote = state.transfer(&transfer).unwrap().unwrap();
    let bounds: BridgeBounds = serde_json::from_value(
        saved_quote.quote_revisions.last().unwrap().payload["bounds"].clone(),
    )
    .unwrap();
    assert_eq!(
        quote["transfers"][0]["quote"]["intent"]["recipient"],
        RECIPIENT
    );
    assert_eq!(
        quote["transfers"][0]["quote"]["intent"]["solana_destination"]["wallet"],
        Value::Null
    );
    let mut input = serde_json::to_value(BridgeTransferInput {
        transfer_id: transfer.clone(),
        step_id: step,
        step_kind: DomainStepKind::Deposit,
        bounds: Some(bounds.clone()),
    })
    .unwrap();
    compact(&mut input);
    let mut boundary = serde_json::to_value(&bounds).unwrap();
    compact(&mut boundary);
    let grant = engine
        .authorize_boundary(BoundaryAuthorizationRequest {
            request_id: "approve-relay-solana".into(),
            operation: "bridge.transfer".into(),
            input: input.clone(),
            bounds: boundary,
            valid_for_seconds: 600,
            replaces: None,
        })
        .await
        .unwrap();
    let request = OperationRequest {
        request_id: "relay-solana-deposit".into(),
        operation: "bridge.transfer".into(),
        input: input.clone(),
        run_id: None,
    };
    let executed = engine
        .execute_boundary(&grant.id, request.clone())
        .await
        .unwrap();
    assert_eq!(executed.status, Status::Completed, "{:?}", executed.error);
    assert_eq!(
        engine
            .execute_boundary(&grant.id, request)
            .await
            .unwrap()
            .id,
        executed.id
    );
    assert_eq!(rpc.sends.load(Ordering::SeqCst), 1);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    let saved = state.transfer(&transfer).unwrap().unwrap();
    assert!(saved.transit.is_some());
    assert_ne!(saved.state, gofin_domain::BridgeTransferState::Completed);
    let reopened = BridgeStateRepository::new(
        Store::open(dir.path().join("journal.sqlite"))
            .unwrap()
            .share(),
    );
    let durable = reopened.transfer(&transfer).unwrap().unwrap();
    assert_eq!(durable.transit, saved.transit);
    let status = bridge
        .read("bridge.status", &json!({"transferId":transfer}))
        .await
        .unwrap();
    assert_ne!(
        status["state"], "completed",
        "Relay's success string is not payout proof"
    );
    provider.phase.store(1, Ordering::SeqCst);
    let short = bridge
        .read("bridge.status", &json!({"transferId":transfer}))
        .await
        .unwrap();
    assert_ne!(
        short["state"], "completed",
        "A finalized SOL payout below the approved floor must fail closed"
    );
    provider.phase.store(2, Ordering::SeqCst);
    let settled = bridge
        .read("bridge.status", &json!({"transferId":transfer}))
        .await
        .unwrap();
    assert_eq!(settled["state"], "completed", "{settled}");
    let completed = state.transfer(&transfer).unwrap().unwrap();
    assert_eq!(
        completed.state,
        gofin_domain::BridgeTransferState::Completed
    );
    assert_eq!(completed.settlement_proofs.len(), 1);
    let evidence = &completed.settlement_proofs[0].evidence;
    assert_eq!(evidence["hub_transaction_hash"], HUB_HASH);
    assert_eq!(evidence["recipient"], RECIPIENT);
    assert_eq!(evidence["net_amount_atoms"], "5000000");
    assert_eq!(
        evidence["proof_method"],
        "relay_hub_payload_and_solana_depository_credit"
    );
    assert_eq!(rpc.sends.load(Ordering::SeqCst), 1);
}
