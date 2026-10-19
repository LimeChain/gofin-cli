//! Controlled protocol state and RPC for broker tests. Native Kamino codecs have separate tests.
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use bincode::Options;
use gofin_domain::*;
use gofin_jupiter::{
    JupiterAdapter,
    api::{JupiterApi, JupiterHost, RateBucket, RequestPriority},
};
use gofin_kamino::{KaminoAdapter, protocol::*};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use gofin_solana::{
    FinalizedReceiptProof, NormalizedInstruction, SolanaRpc, SolanaRuntime, SolanaSigner,
    SolanaWalletAdapter, transaction::NormalizedAccountMeta,
};
use serde_json::{Value, json};
use solana_transaction::versioned::VersionedTransaction;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
pub const INPUT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
pub const UNDERLYING: &str = "So11111111111111111111111111111111111111112";
pub const TOKEN: &str = gofin_solana::token::TOKEN_PROGRAM_ID;
const FEE: u64 = 5000;
pub const ACCOUNT_RENT: u64 = 2_039_280;
const JUPITER: &str = gofin_jupiter::swap::JUPITER_V6_PROGRAM;
fn key(n: u8) -> String {
    bs58::encode([n; 32]).into_string()
}
fn history_hash(height: u64) -> String {
    if height == 850 {
        return key(42);
    }
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&height.to_le_bytes());
    bs58::encode(bytes).into_string()
}
pub fn vault(family: VaultFamily) -> String {
    key(if family == VaultFamily::Lending {
        70
    } else {
        71
    })
}
pub fn share(family: VaultFamily) -> String {
    key(if family == VaultFamily::Lending {
        77
    } else {
        78
    })
}
fn token(mint: &str) -> TokenInfo {
    TokenInfo {
        mint: mint.into(),
        decimals: if mint == UNDERLYING { 9 } else { 6 },
        token_program: TOKEN.into(),
    }
}
pub fn asset(mint: &str) -> AssetId {
    let t = token(mint);
    AssetId {
        network: gofin_solana::NETWORK.into(),
        reference: t.mint,
        decimals: t.decimals,
    }
}
pub fn native() -> AssetId {
    AssetId {
        network: gofin_solana::NETWORK.into(),
        reference: "native".into(),
        decimals: 9,
    }
}
fn ata(owner: &str, mint: &str) -> String {
    gofin_solana::associated_token_address(owner, mint, TOKEN).unwrap()
}
#[derive(Default)]
pub struct Prices;
#[async_trait]
impl UsdPriceSource for Prices {
    async fn quote(&self, _: &AssetId) -> Result<UsdQuote> {
        Ok(UsdQuote {
            usd_per_unit: "1".into(),
            updated_at: now(),
            observed_at: now(),
            source: "controlled_kamino_test".into(),
        })
    }
}
struct State {
    balances: BTreeMap<String, u64>,
    lamports: u64,
    transactions: BTreeMap<String, Value>,
    missing_accounts: BTreeSet<String>,
}
pub struct Chain {
    pub owner: String,
    state: Mutex<State>,
    pub send_count: AtomicUsize,
    pub hide_receipts: AtomicBool,
    pub lose_response: AtomicBool,
    pub reject_deposit: AtomicBool,
    pub inventory_unavailable: AtomicBool,
    /// Simulate the lifetime guard expiring only after the real engine journal
    /// has durably admitted the immutable signed transaction.
    pub expire_after_submission: Mutex<Option<std::path::PathBuf>>,
    pub drop_before_landing: AtomicBool,
    pub finalized_height: AtomicU64,
    pub block_history_available: AtomicBool,
}
impl Chain {
    pub fn new() -> (Arc<Self>, Arc<SolanaWalletAdapter>) {
        let signer=Arc::new(SolanaSigner::parse("99eUso3aSbE9tqGSTXzo3TLfKb9RkMTURrHKQ1K7Zh3StnzFNUx8FKCPPPPpR479qsw5zv2WNBKmgiz7WqgAJfM").unwrap());
        let chain = Arc::new(Self {
            owner: signer.address().into(),
            state: Mutex::new(State {
                balances: BTreeMap::from([
                    (INPUT.into(), 1000),
                    (UNDERLYING.into(), 0),
                    (share(VaultFamily::Lending), 0),
                    (share(VaultFamily::Liquidity), 0),
                ]),
                lamports: 1_000_000_000,
                transactions: BTreeMap::new(),
                missing_accounts: BTreeSet::new(),
            }),
            send_count: AtomicUsize::new(0),
            hide_receipts: AtomicBool::new(false),
            lose_response: AtomicBool::new(false),
            reject_deposit: AtomicBool::new(false),
            inventory_unavailable: AtomicBool::new(false),
            expire_after_submission: Mutex::new(None),
            drop_before_landing: AtomicBool::new(false),
            finalized_height: AtomicU64::new(10),
            block_history_available: AtomicBool::new(false),
        });
        let wallet = Arc::new(
            SolanaWalletAdapter::new(
                "wallet",
                signer.address(),
                Some(signer.clone()),
                chain.clone(),
                Arc::new(Prices),
            )
            .unwrap(),
        );
        (chain, wallet)
    }
    pub fn balance(&self, mint: &str) -> u64 {
        *self.state.lock().unwrap().balances.get(mint).unwrap()
    }
    pub fn seed_balance(&self, mint: &str, atoms: u64) {
        assert_eq!(self.send_count.load(Ordering::SeqCst), 0);
        *self.state.lock().unwrap().balances.get_mut(mint).unwrap() = atoms;
    }
    pub fn resource(&self) -> ResourceId {
        ResourceId(format!("solana:mainnet-beta:{}", self.owner))
    }
    pub fn remove_empty_share_account(&self, family: VaultFamily) -> String {
        assert_eq!(self.send_count.load(Ordering::SeqCst), 0);
        assert_eq!(self.balance(&share(family)), 0);
        let account = ata(&self.owner, &share(family));
        self.state
            .lock()
            .unwrap()
            .missing_accounts
            .insert(account.clone());
        account
    }
    fn missing_share_account(&self, family: VaultFamily) -> bool {
        self.state
            .lock()
            .unwrap()
            .missing_accounts
            .contains(&ata(&self.owner, &share(family)))
    }
    pub fn lamports(&self) -> u64 {
        self.state.lock().unwrap().lamports
    }
    fn account(&self, s: &State, address: &str) -> Value {
        if s.missing_accounts.contains(address) {
            return Value::Null;
        }
        if let Some((mint, balance)) = s
            .balances
            .iter()
            .find(|(mint, _)| ata(&self.owner, mint) == address)
        {
            let mut b = vec![0u8; 165];
            b[..32].copy_from_slice(&bs58::decode(mint).into_vec().unwrap());
            b[32..64].copy_from_slice(&bs58::decode(&self.owner).into_vec().unwrap());
            b[64..72].copy_from_slice(&balance.to_le_bytes());
            b[108] = 1;
            return json!({"owner":TOKEN,"data":[STANDARD.encode(b),"base64"],"lamports":2039280,"executable":false,"rentEpoch":0});
        }
        if s.balances.contains_key(address) {
            let mut b = vec![0u8; 82];
            b[36..44].copy_from_slice(&1_000_000_000u64.to_le_bytes());
            b[44] = token(address).decimals;
            b[45] = 1;
            return json!({"owner":TOKEN,"data":[STANDARD.encode(b),"base64"],"lamports":2039280,"executable":false,"rentEpoch":0});
        }
        Value::Null
    }
    fn apply(&self, s: &mut State, wire: &str) -> Result<String> {
        let bytes = STANDARD.decode(wire).unwrap();
        let tx: VersionedTransaction = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .reject_trailing_bytes()
            .deserialize(&bytes)
            .unwrap();
        let keys: Vec<_> = tx
            .message
            .static_account_keys()
            .iter()
            .map(ToString::to_string)
            .collect();
        let signature = tx.signatures[0].to_string();
        let before = s.balances.clone();
        let absent_before = s.missing_accounts.clone();
        let mut rent = 0;
        for ix in tx.message.instructions() {
            let program = &keys[ix.program_id_index as usize];
            if program == JUPITER {
                let account = |i: usize| &keys[ix.accounts[i] as usize];
                let source = before
                    .keys()
                    .find(|m| ata(&self.owner, m) == *account(2))
                    .unwrap();
                let destination = before
                    .keys()
                    .find(|m| ata(&self.owner, m) == *account(3))
                    .unwrap();
                let n = u64::from_le_bytes(
                    ix.data[ix.data.len() - 19..ix.data.len() - 11]
                        .try_into()
                        .unwrap(),
                );
                let output = if source == INPUT { 125 } else { 65 };
                *s.balances.get_mut(source).unwrap() -= n;
                *s.balances.get_mut(destination).unwrap() += output;
            } else {
                let intent: VaultIntent = serde_json::from_slice(&ix.data).unwrap();
                let shares = share(intent.family);
                if intent.action == VaultAction::Deposit {
                    if s.missing_accounts.remove(&ata(&self.owner, &shares)) {
                        rent += ACCOUNT_RENT;
                    }
                    for row in intent.amounts {
                        *s.balances.get_mut(&row.token.mint).unwrap() -=
                            row.atoms.unwrap().parse::<u64>().unwrap();
                    }
                    *s.balances.get_mut(&shares).unwrap() += 10;
                } else {
                    let n = intent.shares_atoms.unwrap().parse::<u64>().unwrap();
                    *s.balances.get_mut(&shares).unwrap() -= n;
                    *s.balances.get_mut(UNDERLYING).unwrap() += n * 10;
                    if intent.family == VaultFamily::Liquidity {
                        *s.balances.get_mut(INPUT).unwrap() += n * 5;
                    }
                }
            }
        }
        let mut pre = vec![0u64; keys.len()];
        let mut post = pre.clone();
        pre[0] = s.lamports;
        s.lamports -= FEE + rent;
        post[0] = s.lamports;
        let mut pre_tokens = Vec::new();
        let mut post_tokens = Vec::new();
        for (mint, amount) in before {
            if let Some(i) = keys.iter().position(|k| *k == ata(&self.owner, &mint)) {
                pre[i] = if absent_before.contains(&keys[i]) {
                    0
                } else {
                    ACCOUNT_RENT
                };
                post[i] = ACCOUNT_RENT;
                let row = |n: u64| json!({"accountIndex":i,"mint":mint,"owner":self.owner,"programId":TOKEN,"uiTokenAmount":{"amount":n.to_string(),"decimals":token(&mint).decimals}});
                if !absent_before.contains(&keys[i]) {
                    pre_tokens.push(row(amount));
                }
                post_tokens.push(row(s.balances[&mint]));
            }
        }
        s.transactions.insert(signature.clone(),json!({"slot":10,"blockTime":chrono::Utc::now().timestamp(),"transaction":[wire,"base64"],"meta":{"err":null,"fee":FEE,"preBalances":pre,"postBalances":post,"preTokenBalances":pre_tokens,"postTokenBalances":post_tokens,"loadedAddresses":{"writable":[],"readonly":[]},"innerInstructions":[],"logMessages":[]}}));
        Ok(signature)
    }
}
#[async_trait]
impl SolanaRpc for Chain {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let mut s = self.state.lock().unwrap();
        let height = self.finalized_height.load(Ordering::SeqCst);
        let observed_slot = if height >= 850 { height + 150 } else { 10 };
        Ok(match method {
            "getGenesisHash" => json!(gofin_solana::MAINNET_GENESIS_HASH),
            "getSlot" => json!(observed_slot),
            "getBlockTime" => json!(chrono::Utc::now().timestamp()),
            "getBlockHeight" => {
                let expired = self
                    .expire_after_submission
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|path| {
                        gofin_store::Store::open(path)
                            .unwrap()
                            .list_records()
                            .unwrap()
                            .iter()
                            .any(|record| !record["submission"].is_null())
                    });
                json!(if expired {
                    1001
                } else {
                    self.finalized_height.load(Ordering::SeqCst)
                })
            }
            "getEpochInfo" => {
                json!({"epoch":1,"absoluteSlot":10,"slotIndex":10,"slotsInEpoch":432000})
            }
            "getLatestBlockhash" => {
                let height = self.finalized_height.load(Ordering::SeqCst);
                if height > 1000 {
                    json!({"context":{"slot":observed_slot},"value":{"blockhash":history_hash(height),"lastValidBlockHeight":height+150}})
                } else {
                    json!({"context":{"slot":observed_slot},"value":{"blockhash":key(42),"lastValidBlockHeight":1000}})
                }
            }
            "isBlockhashValid" => {
                json!({"context":{"slot":if self.finalized_height.load(Ordering::SeqCst)>1000 {1151}else{10}},"value":self.finalized_height.load(Ordering::SeqCst)<=1000})
            }
            "getBlocks" => json!(
                (1000u64..=1151)
                    .filter(|slot| *slot >= params[0].as_u64().unwrap()
                        && *slot <= params[1].as_u64().unwrap())
                    .collect::<Vec<_>>()
            ),
            "getBlock" => {
                let slot = params[0].as_u64().unwrap();
                assert!(
                    slot == 10 || (1000..=1151).contains(&slot),
                    "Unbounded block history request: {params}"
                );
                if params[1]["transactionDetails"] == "signatures"
                    && !self.block_history_available.load(Ordering::SeqCst)
                {
                    return Err(GoFinError::new(
                        "SOLANA_RPC_UNAVAILABLE",
                        "rpc",
                        "Synthetic block history is unavailable.",
                        "Retain the original reservation.",
                    ));
                }
                let height = if slot == 10 { 10 } else { slot - 150 };
                json!({"blockhash":history_hash(height),"previousBlockhash":history_hash(height-1),"parentSlot":slot-1,"blockHeight":height,"signatures":[]})
            }
            "getFeeForMessage" => json!({"context":{"slot":observed_slot},"value":FEE}),
            "getBalance" => json!({"context":{"slot":observed_slot},"value":s.lamports}),
            "getAccountInfo" => {
                json!({"context":{"slot":observed_slot},"value":self.account(&s,params[0].as_str().unwrap())})
            }
            "getMultipleAccounts" => {
                json!({"context":{"slot":observed_slot},"value":params[0].as_array().unwrap().iter().map(|v|self.account(&s,v.as_str().unwrap())).collect::<Vec<_>>()})
            }
            "getTokenAccountsByOwner" => {
                let filter = &params[1];
                let rows=s.balances.iter().filter(|(mint,_)|!s.missing_accounts.contains(&ata(&self.owner,mint))).filter(|(mint,_)|filter["mint"].as_str().is_none_or(|m|m==mint.as_str())&&filter["programId"].as_str().is_none_or(|p|p==TOKEN)).map(|(mint,_)|json!({"pubkey":ata(&self.owner,mint),"account":self.account(&s,&ata(&self.owner,mint))})).collect::<Vec<_>>();
                json!({"context":{"slot":observed_slot},"value":rows})
            }
            "simulateTransaction" => {
                json!({"context":{"slot":observed_slot},"value":{"err":null,"unitsConsumed":10000}})
            }
            "sendTransaction" => {
                self.send_count.fetch_add(1, Ordering::SeqCst);
                if self.drop_before_landing.load(Ordering::SeqCst) {
                    return Err(GoFinError::new(
                        "CONTROLLED_RESPONSE_LOST",
                        "submit",
                        "The synthetic transport accepted the send but never landed the transaction.",
                        "Reconcile original submission.",
                    ));
                }
                let signature = self.apply(&mut s, params[0].as_str().unwrap())?;
                if self.lose_response.load(Ordering::SeqCst) {
                    return Err(GoFinError::new(
                        "CONTROLLED_RESPONSE_LOST",
                        "submit",
                        "Synthetic transport response lost.",
                        "Reconcile original submission.",
                    ));
                }
                json!(signature)
            }
            "getSignatureStatuses" => {
                let sig = params[0][0].as_str().unwrap();
                let slot = if self.finalized_height.load(Ordering::SeqCst) > 1000 {
                    1151
                } else {
                    10
                };
                if self.hide_receipts.load(Ordering::SeqCst) || !s.transactions.contains_key(sig) {
                    json!({"context":{"slot":slot},"value":[null]})
                } else {
                    json!({"context":{"slot":slot},"value":[{"slot":10,"err":null,"confirmationStatus":"finalized","confirmations":null}]})
                }
            }
            "getTransaction" => {
                if self.hide_receipts.load(Ordering::SeqCst) {
                    Value::Null
                } else {
                    s.transactions
                        .get(params[0].as_str().unwrap())
                        .cloned()
                        .unwrap_or(Value::Null)
                }
            }
            other => {
                return Err(GoFinError::validation(format!(
                    "Unexpected controlled RPC {other}"
                )));
            }
        })
    }
}

