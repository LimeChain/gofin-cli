//! Focused Perps asset and sponsored-sender lifecycle tests.
//!
//! These tests are a child of the Perps fixture module so they can exercise
//! the same in-memory account/RPC transport without exposing fixture keys or
//! controls through the public CLI.

use super::*;
use base64::engine::general_purpose::STANDARD;
use bincode::Options;
use ed25519_dalek::{Signer, SigningKey};
use solana_message::VersionedMessage;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use std::sync::{Mutex, OnceLock};

static SPONSORED_TPSL_UPDATE: OnceLock<Mutex<bool>> = OnceLock::new();

/// Arm one synthetic update candidate whose fee payer is the reviewed keeper.
/// The flag is consumed while constructing the candidate, before GoFin freezes
/// the message; tests must never rewrite a prepared transaction.
#[cfg(test)]
pub(super) fn arm_sponsored_tpsl_update() {
    *SPONSORED_TPSL_UPDATE
        .get_or_init(|| Mutex::new(false))
        .lock()
        .expect("sponsored TPSL fixture mode") = true;
}

/// Select a sponsor before the fixture builds the signed message.
pub(super) fn next_fee_payer(action: &str, wallet: &str) -> Result<Option<Pubkey>> {
    if action != "tpsl-update" {
        return Ok(None);
    }
    let armed = SPONSORED_TPSL_UPDATE
        .get_or_init(|| Mutex::new(false))
        .lock()
        .map_err(|_| error("Sponsored Perps fixture mode is unavailable."))?
        .then_some(());
    if armed.is_none() {
        return Ok(None);
    }
    *SPONSORED_TPSL_UPDATE
        .get_or_init(|| Mutex::new(false))
        .lock()
        .map_err(|_| error("Sponsored Perps fixture mode is unavailable."))? = false;
    let payer = Pubkey::new_from_array(
        SigningKey::from_bytes(&KEEPER_SEED)
            .verifying_key()
            .to_bytes(),
    );
    if payer.to_string() == wallet {
        return Err(error("Synthetic sponsor must differ from the wallet."));
    }
    Ok(Some(payer))
}

