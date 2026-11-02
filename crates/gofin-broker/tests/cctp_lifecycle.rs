//! Synthetic CCTP lifecycle through real adapter, signatures, engine and journal.
//! Iris and chain transports are modeled; this is not live or fork evidence.
//! Build gofin-dev with GOFIN_DEV_BUILD_ID=synthetic-cctp-build, then set GOFIN_CCTP_CLI
//! to its absolute path to include the public CLI and development socket transport.
//! GOFIN_CCTP_BUILD_ID selects another explicitly built development binary identity.
#[cfg(target_os = "macos")]
#[path = "support/native_approval.rs"]
mod native_approval;
#[path = "support/outcome_evidence.rs"]
mod outcome_evidence;

use async_trait::async_trait;
use gofin_bridge::{BridgeBounds, FeeCap, FeeKind, cctp::*};
use gofin_broker::{BridgeAdapter, SharedUsdValuator};
use gofin_domain::{
    Adapter, ApprovalAudit, AssetId, AuthorizationChallenge, Authorizer, BridgeStepLink,
    GoFinError, OperationRequest, Quantity, ResourceId, Result, Status, now,
};
use gofin_engine::Engine;
use gofin_exchange::ccxt::{HyperliquidActionEnvelope, HyperliquidInfoRequest};
use gofin_hyperliquid::{
    DeferredExecutionState, HyperliquidAccount, HyperliquidPrices, HyperliquidProfile,
    HyperliquidTransport,
};
use gofin_protocol::BrokerRequest;
const HL_BRIDGE: &str = "0x2df1c51e09aecf9cacb7bc98cb1742757f163df7";
use gofin_ethereum::{EthereumAdapter, EthereumConfig, WalletConnection};
use gofin_evm::alloy::consensus::transaction::SignerRecoverable;
use gofin_evm::alloy::{
    consensus::{Transaction, TxEnvelope},
    eips::eip2718::Decodable2718,
    primitives::{Address, B256, Bytes, U256, keccak256},
    signers::{SignerSync, local::PrivateKeySigner},
    sol,
    sol_types::{SolCall, SolEvent, SolValue},
};
use gofin_evm::{
    EvmRpc, EvmRuntime, LocalEnvelopeSigner, address, hex_bytes, lower_address, rpc_u64, u256,
};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use gofin_store::{BridgeStateRepository, Store};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