/// Independent loopback endpoints for exercising the production HTTP pool's
/// genesis checks and corroborated finalized block reads. No upstream exists.
pub struct RpcEndpoint {
    pub url: String,
    pub signature_blocks: Arc<AtomicUsize>,
    pub history_available: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}
impl RpcEndpoint {
    pub async fn new(chain: Arc<Chain>) -> Self {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let signature_blocks = Arc::new(AtomicUsize::new(0));
        let count = signature_blocks.clone();
        let history_available = Arc::new(AtomicBool::new(true));
        let available = history_available.clone();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if !matches!(reader.read_line(&mut line).await,Ok(n) if n>0) {
                    continue;
                }
                let mut length = None;
                loop {
                    line.clear();
                    if !matches!(reader.read_line(&mut line).await,Ok(n) if n>0) {
                        break;
                    }
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse::<usize>().ok();
                    }
                }
                let Some(length) = length.filter(|n| *n < 65536) else {
                    continue;
                };
                let mut bytes = vec![0; length];
                if reader.read_exact(&mut bytes).await.is_err() {
                    continue;
                }
                let request: Value = serde_json::from_slice(&bytes).unwrap();
                let method = request["method"].as_str().unwrap();
                if method == "getBlock"
                    && request["params"][1]["transactionDetails"] == "signatures"
                {
                    count.fetch_add(1, Ordering::SeqCst);
                }
                let result = if method == "getBlock"
                    && request["params"][1]["transactionDetails"] == "signatures"
                    && !available.load(Ordering::SeqCst)
                {
                    Err(GoFinError::new(
                        "SOLANA_RPC_UNAVAILABLE",
                        "rpc",
                        "Synthetic second endpoint has no block history.",
                        "Retain the reservation.",
                    ))
                } else {
                    chain.request(method, request["params"].clone()).await
                };
                let body=match result {
                    Ok(value)=>json!({"jsonrpc":"2.0","id":request["id"],"result":value}),
                    Err(error)=>json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":if method=="getBlock" {-32004}else{-32000},"message":error.message}}),
                }.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = reader.get_mut().write_all(response.as_bytes()).await;
            }
        });
        Self {
            url,
            signature_blocks,
            history_available,
            task,
        }
    }
}
impl Drop for RpcEndpoint {
    fn drop(&mut self) {
        self.task.abort();
    }
}
struct NativeProtocol {
    chain: Arc<Chain>,
    family: VaultFamily,
}
impl NativeProtocol {
    fn vault(&self) -> VaultInfo {
        VaultInfo {
            family: self.family,
            address: vault(self.family),
            program: key(69),
            name: Some("Controlled native vault".into()),
            backend: if self.family == VaultFamily::Liquidity {
                Some("orca".into())
            } else {
                None
            },
            assets: if self.family == VaultFamily::Lending {
                vec![token(UNDERLYING)]
            } else {
                vec![token(UNDERLYING), token(INPUT)]
            },
            shares: token(&share(self.family)),
            farm: None,
            deposits_available: true,
            withdrawals_available: true,
            restrictions: vec![],
            observed_slot: Some(10),
            observed_at: now(),
            metrics: json!({}),
            protocol_state: json!({"fixture":true}),
        }
    }
    fn position(&self) -> PositionInfo {
        let v = self.vault();
        let n = self.chain.balance(&share(self.family));
        PositionInfo {
            underlying: v
                .assets
                .iter()
                .map(|t| TokenAmount {
                    token: t.clone(),
                    atoms: Some((n * 10).to_string()),
                })
                .collect(),
            vault: v,
            owner: self.chain.owner.clone(),
            wallet_shares_atoms: n.to_string(),
            staked_shares_atoms: "0".into(),
            pending_shares_atoms: "0".into(),
            rewards: vec![],
            withdrawable_shares_atoms: Some(n.to_string()),
            complete: true,
            warnings: vec![],
            protocol_state: json!({}),
        }
    }
}
#[async_trait]
impl KaminoProtocol for NativeProtocol {
    fn family(&self) -> VaultFamily {
        self.family
    }
    async fn vaults(&self, _: &SolanaRuntime, _: &VaultQuery) -> Result<VaultPage> {
        Ok(VaultPage {
            vaults: vec![self.vault()],
            complete: true,
            next_cursor: None,
            warnings: vec![],
        })
    }
    async fn vault(&self, _: &SolanaRuntime, _: &str) -> Result<VaultInfo> {
        Ok(self.vault())
    }
    async fn positions(&self, _: &SolanaRuntime, _: &str) -> Result<PositionInventory> {
        if self.chain.inventory_unavailable.load(Ordering::SeqCst) {
            return Err(GoFinError::validation(
                "Controlled unrelated vault inventory is unavailable.",
            ));
        }
        Ok(PositionInventory {
            positions: vec![self.position()],
            complete: true,
            observed_at: now(),
            warnings: vec![],
        })
    }
    async fn position(&self, _: &SolanaRuntime, _: &str, _: &str) -> Result<PositionInfo> {
        Ok(self.position())
    }
    async fn build_candidate(
        &self,
        _: &SolanaRuntime,
        intent: &VaultIntent,
        owner: &str,
    ) -> Result<VaultCandidate> {
        if self.chain.reject_deposit.load(Ordering::SeqCst) && intent.action == VaultAction::Deposit
        {
            return Err(GoFinError::validation(
                "Controlled native deposit became unavailable after preceding swap.",
            ));
        }
        let v = self.vault();
        let meta = |address: String, signer: bool, writable: bool| NormalizedAccountMeta {
            address,
            signer,
            writable,
        };
        let mut accounts = vec![meta(owner.into(), true, true)];
        for t in v.assets.iter().chain(std::iter::once(&v.shares)) {
            accounts.push(meta(ata(owner, &t.mint), false, true));
        }
        let shares = intent.shares_atoms.clone().unwrap_or_else(|| "10".into());
        let rent = if self.chain.missing_share_account(self.family) {
            ACCOUNT_RENT
        } else {
            0
        };
        let credits = if intent.action == VaultAction::Withdraw {
            v.assets
                .iter()
                .map(|t| TokenAmount {
                    token: t.clone(),
                    atoms: Some(
                        (shares.parse::<u64>().unwrap() * if t.mint == INPUT { 5 } else { 10 })
                            .to_string(),
                    ),
                })
                .collect()
        } else if self.family == VaultFamily::Lending {
            vec![TokenAmount {
                token: v.shares.clone(),
                atoms: Some(shares.clone()),
            }]
        } else {
            vec![]
        };
        let debits = if intent.action == VaultAction::Deposit {
            intent.amounts.clone()
        } else if self.family == VaultFamily::Lending {
            vec![TokenAmount {
                token: v.shares.clone(),
                atoms: Some(shares.clone()),
            }]
        } else {
            vec![]
        };
        Ok(VaultCandidate {
            vault: v,
            quote: VaultQuote {
                debits,
                credits,
                shares_atoms: shares,
                minimum_shares_enforced: true,
                protocol_fees: vec![],
                rent_lamports: rent,
                network_fee_lamports: FEE,
                expires_at: (chrono::Utc::now() + chrono::Duration::seconds(90)).to_rfc3339(),
                warnings: vec![],
                details: json!({"controlled_protocol":true}),
            },
            instructions: vec![NormalizedInstruction {
                program_id: key(69),
                accounts,
                data_base64: STANDARD.encode(serde_json::to_vec(intent).unwrap()),
            }],
            lookup_tables: vec![],
            blockhash: key(42),
            last_valid_block_height: 1000,
            metadata: json!({"position_before":self.position(),"position_shares_before_atoms":self.chain.balance(&share(self.family)).to_string(),"refundable_rent_accounts":if rent>0 {vec![json!({"account":ata(owner,&share(self.family)),"maximum_lamports":rent})]}else{vec![]}}),
        })
    }
    async fn validate_candidate(
        &self,
        _: &SolanaRuntime,
        intent: &VaultIntent,
        _: &str,
        c: &VaultCandidate,
    ) -> Result<()> {
        let decoded: VaultIntent =
            serde_json::from_slice(&STANDARD.decode(&c.instructions[0].data_base64).unwrap())
                .unwrap();
        if &decoded != intent {
            return Err(GoFinError::validation(
                "Changed controlled vault instruction.",
            ));
        }
        Ok(())
    }
    async fn verify_finalized(
        &self,
        _: &SolanaRuntime,
        _: &VaultIntent,
        owner: &str,
        c: &VaultCandidate,
        p: &FinalizedReceiptProof,
    ) -> Result<VerifiedVaultEffects> {
        Ok(VerifiedVaultEffects {
            native_shares_delta: None,
            token_deltas: c
                .vault
                .assets
                .iter()
                .map(|t| {
                    Ok(VerifiedTokenDelta {
                        token: t.clone(),
                        atoms: p.token_delta_verified(
                            owner,
                            &t.mint,
                            &t.token_program,
                            t.decimals,
                        )?,
                    })
                })
                .collect::<Result<_>>()?,
            shares_delta: p.token_delta_verified(owner, &c.vault.shares.mint, TOKEN, 6)?,
            rent_lamports_delta: SignedQuantity::new(
                c.metadata["refundable_rent_accounts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .try_fold(num_bigint::BigInt::from(0), |total, row| {
                        p.lamport_delta(row["account"].as_str().unwrap())
                            .map(|delta| total + delta.integer())
                    })?
                    .to_string(),
            )?,
            rent_accounts: c.metadata["refundable_rent_accounts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| {
                    let account = row["account"].as_str().unwrap();
                    Ok(VerifiedRentAccountDelta {
                        account: account.into(),
                        atoms: p.lamport_delta(account)?,
                    })
                })
                .collect::<Result<_>>()?,
            position: Some(self.position()),
            evidence: json!({"controlled_native_protocol":true}),
        })
    }
}
pub fn kamino(chain: Arc<Chain>, wallet: Arc<SolanaWalletAdapter>) -> Arc<KaminoAdapter> {
    let protocols = [VaultFamily::Lending, VaultFamily::Liquidity]
        .into_iter()
        .map(|family| {
            Arc::new(NativeProtocol {
                chain: chain.clone(),
                family,
            }) as Arc<dyn KaminoProtocol>
        })
        .collect();
    Arc::new(
        KaminoAdapter::new(
            BTreeMap::from([("wallet".into(), wallet)]),
            protocols,
            Arc::new(Prices),
        )
        .unwrap(),
    )
}
struct Api(Arc<Chain>);
#[async_trait]
impl JupiterApi for Api {
    async fn request(
        &self,
        _: JupiterHost,
        _: reqwest::Method,
        path: &str,
        _: Option<Value>,
        _: RateBucket,
        _: RequestPriority,
        _: Duration,
    ) -> Result<Value> {
        let query = path.split_once('?').unwrap().1;
        let args: BTreeMap<_, _> = query
            .split('&')
            .map(|p| p.split_once('=').unwrap())
            .collect();
        assert!(path.starts_with("/swap/v2/build?"));
        let input = args["inputMint"];
        let output = args["outputMint"];
        let n = args["amount"].parse::<u64>().unwrap();
        let out = if input == INPUT { 120 } else { 60 };
        let slippage = args["slippageBps"].parse::<u16>().unwrap();
        let mut data = vec![229, 23, 203, 151, 122, 227, 173, 42];
        data.extend(1u32.to_le_bytes());
        data.extend([0, 100, 0, 1]);
        data.extend(n.to_le_bytes());
        data.extend((out as u64).to_le_bytes());
        data.extend(slippage.to_le_bytes());
        data.push(0);
        let metas = [
            (TOKEN.to_owned(), false, false),
            (self.0.owner.clone(), true, true),
            (ata(&self.0.owner, input), false, true),
            (ata(&self.0.owner, output), false, true),
            (JUPITER.into(), false, false),
            (output.into(), false, false),
            (JUPITER.into(), false, false),
            (
                "D8cy77BBepLMngZx6ZukaTff5hCt1HrWyKk3Hnd9oitf".into(),
                false,
                false,
            ),
            (JUPITER.into(), false, false),
        ];
        Ok(
            json!({"inputMint":input,"outputMint":output,"inAmount":n.to_string(),"outAmount":out.to_string(),"otherAmountThreshold":((out*99)/100).to_string(),"swapMode":"ExactIn","slippageBps":slippage,"computeBudgetInstructions":[],"setupInstructions":[],"swapInstruction":{"programId":JUPITER,"accounts":metas.iter().map(|(pubkey,is_signer,is_writable)|json!({"pubkey":pubkey,"isSigner":is_signer,"isWritable":is_writable})).collect::<Vec<_>>(),"data":STANDARD.encode(data)},"cleanupInstruction":null,"otherInstructions":[],"tipInstruction":null,"addressesByLookupTableAddress":{},"blockhashWithMetadata":{"blockhash":vec![42;32],"lastValidBlockHeight":1000}}),
        )
    }
}
pub fn jupiter(chain: Arc<Chain>, wallet: Arc<SolanaWalletAdapter>) -> Arc<JupiterAdapter> {
    Arc::new(
        JupiterAdapter::with_price_source(
            BTreeMap::from([("wallet".into(), wallet)]),
            Arc::new(Api(chain)),
            Arc::new(Prices),
        )
        .unwrap(),
    )
}
