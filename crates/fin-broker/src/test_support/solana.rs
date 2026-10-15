//! Synthetic Solana RPC and signer fixture shared by exchange funding tests.
//! It exercises the real wallet, transaction verifier, engine and store; it
//! never contacts a live endpoint or uses production credentials.
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use bincode::Options;
use fin_domain::{FinError, Result};
use fin_pricing::{UsdPriceSource, UsdQuote};
use fin_solana::{
    MAINNET_GENESIS_HASH, SolanaRpc, SolanaSigner, SolanaWalletAdapter, associated_token_address,
    token::TOKEN_PROGRAM_ID,
};
use serde_json::{Value, json};
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_message::{Message, VersionedMessage};
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;

pub const USDC_MINT: &str = fin_solana::funding::MAINNET_USDC_MINT;
const WALLET_SEED: [u8; 32] = [42; 32];
const EXTERNAL_SEED: [u8; 32] = [61; 32];
const TOKEN_RENT: u64 = 2_039_280;
const TX_FEE: u64 = 5_000;

#[derive(Clone, Debug)]
struct TokenAccount {
    mint: String,
    owner: String,
    amount: u64,
    lamports: u64,
    exists: bool,
}
#[derive(Clone, Debug)]
struct StoredTx {
    wire: String,
    pre: Vec<u64>,
    post: Vec<u64>,
    pre_tokens: Vec<Value>,
    post_tokens: Vec<Value>,
    fee: u64,
}
struct State {
    wallet_name: String,
    wallet: String,
    lamports: u64,
    accounts: BTreeMap<String, TokenAccount>,
    known_owners: BTreeMap<String, String>,
    account_keys: BTreeMap<String, (String, Vec<u8>, u64)>,
    transactions: BTreeMap<String, StoredTx>,
    send_calls: usize,
    last_signature: Option<String>,
}

