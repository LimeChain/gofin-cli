use super::*;
use gofin_solana::rpc::SolanaRpc;
use std::sync::Mutex;

struct OfflineApi;
#[async_trait]
impl common::KaminoApi for OfflineApi {
    async fn get(&self, _path: &str) -> Result<Value> {
        Err(invalid("API unavailable in controlled fixture"))
    }
}
fn key(seed: u8) -> String {
    bs58::encode([seed; 32]).into_string()
}
fn account(program: &str, data: Vec<u8>) -> Value {
    json!({"owner":program,"lamports":2039280,"executable":false,"rentEpoch":0,"data":[STANDARD.encode(data),"base64"]})
}
fn bytes(value: &Value) -> Vec<u8> {
    STANDARD.decode(value["data"][0].as_str().unwrap()).unwrap()
}
fn blank(idl: &Value, name: &str) -> Vec<u8> {
    let mut b = vec![0; idl::account_size(idl, name).unwrap()];
    b[..8].copy_from_slice(&idl::discriminator("account", name));
    b
}
fn set_bytes(data: &mut [u8], idl: &Value, name: &str, field: &str, bytes: &[u8]) {
    let offset = idl::field_offset(idl, name, field).unwrap();
    data[offset..offset + bytes.len()].copy_from_slice(bytes)
}
fn set_key(data: &mut [u8], idl: &Value, name: &str, field: &str, key: &str) {
    set_bytes(data, idl, name, field, &key_bytes(key).unwrap())
}
fn set_u64(data: &mut [u8], idl: &Value, name: &str, field: &str, n: u64) {
    set_bytes(data, idl, name, field, &n.to_le_bytes())
}
fn mint(authority: &str, decimals: u8, supply: u64) -> Vec<u8> {
    let mut d = vec![0; 82];
    d[..4].copy_from_slice(&1u32.to_le_bytes());
    d[4..36].copy_from_slice(&key_bytes(authority).unwrap());
    d[36..44].copy_from_slice(&supply.to_le_bytes());
    d[44] = decimals;
    d[45] = 1;
    d
}
fn token(mint: &str, owner: &str, amount: u64) -> Vec<u8> {
    let mut d = vec![0; 165];
    d[..32].copy_from_slice(&key_bytes(mint).unwrap());
    d[32..64].copy_from_slice(&key_bytes(owner).unwrap());
    d[64..72].copy_from_slice(&amount.to_le_bytes());
    d[108] = 1;
    d
}
struct FixtureRpc {
    accounts: Mutex<BTreeMap<String, Value>>,
    owner: String,
    vault: String,
    tokens: Vec<TokenInfo>,
    action: Mutex<VaultAction>,
    withdraw: Mutex<u64>,
    calls: Mutex<Vec<(String, Value)>>,
}
#[async_trait]
impl SolanaRpc for FixtureRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.calls
            .lock()
            .unwrap()
            .push((method.into(), params.clone()));
        let accounts = self.accounts.lock().unwrap();
        match method {
            "getGenesisHash" => Ok(json!(gofin_solana::MAINNET_GENESIS_HASH)),
            "getSlot" => Ok(json!(100)),
            "getAccountInfo" => Ok(
                json!({"context":{"slot":100},"value":accounts.get(params[0].as_str().unwrap())}),
            ),
            "getMultipleAccounts" => Ok(
                json!({"context":{"slot":100},"value":params[0].as_array().unwrap().iter().map(|k|accounts.get(k.as_str().unwrap()).cloned().unwrap_or(Value::Null)).collect::<Vec<_>>()}),
            ),
            "getProgramAccounts" => {
                let program = params[0].as_str().unwrap();
                let mut found = Vec::new();
                for (key, a) in accounts.iter() {
                    if a["owner"] != program {
                        continue;
                    }
                    let data = bytes(a);
                    let matches = params[1]["filters"].as_array().unwrap().iter().all(|f| {
                        if let Some(size) = f["dataSize"].as_u64() {
                            data.len() == size as usize
                        } else {
                            let offset = f["memcmp"]["offset"].as_u64().unwrap() as usize;
                            let filter = bs58::decode(f["memcmp"]["bytes"].as_str().unwrap())
                                .into_vec()
                                .unwrap();
                            data.get(offset..offset + filter.len()) == Some(filter.as_slice())
                        }
                    });
                    if matches {
                        let mut a = a.clone();
                        a["data"] = json!(["", "base64"]);
                        found.push(json!({"pubkey":key,"account":a}));
                    }
                }
                Ok(json!({"context":{"slot":100},"value":found}))
            }
            "getTokenAccountsByOwner" => {
                let owner = params[0].as_str().unwrap();
                let mut found = Vec::new();
                for (key, a) in accounts.iter() {
                    if a["owner"] != TOKEN && a["owner"] != TOKEN_2022 {
                        continue;
                    }
                    let data = bytes(a);
                    if data.len() < 165 || data[32..64] != key_bytes(owner)? {
                        continue;
                    }
                    let mint = bs58::encode(&data[..32]).into_string();
                    if params[1]["mint"].as_str().is_some_and(|m| m != mint)
                        || params[1]["programId"]
                            .as_str()
                            .is_some_and(|p| a["owner"] != p)
                    {
                        continue;
                    }
                    let mut a = a.clone();
                    if params[2]["encoding"] == "jsonParsed" {
                        a["data"] = json!({"parsed":{"info":{"mint":mint,"owner":owner,"tokenAmount":{"amount":u64::from_le_bytes(data[64..72].try_into().unwrap()).to_string()}}}})
                    }
                    found.push(json!({"pubkey":key,"account":a}));
                }
                Ok(json!({"context":{"slot":100},"value":found}))
            }
            "getLatestBlockhash" => Ok(
                json!({"context":{"slot":100},"value":{"blockhash":key(70),"lastValidBlockHeight":999}}),
            ),
            "getMinimumBalanceForRentExemption" => Ok(json!(2039280)),
            "getFeeForMessage" => Ok(json!({"context":{"slot":100},"value":5000})),
            "simulateTransaction" => {
                let action = *self.action.lock().unwrap();
                let returned = params[1]["accounts"]["addresses"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| {
                        let address = a.as_str().unwrap();
                        let mut value = accounts.get(address).cloned().unwrap_or(Value::Null);
                        for (i, t) in self.tokens.iter().enumerate() {
                            if address
                                == gofin_solana::associated_token_address(
                                    &self.owner,
                                    &t.mint,
                                    TOKEN,
                                )
                                .unwrap()
                            {
                                let before = if value.is_null() {
                                    0
                                } else {
                                    u64::from_le_bytes(bytes(&value)[64..72].try_into().unwrap())
                                };
                                let after = match (action, i) {
                                    (VaultAction::Deposit, 0 | 1) => before - 100,
                                    (VaultAction::Deposit, 2) => before + 20,
                                    (VaultAction::Withdraw, 0 | 1) => before + 100,
                                    (VaultAction::Withdraw, 2) => {
                                        before - *self.withdraw.lock().unwrap()
                                    }
                                    _ => unreachable!(),
                                };
                                value = account(TOKEN, token(&t.mint, &self.owner, after));
                            }
                        }
                        value
                    })
                    .collect::<Vec<_>>();
                Ok(
                    json!({"context":{"slot":100},"value":{"err":null,"logs":[],"innerInstructions":[],"unitsConsumed":100000,"fee":5000,"accounts":returned}}),
                )
            }
            _ => Err(invalid(&format!("unexpected fixture RPC method {method}"))),
        }
    }
}
fn fixture(
    dex: u64,
) -> (
    Arc<FixtureRpc>,
    SolanaRuntime,
    LiquidityProtocol,
    VaultIntent,
) {
    let owner = key(20);
    let vault = key(21);
    let global = key(22);
    let pool = key(23);
    let a = key(24);
    let b = key(25);
    let coll = key(26);
    let av = pda(PROGRAM, &[b"svault_a", &key_bytes(&vault).unwrap()]).unwrap();
    let bv = pda(PROGRAM, &[b"svault_b", &key_bytes(&vault).unwrap()]).unwrap();
    let authority = pda(
        PROGRAM,
        &[
            b"authority",
            &key_bytes(&av).unwrap(),
            &key_bytes(&bv).unwrap(),
        ],
    )
    .unwrap();
    let share = pda(
        PROGRAM,
        &[
            b"shares",
            &key_bytes(&vault).unwrap(),
            &key_bytes(&a).unwrap(),
            &key_bytes(&b).unwrap(),
        ],
    )
    .unwrap();
    let share_authority = pda(PROGRAM, &[b"authority", &key_bytes(&share).unwrap()]).unwrap();
    let assets = vec![
        TokenInfo {
            mint: a.clone(),
            decimals: 6,
            token_program: TOKEN.into(),
        },
        TokenInfo {
            mint: b.clone(),
            decimals: 6,
            token_program: TOKEN.into(),
        },
        TokenInfo {
            mint: share.clone(),
            decimals: 6,
            token_program: TOKEN.into(),
        },
    ];
    let mut strategy = blank(idl::liquidity(), "WhirlpoolStrategy");
    for (field, k) in [
        ("globalConfig", global.as_str()),
        ("pool", pool.as_str()),
        ("tokenAMint", a.as_str()),
        ("tokenBMint", b.as_str()),
        ("tokenAVault", av.as_str()),
        ("tokenBVault", bv.as_str()),
        ("baseVaultAuthority", authority.as_str()),
        ("sharesMint", share.as_str()),
        ("sharesMintAuthority", share_authority.as_str()),
        ("poolTokenVaultA", key(30).as_str()),
        ("poolTokenVaultB", key(31).as_str()),
    ] {
        set_key(
            &mut strategy,
            idl::liquidity(),
            "WhirlpoolStrategy",
            field,
            k,
        )
    }
    for (field, n) in [
        ("strategyDex", dex),
        ("status", 1),
        ("tokenAMintDecimals", 6),
        ("tokenBMintDecimals", 6),
        ("sharesMintDecimals", 6),
        ("sharesIssued", 1000),
        ("depositCapUsd", 1_000_000_000_000),
        ("tokenAAmounts", 10000),
        ("tokenBAmounts", 10000),
        ("tokenBCollateralId", 1),
    ] {
        set_u64(
            &mut strategy,
            idl::liquidity(),
            "WhirlpoolStrategy",
            field,
            n,
        )
    }
    set_bytes(
        &mut strategy,
        idl::liquidity(),
        "WhirlpoolStrategy",
        "creationStatus",
        &[2],
    );
    let mut global_data = blank(idl::liquidity(), "GlobalConfig");
    set_key(
        &mut global_data,
        idl::liquidity(),
        "GlobalConfig",
        "tokenInfos",
        &coll,
    );
    let (pool_idl, name, mints, vault_fields) = match dex {
        0 => (
            idl::liquidity(),
            "Whirlpool",
            ["tokenMintA", "tokenMintB"],
            ["tokenVaultA", "tokenVaultB"],
        ),
        1 => (
            idl::liquidity(),
            "PoolState",
            ["tokenMint0", "tokenMint1"],
            ["tokenVault0", "tokenVault1"],
        ),
        _ => (
            idl::meteora(),
            "LbPair",
            ["tokenXMint", "tokenYMint"],
            ["reserveX", "reserveY"],
        ),
    };
    let mut pool_data = blank(pool_idl, name);
    set_key(&mut pool_data, pool_idl, name, mints[0], &a);
    set_key(&mut pool_data, pool_idl, name, mints[1], &b);
    set_key(&mut pool_data, pool_idl, name, vault_fields[0], &key(30));
    set_key(&mut pool_data, pool_idl, name, vault_fields[1], &key(31));
    let mut collateral = blank(idl::liquidity(), "CollateralInfos");
    for (index, mint) in [&a, &b].iter().enumerate() {
        let o = 8 + index * 216;
        collateral[o..o + 32].copy_from_slice(&key_bytes(mint).unwrap());
        collateral[o + 152..o + 184].copy_from_slice(&key_bytes(&key(40)).unwrap());
    }
    let mut accounts = BTreeMap::from([
        (vault.clone(), account(PROGRAM, strategy)),
        (global, account(PROGRAM, global_data)),
        (coll, account(PROGRAM, collateral)),
        (pool.clone(), account(backend(dex).unwrap().1, pool_data)),
        (av, account(TOKEN, token(&a, &authority, 10000))),
        (bv, account(TOKEN, token(&b, &authority, 10000))),
        (key(30), account(TOKEN, token(&a, &pool, 10000))),
        (key(31), account(TOKEN, token(&b, &pool, 10000))),
    ]);
    for t in &assets {
        accounts.insert(
            t.mint.clone(),
            account(
                TOKEN,
                mint(
                    if t.mint == share {
                        &share_authority
                    } else {
                        &owner
                    },
                    6,
                    1000,
                ),
            ),
        );
        accounts.insert(
            gofin_solana::associated_token_address(&owner, &t.mint, TOKEN).unwrap(),
            account(TOKEN, token(&t.mint, &owner, 1000)),
        );
    }
    let intent = VaultIntent {
        wallet: owner.clone(),
        family: VaultFamily::Liquidity,
        action: VaultAction::Deposit,
        vault: vault.clone(),
        amounts: assets[..2].iter().map(|t| amount(t, 100)).collect(),
        shares_atoms: None,
        minimum_shares_atoms: None,
        maximum_network_fee_lamports: 10000,
        maximum_account_funding_lamports: 10000000,
        stake: false,
    };
    let rpc = Arc::new(FixtureRpc {
        accounts: Mutex::new(accounts),
        owner,
        vault,
        tokens: assets,
        action: Mutex::new(VaultAction::Deposit),
        withdraw: Mutex::new(10),
        calls: Mutex::new(vec![]),
    });
    let runtime = SolanaRuntime::new(rpc.clone());
    (
        rpc,
        runtime,
        LiquidityProtocol::with_api(Arc::new(OfflineApi)),
        intent,
    )
}

