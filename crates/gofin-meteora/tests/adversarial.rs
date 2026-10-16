//! Adversarial contract tests against Meteora's public adapter and protocol
//! seams. These use only synthetic pool/position data and an RPC that refuses
//! network access; validator coverage remains separate.

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use gofin_domain::{
    Adapter, AssetId, GoFinError, OperationRecord, PositionAnchorBaseline, PreparedOperation,
    Result, SignedQuantity, Status, Submission,
};
use gofin_meteora::{
    MeteoraAdapter,
    adapter::SecondarySignerVault,
    protocol::{
        MeteoraAction, MeteoraIntent, MeteoraProtocol, PoolInfo, PoolPage, PoolQuery, PositionInfo,
        PositionInventory, ProtocolBuildContext, ProtocolCandidate, ProtocolFamily, QuoteInfo,
        TokenInfo, VerifiedProtocolEffects, validate_candidate_family,
        validate_candidate_signer_scope,
    },
    settlement,
    types::{OP_ADD, OP_SWAP},
};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use gofin_solana::{
    FinalizedReceiptProof, SolanaSigner, SolanaWalletAdapter,
    rpc::SolanaRpc,
    transaction::{NormalizedAccountMeta, NormalizedInstruction},
};
use serde_json::{Value, json};

const WALLET: &str = "11111111111111111111111111111111";
const OTHER_WALLET: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const WSOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const TOKEN: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

struct OfflineRpc;

#[async_trait]
impl SolanaRpc for OfflineRpc {
    async fn request(&self, method: &str, _params: Value) -> Result<Value> {
        Err(GoFinError::new(
            "TEST_NETWORK_DISABLED",
            "test",
            format!("Unexpected RPC call in adversarial fixture: {method}"),
            "Use only synthetic adapter fixtures in this test.",
        ))
    }
}

struct NoPrices;

#[async_trait]
impl UsdPriceSource for NoPrices {
    async fn quote(&self, _asset: &AssetId) -> Result<UsdQuote> {
        Err(GoFinError::validation(
            "Prices are not used by permission tests.",
        ))
    }
}

struct NoAuxiliarySigners;

impl SecondarySignerVault for NoAuxiliarySigners {
    fn create_or_load(
        &self,
        _logical_operation_key: &str,
        _owner_address: &str,
        _purpose: &str,
    ) -> Result<(String, Arc<SolanaSigner>)> {
        Err(GoFinError::validation("No signer is created in this test."))
    }

    fn load(&self, _opaque_reference: &str) -> Result<Arc<SolanaSigner>> {
        Err(GoFinError::validation("No signer is loaded in this test."))
    }
}

#[derive(Clone)]
struct FixtureProtocol {
    family: ProtocolFamily,
    pool: PoolInfo,
    position: PositionInfo,
}

impl FixtureProtocol {
    fn new(family: ProtocolFamily, owner: &str, position_address: &str) -> Self {
        let pool_address = format!("pool-{}", family.as_str());
        let token_a = TokenInfo {
            mint: WSOL.into(),
            decimals: 9,
            token_program: TOKEN.into(),
            transfer_fee: None,
        };
        let token_b = TokenInfo {
            mint: USDC.into(),
            decimals: 6,
            token_program: TOKEN.into(),
            transfer_fee: None,
        };
        Self {
            family,
            pool: PoolInfo {
                family,
                address: pool_address.clone(),
                token_a: token_a.clone(),
                token_b: token_b.clone(),
                active: true,
                current_bin: Some(100),
                fee_bps: Some(10),
                protocol_state: json!({"fixture":true}),
            },
            position: PositionInfo {
                family,
                address: position_address.into(),
                pool: pool_address,
                owner: owner.into(),
                observation_slot: Some(100),
                rent_accounts: vec![],
                token_a,
                token_b,
                receipt_mint: Some("receipt-mint".into()),
                receipt_atoms: Some("1".into()),
                liquidity_units_atoms: Some("500".into()),
                token_a_atoms: Some("1000".into()),
                token_b_atoms: Some("2000".into()),
                fees_a_atoms: Some("0".into()),
                fees_b_atoms: Some("0".into()),
                rewards: vec![],
                bin_lower: Some(99),
                bin_upper: Some(101),
                protocol_state: json!({"fixture":true}),
            },
        }
    }
}

