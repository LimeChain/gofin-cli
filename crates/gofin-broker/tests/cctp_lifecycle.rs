//! Synthetic CCTP lifecycle through real adapter, signatures, engine and journal.
//! Iris and chain transports are modeled; this is not live or fork evidence.
//! Build gofin-dev with GOFIN_DEV_BUILD_ID=synthetic-cctp-build, then set GOFIN_CCTP_CLI
//! to its absolute path to include the public CLI and development socket transport.
//! GOFIN_CCTP_BUILD_ID selects another explicitly built development binary identity.
use async_trait::async_trait;
use gofin_bridge::{BridgeBounds, FeeCap, FeeKind, cctp::*};
use gofin_broker::{BridgeAdapter, SharedUsdValuator};
use gofin_domain::financial_plans::{
    FinancialPlanAuthorizationRequest, FinancialPlanBounds, FinancialPlanSpec,
    FinancialPlanStepSpec, FinancialStepCompletion,
};
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
    lose_response: Option<u64>,
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
            return Ok(encoded(if self.chain != 42161 {
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
        let mut s = self.state.lock().unwrap();
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
        Ok(match method {
            "eth_chainId" => json!(format!("{:#x}", self.chain)),
            "eth_getCode" => json!("0x6000"),
            "eth_getBalance" => json!(if self.chain == 42161 && s.gas_missing {
                "0x0".to_owned()
            } else if self.chain != 42161 && s.source_gas_balance.is_some() {
                format!("{:#x}", s.source_gas_balance.unwrap())
            } else {
                "0x56bc75e2d63100000".to_owned()
            }),
            "eth_blockNumber" => json!(format!(
                "{:#x}",
                if self.chain != 42161 {
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
                s.broadcasts.get(&self.chain).copied().unwrap_or(0)
            )),
            "eth_maxPriorityFeePerGas" => json!("0x3b9aca00"),
            "eth_estimateGas" => json!("0x186a0"),
            "eth_call" => self.call(&p, &s)?,
            "eth_getLogs" => {
                if self.chain == 42161 {
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
                if self.chain == 42161 && s.hide_destination_receipt {
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
                                destinationTokenMessenger: remote_messenger(call.destinationDomain),
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
                        if self.chain != 42161
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
                if self.chain != 42161
                    && let Some(balance) = s.source_gas_balance.as_mut()
                {
                    *balance -= U256::from(30_000_000_000_000u64);
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
        })
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
struct Approvals(AtomicUsize);
#[async_trait]
impl Authorizer for Approvals {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
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
        let chain = Arc::new(Mutex::new(ChainState {
            source_balance: U256::from(20_000_000),
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
        let signer = Arc::new(LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap());
        let wallet = lower_address(signer.address());
        let prices = solana
            .as_ref()
            .and_then(|s| s.prices.get().cloned())
            .unwrap_or_else(|| Arc::new(Prices));
        let valuator = Arc::new(SharedUsdValuator::with_source(prices));
        let mut wallets = BTreeMap::new();
        let mut observers = BTreeMap::new();
        for chain_id in [1, 42161]
            .into_iter()
            .chain(solana.is_some().then_some(8453))
        {
            let rpc = Arc::new(Rpc {
                chain: chain_id,
                state: chain.clone(),
            });
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
                ),
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
        let mut fees: Vec<_> = [1, 42161]
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
            asset: usdc_asset(42161).unwrap().asset_id(),
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
        let input = json!({"provider":"cctp","cctp":{"transferMode":if fast{"fast"}else{"standard"}},"intent":{"wallet":"model-wallet","source":{"chainId":1,"token":usdc_asset(1).unwrap().token},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":usdc_asset(42161).unwrap().token}],"recipient":self.wallet,"refundTo":self.wallet,"allowedRefundAssets":[],"amount":"10000000"},"bounds":bounds});
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
    fn plan(&self, inputs: &[Value], fast: bool) -> FinancialPlanSpec {
        let source = usdc_asset(1)
            .unwrap()
            .amount(Quantity::from_u64(10_000_000));
        FinancialPlanSpec {
            version: gofin_domain::financial_plans::FINANCIAL_PLAN_VERSION,
            name: "Synthetic complete CCTP transfer".into(),
            summary: None,
            steps: inputs
                .iter()
                .enumerate()
                .map(|(i, input)| FinancialPlanStepSpec {
                    action_deadline_seconds: None,
                    step_id: format!("stage-{i}"),
                    operation: "bridge.transfer".into(),
                    input: input.clone(),
                    permitted_variations: json!({}),
                    completion: FinancialStepCompletion {
                        operation_succeeded: true,
                        principal_spend: None,
                        minimum_net_receipt: if input["stepKind"] == "mint" {
                            Some(gofin_domain::ResourceAmount {
                                resource: ResourceId(format!("ethereum:42161:{}", self.wallet)),
                                amount: usdc_asset(42161).unwrap().amount(Quantity::from_u64(
                                    if fast { 9_995_000 } else { 10_000_000 },
                                )),
                            })
                        } else {
                            None
                        },
                    },
                    funding_dependencies: vec![],
                })
                .collect(),
            aggregate_bounds: FinancialPlanBounds {
                maximum_debits: vec![gofin_domain::ResourceAmount {
                    resource: ResourceId(format!("ethereum:1:{}", self.wallet)),
                    amount: source,
                }],
                maximum_costs: self
                    .bounds()
                    .fee_caps
                    .into_iter()
                    .filter(|cap| cap.kind == FeeKind::Gas)
                    .map(|cap| gofin_domain::CostBound {
                        location: gofin_domain::AccountingLocation::Cash {
                            resource: ResourceId(format!("{}:{}", cap.asset.network, self.wallet)),
                        },
                        amount: gofin_domain::Amount {
                            asset: cap.asset,
                            atoms: cap.maximum,
                        },
                        treatment: gofin_domain::CostTreatment::AdditionalDebit,
                    })
                    .collect(),
            },
        }
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
    async fn authorize(&self, plan: FinancialPlanSpec) -> String {
        self.authorize_named(plan, "authorize-model").await
    }
    async fn authorize_named(&self, plan: FinancialPlanSpec, request_id: &str) -> String {
        let mut request = FinancialPlanAuthorizationRequest {
            request_id: request_id.into(),
            plan: plan.clone(),
            valid_for_seconds: Some(600),
            replaces_revision_id: None,
            expected_review_hash: None,
        };
        let review = self
            .call(
                args(&[
                    "plans",
                    "preview",
                    "--input",
                    &serde_json::to_string(&plan).unwrap(),
                    "--valid-for",
                    "10m",
                    "--json",
                ]),
                BrokerRequest::PreviewPlan {
                    request: request.clone(),
                },
            )
            .await;
        request.expected_review_hash = Some(review["review_hash"].as_str().unwrap().into());
        let result = self
            .call(
                args(&[
                    "plans",
                    "authorize",
                    "--input",
                    &serde_json::to_string(&plan).unwrap(),
                    "--id",
                    &request.request_id,
                    "--review-hash",
                    request.expected_review_hash.as_ref().unwrap(),
                    "--valid-for",
                    "10m",
                    "--json",
                ]),
                BrokerRequest::AuthorizePlan { request },
            )
            .await;
        result["plan_id"].as_str().unwrap().into()
    }
    async fn execute(
        &self,
        plan: &str,
        index: usize,
        input: &Value,
    ) -> Result<gofin_domain::OperationRecord> {
        self.engine
            .execute_financial_plan_step(
                plan,
                &format!("stage-{index}"),
                OperationRequest {
                    request_id: format!("execute-stage-{index}"),
                    operation: "bridge.transfer".into(),
                    input: input.clone(),
                    run_id: None,
                },
            )
            .await
    }
    async fn execute_public(
        &self,
        plan: &str,
        step: &str,
        request: OperationRequest,
    ) -> gofin_domain::OperationRecord {
        serde_json::from_value(
            self.call(
                args(&[
                    "execute",
                    &request.operation,
                    "--input",
                    &request.input.to_string(),
                    "--id",
                    &request.request_id,
                    "--plan",
                    plan,
                    "--step",
                    step,
                    "--json",
                ]),
                BrokerRequest::ExecutePlanStep {
                    plan_id: plan.into(),
                    step_id: step.into(),
                    request,
                },
            )
            .await,
        )
        .unwrap()
    }
    async fn operation_status(&self, id: &str) -> gofin_domain::OperationRecord {
        serde_json::from_value(
            self.call(
                args(&["status", id, "--json"]),
                BrokerRequest::Status { id: id.into() },
            )
            .await,
        )
        .unwrap()
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
    async fn status(&self, id: &str) -> Value {
        self.read("bridge.status", json!({"transferId":id})).await
    }
}

#[path = "support/cctp_stack_refresh.rs"]
mod stack_refresh;
#[tokio::test]
async fn known_canonical_mint_settles_without_destination_log_scan() {
    let h = Harness::new();
    let (transfer, inputs) = h.quote(false).await;
    let plan = h.authorize(h.plan(&inputs, false)).await;
    for (i, input) in inputs.iter().enumerate().take(2) {
        h.settle(h.execute(&plan, i, input).await.unwrap()).await;
    }
    h.chain.lock().unwrap().attestation_ready = true;
    h.settle(h.execute(&plan, 2, &inputs[2]).await.unwrap())
        .await;
    assert_ne!(
        h.state.transfer(&transfer).unwrap().unwrap().state,
        gofin_domain::BridgeTransferState::Completed
    );
    let scans_before = {
        let mut state = h.chain.lock().unwrap();
        state.reject_destination_log_scan = true;
        state.destination_log_scans
    };
    let h = h.restart();
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert_eq!(h.chain.lock().unwrap().destination_log_scans, scans_before);
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(1, 2), (42161, 1)])
    );
}

#[tokio::test]
async fn complete_cctp_named_plan_requires_explicit_mint_and_credits_only_net() {
    for fast in [false, true] {
        let h = Harness::new();
        let (transfer, inputs) = h.quote(fast).await;
        assert_eq!(inputs.len(), 3);
        let plan = h.authorize(h.plan(&inputs, fast)).await;
        for (i, input) in inputs.iter().enumerate().take(2) {
            h.settle(h.execute(&plan, i, input).await.unwrap()).await;
        }
        assert_eq!(
            h.state.transfer(&transfer).unwrap().unwrap().state,
            gofin_domain::BridgeTransferState::InTransit
        );
        assert_eq!(h.chain.lock().unwrap().destination_balance, U256::ZERO);
        assert_eq!(
            h.status(&transfer).await["provider_status"]["status"],
            "attestation_pending"
        );
        h.chain.lock().unwrap().attestation_ready = true;
        assert_eq!(
            h.status(&transfer).await["provider_status"]["status"],
            "mint_ready"
        );
        h.settle(h.execute(&plan, 2, &inputs[2]).await.unwrap())
            .await;
        assert_eq!(h.status(&transfer).await["state"], "completed");
        let original = h.state.transfer(&transfer).unwrap().unwrap();
        assert_eq!(h.status(&transfer).await["state"], "completed");
        assert_eq!(original, h.state.transfer(&transfer).unwrap().unwrap());
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
}

#[tokio::test]
async fn response_loss_each_submission_boundary_restarts_without_duplicate_burn_or_mint() {
    let mut h = Harness::new();
    let (transfer, inputs) = h.quote(false).await;
    let plan = h.authorize(h.plan(&inputs, false)).await;
    let mut completed = vec![];
    for (i, input) in inputs.iter().enumerate() {
        if i == 2 {
            h.chain.lock().unwrap().attestation_ready = true;
        }
        h.chain.lock().unwrap().lose_response = Some(if i < 2 { 1 } else { 42161 });
        let op = h.execute(&plan, i, input).await.unwrap();
        assert!(op.submission.is_some(), "{:?}", op.error);
        h = h.restart();
        completed.push(h.settle(op).await);
    }
    for (i, op) in completed.iter().enumerate() {
        let duplicate = h.execute(&plan, i, &inputs[i]).await.unwrap();
        assert_eq!(duplicate.id, op.id);
        assert_eq!(
            duplicate.submission.as_ref().unwrap().reference,
            op.submission.as_ref().unwrap().reference
        );
    }
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(1, 2), (42161, 1)])
    );
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn delayed_evidence_and_gas_shortage_preserve_source_then_resume_same_mint() {
    let h = Harness::new();
    let (transfer, inputs) = h.quote(false).await;
    let plan = h.authorize(h.plan(&inputs, false)).await;
    for (i, input) in inputs.iter().enumerate().take(2) {
        h.settle(h.execute(&plan, i, input).await.unwrap()).await;
    }
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
    assert!(
        h.bridge
            .prepare("bridge.transfer", &inputs[2])
            .await
            .is_err()
    );
    assert_eq!(h.chain.lock().unwrap().broadcasts, BTreeMap::from([(1, 2)]));
    {
        let mut s = h.chain.lock().unwrap();
        s.gas_missing = false;
        s.hide_destination_receipt = true;
    }
    let op = h.execute(&plan, 2, &inputs[2]).await.unwrap();
    assert!(op.submission.is_some(), "{:?}", op.error);
    let h = h.restart();
    let pending = h.status(&transfer).await;
    assert_ne!(pending["state"], "completed");
    assert_eq!(
        h.state.transfer(&transfer).unwrap().unwrap().transit,
        Some(source)
    );
    h.chain.lock().unwrap().hide_destination_receipt = false;
    h.settle(op).await;
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(1, 2), (42161, 1)])
    );
}
#[tokio::test]
async fn invalid_attestation_and_revoked_plan_cannot_dispatch_mint() {
    let h = Harness::new();
    let (transfer, inputs) = h.quote(false).await;
    let plan = h.authorize(h.plan(&inputs, false)).await;
    for (i, input) in inputs.iter().enumerate().take(2) {
        h.settle(h.execute(&plan, i, input).await.unwrap()).await;
    }
    {
        let mut s = h.chain.lock().unwrap();
        s.attestation_ready = true;
        s.mutate_attestation = true;
    }
    assert!(
        h.bridge
            .prepare("bridge.transfer", &inputs[2])
            .await
            .is_err()
    );
    h.chain.lock().unwrap().mutate_attestation = false;
    h.engine.revoke_financial_plan(&plan).await.unwrap();
    assert!(h.execute(&plan, 2, &inputs[2]).await.is_err());
    assert_eq!(
        h.status(&transfer).await["provider_status"]["status"],
        "mint_ready"
    );
    assert_eq!(h.chain.lock().unwrap().broadcasts, BTreeMap::from([(1, 2)]));
    assert!(
        h.state
            .transfer(&transfer)
            .unwrap()
            .unwrap()
            .transit
            .is_some()
    );
}
#[tokio::test]
async fn processed_message_without_native_usdc_credit_never_completes() {
    let h = Harness::new();
    let (transfer, inputs) = h.quote(false).await;
    let plan = h.authorize(h.plan(&inputs, false)).await;
    for (i, input) in inputs.iter().enumerate().take(2) {
        h.settle(h.execute(&plan, i, input).await.unwrap()).await;
    }
    {
        let mut s = h.chain.lock().unwrap();
        s.attestation_ready = true;
        s.omit_credit = true;
    }
    let op = h.execute(&plan, 2, &inputs[2]).await.unwrap();
    assert!(op.submission.is_some());
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
    assert!(
        h.bridge
            .prepare("bridge.transfer", &inputs[2])
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_cctp_mint_receipt_survives_restart_and_funds_correlated_hyperliquid_credit() {
    let h = Harness::new();
    let (transfer, inputs) = h.quote(false).await;
    assert_eq!(h.chain.lock().unwrap().destination_balance, U256::ZERO);
    let deposit = json!({"requestId":"hl-funding","account":"model-hl","wallet":"model-wallet","amount":"10","maxFeeEth":"0.001","maxAmountUsdc":"10"});
    let mut spec = h.plan(&inputs, false);
    let resource = ResourceId(format!("ethereum:42161:{}", h.wallet));
    let destination_asset = usdc_asset(42161).unwrap().asset_id();
    // Match an explicitly exact, receipt-only mint permission. The ordinary
    // fixture's adapter permission does not exercise this authority shape.
    spec.steps[2].permitted_variations = json!({
        "mode":"exact",
        "maximum_debits":[],
        "maximum_costs":[{
            "location":{"kind":"cash","resource":resource},
            "amount":{"asset":{"network":"ethereum:42161","reference":"native","decimals":18},"atoms":"2000000000000000"},
            "treatment":"additional_debit"
        }],
        "possible_credits":[{"resource":resource,"asset":destination_asset}]
    });
    spec.steps[1].completion.principal_spend = Some(gofin_domain::ResourceAmount {
        resource: ResourceId(format!("ethereum:1:{}", h.wallet)),
        amount: usdc_asset(1)
            .unwrap()
            .amount(Quantity::from_u64(10_000_000)),
    });
    spec.steps.push(FinancialPlanStepSpec {
        action_deadline_seconds: None,
        step_id: "hl-credit".into(),
        operation: "hyperliquid.account.deposit".into(),
        input: deposit.clone(),
        permitted_variations: json!({"mode":"exact","maximum_debits":[{"resource":resource,"amount":{"asset":destination_asset,"atoms":"10000000"}}],"maximum_costs":[{"location":{"kind":"cash","resource":resource},"amount":{"asset":{"network":"ethereum:42161","reference":"native","decimals":18},"atoms":"1000000000000000"},"treatment":"additional_debit"}],"possible_credits":[{"resource":HyperliquidAccount{name:"model-hl".into(),owner:h.wallet.clone(),credential_ref:None,signing_enabled:false,execution_scope:Default::default()}.resource("perpetual",None),"asset":gofin_hyperliquid::usdc()}]}),
        completion: FinancialStepCompletion {
            operation_succeeded: true,
            principal_spend: None,
            minimum_net_receipt: None,
        },
        funding_dependencies: vec![
            gofin_domain::financial_plans::FinancialPlanFundingDependency {
                producing_step_id: "stage-2".into(),
                resource: resource.clone(),
                asset: destination_asset.clone(),
                funding_scope: Some(
                    gofin_domain::financial_plans::FinancialPlanFundingScope::PrincipalOnly,
                ),
                maximum_fraction_bps: None,
            },
        ],
    });
    spec.aggregate_bounds
        .maximum_debits
        .push(gofin_domain::ResourceAmount {
            resource: resource.clone(),
            amount: gofin_domain::Amount {
                asset: destination_asset,
                atoms: Quantity::from_u64(10_000_000),
            },
        });
    let plan = h.authorize(spec).await;
    let approved_revision = Store::open(h.temp.path().join("cctp.sqlite"))
        .unwrap()
        .financial_plan_revisions(&plan)
        .unwrap()
        .remove(0);
    let revision: gofin_domain::financial_plans::FinancialPlanRevision =
        serde_json::from_value(approved_revision.clone()).unwrap();
    let mint_permission = revision.permissions[2].exact.as_ref().unwrap();
    assert!(mint_permission.scope.maximum_debits.is_empty());
    assert!(revision.permissions[2].permission.is_none());
    // A pre-existing wallet balance cannot substitute for this plan's declared producer receipt.
    h.chain.lock().unwrap().destination_balance = U256::from(10_000_000);
    let premature = h
        .engine
        .execute_financial_plan_step(
            &plan,
            "hl-credit",
            OperationRequest {
                request_id: "unrelated-funds-cannot-unlock".into(),
                operation: "hyperliquid.account.deposit".into(),
                input: deposit.clone(),
                run_id: None,
            },
        )
        .await;
    assert!(premature.is_err());
    assert!(h.chain.lock().unwrap().hl_tx.is_none());
    h.chain.lock().unwrap().destination_balance = U256::ZERO;
    for (i, input) in inputs.iter().enumerate().take(2) {
        h.settle(
            h.execute_public(
                &plan,
                &format!("stage-{i}"),
                OperationRequest {
                    request_id: format!("execute-stage-{i}"),
                    operation: "bridge.transfer".into(),
                    input: input.clone(),
                    run_id: None,
                },
            )
            .await,
        )
        .await;
    }
    let before_restart = h.state.transfer(&transfer).unwrap().unwrap();
    let h = h.restart();
    assert_eq!(
        Store::open(h.temp.path().join("cctp.sqlite"))
            .unwrap()
            .financial_plan_revisions(&plan)
            .unwrap(),
        vec![approved_revision]
    );
    assert_eq!(
        h.state.transfer(&transfer).unwrap().unwrap(),
        before_restart
    );
    h.chain.lock().unwrap().attestation_ready = true;
    let prepared = h
        .bridge
        .prepare("bridge.transfer", &inputs[2])
        .await
        .unwrap();
    assert!(prepared.intent.debits.is_empty());
    assert!(
        prepared.intent.expected.is_empty(),
        "CCTP settlement must not fabricate a supply-position withdrawal."
    );
    let mint_request = OperationRequest {
        request_id: "execute-stage-2".into(),
        operation: "bridge.transfer".into(),
        input: inputs[2].clone(),
        run_id: None,
    };
    let mint = h
        .settle(
            h.execute_public(&plan, "stage-2", mint_request.clone())
                .await,
        )
        .await;
    let replay = h.execute_public(&plan, "stage-2", mint_request).await;
    assert_eq!(mint.id, replay.id);
    assert_eq!(mint.submission, replay.submission);
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(1, 2), (42161, 1)])
    );
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert!(h.chain.lock().unwrap().hl_tx.is_none());
    let request = OperationRequest {
        request_id: "hl-deposit-write".into(),
        operation: "hyperliquid.account.deposit".into(),
        input: deposit,
        run_id: None,
    };
    let mut operation = h.execute_public(&plan, "hl-credit", request).await;
    assert!(operation.submission.is_some(), "{:?}", operation.error);
    assert_eq!(h.chain.lock().unwrap().destination_balance, U256::ZERO);
    operation = h.operation_status(&operation.id).await;
    assert_ne!(
        operation.status,
        Status::Completed,
        "A Bridge2 receipt is not account credit"
    );
    let h = h.restart();
    h.chain.lock().unwrap().hl_credit_visible = true;
    operation = h.settle(operation).await;
    assert!(operation.effects.as_ref().unwrap().effects.iter().any(|effect|matches!(effect,gofin_domain::FinancialEffect::UsableCredit{amount,..}|gofin_domain::FinancialEffect::Transfer{credit:amount,..} if amount.asset==gofin_hyperliquid::usdc())));
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(1, 2), (42161, 2)])
    );
    let status: gofin_domain::financial_plans::FinancialPlanStatus = serde_json::from_value(
        h.call(
            args(&["plans", "status", &plan, "--json"]),
            BrokerRequest::PlanStatus {
                plan_id: plan.clone(),
            },
        )
        .await,
    )
    .unwrap();
    assert!(
        status.steps.iter().all(|step| step.state
            == gofin_domain::financial_plans::FinancialPlanStepProgressState::Completed),
        "{status:?}"
    );
    assert_eq!(status.revision.revision_id, revision.revision_id);
    assert_eq!(
        status.revision.approval_binding_hash,
        revision.approval_binding_hash
    );
    let receipts = &status.steps[2].verified_receipts;
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].resource, resource);
    assert_eq!(receipts[0].asset, usdc_asset(42161).unwrap().asset_id());
    assert_eq!(receipts[0].available, Quantity::from_u64(10_000_000));
    assert_eq!(receipts[0].consumed, Quantity::from_u64(10_000_000));
}

#[tokio::test]
async fn sufficient_allowance_skips_approval_and_keeps_two_explicit_stages() {
    let h = Harness::new();
    h.chain.lock().unwrap().allowance = U256::from(10_000_000);
    let (transfer, inputs) = h.quote(false).await;
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[0]["stepKind"], "deposit");
    assert_eq!(inputs[1]["stepKind"], "mint");
    let record = h.state.transfer(&transfer).unwrap().unwrap();
    assert!(
        record.quote_revisions[0].payload["quote"]["protocol_steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|step| step["kind"] == "approval")
    );
    let plan = h.authorize(h.plan(&inputs, false)).await;
    h.settle(h.execute(&plan, 0, &inputs[0]).await.unwrap())
        .await;
    h.chain.lock().unwrap().attestation_ready = true;
    h.settle(h.execute(&plan, 1, &inputs[1]).await.unwrap())
        .await;
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
                    reader
                        .get_mut()
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .unwrap();
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    let envelope: Value = serde_json::from_str(&line).unwrap();
                    let request: BrokerRequest =
                        serde_json::from_value(envelope["request"].clone()).unwrap();
                    let response = gofin_protocol::progress::observe(
                        reader.get_mut(),
                        envelope["progress"].as_bool().unwrap_or(false),
                        runtime.handle(request),
                    )
                    .await;
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
            let response = Box::pin(self.runtime.handle(request)).await;
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
    let h = Harness::new();
    let (transfer, inputs) = h.quote(true).await;
    let plan = h.authorize(h.plan(&inputs, true)).await;
    for (i, input) in inputs.iter().enumerate().take(2) {
        h.settle(h.execute(&plan, i, input).await.unwrap()).await;
    }
    let before = h.state.transfer(&transfer).unwrap().unwrap();
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
    h.settle(h.execute(&plan, 2, &inputs[2]).await.unwrap())
        .await;
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(1, 2), (42161, 1)])
    );
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn expired_plan_renews_same_transfer_and_never_replays_source() {
    let h = Harness::new();
    let (transfer, inputs) = h.quote(false).await;
    let spec = h.plan(&inputs, false);
    let mut request = FinancialPlanAuthorizationRequest {
        request_id: "short-authority".into(),
        plan: spec,
        valid_for_seconds: Some(600),
        replaces_revision_id: None,
        expected_review_hash: None,
    };
    let preview = h
        .engine
        .preview_financial_plan(request.clone())
        .await
        .unwrap();
    request.expected_review_hash = Some(preview.review_hash);
    let plan = h
        .engine
        .authorize_financial_plan(request.clone())
        .await
        .unwrap()
        .plan_id;
    for (i, input) in inputs.iter().enumerate().take(2) {
        h.settle(h.execute(&plan, i, input).await.unwrap()).await;
    }
    let original = h.state.transfer(&transfer).unwrap().unwrap();
    h.clock.freeze();
    h.clock.advance(std::time::Duration::from_secs(601));
    let h = h.restart();
    h.chain.lock().unwrap().attestation_ready = true;
    let expired = h.engine.financial_plan_status(&plan).await.unwrap();
    assert_eq!(
        expired.revision.status,
        gofin_domain::financial_plans::FinancialPlanAuthorizationStatus::Expired
    );
    h.clock.unfreeze();
    assert!(h.execute(&plan, 2, &inputs[2]).await.is_err());
    assert_eq!(h.chain.lock().unwrap().broadcasts, BTreeMap::from([(1, 2)]));
    request.request_id = "renew-authority".into();
    request.valid_for_seconds = Some(600);
    request.replaces_revision_id = Some(expired.revision.revision_id);
    request.expected_review_hash = None;
    let preview = h
        .engine
        .preview_financial_plan(request.clone())
        .await
        .unwrap();
    request.expected_review_hash = Some(preview.review_hash);
    let renewed = h.engine.authorize_financial_plan(request).await.unwrap();
    assert_eq!(renewed.plan_id, plan);
    assert_eq!(
        h.state.transfer(&transfer).unwrap().unwrap().transit,
        original.transit
    );
    h.settle(h.execute(&plan, 2, &inputs[2]).await.unwrap())
        .await;
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(1, 2), (42161, 1)])
    );
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 2);
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
    let (transfer, inputs) = h.quote(false).await;
    let plan = h.authorize(h.plan(&inputs, false)).await;
    for (i, input) in inputs.iter().enumerate().take(2) {
        h.settle(h.execute(&plan, i, input).await.unwrap()).await;
    }
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
    assert!(h.execute(&plan, 2, &inputs[2]).await.is_err());
}

