//! Synthetic Meta order backend; all financial writes execute on loopback Surfpool.
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bincode::Options;
use ed25519_dalek::{Signer, SigningKey};
use gofin_domain::{GoFinError, Result};
use gofin_jupiter::api::{JupiterApi, JupiterHost, RateBucket, RequestPriority};
use gofin_solana::SolanaRpc;
use reqwest::{Method, Url};
use serde_json::{Value, json};
use solana_instruction::{AccountMeta, Instruction};
use solana_message::{Message, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use std::{sync::Arc, time::Duration};

const TOKEN: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const USDT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const MAKER: &str = "Gymcte79ALGtFrWybRxqb7M7uRjnbjgaHjGgQf1v4qB8";
fn invalid() -> GoFinError {
    GoFinError::validation("Invalid synthetic Jupiter Surfpool order.")
}
fn codec() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
}
fn ata(owner: &str, mint: &str) -> Result<Pubkey> {
    gofin_solana::associated_token_address(owner, mint, TOKEN)?
        .parse()
        .map_err(|_| invalid())
}
fn transfer(from: Pubkey, to: Pubkey, owner: Pubkey, amount: u64) -> Result<Instruction> {
    let mut data = vec![3];
    data.extend(amount.to_le_bytes());
    Ok(Instruction {
        program_id: TOKEN.parse().map_err(|_| invalid())?,
        accounts: vec![
            AccountMeta::new(from, false),
            AccountMeta::new(to, false),
            AccountMeta::new_readonly(owner, true),
        ],
        data,
    })
}
pub struct LocalMetaApi {
    pub rpc: Arc<dyn SolanaRpc>,
    pub wallet: String,
}
#[async_trait]
impl JupiterApi for LocalMetaApi {
    async fn request(
        &self,
        _host: JupiterHost,
        method: Method,
        path: &str,
        body: Option<Value>,
        _bucket: RateBucket,
        _priority: RequestPriority,
        _max_wait: Duration,
    ) -> Result<Value> {
        if method == Method::GET && path.starts_with("/swap/v2/order?") {
            let url = Url::parse(&format!("http://localhost{path}")).map_err(|_| invalid())?;
            let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
            let input = params.get("inputMint").ok_or_else(invalid)?;
            let output = params.get("outputMint").ok_or_else(invalid)?;
            if !((input == USDT && output == USDC) || (input == USDC && output == USDT))
                || params.get("taker") != Some(&self.wallet)
            {
                return Err(invalid());
            }
            let amount: u64 = params
                .get("amount")
                .ok_or_else(invalid)?
                .parse()
                .map_err(|_| invalid())?;
            let out = amount;
            if amount == 0 || out == 0 {
                return Err(invalid());
            }
            let latest = self
                .rpc
                .request("getLatestBlockhash", json!([{"commitment":"confirmed"}]))
                .await?;
            let blockhash = latest
                .pointer("/value/blockhash")
                .and_then(Value::as_str)
                .ok_or_else(invalid)?
                .parse()
                .map_err(|_| invalid())?;
            let height = latest
                .pointer("/value/lastValidBlockHeight")
                .and_then(Value::as_u64)
                .ok_or_else(invalid)?;
            let wallet: Pubkey = self.wallet.parse().map_err(|_| invalid())?;
            let maker_key = SigningKey::from_bytes(&[92; 32]);
            let maker = Pubkey::new_from_array(maker_key.verifying_key().to_bytes());
            if maker.to_string() != MAKER {
                return Err(invalid());
            }
            let instructions = vec![
                transfer(
                    ata(&self.wallet, input)?,
                    ata(MAKER, input)?,
                    wallet,
                    amount,
                )?,
                transfer(ata(MAKER, output)?, ata(&self.wallet, output)?, maker, out)?,
            ];
            let message = Message::new_with_blockhash(&instructions, Some(&wallet), &blockhash);
            let maker_index = message
                .account_keys
                .iter()
                .take(usize::from(message.header.num_required_signatures))
                .position(|p| p == &maker)
                .ok_or_else(invalid)?;
            let mut tx = VersionedTransaction {
                signatures: vec![
                    Signature::default();
                    usize::from(message.header.num_required_signatures)
                ],
                message: VersionedMessage::Legacy(message),
            };
            let signed_message = codec().serialize(&tx.message).map_err(|_| invalid())?;
            tx.signatures[maker_index] =
                Signature::from(maker_key.sign(&signed_message).to_bytes());
            let wire = codec().serialize(&tx).map_err(|_| invalid())?;
            return Ok(
                json!({"transaction":STANDARD.encode(wire),"requestId":uuid::Uuid::new_v4().to_string(),"inputMint":input,"outputMint":output,"inAmount":amount.to_string(),"outAmount":out.to_string(),"otherAmountThreshold":out.to_string(),"swapMode":"ExactIn","slippageBps":0,"taker":self.wallet,"signatureFeeLamports":10000,"signatureFeePayer":self.wallet,"prioritizationFeeLamports":0,"prioritizationFeePayer":null,"rentFeeLamports":0,"rentFeePayer":null,"router":"metis","lastValidBlockHeight":height,"feeBps":0,"feeMint":null,"expireAt":null}),
            );
        }
        if method == Method::POST && path == "/swap/v2/execute" {
            let body = body.ok_or_else(invalid)?;
            let wire = body
                .get("signedTransaction")
                .and_then(Value::as_str)
                .ok_or_else(invalid)?;
            let signature = self.rpc.request("sendTransaction", json!([wire,{"encoding":"base64","preflightCommitment":"confirmed","skipPreflight":false,"maxRetries":0}])).await?;
            return Ok(
                json!({"status":"Success","signature":signature.as_str().ok_or_else(invalid)?}),
            );
        }
        Err(invalid())
    }
}