#[async_trait]
impl MeteoraProtocol for FixtureProtocol {
    fn family(&self) -> ProtocolFamily {
        self.family
    }

    async fn pools(
        &self,
        _runtime: &gofin_solana::SolanaRuntime,
        _query: &PoolQuery,
    ) -> Result<PoolPage> {
        Ok(PoolPage {
            pools: vec![self.pool.clone()],
            complete: true,
            next_cursor: None,
        })
    }

    async fn pool(
        &self,
        _runtime: &gofin_solana::SolanaRuntime,
        _address: &str,
    ) -> Result<PoolInfo> {
        Ok(self.pool.clone())
    }

    async fn positions(
        &self,
        _runtime: &gofin_solana::SolanaRuntime,
        _owner: &str,
    ) -> Result<PositionInventory> {
        Ok(PositionInventory {
            positions: vec![self.position.clone()],
            complete: true,
            observed_at: "2026-10-14T00:00:00Z".into(),
            coverage: "synthetic exact owner".into(),
        })
    }

    async fn position(
        &self,
        _runtime: &gofin_solana::SolanaRuntime,
        _address: &str,
    ) -> Result<PositionInfo> {
        Ok(self.position.clone())
    }

    async fn quote(
        &self,
        _runtime: &gofin_solana::SolanaRuntime,
        _intent: &MeteoraIntent,
        _context: &ProtocolBuildContext,
    ) -> Result<QuoteInfo> {
        Err(GoFinError::validation("Quote is not used by this fixture."))
    }

    async fn build_candidate(
        &self,
        _runtime: &gofin_solana::SolanaRuntime,
        _intent: &MeteoraIntent,
        _context: &ProtocolBuildContext,
    ) -> Result<ProtocolCandidate> {
        Err(GoFinError::validation(
            "No transaction is built by this fixture.",
        ))
    }

    async fn validate_candidate(
        &self,
        _runtime: &gofin_solana::SolanaRuntime,
        intent: &MeteoraIntent,
        _context: &ProtocolBuildContext,
        candidate: &ProtocolCandidate,
    ) -> Result<()> {
        validate_candidate_family(self.family, intent, candidate)
    }

    async fn verify_finalized(
        &self,
        _runtime: &gofin_solana::SolanaRuntime,
        _intent: &MeteoraIntent,
        _context: &ProtocolBuildContext,
        _candidate: &ProtocolCandidate,
        _proof: &FinalizedReceiptProof,
    ) -> Result<VerifiedProtocolEffects> {
        Err(GoFinError::validation(
            "No finalized write is performed by this fixture.",
        ))
    }
}

fn fixture_adapter(dlmm_owner: &str, dlmm_position: &str) -> (MeteoraAdapter, PoolInfo) {
    let wallet = Arc::new(
        SolanaWalletAdapter::new(
            "test-wallet",
            WALLET,
            None,
            Arc::new(OfflineRpc),
            Arc::new(NoPrices),
        )
        .unwrap(),
    );
    let wallets = BTreeMap::from([("test-wallet".to_owned(), wallet)]);
    let dlmm = FixtureProtocol::new(ProtocolFamily::Dlmm, dlmm_owner, dlmm_position);
    let damm = FixtureProtocol::new(ProtocolFamily::DammV2, WALLET, "damm-position");
    let pool = dlmm.pool.clone();
    let adapter = MeteoraAdapter::new(
        wallets,
        vec![Arc::new(dlmm), Arc::new(damm)],
        Arc::new(NoPrices),
        Arc::new(NoAuxiliarySigners),
    )
    .unwrap();
    (adapter, pool)
}

fn add_input(pool: &PoolInfo, position: &str) -> Value {
    json!({
        "wallet":"test-wallet",
        "network":"solana",
        "cluster":"mainnet-beta",
        "family":"dlmm",
        "pool":pool.address,
        "position":position,
        "token_a_mint":pool.token_a.mint,
        "token_b_mint":pool.token_b.mint,
        "token_a_atoms":"100",
        "token_b_atoms":"200",
        "maximum_slippage_bps":100,
        "maximum_network_fee_lamports":5000,
        "maximum_account_funding_lamports":50000
    })
}