sol! {
 function isFjord() external view returns(bool);
 function isIsthmus() external view returns(bool);
 function isJovian() external view returns(bool);
 function getL1FeeUpperBound(uint256 size) external view returns(uint256);
 function getOperatorFee(uint256 gas) external view returns(uint256);
 function usdcToken() external view returns(address);
 function paused() external view returns(bool);
 function transfer(address to,uint256 value) external returns(bool);
 function localMessageTransmitter() external view returns(address);
 function localMinter() external view returns(address);
 function messageBodyVersion() external view returns(uint32);
 function remoteTokenMessengers(uint32 domain) external view returns(bytes32);
 function localDomain() external view returns(uint32);
 function version() external view returns(uint32);
 function localTokenMessenger() external view returns(address);
 function getLocalToken(uint32 domain,bytes32 token) external view returns(address);
 function decimals() external view returns(uint8);
 function symbol() external view returns(string);
 function balanceOf(address owner) external view returns(uint256);
 function allowance(address owner,address spender) external view returns(uint256);
 function approve(address spender,uint256 amount) external returns(bool);
 function depositForBurn(uint256 amount,uint32 destinationDomain,bytes32 mintRecipient,address burnToken,bytes32 destinationCaller,uint256 maxFee,uint32 minFinalityThreshold) external;
 function receiveMessage(bytes message,bytes attestation) external returns(bool);
 function usedNonces(bytes32 nonce) external view returns(uint256);
 function signatureThreshold() external view returns(uint256);
 function isEnabledAttester(address attester) external view returns(bool);
 event Approval(address indexed owner,address indexed spender,uint256 value);
 event Transfer(address indexed from,address indexed to,uint256 value);
 event MessageSent(bytes message);
 event DepositForBurn(address indexed burnToken,uint256 amount,address indexed depositor,bytes32 mintRecipient,uint32 destinationDomain,bytes32 destinationTokenMessenger,bytes32 destinationCaller,uint256 maxFee,uint32 indexed minFinalityThreshold,bytes hookData);
 event MessageReceived(address indexed caller,uint32 sourceDomain,bytes32 indexed nonce,bytes32 sender,uint32 indexed finalityThresholdExecuted,bytes messageBody);
}
fn fail(code: &str) -> GoFinError {
    GoFinError::new(
        code,
        "fixture",
        "Modeled CCTP transport failure.",
        "Preserve the original submission identity.",
    )
}
fn block_hash(chain: u64) -> B256 {
    B256::repeat_byte(if chain == 1 { 0x11 } else { 0x22 })
}
fn block(chain: u64, number: u64) -> Value {
    json!({"hash":format!("{:#x}",block_hash(chain)),"number":format!("{number:#x}"),"timestamp":"0x2000","baseFeePerGas":"0x3b9aca00","extraData":"0x000000000000000000"})
}
fn encoded<T: SolValue>(value: T) -> Value {
    json!(hex_bytes(&value.abi_encode()))
}
fn event<E: SolEvent>(chain: u64, emitter: &str, index: u64, event: E) -> Value {
    let log = event.encode_log_data();
    json!({"address":emitter,"topics":log.topics().iter().map(|v|format!("{v:#x}")).collect::<Vec<_>>(),"data":hex_bytes(&log.data),"logIndex":format!("{index:#x}"),"removed":false,"blockNumber":"0x64","blockHash":format!("{:#x}",block_hash(chain))})
}
fn attester() -> PrivateKeySigner {
    format!("0x{}", "44".repeat(32)).parse().unwrap()
}
fn remote_messenger(destination: u32) -> B256 {
    if destination == 5 {
        B256::from_slice(
            &bs58::decode(gofin_bridge::cctp::solana::TOKEN_MESSENGER_MINTER)
                .into_vec()
                .unwrap(),
        )
    } else {
        address(TOKEN_MESSENGER).unwrap().into_word()
    }
}
fn source_message(call: &depositForBurnCall, sender: Address, chain: u64) -> Vec<u8> {
    let mut b = vec![0u8; 376];
    for (offset, value) in [
        (0, 1u32),
        (4, gofin_bridge::cctp::domain(chain).unwrap()),
        (8, call.destinationDomain),
        (140, call.minFinalityThreshold),
        (148, 1),
    ] {
        b[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }
    for (offset, word) in [
        (44, address(TOKEN_MESSENGER).unwrap().into_word()),
        (76, remote_messenger(call.destinationDomain)),
        (108, call.destinationCaller),
        (152, call.burnToken.into_word()),
        (184, call.mintRecipient),
        (248, sender.into_word()),
    ] {
        b[offset..offset + 32].copy_from_slice(word.as_slice());
    }
    b[216..248].copy_from_slice(&call.amount.to_be_bytes::<32>());
    b[280..312].copy_from_slice(&call.maxFee.to_be_bytes::<32>());
    b
}
#[derive(Default)]
struct ChainState {
    owned_forks: Option<BTreeMap<u64, Arc<owned_fork::Fork>>>,
    alternate_fork_signer: bool,
    source_chain: u64,
    destination_chain: u64,
    refresh_fault: Option<Arc<stack_refresh::RefreshFault>>,
    transactions: BTreeMap<(u64, String), (Value, Value)>,
    broadcasts: BTreeMap<u64, usize>,
    allowance: U256,
    source_balance: U256,
    destination_balance: U256,
    source_message: Option<Vec<u8>>,
    source_tx: Option<String>,
    used_nonces: BTreeSet<String>,
    attestation_nonce: Option<u8>,
    attestation_fee: Option<u64>,
    attestation_ready: bool,
    omit_credit: bool,
    hide_destination_receipt: bool,
    reject_destination_log_scan: bool,
    destination_log_scans: usize,
    gas_missing: bool,
    reject_mint_revalidation: bool,
    mint_simulations: usize,
    source_gas_balance: Option<U256>,
    reject_preparation_reads: bool,
    stall_source_response: bool,
    release_source_response: Arc<AtomicBool>,
    source_fee_after_approval: bool,
    source_large_fee_after_approval: bool,
    lose_response: Option<u64>,
    lose_source_ordinal: Option<usize>,
    revert_next: Option<u64>,
    mutate_attestation: bool,
    attestation_expiration: Option<u64>,
    source_head: Option<u64>,
    reattest_requests: usize,
    hl_tx: Option<String>,
    hl_amount: U256,
    hl_credit_visible: bool,
}
impl ChainState {
    fn source_chain(&self) -> u64 {
        if self.source_chain == 0 {
            1
        } else {
            self.source_chain
        }
    }

    fn destination_chain(&self) -> u64 {
        if self.destination_chain == 0 {
            42161
        } else {
            self.destination_chain
        }
    }

    fn attested(&self) -> Option<(String, String)> {
        let mut raw = self.source_message.clone()?;
        raw[12..44].fill(self.attestation_nonce.unwrap_or(0x77));
        let threshold = u32::from_be_bytes(raw[140..144].try_into().unwrap());
        raw[144..148].copy_from_slice(&threshold.to_be_bytes());
        let fee = self
            .attestation_fee
            .map(U256::from)
            .unwrap_or_else(|| U256::from_be_slice(&raw[280..312]));
        raw[312..344].copy_from_slice(&fee.to_be_bytes::<32>());
        if let Some(expiration) = self.attestation_expiration {
            raw[344..376].copy_from_slice(&U256::from(expiration).to_be_bytes::<32>());
        }
        if self.mutate_attestation {
            raw[8..12].copy_from_slice(&6u32.to_be_bytes());
        }
        let signature = attester().sign_hash_sync(&keccak256(&raw)).unwrap();
        Some((hex_bytes(&raw), hex_bytes(&signature.as_bytes())))
    }
}
struct Iris {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Iris {
    fn new(state: Arc<Mutex<ChainState>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let shutdown = stop.clone();
        let thread = std::thread::spawn(move || {
            while !shutdown.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        socket.set_nonblocking(false).unwrap();
                        socket
                            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                            .unwrap();
                        let mut raw = [0; 8192];
                        let n = socket.read(&mut raw).unwrap_or(0);
                        let request = String::from_utf8_lossy(&raw[..n]);
                        let mut state = state.lock().unwrap();
                        let payload = if request.contains("/v2/reattest/") {
                            state.reattest_requests += 1;
                            state.attestation_expiration = None;
                            json!({"success":true})
                        } else if request.contains("/v2/burn/USDC/fees/") {
                            json!([{"finalityThreshold":2000,"minimumFee":0},{"finalityThreshold":1000,"minimumFee":5}])
                        } else if state.attestation_ready {
                            let (message, attestation) = state.attested().unwrap();
                            json!({"sourceTxHash":state.source_tx,"messages":[{"status":"complete","cctpVersion":2,"message":message,"attestation":attestation}]})
                        } else {
                            json!({"messages":[]})
                        };
                        let body = payload.to_string();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = socket.write_all(response.as_bytes());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(2))
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for Iris {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}
struct OwnedForkRpc {
    fork: Arc<owned_fork::Fork>,
    state: Arc<Mutex<ChainState>>,
}
#[async_trait]
impl EvmRpc for OwnedForkRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.fork.guard().await?;
        if method == "eth_maxPriorityFeePerGas" {
            let state = self.state.lock().unwrap();
            if self.fork.chain() == state.source_chain()
                && state.source_large_fee_after_approval
                && state
                    .broadcasts
                    .get(&self.fork.chain())
                    .copied()
                    .unwrap_or(0)
                    > 0
            {
                return Ok(json!("0x2e90edd000"));
            }
            if self.fork.chain() == state.source_chain()
                && state.source_fee_after_approval
                && state
                    .broadcasts
                    .get(&self.fork.chain())
                    .copied()
                    .unwrap_or(0)
                    > 0
            {
                return Ok(json!("0x77359400"));
            }
            return Ok(json!("0x3b9aca00"));
        }
        let result = self.fork.raw(method, params).await?;
        if method == "eth_sendRawTransaction" {
            *self
                .state
                .lock()
                .unwrap()
                .broadcasts
                .entry(self.fork.chain())
                .or_default() += 1;
        }
        Ok(result)
    }
}

struct Rpc {
    chain: u64,
    state: Arc<Mutex<ChainState>>,
}
impl Rpc {
    fn call(&self, p: &Value, s: &ChainState) -> Result<Value> {
        let raw = gofin_evm::rpc_bytes(&p[0]["data"])?;
        let selector = raw.get(..4).unwrap_or_default();
        if selector == localMessageTransmitterCall::SELECTOR {
            return Ok(encoded(address(MESSAGE_TRANSMITTER)?));
        }
        if selector == localMinterCall::SELECTOR {
            return Ok(encoded(address(
                "0xfd78ee919681417d192449715b2594ab58f5d002",
            )?));
        }
        if selector == messageBodyVersionCall::SELECTOR || selector == versionCall::SELECTOR {
            return Ok(encoded(1u32));
        }
        if selector == isFjordCall::SELECTOR {
            return Ok(encoded(true));
        }
        if selector == isIsthmusCall::SELECTOR || selector == isJovianCall::SELECTOR {
            return Ok(encoded(false));
        }
        if selector == getL1FeeUpperBoundCall::SELECTOR || selector == getOperatorFeeCall::SELECTOR
        {
            return Ok(encoded(U256::ZERO));
        }
        if selector == remoteTokenMessengersCall::SELECTOR {
            return Ok(encoded(remote_messenger(
                remoteTokenMessengersCall::abi_decode(&raw).unwrap().domain,
            )));
        }
        if selector == localDomainCall::SELECTOR {
            return Ok(encoded(domain(self.chain)?));
        }
        if selector == localTokenMessengerCall::SELECTOR {
            return Ok(encoded(address(TOKEN_MESSENGER)?));
        }
        if selector == getLocalTokenCall::SELECTOR {
            return Ok(encoded(address(&usdc_asset(self.chain)?.token.unwrap())?));
        }
        if selector == usdcTokenCall::SELECTOR {
            return Ok(encoded(address(&usdc_asset(42161)?.token.unwrap())?));
        }
        if selector == pausedCall::SELECTOR {
            return Ok(encoded(false));
        }
        if selector == transferCall::SELECTOR {
            let call = transferCall::abi_decode(&raw).map_err(|_| fail("BAD_TRANSFER"))?;
            if call.to != address(HL_BRIDGE)? || s.destination_balance < call.value {
                return Err(fail("INSUFFICIENT_HL_FUNDING"));
            }
            return Ok(encoded(true));
        }
        if selector == symbolCall::SELECTOR {
            return Ok(encoded("USDC".to_owned()));
        }
        if selector == decimalsCall::SELECTOR {
            return Ok(encoded(U256::from(6)));
        }
        if selector == balanceOfCall::SELECTOR {
            return Ok(encoded(if self.chain != s.destination_chain() {
                s.source_balance
            } else {
                s.destination_balance
            }));
        }
        if selector == allowanceCall::SELECTOR {
            return Ok(encoded(if p[1] == "0x63" {
                U256::ZERO
            } else {
                s.allowance
            }));
        }
        if selector == signatureThresholdCall::SELECTOR {
            return Ok(encoded(U256::from(1)));
        }
        if selector == isEnabledAttesterCall::SELECTOR {
            let call =
                isEnabledAttesterCall::abi_decode(&raw).map_err(|_| fail("BAD_ATTESTER_CALL"))?;
            return Ok(encoded(call.attester == attester().address()));
        }
        if selector == usedNoncesCall::SELECTOR {
            let call = usedNoncesCall::abi_decode(&raw).unwrap();
            return Ok(encoded(U256::from(u8::from(
                s.used_nonces.contains(&hex_bytes(call.nonce.as_slice())),
            ))));
        }
        if selector == approveCall::SELECTOR {
            return Ok(encoded(true));
        }
        if selector == depositForBurnCall::SELECTOR {
            let call = depositForBurnCall::abi_decode(&raw).map_err(|_| fail("BAD_BURN"))?;
            if s.allowance < call.amount || s.source_balance < call.amount {
                return Err(fail("RPC_REQUEST_FAILED"));
            }
            return Ok(json!("0x"));
        }
        if selector == receiveMessageCall::SELECTOR {
            let call = receiveMessageCall::abi_decode(&raw).unwrap();
            let message = decode_message(&hex_bytes(&call.message))?;
            if s.used_nonces.contains(&message.nonce) {
                return Err(fail("RPC_REQUEST_FAILED"));
            }
            return Ok(encoded(true));
        }
        Err(GoFinError::validation(format!(
            "Unexpected fixture selector {}",
            hex_bytes(selector)
        )))
    }
}
#[async_trait]
impl EvmRpc for Rpc {
    async fn request(&self, method: &str, p: Value) -> Result<Value> {
        let mut response_gate = None;
        let result = {
            let mut s = self.state.lock().unwrap();
            if s.reject_preparation_reads && method == "eth_getBalance" {
                return Err(fail("RPC_UNAVAILABLE"));
            }
            if method == "eth_call"
                && p[0]["data"]
                    .as_str()
                    .is_some_and(|data| data.starts_with(&hex_bytes(&receiveMessageCall::SELECTOR)))
            {
                s.mint_simulations += 1;
                if s.reject_mint_revalidation && s.mint_simulations > 1 {
                    return Err(fail("RPC_REQUEST_FAILED"));
                }
            }
            match method {
                "eth_chainId" => json!(format!("{:#x}", self.chain)),
                "eth_getCode" => json!(if p[0].as_str().is_some_and(|address| address
                    .eq_ignore_ascii_case("0x1111111111111111111111111111111111111111"))
                {
                    "0x"
                } else {
                    "0x6000"
                }),
                "eth_getBalance" => {
                    json!(if self.chain == s.destination_chain() && s.gas_missing {
                        "0x0".to_owned()
                    } else if self.chain != s.destination_chain() && s.source_gas_balance.is_some()
                    {
                        format!("{:#x}", s.source_gas_balance.unwrap())
                    } else {
                        "0x56bc75e2d63100000".to_owned()
                    })
                }
                "eth_blockNumber" => json!(format!(
                    "{:#x}",
                    if self.chain != s.destination_chain() {
                        s.source_head.unwrap_or(110)
                    } else {
                        110
                    }
                )),
                "eth_getBlockByHash" => block(self.chain, 100),
                "eth_getBlockByNumber" => block(
                    self.chain,
                    if p[0] == "latest" || p[0] == "finalized" {
                        110
                    } else {
                        rpc_u64(&p[0])?
                    },
                ),
                "eth_getTransactionCount" => json!(format!(
                    "{:#x}",
                    s.transactions
                        .iter()
                        .filter(|((chain, _), (tx, _))| *chain == self.chain && tx["from"] == p[0])
                        .count()
                )),
                "eth_maxPriorityFeePerGas" => json!(if self.chain == s.source_chain()
                    && s.source_large_fee_after_approval
                    && s.allowance > U256::ZERO
                {
                    "0x4a817c800"
                } else if self.chain == s.source_chain()
                    && s.source_fee_after_approval
                    && s.allowance > U256::ZERO
                {
                    "0x77359400"
                } else {
                    "0x3b9aca00"
                }),
                "eth_estimateGas" => json!("0x186a0"),
                "eth_call" if p[0]["data"].as_str().is_none_or(|data| data == "0x") => json!("0x"),
                "eth_call" => self.call(&p, &s)?,
                "eth_getLogs" => {
                    if self.chain == s.destination_chain() {
                        s.destination_log_scans += 1;
                        if s.reject_destination_log_scan {
                            return Err(fail("RPC_UNAVAILABLE"));
                        }
                    }
                    json!(
                        s.transactions
                            .iter()
                            .filter(|((chain, _), _)| *chain == self.chain)
                            .flat_map(|(_, (_, r))| r["logs"].as_array().unwrap().clone())
                            .collect::<Vec<_>>()
                    )
                }
                "eth_getTransactionReceipt" | "eth_getTransactionByHash" => {
                    if self.chain == s.destination_chain() && s.hide_destination_receipt {
                        Value::Null
                    } else {
                        s.transactions
                            .get(&(self.chain, p[0].as_str().unwrap_or_default().into()))
                            .map(|(tx, r)| {
                                if method == "eth_getTransactionReceipt" {
                                    r.clone()
                                } else {
                                    tx.clone()
                                }
                            })
                            .unwrap_or(Value::Null)
                    }
                }
                "eth_sendRawTransaction" => {
                    let raw = gofin_evm::rpc_bytes(&p[0])?.as_ref().to_vec();
                    let envelope = TxEnvelope::decode_2718(&mut raw.as_slice())
                        .map_err(|_| fail("BAD_SIGNED_TRANSACTION"))?;
                    let sender = envelope.recover_signer().map_err(|_| fail("BAD_SENDER"))?;
                    let tx_hash = format!("{:#x}", envelope.tx_hash());
                    if s.transactions.contains_key(&(self.chain, tx_hash.clone())) {
                        return Ok(json!(tx_hash));
                    }
                    let reverted = s.revert_next == Some(self.chain);
                    if reverted {
                        s.revert_next = None;
                    }
                    let selector = envelope.input().get(..4).unwrap_or_default();
                    let mut logs = vec![];
                    if !reverted {
                        if selector == approveCall::SELECTOR {
                            let call = approveCall::abi_decode(envelope.input()).unwrap();
                            s.allowance = call.amount;
                            logs.push(event(
                                self.chain,
                                &usdc_asset(self.chain)?.token.unwrap(),
                                0,
                                Approval {
                                    owner: sender,
                                    spender: call.spender,
                                    value: call.amount,
                                },
                            ));
                        } else if selector == depositForBurnCall::SELECTOR {
                            let call = depositForBurnCall::abi_decode(envelope.input()).unwrap();
                            if s.allowance < call.amount || s.source_balance < call.amount {
                                return Err(fail("INSUFFICIENT_BURN_BALANCE"));
                            }
                            s.source_balance -= call.amount;
                            s.allowance -= call.amount;
                            let raw = source_message(&call, sender, self.chain);
                            s.source_message = Some(raw.clone());
                            s.source_tx = Some(tx_hash.clone());
                            logs.push(event(
                                self.chain,
                                MESSAGE_TRANSMITTER,
                                0,
                                MessageSent {
                                    message: Bytes::from(raw),
                                },
                            ));
                            logs.push(event(
                                self.chain,
                                TOKEN_MESSENGER,
                                1,
                                DepositForBurn {
                                    burnToken: call.burnToken,
                                    amount: call.amount,
                                    depositor: sender,
                                    mintRecipient: call.mintRecipient,
                                    destinationDomain: call.destinationDomain,
                                    destinationTokenMessenger: remote_messenger(
                                        call.destinationDomain,
                                    ),
                                    destinationCaller: call.destinationCaller,
                                    maxFee: call.maxFee,
                                    minFinalityThreshold: call.minFinalityThreshold,
                                    hookData: Bytes::new(),
                                },
                            ));
                        } else if selector == receiveMessageCall::SELECTOR {
                            let call = receiveMessageCall::abi_decode(envelope.input()).unwrap();
                            let message = decode_message(&hex_bytes(&call.message))?;
                            if s.used_nonces.contains(&message.nonce) {
                                return Err(fail("ALREADY_MINTED"));
                            }
                            let net = u256(&message.amount)? - u256(&message.fee_executed)?;
                            s.used_nonces.insert(message.nonce.clone());
                            if !s.omit_credit {
                                s.destination_balance += net;
                                logs.push(event(
                                    self.chain,
                                    &usdc_asset(self.chain)?.token.unwrap(),
                                    0,
                                    Transfer {
                                        from: Address::ZERO,
                                        to: address(&message.mint_recipient)?,
                                        value: net,
                                    },
                                ));
                            }
                            logs.push(event(
                                self.chain,
                                MESSAGE_TRANSMITTER,
                                1,
                                MessageReceived {
                                    caller: sender,
                                    sourceDomain: message.source_domain,
                                    nonce: message.nonce.parse().unwrap(),
                                    sender: address(TOKEN_MESSENGER)?.into_word(),
                                    finalityThresholdExecuted: message.finality_threshold_executed,
                                    messageBody: Bytes::copy_from_slice(&call.message[148..]),
                                },
                            ));
                        } else if selector == transferCall::SELECTOR {
                            let call = transferCall::abi_decode(envelope.input()).unwrap();
                            if self.chain != s.destination_chain()
                                || call.to != address(HL_BRIDGE)?
                                || s.destination_balance < call.value
                            {
                                return Err(fail("BAD_HL_DEPOSIT"));
                            }
                            s.destination_balance -= call.value;
                            s.hl_tx = Some(tx_hash.clone());
                            s.hl_amount = call.value;
                            logs.push(event(
                                self.chain,
                                &usdc_asset(self.chain)?.token.unwrap(),
                                0,
                                Transfer {
                                    from: sender,
                                    to: call.to,
                                    value: call.value,
                                },
                            ));
                        } else if selector.is_empty() && envelope.value() > U256::ZERO {
                            // Independent native transfer used to prove that an
                            // in-flight bridge does not hold unrelated funds.
                        } else {
                            return Err(fail("UNREVIEWED_FINANCIAL_CALL"));
                        }
                    }
                    for log in &mut logs {
                        log["transactionHash"] = json!(tx_hash);
                    }
                    let tx = json!({"hash":tx_hash,"from":lower_address(sender),"to":lower_address(envelope.to().unwrap()),"value":format!("{:#x}",envelope.value()),"input":hex_bytes(envelope.input()),"nonce":format!("{:#x}",envelope.nonce()),"gas":format!("{:#x}",envelope.gas_limit()),"chainId":format!("{:#x}",self.chain),"type":"0x2","maxFeePerGas":format!("{:#x}",envelope.max_fee_per_gas()),"maxPriorityFeePerGas":format!("{:#x}",envelope.max_priority_fee_per_gas().unwrap()),"accessList":[],"blockNumber":"0x64","blockHash":format!("{:#x}",block_hash(self.chain))});
                    let receipt = json!({"transactionHash":tx_hash,"blockNumber":"0x64","blockHash":format!("{:#x}",block_hash(self.chain)),"status":if reverted{"0x0"}else{"0x1"},"gasUsed":"0x7530","effectiveGasPrice":"0x3b9aca00","l1Fee":"0x0","logs":logs});
                    s.transactions
                        .insert((self.chain, tx_hash.clone()), (tx, receipt));
                    *s.broadcasts.entry(self.chain).or_default() += 1;
                    if self.chain != s.destination_chain()
                        && let Some(balance) = s.source_gas_balance.as_mut()
                    {
                        *balance -= U256::from(30_000_000_000_000u64);
                    }
                    if self.chain == s.source_chain() && s.stall_source_response {
                        s.stall_source_response = false;
                        response_gate = Some(s.release_source_response.clone());
                    }
                    if self.chain == s.source_chain()
                        && s.lose_source_ordinal == s.broadcasts.get(&self.chain).copied()
                    {
                        s.lose_source_ordinal = None;
                        return Err(fail("RPC_UNAVAILABLE"));
                    }
                    if s.lose_response == Some(self.chain) {
                        s.lose_response = None;
                        return Err(fail("RPC_UNAVAILABLE"));
                    }
                    json!(tx_hash)
                }
                other => {
                    return Err(GoFinError::validation(format!(
                        "Unexpected fixture RPC {other}"
                    )));
                }
            }
        };
        if let Some(release) = response_gate {
            while !release.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
        Ok(result)
    }
}
struct Prices;
#[async_trait]
impl UsdPriceSource for Prices {
    async fn quote(&self, asset: &AssetId) -> Result<UsdQuote> {
        Ok(UsdQuote {
            usd_per_unit: if asset.reference == "native" {
                "2000"
            } else {
                "1"
            }
            .into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic CCTP lifecycle".into(),
        })
    }
}
#[derive(Default)]
struct Approvals(
    AtomicUsize,
    AtomicBool,
    Mutex<Vec<gofin_domain::approval_review::ApprovalReview>>,
);
#[async_trait]
impl Authorizer for Approvals {
    fn requires_financial_review(&self) -> bool {
        self.1.load(Ordering::SeqCst)
    }
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        if self.requires_financial_review() {
            self.2
                .lock()
                .unwrap()
                .push(c.review.clone().expect("financial review required"));
            #[cfg(target_os = "macos")]
            {
                let reason = native_approval::assert_review_renders(
                    c.review.clone().unwrap(),
                    &c.purpose,
                    &c.operation_id,
                    c.binding.clone(),
                    &c.summary,
                );
                assert!(reason.contains("10"), "{reason}");
                assert!(!reason.to_lowercase().contains("fee"), "{reason}");
                assert!(!reason.to_lowercase().contains("deadline"), "{reason}");
            }
        }
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "synthetic".into(),
            source: None,
            yolo_provenance: None,
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
struct HlTransport(Arc<Mutex<ChainState>>);
#[async_trait]
impl HyperliquidTransport for HlTransport {
    async fn info(&self, request: HyperliquidInfoRequest) -> Result<Value> {
        match request {
            HyperliquidInfoRequest::UserRole { .. } => Ok(json!({"role":"user"})),
            HyperliquidInfoRequest::UserAbstractionState { .. } => {
                Ok(json!({"abstraction":"disabled"}))
            }
            HyperliquidInfoRequest::UserDexAbstractionState { .. } => Ok(json!(false)),
            HyperliquidInfoRequest::UserNonFundingLedgerUpdates {
                end_time_ms: Some(time),
                ..
            } => {
                let s = self.0.lock().unwrap();
                Ok(if s.hl_credit_visible && s.hl_tx.is_some() {
                    json!([{"time":time,"hash":s.hl_tx,"delta":{"type":"deposit","usdc":gofin_evm::quantity(s.hl_amount).to_decimal(6)}}])
                } else {
                    json!([])
                })
            }
            _ => Err(GoFinError::validation(
                "Unexpected Hyperliquid fixture info request",
            )),
        }
    }
    async fn dispatch(
        &self,
        _: &HyperliquidAccount,
        _: &HyperliquidActionEnvelope,
        _: Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> Result<Value> {
        Err(GoFinError::validation(
            "Deposits must not dispatch native Hyperliquid actions",
        ))
    }
}
struct HlPrices;
#[async_trait]
impl HyperliquidPrices for HlPrices {
    async fn usdc_price(&self) -> Result<gofin_domain::AssetPrice> {
        Ok(gofin_domain::AssetPrice {
            asset: gofin_hyperliquid::usdc(),
            unit_atoms: Quantity::from_decimal("1", gofin_hyperliquid::usdc().decimals).unwrap(),
            usd_micros_per_unit: Quantity::from_u64(1_000_000),
            observed_at: now(),
            source: "synthetic fixed USD quote".into(),
        })
    }
}
struct Harness {
    temp: tempfile::TempDir,
    iris: Arc<Iris>,
    state: BridgeStateRepository,
    chain: Arc<Mutex<ChainState>>,
    approvals: Arc<Approvals>,
    wallet: String,
    clock: Arc<OffsetClock>,
    bridge: Arc<BridgeAdapter>,
    engine: Arc<Engine>,
    runtime: Arc<gofin_broker::BrokerRuntime>,
    client: tokio::sync::OnceCell<Client>,
    solana: Option<Arc<solana::SolanaFixture>>,
}

enum OffsetClockState {
    Realtime,
    Frozen(chrono::DateTime<chrono::Utc>),
}

struct OffsetClock(Mutex<OffsetClockState>);

impl OffsetClock {
    fn realtime() -> Arc<Self> {
        Arc::new(Self(Mutex::new(OffsetClockState::Realtime)))
    }

    fn freeze(&self) {
        *self.0.lock().unwrap() = OffsetClockState::Frozen(chrono::Utc::now());
    }

    fn unfreeze(&self) {
        *self.0.lock().unwrap() = OffsetClockState::Realtime;
    }

    fn advance(&self, by: std::time::Duration) {
        let mut instant = self.0.lock().unwrap();
        let OffsetClockState::Frozen(frozen) = &mut *instant else {
            panic!("only frozen fixture clocks can be advanced");
        };
        *frozen += chrono::Duration::from_std(by).unwrap();
    }
}

impl gofin_domain::clock::Clock for OffsetClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        match &*self.0.lock().unwrap() {
            OffsetClockState::Realtime => chrono::Utc::now(),
            OffsetClockState::Frozen(instant) => *instant,
        }
    }
}

impl Harness {
    fn new() -> Self {
        Self::new_with_clock(OffsetClock::realtime())
    }

    fn new_with_clock(clock: Arc<OffsetClock>) -> Self {
        Self::new_with_route(clock, 1, 42161)
    }

    fn new_with_route(clock: Arc<OffsetClock>, source_chain: u64, destination_chain: u64) -> Self {
        let chain = Arc::new(Mutex::new(ChainState {
            source_balance: U256::from(20_000_000),
            source_chain,
            destination_chain,
            ..Default::default()
        }));
        let iris = Arc::new(Iris::new(chain.clone()));
        Self::build(
            tempfile::tempdir().unwrap(),
            chain,
            Arc::new(Approvals::default()),
            iris,
            clock,
        )
    }
    fn build(
        temp: tempfile::TempDir,
        chain: Arc<Mutex<ChainState>>,
        approvals: Arc<Approvals>,
        iris: Arc<Iris>,
        clock: Arc<OffsetClock>,
    ) -> Self {
        Self::build_with_solana_clock(temp, chain, approvals, iris, None, clock)
    }
    fn build_with_solana(
        temp: tempfile::TempDir,
        chain: Arc<Mutex<ChainState>>,
        approvals: Arc<Approvals>,
        iris: Arc<Iris>,
        solana: Option<Arc<solana::SolanaFixture>>,
    ) -> Self {
        Self::build_with_solana_clock(
            temp,
            chain,
            approvals,
            iris,
            solana,
            OffsetClock::realtime(),
        )
    }
    fn build_with_solana_clock(
        temp: tempfile::TempDir,
        chain: Arc<Mutex<ChainState>>,
        approvals: Arc<Approvals>,
        iris: Arc<Iris>,
        solana: Option<Arc<solana::SolanaFixture>>,
        clock: Arc<OffsetClock>,
    ) -> Self {
        let signer_key = {
            let state = chain.lock().unwrap();
            if state.owned_forks.is_some() && !state.alternate_fork_signer {
                owned_fork::ATTESTER_KEY.to_owned()
            } else if state.owned_forks.is_none() && state.alternate_fork_signer {
                "22".repeat(32)
            } else {
                "11".repeat(32)
            }
        };
        let signer = Arc::new(LocalEnvelopeSigner::from_private_key(&signer_key).unwrap());
        let wallet = lower_address(signer.address());
        let prices = solana
            .as_ref()
            .and_then(|s| s.prices.get().cloned())
            .unwrap_or_else(|| Arc::new(Prices));
        let valuator = Arc::new(SharedUsdValuator::with_source(prices));
        let mut wallets = BTreeMap::new();
        let mut observers = BTreeMap::new();
        let source_chain = chain.lock().unwrap().source_chain();
        let destination_chain = chain.lock().unwrap().destination_chain();
        for chain_id in [1, 42161].into_iter().chain(
            (source_chain == 8453 || destination_chain == 8453 || solana.is_some()).then_some(8453),
        ) {
            let fork = chain
                .lock()
                .unwrap()
                .owned_forks
                .as_ref()
                .and_then(|forks| forks.get(&chain_id).cloned());
            let rpc: Arc<dyn EvmRpc> = if let Some(fork) = fork {
                Arc::new(OwnedForkRpc {
                    fork,
                    state: chain.clone(),
                })
            } else {
                Arc::new(Rpc {
                    chain: chain_id,
                    state: chain.clone(),
                })
            };
            let runtime = Arc::new(EvmRuntime::new(rpc.clone(), chain_id, Some(signer.clone())));
            let mut config = EthereumConfig::wallet_only(
                chain_id,
                1,
                vec![usdc_asset(chain_id).unwrap().token.unwrap()],
            );
            config.require_finalized = false;
            wallets.insert(
                chain_id,
                Arc::new(
                    EthereumAdapter::new(
                        config,
                        vec![WalletConnection {
                            name: "model-wallet".into(),
                            address: wallet.clone(),
                            signing_enabled: true,
                        }],
                        runtime,
                    )
                    .unwrap()
                    .with_wallet_valuation(valuator.clone()),
                ),
            );
            observers.insert(chain_id, Arc::new(EvmRuntime::new(rpc, chain_id, None)));
        }
        let provider = Arc::new(
            CctpProvider::new(
                observers.clone(),
                CctpConfig {
                    iris_url: iris.url.clone(),
                },
            )
            .unwrap(),
        );
        let provider: Arc<dyn gofin_bridge::BridgeProvider> =
            if let Some(fault) = chain.lock().unwrap().refresh_fault.clone() {
                Arc::new(stack_refresh::Provider {
                    inner: provider,
                    fault,
                })
            } else {
                provider
            };
        let router = Arc::new(gofin_broker::EvmNetworkRouter::new(wallets.clone()));
        let hl_transport = Arc::new(HlTransport(chain.clone()));
        let hl_profile = Arc::new(
            HyperliquidProfile::new(
                vec![HyperliquidAccount {
                    name: "model-hl".into(),
                    owner: wallet.clone(),
                    credential_ref: None,
                    signing_enabled: false,
                    execution_scope: Default::default(),
                }],
                hl_transport.clone(),
                Arc::new(HlPrices),
                Arc::new(DeferredExecutionState::default()),
            )
            .unwrap(),
        );
        let hl = Arc::new(gofin_broker::HyperliquidEthereumDepositAdapter::new(
            hl_profile,
            wallets[&42161].clone(),
            hl_transport,
        ));
        let binance = solana
            .as_ref()
            .and_then(|s| s.binance.get())
            .map(|b| b.adapter(wallets[&8453].clone()));
        let bridge = Arc::new(
            BridgeAdapter::with_providers(vec![provider], wallets, observers, valuator.clone())
                .unwrap()
                .with_solana_wallets(
                    solana
                        .as_ref()
                        .map(|fixture| BTreeMap::from([("model-solana".into(), fixture.wallet())]))
                        .unwrap_or_default(),
                )
                .with_clock(clock.clone()),
        );
        let shared = Store::open(temp.path().join("cctp.sqlite"))
            .unwrap()
            .share();
        let state = BridgeStateRepository::new(shared.clone());
        let runtime = gofin_broker::compose_runtime_with_clock(
            gofin_broker::BrokerConfigV2 {
                version: 2,
                networks: gofin_broker::NetworkConfig::reviewed_evm_networks(),
                deployments: vec![],
                connections: vec![],
                policy: gofin_broker::BrokerPolicy::beta(),
                approver_uid: unsafe { libc::geteuid() },
                extensions: BTreeMap::new(),
            },
            gofin_broker::RuntimeDependencies {
                store: Store::open(temp.path().join("cctp.sqlite")).unwrap(),
                adapters: {
                    let mut adapters: Vec<Arc<dyn Adapter>> = vec![router, bridge.clone(), hl];
                    if let Some(binance) = binance {
                        adapters.push(binance);
                    }
                    adapters
                },
                authorizer: approvals.clone(),
                valuator,
                bridge: Some(bridge.clone()),
            },
            clock.clone(),
        )
        .unwrap();
        let engine = runtime.engine.clone();
        Self {
            temp,
            iris,
            state,
            chain,
            approvals,
            wallet,
            clock,
            bridge,
            engine,
            runtime,
            client: tokio::sync::OnceCell::new(),
            solana,
        }
    }
    fn restart(self) -> Self {
        let Self {
            temp,
            iris,
            chain,
            approvals,
            solana,
            clock,
            ..
        } = self;
        Self::build_with_solana_clock(temp, chain, approvals, iris, solana, clock)
    }
    fn bounds(&self) -> BridgeBounds {
        let source_chain = self.chain.lock().unwrap().source_chain();
        let destination_chain = self.chain.lock().unwrap().destination_chain();
        let mut fees: Vec<_> = [source_chain, destination_chain]
            .into_iter()
            .map(|chain_id| FeeCap {
                asset: gofin_bridge::EvmAsset {
                    chain_id,
                    token: None,
                    decimals: 18,
                }
                .asset_id(),
                kind: FeeKind::Gas,
                maximum: Quantity::new("10000000000000000").unwrap(),
                maximum_usd_micros: Some(20_000_000),
            })
            .collect();
        fees.push(FeeCap {
            asset: usdc_asset(destination_chain).unwrap().asset_id(),
            kind: FeeKind::WithheldFromOutput,
            maximum: Quantity::from_u64(5000),
            maximum_usd_micros: None,
        });
        BridgeBounds {
            fee_caps: fees,
            ..Default::default()
        }
    }
    fn quote_request(&self, fast: bool) -> Value {
        let mut bounds = self.bounds();
        if !fast {
            bounds
                .fee_caps
                .retain(|cap| cap.kind != FeeKind::WithheldFromOutput);
        }
        let source_chain = self.chain.lock().unwrap().source_chain();
        let destination_chain = self.chain.lock().unwrap().destination_chain();
        let input = json!({"provider":"cctp","cctp":{"transferMode":if fast{"fast"}else{"standard"}},"intent":{"wallet":"model-wallet","source":{"chainId":source_chain,"token":usdc_asset(source_chain).unwrap().token},"destinationChainId":destination_chain,"destinationAssets":[{"chainId":destination_chain,"token":usdc_asset(destination_chain).unwrap().token}],"recipient":self.wallet,"refundTo":self.wallet,"allowedRefundAssets":[],"amount":"10000000"},"bounds":bounds});
        input
    }
    async fn quote(&self, fast: bool) -> (String, Vec<Value>) {
        let input = self.quote_request(fast);
        let bounds = input["bounds"].clone();
        let quoted = self.read("bridge.quotes", input).await;
        let transfer = quoted["transfers"][0]["transfer_id"]
            .as_str()
            .unwrap_or_else(|| panic!("CCTP quote failed: {quoted}"))
            .to_owned();
        let links: Vec<BridgeStepLink> =
            serde_json::from_value(quoted["transfers"][0]["steps"].clone()).unwrap();
        let inputs=links.iter().map(|s|json!({"transferId":transfer,"stepId":s.step_id,"stepKind":s.kind,"bounds":bounds})).collect();
        (transfer, inputs)
    }
    async fn call(&self, args: Vec<String>, request: BrokerRequest) -> Value {
        let client = self
            .client
            .get_or_init(|| Client::new(self.runtime.clone(), self.temp.path()))
            .await;
        client.call(args, request).await
    }
    async fn read(&self, operation: &str, input: Value) -> Value {
        self.call(
            args(&["read", operation, "--input", &input.to_string(), "--json"]),
            BrokerRequest::Read {
                operation: operation.into(),
                input,
            },
        )
        .await
    }
    async fn operation_status(&self, id: &str) -> gofin_domain::OperationRecord {
        Box::pin(self.engine.status(id, true)).await.unwrap()
    }
    async fn settle(&self, mut op: gofin_domain::OperationRecord) -> gofin_domain::OperationRecord {
        for _ in 0..3 {
            if matches!(op.status, Status::Completed | Status::Failed) {
                break;
            }
            op = self.operation_status(&op.id).await;
        }
        assert_eq!(op.status, Status::Completed, "{op:?}");
        op
    }
    async fn start_compound(&self, request_id: &str) -> (String, String) {
        Box::pin(self.start_compound_mode(request_id, false)).await
    }
    async fn start_compound_mode(&self, request_id: &str, fast: bool) -> (String, String) {
        Box::pin(self.start_compound_options(request_id, fast, false)).await
    }
    async fn start_compound_options(
        &self,
        request_id: &str,
        fast: bool,
        reverse_caps: bool,
    ) -> (String, String) {
        let source = self.chain.lock().unwrap().source_chain();
        let destination = self.chain.lock().unwrap().destination_chain();
        let mut bounds = self.bounds();
        bounds.input = Some(gofin_bridge::AmountInterval {
            minimum: Quantity::from_u64(10_000_000),
            maximum: Quantity::from_u64(10_000_000),
        });
        bounds.maximum_source_debit = Some(Quantity::from_u64(10_000_000));
        bounds.minimum_net_receipt = Some(usdc_asset(destination).unwrap().amount(
            Quantity::from_u64(if fast { 9_995_000 } else { 10_000_000 }),
        ));
        if !fast {
            bounds.fee_caps.retain(|cap| cap.kind == FeeKind::Gas);
        }
        for cap in &mut bounds.fee_caps {
            cap.maximum_usd_micros = None;
        }
        if reverse_caps {
            bounds.fee_caps.swap(0, 1);
        }
        let mut input = json!({"intent":{"wallet":"model-wallet","source":{"chainId":source,"token":usdc_asset(source).unwrap().token},"destinationChainId":destination,"destinationAssets":[{"chainId":destination,"token":usdc_asset(destination).unwrap().token}],"recipient":self.wallet,"refundTo":self.wallet,"allowedRefundAssets":[],"amount":{"minimum":"10000000","maximum":"10000000"}},"allowedProviders":["cctp"],"actionDeadline":chrono::Utc::now().timestamp() as u64+600,"bounds":bounds});
        if fast {
            input["cctp"] = json!({"transferMode":"fast"});
        }
        let request = OperationRequest {
            request_id: request_id.into(),
            operation: "bridge.execute".into(),
            input: input.clone(),
            run_id: None,
        };
        let result = self
            .call(
                args(&[
                    "execute",
                    "bridge.execute",
                    "--input",
                    &input.to_string(),
                    "--id",
                    request_id,
                    "--json",
                ]),
                BrokerRequest::Execute { request },
            )
            .await;
        let allow_fee_limit = {
            let state = self.chain.lock().unwrap();
            state.owned_forks.is_some() && state.source_large_fee_after_approval
        };
        assert!(
            result["error"].is_null()
                || (allow_fee_limit && result["error"]["code"] == "FEE_LIMIT"),
            "{result}"
        );
        (
            result["id"].as_str().unwrap().into(),
            result["preview"]["binding"]["transfer_id"]
                .as_str()
                .unwrap()
                .into(),
        )
    }
    async fn continuation_error(&self, id: &str, token: &str) -> gofin_domain::GoFinError {
        let engine = self.engine.clone();
        let id = id.to_owned();
        let token = token.to_owned();
        tokio::spawn(Box::pin(async move {
            let invocation = gofin_domain::execution_progress::Invocation::new(
                std::time::Duration::from_secs(30),
            );
            invocation
                .scope(Box::pin(engine.continue_operation(&id, &token)))
                .await
                .unwrap_err()
        }))
        .await
        .unwrap()
    }
    async fn reauthorization_error(
        &self,
        id: &str,
        token: &str,
        input: Value,
    ) -> gofin_domain::GoFinError {
        let engine = self.engine.clone();
        let id = id.to_owned();
        let token = token.to_owned();
        tokio::spawn(Box::pin(async move {
            let invocation = gofin_domain::execution_progress::Invocation::new(
                std::time::Duration::from_secs(30),
            );
            invocation
                .scope(Box::pin(engine.reauthorize_operation(&id, &token, input)))
                .await
                .unwrap_err()
        }))
        .await
        .unwrap()
    }
    async fn continue_compound(&self, id: &str) -> Value {
        let status = self
            .call(
                args(&["status", id, "--json"]),
                BrokerRequest::Status { id: id.into() },
            )
            .await;
        let token = status
            .pointer("/outcome/next_action/continuation_token")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("missing continuation: {status}"));
        self.call(
            args(&["operations", "continue", id, "--token", token, "--json"]),
            BrokerRequest::ContinueOperation {
                id: id.into(),
                token: token.into(),
            },
        )
        .await
    }
    async fn status(&self, id: &str) -> Value {
        self.read("bridge.status", json!({"transferId":id})).await
    }
}

#[path = "cctp_fork.rs"]
mod owned_fork;
#[path = "support/cctp_stack_refresh.rs"]
mod stack_refresh;
#[tokio::test]
async fn compound_cctp_native_review_covers_all_stages_once() {
    Box::pin(async {
        let h = Harness::new();
        h.approvals.1.store(true, Ordering::SeqCst);
        let (id, transfer) = h.start_compound("native-review").await;
        assert_eq!(h.status(&transfer).await["state"], "in_transit");
        assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
        {
            let reviews = h.approvals.2.lock().unwrap();
            let review = &reviews[0];
            assert_eq!(review.service, "cctp");
            assert_eq!(review.input.as_ref().unwrap().amount.as_deref(), Some("10"));
            assert_eq!(
                review.output.as_ref().unwrap().amount.as_deref(),
                Some("10")
            );
            assert_eq!(
                review
                    .input
                    .as_ref()
                    .unwrap()
                    .account
                    .as_ref()
                    .unwrap()
                    .name,
                "model-wallet"
            );
            assert_eq!(
                review
                    .output
                    .as_ref()
                    .unwrap()
                    .account
                    .as_ref()
                    .unwrap()
                    .name,
                h.wallet.to_lowercase()
            );
        }
        h.chain.lock().unwrap().attestation_ready = true;
        let waiting = h
            .call(
                args(&["status", &id, "--json"]),
                BrokerRequest::Status { id: id.clone() },
            )
            .await;
        let token = waiting
            .pointer("/outcome/next_action/continuation_token")
            .and_then(Value::as_str)
            .unwrap()
            .to_owned();
        let completed = h
            .call(
                args(&["operations", "continue", &id, "--token", &token, "--json"]),
                BrokerRequest::ContinueOperation {
                    id: id.clone(),
                    token: token.clone(),
                },
            )
            .await;
        assert_eq!(completed["outcome"]["phase"], "done", "{completed}");
        let replay = h
            .call(
                args(&["operations", "continue", &id, "--token", &token, "--json"]),
                BrokerRequest::ContinueOperation {
                    id: id.clone(),
                    token,
                },
            )
            .await;
        assert_eq!(replay["id"], id, "{replay}");
        assert_eq!(h.status(&transfer).await["state"], "completed");
        let observed = h
            .call(
                args(&["status", &id, "--json"]),
                BrokerRequest::Status { id: id.clone() },
            )
            .await;
        assert_eq!(observed["outcome"]["phase"], "done", "{observed}");
        let input = observed["input"].clone();
        let repeated = h
            .call(
                args(&[
                    "execute",
                    "bridge.execute",
                    "--input",
                    &input.to_string(),
                    "--id",
                    "native-review",
                    "--json",
                ]),
                BrokerRequest::Execute {
                    request: OperationRequest {
                        request_id: "native-review".into(),
                        operation: "bridge.execute".into(),
                        input,
                        run_id: None,
                    },
                },
            )
            .await;
        assert_eq!(repeated["id"], id, "{repeated}");
        assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            h.chain.lock().unwrap().broadcasts,
            BTreeMap::from([(1, 2), (42161, 1)])
        );
    })
    .await
}

#[tokio::test]
async fn known_canonical_mint_settles_without_destination_log_scan() {
    let h = Harness::new();
    let (id, transfer) = h.start_compound("canonical-mint").await;
    assert_eq!(h.status(&transfer).await["state"], "in_transit");
    h.chain.lock().unwrap().attestation_ready = true;
    let completed = h.continue_compound(&id).await;
    assert!(completed["error"].is_null(), "{completed}");
    assert_eq!(completed["outcome"]["phase"], "done", "{completed}");
    assert_eq!(completed["outcome"]["transfer"], "available", "{completed}");
    assert!(
        completed
            .pointer("/outcome/next_action/continuation_token")
            .is_none(),
        "{completed}"
    );
    assert_eq!(
        completed["outcome"]["attention"]["kind"], "none",
        "{completed}"
    );
    assert_eq!(h.status(&transfer).await["state"], "completed");
    let scans = {
        let mut state = h.chain.lock().unwrap();
        state.reject_destination_log_scan = true;
        state.destination_log_scans
    };
    let h = h.restart();
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert_eq!(h.chain.lock().unwrap().destination_log_scans, scans);
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(1, 2), (42161, 1)])
    );
}

