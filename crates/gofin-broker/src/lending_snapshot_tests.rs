//! Controlled observations only: debt coverage never creates spendable cash.
use super::*;
use gofin_domain::{Amount, PositionAnchor, PositionKind, PositionState, Quantity};

fn asset() -> AssetId {
    AssetId {
        network: gofin_solana::NETWORK.into(),
        reference: bs58::encode([9_u8; 32]).into_string(),
        decimals: 6,
    }
}

fn position(kind: PositionKind, market: &str, amount: u64) -> PositionState {
    let resource = ResourceId("solana:mainnet-beta:synthetic-owner".into());
    let side = if kind == PositionKind::Supply {
        "supply"
    } else {
        "debt"
    };
    let id = format!("kamino:lending:{market}:synthetic-obligation:synthetic-reserve:{side}");
    let amount = Amount {
        asset: asset(),
        atoms: Quantity::from_u64(amount),
    };
    PositionState {
        resource: resource.clone(),
        position: id.clone(),
        kind: kind.clone(),
        amount: amount.clone(),
        scaled_receipt: Some(Quantity::from_u64(7)),
        anchor: Some(PositionAnchor {
            resource,
            position: id,
            position_kind: kind,
            principal: amount,
            scaled_receipt: Some(Quantity::from_u64(7)),
            evidence_version: 1,
            evidence: json!({"synthetic_finalized_obligation":true,"market":market}),
            observed_at: now(),
        }),
    }
}

struct ImportedLendingObserver(StateSnapshot);

#[async_trait]
impl Adapter for ImportedLendingObserver {
    fn definitions(&self) -> Vec<Definition> {
        vec![]
    }
    fn connections(&self) -> Vec<gofin_domain::ConnectionMetadata> {
        vec![gofin_domain::ConnectionMetadata {
            name: "synthetic-klend".into(),
            provider: "kamino".into(),
            resource: ResourceId("solana:mainnet-beta:synthetic-owner".into()),
            role: Some("lending".into()),
            parent: None,
            network: Some(gofin_solana::NETWORK.into()),
            address: None,
            credential_ref: None,
            signing_enabled: false,
            details: json!({}),
        }]
    }
    async fn snapshot(&self) -> gofin_domain::Result<StateSnapshot> {
        Ok(self.0.clone())
    }
    async fn snapshot_scoped(&self, _: &SnapshotScope) -> gofin_domain::Result<StateSnapshot> {
        self.snapshot().await
    }
    async fn read(&self, _: &str, _: &Value) -> gofin_domain::Result<Value> {
        unreachable!("snapshot only")
    }
    async fn prepare(&self, _: &str, _: &Value) -> gofin_domain::Result<PreparedOperation> {
        unreachable!("no financial preparation")
    }
    async fn revalidate(&self, _: &PreparedOperation) -> gofin_domain::Result<PreparedOperation> {
        unreachable!("no financial preparation")
    }
    async fn submit(
        &self,
        _: &PreparedOperation,
        _: Arc<dyn SubmissionJournal>,
    ) -> gofin_domain::Result<Submission> {
        unreachable!("no financial dispatch")
    }
    async fn reconcile(&self, _: &OperationRecord) -> gofin_domain::Result<Reconciliation> {
        unreachable!("no financial submission")
    }
}

struct NoAuthorization;
#[async_trait]
impl Authorizer for NoAuthorization {
    async fn authorize(
        &self,
        _: &gofin_domain::AuthorizationChallenge,
    ) -> gofin_domain::Result<gofin_domain::ApprovalAudit> {
        unreachable!("observations never authorize financial actions")
    }
}

fn snapshot(positions: Vec<PositionState>) -> StateSnapshot {
    StateSnapshot {
        state: FinancialState {
            positions,
            ..Default::default()
        },
        observed_at: now(),
        complete: true,
        coverage: vec!["solana:mainnet-beta:synthetic-owner".into()],
        pending: vec![],
    }
}

