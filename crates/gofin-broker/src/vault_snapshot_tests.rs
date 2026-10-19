use super::*;
use gofin_domain::{
    Amount, FinancialState, PositionAnchor, PositionKind, PositionState, Quantity, ResourceAmount,
    VaultPositionEvidence,
};

fn snapshot(family: &str, wallet_balance: u64) -> StateSnapshot {
    let resource = ResourceId("solana:mainnet-beta:fixture".into());
    let mint = bs58::encode([7_u8; 32]).into_string();
    let vault = bs58::encode([8_u8; 32]).into_string();
    let share = AssetId {
        network: gofin_solana::NETWORK.into(),
        reference: mint,
        decimals: 6,
    };
    let underlying = AssetId {
        network: gofin_solana::NETWORK.into(),
        reference: bs58::encode([9_u8; 32]).into_string(),
        decimals: 6,
    };
    let kind = if family == "lending" {
        PositionKind::Investment
    } else {
        PositionKind::Liquidity
    };
    let position = format!("kamino:{family}:{vault}:shares");
    let evidence = VaultPositionEvidence {
        version: 1,
        family: family.into(),
        vault,
        share_asset: share.clone(),
        share_token_program: gofin_solana::token::TOKEN_PROGRAM_ID.into(),
        wallet_shares_atoms: Quantity::from_u64(40),
        staked_shares_atoms: Quantity::from_u64(50),
        pending_shares_atoms: Quantity::from_u64(10),
        underlying: vec![],
        historical_principal_known: false,
        observed_slot: Some(100),
        observed_at: now(),
        backing_valuation: None,
        anchor_position_id: Some(position.clone()),
        pre_transaction_shares_atoms: None,
        finalized_submission: None,
        shares_delta_atoms: None,
    };
    let amount = Amount {
        asset: share.clone(),
        atoms: Quantity::from_u64(100),
    };
    StateSnapshot {
        state: FinancialState {
            holdings: vec![
                ResourceAmount {
                    resource: resource.clone(),
                    amount: Amount {
                        asset: share,
                        atoms: Quantity::from_u64(wallet_balance),
                    },
                },
                ResourceAmount {
                    resource: resource.clone(),
                    amount: Amount {
                        asset: underlying,
                        atoms: Quantity::from_u64(500),
                    },
                },
            ],
            positions: vec![PositionState {
                resource: resource.clone(),
                position: position.clone(),
                kind: kind.clone(),
                amount: amount.clone(),
                scaled_receipt: None,
                anchor: Some(PositionAnchor {
                    resource,
                    position,
                    position_kind: kind,
                    principal: amount,
                    scaled_receipt: None,
                    evidence_version: 1,
                    evidence: json!({"source":"kamino_position_observation","vault_position":evidence}),
                    observed_at: now(),
                }),
            }],
            ..Default::default()
        },
        observed_at: now(),
        complete: true,
        coverage: vec![],
        pending: vec![],
    }
}

#[test]
fn wallet_shares_are_deduplicated_but_farm_custody_and_free_underlying_are_preserved() {
    for family in ["lending", "liquidity"] {
        let mut snapshot = snapshot(family, 40);
        normalize_composed_snapshot(&mut snapshot);
        assert!(snapshot.complete);
        assert_eq!(snapshot.state.holdings.len(), 1);
        assert_eq!(
            snapshot.state.holdings[0].amount.atoms,
            Quantity::from_u64(500)
        );
        assert_eq!(
            snapshot.state.positions[0].amount.atoms,
            Quantity::from_u64(100)
        );
        let receipt = verified_vault_receipt(&snapshot.state.positions[0]).unwrap();
        assert_eq!(receipt.staked_shares_atoms, Quantity::from_u64(50));
        assert!(!receipt.historical_principal_known);
    }
}

#[test]
fn conflicting_share_observations_are_incomplete_and_invalid_identity_is_not_deduplicated() {
    let mut mismatch = snapshot("lending", 41);
    normalize_composed_snapshot(&mut mismatch);
    assert!(!mismatch.complete);
    assert_eq!(mismatch.state.holdings.len(), 2);
    assert!(
        mismatch
            .pending
            .iter()
            .any(|v| v.contains("conflicting-vault-wallet-shares"))
    );
    for (field, value) in [
        ("share_token_program", json!("unexpected-program")),
        ("version", json!(2)),
        ("wallet_shares_atoms", json!("41")),
        ("anchor_position_id", json!("another-position")),
        ("vault", json!(bs58::encode([10_u8; 32]).into_string())),
    ] {
        let mut wrong = snapshot("liquidity", 40);
        wrong.state.positions[0].anchor.as_mut().unwrap().evidence["vault_position"][field] = value;
        assert!(verified_vault_receipt(&wrong.state.positions[0]).is_none());
        normalize_composed_snapshot(&mut wrong);
        assert_eq!(wrong.state.holdings.len(), 2);
        assert!(!wrong.complete);
    }
}

#[test]
fn repeated_wallet_aliases_never_double_count_vault_backing() {
    for family in ["lending", "liquidity"] {
        let mut repeated = snapshot(family, 40);
        repeated
            .state
            .positions
            .push(repeated.state.positions[0].clone());
        normalize_composed_snapshot(&mut repeated);
        assert!(repeated.complete);
        assert_eq!(repeated.state.positions.len(), 1);
        assert_eq!(repeated.state.holdings.len(), 1);

        let mut conflicting = snapshot(family, 40);
        let mut later = conflicting.state.positions[0].clone();
        later.amount.atoms = Quantity::from_u64(101);
        let anchor = later.anchor.as_mut().unwrap();
        anchor.principal.atoms = Quantity::from_u64(101);
        anchor.evidence["vault_position"]["wallet_shares_atoms"] = json!("41");
        assert!(verified_vault_receipt(&later).is_some());
        conflicting.state.positions.push(later);
        normalize_composed_snapshot(&mut conflicting);
        assert!(!conflicting.complete);
        assert_eq!(conflicting.state.positions.len(), 1);
        assert_eq!(
            conflicting.state.positions[0].amount.atoms,
            Quantity::from_u64(100)
        );
        assert!(
            conflicting
                .pending
                .iter()
                .any(|v| v.contains("conflicting-vault-position"))
        );
    }
}