#[tokio::test]
async fn complete_cctp_compound_requires_explicit_mint_and_credits_only_net() {
    Box::pin(async {
        for fast in [false, true] {
            let h = Harness::new();
            let (id, transfer) = h
                .start_compound_mode(if fast { "fast" } else { "standard" }, fast)
                .await;
            assert_eq!(
                h.state.transfer(&transfer).unwrap().unwrap().state,
                gofin_domain::BridgeTransferState::InTransit
            );
            assert_eq!(h.chain.lock().unwrap().destination_balance, U256::ZERO);
            assert_eq!(
                h.status(&transfer).await["provider_status"]["status"],
                "attestation_pending"
            );
            if fast {
                let status = h
                    .call(
                        args(&["status", &id, "--json"]),
                        BrokerRequest::Status { id: id.clone() },
                    )
                    .await;
                let token = status
                    .pointer("/outcome/next_action/continuation_token")
                    .and_then(Value::as_str)
                    .unwrap();
                let mut changed = status["input"].clone();
                changed["cctp"]["transferMode"] = json!("standard");
                let _ = h.reauthorization_error(&id, token, changed).await;
                assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
            }
            h.chain.lock().unwrap().attestation_ready = true;
            assert_eq!(
                h.status(&transfer).await["provider_status"]["status"],
                "mint_ready"
            );
            let result = h.continue_compound(&id).await;
            assert!(result["error"].is_null(), "{result}");
            assert_eq!(h.status(&transfer).await["state"], "completed");
            let saved = h.state.transfer(&transfer).unwrap().unwrap();
            assert_eq!(h.status(&transfer).await["state"], "completed");
            assert_eq!(saved, h.state.transfer(&transfer).unwrap().unwrap());
            assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
            assert_eq!(
                h.chain.lock().unwrap().source_balance,
                U256::from(10_000_000)
            );
            assert_eq!(
                h.chain.lock().unwrap().destination_balance,
                U256::from(if fast { 9_995_000 } else { 10_000_000 })
            );
        }
    })
    .await
}