#[tokio::test]
async fn source_linked_nonce_allows_historical_mint_with_older_fee_and_expiry() {
    let h = Harness::new();
    let (transfer, inputs) = h.quote(true).await;
    let plan = h.authorize(h.plan(&inputs, true)).await;
    for (i, input) in inputs.iter().enumerate().take(2) {
        h.settle(h.execute(&plan, i, input).await.unwrap()).await;
    }
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
    let (first_transfer, first_inputs) = h.quote(false).await;
    let first_plan = h.authorize(h.plan(&first_inputs, false)).await;
    for (i, input) in first_inputs.iter().enumerate().take(2) {
        h.settle(h.execute(&first_plan, i, input).await.unwrap())
            .await;
    }
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
    let (transfer, inputs) = h.quote(false).await;
    let mut spec = h.plan(&inputs, false);
    spec.name = "Second identical CCTP burn".into();
    let plan = h.authorize_named(spec, "authorize-second-burn").await;
    for (i, input) in inputs.iter().enumerate().take(2) {
        let op = h
            .engine
            .execute_financial_plan_step(
                &plan,
                &format!("stage-{i}"),
                OperationRequest {
                    request_id: format!("second-burn-stage-{i}"),
                    operation: "bridge.transfer".into(),
                    input: input.clone(),
                    run_id: None,
                },
            )
            .await
            .unwrap();
        h.settle(op).await;
    }
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
    let (transfer, inputs) = h.quote(false).await;
    let plan = h.authorize(h.plan(&inputs, false)).await;
    h.settle(h.execute(&plan, 0, &inputs[0]).await.unwrap())
        .await;
    assert_eq!(
        h.chain.lock().unwrap().source_gas_balance,
        Some(U256::from(9_970_000_000_000_000u64))
    );
    h.settle(h.execute(&plan, 1, &inputs[1]).await.unwrap())
        .await;
    h.chain.lock().unwrap().attestation_ready = true;
    h.settle(h.execute(&plan, 2, &inputs[2]).await.unwrap())
        .await;
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn expired_authority_still_reconciles_already_submitted_mint() {
    let h = Harness::new();
    let (transfer, inputs) = h.quote(false).await;
    let mut request = FinancialPlanAuthorizationRequest {
        request_id: "short-pending-authority".into(),
        plan: h.plan(&inputs, false),
        valid_for_seconds: Some(600),
        replaces_revision_id: None,
        expected_review_hash: None,
    };
    let preview = h
        .engine
        .preview_financial_plan(request.clone())
        .await
        .unwrap();
    request.expected_review_hash = Some(preview.review_hash);
    let plan = h
        .engine
        .authorize_financial_plan(request)
        .await
        .unwrap()
        .plan_id;
    for (i, input) in inputs.iter().enumerate().take(2) {
        h.settle(h.execute(&plan, i, input).await.unwrap()).await;
    }
    {
        let mut state = h.chain.lock().unwrap();
        state.attestation_ready = true;
        state.hide_destination_receipt = true;
    }
    let operation = h.execute(&plan, 2, &inputs[2]).await.unwrap();
    assert!(operation.submission.is_some());
    h.clock.freeze();
    h.clock.advance(std::time::Duration::from_secs(601));
    let h = h.restart();
    assert_eq!(
        h.engine
            .financial_plan_status(&plan)
            .await
            .unwrap()
            .revision
            .status,
        gofin_domain::financial_plans::FinancialPlanAuthorizationStatus::Expired
    );
    h.clock.unfreeze();
    h.chain.lock().unwrap().hide_destination_receipt = false;
    h.settle(operation).await;
    assert_eq!(h.status(&transfer).await["state"], "completed");
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
    let (transfer, inputs) = h.quote(false).await;
    let plan = h.authorize(h.plan(&inputs, false)).await;
    h.settle(h.execute(&plan, 0, &inputs[0]).await.unwrap())
        .await;
    let before = h.state.transfer(&transfer).unwrap().unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(91)).await;
    // Preparation refreshes the original expired quote itself. The settled
    // approval stays settled and does not require another human authorization.
    h.settle(h.execute(&plan, 1, &inputs[1]).await.unwrap())
        .await;
    let refreshed = h.state.transfer(&transfer).unwrap().unwrap();
    assert_eq!(refreshed.steps[0], before.steps[0]);
    assert_eq!(
        refreshed
            .steps
            .iter()
            .map(|step| &step.step_id)
            .collect::<Vec<_>>(),
        before
            .steps
            .iter()
            .map(|step| &step.step_id)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        refreshed.quote_revisions.len(),
        before.quote_revisions.len() + 1
    );
    assert_eq!(refreshed.approved_bounds, before.approved_bounds);
    assert_eq!(refreshed.plan_scope, before.plan_scope);
    assert_eq!(refreshed.metadata["provider"], before.metadata["provider"]);
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
    h.chain.lock().unwrap().attestation_ready = true;
    h.settle(h.execute(&plan, 2, &inputs[2]).await.unwrap())
        .await;
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(1, 2), (42161, 1)])
    );
}