/// Sign only the fixture's pinned keeper roles by public-key identity. The
/// wallet signature is intentionally left untouched for GoFin's protected
/// signer, regardless of whether key-ordering starts with the sponsor.
pub(super) fn sign_fixture_cosigners(encoded: &str, sign_fee_payer: bool) -> Result<String> {
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| error("Synthetic Perps transaction is malformed."))?;
    let mut transaction: VersionedTransaction = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .deserialize(&bytes)
        .map_err(|_| error("Synthetic Perps transaction is malformed."))?;
    let VersionedMessage::Legacy(message) = &transaction.message else {
        return Err(error(
            "Synthetic Perps transaction must use a legacy message.",
        ));
    };
    let message_bytes = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .serialize(message)
        .map_err(|_| error("Synthetic Perps message serialization failed."))?;
    let fee_payer = message
        .account_keys
        .first()
        .ok_or_else(|| error("Synthetic Perps message omitted its fee payer."))?;
    let signers = [KEEPER_SEED, [92; 32]];
    for seed in signers {
        let signer = SigningKey::from_bytes(&seed);
        let key = Pubkey::new_from_array(signer.verifying_key().to_bytes());
        if &key == fee_payer && !sign_fee_payer {
            continue;
        }
        let index = message
            .account_keys
            .iter()
            .take(usize::from(message.header.num_required_signatures))
            .position(|candidate| candidate == &key)
            .ok_or_else(|| error("Synthetic Perps message omitted a pinned keeper signer."))?;
        *transaction
            .signatures
            .get_mut(index)
            .ok_or_else(|| error("Synthetic Perps signature slot is missing."))? =
            Signature::from(signer.sign(&message_bytes).to_bytes());
    }
    let signed = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .serialize(&transaction)
        .map_err(|_| error("Synthetic Perps transaction serialization failed."))?;
    Ok(STANDARD.encode(signed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gofin_domain::{ApprovalAudit, Authorizer, OperationRequest, Status};
    use std::sync::Arc;

    async fn engine_fixture(
        name: &str,
    ) -> (
        Arc<gofin_engine::Engine>,
        Arc<FixtureRpc>,
        tempfile::TempDir,
    ) {
        *state().lock().expect("reset process-local Perps fixture") = State::default();
        let (config, applications, fixture_rpc, _) =
            crate::solana_rehearsal::applications_with_fixture().expect("synthetic applications");
        let store_dir = tempfile::tempdir().expect("temporary engine store");
        let store_path = store_dir.path().join(format!("{name}.sqlite"));
        let runtime = gofin_broker::compose_runtime(
            config,
            gofin_broker::RuntimeDependencies {
                store: gofin_store::Store::open(&store_path).expect("open engine store"),
                adapters: applications.adapters.clone(),
                authorizer: Arc::new(TestAuthorizer),
                valuator: applications.valuator.clone(),
                bridge: applications.bridge.clone(),
            },
        )
        .expect("compose synthetic engine");
        (runtime.engine.clone(), fixture_rpc, store_dir)
    }

    struct TestAuthorizer;

    #[async_trait::async_trait]
    impl Authorizer for TestAuthorizer {
        async fn authorize(
            &self,
            challenge: &gofin_domain::AuthorizationChallenge,
        ) -> Result<ApprovalAudit> {
            Ok(ApprovalAudit {
                mechanism: "synthetic-sponsored-perps-test".into(),
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: gofin_domain::now(),
                expires_at: challenge.expires_at.clone(),
            })
        }
    }

    #[tokio::test]
    async fn sponsored_tpsl_update_lost_response_reconciles_exact_message_after_restart() {
        let _guard = crate::solana_rehearsal::FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state().lock().expect("reset sponsored Perps fixture") = State::default();
        let (config, applications, fixture_rpc, _) =
            crate::solana_rehearsal::applications_with_fixture()
                .expect("synthetic broker applications");
        let store_dir = tempfile::tempdir().expect("temporary durable store");
        let store_path = store_dir.path().join("sponsored-perps.sqlite");
        let runtime = gofin_broker::compose_runtime(
            config.clone(),
            gofin_broker::RuntimeDependencies {
                store: gofin_store::Store::open(&store_path).expect("open durable store"),
                adapters: applications.adapters.clone(),
                authorizer: Arc::new(TestAuthorizer),
                valuator: applications.valuator.clone(),
                bridge: applications.bridge.clone(),
            },
        )
        .expect("compose synthetic engine");
        let engine = runtime.engine.clone();
        let opened = engine
            .execute(OperationRequest {
                request_id: "sponsored-tpsl-open".into(),
                operation: "jupiter.perps.increase_market".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "market":"SOL","side":"long","funding_token":"USDC",
                    "collateral_amount_atoms":"11000000","size_usd_atoms":"12100000",
                    "maximum_protocol_fee_atoms":"5000000","maximum_protocol_fee_usd_atoms":"500000",
                    "maximum_slippage_bps":50,"maximum_network_fee_lamports":"15000",
                    "maximum_account_funding_lamports":"6000000"
                }),
                run_id: None,
            })
            .await
            .expect("open synthetic position");
        assert_eq!(opened.status, Status::Completed, "{opened:?}");
        let position = opened.preview.as_ref().unwrap().binding["position"]
            .as_str()
            .expect("position address")
            .to_owned();
        let created = engine
            .execute(OperationRequest {
                request_id: "sponsored-tpsl-create".into(),
                operation: "jupiter.perps.tpsl.create".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "position":position,"request_type":"sl","trigger_price_usd_atoms":"90000000",
                    "receive_token":"USDC","entire_position":true,
                    "maximum_protocol_fee_usd_atoms":"500000","maximum_network_fee_lamports":"15000",
                    "maximum_account_funding_lamports":"6000000"
                }),
                run_id: None,
            })
            .await
            .expect("create resting synthetic stop loss");
        assert_eq!(created.status, Status::Pending, "{created:?}");
        let request = created.preview.as_ref().unwrap().binding["request"]
            .as_str()
            .expect("request address")
            .to_owned();

        arm_sponsored_tpsl_update();
        state()
            .lock()
            .expect("arm lost synthetic update response")
            .hide_next_tpsl_update_response = true;
        let update = engine
            .execute(OperationRequest {
                request_id: "sponsored-tpsl-update".into(),
                operation: "jupiter.perps.tpsl.update".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "request":request,"original_operation_id":created.id,
                    "trigger_price_usd_atoms":"91000000","maximum_network_fee_lamports":"15000"
                }),
                run_id: None,
            })
            .await
            .expect("uncertain update remains durable");
        assert!(
            matches!(
                update.status,
                Status::Unknown | Status::Submitted | Status::Pending
            ),
            "a hidden execute response is never reported completed: {update:?}"
        );
        let submitted = update
            .submission
            .as_ref()
            .expect("the exact update submission is durable")
            .clone();
        assert!(!submitted.reference.is_empty());
        let payer = Pubkey::new_from_array(
            SigningKey::from_bytes(&KEEPER_SEED)
                .verifying_key()
                .to_bytes(),
        )
        .to_string();
        assert!(
            update.preview.as_ref().unwrap().binding["approved_external_cosigners"]
                .as_array()
                .is_some_and(|signers| signers.iter().any(|signer| signer == &payer))
        );
        let signature = state()
            .lock()
            .expect("read provider's accepted sponsored signature")
            .hidden_transaction_signatures
            .iter()
            .next()
            .cloned()
            .expect("provider accepted the sponsored message before losing its response");
        let before_restart = fixture_rpc
            .0
            .lock()
            .expect("fixture ledger before restart")
            .transactions
            .len();
        assert!(
            state()
                .lock()
                .expect("provider response loss is recorded")
                .hidden_transaction_signatures
                .contains(&signature)
        );
        assert!(
            state()
                .lock()
                .expect("wallet signature history is persisted")
                .history
                .get(&fixture_rpc.0.lock().expect("wallet identity").wallet)
                .is_some_and(|history| history.contains(&signature)),
            "the sponsored transaction is indexed under the wallet for recovery"
        );
        let still_hidden = engine
            .status(&update.id, true)
            .await
            .expect("unavailable finalized RPC proof remains unresolved");
        assert_eq!(still_hidden.status, Status::Pending, "{still_hidden:?}");

        drop(engine);
        drop(runtime);
        *state()
            .lock()
            .expect("clear only process-local fixture cache") = State::default();
        // Simulate RPC visibility recovering after restart. The finalized
        // ledger transaction and exact message are unchanged; only the
        // synthetic read fault is cleared.
        {
            let mut ledger = fixture_rpc.0.lock().expect("fixture ledger visibility");
            let mut persisted = ledger
                .perps_state
                .clone()
                .and_then(|value| serde_json::from_value::<State>(value).ok())
                .expect("persisted Perps proof state");
            persisted.hidden_transaction_signatures.remove(&signature);
            ledger.perps_state =
                Some(serde_json::to_value(persisted).expect("serialize recovered RPC visibility"));
        }
        let restarted = gofin_broker::compose_runtime(
            config,
            gofin_broker::RuntimeDependencies {
                store: gofin_store::Store::open(&store_path).expect("reopen durable store"),
                adapters: applications.adapters.clone(),
                authorizer: Arc::new(TestAuthorizer),
                valuator: applications.valuator.clone(),
                bridge: applications.bridge.clone(),
            },
        )
        .expect("recompose synthetic engine after restart");
        let recovered = restarted
            .engine
            .status(&update.id, true)
            .await
            .expect("reconcile the original sponsored submission");
        assert_eq!(recovered.status, Status::Completed, "{recovered:?}");
        assert_eq!(
            recovered.submission.as_ref().map(|saved| &saved.reference),
            Some(&submitted.reference),
            "restart keeps the exact persisted message identity"
        );
        assert!(
            recovered
                .effects
                .as_ref()
                .is_some_and(|verified| verified.effects.iter().all(|effect| {
                    !matches!(effect, gofin_domain::FinancialEffect::Cost { category, .. }
                    if category == "solana_network_fee")
                })),
            "a keeper-paid update does not charge the wallet for its network fee"
        );
        assert_eq!(
            fixture_rpc
                .0
                .lock()
                .expect("fixture ledger after restart reconciliation")
                .transactions
                .len(),
            before_restart,
            "recovery inspects the exact finalized transaction without rebroadcast"
        );
        let operations = restarted.engine.operations().expect("durable operations");
        let recovered_update = operations
            .iter()
            .find(|record| record.id == update.id)
            .expect("updated operation remains durable");
        assert_eq!(recovered_update.submission.as_ref(), Some(&submitted));
    }

    #[tokio::test]
    async fn eth_funded_position_closes_to_btc_through_the_engine() {
        let _guard = crate::solana_rehearsal::FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (engine, _fixture, _store_dir) = engine_fixture("eth-to-btc").await;
        let opened = engine
            .execute(OperationRequest {
                request_id: "asset-eth-open".into(),
                operation: "jupiter.perps.increase_market".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "market":"ETH","side":"long","funding_token":"ETH",
                    "collateral_amount_atoms":"5500000","size_usd_atoms":"12100000",
                    "maximum_protocol_fee_atoms":"10000000","maximum_protocol_fee_usd_atoms":"500000",
                    "maximum_slippage_bps":50,"maximum_network_fee_lamports":"15000",
                    "maximum_account_funding_lamports":"6000000"
                }),
                run_id: None,
            })
            .await
            .expect("ETH-funded long opens");
        assert_eq!(opened.status, Status::Completed, "{opened:?}");
        let position = opened.preview.as_ref().unwrap().binding["position"]
            .as_str()
            .unwrap()
            .to_owned();
        let closed = engine
            .execute(OperationRequest {
                request_id: "asset-eth-close-to-btc".into(),
                operation: "jupiter.perps.decrease_market".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "position":position,"receive_token":"BTC","size_usd_atoms":"12100000",
                    "minimum_receive_atoms":"21900","maximum_protocol_fee_usd_atoms":"500000",
                    "maximum_slippage_bps":50,"maximum_network_fee_lamports":"15000",
                    "maximum_account_funding_lamports":"6000000"
                }),
                run_id: None,
            })
            .await
            .expect("ETH position closes to BTC");
        assert_eq!(closed.status, Status::Completed, "{closed:?}");
        assert!(closed.effects.as_ref().is_some_and(|verified| verified.effects.iter().any(|effect|
            matches!(effect, gofin_domain::FinancialEffect::UsableCredit { amount, .. }
                if amount.asset.reference == BTC_MINT && amount.atoms == gofin_domain::Quantity::from_u64(21_996)
            )
        )), "the exact BTC receive is a settled wallet credit: {closed:?}");
    }

    #[tokio::test]
    async fn btc_market_opens_and_closes_to_btc_through_the_engine() {
        let _guard = crate::solana_rehearsal::FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (engine, _fixture, _store_dir) = engine_fixture("btc-market-roundtrip").await;
        let opened = engine
            .execute(OperationRequest {
                request_id: "asset-btc-open".into(),
                operation: "jupiter.perps.increase_market".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "market":"BTC","side":"long","funding_token":"USDC",
                    "collateral_amount_atoms":"11000000","size_usd_atoms":"12100000",
                    "maximum_protocol_fee_atoms":"5000000","maximum_protocol_fee_usd_atoms":"500000",
                    "maximum_slippage_bps":50,"maximum_network_fee_lamports":"15000",
                    "maximum_account_funding_lamports":"6000000"
                }),
                run_id: None,
            })
            .await
            .expect("BTC-market long opens from USDC");
        assert_eq!(opened.status, Status::Completed, "{opened:?}");
        let position = opened.preview.as_ref().unwrap().binding["position"]
            .as_str()
            .unwrap()
            .to_owned();
        let closed = engine
            .execute(OperationRequest {
                request_id: "asset-btc-close".into(),
                operation: "jupiter.perps.decrease_market".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "position":position,"receive_token":"BTC","size_usd_atoms":"12100000",
                    "minimum_receive_atoms":"21996","maximum_protocol_fee_usd_atoms":"500000",
                    "maximum_slippage_bps":50,"maximum_network_fee_lamports":"15000",
                    "maximum_account_funding_lamports":"6000000"
                }),
                run_id: None,
            })
            .await
            .expect("BTC-market position closes to BTC");
        assert_eq!(closed.status, Status::Completed, "{closed:?}");
        assert!(closed.effects.as_ref().is_some_and(|verified| verified.effects.iter().any(|effect|
            matches!(effect, gofin_domain::FinancialEffect::UsableCredit { amount, .. }
                if amount.asset.reference == BTC_MINT && amount.atoms == gofin_domain::Quantity::from_u64(21_996)
            )
        )), "the exact BTC receive settles through its wallet credit: {closed:?}");
    }

    #[tokio::test]
    async fn eth_funding_converts_to_sol_collateral_and_btc_payout() {
        let _guard = crate::solana_rehearsal::FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (engine, _fixture, _store_dir) = engine_fixture("eth-to-sol-collateral").await;
        let opened = engine
            .execute(OperationRequest {
                request_id: "asset-eth-to-sol-open".into(),
                operation: "jupiter.perps.increase_market".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "market":"SOL","side":"long","funding_token":"ETH",
                    "collateral_amount_atoms":"5500000","size_usd_atoms":"12100000",
                    "maximum_protocol_fee_atoms":"10000000","maximum_protocol_fee_usd_atoms":"500000",
                    "maximum_slippage_bps":50,"maximum_network_fee_lamports":"15000",
                    "maximum_account_funding_lamports":"6000000"
                }),
                run_id: None,
            })
            .await
            .expect("ETH funding converts into SOL collateral");
        assert_eq!(opened.status, Status::Completed, "{opened:?}");
        let position = opened.preview.as_ref().unwrap().binding["position"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            opened.preview.as_ref().unwrap().binding["collateral_mint"],
            SOL_MINT
        );
        assert_eq!(
            opened.preview.as_ref().unwrap().binding["funding_mint"],
            ETH_MINT
        );
        let closed = engine
            .execute(OperationRequest {
                request_id: "asset-eth-to-sol-close-to-btc".into(),
                operation: "jupiter.perps.decrease_market".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "position":position,"receive_token":"BTC","size_usd_atoms":"12100000",
                    "minimum_receive_atoms":"21996","maximum_protocol_fee_usd_atoms":"500000",
                    "maximum_slippage_bps":50,"maximum_network_fee_lamports":"15000",
                    "maximum_account_funding_lamports":"6000000"
                }),
                run_id: None,
            })
            .await
            .expect("SOL-collateral position closes to BTC");
        assert_eq!(closed.status, Status::Completed, "{closed:?}");
        assert!(closed.effects.as_ref().is_some_and(|verified| verified.effects.iter().any(|effect|
            matches!(effect, gofin_domain::FinancialEffect::UsableCredit { amount, .. }
                if amount.asset.reference == BTC_MINT && amount.atoms == gofin_domain::Quantity::from_u64(21_996)
            )
        )), "the exact BTC payout is a settled receive-token credit: {closed:?}");
    }

    #[tokio::test]
    async fn legacy_usdt_short_add_remove_and_close_settle_through_the_engine() {
        let _guard = crate::solana_rehearsal::FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (engine, fixture_rpc, _store_dir) = engine_fixture("legacy-usdt-short").await;
        let wallet = { fixture_rpc.0.lock().expect("fixture wallet").wallet.clone() };
        let position = super::seed_existing_position(
            &fixture_rpc,
            &wallet,
            "SOL",
            "USDT",
            2,
            12_100_000,
            10_999_000,
            100_000_000,
        )
        .expect("seed legacy USDT collateral short");
        let add = engine.execute(OperationRequest {
            request_id: "legacy-usdt-add".into(),
            operation: "jupiter.perps.collateral.add".into(),
            input: json!({"network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                "position":position,"market":"SOL","side":"short","funding_token":"USDC",
                "collateral_amount_atoms":"1000000","maximum_protocol_fee_atoms":"5000000",
                "maximum_protocol_fee_usd_atoms":"500000","maximum_slippage_bps":50,
                "maximum_network_fee_lamports":"15000","maximum_account_funding_lamports":"3000000"}),
            run_id: None,
        }).await.expect("add USDT collateral");
        assert_eq!(add.status, Status::Completed, "{add:?}");
        let remove = engine.execute(OperationRequest {
            request_id: "legacy-usdt-remove".into(),
            operation: "jupiter.perps.collateral.remove".into(),
            input: json!({"network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                "position":position,"receive_token":"USDC","collateral_usd_atoms":"1000000",
                "minimum_receive_atoms":"994005","maximum_protocol_fee_usd_atoms":"500000",
                "maximum_slippage_bps":50,"maximum_network_fee_lamports":"15000",
                "maximum_account_funding_lamports":"3000000"}),
            run_id: None,
        }).await.expect("remove USDT collateral");
        assert_eq!(remove.status, Status::Completed, "{remove:?}");
        let close = engine.execute(OperationRequest {
            request_id: "legacy-usdt-close".into(),
            operation: "jupiter.perps.decrease_market".into(),
            input: json!({"network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                "position":position,"receive_token":"USDC","size_usd_atoms":"12100000",
                "minimum_receive_atoms":"10943010","maximum_protocol_fee_usd_atoms":"500000",
                "maximum_slippage_bps":50,"maximum_network_fee_lamports":"15000",
                "maximum_account_funding_lamports":"6000000"}),
            run_id: None,
        }).await.expect("close legacy USDT collateral short");
        assert_eq!(close.status, Status::Completed, "{close:?}");
    }

    #[tokio::test]
    async fn paired_tpsl_update_cancel_refund_and_close_survive_restart() {
        let _guard = crate::solana_rehearsal::FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state().lock().expect("reset paired-cancel fixture") = State::default();
        let (config, applications, _fixture_rpc, _) =
            crate::solana_rehearsal::applications_with_fixture().expect("fixture applications");
        let store_dir = tempfile::tempdir().expect("paired-cancel store");
        let store_path = store_dir.path().join("paired-cancel.sqlite");
        let make_runtime = || {
            gofin_broker::compose_runtime(
                config.clone(),
                gofin_broker::RuntimeDependencies {
                    store: gofin_store::Store::open(&store_path).expect("paired engine store"),
                    adapters: applications.adapters.clone(),
                    authorizer: Arc::new(TestAuthorizer),
                    valuator: applications.valuator.clone(),
                    bridge: applications.bridge.clone(),
                },
            )
            .expect("compose paired-cancel engine")
        };
        let runtime = make_runtime();
        let engine = runtime.engine.clone();
        let opened = engine
            .execute(OperationRequest {
                request_id: "paired-cancel-open".into(),
                operation: "jupiter.perps.increase_market".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "market":"SOL","side":"long","funding_token":"USDC",
                    "collateral_amount_atoms":"11000000","size_usd_atoms":"12100000",
                    "maximum_protocol_fee_atoms":"5000000","maximum_protocol_fee_usd_atoms":"500000",
                    "maximum_slippage_bps":50,"maximum_network_fee_lamports":"15000",
                    "maximum_account_funding_lamports":"6000000"
                }),
                run_id: None,
            })
            .await
            .expect("open position for paired protections");
        assert_eq!(opened.status, Status::Completed, "{opened:?}");
        let position = opened.preview.as_ref().unwrap().binding["position"]
            .as_str()
            .unwrap()
            .to_owned();
        let first = engine
            .execute(OperationRequest {
                request_id: "paired-cancel-tp".into(),
                operation: "jupiter.perps.tpsl.create".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "position":position,"request_type":"tp","trigger_price_usd_atoms":"110000000",
                    "receive_token":"USDC","entire_position":false,"size_usd_atoms":"6050000",
                    "maximum_protocol_fee_usd_atoms":"500000","maximum_network_fee_lamports":"15000",
                    "maximum_account_funding_lamports":"6000000"
                }),
                run_id: None,
            })
            .await
            .expect("create TP");
        assert_eq!(first.status, Status::Pending, "{first:?}");
        let second = engine
            .execute(OperationRequest {
                request_id: "paired-cancel-sl".into(),
                operation: "jupiter.perps.tpsl.create".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "position":position,"request_type":"sl","trigger_price_usd_atoms":"90000000",
                    "receive_token":"USDC","entire_position":true,
                    "maximum_protocol_fee_usd_atoms":"500000","maximum_network_fee_lamports":"15000",
                    "maximum_account_funding_lamports":"6000000",
                    "protection_parent_operation_id":first.id
                }),
                run_id: None,
            })
            .await
            .expect("create opposite SL sibling");
        assert_eq!(second.status, Status::Pending, "{second:?}");

        let first_request = first.preview.as_ref().unwrap().binding["request"]
            .as_str()
            .unwrap()
            .to_owned();
        let updated = engine
            .execute(OperationRequest {
                request_id: "paired-cancel-tp-update".into(),
                operation: "jupiter.perps.tpsl.update".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "request":first_request,"original_operation_id":first.id,
                    "trigger_price_usd_atoms":"111000000","maximum_network_fee_lamports":"15000"
                }),
                run_id: None,
            })
            .await
            .expect("amend the first protection");
        assert_eq!(updated.status, Status::Completed, "{updated:?}");

        let second_request = second.preview.as_ref().unwrap().binding["request"]
            .as_str()
            .unwrap()
            .to_owned();
        let updated_second = engine
            .execute(OperationRequest {
                request_id: "paired-cancel-sl-update".into(),
                operation: "jupiter.perps.tpsl.update".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "request":second_request,"original_operation_id":second.id,
                    "trigger_price_usd_atoms":"89000000","maximum_network_fee_lamports":"15000"
                }),
                run_id: None,
            })
            .await
            .expect("amend the opposite protection member");
        assert_eq!(
            updated_second.status,
            Status::Completed,
            "{updated_second:?}"
        );

        drop(engine);
        drop(runtime);
        *state().lock().expect("drop process state before restart") = State::default();
        let restarted = make_runtime();
        let restarted_engine = restarted.engine.clone();
        let first_request = first.preview.as_ref().unwrap().binding["request"]
            .as_str()
            .unwrap()
            .to_owned();
        let cancel_first = restarted_engine
            .execute(OperationRequest {
                request_id: "paired-cancel-tp-cancel".into(),
                operation: "jupiter.perps.tpsl.cancel".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "request":first_request,"original_operation_id":first.id,
                    "maximum_network_fee_lamports":"15000"
                }),
                run_id: None,
            })
            .await
            .expect("cancel the amended TP after restart");
        assert_eq!(cancel_first.status, Status::Completed, "{cancel_first:?}");
        let first_parent = restarted_engine
            .status(&first.id, true)
            .await
            .expect("settle first parent cancellation");
        assert_eq!(first_parent.status, Status::Completed, "{first_parent:?}");

        let second_request = second.preview.as_ref().unwrap().binding["request"]
            .as_str()
            .unwrap()
            .to_owned();
        let cancel_second = restarted_engine
            .execute(OperationRequest {
                request_id: "paired-cancel-sl-cancel".into(),
                operation: "jupiter.perps.tpsl.cancel".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "request":second_request,"original_operation_id":second.id,
                    "maximum_network_fee_lamports":"15000"
                }),
                run_id: None,
            })
            .await
            .expect("cancel the remaining SL sibling");
        assert_eq!(cancel_second.status, Status::Completed, "{cancel_second:?}");
        let second_parent = restarted_engine
            .status(&second.id, true)
            .await
            .expect("settle second parent cancellation");
        assert_eq!(second_parent.status, Status::Completed, "{second_parent:?}");

        let close = restarted_engine
            .execute(OperationRequest {
                request_id: "paired-cancel-close".into(),
                operation: "jupiter.perps.decrease_market".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "position":position,"receive_token":"USDC","size_usd_atoms":"12100000",
                    "minimum_receive_atoms":"10943010","maximum_protocol_fee_usd_atoms":"500000",
                    "maximum_slippage_bps":50,"maximum_network_fee_lamports":"15000",
                    "maximum_account_funding_lamports":"6000000"
                }),
                run_id: None,
            })
            .await
            .expect("close position after both protections are cancelled");
        assert_eq!(close.status, Status::Completed, "{close:?}");
        assert!(state().lock().unwrap().tpsl_requests.is_empty());
    }
}
