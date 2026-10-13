//! Explicit owned-fork contract evidence. Iris is replaced by a public synthetic
//! attester enabled only on the guarded destination forks. No live credentials.
use fin_bridge::{BridgeIntent, canonical::CanonicalRawLog, cctp::*};
use fin_domain::{FinError, Quantity, ResourceId, Result};
use fin_evm::alloy::{
    primitives::{U256, keccak256},
    signers::{SignerSync, local::PrivateKeySigner},
    sol,
    sol_types::SolCall,
};
use fin_evm::{EvmRpc, HttpRpc, address, hex_bytes, rpc_bytes, rpc_u64};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

const OWNER: &str = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266";
// Public Anvil key; valid only in disposable guarded local fixtures.
const ATTESTER_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
sol! {
    interface FixtureToken {
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address owner) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function decimals() external view returns (uint8);
    }
    interface FixtureTransmitter {
        function localDomain() external view returns (uint32);
        function attesterManager() external view returns (address);
        function enableAttester(address newAttester) external;
        function setSignatureThreshold(uint256 newSignatureThreshold) external;
        function usedNonces(bytes32 nonce) external view returns (uint256);
    }
}
fn blocked(message: &str) -> FinError {
    FinError::validation(message)
}

#[tokio::test]
#[ignore = "Public read used only by make test-cctp-fork to pin owned nodes"]
async fn public_cctp_fork_pin() -> Result<()> {
    let chain: u64 = std::env::var("FIN_CCTP_PIN_CHAIN_ID")
        .unwrap_or_default()
        .parse()
        .map_err(|_| blocked("Missing CCTP chain pin"))?;
    domain(chain)?;
    let network = fin_networks::by_chain_id(chain).ok_or_else(|| blocked("Unknown network"))?;
    let rpc = HttpRpc::new(
        network.rpc_urls.iter().map(|v| v.to_string()).collect(),
        chain,
    )?;
    assert_eq!(
        rpc_u64(&rpc.request("eth_chainId", json!([])).await?)?,
        chain
    );
    let height = rpc_u64(&rpc.request("eth_blockNumber", json!([])).await?)?
        .checked_sub(if chain == 1 { 32 } else { 128 })
        .ok_or_else(|| blocked("Missing stable chain head"))?;
    let block = rpc
        .request(
            "eth_getBlockByNumber",
            json!([format!("{height:#x}"), false]),
        )
        .await?;
    println!(
        "CCTP_FORK_PIN={}",
        json!({"chain_id":chain,"block":height,"hash":block["hash"],"timestamp":rpc_u64(&block["timestamp"])?})
    );
    Ok(())
}