/// One isolated mainnet-shaped fixture, with real Ed25519 signatures and
/// finalized transaction metadata synthesized from the submitted message.
#[derive(Clone)]
pub struct SolanaFundingFixture {
    state: Arc<Mutex<State>>,
    rpc: Arc<FundingRpc>,
    wallet_adapter: Arc<SolanaWalletAdapter>,
}
impl SolanaFundingFixture {
    pub fn new(wallet_name: &str, initial_sol_lamports: u64, initial_usdc_atoms: u64) -> Self {
        let signer = signer_from_seed(WALLET_SEED);
        let wallet = signer.address().to_owned();
        let state = Arc::new(Mutex::new(State {
            wallet_name: wallet_name.into(),
            wallet: wallet.clone(),
            lamports: initial_sol_lamports,
            accounts: BTreeMap::new(),
            known_owners: BTreeMap::new(),
            account_keys: BTreeMap::new(),
            transactions: BTreeMap::new(),
            send_calls: 0,
            last_signature: None,
        }));
        let source_ata = associated_token_address(&wallet, USDC_MINT, TOKEN_PROGRAM_ID).unwrap();
        {
            let mut s = state.lock().unwrap();
            s.accounts.insert(
                source_ata.clone(),
                TokenAccount {
                    mint: USDC_MINT.into(),
                    owner: wallet.clone(),
                    amount: initial_usdc_atoms,
                    lamports: TOKEN_RENT,
                    exists: true,
                },
            );
            s.known_owners.insert(wallet.clone(), source_ata);
            let mint_data = mint_data(6, 1_000_000_000_000_000);
            s.account_keys.insert(
                USDC_MINT.into(),
                (TOKEN_PROGRAM_ID.into(), mint_data, TOKEN_RENT),
            );
        }
        let rpc = Arc::new(FundingRpc {
            state: state.clone(),
        });
        let wallet_adapter = Arc::new(
            SolanaWalletAdapter::new(
                wallet_name,
                &wallet,
                Some(signer),
                rpc.clone(),
                Arc::new(FixturePrices),
            )
            .unwrap(),
        );
        Self {
            state,
            rpc,
            wallet_adapter,
        }
    }
    pub fn wallet(&self) -> Arc<SolanaWalletAdapter> {
        self.wallet_adapter.clone()
    }
    pub fn rpc(&self) -> Arc<FundingRpc> {
        self.rpc.clone()
    }
    pub fn signer_address(&self) -> String {
        self.state.lock().unwrap().wallet.clone()
    }
    pub fn wallet_name(&self) -> String {
        self.state.lock().unwrap().wallet_name.clone()
    }
    pub fn last_signature(&self) -> Option<String> {
        self.state.lock().unwrap().last_signature.clone()
    }
    pub fn set_balance(&self, sol_lamports: u64, usdc_atoms: u64) {
        let mut s = self.state.lock().unwrap();
        s.lamports = sol_lamports;
        let wallet = s.wallet.clone();
        if let Some(account) = s.accounts.values_mut().find(|a| a.owner == wallet) {
            account.amount = usdc_atoms;
        }
    }
    /// Register an exchange wallet owner. Its SPL account is initially absent,
    /// so a real wallet.transfer preview exercises associated-account creation.
    pub fn register_token_owner(&self, owner: &str) -> String {
        let ata = associated_token_address(owner, USDC_MINT, TOKEN_PROGRAM_ID).unwrap();
        let mut s = self.state.lock().unwrap();
        s.known_owners.insert(owner.to_owned(), ata.clone());
        s.accounts.entry(ata.clone()).or_insert(TokenAccount {
            mint: USDC_MINT.into(),
            owner: owner.into(),
            amount: 0,
            lamports: 0,
            exists: false,
        });
        ata
    }
    pub fn token_balance(&self, owner: &str) -> u64 {
        let s = self.state.lock().unwrap();
        s.accounts
            .values()
            .filter(|a| a.owner == owner && a.mint == USDC_MINT)
            .map(|a| a.amount)
            .sum()
    }
    /// Inject a CEX-originated, correctly signed external payout transaction
    /// for native SOL or USDC. `recipient` is the native address or SPL owner.
    pub fn inject_finalized_transfer(
        &self,
        asset: &str,
        recipient: &str,
        atoms: u64,
    ) -> Result<String> {
        if atoms == 0 {
            return Err(FinError::validation(
                "Synthetic transfer amount must be positive.",
            ));
        }
        let external = signer_from_seed(EXTERNAL_SEED);
        let recipient_key = recipient
            .parse::<solana_pubkey::Pubkey>()
            .map_err(|_| FinError::validation("Invalid synthetic recipient."))?;
        let payer = external
            .address()
            .parse::<solana_pubkey::Pubkey>()
            .map_err(|_| FinError::validation("Invalid synthetic payer."))?;
        let (ix, affected, token) = if asset == "native" {
            let mut data = 2u32.to_le_bytes().to_vec();
            data.extend_from_slice(&atoms.to_le_bytes());
            let ix = Instruction {
                program_id: "11111111111111111111111111111111".parse().unwrap(),
                accounts: vec![
                    AccountMeta::new(payer, true),
                    AccountMeta::new(recipient_key, false),
                ],
                data,
            };
            (
                ix,
                vec![external.address().to_owned(), recipient.to_owned()],
                None,
            )
        } else if asset == USDC_MINT {
            let source_owner = external.address().to_owned();
            let source_ata =
                associated_token_address(&source_owner, USDC_MINT, TOKEN_PROGRAM_ID).unwrap();
            let dest_ata =
                associated_token_address(recipient, USDC_MINT, TOKEN_PROGRAM_ID).unwrap();
            let mint_key = USDC_MINT
                .parse::<solana_pubkey::Pubkey>()
                .map_err(|_| FinError::validation("Invalid synthetic mint."))?;
            let source_key = source_ata.parse::<solana_pubkey::Pubkey>().unwrap();
            let dest_key = dest_ata.parse::<solana_pubkey::Pubkey>().unwrap();
            let mut data = vec![12];
            data.extend_from_slice(&atoms.to_le_bytes());
            data.push(6);
            let ix = Instruction {
                program_id: TOKEN_PROGRAM_ID.parse().unwrap(),
                accounts: vec![
                    AccountMeta::new(source_key, false),
                    AccountMeta::new_readonly(mint_key, false),
                    AccountMeta::new(dest_key, false),
                    AccountMeta::new_readonly(payer, true),
                ],
                data,
            };
            let mut s = self.state.lock().unwrap();
            s.accounts.insert(
                source_ata.clone(),
                TokenAccount {
                    mint: USDC_MINT.into(),
                    owner: source_owner.clone(),
                    amount: atoms,
                    lamports: TOKEN_RENT,
                    exists: true,
                },
            );
            s.known_owners
                .insert(recipient.to_owned(), dest_ata.clone());
            s.accounts.entry(dest_ata.clone()).or_insert(TokenAccount {
                mint: USDC_MINT.into(),
                owner: recipient.to_owned(),
                amount: 0,
                lamports: 0,
                exists: false,
            });
            (
                ix,
                vec![source_ata, dest_ata],
                Some((source_owner, recipient.to_owned())),
            )
        } else {
            return Err(FinError::validation(
                "Only native SOL and canonical USDC are supported in the synthetic fixture.",
            ));
        };
        let message =
            Message::new_with_blockhash(&[ix], Some(&payer), &Hash::new_from_array([9u8; 32]));
        let message_enum = VersionedMessage::Legacy(message);
        let message_bytes = codec()
            .serialize(&message_enum)
            .map_err(|_| FinError::validation("Unable to encode synthetic transaction."))?;
        use ed25519_dalek::Signer as _;
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&EXTERNAL_SEED);
        let signature = Signature::from(signing_key.sign(&message_bytes).to_bytes());
        let tx = VersionedTransaction {
            signatures: vec![signature],
            message: message_enum,
        };
        let wire_bytes = codec()
            .serialize(&tx)
            .map_err(|_| FinError::validation("Unable to encode synthetic transaction."))?;
        let wire = STANDARD.encode(&wire_bytes);
        let signature = tx.signatures[0].to_string();
        {
            let mut state = self.state.lock().unwrap();
            if asset == "native" && recipient == state.wallet {
                state.lamports = state.lamports.saturating_add(atoms);
            }
            if let Some((_, owner)) = token.as_ref()
                && let Some(account) = state
                    .accounts
                    .values_mut()
                    .find(|account| account.owner == *owner && account.mint == USDC_MINT)
            {
                account.amount = account.amount.saturating_add(atoms);
                account.exists = true;
                if account.lamports == 0 {
                    account.lamports = TOKEN_RENT;
                }
            }
            state.last_signature = Some(signature.clone());
        }
        self.rpc
            .store_external_tx(&signature, &wire, &affected, token, atoms)?;
        Ok(signature)
    }
}

