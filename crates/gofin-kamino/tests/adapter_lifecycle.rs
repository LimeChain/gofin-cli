//! Synthetic protocol + controlled Solana transport tests for the real adapter lifecycle.
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use gofin_domain::*;
use gofin_kamino::{KaminoAdapter, protocol::*};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use gofin_solana::{
    FinalizedReceiptProof, NormalizedInstruction, SolanaRuntime, SolanaSigner, SolanaWalletAdapter,
    rpc::SolanaRpc, transaction::NormalizedAccountMeta,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
const VAULT: &str = "Stake11111111111111111111111111111111111111";
const ASSET: &str = "So11111111111111111111111111111111111111112";
const SHARE: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const OTHER: &str = "11111111111111111111111111111111";
const TOKEN: &str = gofin_solana::token::TOKEN_PROGRAM_ID;
struct Prices;
#[async_trait]
impl UsdPriceSource for Prices {
    async fn quote(&self, _: &AssetId) -> Result<UsdQuote> {
        Ok(UsdQuote {
            usd_per_unit: "1.25".into(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            observed_at: chrono::Utc::now().to_rfc3339(),
            source: "synthetic".into(),
        })
    }
}
#[derive(Default)]
struct Journal {
    persisted: Mutex<Option<Submission>>,
}
impl SubmissionJournal for Journal {
    fn assert_authorized(&self) -> Result<()> {
        Ok(())
    }
    fn persist_submission(&self, s: &Submission) -> Result<()> {
        *self.persisted.lock().unwrap() = Some(s.clone());
        Ok(())
    }
    fn persist_dispatch_evidence(&self, s: &Submission, _: &Value) -> Result<()> {
        assert_eq!(self.persisted.lock().unwrap().as_ref(), Some(s));
        Ok(())
    }
}
struct Rpc {
    journal: Arc<Journal>,
    calls: Mutex<Vec<String>>,
    fee: AtomicU64,
    uncertain: AtomicBool,
    rent_exists: AtomicBool,
    latest_hash_byte: AtomicU64,
    height: AtomicU64,
    wire_hashes: Mutex<Vec<(String, String)>>,
}
#[async_trait]
impl SolanaRpc for Rpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.calls.lock().unwrap().push(method.into());
        if matches!(method, "simulateTransaction" | "sendTransaction") {
            // This fixture emits legacy messages with fewer than 128 accounts.
            let wire = STANDARD.decode(params[0].as_str().unwrap()).unwrap();
            assert_eq!(wire[0], 1);
            let message = &wire[65..];
            assert_eq!(message[0] & 0x80, 0);
            assert!(message[3] < 128);
            let hash_start = 4 + usize::from(message[3]) * 32;
            self.wire_hashes.lock().unwrap().push((
                method.into(),
                bs58::encode(&message[hash_start..hash_start + 32]).into_string(),
            ));
        }
        match method {
            "getAccountInfo" => {
                assert_eq!(params[1]["commitment"], "finalized");
                Ok(
                    json!({"context":{"slot":1},"value":if self.rent_exists.load(Ordering::SeqCst) {json!({"lamports":1000})}else{Value::Null}}),
                )
            }
            "getSlot" => Ok(json!(1)),
            "getBlockHeight" => Ok(json!(self.height.load(Ordering::SeqCst))),
            "getLatestBlockhash" => {
                assert_eq!(params[0]["commitment"], "confirmed");
                Ok(
                    json!({"context":{"slot":1},"value":{"blockhash":bs58::encode([self.latest_hash_byte.load(Ordering::SeqCst) as u8;32]).into_string(),"lastValidBlockHeight":200}}),
                )
            }
            "getFeeForMessage" => Ok(json!({"value":self.fee.load(Ordering::SeqCst)})),
            "simulateTransaction" => Ok(json!({"value":{"err":null,"unitsConsumed":50000}})),
            "sendTransaction" => {
                let persisted = self
                    .journal
                    .persisted
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("immutable identity must be durable before network send");
                let bytes = STANDARD.decode(params[0].as_str().unwrap()).unwrap();
                assert_eq!(bytes[0], 1);
                let signature = bs58::encode(&bytes[1..65]).into_string();
                assert_eq!(persisted.data["wallet_signature"], signature);
                assert!(persisted.reference.starts_with("solana:message:"));
                if self.uncertain.load(Ordering::SeqCst) {
                    Err(GoFinError::new(
                        "SYNTHETIC_TIMEOUT",
                        "submit",
                        "The controlled transport lost its response.",
                        "Reconcile the durable identity.",
                    ))
                } else {
                    Ok(json!(signature))
                }
            }
            "getSignatureStatuses" => Ok(json!({"value":[null]})),
            _ => Err(GoFinError::validation(format!(
                "Unexpected synthetic RPC {method}"
            ))),
        }
    }
}
struct Protocol {
    family: VaultFamily,
    shares: Arc<AtomicU64>,
    malicious: Arc<AtomicBool>,
    inventory_forbidden: bool,
}
impl Protocol {
    fn farm_info(&self) -> gofin_kamino::farms::FarmInfo {
        gofin_kamino::farms::FarmInfo {
            address: "Vote111111111111111111111111111111111111111".into(),
            token: TokenInfo {
                mint: SHARE.into(),
                decimals: 6,
                token_program: TOKEN.into(),
            },
            vault: OTHER.into(),
            authority: OTHER.into(),
            scope_prices: OTHER.into(),
            frozen: false,
            delegated: false,
            time_unit: 1,
            warmup: 3,
            cooldown: 5,
            locking_mode: 2,
            locking_start: 100,
            locking_duration: 60,
            early_penalty_bps: 25,
            deposit_cap: 0,
            total_staked: 100,
            total_active_scaled: "100000000000000000000".into(),
            total_pending_scaled: "0".into(),
            total_pending_amount: 0,
            withdraw_authority: OTHER.into(),
            reward_tokens: vec![],
        }
    }
    fn vault_info(&self) -> VaultInfo {
        let token = |mint: &str| TokenInfo {
            mint: mint.into(),
            decimals: 6,
            token_program: TOKEN.into(),
        };
        VaultInfo {
            family: self.family,
            address: VAULT.into(),
            program: OTHER.into(),
            name: Some("Synthetic vault".into()),
            backend: None,
            assets: if self.family == VaultFamily::Lending {
                vec![token(ASSET)]
            } else {
                vec![token(ASSET), token(OTHER)]
            },
            shares: token(SHARE),
            farm: Some("Vote111111111111111111111111111111111111111".into()),
            deposits_available: true,
            withdrawals_available: true,
            restrictions: vec![],
            observed_slot: Some(1),
            observed_at: chrono::Utc::now().to_rfc3339(),
            metrics: json!({}),
            protocol_state: json!({}),
        }
    }
    fn position_info(&self, owner: &str) -> PositionInfo {
        let vault = self.vault_info();
        let n = self.shares.load(Ordering::SeqCst).to_string();
        PositionInfo {
            underlying: vault
                .assets
                .iter()
                .map(|t| TokenAmount {
                    token: t.clone(),
                    atoms: Some("100".into()),
                })
                .collect(),
            vault,
            owner: owner.into(),
            wallet_shares_atoms: n.clone(),
            staked_shares_atoms: "0".into(),
            pending_shares_atoms: "0".into(),
            rewards: vec![],
            withdrawable_shares_atoms: Some(n),
            complete: true,
            warnings: vec![],
            protocol_state: if self.family == VaultFamily::Liquidity {
                json!({"valuation":{"basis":"underlying_principal_only","complete":false,"excluded":["unclaimed_fees","rewards"]}})
            } else {
                json!({})
            },
        }
    }
}
#[async_trait]
impl KaminoProtocol for Protocol {
    fn family(&self) -> VaultFamily {
        self.family
    }
    async fn vaults(&self, _: &SolanaRuntime, _: &VaultQuery) -> Result<VaultPage> {
        assert!(
            !self.inventory_forbidden,
            "An exact vault filter must not scan discovery."
        );
        if self.family == VaultFamily::Liquidity && self.malicious.load(Ordering::SeqCst) {
            return Err(GoFinError::validation(
                "Controlled liquidity discovery unavailable.",
            ));
        }
        Ok(VaultPage {
            vaults: vec![self.vault_info()],
            complete: true,
            next_cursor: None,
            warnings: vec![],
        })
    }
    async fn vault(&self, _: &SolanaRuntime, _: &str) -> Result<VaultInfo> {
        Ok(self.vault_info())
    }
    async fn staking_farm(
        &self,
        _: &SolanaRuntime,
        _: &VaultInfo,
    ) -> Result<gofin_kamino::farms::FarmInfo> {
        Ok(self.farm_info())
    }
    async fn positions(&self, _: &SolanaRuntime, owner: &str) -> Result<PositionInventory> {
        assert!(
            !self.inventory_forbidden,
            "An exact vault filter must not scan all positions."
        );
        if self.family == VaultFamily::Liquidity && self.malicious.load(Ordering::SeqCst) {
            return Err(GoFinError::validation(
                "Controlled liquidity positions unavailable.",
            ));
        }
        Ok(PositionInventory {
            positions: vec![self.position_info(owner)],
            complete: true,
            observed_at: chrono::Utc::now().to_rfc3339(),
            warnings: vec![],
        })
    }
    async fn position(&self, _: &SolanaRuntime, owner: &str, _: &str) -> Result<PositionInfo> {
        Ok(self.position_info(owner))
    }
    async fn build_candidate(
        &self,
        _: &SolanaRuntime,
        intent: &VaultIntent,
        owner: &str,
    ) -> Result<VaultCandidate> {
        let mut debits = if intent.action == VaultAction::Deposit {
            intent.amounts.clone()
        } else if self.family == VaultFamily::Lending {
            vec![TokenAmount {
                token: self.vault_info().shares,
                atoms: intent.shares_atoms.clone(),
            }]
        } else {
            vec![]
        };
        if self.malicious.load(Ordering::SeqCst) && !debits.is_empty() {
            debits[0].atoms = Some("1000000".into());
        }
        Ok(VaultCandidate {
            vault: self.vault_info(),
            quote: VaultQuote {
                debits,
                credits: if intent.action == VaultAction::Withdraw {
                    self.vault_info()
                        .assets
                        .into_iter()
                        .map(|token| TokenAmount {
                            token,
                            atoms: Some("100".into()),
                        })
                        .collect()
                } else if self.family == VaultFamily::Lending {
                    vec![TokenAmount {
                        token: self.vault_info().shares,
                        atoms: Some("10".into()),
                    }]
                } else {
                    vec![]
                },
                shares_atoms: intent.shares_atoms.clone().unwrap_or_else(|| "10".into()),
                minimum_shares_enforced: true,
                protocol_fees: vec![],
                rent_lamports: if intent.maximum_account_funding_lamports > 0 {
                    1000
                } else {
                    0
                },
                network_fee_lamports: 5000,
                expires_at: (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339(),
                warnings: vec![],
                details: json!({}),
            },
            instructions: vec![NormalizedInstruction {
                program_id: OTHER.into(),
                accounts: vec![NormalizedAccountMeta {
                    address: owner.into(),
                    signer: true,
                    writable: true,
                }],
                data_base64: STANDARD.encode([1u8]),
            }],
            lookup_tables: vec![],
            blockhash: bs58::encode([42u8; 32]).into_string(),
            last_valid_block_height: 100,
            metadata: json!({"position_before":self.position_info(owner),"position_shares_before_atoms":self.shares.load(Ordering::SeqCst).to_string(),"refundable_rent_accounts":if intent.maximum_account_funding_lamports > 0 {vec![json!({"account":gofin_solana::associated_token_address(owner,SHARE,TOKEN).unwrap(),"maximum_lamports":1000})]}else{vec![]},"stake_atoms":if intent.stake {Some("10")}else{None},"farm":if intent.stake && self.family==VaultFamily::Liquidity {Some(self.farm_info())}else{None},"farms":if intent.stake && self.family==VaultFamily::Lending {vec![self.farm_info()]}else{vec![]}}),
        })
    }
    async fn validate_candidate(
        &self,
        _: &SolanaRuntime,
        _: &VaultIntent,
        _: &str,
        _: &VaultCandidate,
    ) -> Result<()> {
        Ok(())
    }
    async fn verify_finalized(
        &self,
        _: &SolanaRuntime,
        _: &VaultIntent,
        _: &str,
        _: &VaultCandidate,
        _: &FinalizedReceiptProof,
    ) -> Result<VerifiedVaultEffects> {
        Err(GoFinError::validation(
            "Not used in unresolved transport tests.",
        ))
    }
}
fn fixture() -> (
    KaminoAdapter,
    Arc<Rpc>,
    Arc<Journal>,
    Arc<AtomicU64>,
    Arc<AtomicBool>,
) {
    fixture_with_inventory_guard(false)
}
fn fixture_with_inventory_guard(
    inventory_forbidden: bool,
) -> (
    KaminoAdapter,
    Arc<Rpc>,
    Arc<Journal>,
    Arc<AtomicU64>,
    Arc<AtomicBool>,
) {
    let signer=Arc::new(SolanaSigner::parse("99eUso3aSbE9tqGSTXzo3TLfKb9RkMTURrHKQ1K7Zh3StnzFNUx8FKCPPPPpR479qsw5zv2WNBKmgiz7WqgAJfM").unwrap());
    let journal = Arc::new(Journal::default());
    let rpc = Arc::new(Rpc {
        journal: journal.clone(),
        calls: Mutex::new(vec![]),
        fee: AtomicU64::new(5000),
        uncertain: AtomicBool::new(false),
        rent_exists: AtomicBool::new(false),
        latest_hash_byte: AtomicU64::new(43),
        height: AtomicU64::new(1),
        wire_hashes: Mutex::new(vec![]),
    });
    let wallet = Arc::new(
        SolanaWalletAdapter::new(
            "wallet",
            signer.address(),
            Some(signer.clone()),
            rpc.clone(),
            Arc::new(Prices),
        )
        .unwrap(),
    );
    let shares = Arc::new(AtomicU64::new(20));
    let malicious = Arc::new(AtomicBool::new(false));
    let protocols: Vec<Arc<dyn KaminoProtocol>> = [VaultFamily::Lending, VaultFamily::Liquidity]
        .into_iter()
        .map(|family| {
            Arc::new(Protocol {
                family,
                shares: shares.clone(),
                malicious: malicious.clone(),
                inventory_forbidden,
            }) as Arc<dyn KaminoProtocol>
        })
        .collect();
    let adapter = KaminoAdapter::new(
        BTreeMap::from([
            ("wallet".into(), wallet.clone()),
            ("wallet-alias".into(), wallet),
        ]),
        protocols,
        Arc::new(Prices),
    )
    .unwrap();
    (adapter, rpc, journal, shares, malicious)
}
fn input() -> Value {
    json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","family":"lending","vault":VAULT,"share_mint":SHARE,"amounts":[{"mint":ASSET,"atoms":"100"}],"maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0})
}
fn bounds() -> Value {
    json!({"maximum_amounts":[{"mint":ASSET,"atoms":"100"}],"maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0})
}
#[tokio::test]
async fn exact_vault_filters_use_direct_reads_for_both_families() {
    let (adapter, _, _, _, _) = fixture_with_inventory_guard(true);
    for family in ["lending", "liquidity"] {
        let query = json!({"wallet":"wallet","family":family,"vault":VAULT});
        let discovered = adapter.read("kamino.vaults", &query).await.unwrap();
        let page = &discovered["families"][0]["page"];
        assert_eq!(page["vaults"].as_array().unwrap().len(), 1);
        assert_eq!(page["vaults"][0]["address"], VAULT);
        assert_eq!(page["complete"], true);
        assert!(page["next_cursor"].is_null());
        let positions = adapter.read("kamino.positions", &query).await.unwrap();
        let inventory = &positions["families"][0]["inventory"];
        assert_eq!(inventory["positions"].as_array().unwrap().len(), 1);
        assert_eq!(inventory["positions"][0]["vault"]["address"], VAULT);
        assert_eq!(inventory["complete"], true);
        for wrong_address in [OTHER, "malformed-address"] {
            let mut wrong = query.clone();
            wrong["vault"] = json!(wrong_address);
            for operation in ["kamino.vaults", "kamino.positions"] {
                assert!(adapter.read(operation, &wrong).await.is_err());
            }
        }
        let mut filtered = query.clone();
        filtered["search"] = json!("SYNTHETIC");
        filtered["token_mint"] = json!(ASSET);
        assert_eq!(adapter.read("kamino.vaults", &filtered).await.unwrap()["families"][0]["page"]["vaults"].as_array().unwrap().len(), 1);
        filtered["token_mint"] = json!(SHARE);
        assert!(adapter.read("kamino.vaults", &filtered).await.unwrap()["families"][0]["page"]["vaults"].as_array().unwrap().is_empty());
        filtered["token_mint"] = json!(ASSET);
        filtered["search"] = json!("unrelated-name");
        assert!(adapter.read("kamino.vaults", &filtered).await.unwrap()["families"][0]["page"]["vaults"].as_array().unwrap().is_empty());
        let mut paged = query;
        paged["cursor"] = json!("next");
        assert!(adapter.read("kamino.vaults", &paged).await.is_err());
    }
    for operation in ["kamino.vaults", "kamino.positions"] {
        let observed = adapter
            .read(operation, &json!({"wallet":"wallet","vault":VAULT}))
            .await
            .unwrap();
        assert_eq!(observed["families"].as_array().unwrap().len(), 2);
    }
}
#[tokio::test]
async fn exact_baselines_bind_verified_position_and_rent_absence_without_inventory() {
    let (adapter, rpc, journal, _, _) = fixture_with_inventory_guard(true);
    let mut request = input();
    request["maximum_account_funding_lamports"] = json!(1000);
    let prepared = adapter.prepare("kamino.deposit", &request).await.unwrap();
    let anchors: Vec<PositionAnchor> =
        serde_json::from_value(prepared.binding[BASELINE_ANCHORS_KEY].clone()).unwrap();
    assert_eq!(anchors.len(), 1);
    assert_eq!(anchors[0].principal.atoms.to_string(), "20");
    assert_eq!(anchors[0].evidence["vault_position"]["observed_slot"], 1);
    assert_eq!(
        anchors[0].observed_at,
        prepared.binding["protocol_candidate"]["metadata"]["position_before"]["vault"]["observed_at"]
    );
    let rent_observation =
        &prepared.binding["protocol_candidate"]["metadata"]["account_funding_observations"][0];
    assert_eq!(rent_observation["account_absence_proven"], true);
    assert_eq!(rent_observation["commitment"], "finalized");
    adapter.revalidate(&prepared).await.unwrap();
    for pointer in [
        "/protocol_candidate/metadata/position_before/complete",
        "/protocol_candidate/metadata/account_funding_observations/0/account_absence_proven",
    ] {
        let mut altered = prepared.clone();
        *altered.binding.pointer_mut(pointer).unwrap() = json!(false);
        assert!(adapter.revalidate(&altered).await.is_err());
    }
    for (pointer, value) in [
        (
            "/protocol_candidate/metadata/position_before/vault/observed_at",
            json!("2000-01-01T00:00:00Z"),
        ),
        (
            "/protocol_candidate/metadata/position_before/wallet_shares_atoms",
            json!("21"),
        ),
        (
            "/protocol_candidate/metadata/account_funding_observations/0/account",
            json!(OTHER),
        ),
        (
            "/protocol_candidate/metadata/account_funding_observations/0/observed_at",
            json!("2000-01-01T00:00:00Z"),
        ),
    ] {
        let mut altered = prepared.clone();
        *altered.binding.pointer_mut(pointer).unwrap() = value;
        assert!(adapter.revalidate(&altered).await.is_err());
    }
    let mut altered = prepared.clone();
    altered.binding[BASELINE_ANCHORS_KEY][0]["principal"]["atoms"] = json!("999");
    assert!(adapter.revalidate(&altered).await.is_err());
    let mut missing = prepared.clone();
    missing
        .binding
        .as_object_mut()
        .unwrap()
        .remove(BASELINE_ANCHORS_KEY);
    assert!(adapter.revalidate(&missing).await.is_err());
    rpc.rent_exists.store(true, Ordering::SeqCst);
    assert!(adapter.prepare("kamino.deposit", &request).await.is_err());
    assert!(journal.persisted.lock().unwrap().is_none());
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == "sendTransaction")
    );
}
#[tokio::test]
async fn wallet_aliases_do_not_duplicate_current_vault_positions() {
    let (adapter, _, _, _, _) = fixture();
    let snapshot = adapter.snapshot().await.unwrap();
    assert!(snapshot.complete);
    assert_eq!(snapshot.coverage.len(), 1);
    assert_eq!(snapshot.state.positions.len(), 2);
    assert_ne!(
        snapshot.state.positions[0].position,
        snapshot.state.positions[1].position
    );
    let read = adapter
        .read("kamino.positions", &json!({"wallet":"wallet"}))
        .await
        .unwrap();
    let liquidity = &read["families"][1]["inventory"]["positions"][0];
    assert_eq!(liquidity["complete"], true);
    assert_eq!(liquidity["current_value"]["complete"], false);
    assert_eq!(
        liquidity["current_value"]["basis"],
        "underlying_principal_only"
    );
    assert_eq!(
        liquidity["current_value"]["excluded_components"],
        json!(["unclaimed_fees", "rewards"])
    );
    assert!(
        !liquidity["current_value"]["missing_data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
#[tokio::test]
async fn discovery_and_native_quote_expose_assets_without_swap_or_mcp() {
    let (a, _, _, _, _) = fixture();
    let definitions = a.definitions();
    assert_eq!(definitions.len(), 7);
    assert!(
        definitions.iter().all(|d| !d.name.contains("mcp")
            && !d.name.contains("plan")
            && !d.name.contains("swap"))
    );
    let vaults = a.read("kamino.vaults", &json!({})).await.unwrap();
    assert_eq!(vaults["families"].as_array().unwrap().len(), 2);
    let mut i = input();
    i["action"] = json!("deposit");
    let quote = a.read("kamino.quote", &i).await.unwrap();
    assert_eq!(
        quote["resolved_intent"]["amounts"][0]["token"]["mint"],
        ASSET
    );
    assert_eq!(quote["quote"]["shares_atoms"], "10");
}
#[tokio::test]
async fn native_receipt_shapes_accept_exact_shares_and_reject_unapproved_assets() {
    let (adapter, _, _, _, _) = fixture();
    for family in ["lending", "liquidity"] {
        let mut deposit = input();
        deposit["family"] = json!(family);
        if family == "liquidity" {
            deposit["amounts"]
                .as_array_mut()
                .unwrap()
                .push(json!({"mint":OTHER,"atoms":"50"}));
        }
        let prepared = adapter.prepare("kamino.deposit", &deposit).await.unwrap();
        adapter.revalidate(&prepared).await.unwrap();
        let mut underlying_receipt = prepared.clone();
        underlying_receipt.binding["protocol_candidate"]["quote"]["credits"] =
            json!([{"token":{"mint":ASSET,"decimals":6,"token_program":TOKEN},"atoms":"1"}]);
        assert_eq!(
            adapter.revalidate(&underlying_receipt).await.is_ok(),
            family == "liquidity"
        );
        if family == "lending" {
            assert_eq!(
                prepared.binding["protocol_candidate"]["quote"]["credits"][0]["token"]["mint"],
                SHARE
            );
        }
        let receipt =
            json!({"token":{"mint":SHARE,"decimals":6,"token_program":TOKEN},"atoms":"10"});
        let mut share_receipt = prepared.clone();
        share_receipt.binding["protocol_candidate"]["quote"]["credits"] = json!([receipt.clone()]);
        adapter.revalidate(&share_receipt).await.unwrap();
        for (field, value) in [
            ("mint", json!(VAULT)),
            ("token_program", json!(OTHER)),
            ("decimals", json!(9)),
        ] {
            let mut changed = share_receipt.clone();
            changed.binding["protocol_candidate"]["quote"]["credits"][0]["token"][field] = value;
            assert!(adapter.revalidate(&changed).await.is_err());
        }
        let mut changed = share_receipt.clone();
        changed.binding["protocol_candidate"]["quote"]["credits"][0]["atoms"] = json!("11");
        assert!(adapter.revalidate(&changed).await.is_err());
        changed = share_receipt;
        changed.binding["protocol_candidate"]["quote"]["credits"] =
            json!([receipt.clone(), receipt]);
        assert!(adapter.revalidate(&changed).await.is_err());

        let mut withdrawal = deposit;
        withdrawal.as_object_mut().unwrap().remove("amounts");
        withdrawal["shares_atoms"] = json!("4");
        let prepared = adapter
            .prepare("kamino.withdraw", &withdrawal)
            .await
            .unwrap();
        adapter.revalidate(&prepared).await.unwrap();
        let permission = adapter.prepare_plan_permission("kamino.withdraw", &withdrawal, &json!({"maximum_shares_atoms":"4","maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0})).await.unwrap();
        adapter
            .validate_boundary_candidate(&permission.spec, &prepared)
            .unwrap();
        if family == "lending" {
            assert_eq!(
                prepared.binding["protocol_candidate"]["quote"]["debits"][0]["token"]["mint"],
                SHARE
            );
        }
        let mut changed = prepared.clone();
        changed.binding["protocol_candidate"]["quote"]["debits"] =
            json!([{"token":{"mint":ASSET,"decimals":6,"token_program":TOKEN},"atoms":"4"}]);
        assert!(adapter.revalidate(&changed).await.is_err());
        changed.binding["protocol_candidate"]["quote"]["debits"][0]["token"]["mint"] = json!(SHARE);
        adapter.revalidate(&changed).await.unwrap();
        changed.binding["protocol_candidate"]["quote"]["debits"][0]["atoms"] = json!("5");
        assert!(adapter.revalidate(&changed).await.is_err());
        changed = prepared;
        changed.binding["protocol_candidate"]["quote"]["credits"][0]["token"]["mint"] =
            json!(SHARE);
        assert!(adapter.revalidate(&changed).await.is_err());
    }
}
#[tokio::test]
async fn approval_then_durable_submission_and_uncertain_response_retains_identity() {
    let (a, rpc, journal, _, _) = fixture();
    let p = a.prepare("kamino.deposit", &input()).await.unwrap();
    assert!(p.valuation.is_some());
    assert_eq!(p.input, input());
    let before = rpc.calls.lock().unwrap().len();
    a.revalidate(&p).await.unwrap();
    assert_eq!(
        before,
        rpc.calls.lock().unwrap().len(),
        "post approval revalidation is local"
    );
    rpc.uncertain.store(true, Ordering::SeqCst);
    assert!(a.submit(&p, journal.clone()).await.is_err());
    assert!(journal.persisted.lock().unwrap().is_some());
    assert_eq!(
        rpc.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.as_str() == "sendTransaction")
            .count(),
        1
    );
}
#[tokio::test]
async fn final_preapproval_hash_follows_account_reads_and_remains_immutable_at_submission() {
    let (adapter, rpc, journal, _, _) = fixture();
    let mut request = input();
    request["maximum_account_funding_lamports"] = json!(1000);
    let prepared = adapter.prepare("kamino.deposit", &request).await.unwrap();
    let approved_hash = bs58::encode([43u8; 32]).into_string();
    assert_eq!(
        prepared.binding["protocol_candidate"]["blockhash"],
        approved_hash
    );
    assert_eq!(
        prepared.binding["protocol_candidate"]["last_valid_block_height"],
        200
    );
    assert_eq!(prepared.binding["last_valid_block_height"], 200);
    assert_eq!(
        prepared.expires_at,
        prepared.binding["protocol_candidate"]["quote"]["expires_at"]
    );
    let calls = rpc.calls.lock().unwrap().clone();
    let hash_index = calls
        .iter()
        .position(|m| m == "getLatestBlockhash")
        .unwrap();
    assert!(calls.iter().rposition(|m| m == "getAccountInfo").unwrap() < hash_index);
    assert!(
        calls
            .iter()
            .position(|m| m == "simulateTransaction")
            .unwrap()
            > hash_index
    );
    // A newer hash exists by approval time. Submission must keep the one whose
    // message and validity height were reviewed and durably authorized.
    rpc.latest_hash_byte.store(44, Ordering::SeqCst);
    adapter.submit(&prepared, journal.clone()).await.unwrap();
    let submission = journal.persisted.lock().unwrap().clone().unwrap();
    assert_eq!(submission.data["recent_blockhash"], approved_hash);
    assert_eq!(submission.data["last_valid_block_height"], 200);
    assert_eq!(
        submission.data["message_sha256"],
        prepared.binding["message_sha256"]
    );
    assert_eq!(
        rpc.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|m| *m == "getLatestBlockhash")
            .count(),
        1
    );
    let hashes = rpc.wire_hashes.lock().unwrap();
    assert!(hashes.iter().any(|(method, _)| method == "sendTransaction"));
    assert!(hashes.iter().all(|(_, hash)| hash == &approved_hash));
}
#[tokio::test]
async fn expired_approved_hash_fails_without_refresh_signing_or_send() {
    let (adapter, rpc, journal, _, _) = fixture();
    let prepared = adapter.prepare("kamino.deposit", &input()).await.unwrap();
    rpc.height.store(201, Ordering::SeqCst);
    rpc.latest_hash_byte.store(44, Ordering::SeqCst);
    let error = adapter
        .submit(&prepared, journal.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, "SOLANA_BLOCKHASH_EXPIRED");
    assert!(journal.persisted.lock().unwrap().is_none());
    let calls = rpc.calls.lock().unwrap();
    assert_eq!(
        calls.iter().filter(|m| *m == "getLatestBlockhash").count(),
        1
    );
    assert!(!calls.iter().any(|m| m == "sendTransaction"));
}
#[tokio::test]
async fn withdrawal_permission_recognizes_finite_source_without_repricing_receipts() {
    let (adapter, _, _, _, _) = fixture();
    for family in ["lending", "liquidity"] {
        let mut i = input();
        i.as_object_mut().unwrap().remove("amounts");
        i["family"] = json!(family);
        i["shares_atoms"] = json!("20");
        let b = json!({"maximum_shares_atoms":"20","maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0});
        let permission = adapter
            .prepare_plan_permission("kamino.withdraw", &i, &b)
            .await
            .unwrap();
        // The old persisted permission shape is enough; no new consent fields.
        assert!(permission.deferred_receipt.is_none());
        let serialized = serde_json::to_value(&permission).unwrap();
        let permission: gofin_domain::financial_plans::PlanPermission =
            serde_json::from_value(serialized.clone()).unwrap();
        for credit in &permission.possible_credits {
            let target = ResourceAmount {
                resource: credit.resource.clone(),
                amount: Amount {
                    asset: credit.asset.clone(),
                    atoms: Quantity::new("300").unwrap(),
                },
            };
            assert!(adapter.plan_has_finite_non_cash_source(
                "kamino.withdraw",
                &permission,
                &target
            ));
            assert!(!adapter.plan_has_finite_non_cash_source(
                "kamino.deposit",
                &permission,
                &target
            ));
            for cap in [
                Value::Null,
                json!("0"),
                json!("all"),
                json!("-1"),
                json!("18446744073709551616"),
            ] {
                let mut invalid = permission.clone();
                invalid.spec.parameters["maximum_shares_atoms"] = cap;
                assert!(!adapter.plan_has_finite_non_cash_source(
                    "kamino.withdraw",
                    &invalid,
                    &target
                ));
            }
            let mut wrong = target.clone();
            wrong.resource = ResourceId("other-wallet".into());
            assert!(!adapter.plan_has_finite_non_cash_source(
                "kamino.withdraw",
                &permission,
                &wrong
            ));
            wrong = target;
            wrong.amount.asset.reference = "other-mint".into();
            assert!(!adapter.plan_has_finite_non_cash_source(
                "kamino.withdraw",
                &permission,
                &wrong
            ));
            let mut invalid = permission.clone();
            invalid.spec.fixed["action"] = json!("deposit");
            assert!(!adapter.plan_has_finite_non_cash_source("kamino.withdraw", &invalid, &wrong));
        }
        assert_eq!(serialized, serde_json::to_value(&permission).unwrap());
    }
}

#[tokio::test]
async fn all_shares_freeze_once_and_named_plan_requires_numeric_cap() {
    let (a, _, journal, shares, _) = fixture();
    let mut i = input();
    i.as_object_mut().unwrap().remove("amounts");
    i["shares_atoms"] = json!("all");
    let p = a.prepare("kamino.withdraw", &i).await.unwrap();
    assert_eq!(p.input["shares_atoms"], "all");
    assert_eq!(p.binding["intent"]["shares_atoms"], "20");
    shares.store(40, Ordering::SeqCst);
    let s = a.submit(&p, journal).await.unwrap();
    assert_eq!(s.kind, "solana.transaction");
    assert_eq!(p.binding["intent"]["shares_atoms"], "20");
    let b = json!({"maximum_shares_atoms":"20","maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0});
    assert!(
        a.prepare_plan_permission("kamino.withdraw", &i, &b)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn balance_independent_permission_accepts_smaller_explicit_native_step_and_rejects_identity_change()
 {
    let (a, rpc, _, _, _) = fixture();
    let permission = a
        .prepare_plan_permission("kamino.deposit", &input(), &bounds())
        .await
        .unwrap();
    assert!(
        rpc.calls.lock().unwrap().is_empty(),
        "permission should not simulate or check funding"
    );
    let mut i = input();
    i["amounts"][0]["atoms"] = json!("75");
    let p = a.prepare("kamino.deposit", &i).await.unwrap();
    a.validate_boundary_candidate(&permission.spec, &p).unwrap();
    let mut changed = p.clone();
    changed.binding["intent"]["vault"] = json!(OTHER);
    assert!(
        a.validate_boundary_candidate(&permission.spec, &changed)
            .is_err()
    );
    let mut high = p;
    high.input["amounts"][0]["atoms"] = json!("101");
    high.binding["intent"]["amounts"][0]["atoms"] = json!("101");
    assert!(
        a.validate_boundary_candidate(&permission.spec, &high)
            .is_err()
    );
}
#[tokio::test]
async fn staking_authority_requires_and_enforces_a_finite_share_custody_cap() {
    let (adapter, _, _, _, _) = fixture();
    for family in ["lending", "liquidity"] {
        let mut intent = input();
        intent["family"] = json!(family);
        intent["stake"] = json!(true);
        let mut bounded = bounds();
        if family == "liquidity" {
            intent["amounts"]
                .as_array_mut()
                .unwrap()
                .push(json!({"mint":OTHER,"atoms":"50"}));
            bounded["maximum_amounts"]
                .as_array_mut()
                .unwrap()
                .push(json!({"mint":OTHER,"atoms":"50"}));
        }
        let prepared = adapter.prepare("kamino.deposit", &intent).await.unwrap();
        // Exact standalone approval discloses and freezes this quantity without
        // requiring a separate custody-cap field in the native input schema.
        assert!(prepared.summary.contains("Stake 10 share atoms"));
        for term in [
            "warmup 3 slots",
            "cooldown 5 slots",
            "locking mode 2 (fixed start and expiry)",
            "start 100 slots",
            "duration 60 slots",
            "early withdrawal penalty 25 basis points",
        ] {
            assert!(prepared.summary.contains(term), "{}", prepared.summary);
        }
        assert!(
            adapter
                .prepare_boundaries(&prepared, &bounded)
                .await
                .is_err()
        );
        assert!(
            adapter
                .prepare_plan_permission("kamino.deposit", &intent, &bounded)
                .await
                .is_err()
        );
        for invalid in [
            json!("0"),
            json!("01"),
            json!("18446744073709551616"),
            json!(10),
        ] {
            bounded["maximum_stake_shares_atoms"] = invalid;
            assert!(
                adapter
                    .prepare_plan_permission("kamino.deposit", &intent, &bounded)
                    .await
                    .is_err()
            );
        }
        bounded["maximum_stake_shares_atoms"] = json!("10");
        let permission = adapter
            .prepare_plan_permission("kamino.deposit", &intent, &bounded)
            .await
            .unwrap();
        let exact_bounds = adapter
            .prepare_boundaries(&prepared, &bounded)
            .await
            .unwrap();
        assert!(permission.spec.summary.contains("cooldown 5 slots"));
        assert!(
            exact_bounds
                .summary
                .contains("early withdrawal penalty 25 basis points")
        );
        adapter
            .validate_boundary_candidate(&exact_bounds, &prepared)
            .unwrap();
        adapter
            .validate_boundary_candidate(&permission.spec, &prepared)
            .unwrap();
        let farm_path = if family == "lending" {
            "/protocol_candidate/metadata/farms/0"
        } else {
            "/protocol_candidate/metadata/farm"
        };
        let mut changed_terms = prepared.clone();
        changed_terms.binding.pointer_mut(farm_path).unwrap()["cooldown"] = json!(6);
        assert!(
            adapter
                .validate_boundary_candidate(&permission.spec, &changed_terms)
                .is_err()
        );
        let mut dynamic_pool = prepared.clone();
        dynamic_pool.binding.pointer_mut(farm_path).unwrap()["total_staked"] = json!(200);
        dynamic_pool.binding.pointer_mut(farm_path).unwrap()["deposit_cap"] = json!(1000);
        adapter
            .validate_boundary_candidate(&permission.spec, &dynamic_pool)
            .unwrap();
        let mut changed_fee = prepared.clone();
        if family == "lending" {
            changed_fee.binding["protocol_candidate"]["vault"]["metrics"]["fees"] =
                json!({"withdrawal_bps":50});
        } else {
            changed_fee.binding["protocol_candidate"]["vault"]["protocol_state"]["strategy"]["reward0Fee"] =
                json!(50);
        }
        assert!(
            adapter
                .validate_boundary_candidate(&permission.spec, &changed_fee)
                .is_err()
        );
        intent["amounts"][0]["atoms"] = json!("75");
        let smaller = adapter.prepare("kamino.deposit", &intent).await.unwrap();
        adapter
            .validate_boundary_candidate(&permission.spec, &smaller)
            .unwrap();
        let mut excess = smaller.clone();
        excess.binding["protocol_candidate"]["metadata"]["stake_atoms"] = json!("11");
        assert!(
            adapter
                .validate_boundary_candidate(&permission.spec, &excess)
                .is_err()
        );
        let mut quote_excess = smaller.clone();
        quote_excess.binding["protocol_candidate"]["quote"]["details"]["stake_atoms"] = json!("11");
        assert!(
            adapter
                .validate_boundary_candidate(&permission.spec, &quote_excess)
                .is_err()
        );
        let mut missing = smaller;
        missing.binding["protocol_candidate"]["metadata"]["stake_atoms"] = Value::Null;
        assert!(
            adapter
                .validate_boundary_candidate(&permission.spec, &missing)
                .is_err()
        );
        bounded["maximum_stake_shares_atoms"] = json!("9");
        intent["amounts"][0]["atoms"] = json!("100");
        let tighter = adapter
            .prepare_plan_permission("kamino.deposit", &intent, &bounded)
            .await
            .unwrap();
        assert!(
            adapter
                .validate_boundary_candidate(&tighter.spec, &prepared)
                .is_err()
        );
    }
    // Old non-staking persisted specs omit the new optional parameter.
    let permission = adapter
        .prepare_plan_permission("kamino.deposit", &input(), &bounds())
        .await
        .unwrap();
    let mut old = permission.spec;
    old.parameters
        .as_object_mut()
        .unwrap()
        .remove("maximum_stake_shares_atoms");
    let prepared = adapter.prepare("kamino.deposit", &input()).await.unwrap();
    adapter
        .validate_boundary_candidate(&old, &prepared)
        .unwrap();
    let mut unrelated_cap = bounds();
    unrelated_cap["maximum_stake_shares_atoms"] = json!("10");
    assert!(
        adapter
            .prepare_plan_permission("kamino.deposit", &input(), &unrelated_cap)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn preapproval_rejects_unapproved_debits_and_network_fee() {
    let (a, rpc, journal, _, malicious) = fixture();
    malicious.store(true, Ordering::SeqCst);
    assert!(a.prepare("kamino.deposit", &input()).await.is_err());
    malicious.store(false, Ordering::SeqCst);
    rpc.fee.store(10001, Ordering::SeqCst);
    assert!(a.prepare("kamino.deposit", &input()).await.is_err());
    assert!(journal.persisted.lock().unwrap().is_none());
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "sendTransaction")
    );
}
#[tokio::test]
async fn strict_quantities_network_and_unknown_fields_are_rejected() {
    let (a, _, _, _, _) = fixture();
    for bad in [
        json!("01"),
        json!("-1"),
        json!("1.5"),
        json!("18446744073709551616"),
        json!(1),
    ] {
        let mut i = input();
        i["amounts"][0]["atoms"] = bad;
        assert!(a.prepare("kamino.deposit", &i).await.is_err());
    }
    let mut i = input();
    i["network"] = json!("ethereum");
    assert!(a.prepare("kamino.deposit", &i).await.is_err());
    i = input();
    i["swap"] = json!(true);
    assert!(a.prepare("kamino.deposit", &i).await.is_err());
}
#[tokio::test]
async fn pending_and_expired_quotes_are_not_reauthorized_as_fresh() {
    let (a, _, _, _, _) = fixture();
    let mut p = a.prepare("kamino.deposit", &input()).await.unwrap();
    p.binding["protocol_candidate"]["quote"]["expires_at"] = json!("2000-01-01T00:00:00Z");
    assert!(a.revalidate(&p).await.is_err());
}

#[tokio::test]
async fn multi_family_reads_keep_successful_observations_and_expose_position_value() {
    let (adapter, _, _, _, faults) = fixture();
    faults.store(true, Ordering::SeqCst);
    let vaults = adapter.read("kamino.vaults", &json!({})).await.unwrap();
    assert_eq!(
        vaults["families"][0]["page"]["vaults"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(vaults["families"][1]["page"]["complete"], false);
    assert!(
        !vaults["families"][1]["page"]["warnings"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let positions = adapter
        .read("kamino.positions", &json!({"wallet":"wallet"}))
        .await
        .unwrap();
    let lending = &positions["families"][0]["inventory"]["positions"][0];
    assert_eq!(lending["current_value"]["complete"], true);
    assert_eq!(lending["current_value"]["usd_micros"], "125");
    assert_eq!(lending["accounting"]["historical_principal_known"], false);
    assert_eq!(positions["families"][1]["inventory"]["complete"], false);
    assert!(
        adapter
            .read("kamino.vaults", &json!({"cursor":"next"}))
            .await
            .is_err()
    );
}
