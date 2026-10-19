//! Opt-in owned-validator evidence; never connects a financial transport to mainnet.
use async_trait::async_trait;
use gofin_domain::{GoFinError, PreparedOperation, Result, Submission, SubmissionJournal};
use gofin_kamino::{common::KaminoApi, liquidity::LiquidityProtocol, protocol::*};
use gofin_solana::{
    SolanaRuntime, SolanaSigner,
    rpc::SolanaRpc,
    transaction::{FrozenTransaction, TransactionSender},
};
use serde_json::{Value, json};
use std::{
    fs,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::sleep;
struct OfflineApi;
#[async_trait]
impl KaminoApi for OfflineApi {
    async fn get(&self, _: &str) -> Result<Value> {
        Err(error(
            "Public API is intentionally disabled during owned-validator execution.",
        ))
    }
}
fn error(message: &str) -> GoFinError {
    GoFinError::validation(message)
}
struct LocalRpc {
    url: String,
    client: reqwest::Client,
}
#[async_trait]
impl SolanaRpc for LocalRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "getGenesisHash" {
            return Ok(json!(gofin_solana::MAINNET_GENESIS_HASH));
        }
        let value: Value = self
            .client
            .post(&self.url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
            .map_err(|_| error("Local validator unavailable"))?
            .json()
            .await
            .map_err(|_| error("Invalid local validator response"))?;
        if let Some(e) = value.get("error") {
            return Err(error(&format!("Local validator {method}: {e}")));
        }
        if method == "simulateTransaction" && value["result"]["value"]["err"] != Value::Null {
            eprintln!("Local simulation failure: {}", value["result"]["value"]);
        }
        Ok(value["result"].clone())
    }
}
#[derive(Default)]
struct Journal(Mutex<Vec<Submission>>);
impl SubmissionJournal for Journal {
    fn assert_authorized(&self) -> Result<()> {
        Ok(())
    }
    fn persist_submission(&self, s: &Submission) -> Result<()> {
        self.0.lock().unwrap().push(s.clone());
        Ok(())
    }
    fn persist_dispatch_evidence(&self, _: &Submission, _: &Value) -> Result<()> {
        Ok(())
    }
}
struct Sender(Arc<LocalRpc>);
#[async_trait]
impl TransactionSender for Sender {
    async fn send_signed(&self, wire: &str, _: &FrozenTransaction) -> Result<String> {
        self.0.request("sendTransaction",json!([wire,{"encoding":"base64","skipPreflight":false,"preflightCommitment":"confirmed","maxRetries":0}])).await?.as_str().map(str::to_owned).ok_or_else(||error("Missing local signature"))
    }
}
fn prepared(intent: &VaultIntent) -> PreparedOperation {
    let now = chrono::Utc::now();
    serde_json::from_value(json!({"operation":"kamino.validator.liquidity","input":intent,"binding":{"validator_test":true},"summary":"Synthetic owned-validator Kamino liquidity operation","checks":[],"warnings":[],"observed_at":now.to_rfc3339(),"expires_at":(now+chrono::Duration::minutes(10)).to_rfc3339(),"valuation":null,"intent":{"debits":[],"maximum_costs":[],"expected":[],"locks":[]},"operation_version":1})).unwrap()
}
async fn execute(
    runtime: &SolanaRuntime,
    rpc: Arc<LocalRpc>,
    protocol: &LiquidityProtocol,
    wallet: &SolanaSigner,
    intent: &VaultIntent,
) -> Result<VerifiedVaultEffects> {
    let candidate = protocol
        .build_candidate(runtime, intent, wallet.address())
        .await?;
    protocol
        .validate_candidate(runtime, intent, wallet.address(), &candidate)
        .await?;
    let frozen = runtime
        .freeze_instructions(
            &candidate.instructions,
            &candidate.lookup_tables,
            &candidate.blockhash,
            wallet,
            candidate.last_valid_block_height,
            intent.maximum_network_fee_lamports,
            Some("owned-kamino-liquidity-validator".into()),
            &[],
        )
        .await?;
    runtime.simulate(&frozen).await?;
    let submission = runtime
        .dispatch_with_sender(
            &frozen,
            wallet,
            &prepared(intent),
            Arc::new(Journal::default()),
            &Sender(rpc.clone()),
        )
        .await?;
    let signature = submission.data["chain_signature"]
        .as_str()
        .ok_or_else(|| error("Submission identity missing"))?;
    for _ in 0..120 {
        let status = rpc
            .request(
                "getSignatureStatuses",
                json!([[signature],{"searchTransactionHistory":true}]),
            )
            .await?;
        if status["value"][0]["confirmationStatus"] == "finalized" {
            break;
        }
        sleep(Duration::from_millis(500)).await;
    }
    let proof = runtime.observe_finalized_proof(signature).await?;
    let result = protocol
        .verify_finalized(runtime, intent, wallet.address(), &candidate, &proof)
        .await?;
    eprintln!(
        "Owned-validator {:?}: {} finalized at slot {}, shares {}",
        intent.action,
        signature,
        proof.slot,
        result.shares_delta.as_str()
    );
    Ok(result)
}
#[tokio::test]
#[ignore = "requires scripts/verify-kamino-liquidity-validator.py public clones and synthetic local genesis"]
async fn native_liquidity_deposit_partial_and_full_withdrawal_on_owned_validator() -> Result<()> {
    let url = std::env::var("GOFIN_KAMINO_LIQUIDITY_VALIDATOR_RPC")
        .map_err(|_| error("Missing owned-validator URL"))?;
    if !url.starts_with("http://127.0.0.1:") && !url.starts_with("http://localhost:") {
        return Err(error(
            "Owned-validator writes require a loopback transport.",
        ));
    }
    let path = std::env::var("GOFIN_KAMINO_LIQUIDITY_VALIDATOR_MANIFEST")
        .map_err(|_| error("Missing synthetic manifest"))?;
    let manifest: Value =
        serde_json::from_slice(&fs::read(path).map_err(|_| error("Missing synthetic fixture"))?)
            .map_err(|_| error("Invalid synthetic fixture"))?;
    let wallet = SolanaSigner::parse(
        &fs::read_to_string(manifest["wallet_key"].as_str().unwrap())
            .map_err(|_| error("Missing synthetic key"))?,
    )?;
    if manifest["wallet"] != wallet.address() {
        return Err(error("Synthetic wallet mismatch"));
    }
    let rpc = Arc::new(LocalRpc {
        url,
        client: reqwest::Client::new(),
    });
    let runtime = SolanaRuntime::new(rpc.clone());
    let protocol = LiquidityProtocol::with_api(Arc::new(OfflineApi));
    for strategy in manifest["strategies"].as_array().unwrap() {
        let address = strategy["address"].as_str().unwrap();
        let vault = protocol.vault(&runtime, address).await?;
        eprintln!("Testing cloned {} vault {}", strategy["backend"], address);
        assert!(
            vault.deposits_available,
            "cloned vault must accept native deposits: {:?}",
            vault.restrictions
        );
        let mut intent = VaultIntent {
            wallet: wallet.address().into(),
            family: VaultFamily::Liquidity,
            action: VaultAction::Deposit,
            vault: address.into(),
            amounts: vault
                .assets
                .iter()
                .map(|t| TokenAmount {
                    token: t.clone(),
                    atoms: Some(
                        (manifest["deposit_units"].as_u64().unwrap_or(1)
                            * 10u64.pow(u32::from(t.decimals)))
                        .to_string(),
                    ),
                })
                .collect(),
            shares_atoms: None,
            minimum_shares_atoms: None,
            maximum_network_fee_lamports: 1000000,
            maximum_account_funding_lamports: 100000000,
            stake: manifest["stake"].as_bool().unwrap_or(false),
        };
        let deposit = execute(&runtime, rpc.clone(), &protocol, &wallet, &intent).await?;
        let minted = deposit
            .shares_delta
            .as_str()
            .parse::<u64>()
            .map_err(|_| error("Deposit failed to mint positive shares"))?;
        assert!(minted > 1);
        intent.stake = false;
        intent.action = VaultAction::Withdraw;
        intent.amounts.clear();
        intent.shares_atoms = Some((minted / 2).to_string());
        let partial = execute(&runtime, rpc.clone(), &protocol, &wallet, &intent).await?;
        assert!(partial.shares_delta.is_negative());
        let position = protocol
            .position(&runtime, wallet.address(), address)
            .await?;
        let remaining = position.wallet_shares_atoms.parse::<u64>().unwrap()
            + position.staked_shares_atoms.parse::<u64>().unwrap()
            + position.pending_shares_atoms.parse::<u64>().unwrap();
        intent.shares_atoms = Some(remaining.to_string());
        execute(&runtime, rpc.clone(), &protocol, &wallet, &intent).await?;
        let final_position = protocol
            .position(&runtime, wallet.address(), address)
            .await?;
        assert_eq!(final_position.wallet_shares_atoms, "0");
        assert_eq!(final_position.staked_shares_atoms, "0");
        assert_eq!(final_position.pending_shares_atoms, "0");
        eprintln!(
            "Final residual farm custody: {}",
            final_position.protocol_state["farm_custody"]
        );
    }
    Ok(())
}