pub struct FundingRpc {
    state: Arc<Mutex<State>>,
}
impl FundingRpc {
    pub fn last_signature(&self) -> Option<String> {
        self.state.lock().unwrap().last_signature.clone()
    }
    pub fn submission_count(&self) -> usize {
        self.state.lock().unwrap().transactions.len()
    }
    /// Count RPC dispatch attempts, including repeats of the same signature.
    pub fn send_count(&self) -> usize {
        self.state.lock().unwrap().send_calls
    }
    fn store_external_tx(
        &self,
        signature: &str,
        wire: &str,
        affected: &[String],
        token: Option<(String, String)>,
        atoms: u64,
    ) -> Result<()> {
        let tx = decode_wire(wire)?;
        let keys = message_keys(&tx);
        let mut s = self.state.lock().unwrap();
        let mut pre = vec![0u64; keys.len()];
        let mut post = vec![0u64; keys.len()];
        for (i, key) in keys.iter().enumerate() {
            if key == &s.wallet {
                pre[i] = s.lamports;
                post[i] = if token.is_none() && affected.get(1) == Some(&s.wallet) {
                    s.lamports.saturating_add(atoms)
                } else {
                    s.lamports
                };
            }
        }
        if token.is_none() {
            if affected.get(1) != Some(&s.wallet) {
                for (i, key) in keys.iter().enumerate() {
                    if key == &affected[1] {
                        post[i] = atoms;
                    }
                }
            } else if let Some(index) = keys.iter().position(|key| key == &s.wallet) {
                post[index] = s.lamports;
                pre[index] = s.lamports.saturating_sub(atoms);
            }
        }
        let (pre_tokens, post_tokens) = if let Some((source_owner, dest_owner)) = token {
            let destination_account = s.accounts.get(&affected[1]).ok_or_else(|| {
                FinError::validation("Synthetic destination token account is missing.")
            })?;
            let post_amount = destination_account.amount;
            let pre_amount = post_amount.saturating_sub(atoms);
            (
                vec![
                    token_row_for(&keys, &affected[0], USDC_MINT, &source_owner, 6, atoms),
                    token_row_for(&keys, &affected[1], USDC_MINT, &dest_owner, 6, pre_amount),
                ],
                vec![
                    token_row_for(&keys, &affected[0], USDC_MINT, &source_owner, 6, 0),
                    token_row_for(&keys, &affected[1], USDC_MINT, &dest_owner, 6, post_amount),
                ],
            )
        } else {
            (vec![], vec![])
        };
        s.transactions.insert(
            signature.into(),
            StoredTx {
                wire: wire.into(),
                pre,
                post,
                pre_tokens,
                post_tokens,
                fee: TX_FEE,
            },
        );
        Ok(())
    }
}

