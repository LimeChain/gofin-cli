//! Read-only mainnet clones run in an isolated local Solana validator.
//!
//! Run only through `scripts/verify-meteora-validator.py`; the test rejects
//! non-loopback RPC endpoints and uses synthetic keypairs/accounts from the
//! ignored `.context/meteora-validator` directory.

use std::{
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use gofin_domain::{GoFinError, PreparedOperation, Result, Submission, SubmissionJournal};
use gofin_meteora::{
    damm_v2::DammV2Protocol,
    dlmm::DlmmProtocol,
    protocol::{
        MeteoraAction, MeteoraIntent, MeteoraProtocol, ProtocolBuildContext, ProtocolCandidate,
        ProtocolFamily,
    },
};
use gofin_solana::{
    FinalizedReceiptProof, SolanaRuntime, SolanaSigner,
    rpc::SolanaRpc,
    transaction::{FrozenTransaction, TransactionSender},
};
use serde_json::{Value, json};
use tokio::time::sleep;

const DLMM_POOL: &str = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6";
const DAMM_POOL: &str = "8Pm2kZpnxD3hoMmt4bjStX2Pw2Z9abpbHzZxMPqxPmie";
const WSOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

struct LocalValidatorRpc {
    url: String,
    client: reqwest::Client,
}

static PRINTED_SIMULATION_INNER: AtomicBool = AtomicBool::new(false);

impl LocalValidatorRpc {
    fn from_env() -> Self {
        let url = std::env::var("GOFIN_METEORA_VALIDATOR_RPC")
            .expect("validator script must set GOFIN_METEORA_VALIDATOR_RPC");
        assert!(url.starts_with("http://127.0.0.1:") || url.starts_with("http://localhost:"));
        Self {
            url,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl SolanaRpc for LocalValidatorRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "getGenesisHash" {
            // SolanaRuntime's production RPC is mainnet-pinned. The wrapper
            // preserves all other calls to the isolated local validator.
            return Ok(json!(gofin_solana::MAINNET_GENESIS_HASH));
        }
        let response = self
            .client
            .post(&self.url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
            .map_err(|_| validator_error("local validator RPC request failed"))?;
        let value: Value = response
            .json()
            .await
            .map_err(|_| validator_error("local validator returned malformed JSON"))?;
        if method == "simulateTransaction"
            && std::env::var_os("GOFIN_METEORA_VALIDATOR_DEBUG").is_some()
            && value
                .pointer("/result/value/err")
                .is_some_and(|v| !v.is_null())
        {
            eprintln!(
                "LOCAL VALIDATOR SIMULATION FAILURE: {}",
                value.pointer("/result/value").unwrap_or(&Value::Null)
            );
        }
        if method == "simulateTransaction"
            && std::env::var_os("GOFIN_METEORA_VALIDATOR_DEBUG_INNER").is_some()
            && !PRINTED_SIMULATION_INNER.swap(true, Ordering::Relaxed)
            && value
                .pointer("/result/value/err")
                .is_some_and(Value::is_null)
        {
            eprintln!(
                "LOCAL VALIDATOR INNER INSTRUCTIONS: {}",
                value
                    .pointer("/result/value/innerInstructions")
                    .unwrap_or(&Value::Null)
            );
        }
        if let Some(error) = value.get("error") {
            return Err(validator_error(&format!(
                "local validator rejected {method}: {error}"
            )));
        }
        value
            .get("result")
            .cloned()
            .ok_or_else(|| validator_error("local validator response omitted result"))
    }
}

fn validator_error(message: &str) -> GoFinError {
    GoFinError::new(
        "METEORA_VALIDATOR_TEST_RPC",
        "test",
        message,
        "Inspect the isolated validator log in .context/meteora-validator.",
    )
}

#[derive(Default)]
struct TestJournal(Mutex<Vec<Submission>>);

impl SubmissionJournal for TestJournal {
    fn assert_authorized(&self) -> Result<()> {
        Ok(())
    }

    fn persist_submission(&self, submission: &Submission) -> Result<()> {
        self.0.lock().unwrap().push(submission.clone());
        Ok(())
    }

    fn persist_dispatch_evidence(&self, _submission: &Submission, _evidence: &Value) -> Result<()> {
        Ok(())
    }
}

struct LocalSender(Arc<LocalValidatorRpc>);

#[async_trait]
impl TransactionSender for LocalSender {
    async fn send_signed(
        &self,
        signed_transaction_base64: &str,
        _frozen: &FrozenTransaction,
    ) -> Result<String> {
        self.0
            .request(
                "sendTransaction",
                json!([signed_transaction_base64,{"encoding":"base64","skipPreflight":false,"preflightCommitment":"confirmed","maxRetries":0}]),
            )
            .await?
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| validator_error("local validator omitted the transaction signature"))
    }
}

fn signer_from_env(name: &str) -> SolanaSigner {
    let path = std::env::var(name).expect("validator script must set signer key paths");
    let key = fs::read_to_string(path).expect("synthetic keypair file must be readable");
    SolanaSigner::parse(&key).expect("synthetic keypair must parse")
}

fn intent(
    family: ProtocolFamily,
    action: MeteoraAction,
    pool: &str,
    wallet: &str,
) -> MeteoraIntent {
    MeteoraIntent {
        wallet: wallet.into(),
        family,
        action,
        pool: pool.into(),
        position: None,
        input_mint: None,
        output_mint: None,
        token_a_mint: None,
        token_b_mint: None,
        amount_atoms: None,
        minimum_output_atoms: None,
        token_a_atoms: None,
        token_b_atoms: None,
        minimum_token_a_atoms: None,
        minimum_token_b_atoms: None,
        remove_share_bps: None,
        bin_lower: None,
        bin_upper: None,
        liquidity_distribution: None,
        maximum_slippage_bps: 100,
        maximum_network_fee_lamports: 10_000_000,
        // Synthetic local validator covers the position account plus one
        // initialized 9,016-byte bin array; no production intent is implied.
        maximum_account_funding_lamports: 500_000_000,
    }
}

fn context(
    wallet: &SolanaSigner,
    position_signer: Option<&SolanaSigner>,
    wsol_signer: Option<&SolanaSigner>,
) -> ProtocolBuildContext {
    ProtocolBuildContext {
        wallet_address: wallet.address().into(),
        position_signer_address: position_signer.map(|s| s.address().into()),
        temporary_wsol_signer_address: wsol_signer.map(|s| s.address().into()),
    }
}

fn prepared(intent: &MeteoraIntent) -> PreparedOperation {
    let now = chrono::Utc::now();
    serde_json::from_value(json!({
        "operation":"meteora.validator.integration",
        "input":intent,
        "binding":{"validator_test":true},
        "summary":"Synthetic local-validator Meteora integration test",
        "checks":[],
        "warnings":[],
        "observed_at":now.to_rfc3339(),
        "expires_at":(now + chrono::Duration::minutes(10)).to_rfc3339(),
        "valuation":null,
        "intent":{"debits":[],"maximum_costs":[],"expected":[],"locks":[]},
        "operation_version":1
    }))
    .expect("test PreparedOperation schema")
}

async fn wait_finalized(rpc: &dyn SolanaRpc, signature: &str) -> Result<()> {
    for _ in 0..120 {
        let status = rpc
            .request(
                "getSignatureStatuses",
                json!([[signature],{"searchTransactionHistory":true}]),
            )
            .await?;
        if let Some(value) = status.pointer("/value/0").filter(|v| !v.is_null()) {
            if !value.get("err").is_none_or(Value::is_null) {
                return Err(validator_error("local validator transaction failed"));
            }
            if value.get("confirmationStatus").and_then(Value::as_str) == Some("finalized") {
                return Ok(());
            }
        }
        sleep(Duration::from_millis(500)).await;
    }
    Err(validator_error(
        "local validator transaction did not finalize in 60 seconds",
    ))
}

async fn execute(
    runtime: &SolanaRuntime,
    rpc: Arc<LocalValidatorRpc>,
    protocol: &dyn MeteoraProtocol,
    intent: &MeteoraIntent,
    context: &ProtocolBuildContext,
    wallet: &SolanaSigner,
    secondary: &[Arc<SolanaSigner>],
) -> Result<(ProtocolCandidate, FinalizedReceiptProof)> {
    let candidate = protocol.build_candidate(runtime, intent, context).await?;
    protocol
        .validate_candidate(runtime, intent, context, &candidate)
        .await?;
    let frozen = runtime
        .freeze_instructions(
            &candidate.instructions,
            &candidate.lookup_tables,
            &candidate.blockhash,
            wallet,
            candidate.last_valid_block_height,
            intent.maximum_network_fee_lamports,
            Some("meteora-local-validator-test".into()),
            &candidate.required_secondary_signers,
        )
        .await?;
    assert!(frozen.instructions().len() <= 64);
    runtime.simulate(&frozen).await?;
    let journal: Arc<dyn SubmissionJournal> = Arc::new(TestJournal::default());
    let sender = LocalSender(rpc.clone());
    let submitted = runtime
        .dispatch_with_secondary_signers(
            &frozen,
            wallet,
            secondary,
            &prepared(intent),
            journal,
            &sender,
        )
        .await?;
    let signature = submitted
        .data
        .get("chain_signature")
        .and_then(Value::as_str)
        .ok_or_else(|| validator_error("local transaction signature was not journaled"))?;
    wait_finalized(rpc.as_ref(), signature).await?;
    let proof = runtime.observe_finalized_proof(signature).await?;
    let evidence_path = std::env::var_os("GOFIN_METEORA_VALIDATOR_EXECUTION_EVIDENCE")
        .map(std::path::PathBuf::from);
    if let Some(path) = &evidence_path {
        let mut rows = if path.exists() {
            serde_json::from_slice::<Value>(
                &fs::read(path)
                    .map_err(|_| validator_error("unable to read local execution evidence"))?,
            )
            .ok()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default()
        } else {
            Vec::new()
        };
        rows.push(json!({
            "family": format!("{:?}", intent.family),
            "action": format!("{:?}", intent.action),
            "signature": signature,
            "slot": proof.slot,
            "finalized": true,
            "failed": proof.failed,
            "fee_lamports": proof.fee_lamports,
            "message_sha256": proof.message_sha256,
            "verified": false,
        }));
        fs::write(
            path,
            serde_json::to_vec_pretty(&rows)
                .map_err(|_| validator_error("unable to serialize local execution evidence"))?,
        )
        .map_err(|_| validator_error("unable to write local execution evidence"))?;
    }
    protocol
        .verify_finalized(runtime, intent, context, &candidate, &proof)
        .await?;
    if let Some(path) = evidence_path {
        let mut rows = serde_json::from_slice::<Value>(
            &fs::read(&path)
                .map_err(|_| validator_error("unable to read local execution evidence"))?,
        )
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
        if let Some(row) = rows.last_mut() {
            row["verified"] = json!(true);
        }
        fs::write(
            path,
            serde_json::to_vec_pretty(&rows)
                .map_err(|_| validator_error("unable to serialize local execution evidence"))?,
        )
        .map_err(|_| validator_error("unable to write local execution evidence"))?;
    }
    Ok((candidate, proof))
}

async fn runtime_and_keys() -> (
    SolanaRuntime,
    Arc<LocalValidatorRpc>,
    SolanaSigner,
    SolanaSigner,
    SolanaSigner,
) {
    let rpc = Arc::new(LocalValidatorRpc::from_env());
    let wallet = signer_from_env("GOFIN_METEORA_VALIDATOR_WALLET_KEY");
    let position = signer_from_env("GOFIN_METEORA_VALIDATOR_POSITION_KEY");
    let wsol = signer_from_env("GOFIN_METEORA_VALIDATOR_WSOL_KEY");
    let account = rpc
        .request(
            "getAccountInfo",
            json!([wallet.address(),{"encoding":"base64"}]),
        )
        .await
        .unwrap();
    assert!(
        !account.pointer("/value").unwrap().is_null(),
        "synthetic wallet must exist in genesis"
    );
    (SolanaRuntime::new(rpc.clone()), rpc, wallet, position, wsol)
}

#[tokio::test]
#[ignore = "read-only diagnostic for the saved synthetic validator position"]
async fn dlmm_saved_position_composition_reads_on_local_validator() -> Result<()> {
    let (runtime, _rpc, wallet, position_signer, _wsol) = runtime_and_keys().await;
    let observed = DlmmProtocol::new()
        .position(&runtime, position_signer.address())
        .await?;
    assert_eq!(observed.owner, wallet.address());
    assert!(observed.token_a_atoms.is_some());
    assert!(observed.token_b_atoms.is_some());
    Ok(())
}

#[tokio::test]
#[ignore = "requires scripts/verify-meteora-validator.py and cloned public programs"]
async fn dlmm_native_swaps_and_full_position_lifecycle_on_local_validator() -> Result<()> {
    let (runtime, rpc, wallet, position_signer, wsol_signer) = runtime_and_keys().await;
    let protocol = DlmmProtocol::new();
    let pool = protocol.pool(&runtime, DLMM_POOL).await?;
    assert_eq!(pool.token_a.mint, WSOL);
    assert_eq!(pool.token_b.mint, USDC);

    for (input_mint, output_mint, amount) in
        [("native", USDC, "100000000"), (USDC, "native", "100000")]
    {
        let mut swap = intent(
            ProtocolFamily::Dlmm,
            MeteoraAction::Swap,
            DLMM_POOL,
            wallet.address(),
        );
        swap.input_mint = Some(input_mint.into());
        swap.output_mint = Some(output_mint.into());
        swap.amount_atoms = Some(amount.into());
        swap.token_a_mint = None;
        swap.token_b_mint = None;
        let quote_context = context(&wallet, None, Some(&wsol_signer));
        let quote = protocol.quote(&runtime, &swap, &quote_context).await?;
        let gross_net = if output_mint == "native" {
            quote.native_cash_delta.as_deref().unwrap_or("0")
        } else if output_mint == USDC {
            quote.token_b_credit_atoms.as_deref().unwrap_or("0")
        } else {
            quote.token_a_credit_atoms.as_deref().unwrap_or("0")
        };
        let quoted = gross_net
            .trim_start_matches('-')
            .parse::<u64>()
            .unwrap_or(0);
        assert!(
            quoted > 0,
            "swap quote must return an exact positive output"
        );
        swap.minimum_output_atoms = Some((quoted * 9_900 / 10_000).to_string());
        let ctx = context(&wallet, None, Some(&wsol_signer));
        let (_, proof) = execute(
            &runtime,
            rpc.clone(),
            &protocol,
            &swap,
            &ctx,
            &wallet,
            &[Arc::new(signer_from_env(
                "GOFIN_METEORA_VALIDATOR_WSOL_KEY",
            ))],
        )
        .await?;
        assert!(!proof.failed);
    }

    // The prior swaps may move the active bin; use a fresh account snapshot
    // for the explicitly pinned liquidity range.
    let pool = protocol.pool(&runtime, DLMM_POOL).await?;
    let active = pool.current_bin.expect("DLMM pool exposes current bin");
    let lower = active - 1;
    // Keep explicit Y liquidity below active and X liquidity above active,
    // with both sides in the active bin so the follow-up opposite-side swap
    // earns a real fee for this position.
    let upper = active + 1;
    let distribution = json!([
        {"bin_id":lower,"distribution_x":0,"distribution_y":5000},
        {"bin_id":active,"distribution_x":5000,"distribution_y":5000},
        {"bin_id":upper,"distribution_x":5000,"distribution_y":0}
    ]);
    let open_ctx = context(&wallet, Some(&position_signer), None);
    let mut open = intent(
        ProtocolFamily::Dlmm,
        MeteoraAction::Open,
        DLMM_POOL,
        wallet.address(),
    );
    open.token_a_mint = Some(pool.token_a.mint.clone());
    open.token_b_mint = Some(pool.token_b.mint.clone());
    open.bin_lower = Some(lower);
    open.bin_upper = Some(upper);
    open.token_a_atoms = Some("5000000000".into());
    open.token_b_atoms = Some("150000000".into());
    open.liquidity_distribution = Some(distribution.clone());
    let (opened, proof) = execute(
        &runtime,
        rpc.clone(),
        &protocol,
        &open,
        &open_ctx,
        &wallet,
        &[Arc::new(signer_from_env(
            "GOFIN_METEORA_VALIDATOR_POSITION_KEY",
        ))],
    )
    .await?;
    assert!(!proof.failed);
    let position = opened
        .stable_position_address
        .expect("open binds stable position identity");

    let mut add = intent(
        ProtocolFamily::Dlmm,
        MeteoraAction::Add,
        DLMM_POOL,
        wallet.address(),
    );
    add.position = Some(position.clone());
    add.token_a_mint = Some(pool.token_a.mint.clone());
    add.token_b_mint = Some(pool.token_b.mint.clone());
    add.token_a_atoms = Some("5000000000".into());
    add.token_b_atoms = Some("150000000".into());
    add.bin_lower = Some(lower);
    add.bin_upper = Some(upper);
    add.liquidity_distribution = Some(distribution);
    let no_secondary = context(&wallet, None, None);
    let (_, proof) = execute(
        &runtime,
        rpc.clone(),
        &protocol,
        &add,
        &no_secondary,
        &wallet,
        &[],
    )
    .await?;
    assert!(!proof.failed);

    // A real opposite-side swap traverses the active bins and accrues fees
    // against the synthetic position before collection and removal.
    let mut fee_swap = intent(
        ProtocolFamily::Dlmm,
        MeteoraAction::Swap,
        DLMM_POOL,
        wallet.address(),
    );
    fee_swap.input_mint = Some(USDC.into());
    fee_swap.output_mint = Some("native".into());
    // The synthetic wallet is funded with 5,000 USDC and 30 WSOL. The
    // $100-sized trade produces a representable fee share on this larger
    // locally funded position while remaining comfortably within pool depth.
    fee_swap.amount_atoms = Some("100000000".into());
    fee_swap.minimum_output_atoms = Some("0".into());
    let swap_ctx = context(&wallet, None, Some(&wsol_signer));
    let (_, proof) = execute(
        &runtime,
        rpc.clone(),
        &protocol,
        &fee_swap,
        &swap_ctx,
        &wallet,
        &[Arc::new(signer_from_env(
            "GOFIN_METEORA_VALIDATOR_WSOL_KEY",
        ))],
    )
    .await?;
    assert!(!proof.failed);

    let mut collect = intent(
        ProtocolFamily::Dlmm,
        MeteoraAction::Collect,
        DLMM_POOL,
        wallet.address(),
    );
    collect.position = Some(position.clone());
    collect.token_a_mint = Some(pool.token_a.mint.clone());
    collect.token_b_mint = Some(pool.token_b.mint.clone());
    let collect_quote = protocol.quote(&runtime, &collect, &no_secondary).await?;
    assert!(
        collect_quote
            .fees_a_atoms
            .as_deref()
            .and_then(|amount| amount.parse::<u64>().ok())
            .unwrap_or_default()
            > 0
            || collect_quote
                .fees_b_atoms
                .as_deref()
                .and_then(|amount| amount.parse::<u64>().ok())
                .unwrap_or_default()
                > 0,
        "DLMM collection quote must expose the newly accrued positive fee"
    );
    let (collect_candidate, proof) = execute(
        &runtime,
        rpc.clone(),
        &protocol,
        &collect,
        &no_secondary,
        &wallet,
        &[],
    )
    .await?;
    assert!(!proof.failed);
    let collected = protocol
        .verify_finalized(
            &runtime,
            &collect,
            &no_secondary,
            &collect_candidate,
            &proof,
        )
        .await?;
    assert!(
        collected
            .collected_fees
            .iter()
            .any(|fee| !fee.atoms.is_zero()),
        "the local DLMM lifecycle must collect a nonzero fee earned after opening the funded position"
    );

    for bps in [5_000u16, 10_000u16] {
        let mut remove = intent(
            ProtocolFamily::Dlmm,
            MeteoraAction::Remove,
            DLMM_POOL,
            wallet.address(),
        );
        remove.position = Some(position.clone());
        remove.token_a_mint = Some(pool.token_a.mint.clone());
        remove.token_b_mint = Some(pool.token_b.mint.clone());
        remove.remove_share_bps = Some(bps);
        let removal_quote = protocol.quote(&runtime, &remove, &no_secondary).await?;
        let quoted_a = removal_quote
            .token_a_credit_atoms
            .as_deref()
            .ok_or_else(|| validator_error("DLMM remove quote omitted token A credit"))?
            .parse::<u64>()
            .map_err(|_| validator_error("DLMM token A remove quote was malformed"))?;
        let quoted_b = removal_quote
            .token_b_credit_atoms
            .as_deref()
            .ok_or_else(|| validator_error("DLMM remove quote omitted token B credit"))?
            .parse::<u64>()
            .map_err(|_| validator_error("DLMM token B remove quote was malformed"))?;
        assert!(
            quoted_a > 0 || quoted_b > 0,
            "DLMM remove quote must expose a positive token credit"
        );
        remove.minimum_token_a_atoms = Some((u128::from(quoted_a) * 9_900 / 10_000).to_string());
        remove.minimum_token_b_atoms = Some((u128::from(quoted_b) * 9_900 / 10_000).to_string());
        let (_, proof) = execute(
            &runtime,
            rpc.clone(),
            &protocol,
            &remove,
            &no_secondary,
            &wallet,
            &[],
        )
        .await?;
        assert!(!proof.failed);
    }
    let mut close = intent(
        ProtocolFamily::Dlmm,
        MeteoraAction::Close,
        DLMM_POOL,
        wallet.address(),
    );
    close.position = Some(position);
    close.token_a_mint = Some(pool.token_a.mint);
    close.token_b_mint = Some(pool.token_b.mint);
    let (_, proof) = execute(
        &runtime,
        rpc,
        &protocol,
        &close,
        &no_secondary,
        &wallet,
        &[],
    )
    .await?;
    assert!(!proof.failed);
    Ok(())
}

#[tokio::test]
#[ignore = "requires scripts/verify-meteora-validator.py and cloned public programs"]
async fn damm_v2_native_swap_and_full_position_lifecycle_on_local_validator() -> Result<()> {
    let (runtime, rpc, wallet, position_signer, wsol_signer) = runtime_and_keys().await;
    let protocol = DammV2Protocol::new();
    let pool = protocol.pool(&runtime, DAMM_POOL).await?;
    assert!(pool.token_a.mint == WSOL || pool.token_b.mint == WSOL);
    assert!(pool.token_a.mint == USDC || pool.token_b.mint == USDC);
    let wsol_context = context(&wallet, None, Some(&wsol_signer));
    for (input_mint, output_mint, amount) in
        [("native", USDC, "10000000"), (USDC, "native", "10000")]
    {
        let mut swap = intent(
            ProtocolFamily::DammV2,
            MeteoraAction::Swap,
            DAMM_POOL,
            wallet.address(),
        );
        swap.input_mint = Some(input_mint.into());
        swap.output_mint = Some(output_mint.into());
        swap.amount_atoms = Some(amount.into());
        let quote = protocol.quote(&runtime, &swap, &wsol_context).await?;
        let amount_out = if output_mint == "native" {
            quote
                .native_cash_delta
                .as_deref()
                .unwrap_or("0")
                .trim_start_matches('-')
                .parse::<u64>()
                .unwrap_or(0)
        } else if output_mint == pool.token_a.mint {
            quote
                .token_a_credit_atoms
                .as_deref()
                .unwrap_or("0")
                .parse::<u64>()
                .unwrap_or(0)
        } else {
            quote
                .token_b_credit_atoms
                .as_deref()
                .unwrap_or("0")
                .parse::<u64>()
                .unwrap_or(0)
        };
        assert!(amount_out > 0, "DAMM swap quote must be positive");
        swap.minimum_output_atoms = Some((amount_out * 9_900 / 10_000).to_string());
        let (_, proof) = execute(
            &runtime,
            rpc.clone(),
            &protocol,
            &swap,
            &wsol_context,
            &wallet,
            &[Arc::new(signer_from_env(
                "GOFIN_METEORA_VALIDATOR_WSOL_KEY",
            ))],
        )
        .await?;
        assert!(!proof.failed);
    }

    // Open a real position using explicit token caps, then grow and close that
    // same NFT-backed PDA through finalized program transactions.
    let open_context = context(&wallet, Some(&position_signer), None);
    let mut open = intent(
        ProtocolFamily::DammV2,
        MeteoraAction::Open,
        DAMM_POOL,
        wallet.address(),
    );
    open.token_a_mint = Some(pool.token_a.mint.clone());
    open.token_b_mint = Some(pool.token_b.mint.clone());
    open.token_a_atoms = Some("10000000000".into());
    open.token_b_atoms = Some("5000000000".into());
    let open_quote = protocol.quote(&runtime, &open, &open_context).await?;
    assert!(
        open_quote
            .receipt_delta_atoms
            .as_deref()
            .and_then(|value| value.parse::<i128>().ok())
            .is_some_and(|value| value > 0),
        "DAMM open quote must prove positive liquidity units"
    );
    let (open_candidate, open_proof) = execute(
        &runtime,
        rpc.clone(),
        &protocol,
        &open,
        &open_context,
        &wallet,
        &[Arc::new(signer_from_env(
            "GOFIN_METEORA_VALIDATOR_POSITION_KEY",
        ))],
    )
    .await?;
    assert!(!open_proof.failed);
    let position = open_candidate
        .stable_position_address
        .clone()
        .expect("open must bind its derived position PDA");
    let open_effects = protocol
        .verify_finalized(&runtime, &open, &open_context, &open_candidate, &open_proof)
        .await?;
    let opened = protocol.position(&runtime, &position).await?;
    assert_eq!(opened.owner, wallet.address());
    let quoted_units = open_quote
        .receipt_delta_atoms
        .as_deref()
        .ok_or_else(|| validator_error("DAMM open quote omitted liquidity units"))?;
    assert_eq!(opened.liquidity_units_atoms.as_deref(), Some(quoted_units));
    for (mint, composition) in [
        (pool.token_a.mint.as_str(), opened.token_a_atoms.as_deref()),
        (pool.token_b.mint.as_str(), opened.token_b_atoms.as_deref()),
    ] {
        let composition = composition
            .ok_or_else(|| validator_error("DAMM open position composition was unknown"))?
            .parse::<u64>()
            .map_err(|_| validator_error("DAMM open position composition was malformed"))?;
        let debit = open_effects
            .token_deltas
            .iter()
            .find(|delta| delta.mint == mint)
            .and_then(|delta| delta.atoms.as_str().parse::<i128>().ok())
            .and_then(|delta| delta.checked_neg())
            .and_then(|delta| u64::try_from(delta).ok())
            .ok_or_else(|| {
                validator_error("DAMM finalized open debit was absent or nonnegative")
            })?;
        assert!(
            composition <= debit && debit - composition <= 1,
            "DAMM inverse position composition must match finalized open debit within the protocol's add-up/remove-down rounding atom for mint {mint}; units={quoted_units}, composition={composition}, debit={debit}, position={:?}, proof_deltas={:?}",
            opened.liquidity_units_atoms,
            open_effects.token_deltas
        );
    }

    let mut add = intent(
        ProtocolFamily::DammV2,
        MeteoraAction::Add,
        DAMM_POOL,
        wallet.address(),
    );
    add.position = Some(position.clone());
    add.token_a_mint = Some(pool.token_a.mint.clone());
    add.token_b_mint = Some(pool.token_b.mint.clone());
    add.token_a_atoms = Some("1000000000".into());
    add.token_b_atoms = Some("500000000".into());
    let add_quote = protocol
        .quote(&runtime, &add, &context(&wallet, None, None))
        .await?;
    assert!(
        add_quote
            .receipt_delta_atoms
            .as_deref()
            .and_then(|value| value.parse::<i128>().ok())
            .is_some_and(|value| value > 0)
    );
    let (_, add_proof) = execute(
        &runtime,
        rpc.clone(),
        &protocol,
        &add,
        &context(&wallet, None, None),
        &wallet,
        &[],
    )
    .await?;
    assert!(!add_proof.failed);

    // Trade against the pool after funding the position so collection proves
    // an earned fee. Zero-value/inactive reward slots are not counted as claims.
    let mut fee_swap = intent(
        ProtocolFamily::DammV2,
        MeteoraAction::Swap,
        DAMM_POOL,
        wallet.address(),
    );
    fee_swap.input_mint = Some(pool.token_b.mint.clone());
    fee_swap.output_mint = Some("native".into());
    fee_swap.amount_atoms = Some("1000000".into());
    let fee_swap_quote = protocol.quote(&runtime, &fee_swap, &wsol_context).await?;
    let fee_swap_output = fee_swap_quote
        .native_cash_delta
        .as_deref()
        .ok_or_else(|| validator_error("DAMM fee swap quote omitted its native output"))?
        .trim_start_matches('-')
        .parse::<u64>()
        .map_err(|_| validator_error("DAMM fee swap output was malformed"))?;
    fee_swap.minimum_output_atoms = Some(
        fee_swap_output
            .saturating_mul(9_900)
            .checked_div(10_000)
            .ok_or_else(|| validator_error("DAMM fee swap output floor overflowed"))?
            .to_string(),
    );
    let (_, fee_swap_proof) = execute(
        &runtime,
        rpc.clone(),
        &protocol,
        &fee_swap,
        &wsol_context,
        &wallet,
        &[Arc::new(signer_from_env(
            "GOFIN_METEORA_VALIDATOR_WSOL_KEY",
        ))],
    )
    .await?;
    assert!(!fee_swap_proof.failed);

    let mut collect = intent(
        ProtocolFamily::DammV2,
        MeteoraAction::Collect,
        DAMM_POOL,
        wallet.address(),
    );
    collect.position = Some(position.clone());
    collect.token_a_mint = Some(pool.token_a.mint.clone());
    collect.token_b_mint = Some(pool.token_b.mint.clone());
    let collect_context = context(&wallet, None, None);
    let (collect_candidate, collect_proof) = execute(
        &runtime,
        rpc.clone(),
        &protocol,
        &collect,
        &collect_context,
        &wallet,
        &[],
    )
    .await?;
    assert!(!collect_proof.failed);
    let collected = protocol
        .verify_finalized(
            &runtime,
            &collect,
            &collect_context,
            &collect_candidate,
            &collect_proof,
        )
        .await?;
    assert!(
        collected.collected_fees.iter().any(|fee| fee
            .atoms
            .as_str()
            .parse::<u64>()
            .is_ok_and(|amount| amount > 0)),
        "DAMM collect must prove an earned positive fee after the swap; pool={:?}, position={:?}, collected={:?}",
        pool.protocol_state,
        opened.protocol_state,
        collected.collected_fees
    );

    let remove_context = context(&wallet, None, None);
    let mut partial_remove = intent(
        ProtocolFamily::DammV2,
        MeteoraAction::Remove,
        DAMM_POOL,
        wallet.address(),
    );
    partial_remove.position = Some(position.clone());
    partial_remove.token_a_mint = Some(pool.token_a.mint.clone());
    partial_remove.token_b_mint = Some(pool.token_b.mint.clone());
    partial_remove.remove_share_bps = Some(5_000);
    let partial_quote = protocol
        .quote(&runtime, &partial_remove, &remove_context)
        .await?;
    partial_remove.minimum_token_a_atoms = Some(
        partial_quote
            .token_a_credit_atoms
            .as_deref()
            .ok_or_else(|| validator_error("DAMM partial remove quote omitted token A"))?
            .parse::<u64>()
            .map_err(|_| validator_error("DAMM partial token A quote was malformed"))?
            .saturating_mul(9_900)
            .checked_div(10_000)
            .ok_or_else(|| validator_error("DAMM partial token A floor overflowed"))?
            .to_string(),
    );
    partial_remove.minimum_token_b_atoms = Some(
        partial_quote
            .token_b_credit_atoms
            .as_deref()
            .ok_or_else(|| validator_error("DAMM partial remove quote omitted token B"))?
            .parse::<u64>()
            .map_err(|_| validator_error("DAMM partial token B quote was malformed"))?
            .saturating_mul(9_900)
            .checked_div(10_000)
            .ok_or_else(|| validator_error("DAMM partial token B floor overflowed"))?
            .to_string(),
    );
    let (_, partial_proof) = execute(
        &runtime,
        rpc.clone(),
        &protocol,
        &partial_remove,
        &remove_context,
        &wallet,
        &[],
    )
    .await?;
    assert!(!partial_proof.failed);

    let mut full_remove = intent(
        ProtocolFamily::DammV2,
        MeteoraAction::Remove,
        DAMM_POOL,
        wallet.address(),
    );
    full_remove.position = Some(position.clone());
    full_remove.token_a_mint = Some(pool.token_a.mint.clone());
    full_remove.token_b_mint = Some(pool.token_b.mint.clone());
    let full_quote = protocol
        .quote(&runtime, &full_remove, &remove_context)
        .await?;
    full_remove.minimum_token_a_atoms = Some(
        full_quote
            .token_a_credit_atoms
            .as_deref()
            .ok_or_else(|| validator_error("DAMM full remove quote omitted token A"))?
            .parse::<u64>()
            .map_err(|_| validator_error("DAMM full token A quote was malformed"))?
            .saturating_mul(9_900)
            .checked_div(10_000)
            .ok_or_else(|| validator_error("DAMM full token A floor overflowed"))?
            .to_string(),
    );
    full_remove.minimum_token_b_atoms = Some(
        full_quote
            .token_b_credit_atoms
            .as_deref()
            .ok_or_else(|| validator_error("DAMM full remove quote omitted token B"))?
            .parse::<u64>()
            .map_err(|_| validator_error("DAMM full token B quote was malformed"))?
            .saturating_mul(9_900)
            .checked_div(10_000)
            .ok_or_else(|| validator_error("DAMM full token B floor overflowed"))?
            .to_string(),
    );
    let (_, full_proof) = execute(
        &runtime,
        rpc.clone(),
        &protocol,
        &full_remove,
        &remove_context,
        &wallet,
        &[],
    )
    .await?;
    assert!(!full_proof.failed);
    let empty = protocol.position(&runtime, &position).await?;
    assert_eq!(empty.liquidity_units_atoms.as_deref(), Some("0"));

    let mut close = intent(
        ProtocolFamily::DammV2,
        MeteoraAction::Close,
        DAMM_POOL,
        wallet.address(),
    );
    close.position = Some(position.clone());
    close.token_a_mint = Some(pool.token_a.mint.clone());
    close.token_b_mint = Some(pool.token_b.mint.clone());
    let (_, close_proof) = execute(
        &runtime,
        rpc,
        &protocol,
        &close,
        &remove_context,
        &wallet,
        &[],
    )
    .await?;
    assert!(!close_proof.failed);
    assert!(
        runtime
            .rpc()
            .request("getAccountInfo", json!([position, {"encoding":"base64"}]))
            .await?
            .pointer("/value")
            .is_none_or(Value::is_null),
        "DAMM close must reclaim the empty position account"
    );
    Ok(())
}