fn liquidity_bounds() -> Value {
    json!({
        "maximum_token_a_atoms":"100",
        "maximum_token_b_atoms":"200",
        "maximum_slippage_bps":100,
        "maximum_network_fee_lamports":5000,
        "maximum_account_funding_lamports":25000
    })
}

#[tokio::test]
async fn public_add_plan_permission_rejects_foreign_position_pool_and_asset_pins() {
    let (adapter, pool) = fixture_adapter(WALLET, "position-a");
    let input = add_input(&pool, "position-a");
    let permission = adapter
        .prepare_plan_permission(OP_ADD, &input, &liquidity_bounds())
        .await
        .unwrap();
    assert_eq!(permission.spec.maximum_debits.len(), 3);
    assert_eq!(
        permission.spec.maximum_debits[0].amount.atoms.as_str(),
        "100"
    );
    assert_eq!(
        permission.spec.maximum_debits[1].amount.atoms.as_str(),
        "200"
    );
    assert_eq!(
        permission.spec.maximum_debits[2].amount.asset.reference,
        "native"
    );
    assert_eq!(
        permission.spec.maximum_costs[0].amount.atoms.as_str(),
        "5000"
    );

    let (foreign_owner_adapter, _) = fixture_adapter(OTHER_WALLET, "position-a");
    assert!(
        foreign_owner_adapter
            .prepare_plan_permission(OP_ADD, &input, &liquidity_bounds())
            .await
            .is_err()
    );

    let mut wrong_position = input.clone();
    wrong_position["position"] = json!("different-position");
    assert!(
        adapter
            .prepare_plan_permission(OP_ADD, &wrong_position, &liquidity_bounds())
            .await
            .is_err()
    );

    let mut wrong_pool = input.clone();
    wrong_pool["pool"] = json!("different-pool");
    assert!(
        adapter
            .prepare_plan_permission(OP_ADD, &wrong_pool, &liquidity_bounds())
            .await
            .is_err()
    );

    let mut wrong_mint = input.clone();
    wrong_mint["token_b_mint"] = json!("different-mint");
    assert!(
        adapter
            .prepare_plan_permission(OP_ADD, &wrong_mint, &liquidity_bounds())
            .await
            .is_err()
    );
}

fn swap_input(pool: &PoolInfo, input_mint: &str, output_mint: &str) -> Value {
    json!({
        "wallet":"test-wallet",
        "network":"solana",
        "cluster":"mainnet-beta",
        "family":"dlmm",
        "pool":pool.address,
        "input_mint":input_mint,
        "output_mint":output_mint,
        "amount_atoms":"1000",
        "minimum_output_atoms":"90",
        "maximum_slippage_bps":100,
        "maximum_network_fee_lamports":5000,
        "maximum_account_funding_lamports":500
    })
}

fn swap_bounds() -> Value {
    json!({
        "maximum_input_atoms":"1000",
        "minimum_output_atoms":"90",
        "maximum_slippage_bps":100,
        "maximum_network_fee_lamports":5000,
        "maximum_account_funding_lamports":500
    })
}

#[tokio::test]
async fn named_swap_permission_pins_native_direction_and_amount_and_cost_limits() {
    let (adapter, pool) = fixture_adapter(WALLET, "position-a");
    let native_to_usdc = swap_input(&pool, "native", USDC);
    let permission = adapter
        .prepare_plan_permission(OP_SWAP, &native_to_usdc, &swap_bounds())
        .await
        .unwrap();
    assert!(
        permission
            .possible_credits
            .iter()
            .any(|credit| credit.asset.reference == USDC)
    );

    for (input_mint, output_mint) in [("native", "native"), ("USDC", "native")] {
        let input = swap_input(&pool, input_mint, output_mint);
        assert!(
            adapter
                .prepare_plan_permission(OP_SWAP, &input, &swap_bounds())
                .await
                .is_err()
        );
    }

    let too_much_input = json!({
        "maximum_input_atoms":"1001",
        "minimum_output_atoms":"90",
        "maximum_slippage_bps":100,
        "maximum_network_fee_lamports":5000,
        "maximum_account_funding_lamports":500
    });
    assert!(
        adapter
            .prepare_plan_permission(OP_SWAP, &native_to_usdc, &too_much_input)
            .await
            .is_err()
    );

    let excessive_fee = json!({
        "maximum_input_atoms":"1000",
        "minimum_output_atoms":"90",
        "maximum_slippage_bps":100,
        "maximum_network_fee_lamports":10001,
        "maximum_account_funding_lamports":500
    });
    assert!(
        adapter
            .prepare_plan_permission(OP_SWAP, &native_to_usdc, &excessive_fee)
            .await
            .is_err()
    );
}