#[async_trait]
impl SolanaRpc for FundingRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let mut s = self.state.lock().unwrap();
        Ok(match method {
            "getGenesisHash" => json!(MAINNET_GENESIS_HASH),
            "getSlot" => json!(100),
            "getEpochInfo" => {
                json!({"epoch":1,"slotIndex":1,"slotsInEpoch":432000,"absoluteSlot":100})
            }
            "getBalance" => {
                let address = params[0].as_str().unwrap_or("");
                json!({"context":{"slot":100},"value":if address==s.wallet {s.lamports} else {0}})
            }
            "getLatestBlockhash" => {
                json!({"context":{"slot":100},"value":{"blockhash":"11111111111111111111111111111111","lastValidBlockHeight":1000}})
            }
            "getBlockHeight" => json!(100),
            "isBlockhashValid" => json!({"context":{"slot":100},"value":true}),
            "getFeeForMessage" => json!({"context":{"slot":100},"value":TX_FEE}),
            "getMinimumBalanceForRentExemption" => json!(TOKEN_RENT),
            "simulateTransaction" => {
                let addresses = params
                    .get(1)
                    .and_then(|v| v.pointer("/accounts/addresses"))
                    .and_then(Value::as_array);
                let accounts=addresses.map(|addresses|addresses.iter().map(|v| { let address=v.as_str().unwrap_or(""); s.accounts.get(address).filter(|a|a.exists).map(|a| json!({"address":address,"owner":TOKEN_PROGRAM_ID,"lamports":a.lamports,"data":[STANDARD.encode(token_account_data(&a.mint,&a.owner,a.amount)),"base64"],"executable":false,"rentEpoch":0})) }).collect::<Vec<_>>());
                json!({"context":{"slot":100},"value":{"err":null,"unitsConsumed":100_000,"accounts":accounts}})
            }
            "getAccountInfo" => {
                let address = params[0].as_str().unwrap_or("");
                if address == USDC_MINT {
                    account_value(s.account_keys.get(address))
                } else if let Some(a) = s.accounts.get(address).filter(|a| a.exists) {
                    json!({"context":{"slot":100},"value":{"owner":TOKEN_PROGRAM_ID,"lamports":a.lamports,"data":[STANDARD.encode(token_account_data(&a.mint,&a.owner,a.amount)),"base64"],"executable":false,"rentEpoch":0}})
                } else {
                    json!({"context":{"slot":100},"value":null})
                }
            }
            "getMultipleAccounts" => {
                let addresses = params
                    .as_array()
                    .and_then(|args| args.first())
                    .and_then(Value::as_array)
                    .ok_or_else(|| FinError::validation("Malformed synthetic account batch."))?;
                let values = addresses
                    .iter()
                    .map(|address| {
                        let address = address.as_str().unwrap_or_default();
                        s.account_keys
                            .get(address)
                            .map(|(owner, data, lamports)| {
                                json!({"owner":owner,"lamports":lamports,"data":[STANDARD.encode(data),"base64"],"executable":false,"rentEpoch":0})
                            })
                            .or_else(|| {
                                s.accounts
                                    .get(address)
                                    .filter(|account| account.exists)
                                    .map(|account| {
                                        json!({"owner":TOKEN_PROGRAM_ID,"lamports":account.lamports,"data":[STANDARD.encode(token_account_data(&account.mint,&account.owner,account.amount)),"base64"],"executable":false,"rentEpoch":0})
                                    })
                            })
                    })
                    .collect::<Vec<_>>();
                json!({"context":{"slot":100},"value":values})
            }
            "getTokenAccountsByOwner" => {
                let owner = params[0].as_str().unwrap_or("");
                let program = params
                    .pointer("/1/programId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let rows=s.accounts.iter().filter(|(_,a)|a.exists && a.owner==owner && program==TOKEN_PROGRAM_ID).map(|(address,a)|json!({"pubkey":address,"account":{"owner":TOKEN_PROGRAM_ID,"lamports":a.lamports,"data":[STANDARD.encode(token_account_data(&a.mint,&a.owner,a.amount)),"base64"]}})).collect::<Vec<_>>();
                json!({"context":{"slot":100},"value":rows})
            }
            "sendTransaction" => {
                s.send_calls += 1;
                let encoded = params[0]
                    .as_str()
                    .ok_or_else(|| FinError::validation("Missing transaction bytes."))?
                    .to_owned();
                let tx = decode_wire(&encoded)?;
                let signature = tx
                    .signatures
                    .first()
                    .map(ToString::to_string)
                    .ok_or_else(|| FinError::validation("Missing transaction signature."))?;
                process_wallet_transfer(&mut s, &tx, &encoded)?;
                s.last_signature = Some(signature.clone());
                json!(signature)
            }
            "getSignatureStatuses" => {
                let signature = params.pointer("/0/0").and_then(Value::as_str).unwrap_or("");
                if s.transactions.contains_key(signature) {
                    json!({"value":[{"confirmationStatus":"finalized","err":null}]})
                } else {
                    json!({"value":[null]})
                }
            }
            "getTransaction" => {
                let signature = params[0].as_str().unwrap_or("");
                if let Some(tx) = s.transactions.get(signature) {
                    json!({"slot":99,"transaction":[tx.wire,"base64"],"meta":{"fee":tx.fee,"err":null,"preBalances":tx.pre,"postBalances":tx.post,"preTokenBalances":tx.pre_tokens,"postTokenBalances":tx.post_tokens,"loadedAddresses":{"writable":[],"readonly":[]},"innerInstructions":[],"logMessages":[]}})
                } else {
                    Value::Null
                }
            }
            _ => {
                return Err(FinError::validation(
                    "Unexpected synthetic Solana RPC method.",
                ));
            }
        })
    }
}