#[tokio::test]
async fn response_loss_each_submission_boundary_restarts_without_duplicate_burn_or_mint() {
    Box::pin(async {
        for lost in 1..=3 {
            let mut h = Harness::new();
            if lost < 3 {
                h.chain.lock().unwrap().lose_source_ordinal = Some(lost);
            }
            let (id, transfer) = h.start_compound(&format!("lost-{lost}")).await;
            h = h.restart();
            for _ in 0..3 {
                let _ = h
                    .call(
                        args(&["status", &id, "--json"]),
                        BrokerRequest::Status { id: id.clone() },
                    )
                    .await;
                if h.state
                    .transfer(&transfer)
                    .unwrap()
                    .unwrap()
                    .transit
                    .is_some()
                {
                    break;
                }
                let _ = h.continue_compound(&id).await;
            }
            assert!(
                h.state
                    .transfer(&transfer)
                    .unwrap()
                    .unwrap()
                    .transit
                    .is_some()
            );
            h.chain.lock().unwrap().attestation_ready = true;
            if lost == 3 {
                h.chain.lock().unwrap().lose_response = Some(42161);
            }
            let _ = h.continue_compound(&id).await;
            h = h.restart();
            for _ in 0..3 {
                let _ = h
                    .call(
                        args(&["status", &id, "--json"]),
                        BrokerRequest::Status { id: id.clone() },
                    )
                    .await;
                if h.status(&transfer).await["state"] == "completed" {
                    break;
                }
            }
            assert_eq!(h.status(&transfer).await["state"], "completed");
            assert_eq!(
                h.chain.lock().unwrap().broadcasts,
                BTreeMap::from([(1, 2), (42161, 1)])
            );
            assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
        }
    })
    .await
}
#[tokio::test]
async fn delayed_evidence_and_gas_shortage_preserve_source_then_resume_same_mint() {
    let h = Harness::new();
    let (id, transfer) = h.start_compound("delayed-mint").await;
    let source = h
        .state
        .transfer(&transfer)
        .unwrap()
        .unwrap()
        .transit
        .unwrap();
    {
        let mut s = h.chain.lock().unwrap();
        s.attestation_ready = true;
        s.gas_missing = true;
    }
    assert_eq!(
        h.status(&transfer).await["provider_status"]["status"],
        "destination_gas_required"
    );
    assert_eq!(h.chain.lock().unwrap().broadcasts, BTreeMap::from([(1, 2)]));
    {
        let mut s = h.chain.lock().unwrap();
        s.gas_missing = false;
        s.hide_destination_receipt = true;
    }
    let _ = h.continue_compound(&id).await;
    assert_eq!(h.chain.lock().unwrap().broadcasts.get(&42161), Some(&1));
    let h = h.restart();
    assert_ne!(h.status(&transfer).await["state"], "completed");
    assert_eq!(
        h.state.transfer(&transfer).unwrap().unwrap().transit,
        Some(source)
    );
    h.chain.lock().unwrap().hide_destination_receipt = false;
    let _ = h
        .call(
            args(&["status", &id, "--json"]),
            BrokerRequest::Status { id },
        )
        .await;
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(1, 2), (42161, 1)])
    );
}
#[tokio::test]
async fn invalid_attestation_cannot_dispatch_mint() {
    Box::pin(async {
        let h = Harness::new();
        let (id, transfer) = h.start_compound("invalid-attestation").await;
        {
            let mut s = h.chain.lock().unwrap();
            s.attestation_ready = true;
            s.mutate_attestation = true;
        }
        let status = h
            .call(
                args(&["status", &id, "--json"]),
                BrokerRequest::Status { id: id.clone() },
            )
            .await;
        let token = status
            .pointer("/outcome/next_action/continuation_token")
            .and_then(Value::as_str)
            .unwrap();
        let err = h.continuation_error(&id, token).await;
        assert_eq!(err.code, "CCTP_VALIDATION_FAILED");
        assert_eq!(h.chain.lock().unwrap().broadcasts, BTreeMap::from([(1, 2)]));
        assert!(
            h.state
                .transfer(&transfer)
                .unwrap()
                .unwrap()
                .transit
                .is_some()
        );
        assert_ne!(
            h.state.transfer(&transfer).unwrap().unwrap().state,
            gofin_domain::BridgeTransferState::Completed
        );
    })
    .await
}
#[tokio::test]
async fn processed_message_without_native_usdc_credit_never_completes() {
    let h = Harness::new();
    let (id, transfer) = h.start_compound("missing-credit").await;
    {
        let mut state = h.chain.lock().unwrap();
        state.attestation_ready = true;
        state.omit_credit = true;
    }
    let _result = h.continue_compound(&id).await;
    let status = h.status(&transfer).await;
    assert_ne!(status["state"], "completed");
    assert_eq!(
        status["provider_status"]["status"],
        "processed_receipt_pending"
    );
    assert!(
        h.state
            .transfer(&transfer)
            .unwrap()
            .unwrap()
            .transit
            .is_some()
    );
    assert_eq!(h.chain.lock().unwrap().destination_balance, U256::ZERO);
    assert_eq!(h.chain.lock().unwrap().broadcasts.get(&42161), Some(&1));
    let _ = h
        .call(
            args(&["status", &id, "--json"]),
            BrokerRequest::Status { id },
        )
        .await;
    assert_eq!(h.chain.lock().unwrap().broadcasts.get(&42161), Some(&1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_cctp_mint_receipt_survives_restart_and_funds_correlated_hyperliquid_credit() {
    Box::pin(async {
        let h=Harness::new();let (id,transfer)=h.start_compound("cctp-hl").await;
        h.chain.lock().unwrap().attestation_ready=true;let _=h.continue_compound(&id).await;
        assert_eq!(h.status(&transfer).await["state"],"completed");
        let saved=h.state.transfer(&transfer).unwrap().unwrap();assert_eq!(saved.settlement_proofs.len(),1);
        let proof=saved.settlement_proofs[0].clone();
        let h=h.restart();assert_eq!(h.status(&transfer).await["state"],"completed");
        assert_eq!(h.state.transfer(&transfer).unwrap().unwrap().settlement_proofs[0],proof);
        assert_eq!(h.chain.lock().unwrap().destination_balance,U256::from(10_000_000));
        let input=json!({"requestId":"hl-funding","account":"model-hl","wallet":"model-wallet","amount":"10","maxFeeEth":"0.001","maxAmountUsdc":"10"});
        let request=OperationRequest{request_id:"hl-funding".into(),operation:"hyperliquid.account.deposit".into(),input:input.clone(),run_id:None};
        let deposited=h.call(args(&["execute","hyperliquid.account.deposit","--input",&input.to_string(),"--id","hl-funding","--json"]),BrokerRequest::Execute{request}).await;
        let operation_id=deposited["id"].as_str().unwrap();let op=h.engine.operations().unwrap().into_iter().find(|op|op.id==operation_id).unwrap();
        h.chain.lock().unwrap().hl_credit_visible=true;let settled=h.settle(op).await;
        assert_eq!(settled.status,Status::Completed);assert_eq!(h.approvals.0.load(Ordering::SeqCst),2);
        assert_eq!(h.chain.lock().unwrap().destination_balance,U256::ZERO);
        assert_eq!(h.chain.lock().unwrap().broadcasts,BTreeMap::from([(1,2),(42161,2)]));
    }).await
}

#[tokio::test]
async fn sufficient_allowance_skips_approval_and_keeps_two_explicit_stages() {
    let h = Harness::new();
    h.chain.lock().unwrap().allowance = U256::from(10_000_000);
    let (id, transfer) = h.start_compound("existing-allowance").await;
    let saved = h.state.transfer(&transfer).unwrap().unwrap();
    assert_eq!(
        saved
            .steps
            .iter()
            .filter(|step| step.operation_id.is_some())
            .count(),
        1
    );
    assert_eq!(h.chain.lock().unwrap().broadcasts.get(&1), Some(&1));
    h.chain.lock().unwrap().attestation_ready = true;
    let result = h.continue_compound(&id).await;
    assert!(result["error"].is_null(), "{result}");
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(1, 1), (42161, 1)])
    );
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reverted_mint_preserves_burn_and_charged_gas_then_recovers_with_new_identity() {
    let mut h = Harness::new();
    let (transfer, inputs) = h.quote(false).await;
    let mut originals = Vec::new();
    for (i, input) in inputs.iter().enumerate() {
        if i == 2 {
            let mut state = h.chain.lock().unwrap();
            state.attestation_ready = true;
            state.revert_next = Some(42161);
        }
        let mut operation = h
            .engine
            .execute(OperationRequest {
                request_id: format!("exact-stage-{i}"),
                operation: "bridge.transfer".into(),
                input: input.clone(),
                run_id: None,
            })
            .await
            .unwrap();
        if i < 2 {
            operation = h.settle(operation).await;
        } else {
            operation = h.engine.status(&operation.id, true).await.unwrap();
        }
        originals.push(operation);
    }
    let failed = &originals[2];
    assert_eq!(failed.status, Status::Failed);
    assert!(failed.effects.as_ref().unwrap().effects.iter().any(|effect|matches!(effect,gofin_domain::FinancialEffect::Cost{amount,..} if amount.atoms == Quantity::from_u64(30_000_000_000_000))));
    let prior = h.state.transfer(&transfer).unwrap().unwrap();
    assert!(prior.transit.is_some());
    assert_eq!(
        prior.steps[2].state,
        gofin_domain::BridgeStepState::NeedsAttention
    );
    h = h.restart();
    let recovery = h
        .bridge
        .read("bridge.recovery", &json!({"transferId":transfer}))
        .await
        .unwrap();
    assert_eq!(recovery["created"], true);
    let repeat = h
        .bridge
        .read("bridge.recovery", &json!({"transferId":transfer}))
        .await
        .unwrap();
    assert_eq!(repeat["created"], false);
    assert_eq!(repeat["steps"], recovery["steps"]);
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 3);
    assert_eq!(
        h.state.transfer(&transfer).unwrap().unwrap().transit,
        prior.transit
    );
    let mut input = inputs[2].clone();
    input["stepId"] = recovery["steps"][0]["step_id"].clone();
    let mint = h
        .settle(
            h.engine
                .execute(OperationRequest {
                    request_id: "exact-recovery-mint".into(),
                    operation: "bridge.transfer".into(),
                    input,
                    run_id: None,
                })
                .await
                .unwrap(),
        )
        .await;
    assert_ne!(mint.id, failed.id);
    assert_ne!(mint.submission, failed.submission);
    assert_eq!(h.status(&transfer).await["state"], "completed");
    let record = h.state.transfer(&transfer).unwrap().unwrap();
    assert_eq!(&record.steps[..3], &prior.steps);
    assert_eq!(record.steps.len(), 4);
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(1, 2), (42161, 2)])
    );
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 4);
}

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| (*v).into()).collect()
}
/// The development wire wrapper transports actual CLI requests into BrokerRuntime.
/// Production identity enforcement stays in the protected listener; this socket
/// exists only inside the isolated synthetic fixture.
struct Client {
    runtime: Arc<gofin_broker::BrokerRuntime>,
    socket: std::path::PathBuf,
    server: tokio::task::JoinHandle<()>,
}
impl Client {
    async fn new(runtime: Arc<gofin_broker::BrokerRuntime>, dir: &std::path::Path) -> Self {
        use tokio::{
            io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
            net::UnixListener,
        };
        let socket = dir.join(format!("{}.sock", uuid::Uuid::new_v4().simple()));
        let listener = UnixListener::bind(&socket).unwrap();
        runtime.engine.recover_interrupted().unwrap();
        let r = runtime.clone();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let runtime = r.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    let hello: Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(hello["identity"], "synthetic-cctp");
                    let reply = json!({"kind":"hello","identity":hello["identity"],"build_id":hello["build_id"],"mode":"synthetic"});
                    let _ = reader
                        .get_mut()
                        .write_all(format!("{reply}\n").as_bytes())
                        .await;
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    let envelope: Value = serde_json::from_str(&line).unwrap();
                    let request: BrokerRequest =
                        serde_json::from_value(envelope["request"].clone()).unwrap();
                    let financial = envelope["cancel_on_disconnect"].as_bool().unwrap_or(false)
                        && matches!(
                            request,
                            BrokerRequest::Execute { .. }
                                | BrokerRequest::ContinueOperation { .. }
                                | BrokerRequest::ReauthorizeOperation { .. }
                        );
                    let response = if financial {
                        gofin_protocol::progress::observe_financial_request(
                            reader.get_mut(),
                            envelope["progress"].as_bool().unwrap_or(false),
                            false,
                            Box::pin(runtime.handle(request)),
                        )
                        .await
                    } else {
                        gofin_protocol::progress::observe(
                            reader.get_mut(),
                            envelope["progress"].as_bool().unwrap_or(false),
                            Box::pin(runtime.handle(request)),
                        )
                        .await
                    };
                    let reply = json!({"identity":hello["identity"],"build_id":hello["build_id"],"mode":"synthetic","response":response});
                    reader
                        .get_mut()
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .unwrap();
                });
            }
        });
        Self {
            runtime,
            socket,
            server,
        }
    }
    async fn call(&self, args: Vec<String>, request: BrokerRequest) -> Value {
        if let Ok(binary) = std::env::var("GOFIN_CCTP_CLI") {
            let socket = self.socket.clone();
            let build = std::env::var("GOFIN_CCTP_BUILD_ID")
                .unwrap_or_else(|_| "synthetic-cctp-build".into());
            let output = tokio::task::spawn_blocking(move || {
                std::process::Command::new(binary)
                    .args(["--identity", "synthetic-cctp", "--build-id", &build])
                    .args(args)
                    .env("GOFIN_DEV_IDENTITY", "synthetic-cctp")
                    .env("GOFIN_DEV_BUILD_ID", build)
                    .env("GOFIN_DEV_SOCKET", socket)
                    .output()
                    .unwrap()
            })
            .await
            .unwrap();
            assert!(
                output.stderr.is_empty(),
                "JSON stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
                panic!("CLI stdout: {}", String::from_utf8_lossy(&output.stdout))
            });
            assert!(output.status.success(), "CLI failed: {value}");
            value
        } else {
            let runtime = self.runtime.clone();
            let response = tokio::spawn(Box::pin(async move {
                let invocation = gofin_domain::execution_progress::Invocation::new(
                    std::time::Duration::from_secs(30),
                );
                invocation.scope(Box::pin(runtime.handle(request))).await
            }))
            .await
            .unwrap();
            assert!(
                response.ok,
                "broker failed for {args:?}: {:?}",
                response.error
            );
            response.result.unwrap()
        }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_file(&self.socket);
    }
}