#[tokio::test]
async fn all_backend_deposits_and_partial_withdrawals_build_native_graphs() {
    for dex in 0..3 {
        let (rpc, runtime, protocol, mut intent) = fixture(dex);
        let candidate = protocol
            .build_candidate(&runtime, &intent, &rpc.owner)
            .await
            .unwrap();
        assert_eq!(candidate.quote.shares_atoms, "20");
        assert!(!candidate.quote.minimum_shares_enforced);
        assert_eq!(candidate.quote.debits.len(), 2);
        protocol
            .validate_candidate(&runtime, &intent, &rpc.owner, &candidate)
            .await
            .unwrap();
        assert!(
            candidate
                .instructions
                .iter()
                .all(|i| i.program_id == PROGRAM
                    || i.program_id == "ComputeBudget111111111111111111111111111111")
        );
        let mut tampered = candidate.clone();
        tampered.instructions.last_mut().unwrap().accounts[10].address = key(99);
        assert!(
            protocol
                .validate_candidate(&runtime, &intent, &rpc.owner, &tampered)
                .await
                .is_err()
        );
        *rpc.action.lock().unwrap() = VaultAction::Withdraw;
        intent.action = VaultAction::Withdraw;
        intent.amounts.clear();
        intent.shares_atoms = Some("10".into());
        let withdrawal = protocol
            .build_candidate(&runtime, &intent, &rpc.owner)
            .await
            .unwrap();
        assert_eq!(withdrawal.quote.shares_atoms, "10");
        assert_eq!(withdrawal.quote.credits.len(), 2);
        protocol
            .validate_candidate(&runtime, &intent, &rpc.owner, &withdrawal)
            .await
            .unwrap();
        *rpc.withdraw.lock().unwrap() = 1000;
        intent.shares_atoms = Some("1000".into());
        let full = protocol
            .build_candidate(&runtime, &intent, &rpc.owner)
            .await
            .unwrap();
        assert_eq!(full.quote.shares_atoms, "1000");
    }
}
#[tokio::test]
async fn discovery_fallback_positions_and_non_ata_shares_are_verified() {
    let (rpc, runtime, protocol, _) = fixture(0);
    let extra = key(91);
    rpc.accounts.lock().unwrap().insert(
        extra,
        account(TOKEN, token(&rpc.tokens[2].mint, &rpc.owner, 17)),
    );
    let page = protocol
        .vaults(
            &runtime,
            &VaultQuery {
                search: None,
                token_mint: None,
                limit: 10,
                cursor: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.vaults.len(), 1);
    assert!(page.warnings.iter().any(|w| w.contains("unavailable")));
    let p = protocol.positions(&runtime, &rpc.owner).await.unwrap();
    assert_eq!(p.positions.len(), 1);
    assert_eq!(p.positions[0].wallet_shares_atoms, "1017");
    assert_eq!(p.positions[0].underlying[0].atoms.as_deref(), Some("10170"));
    assert!(p.complete);
    assert_eq!(
        p.positions[0].protocol_state["valuation"]["basis"],
        "underlying_principal_only"
    );
    assert_eq!(
        p.positions[0].protocol_state["valuation"]["complete"],
        false
    );
    assert!(rpc.calls.lock().unwrap().iter().any(|(m, p)| {
        m == "getProgramAccounts"
            && p[1]["filters"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["memcmp"]["offset"] == 720)
    }));
}
#[tokio::test]
async fn wrong_owner_mint_or_canonical_vault_is_rejected() {
    for mutation in 0..3 {
        let (rpc, runtime, protocol, _) = fixture(0);
        {
            let mut accounts = rpc.accounts.lock().unwrap();
            if mutation == 0 {
                accounts.get_mut(&rpc.vault).unwrap()["owner"] = json!(RAYDIUM);
            } else {
                let value = accounts.get_mut(&rpc.vault).unwrap();
                let mut data = bytes(value);
                set_key(
                    &mut data,
                    idl::liquidity(),
                    "WhirlpoolStrategy",
                    if mutation == 1 {
                        "tokenAMint"
                    } else {
                        "tokenAVault"
                    },
                    &key(98),
                );
                *value = account(PROGRAM, data);
            }
        }
        assert!(protocol.vault(&runtime, &rpc.vault).await.is_err());
    }
}
#[tokio::test]
async fn candidate_reuses_one_vault_and_wallet_share_snapshot() {
    let (rpc, runtime, protocol, intent) = fixture(0);
    let extra = key(91);
    rpc.accounts.lock().unwrap().insert(
        extra.clone(),
        account(TOKEN, token(&rpc.tokens[2].mint, &rpc.owner, 17)),
    );
    let candidate = protocol
        .build_candidate(&runtime, &intent, &rpc.owner)
        .await
        .unwrap();
    assert_eq!(
        candidate.metadata["position_before"]["wallet_shares_atoms"],
        "1017"
    );
    assert!(
        candidate.metadata["wallet_share_accounts_before"]
            .as_array()
            .unwrap()
            .iter()
            .any(|account| account["address"] == extra && account["atoms"] == "17")
    );
    let calls = rpc.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|(method, params)| { method == "getAccountInfo" && params[0] == rpc.vault })
            .count(),
        1,
        "candidate position must reuse the verified strategy snapshot"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|(method, params)| {
                method == "getTokenAccountsByOwner" && params[1]["mint"] == rpc.tokens[2].mint
            })
            .count(),
        2,
        "one share inventory snapshot plus the independent simulation observation"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|(method, _)| method == "getTokenAccountsByOwner")
            .count(),
        4,
        "funded ATAs need only inventory and the three simulation token observations"
    );
}