fn process_wallet_transfer(state: &mut State, tx: &VersionedTransaction, wire: &str) -> Result<()> {
    let keys = message_keys(tx);
    let message_instructions: Vec<(String, Vec<String>, Vec<u8>)> = match &tx.message {
        VersionedMessage::Legacy(m) => m
            .instructions
            .iter()
            .map(|ix| {
                (
                    keys[ix.program_id_index as usize].clone(),
                    ix.accounts
                        .iter()
                        .map(|i| keys[*i as usize].clone())
                        .collect(),
                    ix.data.clone(),
                )
            })
            .collect(),
        VersionedMessage::V0(m) => m
            .instructions
            .iter()
            .map(|ix| {
                (
                    keys[ix.program_id_index as usize].clone(),
                    ix.accounts
                        .iter()
                        .map(|i| keys[*i as usize].clone())
                        .collect(),
                    ix.data.clone(),
                )
            })
            .collect(),
        _ => {
            return Err(FinError::validation(
                "Unsupported synthetic message version.",
            ));
        }
    };
    let mut pre = vec![0u64; keys.len()];
    let mut post = vec![0u64; keys.len()];
    let payer = keys
        .first()
        .cloned()
        .ok_or_else(|| FinError::validation("No payer."))?;
    let wallet_balance = state.lamports;
    if let Some(i) = keys.iter().position(|k| k == &payer) {
        pre[i] = wallet_balance;
        post[i] = wallet_balance.saturating_sub(TX_FEE);
    }
    let mut pre_tokens = Vec::new();
    let mut post_tokens = Vec::new();
    let mut fee = TX_FEE;
    for (program, accounts, data) in message_instructions {
        if program == "11111111111111111111111111111111"
            && data.len() == 12
            && u32::from_le_bytes(data[..4].try_into().unwrap()) == 2
        {
            let amount = u64::from_le_bytes(data[4..12].try_into().unwrap());
            let (from, to) = (&accounts[0], &accounts[1]);
            let from_i = keys.iter().position(|k| k == from).unwrap();
            let to_i = keys.iter().position(|k| k == to).unwrap();
            pre[from_i] = wallet_balance;
            post[from_i] = post[from_i].saturating_sub(amount);
            post[to_i] = post[to_i].saturating_add(amount);
            state.lamports = state.lamports.saturating_sub(amount + fee);
            fee = 0;
        } else if program == TOKEN_PROGRAM_ID && matches!(data.first(), Some(3) | Some(12)) {
            let (source, destination, amount) = if data[0] == 3 {
                (
                    &accounts[0],
                    &accounts[1],
                    u64::from_le_bytes(data[1..9].try_into().unwrap()),
                )
            } else {
                (
                    &accounts[0],
                    &accounts[2],
                    u64::from_le_bytes(data[1..9].try_into().unwrap()),
                )
            };
            let mut src = state
                .accounts
                .get(source)
                .cloned()
                .ok_or_else(|| FinError::validation("Source token account missing."))?;
            let mut dst = state
                .accounts
                .get(destination)
                .cloned()
                .or_else(|| {
                    state
                        .known_owners
                        .iter()
                        .find(|(_, a)| a.as_str() == destination)
                        .map(|(owner, _)| TokenAccount {
                            mint: USDC_MINT.into(),
                            owner: owner.clone(),
                            amount: 0,
                            lamports: 0,
                            exists: false,
                        })
                })
                .ok_or_else(|| FinError::validation("Destination owner not registered."))?;
            let before_src = src.amount;
            let before_dst = dst.amount;
            let destination_pre_lamports = dst.lamports;
            dst.exists = true;
            if dst.lamports == 0 {
                dst.lamports = TOKEN_RENT;
                state.lamports = state.lamports.saturating_sub(TOKEN_RENT);
                post[0] = post[0].saturating_sub(TOKEN_RENT);
            }
            src.amount = src
                .amount
                .checked_sub(amount)
                .ok_or_else(|| FinError::validation("Insufficient token balance."))?;
            dst.amount = dst
                .amount
                .checked_add(amount)
                .ok_or_else(|| FinError::validation("Token balance overflow."))?;
            let source_index = keys.iter().position(|k| k == source).unwrap();
            let dest_index = keys.iter().position(|k| k == destination).unwrap();
            pre_tokens.push(token_row_for(
                &keys, source, &src.mint, &src.owner, 6, before_src,
            ));
            post_tokens.push(token_row_for(
                &keys, source, &src.mint, &src.owner, 6, src.amount,
            ));
            pre_tokens.push(token_row_for(
                &keys,
                destination,
                &dst.mint,
                &dst.owner,
                6,
                before_dst,
            ));
            post_tokens.push(token_row_for(
                &keys,
                destination,
                &dst.mint,
                &dst.owner,
                6,
                dst.amount,
            ));
            pre[source_index] = TOKEN_RENT;
            post[source_index] = TOKEN_RENT;
            pre[dest_index] = destination_pre_lamports;
            post[dest_index] = dst.lamports;
            state.accounts.insert(source.clone(), src);
            state.accounts.insert(destination.clone(), dst);
            state.lamports = state.lamports.saturating_sub(fee);
            fee = 0;
        }
    }
    if fee > 0 {
        state.lamports = state.lamports.saturating_sub(fee);
    }
    let signature = tx.signatures[0].to_string();
    state.transactions.insert(
        signature,
        StoredTx {
            wire: wire.into(),
            pre,
            post,
            pre_tokens,
            post_tokens,
            fee: TX_FEE,
        },
    );
    Ok(())
}