#[tokio::test]
async fn expired_attestation_refresh_keeps_original_burn_and_unsubmitted_mint() {
    Box::pin(async { let h = Harness::new();
    let (id, transfer) = h.start_compound_mode("expired-attestation",true).await;
    let parent=h.engine.operations().unwrap().into_iter().find(|op|op.id==id).unwrap();
    let before = h.state.transfer(&transfer).unwrap().unwrap();
    let inputs=before.steps.iter().map(|step|json!({"transferId":transfer,"stepId":step.step_id,"stepKind":step.kind,"bounds":parent.input["bounds"]})).collect::<Vec<_>>();
    {
        let mut state = h.chain.lock().unwrap();
        state.attestation_ready = true;
        state.attestation_expiration = Some(120);
    }
    let frozen = h
        .bridge
        .prepare("bridge.transfer", &inputs[2])
        .await
        .unwrap();
    h.chain.lock().unwrap().source_head = Some(130);
    assert!(h.bridge.revalidate(&frozen).await.is_err());
    assert_eq!(
        h.status(&transfer).await["provider_status"]["status"],
        "attestation_expired"
    );
    assert!(
        h.bridge
            .prepare("bridge.transfer", &inputs[2])
            .await
            .is_err()
    );
    assert_eq!(h.chain.lock().unwrap().reattest_requests, 0);
    let recovery = h
        .read("bridge.recovery", json!({"transferId":transfer}))
        .await;
    assert_eq!(recovery["created"], false);
    assert_eq!(recovery["attestation_refresh_requested"], true);
    assert_eq!(recovery["steps"][0]["step_id"], inputs[2]["stepId"]);
    assert_eq!(
        h.state.transfer(&transfer).unwrap().unwrap().transit,
        before.transit
    );
    assert_eq!(h.chain.lock().unwrap().reattest_requests, 1);
    let result=h.continue_compound(&id).await;assert!(result["error"].is_null(),"{result}");
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(1, 2), (42161, 1)])
    );
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
    }).await
}

#[tokio::test]
async fn expired_action_reauthorizes_remaining_mint_without_replaying_source() {
    Box::pin(async {
        let h = Harness::new();
        let (id, transfer) = h.start_compound("expired-action").await;
        let before = h.state.transfer(&transfer).unwrap().unwrap();
        h.clock.freeze();
        h.clock.advance(std::time::Duration::from_secs(601));
        let h = h.restart();
        h.chain.lock().unwrap().attestation_ready = true;
        let status = h
            .call(
                args(&["status", &id, "--json"]),
                BrokerRequest::Status { id: id.clone() },
            )
            .await;
        assert_eq!(
            status["outcome"]["next_action"]["code"], "reauthorize",
            "{status}"
        );
        assert_eq!(h.chain.lock().unwrap().broadcasts, BTreeMap::from([(1, 2)]));
        h.clock.unfreeze();
        let token = status
            .pointer("/outcome/next_action/continuation_token")
            .and_then(Value::as_str)
            .unwrap();
        let mut input = status["input"].clone();
        input["actionDeadline"] = json!(chrono::Utc::now().timestamp() as u64 + 900);
        let engine = h.engine.clone();
        let oid = id.clone();
        let token = token.to_owned();
        let result = tokio::spawn(Box::pin(async move {
            let invocation = gofin_domain::execution_progress::Invocation::new(
                std::time::Duration::from_secs(30),
            );
            invocation
                .scope(Box::pin(engine.reauthorize_operation(&oid, &token, input)))
                .await
        }))
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result["reauthorization"], "approved", "{result}");
        assert_eq!(h.status(&transfer).await["state"], "completed");
        assert_eq!(h.approvals.0.load(Ordering::SeqCst), 2);
        let after = h.state.transfer(&transfer).unwrap().unwrap();
        assert_eq!(after.steps[0].operation_id, before.steps[0].operation_id);
        assert_eq!(after.steps[1].operation_id, before.steps[1].operation_id);
        assert_eq!(
            h.chain.lock().unwrap().broadcasts,
            BTreeMap::from([(1, 2), (42161, 1)])
        );
    })
    .await
}