#[tokio::test]
async fn non_sorted_fee_caps_keep_exact_plan_scope_through_mint() {
    let h = Harness::new();
    let mut request = h.quote_request(true);
    request["bounds"]["fee_caps"]
        .as_array_mut()
        .unwrap()
        .swap(0, 1);
    request["bounds"]["input"] = json!({"minimum":"10000000","maximum":"10000000"});
    request["bounds"]["maximum_source_debit"] = json!("10000000");
    request["bounds"]["minimum_net_receipt"] =
        json!({"asset":usdc_asset(42161).unwrap().asset_id(),"atoms":"9995000"});
    let quoted = h.read("bridge.quotes", request.clone()).await;
    let transfer = quoted["transfers"][0]["transfer_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let links: Vec<BridgeStepLink> =
        serde_json::from_value(quoted["transfers"][0]["steps"].clone()).unwrap();
    let inputs=links.iter().map(|s|json!({"transferId":transfer,"stepId":s.step_id,"stepKind":s.kind,"bounds":request["bounds"]})).collect::<Vec<_>>();
    let plan = h.authorize(h.plan(&inputs, true)).await;
    for (i, input) in inputs.iter().enumerate().take(2) {
        h.settle(h.execute(&plan, i, input).await.unwrap()).await;
    }
    h.chain.lock().unwrap().attestation_ready = true;
    h.settle(h.execute(&plan, 2, &inputs[2]).await.unwrap())
        .await;
    assert_eq!(h.status(&transfer).await["state"], "completed");
}

#[tokio::test]
async fn rejected_mint_candidate_releases_plan_claim_atomically_and_preserves_original_error() {
    let h = Harness::new();
    let (transfer, inputs) = h.quote(false).await;
    let plan = h.authorize(h.plan(&inputs, false)).await;
    for (i, input) in inputs.iter().enumerate().take(2) {
        h.settle(h.execute(&plan, i, input).await.unwrap()).await;
    }
    let prior = h.state.transfer(&transfer).unwrap().unwrap();
    {
        let mut state = h.chain.lock().unwrap();
        state.attestation_ready = true;
        state.reject_mint_revalidation = true;
    }
    let failed = h.execute(&plan, 2, &inputs[2]).await.unwrap();
    assert_eq!(failed.status, Status::Failed);
    assert_eq!(failed.error.as_ref().unwrap().code, "RPC_REQUEST_FAILED");
    assert!(failed.submission.is_none());
    let saved = h.engine.status(&failed.id, true).await.unwrap();
    assert_eq!(saved.status, Status::Failed);
    let parent = h.state.transfer(&transfer).unwrap().unwrap();
    assert_eq!(parent.transit, prior.transit);
    assert_eq!(
        parent.steps[2].state,
        gofin_domain::BridgeStepState::Unsubmitted
    );
    assert!(parent.steps[2].operation_id.is_none());
    assert_eq!(h.chain.lock().unwrap().broadcasts, BTreeMap::from([(1, 2)]));
    h.chain.lock().unwrap().reject_mint_revalidation = false;
    let request = OperationRequest {
        request_id: "explicit-next-mint-candidate".into(),
        operation: "bridge.transfer".into(),
        input: inputs[2].clone(),
        run_id: None,
    };
    let recovered = h
        .engine
        .execute_financial_plan_step(&plan, "stage-2", request)
        .await
        .unwrap();
    h.settle(recovered).await;
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(1, 2), (42161, 1)])
    );
}

#[path = "support/cctp_solana_lifecycle.rs"]
mod solana;