#[test]
fn candidate_contract_rejects_unbound_signers_but_accepts_writable_extras() {
    let intent = MeteoraIntent {
        wallet: "test-wallet".into(),
        family: ProtocolFamily::Dlmm,
        action: MeteoraAction::Swap,
        pool: "pool-dlmm".into(),
        position: None,
        input_mint: Some(WSOL.into()),
        output_mint: Some(USDC.into()),
        token_a_mint: None,
        token_b_mint: None,
        amount_atoms: Some("10".into()),
        minimum_output_atoms: Some("1".into()),
        token_a_atoms: None,
        token_b_atoms: None,
        minimum_token_a_atoms: None,
        minimum_token_b_atoms: None,
        remove_share_bps: None,
        bin_lower: None,
        bin_upper: None,
        liquidity_distribution: None,
        maximum_slippage_bps: 100,
        maximum_network_fee_lamports: 10000,
        maximum_account_funding_lamports: 10000,
    };
    let mut candidate: ProtocolCandidate = serde_json::from_value(json!({
        "quote":{
            "family":"dlmm","action":"swap","pool":"pool-dlmm",
            "expires_at":"2030-01-01T00:00:00Z"
        },
        "instructions":[{
            "program_id":"program",
            "accounts":[
                {"address":"writable-extra","signer":false,"writable":true}
            ],
            "data_base64":""
        }],
        "blockhash":"fixture-blockhash",
        "last_valid_block_height":42
    }))
    .unwrap();
    validate_candidate_family(ProtocolFamily::Dlmm, &intent, &candidate).unwrap();
    let wallet_context = ProtocolBuildContext {
        wallet_address: "wallet-address".into(),
        position_signer_address: None,
        temporary_wsol_signer_address: None,
    };
    validate_candidate_signer_scope(&candidate, &wallet_context).unwrap();

    candidate.instructions[0]
        .accounts
        .push(NormalizedAccountMeta {
            address: "unapproved-signer".into(),
            signer: true,
            writable: true,
        });
    assert!(validate_candidate_signer_scope(&candidate, &wallet_context).is_err());

    candidate.required_secondary_signers = vec!["approved-auxiliary".into()];
    candidate.instructions[0].accounts[1].address = "approved-auxiliary".into();
    let bound_context = ProtocolBuildContext {
        wallet_address: "wallet-address".into(),
        position_signer_address: Some("approved-auxiliary".into()),
        temporary_wsol_signer_address: None,
    };
    validate_candidate_signer_scope(&candidate, &bound_context).unwrap();

    candidate.quote.pool = "foreign-pool".into();
    assert!(validate_candidate_family(ProtocolFamily::Dlmm, &intent, &candidate).is_err());
}