async fn external_mint(h: &Harness) {
    use gofin_evm::EnvelopeSigner;
    let (message, attestation) = h.chain.lock().unwrap().attested().unwrap();
    let call = mint_call(42161, &message, &attestation).unwrap();
    let tx = gofin_evm::FrozenTransaction {
        chain_id: 42161,
        sender: h.wallet.clone(),
        to: call.to,
        value: call.value,
        calldata: call.data,
        nonce: 0,
        gas: 150000,
        max_fee_per_gas: Quantity::from_u64(3_000_000_000),
        max_priority_fee_per_gas: Quantity::from_u64(1_000_000_000),
        rollup_fee: None,
    };
    let signer = LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap();
    let signed = signer.sign_prepared(&tx).await.unwrap();
    let rpc = Rpc {
        chain: 42161,
        state: h.chain.clone(),
    };
    rpc.request("eth_sendRawTransaction", json!([hex_bytes(&signed)]))
        .await
        .unwrap();
}

#[tokio::test]
async fn external_mint_requires_source_linked_iris_nonce_before_settlement() {
    let h = Harness::new();
    let (id, transfer) = h.start_compound("external-mint").await;
    let prior = h.state.transfer(&transfer).unwrap().unwrap();
    assert_eq!(prior.steps[2].operation_id, None);
    external_mint(&h).await;
    // A canonical mint alone cannot link a zero-nonce source message to this source transaction.
    assert!(!h.chain.lock().unwrap().attestation_ready);
    let h = h.restart();
    assert_ne!(h.status(&transfer).await["state"], "completed");
    assert!(
        h.state
            .transfer(&transfer)
            .unwrap()
            .unwrap()
            .transit
            .is_some()
    );
    assert_eq!(h.chain.lock().unwrap().destination_log_scans, 0);
    h.chain.lock().unwrap().attestation_ready = true;
    assert_eq!(h.status(&transfer).await["state"], "completed");
    let record = h.state.transfer(&transfer).unwrap().unwrap();
    assert_eq!(record.steps[2].operation_id, None);
    assert!(record.transit.is_none());
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(1, 2), (42161, 1)])
    );
    let _ = h
        .call(
            args(&["status", &id, "--json"]),
            BrokerRequest::Status { id },
        )
        .await;
    assert_eq!(h.chain.lock().unwrap().broadcasts.get(&42161), Some(&1));
}

#[tokio::test]
async fn source_linked_nonce_allows_historical_mint_with_older_fee_and_expiry() {
    let h = Harness::new();
    let (_id, transfer) = h.start_compound_mode("historical-fee", true).await;
    {
        let mut state = h.chain.lock().unwrap();
        state.attestation_fee = Some(4000);
        state.attestation_expiration = Some(120);
    }
    external_mint(&h).await;
    {
        let mut state = h.chain.lock().unwrap();
        state.attestation_ready = true;
        state.attestation_fee = Some(5000);
        state.attestation_expiration = Some(200);
    }
    let h = h.restart();
    assert_eq!(h.status(&transfer).await["state"], "completed");
    let record = h.state.transfer(&transfer).unwrap().unwrap();
    assert_eq!(record.settlement_proofs.len(), 1);
    let evidence = &record.settlement_proofs[0].evidence;
    assert_eq!(evidence["net_amount_atoms"], "9996000");
    assert_eq!(evidence["fee_executed"], "4000");
    assert_eq!(record.steps[2].operation_id, None);
    assert_eq!(
        h.chain.lock().unwrap().destination_balance,
        U256::from(9_996_000)
    );
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn identical_burns_cannot_reuse_an_older_mint_with_a_different_nonce() {
    let h = Harness::new();
    let (_first_id, first_transfer) = h.start_compound("first-identical").await;
    let (first_source, first_hash) = {
        let state = h.chain.lock().unwrap();
        (
            state.source_tx.clone().unwrap(),
            immutable_message_hash(&hex_bytes(state.source_message.as_ref().unwrap())).unwrap(),
        )
    };
    external_mint(&h).await;
    // Leave the old mint uncredited in GoFin: deduplication must not be the only protection.
    assert!(
        h.state
            .transfer(&first_transfer)
            .unwrap()
            .unwrap()
            .settlement_proofs
            .is_empty()
    );
    h.chain.lock().unwrap().allowance = U256::ZERO;
    let (_second_id, transfer) = h.start_compound("second-identical").await;
    {
        let mut state = h.chain.lock().unwrap();
        assert_ne!(state.source_tx.as_ref().unwrap(), &first_source);
        assert_eq!(
            immutable_message_hash(&hex_bytes(state.source_message.as_ref().unwrap())).unwrap(),
            first_hash
        );
        state.attestation_nonce = Some(0x88);
    }
    let h = h.restart();
    assert_ne!(h.status(&transfer).await["state"], "completed");
    h.chain.lock().unwrap().attestation_ready = true;
    let status = h.status(&transfer).await;
    assert_ne!(status["state"], "completed");
    assert_eq!(status["provider_status"]["status"], "mint_ready");
    let record = h.state.transfer(&transfer).unwrap().unwrap();
    assert!(record.transit.is_some());
    assert!(record.settlement_proofs.is_empty());
    assert_eq!(record.steps[2].operation_id, None);
    assert_eq!(
        h.chain.lock().unwrap().destination_balance,
        U256::from(10_000_000)
    );
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(1, 4), (42161, 1)])
    );
}

#[tokio::test]
async fn source_gas_balance_may_fall_by_proven_approval_cost_without_reapproval() {
    let h = Harness::new();
    h.chain.lock().unwrap().source_gas_balance = Some(U256::from(10_000_000_000_000_000u64));
    let (id, transfer) = h.start_compound("source-gas").await;
    assert_eq!(
        h.chain.lock().unwrap().source_gas_balance,
        Some(U256::from(9_940_000_000_000_000u64))
    );
    assert_eq!(h.chain.lock().unwrap().broadcasts.get(&1), Some(&2));
    h.chain.lock().unwrap().attestation_ready = true;
    let _ = h.continue_compound(&id).await;
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn expired_authority_still_reconciles_already_submitted_mint() {
    let h = Harness::new();
    let (id, transfer) = h.start_compound("expired-pending").await;
    {
        let mut state = h.chain.lock().unwrap();
        state.attestation_ready = true;
        state.hide_destination_receipt = true;
    }
    let _ = h.continue_compound(&id).await;
    let before = h.state.transfer(&transfer).unwrap().unwrap();
    let mint = before.steps[2].operation_id.clone().unwrap();
    h.clock.freeze();
    h.clock.advance(std::time::Duration::from_secs(601));
    let h = h.restart();
    assert_ne!(h.status(&transfer).await["state"], "completed");
    h.chain.lock().unwrap().hide_destination_receipt = false;
    let _ = h
        .call(
            args(&["status", &id, "--json"]),
            BrokerRequest::Status { id },
        )
        .await;
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert_eq!(
        h.state.transfer(&transfer).unwrap().unwrap().steps[2]
            .operation_id
            .as_ref(),
        Some(&mint)
    );
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(1, 2), (42161, 1)])
    );
}

#[tokio::test]
#[ignore = "Real provider 90-second quote expiry; run explicitly for stale-source lifecycle evidence"]
async fn stale_quote_after_approval_refreshes_same_transfer_under_existing_plan() {
    let h = Harness::new();
    h.chain.lock().unwrap().lose_source_ordinal = Some(1);
    let (id, transfer) = h.start_compound("stale-quote").await;
    let _ = h
        .call(
            args(&["status", &id, "--json"]),
            BrokerRequest::Status { id: id.clone() },
        )
        .await;
    let before = h.state.transfer(&transfer).unwrap().unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(91)).await;
    let _ = h.continue_compound(&id).await;
    let refreshed = h.state.transfer(&transfer).unwrap().unwrap();
    assert_eq!(
        refreshed.steps[0].operation_id,
        before.steps[0].operation_id
    );
    assert!(refreshed.quote_revisions.len() > before.quote_revisions.len());
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
    h.chain.lock().unwrap().attestation_ready = true;
    let _ = h.continue_compound(&id).await;
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(1, 2), (42161, 1)])
    );
}

#[tokio::test]
async fn half_life_quote_keeps_allowance_step_and_refreshes_before_deposit() {
    let h = Harness::new();
    h.chain.lock().unwrap().lose_source_ordinal = Some(1);
    let (id, transfer) = h.start_compound("half-life").await;
    let _ = h
        .call(
            args(&["status", &id, "--json"]),
            BrokerRequest::Status { id: id.clone() },
        )
        .await;
    let approved = h.state.transfer(&transfer).unwrap().unwrap();
    assert_eq!(
        approved.steps[0].state,
        gofin_domain::BridgeStepState::Settled
    );
    h.clock.freeze();
    h.clock.advance(std::time::Duration::from_secs(46));
    let _ = h.continue_compound(&id).await;
    let refreshed = h.state.transfer(&transfer).unwrap().unwrap();
    assert_eq!(
        refreshed.steps[0].operation_id,
        approved.steps[0].operation_id
    );
    assert!(refreshed.quote_revisions.len() > approved.quote_revisions.len());
    h.chain.lock().unwrap().attestation_ready = true;
    let _ = h.continue_compound(&id).await;
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(1, 2), (42161, 1)])
    );
}

#[tokio::test]
async fn non_sorted_fee_caps_keep_exact_compound_scope_through_mint() {
    let h = Harness::new();
    let (id, transfer) = h.start_compound_options("unsorted-caps", true, true).await;
    h.chain.lock().unwrap().attestation_ready = true;
    let result = h.continue_compound(&id).await;
    assert!(result["error"].is_null(), "{result}");
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert_eq!(
        h.chain.lock().unwrap().destination_balance,
        U256::from(9_995_000)
    );
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_mint_candidate_preserves_transit_and_reuses_authority() {
    Box::pin(async {
        let h = Harness::new();
        let (id, transfer) = h.start_compound("reject-mint").await;
        let prior = h.state.transfer(&transfer).unwrap().unwrap();
        {
            let mut state = h.chain.lock().unwrap();
            state.attestation_ready = true;
            state.reject_mint_revalidation = true;
        }
        let status = h
            .call(
                args(&["status", &id, "--json"]),
                BrokerRequest::Status { id: id.clone() },
            )
            .await;
        let token = status
            .pointer("/outcome/next_action/continuation_token")
            .and_then(Value::as_str)
            .unwrap();
        let err = h.continuation_error(&id, token).await;
        assert_eq!(err.code, "RPC_REQUEST_FAILED");
        assert_eq!(
            h.state.transfer(&transfer).unwrap().unwrap().transit,
            prior.transit
        );
        assert_eq!(h.chain.lock().unwrap().broadcasts, BTreeMap::from([(1, 2)]));
        h.chain.lock().unwrap().reject_mint_revalidation = false;
        let _ = h.continue_compound(&id).await;
        assert_eq!(h.status(&transfer).await["state"], "completed");
        assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            h.chain.lock().unwrap().broadcasts,
            BTreeMap::from([(1, 2), (42161, 1)])
        );
    })
    .await
}