#[tokio::test]
async fn candidate_fetches_blockhash_after_initial_graph_account_reads() {
    for action in [VaultAction::Deposit, VaultAction::Withdraw] {
        let (rpc, runtime, protocol, mut intent) = fixture(0);
        intent.action = action;
        *rpc.action.lock().unwrap() = action;
        if action == VaultAction::Withdraw {
            intent.amounts.clear();
            intent.shares_atoms = Some("10".into());
        }
        let candidate = protocol
            .build_candidate(&runtime, &intent, &rpc.owner)
            .await
            .unwrap();
        assert_eq!(candidate.blockhash, key(70));
        let calls = rpc.calls.lock().unwrap();
        let hash = calls
            .iter()
            .position(|(method, _)| method == "getLatestBlockhash")
            .unwrap();
        let graph_account = calls
            .iter()
            .rposition(|(method, _)| method == "getAccountInfo")
            .unwrap();
        let simulation = calls
            .iter()
            .position(|(method, _)| method == "simulateTransaction")
            .unwrap();
        assert!(
            graph_account < hash && hash < simulation,
            "the blockhash must remain fresh through initial graph preparation"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _)| method == "getLatestBlockhash")
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn funded_atas_skip_owner_enumeration_for_native_graphs() {
    let (rpc, runtime, protocol, mut intent) = fixture(0);
    let state = protocol.state(&runtime, &rpc.vault).await.unwrap();
    for action in [VaultAction::Deposit, VaultAction::Withdraw] {
        intent.action = action;
        if action == VaultAction::Withdraw {
            intent.amounts.clear();
            intent.shares_atoms = Some("1000".into());
        }
        rpc.calls.lock().unwrap().clear();
        let graph = build_graph(&runtime, &intent, &rpc.owner, &state, 0)
            .await
            .unwrap();
        assert!(graph.instructions.iter().all(|ix| ix.program_id != TOKEN));
        assert!(
            rpc.calls
                .lock()
                .unwrap()
                .iter()
                .all(|(method, _)| method != "getTokenAccountsByOwner")
        );
    }
}

#[tokio::test]
async fn ata_fast_path_revalidates_transfer_restrictions_before_signing() {
    for frozen in [false, true] {
        let (rpc, runtime, protocol, intent) = fixture(0);
        let candidate = protocol
            .build_candidate(&runtime, &intent, &rpc.owner)
            .await
            .unwrap();
        let ata =
            gofin_solana::associated_token_address(&rpc.owner, &rpc.tokens[0].mint, TOKEN).unwrap();
        {
            let mut accounts = rpc.accounts.lock().unwrap();
            let value = accounts.get_mut(&ata).unwrap();
            let mut data = bytes(value);
            if frozen {
                data[108] = 2;
            } else {
                data[72..76].copy_from_slice(&1u32.to_le_bytes());
                data[76..108].copy_from_slice(&key_bytes(&key(95)).unwrap());
            }
            *value = account(TOKEN, data);
        }
        assert!(
            protocol
                .validate_candidate(&runtime, &intent, &rpc.owner, &candidate)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn partial_native_input_consolidates_owned_accounts_without_swap() {
    let (rpc, runtime, protocol, intent) = fixture(0);
    let t = &rpc.tokens[0];
    let ata = gofin_solana::associated_token_address(&rpc.owner, &t.mint, TOKEN).unwrap();
    let extra = key(92);
    {
        let mut a = rpc.accounts.lock().unwrap();
        a.insert(ata, account(TOKEN, token(&t.mint, &rpc.owner, 30)));
        a.insert(
            extra.clone(),
            account(TOKEN, token(&t.mint, &rpc.owner, 70)),
        );
    }
    let state = protocol.state(&runtime, &rpc.vault).await.unwrap();
    let graph = build_graph(&runtime, &intent, &rpc.owner, &state, 0)
        .await
        .unwrap();
    let transfer = graph
        .instructions
        .iter()
        .find(|i| i.program_id == TOKEN)
        .unwrap();
    assert_eq!(transfer.accounts[0].address, extra);
    assert_eq!(
        &STANDARD.decode(&transfer.data_base64).unwrap()[1..9],
        &70u64.to_le_bytes()
    );
    assert!(rpc.calls.lock().unwrap().iter().any(|(method, params)| {
        method == "getTokenAccountsByOwner" && params[1]["mint"] == t.mint
    }));
}
#[test]
fn unsupported_token_behaviors_do_not_gain_execution_eligibility() {
    let mut m = gofin_solana::token::parse_mint(TOKEN, &mint(&key(1), 6, 100), 0).unwrap();
    assert!(native_mint_supported(&m));
    m.extensions.push("TransferFeeConfig".into());
    assert!(!native_mint_supported(&m));
    m.extensions = vec!["MetadataPointer".into()];
    assert!(native_mint_supported(&m));
    m.permanent_delegate = Some(key(2));
    assert!(!native_mint_supported(&m));
    m.permanent_delegate = None;
    m.transfer_hook_program = Some(key(3));
    assert!(!native_mint_supported(&m));
    m.transfer_hook_program = None;
    m.paused = Some(false);
    assert!(!native_mint_supported(&m));
}

#[tokio::test]
async fn exact_disclosed_stake_can_include_existing_shares_when_minted_receipt_drifts() {
    let (rpc, runtime, protocol, intent) = fixture(0);
    let mut candidate = protocol
        .build_candidate(&runtime, &intent, &rpc.owner)
        .await
        .unwrap();
    let farm = crate::farms::FarmInfo {
        address: key(80),
        token: rpc.tokens[2].clone(),
        vault: key(81),
        authority: key(82),
        scope_prices: ZERO.into(),
        frozen: false,
        delegated: false,
        time_unit: 1,
        warmup: 0,
        cooldown: 0,
        locking_mode: 0,
        locking_start: 0,
        locking_duration: 0,
        early_penalty_bps: 0,
        deposit_cap: 0,
        total_staked: 0,
        total_active_scaled: "0".into(),
        total_pending_scaled: "0".into(),
        total_pending_amount: 0,
        withdraw_authority: ZERO.into(),
        reward_tokens: vec![],
    };
    let user = crate::farms::UserFarmInfo {
        address: crate::farms::user_state_address(&farm.address, &rpc.owner).unwrap(),
        ..Default::default()
    };
    candidate.metadata["position_before"]["protocol_state"]["farm_custody"] =
        json!({"farm":farm,"user":user,"chain_time":100});
    candidate.metadata["farm"] = json!(farm);
    candidate.metadata["stake_atoms"] = json!("100");
    let token_balance = |index: u64, owner: &str, atoms: &str| json!({"accountIndex":index,"mint":rpc.tokens[2].mint,"owner":owner,"programId":TOKEN,"uiTokenAmount":{"amount":atoms,"decimals":6}});
    let proof:FinalizedReceiptProof=serde_json::from_value(json!({"signature":"fixture","slot":101,"fee_lamports":5000,"failed":false,"message_sha256":"fixture","account_keys":[rpc.owner,gofin_solana::associated_token_address(&rpc.owner,&rpc.tokens[2].mint,TOKEN).unwrap(),farm.vault],"message_instructions":[],"required_signatures":[],"pre_balances_lamports":[0,0,0],"post_balances_lamports":[0,0,0],"pre_token_balances":[token_balance(1,&rpc.owner,"1000"),token_balance(2,&farm.authority,"0")],"post_token_balances":[token_balance(1,&rpc.owner,"920"),token_balance(2,&farm.authority,"100")],"loaded_addresses":{},"inner_instructions":[],"log_messages":[],"error":null})).unwrap();
    let position = finalized_position(
        &candidate,
        &proof,
        &rpc.owner,
        &SignedQuantity::new("20").unwrap(),
        Some(&(
            farm.clone(),
            crate::farms::UserFarmInfo {
                active_atoms: 100,
                ..Default::default()
            },
        )),
    )
    .unwrap();
    assert_eq!(position.wallet_shares_atoms, "920");
    assert_eq!(position.staked_shares_atoms, "100");
    assert_eq!(position_total(&position).unwrap(), 1020);
    let rounded = finalized_position(
        &candidate,
        &proof,
        &rpc.owner,
        &SignedQuantity::new("19").unwrap(),
        Some(&(
            farm.clone(),
            crate::farms::UserFarmInfo {
                active_atoms: 99,
                fractional_claims: json!({"active":{"numerator":"999","denominator":"1000"}}),
                ..Default::default()
            },
        )),
    )
    .unwrap();
    assert_eq!(position_total(&rounded).unwrap(), 1019);
    assert!(
        !rounded.protocol_state["finalized_farm_entitlement"]["user"]["fractional_claims"]
            .is_null()
    );
    let mut drifted = proof.clone();
    drifted.pre_token_balances[0]["uiTokenAmount"]["amount"] = json!("1001");
    assert!(verify_wallet_prestate(&candidate, &drifted, &rpc.owner).is_err());
}

#[tokio::test]
async fn disabled_collateral_is_reported_before_deposit_and_preserves_withdrawal_discovery() {
    let (rpc, runtime, protocol, intent) = fixture(2);
    {
        let mut accounts = rpc.accounts.lock().unwrap();
        let collateral = accounts
            .values_mut()
            .find(|a| {
                a["owner"] == PROGRAM
                    && bytes(a).starts_with(&idl::discriminator("account", "CollateralInfos"))
            })
            .unwrap();
        let mut data = bytes(collateral);
        data[8 + 136] = 1;
        *collateral = account(PROGRAM, data);
    }
    let state = protocol.state(&runtime, &intent.vault).await.unwrap();
    assert!(!state.info.deposits_available);
    assert!(state.info.withdrawals_available);
    assert!(
        state
            .info
            .restrictions
            .iter()
            .any(|r| r.contains("collateral") && r.contains("disabled"))
    );
    assert!(
        protocol
            .build_candidate(&runtime, &intent, &rpc.owner)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn fee_change_after_review_invalidates_identical_native_instruction_graph() {
    let (rpc, runtime, protocol, intent) = fixture(0);
    let candidate = protocol
        .build_candidate(&runtime, &intent, &rpc.owner)
        .await
        .unwrap();
    {
        let mut accounts = rpc.accounts.lock().unwrap();
        let strategy = accounts.get_mut(&intent.vault).unwrap();
        let mut data = bytes(strategy);
        set_u64(
            &mut data,
            idl::liquidity(),
            "WhirlpoolStrategy",
            "withdrawFee",
            100,
        );
        *strategy = account(PROGRAM, data);
    }
    let error = protocol
        .validate_candidate(&runtime, &intent, &rpc.owner, &candidate)
        .await
        .unwrap_err();
    assert!(error.message.contains("fee terms changed"));
}

#[test]
fn farm_rounding_cannot_broaden_the_reviewed_withdrawal_limit() {
    let (_, _, _, mut intent) = fixture(0);
    intent.action = VaultAction::Withdraw;
    intent.shares_atoms = Some("10".into());
    let mut simulation = Simulation {
        underlying: vec![5, 5],
        shares: -10,
        owned_shares: -11,
        farm_entitlement: Value::Null,
        fee: 5000,
    };
    assert!(check_withdrawal_entitlement(&intent, &simulation).is_err());
    simulation.owned_shares = -10;
    assert!(check_withdrawal_entitlement(&intent, &simulation).is_ok());
    simulation.owned_shares = -9;
    assert!(check_withdrawal_entitlement(&intent, &simulation).is_ok());
}

#[test]
fn fractional_only_deposit_is_rejected_before_approval() {
    let (_, _, _, intent) = fixture(0);
    let simulation = Simulation {
        underlying: vec![-1, -1],
        shares: 1,
        owned_shares: 0,
        farm_entitlement: Value::Null,
        fee: 5000,
    };
    assert!(check_withdrawal_entitlement(&intent, &simulation).is_err());
}

#[tokio::test]
async fn api_token_filter_precedes_candidate_bound_and_preserves_unknown_assets() {
    struct CatalogApi(Vec<Value>);
    #[async_trait]
    impl common::KaminoApi for CatalogApi {
        async fn get(&self, _: &str) -> Result<Value> {
            Ok(json!(self.0))
        }
    }
    let (rpc, runtime, _, _) = fixture(0);
    let mint = rpc.tokens[0].mint.clone();
    let unrelated = key(89);
    let matching_a = key(90);
    let missing_assets = key(91);
    let matching_b = key(92);
    let mut rows =
        vec![json!({"address":unrelated,"tokenAMint":key(95),"tokenBMint":key(96)}); MAX_DISCOVERY];
    rows.extend([
        json!({"address":matching_a,"tokenAMint":mint,"tokenBMint":key(96)}),
        json!({"address":matching_b,"tokenAMint":key(95),"tokenBMint":mint}),
        json!({"address":missing_assets}),
    ]);
    let protocol = LiquidityProtocol::with_api(Arc::new(CatalogApi(rows)));
    let (addresses, complete, warnings) = protocol
        .strategy_addresses(
            &runtime,
            &VaultQuery {
                search: None,
                token_mint: Some(mint),
                limit: 1,
                cursor: None,
            },
        )
        .await
        .unwrap();
    assert!(complete);
    assert!(warnings.is_empty());
    assert!(!addresses.contains(&unrelated));
    assert!(addresses.contains(&matching_a));
    assert!(addresses.contains(&matching_b));
    assert!(addresses.contains(&missing_assets));
    assert!(addresses.contains(&rpc.vault));
    assert_eq!(addresses.len(), 4);
}