struct Fork {
    endpoint: String,
    instance: String,
    pin: Value,
    client: reqwest::Client,
    ids: AtomicU64,
}
impl Fork {
    async fn new(config: &Value) -> Result<Self> {
        let endpoint = config["url"]
            .as_str()
            .ok_or_else(|| blocked("Missing owned fork URL"))?
            .to_owned();
        let port = endpoint
            .strip_prefix("http://127.0.0.1:")
            .and_then(|v| v.parse::<u16>().ok())
            .filter(|v| *v > 0)
            .ok_or_else(|| blocked("Exact loopback fork required"))?;
        if endpoint != format!("http://127.0.0.1:{port}") {
            return Err(blocked("Exact loopback fork required"));
        }
        let fork = Self {
            endpoint,
            instance: config["instance"]
                .as_str()
                .ok_or_else(|| blocked("Missing fork instance"))?
                .into(),
            pin: config["pin"].clone(),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|_| blocked("Fork client failed"))?,
            ids: AtomicU64::new(1),
        };
        fork.guard().await?;
        Ok(fork)
    }
    fn chain(&self) -> u64 {
        self.pin["chain_id"].as_u64().unwrap()
    }
    async fn raw(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.ids.fetch_add(1, Ordering::SeqCst);
        let response = self
            .client
            .post(&self.endpoint)
            .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .send()
            .await
            .map_err(|_| blocked("Owned fork transport failed"))?;
        let body: Value = response
            .json()
            .await
            .map_err(|_| blocked("Malformed owned fork response"))?;
        if body["id"] != id || body["jsonrpc"] != "2.0" || !body["error"].is_null() {
            return Err(blocked(&format!(
                "Owned fork {method} failed: {}",
                body["error"]
            )));
        }
        body.get("result")
            .cloned()
            .ok_or_else(|| blocked("Missing owned fork result"))
    }
    async fn guard(&self) -> Result<()> {
        let metadata = self.raw("anvil_metadata", json!([])).await?;
        if metadata["instanceId"] != self.instance
            || metadata["chainId"] != self.pin["chain_id"]
            || metadata["forkedNetwork"]["forkBlockNumber"] != self.pin["block"]
            || metadata["forkedNetwork"]["forkBlockHash"] != self.pin["hash"]
            || !metadata["clientVersion"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .starts_with("anvil/")
        {
            return Err(blocked("Owned fork identity changed"));
        }
        Ok(())
    }
    async fn mutate(&self, method: &str, params: Value) -> Result<Value> {
        self.guard().await?;
        self.raw(method, params).await
    }
    async fn call(&self, to: &str, data: Vec<u8>) -> Result<Vec<u8>> {
        Ok(rpc_bytes(
            &self
                .raw(
                    "eth_call",
                    json!([{"to":to,"data":hex_bytes(&data)},"latest"]),
                )
                .await?,
        )?
        .to_vec())
    }
    async fn send(&self, from: &str, to: &str, data: &str) -> Result<Value> {
        let hash = self
            .mutate(
                "eth_sendTransaction",
                json!([{"from":from,"to":to,"data":data,"gas":"0x1e8480"}]),
            )
            .await?;
        let mut receipt = Value::Null;
        for _ in 0..100 {
            receipt = self.raw("eth_getTransactionReceipt", json!([hash])).await?;
            if !receipt.is_null() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if receipt.is_null() {
            return Err(blocked(&format!(
                "Owned fork receipt remains pending: {hash}"
            )));
        }
        if rpc_u64(&receipt["status"])? != 1 {
            return Err(blocked(&format!("Owned fork contract reverted: {hash}")));
        }
        Ok(receipt)
    }
    async fn balance(&self, token: &str) -> Result<U256> {
        Ok(U256::from_be_slice(
            &self
                .call(
                    token,
                    FixtureToken::balanceOfCall {
                        owner: address(OWNER)?,
                    }
                    .abi_encode(),
                )
                .await?,
        ))
    }
    async fn fund_usdc(&self, token: &str) -> Result<()> {
        let amount = U256::from(100_000_000u64);
        for slot in 0..128u64 {
            let encoded = format!("{:0>64}{slot:064x}", &OWNER[2..]);
            let key = format!("{:#x}", keccak256(hex::decode(encoded).unwrap()));
            let previous = self
                .raw("eth_getStorageAt", json!([token, key, "latest"]))
                .await?;
            self.mutate(
                "anvil_setStorageAt",
                json!([token, key, format!("0x{amount:064x}")]),
            )
            .await?;
            if self.balance(token).await? == amount {
                return Ok(());
            }
            self.mutate("anvil_setStorageAt", json!([token, key, previous]))
                .await?;
        }
        Err(blocked("Could not locate isolated token balance slot"))
    }
    async fn synthetic_attester(&self, signer: &PrivateKeySigner) -> Result<()> {
        let manager = self
            .call(
                MESSAGE_TRANSMITTER,
                FixtureTransmitter::attesterManagerCall {}.abi_encode(),
            )
            .await?;
        let manager = format!(
            "{:#x}",
            fin_evm::alloy::primitives::Address::from_slice(&manager[12..])
        );
        self.mutate("anvil_impersonateAccount", json!([manager]))
            .await?;
        self.mutate("anvil_setBalance", json!([manager, "0xde0b6b3a7640000"]))
            .await?;
        self.send(
            &manager,
            MESSAGE_TRANSMITTER,
            &hex_bytes(
                &FixtureTransmitter::enableAttesterCall {
                    newAttester: signer.address(),
                }
                .abi_encode(),
            ),
        )
        .await?;
        self.send(
            &manager,
            MESSAGE_TRANSMITTER,
            &hex_bytes(
                &FixtureTransmitter::setSignatureThresholdCall {
                    newSignatureThreshold: U256::from(1),
                }
                .abi_encode(),
            ),
        )
        .await?;
        self.mutate("anvil_stopImpersonatingAccount", json!([manager]))
            .await?;
        Ok(())
    }
}
fn logs(receipt: &Value) -> Result<Vec<CanonicalRawLog>> {
    receipt["logs"]
        .as_array()
        .ok_or_else(|| blocked("Missing receipt logs"))?
        .iter()
        .map(|log| {
            Ok(CanonicalRawLog {
                log_index: u32::try_from(rpc_u64(&log["logIndex"])?)
                    .map_err(|_| blocked("Log index overflow"))?,
                address: log["address"].as_str().unwrap().into(),
                topics: log["topics"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().into())
                    .collect(),
                data: log["data"].as_str().unwrap().into(),
            })
        })
        .collect()
}

#[tokio::test]
#[ignore = "Explicit owned source/destination forks with synthetic attester; use make test-cctp-fork"]
async fn deployed_cctp_burn_and_mint_bind_each_supported_destination() -> Result<()> {
    let configurations: Vec<Value> = serde_json::from_str(
        &std::env::var("FIN_CCTP_FORKS").map_err(|_| blocked("Use owned CCTP fork runner"))?,
    )
    .map_err(|_| blocked("Malformed fork list"))?;
    let mut forks = BTreeMap::new();
    for configuration in configurations {
        let fork = Fork::new(&configuration).await?;
        forks.insert(fork.chain(), fork);
    }
    assert_eq!(
        forks.keys().copied().collect::<Vec<_>>(),
        vec![1, 8453, 42161]
    );
    let selected = std::env::var("FIN_CCTP_SOURCE_CHAIN")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    let signer: PrivateKeySigner = ATTESTER_KEY.parse().unwrap();
    for fork in forks.values() {
        let token = usdc_asset(fork.chain())?.token.unwrap();
        assert_eq!(
            U256::from_be_slice(
                &fork
                    .call(&token, FixtureToken::decimalsCall {}.abi_encode())
                    .await?
            ),
            U256::from(6)
        );
        assert_eq!(
            U256::from_be_slice(
                &fork
                    .call(
                        MESSAGE_TRANSMITTER,
                        FixtureTransmitter::localDomainCall {}.abi_encode()
                    )
                    .await?
            ),
            U256::from(domain(fork.chain())?)
        );
        fork.fund_usdc(&token).await?;
        fork.synthetic_attester(&signer).await?;
    }
    let mut sequence = 0u64;
    for (&source_chain, source) in &forks {
        if selected.is_some_and(|chain| chain != source_chain) {
            continue;
        }
        for (&destination_chain, destination) in &forks {
            if source_chain == destination_chain {
                continue;
            }
            for transfer_mode in [CctpTransferMode::Standard, CctpTransferMode::Fast] {
                sequence += 1;
                let token = usdc_asset(source_chain)?.token.unwrap();
                let destination_token = usdc_asset(destination_chain)?.token.unwrap();
                let intent = BridgeIntent {
                    wallet: ResourceId(format!("ethereum:{source_chain}:{OWNER}")),
                    source_address: OWNER.into(),
                    source: usdc_asset(source_chain)?,
                    destination_chain_id: destination_chain,
                    destination_assets: vec![usdc_asset(destination_chain)?],
                    recipient: OWNER.into(),
                    refund_to: OWNER.into(),
                    allowed_refund_assets: vec![],
                    amount: Quantity::from_u64(5_000_000),
                };
                let metadata = CctpQuoteMetadata {
                    protocol_version: 2,
                    source_domain: domain(source_chain)?,
                    destination_domain: domain(destination_chain)?,
                    transfer_mode,
                    max_fee: Quantity::from_u64(100_000),
                    min_finality_threshold: transfer_mode.threshold(),
                };
                let before = source.balance(&token).await?;
                source
                    .send(
                        OWNER,
                        &token,
                        &hex_bytes(
                            &FixtureToken::approveCall {
                                spender: address(TOKEN_MESSENGER)?,
                                amount: U256::from(5_000_000),
                            }
                            .abi_encode(),
                        ),
                    )
                    .await?;
                let burn = burn_call(&intent, &metadata)?;
                let receipt = source.send(OWNER, &burn.to, &burn.data).await?;
                let source_message = verify_source_logs(&logs(&receipt)?, &intent, &metadata)?;
                assert_eq!(
                    before - source.balance(&token).await?,
                    U256::from(5_000_000)
                );
                assert_eq!(
                    U256::from_be_slice(
                        &source
                            .call(
                                &token,
                                FixtureToken::allowanceCall {
                                    owner: address(OWNER)?,
                                    spender: address(TOKEN_MESSENGER)?
                                }
                                .abi_encode()
                            )
                            .await?
                    ),
                    U256::ZERO
                );
                let mut message = rpc_bytes(&json!(source_message.raw_message))?.to_vec();
                message[12..44].copy_from_slice(
                    keccak256(format!("Fin public synthetic fork nonce {sequence}")).as_slice(),
                );
                message[144..148].copy_from_slice(&transfer_mode.threshold().to_be_bytes());
                // Mock Iris fee/finality assignment, preserving every immutable source field.
                message[312..344].copy_from_slice(&U256::from(10_000).to_be_bytes::<32>());
                let encoded = hex_bytes(&message);
                let decoded =
                    validate_attested_message(&source_message.raw_message, &metadata, &encoded)?;
                let signature = signer
                    .sign_hash_sync(&keccak256(&message))
                    .map_err(|_| blocked("Synthetic attester signature failed"))?;
                let attestation = hex_bytes(&signature.as_bytes());
                let mint = mint_call(destination_chain, &encoded, &attestation)?;
                let before = destination.balance(&destination_token).await?;
                let receipt = destination.send(OWNER, &mint.to, &mint.data).await?;
                verify_destination_logs(&logs(&receipt)?, &encoded)?;
                assert_eq!(
                    destination.balance(&destination_token).await? - before,
                    U256::from(4_990_000)
                );
                let used = destination
                    .call(
                        MESSAGE_TRANSMITTER,
                        FixtureTransmitter::usedNoncesCall {
                            nonce: decoded.nonce.parse().unwrap(),
                        }
                        .abi_encode(),
                    )
                    .await?;
                assert_eq!(U256::from_be_slice(&used), U256::from(1));
                assert!(
                    destination
                        .raw(
                            "eth_call",
                            json!([{"from":OWNER,"to":mint.to,"data":mint.data},"latest"])
                        )
                        .await
                        .is_err()
                );
                println!(
                    "CCTP_FORK_VERIFIED source={source_chain} destination={destination_chain} mode={transfer_mode:?} burn=5000000 fee=10000 mint=4990000 duplicate_rejected=true synthetic_attester=true"
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn fork_mutations_cannot_target_remote_or_ambiguous_endpoints() {
    for endpoint in [
        "https://ethereum-rpc.publicnode.com",
        "http://localhost:8545",
        "http://127.0.0.1:8545/",
        "http://127.0.0.1:0",
        "http://127.0.0.1:8545?remote=true",
    ] {
        assert!(
            Fork::new(&json!({"url":endpoint,"instance":"unused","pin":{}}))
                .await
                .is_err()
        );
    }
}