#[path = "support/cctp_solana_lifecycle.rs"]
mod solana;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicitly built development CLI"]
async fn cli_base_arbitrum_cctp_positive_lifecycle() {
    Box::pin(async {
        assert!(
            std::env::var_os("GOFIN_CCTP_CLI").is_some(),
            "real CLI is required"
        );
        let h = Harness::new_with_route(OffsetClock::realtime(), 8453, 42161);
        let (id, transfer) = Box::pin(h.start_compound("cli-route")).await;
        assert_eq!(h.status(&transfer).await["state"], "in_transit");
        {
            let mut state = h.chain.lock().unwrap();
            let raw = state.source_message.as_ref().expect("burn message");
            let message = decode_message(&hex_bytes(raw)).unwrap();
            assert_eq!(message.source_domain, domain(8453).unwrap());
            assert_eq!(message.destination_domain, domain(42161).unwrap());
            assert_eq!(state.source_balance, U256::from(10_000_000));
            assert_eq!(state.destination_balance, U256::ZERO);
            state.attestation_ready = true;
        }
        let done = Box::pin(h.continue_compound(&id)).await;
        assert_eq!(done["outcome"]["phase"],"done","{done}");
        assert_eq!(done["outcome"]["result"],"executed");
        assert_eq!(done["outcome"]["replay_safe"],false);
        assert_eq!(Box::pin(h.status(&transfer)).await["state"], "completed");
        let state = h.chain.lock().unwrap();
        assert_eq!(state.destination_balance, U256::from(10_000_000));
        assert_eq!(state.broadcasts, BTreeMap::from([(8453, 2), (42161, 1)]));
        if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {
            std::fs::write(path,serde_json::to_vec_pretty(&json!({"scenario":"cctp-base-arbitrum","approval_count":h.approvals.0.load(Ordering::SeqCst),"external_submission_count":state.broadcasts.values().sum::<usize>(),"stage_submissions":state.broadcasts,"intervention_count":0,"remaining_external_uncertainty":false,"blocked_unrelated_activity":null})).unwrap()).unwrap();
        }
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicitly built development CLI"]
async fn cli_arbitrum_base_cctp_positive_lifecycle() {
    Box::pin(async {
        assert!(
            std::env::var_os("GOFIN_CCTP_CLI").is_some(),
            "real CLI is required"
        );
        let h = Harness::new_with_route(OffsetClock::realtime(), 42161, 8453);
        let (id, transfer) = Box::pin(h.start_compound("cli-route")).await;
        assert_eq!(h.status(&transfer).await["state"], "in_transit");
        {
            let mut state = h.chain.lock().unwrap();
            let raw = state.source_message.as_ref().expect("burn message");
            let message = decode_message(&hex_bytes(raw)).unwrap();
            assert_eq!(message.source_domain, domain(42161).unwrap());
            assert_eq!(message.destination_domain, domain(8453).unwrap());
            assert_eq!(state.source_balance, U256::from(10_000_000));
            assert_eq!(state.destination_balance, U256::ZERO);
            state.attestation_ready = true;
        }
        let done = Box::pin(h.continue_compound(&id)).await;
        assert_eq!(done["outcome"]["phase"],"done","{done}");
        assert_eq!(done["outcome"]["result"],"executed");
        assert_eq!(done["outcome"]["replay_safe"],false);
        assert_eq!(Box::pin(h.status(&transfer)).await["state"], "completed");
        let state = h.chain.lock().unwrap();
        assert_eq!(state.destination_balance, U256::from(10_000_000));
        assert_eq!(state.broadcasts, BTreeMap::from([(42161, 2), (8453, 1)]));
        if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {
            std::fs::write(path,serde_json::to_vec_pretty(&json!({"scenario":"cctp-arbitrum-base","approval_count":h.approvals.0.load(Ordering::SeqCst),"external_submission_count":state.broadcasts.values().sum::<usize>(),"stage_submissions":state.broadcasts,"intervention_count":0,"remaining_external_uncertainty":false,"blocked_unrelated_activity":null})).unwrap()).unwrap();
        }
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicitly built development CLI"]
async fn cli_compound_cctp_refreshes_fee_after_allowance_without_reapproval() {
    assert!(
        std::env::var_os("GOFIN_CCTP_CLI").is_some(),
        "real CLI is required"
    );
    Box::pin(async {
        let h=Harness::new();h.chain.lock().unwrap().source_fee_after_approval=true;
        let mut bounds=h.bounds();bounds.input=Some(gofin_bridge::AmountInterval{minimum:Quantity::from_u64(10_000_000),maximum:Quantity::from_u64(10_000_000)});bounds.maximum_source_debit=Some(Quantity::from_u64(10_000_000));bounds.minimum_net_receipt=Some(usdc_asset(42161).unwrap().amount(Quantity::from_u64(10_000_000)));bounds.latest_protocol_deadline=None;bounds.fee_caps.retain(|c|c.kind==FeeKind::Gas);for cap in &mut bounds.fee_caps {cap.maximum_usd_micros=None;}
        let input=json!({"intent":{"wallet":"model-wallet","source":{"chainId":1,"token":usdc_asset(1).unwrap().token},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":usdc_asset(42161).unwrap().token}],"recipient":h.wallet,"refundTo":h.wallet,"allowedRefundAssets":[],"amount":{"minimum":"10000000","maximum":"10000000"}},"allowedProviders":["cctp"],"actionDeadline":chrono::Utc::now().timestamp() as u64+600,"bounds":bounds});
        let request=OperationRequest{request_id:"cctp-foreground".into(),operation:"bridge.execute".into(),input:input.clone(),run_id:None};
        let original=h.call(args(&["execute","bridge.execute","--input",&input.to_string(),"--id","cctp-foreground","--json"]),BrokerRequest::Execute{request:request.clone()}).await;
        if original["error"]["code"] == "BROKER_INTERNAL" { let tid=original["preview"]["binding"]["transfer_id"].as_str().unwrap(); eprintln!("CCTP saved transfer: {:#?}", h.state.transfer(tid).unwrap()); }
        assert_ne!(original["error"]["code"], "BROKER_INTERNAL", "{original}");
        let id=original["id"].as_str().unwrap().to_owned();
        let transfer_id=original["preview"]["binding"]["transfer_id"].as_str().unwrap_or_else(||panic!("parent: {original}")).to_owned();
        let transfer=h.state.transfer(&transfer_id).unwrap().unwrap();
        assert_eq!(transfer.steps.iter().filter(|s|s.state==gofin_domain::BridgeStepState::Settled).count(),2,"{original}");
        assert_eq!(h.approvals.0.load(Ordering::SeqCst),1);assert_eq!(h.chain.lock().unwrap().broadcasts.get(&1),Some(&2));
        let attempts=Store::open(h.temp.path().join("cctp.sqlite")).unwrap().list_stage_attempts(&id).unwrap();
        assert_eq!(attempts.len(),2);assert_ne!(attempts[0].candidate,attempts[1].candidate);
        let status=h.call(args(&["status",&id,"--json"]),BrokerRequest::Status{id:id.clone()}).await;
        assert_eq!(status["outcome"]["transfer"],"in_transit","{status}");assert_eq!(status["outcome"]["phase"],"in_progress");
        let repeated=h.call(args(&["execute","bridge.execute","--input",&input.to_string(),"--id","cctp-foreground","--json"]),BrokerRequest::Execute{request}).await;assert_eq!(repeated["id"],id);assert_eq!(h.approvals.0.load(Ordering::SeqCst),1);assert_eq!(h.chain.lock().unwrap().broadcasts.get(&1),Some(&2));
        let fees={let state=h.chain.lock().unwrap();state.transactions.iter().filter(|((chain,_),_)|*chain==1).map(|(_, (tx,_))|tx["maxPriorityFeePerGas"].as_str().unwrap().to_owned()).collect::<std::collections::BTreeSet<_>>()};
        assert_eq!(fees,std::collections::BTreeSet::from(["0x59682f00".to_owned(),"0xb2d05e00".to_owned()]));
        // Independent destination funds appear without a correlatable CCTP
        // mint; a separately approved Hyperliquid deposit may spend them.
        h.chain.lock().unwrap().destination_balance=U256::from(10_000_000);
        let deposit=json!({"requestId":"destination-unattributed","account":"model-hl","wallet":"model-wallet","amount":"10","maxFeeEth":"0.001","maxAmountUsdc":"10"});
        let separate=h.call(args(&["execute","hyperliquid.account.deposit","--input",&deposit.to_string(),"--id","destination-unattributed","--json"]),BrokerRequest::Execute{request:OperationRequest{request_id:"destination-unattributed".into(),operation:"hyperliquid.account.deposit".into(),input:deposit,run_id:None}}).await;
        assert!(!separate["submission"].is_null(),"{separate}");assert_eq!(h.approvals.0.load(Ordering::SeqCst),2);
        let still=h.call(args(&["status",&id,"--json"]),BrokerRequest::Status{id:id.clone()}).await;
        assert_eq!(still["outcome"]["transfer"],"in_transit","{still}");
        let main_counts=(h.approvals.0.load(Ordering::SeqCst),h.chain.lock().unwrap().broadcasts.values().sum::<usize>());
        let mut admission_probes=Vec::new();
        let before=main_counts;
        let probe=json!({"request_id":"bridge-source-probe","wallet":"model-wallet","network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":"0x1111111111111111111111111111111111111111","max_fee_eth":"0.001"});
        let independent=h.call(args(&["execute","wallet.transfer","--input",&probe.to_string(),"--id","bridge-source-probe","--json"]),BrokerRequest::Execute{request:OperationRequest{request_id:"bridge-source-probe".into(),operation:"wallet.transfer".into(),input:probe,run_id:None}}).await;
        assert!(!independent["submission"].is_null(),"{independent}");assert_eq!(h.approvals.0.load(Ordering::SeqCst),3);
        admission_probes.push(outcome_evidence::admission_probe("same_account", before, (h.approvals.0.load(Ordering::SeqCst),h.chain.lock().unwrap().broadcasts.values().sum::<usize>()), &independent));
        let original_wallet=h.wallet.clone();
        h.chain.lock().unwrap().alternate_fork_signer=true;
        let h=h.restart();assert_ne!(h.wallet,original_wallet);
        let before=(h.approvals.0.load(Ordering::SeqCst),h.chain.lock().unwrap().broadcasts.values().sum::<usize>());
        let probe=json!({"request_id":"bridge-other-probe","wallet":"model-wallet","network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":"0x1111111111111111111111111111111111111111","max_fee_eth":"0.001"});
        let independent=h.call(args(&["execute","wallet.transfer","--input",&probe.to_string(),"--id","bridge-other-probe","--json"]),BrokerRequest::Execute{request:OperationRequest{request_id:"bridge-other-probe".into(),operation:"wallet.transfer".into(),input:probe,run_id:None}}).await;
        admission_probes.push(outcome_evidence::admission_probe("different_account", before, (h.approvals.0.load(Ordering::SeqCst),h.chain.lock().unwrap().broadcasts.values().sum::<usize>()), &independent));
        if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {let scenario=std::env::var("GOFIN_OUTCOME_SCENARIO").unwrap_or_default();let mut evidence=json!({"scenario":if scenario=="outcome.a17" {"outcome.a17"}else{"outcome.a18"},"approval_count":main_counts.0,"external_submission_count":main_counts.1,"intervention_count":0,"remaining_external_uncertainty":true});outcome_evidence::attach_probes(&mut evidence,&admission_probes);std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();}
    }).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicitly built development CLI"]
async fn cli_cctp_reauthorizes_remaining_gas_without_repeating_allowance() {
    assert!(
        std::env::var_os("GOFIN_CCTP_CLI").is_some(),
        "real CLI is required"
    );
    Box::pin(async {
        let h=Harness::new();h.chain.lock().unwrap().source_large_fee_after_approval=true;
        let mut bounds=h.bounds();bounds.input=Some(gofin_bridge::AmountInterval{minimum:Quantity::from_u64(10_000_000),maximum:Quantity::from_u64(10_000_000)});bounds.maximum_source_debit=Some(Quantity::from_u64(10_000_000));bounds.minimum_net_receipt=Some(usdc_asset(42161).unwrap().amount(Quantity::from_u64(10_000_000)));bounds.latest_protocol_deadline=None;bounds.fee_caps.retain(|c|c.kind==FeeKind::Gas);for cap in &mut bounds.fee_caps {cap.maximum_usd_micros=None;if cap.asset==(gofin_bridge::EvmAsset{chain_id:1,token:None,decimals:18}).asset_id() {cap.maximum=Quantity::new("2500000000000000").unwrap();}}
        let mut input=json!({"intent":{"wallet":"model-wallet","source":{"chainId":1,"token":usdc_asset(1).unwrap().token},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":usdc_asset(42161).unwrap().token}],"recipient":h.wallet,"refundTo":h.wallet,"allowedRefundAssets":[],"amount":{"minimum":"10000000","maximum":"10000000"}},"allowedProviders":["cctp"],"actionDeadline":chrono::Utc::now().timestamp() as u64+600,"bounds":bounds});
        let request=OperationRequest{request_id:"cctp-reauth".into(),operation:"bridge.execute".into(),input:input.clone(),run_id:None};
        let original=h.call(args(&["execute","bridge.execute","--input",&input.to_string(),"--id","cctp-reauth","--json"]),BrokerRequest::Execute{request}).await;
        let id=original["id"].as_str().unwrap().to_owned();let transfer_id=original["preview"]["binding"]["transfer_id"].as_str().unwrap().to_owned();
        let before=h.state.transfer(&transfer_id).unwrap().unwrap();assert_eq!(h.chain.lock().unwrap().broadcasts.get(&1),Some(&1),"{original}");assert_eq!(h.approvals.0.load(Ordering::SeqCst),1);
        let token=original.pointer("/outcome/next_action/continuation_token").and_then(Value::as_str).unwrap_or_else(||panic!("{original}"));
        input["bounds"]["fee_caps"][0]["maximum"]=json!("10000000000000000");input["actionDeadline"]=json!(chrono::Utc::now().timestamp() as u64+900);
        let result=h.call(args(&["operations","reauthorize",&id,"--token",token,"--input",&input.to_string(),"--json"]),BrokerRequest::ReauthorizeOperation{id:id.clone(),token:token.into(),input}).await;
        assert_eq!(result["reauthorization"],"approved","{result}");assert_eq!(h.approvals.0.load(Ordering::SeqCst),2);assert_eq!(h.chain.lock().unwrap().broadcasts.get(&1),Some(&2));let after=h.state.transfer(&transfer_id).unwrap().unwrap();assert_eq!(before.steps[0].operation_id,after.steps[0].operation_id);
    }).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicitly built development CLI"]
async fn cli_cctp_mint_continues_original_bridge_after_attestation() {
    assert!(
        std::env::var_os("GOFIN_CCTP_CLI").is_some(),
        "real CLI is required"
    );
    Box::pin(async {
        let h=Harness::new();h.chain.lock().unwrap().source_fee_after_approval=true;
        let mut bounds=h.bounds();bounds.input=Some(gofin_bridge::AmountInterval{minimum:Quantity::from_u64(10_000_000),maximum:Quantity::from_u64(10_000_000)});bounds.maximum_source_debit=Some(Quantity::from_u64(10_000_000));bounds.minimum_net_receipt=Some(usdc_asset(42161).unwrap().amount(Quantity::from_u64(10_000_000)));bounds.latest_protocol_deadline=None;bounds.fee_caps.retain(|c|c.kind==FeeKind::Gas);for cap in &mut bounds.fee_caps {cap.maximum_usd_micros=None;}
        let input=json!({"intent":{"wallet":"model-wallet","source":{"chainId":1,"token":usdc_asset(1).unwrap().token},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":usdc_asset(42161).unwrap().token}],"recipient":h.wallet,"refundTo":h.wallet,"allowedRefundAssets":[],"amount":{"minimum":"10000000","maximum":"10000000"}},"allowedProviders":["cctp"],"actionDeadline":chrono::Utc::now().timestamp() as u64+600,"bounds":bounds});
        let request=OperationRequest{request_id:"cctp-foreground".into(),operation:"bridge.execute".into(),input:input.clone(),run_id:None};
        let original=h.call(args(&["execute","bridge.execute","--input",&input.to_string(),"--id","cctp-foreground","--json"]),BrokerRequest::Execute{request:request.clone()}).await;
        if original["error"]["code"] == "BROKER_INTERNAL" { let tid=original["preview"]["binding"]["transfer_id"].as_str().unwrap(); eprintln!("CCTP saved transfer: {:#?}", h.state.transfer(tid).unwrap()); }
        assert_ne!(original["error"]["code"], "BROKER_INTERNAL", "{original}");
        let id=original["id"].as_str().unwrap().to_owned();
        let transfer_id=original["preview"]["binding"]["transfer_id"].as_str().unwrap_or_else(||panic!("parent: {original}")).to_owned();
        let transfer=h.state.transfer(&transfer_id).unwrap().unwrap();
        assert_eq!(transfer.steps.iter().filter(|s|s.state==gofin_domain::BridgeStepState::Settled).count(),2,"{original}");
        assert_eq!(h.approvals.0.load(Ordering::SeqCst),1);assert_eq!(h.chain.lock().unwrap().broadcasts.get(&1),Some(&2));
        let attempts=Store::open(h.temp.path().join("cctp.sqlite")).unwrap().list_stage_attempts(&id).unwrap();
        assert_eq!(attempts.len(),2);assert_ne!(attempts[0].candidate,attempts[1].candidate);
        let status=h.call(args(&["status",&id,"--json"]),BrokerRequest::Status{id:id.clone()}).await;
        assert_eq!(status["outcome"]["transfer"],"in_transit","{status}");assert_eq!(status["outcome"]["phase"],"in_progress");
        let repeated=h.call(args(&["execute","bridge.execute","--input",&input.to_string(),"--id","cctp-foreground","--json"]),BrokerRequest::Execute{request}).await;assert_eq!(repeated["id"],id);assert_eq!(h.approvals.0.load(Ordering::SeqCst),1);assert_eq!(h.chain.lock().unwrap().broadcasts.get(&1),Some(&2));
        let fees={let state=h.chain.lock().unwrap();state.transactions.iter().filter(|((chain,_),_)|*chain==1).map(|(_, (tx,_))|tx["maxPriorityFeePerGas"].as_str().unwrap().to_owned()).collect::<std::collections::BTreeSet<_>>()};
        assert_eq!(fees,std::collections::BTreeSet::from(["0x59682f00".to_owned(),"0xb2d05e00".to_owned()]));
        h.chain.lock().unwrap().attestation_ready=true;
        let observed=h.call(args(&["status",&id,"--json"]),BrokerRequest::Status{id:id.clone()}).await;
        let token=observed.pointer("/outcome/next_action/continuation_token").and_then(Value::as_str).unwrap_or_else(||panic!("missing continuation: {observed}"));
        let continued=h.call(args(&["operations","continue",&id,"--token",token,"--json"]),BrokerRequest::ContinueOperation{id:id.clone(),token:token.into()}).await;
        assert_eq!(h.approvals.0.load(Ordering::SeqCst),1,"{continued}");
        assert_eq!(h.chain.lock().unwrap().broadcasts.get(&42161),Some(&1),"{continued}");
        let final_status=h.call(args(&["status",&id,"--json"]),BrokerRequest::Status{id:id.clone()}).await;
        assert_eq!(final_status["outcome"]["transfer"],"available","{final_status}");
        assert_eq!(h.state.transfer(&transfer_id).unwrap().unwrap().state,gofin_domain::BridgeTransferState::Completed);
        assert_eq!(h.chain.lock().unwrap().destination_balance,U256::from(10_000_000));
        let scans={let mut state=h.chain.lock().unwrap();state.reject_destination_log_scan=true;state.destination_log_scans};
        let restarted=h.restart();
        let again=restarted.call(args(&["status",&id,"--json"]),BrokerRequest::Status{id:id.clone()}).await;
        assert_eq!(again["outcome"]["transfer"],"available","{again}");
        assert_eq!(restarted.chain.lock().unwrap().destination_log_scans,scans);
        assert_eq!(restarted.approvals.0.load(Ordering::SeqCst),1);
    }).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires guarded owned CCTP forks and an explicitly built development CLI"]
async fn owned_fork_cli_cctp_fee_refresh_and_remaining_reauthorization() {
    assert!(std::env::var_os("GOFIN_CCTP_CLI").is_some());
    Box::pin(async {
        let configs:Vec<Value>=serde_json::from_str(&std::env::var("GOFIN_CCTP_FORKS").expect("owned fork configuration required")).unwrap();
        let mut forks=BTreeMap::new();for config in configs {let fork=Arc::new(owned_fork::Fork::new(&config).await.unwrap());forks.insert(fork.chain(),fork);}
        assert!(forks.contains_key(&1)&&forks.contains_key(&42161));
        forks[&1].fund_usdc(&usdc_asset(1).unwrap().token.unwrap()).await.unwrap();
        let other=lower_address(LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap().address());
        forks[&1].mutate("anvil_setBalance",json!([other,"0x56bc75e2d63100000"])).await.unwrap();
        let mut main_counts=(0,0);
        let mut admission_probes=Vec::new();
        for large in [false,true] {
            let chain=Arc::new(Mutex::new(ChainState{owned_forks:Some(forks.clone()),source_fee_after_approval:!large,source_large_fee_after_approval:large,..Default::default()}));
            let h=Harness::build(tempfile::tempdir().unwrap(),chain.clone(),Arc::new(Approvals::default()),Arc::new(Iris::new(chain)),OffsetClock::realtime());
            assert_eq!(h.wallet,owned_fork::OWNER);
            let (id,transfer)=h.start_compound(if large {"fork-reauthorize"} else {"fork-refresh"}).await;
            let before=h.state.transfer(&transfer).unwrap().unwrap();
            assert_eq!(before.steps[0].state,gofin_domain::BridgeStepState::Settled);
            if large {
                assert_eq!(h.chain.lock().unwrap().broadcasts.get(&1),Some(&1));
                let status=h.call(args(&["status",&id,"--json"]),BrokerRequest::Status{id:id.clone()}).await;
                assert_eq!(status["outcome"]["next_action"]["code"],"reauthorize","{status}");
                let token=status.pointer("/outcome/next_action/continuation_token").and_then(Value::as_str).unwrap();
                let mut input=status["input"].clone();input["bounds"]["fee_caps"][0]["maximum"]=json!("100000000000000000");input["actionDeadline"]=json!(chrono::Utc::now().timestamp() as u64+900);
                let result=h.call(args(&["operations","reauthorize",&id,"--token",token,"--input",&input.to_string(),"--json"]),BrokerRequest::ReauthorizeOperation{id:id.clone(),token:token.into(),input}).await;
                assert_eq!(result["reauthorization"],"approved","{result}");assert_eq!(h.approvals.0.load(Ordering::SeqCst),2);
                let revised=h.call(args(&["status",&id,"--json"]),BrokerRequest::Status{id:id.clone()}).await;assert!(revised["error"].is_null(),"{revised}");
            } else {assert_eq!(h.approvals.0.load(Ordering::SeqCst),1);}
            let after=h.state.transfer(&transfer).unwrap().unwrap();assert_eq!(after.steps[0].operation_id,before.steps[0].operation_id);
            assert_eq!(after.steps[1].state,gofin_domain::BridgeStepState::Settled,"{after:?}");
            assert_eq!(h.chain.lock().unwrap().broadcasts.get(&1),Some(&2));
            let attempts=Store::open(h.temp.path().join("cctp.sqlite")).unwrap().list_stage_attempts(&id).unwrap();assert_eq!(attempts.len(),2);
            assert_ne!(attempts[0].candidate.as_ref().unwrap().pointer("/binding/transaction/max_priority_fee_per_gas"),attempts[1].candidate.as_ref().unwrap().pointer("/binding/transaction/max_priority_fee_per_gas"));
            let before=(h.approvals.0.load(Ordering::SeqCst),h.chain.lock().unwrap().broadcasts.values().sum::<usize>());main_counts.0+=before.0;main_counts.1+=before.1;
            let probe_id=format!("fork-probe-{large}-same");
            let input=json!({"request_id":probe_id,"wallet":"model-wallet","network":"ethereum","chain_id":1,"token":"native","amount":"0.00001","recipient":"0x1111111111111111111111111111111111111111","max_fee_eth":"0.1"});
            let request=OperationRequest{request_id:probe_id.clone(),operation:"wallet.transfer".into(),input:input.clone(),run_id:None};
            let result=h.call(args(&["execute","wallet.transfer","--input",&input.to_string(),"--id",&probe_id,"--json"]),BrokerRequest::Execute{request}).await;
            admission_probes.push(outcome_evidence::admission_probe("same_account",before,(h.approvals.0.load(Ordering::SeqCst),h.chain.lock().unwrap().broadcasts.values().sum::<usize>()),&result));
            h.chain.lock().unwrap().alternate_fork_signer=true;
            let h=h.restart();assert_ne!(h.wallet,owned_fork::OWNER);
            let before=(h.approvals.0.load(Ordering::SeqCst),h.chain.lock().unwrap().broadcasts.values().sum::<usize>());
            let probe_id=format!("fork-probe-{large}-other");
            let input=json!({"request_id":probe_id,"wallet":"model-wallet","network":"ethereum","chain_id":1,"token":"native","amount":"0.00001","recipient":"0x1111111111111111111111111111111111111111","max_fee_eth":"0.1"});
            let request=OperationRequest{request_id:probe_id.clone(),operation:"wallet.transfer".into(),input:input.clone(),run_id:None};
            let result=h.call(args(&["execute","wallet.transfer","--input",&input.to_string(),"--id",&probe_id,"--json"]),BrokerRequest::Execute{request}).await;
            admission_probes.push(outcome_evidence::admission_probe("different_account",before,(h.approvals.0.load(Ordering::SeqCst),h.chain.lock().unwrap().broadcasts.values().sum::<usize>()),&result));
            assert_eq!(h.approvals.0.load(Ordering::SeqCst),if large {4}else{3});
            assert_eq!(h.chain.lock().unwrap().broadcasts,BTreeMap::from([(1,4)]));
        }
        if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {
            let mut evidence=json!({"scenario":"outcome.a18","approval_count":main_counts.0,"external_submission_count":main_counts.1,"probe_approval_count":4,"probe_submission_count":4,"intervention_count":0,"remaining_external_uncertainty":true,"blocked_unrelated_activity":false,"fork_scope":"guarded Ethereum and Arbitrum Anvil forks, public synthetic signer, no live writes","stage_submissions":{"allowance":2,"burn":2},"probe_scope":"same source wallet and a different synthetic wallet on the same source chain"});outcome_evidence::attach_probes(&mut evidence,&admission_probes);std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
        }
    }).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicitly built development CLI"]
async fn cli_preapproval_cctp_failure_continues_same_original_once() {
    assert!(std::env::var_os("GOFIN_CCTP_CLI").is_some());
    Box::pin(async {
        let h=Harness::new();h.chain.lock().unwrap().reject_preparation_reads=true;
        let mut bounds=h.bounds();bounds.input=Some(gofin_bridge::AmountInterval{minimum:Quantity::from_u64(10_000_000),maximum:Quantity::from_u64(10_000_000)});bounds.maximum_source_debit=Some(Quantity::from_u64(10_000_000));bounds.minimum_net_receipt=Some(usdc_asset(42161).unwrap().amount(Quantity::from_u64(10_000_000)));bounds.fee_caps.retain(|c|c.kind==FeeKind::Gas);for cap in &mut bounds.fee_caps {cap.maximum_usd_micros=None;}
        let input=json!({"intent":{"wallet":"model-wallet","source":{"chainId":1,"token":usdc_asset(1).unwrap().token},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":usdc_asset(42161).unwrap().token}],"recipient":h.wallet,"refundTo":h.wallet,"allowedRefundAssets":[],"amount":{"minimum":"10000000","maximum":"10000000"}},"allowedProviders":["cctp"],"actionDeadline":chrono::Utc::now().timestamp() as u64+600,"bounds":bounds});
        let request=OperationRequest{request_id:"cctp-preapproval-failure".into(),operation:"bridge.execute".into(),input:input.clone(),run_id:None};
        let first=h.call(args(&["execute","bridge.execute","--input",&input.to_string(),"--id","cctp-preapproval-failure","--json"]),BrokerRequest::Execute{request:request.clone()}).await;
        assert_eq!(first["outcome"]["dispatch"],"not_dispatched","{first}");let id=first["id"].as_str().unwrap().to_owned();let token=first.pointer("/outcome/next_action/continuation_token").and_then(Value::as_str).unwrap_or_else(||panic!("{first}"));
        assert_eq!(h.approvals.0.load(Ordering::SeqCst),0);assert!(h.chain.lock().unwrap().broadcasts.is_empty());
        {let mut state=h.chain.lock().unwrap();state.reject_preparation_reads=false;state.attestation_ready=true;}
        let continued=h.call(args(&["operations","continue",&id,"--token",token,"--json"]),BrokerRequest::ContinueOperation{id:id.clone(),token:token.into()}).await;
        assert_eq!(continued["id"],id,"{continued}");assert_eq!(h.approvals.0.load(Ordering::SeqCst),1);
        let prior=h.chain.lock().unwrap().broadcasts.clone();assert_eq!(prior,BTreeMap::from([(1,2),(42161,1)]));
        let repeated=h.call(args(&["execute","bridge.execute","--input",&input.to_string(),"--id","cctp-preapproval-failure","--json"]),BrokerRequest::Execute{request}).await;assert_eq!(repeated["id"],id);
        let _=h.call(args(&["operations","continue",&id,"--token",token,"--json"]),BrokerRequest::ContinueOperation{id,token:token.into()}).await;
        assert_eq!(h.approvals.0.load(Ordering::SeqCst),1);assert_eq!(h.chain.lock().unwrap().broadcasts,prior);
    }).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicitly built development CLI"]
async fn cli_resumed_cctp_disconnect_after_admission_stops_later_stages() {
    let binary = std::env::var("GOFIN_CCTP_CLI").expect("real CLI required");
    Box::pin(async {
        let h=Harness::new();h.chain.lock().unwrap().reject_preparation_reads=true;
        let mut bounds=h.bounds();bounds.input=Some(gofin_bridge::AmountInterval{minimum:Quantity::from_u64(10_000_000),maximum:Quantity::from_u64(10_000_000)});bounds.maximum_source_debit=Some(Quantity::from_u64(10_000_000));bounds.minimum_net_receipt=Some(usdc_asset(42161).unwrap().amount(Quantity::from_u64(10_000_000)));bounds.fee_caps.retain(|c|c.kind==FeeKind::Gas);for cap in &mut bounds.fee_caps {cap.maximum_usd_micros=None;}
        let input=json!({"intent":{"wallet":"model-wallet","source":{"chainId":1,"token":usdc_asset(1).unwrap().token},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":usdc_asset(42161).unwrap().token}],"recipient":h.wallet,"refundTo":h.wallet,"allowedRefundAssets":[],"amount":{"minimum":"10000000","maximum":"10000000"}},"allowedProviders":["cctp"],"actionDeadline":chrono::Utc::now().timestamp() as u64+600,"bounds":bounds});
        let request=OperationRequest{request_id:"cctp-disconnect-resume".into(),operation:"bridge.execute".into(),input:input.clone(),run_id:None};
        let first=h.call(args(&["execute","bridge.execute","--input",&input.to_string(),"--id","cctp-disconnect-resume","--json"]),BrokerRequest::Execute{request}).await;
        let id=first["id"].as_str().unwrap().to_owned();let token=first.pointer("/outcome/next_action/continuation_token").and_then(Value::as_str).unwrap().to_owned();
        let release={let mut state=h.chain.lock().unwrap();state.reject_preparation_reads=false;state.stall_source_response=true;state.release_source_response.clone()};
        let socket=h.client.get().unwrap().socket.clone();
        let mut child=tokio::process::Command::new(binary).args(["--identity","synthetic-cctp","--build-id","synthetic-cctp-build","operations","continue",&id,"--token",&token,"--json"]).env("GOFIN_DEV_IDENTITY","synthetic-cctp").env("GOFIN_DEV_BUILD_ID","synthetic-cctp-build").env("GOFIN_DEV_SOCKET",socket).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10),async {loop {if h.chain.lock().unwrap().broadcasts.get(&1)==Some(&1){break}tokio::time::sleep(std::time::Duration::from_millis(5)).await;}}).await.unwrap();
        child.kill().await.unwrap();let _=child.wait().await;tokio::time::sleep(std::time::Duration::from_millis(250)).await;release.store(true,Ordering::SeqCst);tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(h.approvals.0.load(Ordering::SeqCst),1);assert_eq!(h.chain.lock().unwrap().broadcasts,BTreeMap::from([(1,1)]));
        let _=h.call(args(&["status",&id,"--json"]),BrokerRequest::Status{id:id.clone()}).await;
        let _=h.call(args(&["operations","continue",&id,"--token",&token,"--json"]),BrokerRequest::ContinueOperation{id,token}).await;
        assert_eq!(h.approvals.0.load(Ordering::SeqCst),1);assert_eq!(h.chain.lock().unwrap().broadcasts,BTreeMap::from([(1,1)]));
    }).await
}
