//! Full shared-settlement regressions for hostile wallet deltas and net
//! Token-2022 receipts. These fixtures are synthetic and never contact RPC.

use gofin_domain::{
    FinancialEffect, OperationRecord, PreparedOperation, SignedQuantity, Status, Submission,
};
use gofin_meteora::{
    protocol::{
        MeteoraAction, MeteoraIntent, PoolInfo, ProtocolCandidate, ProtocolFamily, QuoteInfo,
        TokenInfo, VerifiedProtocolEffects, VerifiedTokenDelta,
    },
    settlement,
};
use gofin_solana::{
    FinalizedReceiptProof,
    token::{TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID},
};
use serde_json::{Value, json};

const WALLET: &str = "11111111111111111111111111111111";
const WSOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const OTHER_MINT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
const MESSAGE_HASH: &str = "approved-message-hash";
const SIGNATURE: &str = "finalized-synthetic-signature";
const SLOT: u64 = 42;
const FEE_LAMPORTS: u64 = 5_000;

struct SwapFixture {
    record: OperationRecord,
    submission: Submission,
    intent: MeteoraIntent,
    candidate: ProtocolCandidate,
    verified: VerifiedProtocolEffects,
    proof: FinalizedReceiptProof,
}

fn token(mint: &str, decimals: u8, program: &str, transfer_fee: Option<Value>) -> TokenInfo {
    TokenInfo {
        mint: mint.into(),
        decimals,
        token_program: program.into(),
        transfer_fee,
    }
}

fn token_balance(index: u64, mint: &str, program: &str, amount: &str, decimals: u8) -> Value {
    json!({
        "accountIndex": index,
        "mint": mint,
        "owner": WALLET,
        "programId": program,
        "uiTokenAmount": {"amount": amount, "decimals": decimals}
    })
}

fn delta(mint: &str, decimals: u8, program: &str, atoms: &str) -> VerifiedTokenDelta {
    VerifiedTokenDelta {
        mint: mint.into(),
        decimals,
        token_program: program.into(),
        atoms: SignedQuantity::new(atoms).unwrap(),
    }
}

