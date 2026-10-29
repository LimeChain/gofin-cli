//! Owned-validator test: public program/account clones and synthetic balances only.
//! Run through scripts/verify-kamino-lending-validator.py; no production credentials.
use async_trait::async_trait;
use gofin_domain::{GoFinError, PreparedOperation, Result, Submission, SubmissionJournal};
use gofin_kamino::{lending::LendingProtocol, protocol::*};
use gofin_solana::{
    SolanaRpc, SolanaRuntime, SolanaSigner,
    transaction::{FrozenTransaction, TransactionSender},
};
use serde_json::{Value, json};
use std::{
    fs,
    sync::{Arc, Mutex},
    time::Duration,
};
struct Rpc {
    url: String,
    client: reqwest::Client,
}
fn err(message: &str) -> GoFinError {
    GoFinError::new(
        "KAMINO_VALIDATOR",
        "test",
        message,
        "Inspect ignored local validator evidence.",
    )
}
#[async_trait]
impl SolanaRpc for Rpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "getGenesisHash" {
            return Ok(json!(gofin_solana::MAINNET_GENESIS_HASH));
        }
        let result: Value = self
            .client
            .post(&self.url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
            .map_err(|_| err("loopback RPC failed"))?
            .json()
            .await
            .map_err(|_| err("invalid loopback response"))?;
        if result.get("error").is_some() {
            return Err(err(&format!("Loopback {method}: {}", result["error"])));
        }
        if method == "simulateTransaction" && !result["result"]["value"]["err"].is_null() {
            eprintln!("SIMULATION FAILURE {}", result["result"]["value"]);
        }
        result
            .get("result")
            .cloned()
            .ok_or_else(|| err("missing result"))
    }
}
struct Sender(Arc<Rpc>);
#[async_trait]
impl TransactionSender for Sender {
    async fn send_signed(&self, wire: &str, _: &FrozenTransaction) -> Result<String> {
        self.0.request("sendTransaction",json!([wire,{"encoding":"base64","skipPreflight":false,"preflightCommitment":"confirmed","maxRetries":0}])).await?.as_str().map(str::to_owned).ok_or_else(||err("missing signature"))
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
fn prepared(intent: &VaultIntent) -> PreparedOperation {
    let now = chrono::Utc::now();
    serde_json::from_value(json!({"operation":"kamino.lending.validator","input":intent,"binding":{"synthetic_validator":true},"summary":"Synthetic local Kamino lending test","checks":[],"warnings":[],"observed_at":now.to_rfc3339(),"expires_at":(now+chrono::Duration::minutes(10)).to_rfc3339(),"valuation":null,"intent":{"debits":[],"maximum_costs":[],"expected":[],"locks":[]},"operation_version":1})).unwrap()
}
async fn execute(
    protocol: &LendingProtocol,
    runtime: &SolanaRuntime,
    rpc: Arc<Rpc>,
    signer: &SolanaSigner,
    intent: &VaultIntent,
) -> Result<VerifiedVaultEffects> {
    let candidate = protocol
        .build_candidate(runtime, intent, signer.address())
        .await?;
    protocol
        .validate_candidate(runtime, intent, signer.address(), &candidate)
        .await?;
    let frozen = runtime
        .freeze_instructions(
            &candidate.instructions,
            &candidate.lookup_tables,
            &candidate.blockhash,
            signer,
            candidate.last_valid_block_height,
            intent.maximum_network_fee_lamports,
            Some("kamino-synthetic-validator".into()),
            &[],
        )
        .await?;
    runtime.simulate(&frozen).await?;
    let result = runtime
        .dispatch_with_sender(
            &frozen,
            signer,
            &prepared(intent),
            Arc::new(Journal::default()),
            &Sender(rpc.clone()),
        )
        .await?;
    let signature = result.data["chain_signature"]
        .as_str()
        .ok_or_else(|| err("missing journaled signature"))?;
    let mut finalized = false;
    for _ in 0..120 {
        let result = rpc
            .request(
                "getSignatureStatuses",
                json!([[signature],{"searchTransactionHistory":true}]),
            )
            .await?;
        if result["value"][0]["confirmationStatus"] == "finalized" {
            finalized = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if !finalized {
        return Err(err("local transaction did not finalize"));
    }
    let proof = runtime.observe_finalized_proof(signature).await?;
    let evidence = protocol
        .verify_finalized(runtime, intent, signer.address(), &candidate, &proof)
        .await?;
    let path = std::env::var("GOFIN_KAMINO_LENDING_VALIDATOR_EVIDENCE").unwrap();
    let mut rows: Vec<Value> = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    rows.push(json!({"action":intent.action,"signature":signature,"slot":proof.slot,"fee_lamports":proof.fee_lamports,"finalized":true,"verified":true,"shares_delta":evidence.shares_delta,"native_shares_delta":evidence.native_shares_delta,"token_deltas":evidence.token_deltas,"evidence":evidence.evidence,"position":evidence.position,"quote":candidate.quote}));
    fs::write(path, serde_json::to_vec_pretty(&rows).unwrap()).unwrap();
    Ok(evidence)
}
#[tokio::test]
#[ignore = "requires owned validator with cloned public programs; run scripts/verify-kamino-lending-validator.py"]
async fn deposit_stake_partial_and_full_native_withdrawal() -> Result<()> {
    let url = std::env::var("GOFIN_KAMINO_LENDING_VALIDATOR_RPC").expect("run validator script");
    assert!(url.starts_with("http://127.0.0.1:"));
    let manifest: Value = serde_json::from_slice(
        &fs::read(std::env::var("GOFIN_KAMINO_LENDING_VALIDATOR_MANIFEST").unwrap()).unwrap(),
    )
    .unwrap();
    let signer = SolanaSigner::parse(
        &fs::read_to_string(manifest["wallet_key"].as_str().unwrap()).unwrap(),
    )?;
    assert_eq!(signer.address(), manifest["wallet"].as_str().unwrap());
    let rpc = Arc::new(Rpc {
        url,
        client: reqwest::Client::new(),
    });
    let runtime = SolanaRuntime::new(rpc.clone());
    let protocol = LendingProtocol::default();
    let vault = protocol
        .vault(&runtime, manifest["vault"].as_str().unwrap())
        .await?;
    let position = protocol
        .position(&runtime, signer.address(), &vault.address)
        .await?;
    assert_eq!(position.wallet_shares_atoms, "0");
    assert_eq!(position.staked_shares_atoms, "0");
    let mut intent = VaultIntent {
        wallet: "synthetic-validator".into(),
        family: VaultFamily::Lending,
        action: VaultAction::Deposit,
        vault: vault.address.clone(),
        amounts: vec![TokenAmount {
            token: vault.assets[0].clone(),
            atoms: Some((50 * 10u64.pow(u32::from(vault.assets[0].decimals))).to_string()),
        }],
        shares_atoms: None,
        minimum_shares_atoms: Some("1".into()),
        maximum_network_fee_lamports: 1_000_000,
        maximum_account_funding_lamports: 100_000_000,
        stake: true,
    };
    let deposited = execute(&protocol, &runtime, rpc.clone(), &signer, &intent).await?;
    let minted = deposited.shares_delta.as_str().parse::<u64>().unwrap();
    assert!(minted > 0);
    let position = protocol
        .position(&runtime, signer.address(), &vault.address)
        .await?;
    assert!(position.staked_shares_atoms.parse::<u64>().unwrap() > 0);
    intent.action = VaultAction::Withdraw;
    intent.amounts.clear();
    intent.minimum_shares_atoms = None;
    intent.stake = false;
    intent.shares_atoms = Some((minted / 2).to_string());
    execute(&protocol, &runtime, rpc.clone(), &signer, &intent).await?;
    let position = protocol
        .position(&runtime, signer.address(), &vault.address)
        .await?;
    let remaining = position.wallet_shares_atoms.parse::<u64>().unwrap()
        + position.staked_shares_atoms.parse::<u64>().unwrap()
        + position.pending_shares_atoms.parse::<u64>().unwrap();
    assert!(remaining > 0);
    intent.shares_atoms = Some(remaining.to_string());
    execute(&protocol, &runtime, rpc, &signer, &intent).await?;
    let position = protocol
        .position(&runtime, signer.address(), &vault.address)
        .await?;
    assert_eq!(position.wallet_shares_atoms, "0");
    assert_eq!(position.staked_shares_atoms, "0");
    assert_eq!(position.pending_shares_atoms, "0");
    let user = &position.protocol_state["farms"][0]["user"];
    assert_ne!(user["active_scaled"], "0");
    assert!(!user["fractional_claims"]["active"].is_null());
    let inventory = protocol.positions(&runtime, signer.address()).await?;
    assert!(
        inventory
            .positions
            .iter()
            .any(|p| p.vault.address == vault.address)
    );
    Ok(())
}

/// Synthetic account configuration, deployed cloned Farms program. The raw
/// instruction deliberately bypasses adapter eligibility to establish why
/// additional stakes need restrictions on existing custody.
#[tokio::test]
#[ignore = "requires scripts/verify-kamino-lending-validator.py --farm-lock-semantics"]
async fn farm_stake_resets_existing_continuous_lock_and_pending_warmup() -> Result<()> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use gofin_kamino::{common, farms};
    use gofin_solana::{NormalizedInstruction, transaction::NormalizedAccountMeta};
    let url = std::env::var("GOFIN_KAMINO_LENDING_VALIDATOR_RPC").expect("run validator script");
    assert!(url.starts_with("http://127.0.0.1:"));
    let manifest: Value = serde_json::from_slice(
        &fs::read(std::env::var("GOFIN_KAMINO_LENDING_VALIDATOR_MANIFEST").unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["synthetic_lock_semantics_fixture"], true);
    let owner = manifest["wallet"].as_str().unwrap();
    let rpc = Arc::new(Rpc {
        url,
        client: reqwest::Client::new(),
    });
    let runtime = SolanaRuntime::new(rpc);
    let farm = farms::farm(
        &runtime,
        manifest["farm"].as_str().unwrap(),
        manifest["shares"].as_str().unwrap(),
    )
    .await?;
    let before = farms::user_farms(&runtime, owner, &farm).await?;
    assert_eq!(farm.locking_mode, 1);
    assert!(before.active_scaled != "0" && before.pending_deposit_scaled != "0");
    let ata =
        gofin_solana::associated_token_address(owner, &farm.token.mint, &farm.token.token_program)?;
    let meta = |address: &str, signer, writable| NormalizedAccountMeta {
        address: address.into(),
        signer,
        writable,
    };
    let scope = if farm.scope_prices == "11111111111111111111111111111111" {
        farms::PROGRAM_ID
    } else {
        &farm.scope_prices
    };
    let mut data = vec![206, 176, 202, 18, 200, 209, 179, 108];
    data.extend(10u64.to_le_bytes());
    let stake = NormalizedInstruction {
        program_id: farms::PROGRAM_ID.into(),
        accounts: vec![
            meta(owner, true, false),
            meta(&before.address, false, true),
            meta(&farm.address, false, true),
            meta(&farm.vault, false, true),
            meta(&ata, false, true),
            meta(&farm.token.mint, false, false),
            meta(scope, false, false),
            meta(farms::TOKEN_PROGRAM, false, false),
        ],
        data_base64: STANDARD.encode(data),
    };
    let (blockhash, _) = common::latest_blockhash(&runtime).await?;
    let result = runtime
        .simulate_unsigned_instructions_with_accounts(
            owner,
            &[stake],
            &[],
            &blockhash,
            &[farm.address.clone(), before.address.clone()],
        )
        .await?;
    let after_farm = farms::decode_farm(&farm.address, &result.accounts[0].as_ref().unwrap().data)?;
    let after = farms::decode_user(
        &before.address,
        owner,
        &after_farm,
        &result.accounts[1].as_ref().unwrap().data,
    )?;
    eprintln!(
        "SYNTHETIC CLONED FARM MATURITY {}",
        json!({"before":before,"after":after})
    );
    assert!(after.last_stake > before.last_stake);
    assert!(after.pending_deposit_until > before.pending_deposit_until);
    assert!(after.active_atoms >= before.active_atoms);
    assert!(after.pending_deposit_atoms > before.pending_deposit_atoms);
    Ok(())
}