#[test]
fn output_floor_and_unapproved_input_debit_are_checked_on_public_boundary_validation() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (adapter, pool) = fixture_adapter(WALLET, "position-a");
        let input = swap_input(&pool, WSOL, USDC);
        let permission = adapter
            .prepare_plan_permission(OP_SWAP, &input, &swap_bounds())
            .await
            .unwrap();
        let wallet_binding = json!({
            "wallet_address":WALLET,
            "pool":pool,
        });
        let make_prepared = |debit: &str, credit: &str| {
            let candidate = ProtocolCandidate {
                quote: QuoteInfo {
                    family: ProtocolFamily::Dlmm,
                    action: MeteoraAction::Swap,
                    pool: pool.address.clone(),
                    position: None,
                    token_a_debit_atoms: Some(debit.into()),
                    token_b_debit_atoms: None,
                    token_a_credit_atoms: None,
                    token_b_credit_atoms: Some(credit.into()),
                    receipt_delta_atoms: None,
                    fees_a_atoms: None,
                    fees_b_atoms: None,
                    rewards: vec![],
                    expires_at: "2030-01-01T00:00:00Z".into(),
                    price_impact_bps: Some(0),
                    native_cash_delta: None,
                    protocol_details: Value::Null,
                },
                instructions: vec![NormalizedInstruction {
                    program_id: "program".into(),
                    accounts: vec![],
                    data_base64: String::new(),
                }],
                lookup_tables: vec![],
                blockhash: "fixture-blockhash".into(),
                last_valid_block_height: 42,
                stable_position_address: None,
                required_secondary_signers: vec![],
                metadata: Value::Null,
            };
            let mut binding = wallet_binding.clone();
            binding["protocol_candidate"] = serde_json::to_value(candidate).unwrap();
            PreparedOperation {
                operation: OP_SWAP.into(),
                input: input.clone(),
                binding,
                summary: String::new(),
                checks: vec![],
                warnings: vec![],
                observed_at: "2026-10-14T00:00:00Z".into(),
                expires_at: "2030-01-01T00:00:00Z".into(),
                valuation: None,
                intent: gofin_domain::FinancialIntent::default(),
                operation_version: 1,
                action_deadline: None,
            }
        };
        assert!(
            adapter
                .validate_boundary_candidate(&permission.spec, &make_prepared("1000", "89"))
                .is_err()
        );
        assert!(
            adapter
                .validate_boundary_candidate(&permission.spec, &make_prepared("1001", "90"))
                .is_err()
        );
        adapter
            .validate_boundary_candidate(&permission.spec, &make_prepared("1000", "90"))
            .unwrap();
    });
}

#[test]
fn duplicate_owner_token_rows_are_rejected_and_failed_receipts_charge_only_fee() {
    let mut proof = empty_proof(false);
    proof.account_keys = vec!["wallet-ata".into()];
    proof.pre_token_balances = vec![token_balance(0, "10"), token_balance(0, "10")];
    proof.post_token_balances = vec![token_balance(0, "10")];
    assert!(proof.all_owner_token_deltas_verified(WALLET).is_err());

    let mut failed = empty_proof(true);
    failed.signature = "signature-failed".into();
    failed.slot = 12;
    failed.fee_lamports = 5000;
    failed.message_sha256 = "message-hash".into();
    let submission = Submission {
        kind: "solana.transaction".into(),
        reference: "signature-failed".into(),
        data: json!({
            "message_sha256":"message-hash",
            "wallet_signature":"signature-failed"
        }),
    };
    let verified = settlement::verify_failed(&submission, &failed, WALLET, 6000, 5000, 12).unwrap();
    assert!(matches!(
        verified.effects.as_slice(),
        [gofin_domain::FinancialEffect::Cost { amount, category, .. }]
            if amount.asset.reference == "native"
                && amount.atoms.as_str() == "5000"
                && category == "network_fee"
    ));
    assert!(settlement::verify_failed(&submission, &failed, WALLET, 4999, 5000, 12).is_err());
}