fn swap_fixture() -> SwapFixture {
    let pool = PoolInfo {
        family: ProtocolFamily::Dlmm,
        address: "synthetic-pool".into(),
        token_a: token(WSOL, 9, TOKEN_PROGRAM_ID, None),
        token_b: token(
            USDC,
            6,
            TOKEN_2022_PROGRAM_ID,
            Some(json!({"epoch":0,"basis_points":1000,"maximum_fee_atoms":"7"})),
        ),
        active: true,
        current_bin: Some(100),
        fee_bps: Some(10),
        protocol_state: json!({"fixture":true}),
    };
    let intent = MeteoraIntent {
        wallet: "test-wallet".into(),
        family: ProtocolFamily::Dlmm,
        action: MeteoraAction::Swap,
        pool: pool.address.clone(),
        position: None,
        input_mint: Some(WSOL.into()),
        output_mint: Some(USDC.into()),
        token_a_mint: None,
        token_b_mint: None,
        amount_atoms: Some("1000".into()),
        minimum_output_atoms: Some("90".into()),
        token_a_atoms: None,
        token_b_atoms: None,
        minimum_token_a_atoms: None,
        minimum_token_b_atoms: None,
        remove_share_bps: None,
        bin_lower: None,
        bin_upper: None,
        liquidity_distribution: None,
        maximum_slippage_bps: 100,
        maximum_network_fee_lamports: FEE_LAMPORTS,
        maximum_account_funding_lamports: 0,
    };
    let candidate = ProtocolCandidate {
        quote: QuoteInfo {
            family: ProtocolFamily::Dlmm,
            action: MeteoraAction::Swap,
            pool: pool.address.clone(),
            position: None,
            token_a_debit_atoms: Some("1000".into()),
            token_b_debit_atoms: None,
            token_a_credit_atoms: None,
            // The protocol quote is gross. Settlement must book the 93 atoms
            // actually received after the 7 atom capped transfer fee.
            token_b_credit_atoms: Some("100".into()),
            receipt_delta_atoms: None,
            fees_a_atoms: None,
            fees_b_atoms: None,
            rewards: vec![],
            expires_at: "2030-01-01T00:00:00Z".into(),
            price_impact_bps: Some(0),
            native_cash_delta: None,
            protocol_details: json!({
                "transfer_fee_schedules":[{
                    "mint":USDC,
                    "older":{"epoch":0,"basis_points":1000,"maximum_fee_atoms":"7"},
                    "newer":{"epoch":0,"basis_points":1000,"maximum_fee_atoms":"7"}
                }]
            }),
        },
        instructions: vec![],
        lookup_tables: vec![],
        blockhash: "synthetic-blockhash".into(),
        last_valid_block_height: 100,
        stable_position_address: None,
        required_secondary_signers: vec![],
        metadata: json!({"refundable_rent_accounts":[]}),
    };
    let prepared = PreparedOperation {
        operation: "meteora.swap".into(),
        input: json!({}),
        binding: json!({
            "wallet_address":WALLET,
            "message_sha256":MESSAGE_HASH,
            "pool":pool
        }),
        summary: "synthetic swap".into(),
        checks: vec![],
        warnings: vec![],
        observed_at: "2026-10-14T00:00:00Z".into(),
        expires_at: "2030-01-01T00:00:00Z".into(),
        valuation: None,
        intent: gofin_domain::FinancialIntent::default(),
        operation_version: 1,
        action_deadline: None,
    };
    let record = OperationRecord {
        id: "swap-operation".into(),
        request_id: "swap-request".into(),
        fingerprint: "swap-fingerprint".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
        input: json!({}),
        status: Status::Submitted,
        created_at: "2026-10-14T00:00:00Z".into(),
        updated_at: "2026-10-14T00:00:00Z".into(),
        preview: Some(prepared),
        approval: None,
        submission: None,
        dispatch_evidence: None,
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let submission = Submission {
        kind: "solana.transaction".into(),
        reference: SIGNATURE.into(),
        data: json!({
            "message_sha256":MESSAGE_HASH,
            "wallet_signature":SIGNATURE
        }),
    };
    let proof = FinalizedReceiptProof {
        signature: SIGNATURE.into(),
        slot: SLOT,
        fee_lamports: FEE_LAMPORTS,
        failed: false,
        message_sha256: MESSAGE_HASH.into(),
        account_keys: vec![
            WALLET.into(),
            "wallet-wsol-account".into(),
            "wallet-usdc-account".into(),
        ],
        message_instructions: vec![],
        required_signatures: vec![],
        pre_balances_lamports: vec![100_000, 0, 0],
        post_balances_lamports: vec![95_000, 0, 0],
        pre_token_balances: vec![
            token_balance(1, WSOL, TOKEN_PROGRAM_ID, "1000", 9),
            token_balance(2, USDC, TOKEN_2022_PROGRAM_ID, "0", 6),
        ],
        post_token_balances: vec![
            token_balance(1, WSOL, TOKEN_PROGRAM_ID, "0", 9),
            token_balance(2, USDC, TOKEN_2022_PROGRAM_ID, "93", 6),
        ],
        loaded_addresses: json!({"writable":[],"readonly":[]}),
        inner_instructions: vec![],
        log_messages: vec![],
        error: None,
    };
    let verified = VerifiedProtocolEffects {
        token_deltas: vec![
            delta(WSOL, 9, TOKEN_PROGRAM_ID, "-1000"),
            delta(USDC, 6, TOKEN_2022_PROGRAM_ID, "93"),
        ],
        native_cash_delta: None,
        collected_fees: vec![],
        collected_rewards: vec![],
        position_asset_deltas: vec![],
        rent_accounts: vec![],
        receipt_delta_atoms: None,
        position: None,
        rent_lamports_delta: SignedQuantity::new("-5000").unwrap(),
        evidence: json!({"fixture":"finalized net receipt"}),
    };
    SwapFixture {
        record,
        submission,
        intent,
        candidate,
        verified,
        proof,
    }
}

#[test]
fn finalized_swap_books_token_2022_net_receipt_after_capped_fee() {
    let fixture = swap_fixture();
    let settled = settlement::verify(
        &fixture.record,
        &fixture.submission,
        &fixture.intent,
        &fixture.candidate,
        &fixture.verified,
        &fixture.proof,
        SLOT,
        FEE_LAMPORTS,
    )
    .expect("a receipt meeting the approved net floor should settle");

    assert!(settled.effects.iter().any(|effect| matches!(
        effect,
        FinancialEffect::Conversion { spent, received, .. }
            if spent.asset.reference == WSOL
                && spent.atoms.as_str() == "1000"
                && received.asset.reference == USDC
                && received.atoms.as_str() == "93"
    )));
}

#[test]
fn finalized_swap_rejects_hostile_hook_debit_from_unapproved_wallet_mint() {
    let mut fixture = swap_fixture();
    fixture
        .proof
        .account_keys
        .push("wallet-other-token-account".into());
    fixture.proof.pre_balances_lamports.push(0);
    fixture.proof.post_balances_lamports.push(0);
    fixture
        .proof
        .pre_token_balances
        .push(token_balance(3, OTHER_MINT, TOKEN_PROGRAM_ID, "50", 6));
    fixture
        .proof
        .post_token_balances
        .push(token_balance(3, OTHER_MINT, TOKEN_PROGRAM_ID, "40", 6));
    fixture
        .verified
        .token_deltas
        .push(delta(OTHER_MINT, 6, TOKEN_PROGRAM_ID, "-10"));

    let error = settlement::verify(
        &fixture.record,
        &fixture.submission,
        &fixture.intent,
        &fixture.candidate,
        &fixture.verified,
        &fixture.proof,
        SLOT,
        FEE_LAMPORTS,
    )
    .expect_err("a finalized writable-hook debit outside the approved input must not settle");

    assert_eq!(
        error
            .details
            .as_deref()
            .and_then(|details| details["check"].as_str()),
        Some("unexpected_token_mint")
    );
}