fn signer_from_seed(seed: [u8; 32]) -> Arc<SolanaSigner> {
    let key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let mut bytes = [0u8; 64];
    bytes[..32].copy_from_slice(&seed);
    bytes[32..].copy_from_slice(key.verifying_key().as_bytes());
    Arc::new(SolanaSigner::parse(&bs58::encode(bytes).into_string()).unwrap())
}
fn codec() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(1232)
}
fn decode_wire(encoded: &str) -> Result<VersionedTransaction> {
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| FinError::validation("Malformed fixture transaction."))?;
    codec()
        .deserialize(&bytes)
        .map_err(|_| FinError::validation("Malformed fixture transaction."))
}
fn message_keys(tx: &VersionedTransaction) -> Vec<String> {
    match &tx.message {
        VersionedMessage::Legacy(m) => m.account_keys.iter().map(ToString::to_string).collect(),
        VersionedMessage::V0(m) => m.account_keys.iter().map(ToString::to_string).collect(),
        _ => vec![],
    }
}
fn token_account_data(mint: &str, owner: &str, amount: u64) -> Vec<u8> {
    let mut b = vec![0u8; 165];
    b[..32].copy_from_slice(&bs58::decode(mint).into_vec().unwrap());
    b[32..64].copy_from_slice(&bs58::decode(owner).into_vec().unwrap());
    b[64..72].copy_from_slice(&amount.to_le_bytes());
    b[108] = 1;
    b
}
fn mint_data(decimals: u8, supply: u64) -> Vec<u8> {
    let mut b = vec![0u8; 82];
    b[36..44].copy_from_slice(&supply.to_le_bytes());
    b[44] = decimals;
    b[45] = 1;
    b
}
fn token_row_for(
    keys: &[String],
    account: &str,
    mint: &str,
    owner: &str,
    decimals: u8,
    amount: u64,
) -> Value {
    let index = keys.iter().position(|k| k == account).unwrap_or(0);
    json!({"accountIndex":index,"mint":mint,"owner":owner,"programId":TOKEN_PROGRAM_ID,"uiTokenAmount":{"amount":amount.to_string(),"decimals":decimals}})
}
fn account_value(value: Option<&(String, Vec<u8>, u64)>) -> Value {
    if let Some((owner, data, lamports)) = value {
        json!({"context":{"slot":100},"value":{"owner":owner,"lamports":lamports,"data":[STANDARD.encode(data),"base64"],"executable":false,"rentEpoch":0}})
    } else {
        json!({"context":{"slot":100},"value":null})
    }
}
struct FixturePrices;
#[async_trait]
impl UsdPriceSource for FixturePrices {
    async fn quote(&self, _asset: &fin_domain::AssetId) -> Result<UsdQuote> {
        Ok(UsdQuote {
            usd_per_unit: "100".into(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            observed_at: chrono::Utc::now().to_rfc3339(),
            source: "synthetic-solana-funding-test".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fin_domain::AssetId;
    use fin_domain::{Adapter, Submission, SubmissionJournal};
    use std::sync::Mutex as StdMutex;

    struct Journal(StdMutex<Vec<Submission>>);
    impl SubmissionJournal for Journal {
        fn assert_authorized(&self) -> Result<()> {
            Ok(())
        }
        fn persist_submission(&self, value: &Submission) -> Result<()> {
            self.0.lock().unwrap().push(value.clone());
            Ok(())
        }
        fn persist_dispatch_evidence(&self, _: &Submission, _: &Value) -> Result<()> {
            Ok(())
        }
    }
    use fin_solana::funding::{AmountBounds, ExternalPayoutRequest, ExternalPayoutStatus};

    #[tokio::test]
    async fn injected_native_and_usdc_credits_have_finalized_transaction_proofs() {
        let fixture = SolanaFundingFixture::new("fixture", 5_000_000_000, 2_000_000);
        fixture.set_balance(5_000_000_000, 2_000_000);
        let wallet = fixture.signer_address();
        let runtime = fixture.wallet().runtime();
        for (asset, decimals, amount) in [("native", 9, 250_000_000), (USDC_MINT, 6, 400_000)] {
            let signature = fixture
                .inject_finalized_transfer(asset, &wallet, amount)
                .unwrap();
            let request = ExternalPayoutRequest {
                signature,
                asset: AssetId {
                    network: fin_solana::NETWORK.into(),
                    reference: asset.into(),
                    decimals,
                },
                recipient: wallet.clone(),
                recipient_owner: None,
                source_owner: None,
                amount: AmountBounds {
                    minimum_atoms: amount,
                    maximum_atoms: amount,
                },
            };
            let result = runtime
                .verify_external_payout(&request)
                .await
                .unwrap_or_else(|e| panic!("asset {asset}: {e:?}"));
            let ExternalPayoutStatus::Finalized { evidence } = result else {
                panic!("injected transfer did not finalize")
            };
            assert_eq!(evidence.credited_atoms, amount);
            if asset == USDC_MINT {
                assert_eq!(fixture.token_balance(&wallet), 2_400_000);
            }
        }
    }

    #[tokio::test]
    async fn real_wallet_native_and_usdc_sends_are_signed_and_finalized() {
        for token in [false, true] {
            let fixture = SolanaFundingFixture::new("fixture", 10_000_000_000, 5_000_000);
            let wallet = fixture.wallet();
            let recipient = signer_from_seed(EXTERNAL_SEED).address().to_owned();
            if token {
                fixture.register_token_owner(&recipient);
            }
            let mut input = json!({"wallet":fixture.wallet_name(),"network":"solana","cluster":"mainnet-beta","recipient":recipient,"amount":if token {"1.25"} else {"0.01"},"max_fee_sol":"0.00001","requestId":if token {"fixture-usdc-send"} else {"fixture-sol-send"}});
            if token {
                input["mint"] = json!(USDC_MINT);
                input["source_token_account"] = json!(
                    associated_token_address(
                        &fixture.signer_address(),
                        USDC_MINT,
                        TOKEN_PROGRAM_ID
                    )
                    .unwrap()
                );
                input["max_account_funding_sol"] = json!("0.003");
            }
            let prepared = wallet.prepare("wallet.transfer", &input).await.unwrap();
            let journal = Arc::new(Journal(StdMutex::new(Vec::new())));
            let submitted = wallet.submit(&prepared, journal.clone()).await.unwrap();
            let signature = submitted.data["chain_signature"].as_str().unwrap();
            let proof = wallet
                .runtime()
                .observe_finalized_proof(signature)
                .await
                .unwrap();
            assert!(!proof.failed);
            assert_eq!(fixture.rpc().submission_count(), 1);
            assert_eq!(fixture.rpc().last_signature().as_deref(), Some(signature));
            assert_eq!(journal.0.lock().unwrap().len(), 1);
        }
    }
}