async fn observe(snapshot: StateSnapshot) -> StateSnapshot {
    let observer = CompositeObserver::new(vec![Arc::new(ImportedLendingObserver(snapshot))]);
    let engine = Arc::new(
        Engine::new(
            Store::in_memory().unwrap(),
            vec![],
            Arc::new(NoAuthorization),
            EngineOptions::default(),
        )
        .unwrap(),
    );
    observer.attach(Arc::downgrade(&engine));
    observer
        .observe(&SnapshotScope {
            assets: vec![asset()],
            ..Default::default()
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn imported_supply_and_debt_cover_exact_asset_without_wallet_cash() {
    let positions = vec![
        position(PositionKind::Supply, "market-one", 120),
        position(PositionKind::VariableDebt, "market-one", 40),
        position(PositionKind::VariableDebt, "market-two", 15),
    ];
    let result = observe(snapshot(positions.clone())).await;
    assert!(result.complete, "{:?}", result.pending);
    assert!(
        result.state.holdings.is_empty(),
        "borrowing exposure is not wallet cash"
    );
    assert_eq!(
        result.state.positions, positions,
        "markets, supply and debt must remain separate"
    );
    assert!(result.pending.is_empty());
}

#[tokio::test]
async fn lending_coverage_requires_matching_anchor_and_full_asset_identity() {
    let valid = position(PositionKind::VariableDebt, "market", 40);
    for case in 0..11 {
        let mut changed = valid.clone();
        match case {
            0 => changed.anchor = None,
            1 => changed.anchor.as_mut().unwrap().resource = ResourceId("foreign-owner".into()),
            2 => changed.anchor.as_mut().unwrap().position = "foreign-obligation".into(),
            3 => changed.anchor.as_mut().unwrap().position_kind = PositionKind::Supply,
            4 => changed.anchor.as_mut().unwrap().principal.atoms = Quantity::from_u64(41),
            5 => changed.anchor.as_mut().unwrap().scaled_receipt = None,
            6 => changed.anchor.as_mut().unwrap().evidence_version = 0,
            7 => changed.anchor.as_mut().unwrap().evidence = json!({}),
            8 => {
                changed.amount.asset.decimals = 9;
                changed.anchor.as_mut().unwrap().principal.asset.decimals = 9;
            }
            9 => {
                changed.amount.asset.reference = bs58::encode([8_u8; 32]).into_string();
                changed.anchor.as_mut().unwrap().principal.asset = changed.amount.asset.clone();
            }
            10 => {
                changed.kind = PositionKind::Investment;
                changed.anchor.as_mut().unwrap().position_kind = PositionKind::Investment;
            }
            _ => unreachable!(),
        }
        let result = observe(snapshot(vec![changed])).await;
        assert!(!result.complete, "case {case}");
        assert!(
            result
                .pending
                .iter()
                .any(|p| p.starts_with("scope:asset-not-covered:")),
            "case {case}: {:?}",
            result.pending
        );
        assert!(result.state.holdings.is_empty());
    }
}

#[tokio::test]
async fn incomplete_lending_observation_is_not_upgraded_by_valid_debt_anchor() {
    let mut initial = snapshot(vec![position(PositionKind::VariableDebt, "market", 40)]);
    initial.complete = false;
    initial
        .pending
        .push("synthetic-obligation-price-unavailable".into());
    let result = observe(initial).await;
    assert!(!result.complete);
    assert!(
        result
            .pending
            .contains(&"synthetic-obligation-price-unavailable".into())
    );
    assert!(
        !result
            .pending
            .iter()
            .any(|p| p.starts_with("scope:asset-not-covered:"))
    );
    assert_eq!(
        result.state.positions[0].amount.atoms,
        Quantity::from_u64(40)
    );
}

#[tokio::test]
async fn lending_positions_preserve_free_underlying_and_unrelated_vault_rows() {
    let supply = position(PositionKind::Supply, "market", 120);
    let debt = position(PositionKind::VariableDebt, "market", 40);
    let mut vault = position(PositionKind::Investment, "vault", 25);
    vault.position = "kamino:lending:synthetic-vault:shares".into();
    vault.amount.asset.reference = bs58::encode([6_u8; 32]).into_string();
    let anchor = vault.anchor.as_mut().unwrap();
    anchor.position = vault.position.clone();
    anchor.principal = vault.amount.clone();
    anchor.evidence = json!({"synthetic_distinct_vault_position":true});
    let mut initial = snapshot(vec![supply, debt, vault]);
    initial.state.holdings.push(gofin_domain::ResourceAmount {
        resource: initial.state.positions[0].resource.clone(),
        amount: Amount {
            asset: asset(),
            atoms: Quantity::from_u64(60),
        },
    });
    let expected = initial.state.clone();
    let result = observe(initial).await;
    assert!(result.complete, "{:?}", result.pending);
    assert_eq!(result.state, expected);
}

fn collateral_position() -> PositionState {
    let address = |byte| bs58::encode([byte; 32]).into_string();
    let owner = address(1);
    let market = address(2);
    let obligation = address(3);
    let reserve = address(4);
    let collateral = AssetId {
        reference: address(5),
        ..asset()
    };
    let evidence = gofin_domain::LendingPositionEvidence {
        version: 1,
        owner: owner.clone(),
        market: market.clone(),
        obligation: obligation.clone(),
        reserve: reserve.clone(),
        collateral_asset: collateral.clone(),
        collateral_token_program: gofin_solana::token::TOKEN_PROGRAM_ID.into(),
        collateral_atoms: Quantity::from_u64(100),
        underlying: gofin_domain::LiquidityCompositionAmount {
            asset: asset(),
            token_program: gofin_solana::token::TOKEN_PROGRAM_ID.into(),
            atoms: Some(Quantity::from_u64(120)),
        },
        observed_slot: 42,
        observed_at: now(),
        historical_principal_known: false,
        backing_valuation: Some(gofin_domain::VaultBackingValuation {
            total_usd_micros: Quantity::from_u64(120),
            observed_at: now(),
            source: "synthetic-underlying-price".into(),
            complete: true,
            basis: "verified_underlying_exposure".into(),
            excluded_components: vec![],
        }),
        pre_transaction_collateral_atoms: None,
        finalized_submission: None,
        collateral_delta_atoms: None,
    };
    let mut position = position(PositionKind::Supply, &market, 100);
    position.resource = ResourceId(format!("solana:mainnet-beta:{owner}"));
    position.position = format!("kamino:lending:{market}:{obligation}:{reserve}:supply");
    position.amount.asset = collateral;
    position.scaled_receipt = None;
    position.anchor = Some(PositionAnchor {
        resource: position.resource.clone(),
        position: position.position.clone(),
        position_kind: PositionKind::Supply,
        principal: position.amount.clone(),
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"lending_position":evidence}),
        observed_at: now(),
    });
    position
}

#[tokio::test]
async fn obligation_collateral_coverage_keeps_exact_receipts_and_underlying_separate() {
    let position = collateral_position();
    let collateral = position.amount.asset.clone();
    assert!(verified_lending_position_asset(&position, &collateral));
    assert!(verified_lending_position_asset(&position, &asset()));
    let result = observe(snapshot(vec![position.clone()])).await;
    assert!(result.complete, "{:?}", result.pending);
    assert!(result.state.holdings.is_empty());
    assert_eq!(result.state.positions, vec![position]);
    assert_eq!(
        result.state.positions[0].amount.atoms,
        Quantity::from_u64(100)
    );
    assert_eq!(result.state.positions[0].amount.asset, collateral);
    assert_ne!(collateral, asset());
}

#[tokio::test]
async fn collateral_coverage_rejects_foreign_native_identity_or_unknown_underlying() {
    let valid = collateral_position();
    for (field, changed) in [
        ("version", json!(2)),
        ("market", json!(bs58::encode([20_u8; 32]).into_string())),
        ("obligation", json!(bs58::encode([21_u8; 32]).into_string())),
        ("reserve", json!(bs58::encode([22_u8; 32]).into_string())),
        ("owner", json!(bs58::encode([23_u8; 32]).into_string())),
        ("collateral_token_program", json!("unreviewed-program")),
        ("collateral_atoms", json!("101")),
        ("collateral_asset", json!(asset())),
    ] {
        let mut corrupt = valid.clone();
        corrupt.anchor.as_mut().unwrap().evidence["lending_position"][field] = changed;
        let result = observe(snapshot(vec![corrupt])).await;
        assert!(!result.complete, "field {field}");
        assert!(result.state.holdings.is_empty());
    }
    let mut missing = valid;
    missing.anchor.as_mut().unwrap().evidence["lending_position"]["underlying"]["atoms"] =
        json!(null);
    assert!(verified_lending_position_asset(
        &missing,
        &missing.amount.asset
    ));
    assert!(!verified_lending_position_asset(&missing, &asset()));
    let result = observe(snapshot(vec![missing])).await;
    assert!(!result.complete, "unknown underlying is not observed zero");
}

#[test]
fn duplicate_collateral_observations_count_backing_once_and_conflicts_stay_incomplete() {
    let collateral = collateral_position();
    let mut observed = snapshot(vec![collateral.clone(), collateral.clone()]);
    observed.state.holdings.push(gofin_domain::ResourceAmount {
        resource: collateral.resource.clone(),
        amount: collateral.amount.clone(),
    });
    // Even same-mint free receipt tokens are separate from obligation custody.
    normalize_composed_snapshot(&mut observed);
    assert!(observed.complete);
    assert_eq!(observed.state.positions.len(), 1);
    assert_eq!(observed.state.holdings.len(), 1);

    let mut different = collateral.clone();
    different.amount.atoms = Quantity::from_u64(101);
    let anchor = different.anchor.as_mut().unwrap();
    anchor.principal = different.amount.clone();
    anchor.evidence["lending_position"]["collateral_atoms"] = json!("101");
    let mut conflicting = snapshot(vec![collateral.clone(), different]);
    normalize_composed_snapshot(&mut conflicting);
    assert!(!conflicting.complete);
    assert_eq!(conflicting.state.positions.len(), 1);
    assert!(
        conflicting
            .pending
            .iter()
            .any(|row| row.starts_with("snapshot:conflicting-lending-collateral:"))
    );

    let mut invalid = collateral;
    invalid.anchor.as_mut().unwrap().evidence_version = 0;
    let mut incomplete = snapshot(vec![invalid]);
    normalize_composed_snapshot(&mut incomplete);
    assert!(
        !incomplete.complete,
        "full snapshots also reject invalid evidence"
    );
}