#[test]
fn one_sided_collect_settles_and_zero_classification_is_rejected() {
    let fixture = FixtureProtocol::new(ProtocolFamily::Dlmm, WALLET, "position-a");
    let pool = fixture.pool;
    let position = fixture.position;
    let intent = MeteoraIntent {
        wallet: "test-wallet".into(),
        family: ProtocolFamily::Dlmm,
        action: MeteoraAction::Collect,
        pool: pool.address.clone(),
        position: Some(position.address.clone()),
        input_mint: None,
        output_mint: None,
        token_a_mint: Some(pool.token_a.mint.clone()),
        token_b_mint: Some(pool.token_b.mint.clone()),
        amount_atoms: None,
        minimum_output_atoms: None,
        token_a_atoms: None,
        token_b_atoms: None,
        minimum_token_a_atoms: Some("0".into()),
        minimum_token_b_atoms: Some("0".into()),
        remove_share_bps: None,
        bin_lower: None,
        bin_upper: None,
        liquidity_distribution: None,
        maximum_slippage_bps: 0,
        maximum_network_fee_lamports: 6_000,
        maximum_account_funding_lamports: 0,
    };
    let candidate = ProtocolCandidate {
        quote: QuoteInfo {
            family: ProtocolFamily::Dlmm,
            action: MeteoraAction::Collect,
            pool: pool.address.clone(),
            position: Some(position.address.clone()),
            token_a_debit_atoms: None,
            token_b_debit_atoms: None,
            token_a_credit_atoms: None,
            token_b_credit_atoms: None,
            receipt_delta_atoms: Some("0".into()),
            fees_a_atoms: None,
            fees_b_atoms: None,
            rewards: vec![],
            expires_at: "2030-01-01T00:00:00Z".into(),
            price_impact_bps: None,
            native_cash_delta: None,
            protocol_details: Value::Null,
        },
        instructions: vec![],
        lookup_tables: vec![],
        blockhash: "fixture-blockhash".into(),
        last_valid_block_height: 42,
        stable_position_address: Some(position.address.clone()),
        required_secondary_signers: vec![],
        metadata: json!({
            "position_units_before_atoms":"500",
            "refundable_rent_accounts":[]
        }),
    };
    let baseline_states = settlement::snapshot_anchors("test-wallet", WALLET, &position).unwrap();
    let baseline = PositionAnchorBaseline {
        version: 1,
        captured_at: "2026-10-14T00:00:00Z".into(),
        operation_id: "collect-operation".into(),
        prepared_binding_hash: "binding-hash".into(),
        scope: vec![gofin_domain::ResourceId(format!(
            "solana:mainnet-beta:{WALLET}"
        ))],
        coverage: vec!["position-a".into()],
        anchors: baseline_states
            .into_iter()
            .filter_map(|state| state.anchor)
            .collect(),
    };
    let prepared = PreparedOperation {
        operation: "meteora.position.collect".into(),
        input: json!({}),
        binding: json!({
            "wallet_address":WALLET,
            "message_sha256":"message-hash",
            "pool":pool
        }),
        summary: "synthetic collect".into(),
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
        id: "collect-operation".into(),
        request_id: "collect-request".into(),
        fingerprint: "fingerprint".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
        input: json!({}),
        status: Status::Submitted,
        created_at: "2026-10-14T00:00:00Z".into(),
        updated_at: "2026-10-14T00:00:00Z".into(),
        preview: Some(prepared),
        approval: None,
        authorization_source: None,
        submission: None,
        dispatch_evidence: None,
        position_baseline: Some(baseline),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let submission = Submission {
        kind: "solana.transaction".into(),
        reference: "collect-signature".into(),
        data: json!({
            "message_sha256":"message-hash",
            "wallet_signature":"collect-signature"
        }),
    };
    let mut proof = empty_proof(false);
    proof.signature = "collect-signature".into();
    proof.slot = 42;
    proof.fee_lamports = 5_000;
    proof.message_sha256 = "message-hash".into();
    proof.account_keys = vec![WALLET.into(), "wallet-usdc-ata".into()];
    proof.pre_balances_lamports = vec![100_000, 0];
    proof.post_balances_lamports = vec![95_000, 0];
    proof.pre_token_balances = vec![token_balance_with_mint(1, USDC, "10", 6)];
    proof.post_token_balances = vec![token_balance_with_mint(1, USDC, "17", 6)];
    let fee = gofin_meteora::protocol::VerifiedTokenDelta {
        mint: USDC.into(),
        decimals: 6,
        token_program: TOKEN.into(),
        atoms: SignedQuantity::new("7").unwrap(),
    };
    let verified = VerifiedProtocolEffects {
        token_deltas: vec![fee.clone()],
        native_cash_delta: None,
        collected_fees: vec![fee],
        collected_rewards: vec![],
        position_asset_deltas: vec![],
        rent_accounts: vec![],
        receipt_delta_atoms: Some(SignedQuantity::new("0").unwrap()),
        position: Some(position),
        rent_lamports_delta: SignedQuantity::new("-5000").unwrap(),
        evidence: json!({"event":"one-sided-fee-claim"}),
    };
    let settled = settlement::verify(
        &record,
        &submission,
        &intent,
        &candidate,
        &verified,
        &proof,
        42,
        5_000,
    )
    .expect("one-sided fee claims with zero opposite-side amounts omitted should settle");
    let mut equal_slot = verified.clone();
    equal_slot.position.as_mut().unwrap().observation_slot = Some(proof.slot);
    settlement::verify(
        &record,
        &submission,
        &intent,
        &candidate,
        &equal_slot,
        &proof,
        42,
        5_000,
    )
    .expect("an observation at the finalized slot is current");
    assert!(settled.effects.iter().any(|effect| matches!(
        effect,
        gofin_domain::FinancialEffect::UsableCredit { amount, .. }
            if amount.asset.reference == USDC && amount.atoms.as_str() == "7"
    )));

    let mut foreign_position = verified.clone();
    foreign_position.position.as_mut().unwrap().owner = OTHER_WALLET.into();
    assert!(
        settlement::verify(
            &record,
            &submission,
            &intent,
            &candidate,
            &foreign_position,
            &proof,
            42,
            5_000,
        )
        .is_err()
    );

    let mut missing_position = verified.clone();
    missing_position.position = None;
    assert!(
        settlement::verify(
            &record,
            &submission,
            &intent,
            &candidate,
            &missing_position,
            &proof,
            42,
            5_000,
        )
        .is_err()
    );

    let mut stale_observation = verified.clone();
    stale_observation
        .position
        .as_mut()
        .unwrap()
        .observation_slot = Some(proof.slot - 1);
    assert!(
        settlement::verify(
            &record,
            &submission,
            &intent,
            &candidate,
            &stale_observation,
            &proof,
            42,
            5_000,
        )
        .is_err()
    );

    let mut unknown_units = verified.clone();
    unknown_units
        .position
        .as_mut()
        .unwrap()
        .liquidity_units_atoms = None;
    assert!(
        settlement::verify(
            &record,
            &submission,
            &intent,
            &candidate,
            &unknown_units,
            &proof,
            42,
            5_000,
        )
        .is_err()
    );

    let mut wrong_a_mint = verified.clone();
    wrong_a_mint.position.as_mut().unwrap().token_a.mint = USDC.into();
    assert!(
        settlement::verify(
            &record,
            &submission,
            &intent,
            &candidate,
            &wrong_a_mint,
            &proof,
            42,
            5_000,
        )
        .is_err()
    );

    let mut wrong_a_program = verified.clone();
    wrong_a_program
        .position
        .as_mut()
        .unwrap()
        .token_a
        .token_program = "SysvarRent111111111111111111111111111111111".into();
    assert!(
        settlement::verify(
            &record,
            &submission,
            &intent,
            &candidate,
            &wrong_a_program,
            &proof,
            42,
            5_000,
        )
        .is_err()
    );

    let mut wrong_b_decimals = verified.clone();
    wrong_b_decimals.position.as_mut().unwrap().token_b.decimals += 1;
    assert!(
        settlement::verify(
            &record,
            &submission,
            &intent,
            &candidate,
            &wrong_b_decimals,
            &proof,
            42,
            5_000,
        )
        .is_err()
    );

    let mut zero_classification = verified;
    zero_classification
        .collected_fees
        .push(gofin_meteora::protocol::VerifiedTokenDelta {
            mint: WSOL.into(),
            decimals: 9,
            token_program: TOKEN.into(),
            atoms: SignedQuantity::new("0").unwrap(),
        });
    assert!(
        settlement::verify(
            &record,
            &submission,
            &intent,
            &candidate,
            &zero_classification,
            &proof,
            42,
            5_000,
        )
        .is_err()
    );
}

fn empty_proof(failed: bool) -> FinalizedReceiptProof {
    FinalizedReceiptProof {
        signature: String::new(),
        slot: 0,
        fee_lamports: 0,
        failed,
        message_sha256: String::new(),
        account_keys: vec![],
        message_instructions: vec![],
        required_signatures: vec![],
        pre_balances_lamports: vec![],
        post_balances_lamports: vec![],
        pre_token_balances: vec![],
        post_token_balances: vec![],
        loaded_addresses: json!({"writable":[],"readonly":[]}),
        inner_instructions: vec![],
        log_messages: vec![],
        error: None,
    }
}

fn token_balance(index: u64, amount: &str) -> Value {
    token_balance_with_mint(index, WSOL, amount, 9)
}

fn token_balance_with_mint(index: u64, mint: &str, amount: &str, decimals: u8) -> Value {
    json!({
        "accountIndex":index,
        "mint":mint,
        "owner":WALLET,
        "programId":TOKEN,
        "uiTokenAmount":{"amount":amount,"decimals":decimals}
    })
}
