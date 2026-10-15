//! Cross-VM continuation of the common CCTP fixture. All keys and RPC evidence are synthetic.
use super::*;
#[path = "cctp_binance_funding.rs"]
mod binance;
#[path = "cctp_solana_pricing.rs"]
mod pricing;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bincode::Options;
use gofin_bridge::cctp::solana as protocol;
use gofin_solana::{SolanaRpc, SolanaSigner, SolanaWalletAdapter, token::TOKEN_PROGRAM_ID};
use sha2::{Digest, Sha256};
use solana_transaction::versioned::VersionedTransaction;
const SOL_BALANCE: u64 = 1_000_000_000;
const FEE: u64 = 5_000;
const RENT: u64 = 1_000_000;
fn key(value: &str) -> Vec<u8> {
    bs58::decode(value).into_vec().unwrap()
}
fn discriminator(namespace: &str, name: &str) -> Vec<u8> {
    Sha256::digest(format!("{namespace}:{name}").as_bytes())[..8].to_vec()
}
fn pda(program: &str, seeds: &[&[u8]]) -> String {
    solana_pubkey::Pubkey::find_program_address(seeds, &program.parse().unwrap())
        .0
        .to_string()
}
fn signer() -> Arc<SolanaSigner> {
    let pair = ed25519_dalek::SigningKey::from_bytes(&[42; 32]).to_keypair_bytes();
    Arc::new(SolanaSigner::parse(&serde_json::to_string(&pair.to_vec()).unwrap()).unwrap())
}
fn account(owner: &str, data: Vec<u8>, executable: bool) -> Value {
    json!({"owner":owner,"data":[STANDARD.encode(data),"base64"],"lamports":RENT,"executable":executable,"rentEpoch":0})
}
fn state_account(owner: &str, name: &str, data: Vec<u8>) -> Value {
    let mut bytes = discriminator("account", name);
    bytes.extend(data);
    account(owner, bytes, false)
}
fn token_account(owner: &str, atoms: u64) -> Value {
    let mut data = vec![0; 165];
    data[..32].copy_from_slice(&key(protocol::USDC_MINT));
    data[32..64].copy_from_slice(&key(owner));
    data[64..72].copy_from_slice(&atoms.to_le_bytes());
    data[108] = 1;
    account(TOKEN_PROGRAM_ID, data, false)
}
#[derive(Default)]
struct SolanaState {
    transaction: Option<Value>,
    signature: Option<String>,
    sends: usize,
    amount: u64,
    hidden: bool,
    lose_response: bool,
    wrong_credit: bool,
    wrong_event: bool,
    gas_missing: bool,
    block_height: u64,
    advance_on_balance: u64,
    advance_on_fee: u64,
    latest_blockhash_commitments: Vec<String>,
}
pub(super) struct SolanaFixture {
    state: Mutex<SolanaState>,
    owner: String,
    ata: String,
    source: Arc<Mutex<ChainState>>,
    pub(super) binance: std::sync::OnceLock<Arc<binance::BinanceFixture>>,
    pub(super) prices: std::sync::OnceLock<Arc<dyn UsdPriceSource>>,
}
impl SolanaFixture {
    fn new(source: Arc<Mutex<ChainState>>) -> Arc<Self> {
        let owner = signer().address().to_owned();
        let ata =
            gofin_solana::associated_token_address(&owner, protocol::USDC_MINT, TOKEN_PROGRAM_ID)
                .unwrap();
        Arc::new(Self {
            state: Mutex::new(SolanaState::default()),
            owner,
            ata,
            source,
            binance: std::sync::OnceLock::new(),
            prices: std::sync::OnceLock::new(),
        })
    }
    pub(super) fn wallet(self: &Arc<Self>) -> Arc<SolanaWalletAdapter> {
        Arc::new(
            SolanaWalletAdapter::new(
                "model-solana",
                &self.owner,
                Some(signer()),
                self.clone(),
                self.prices
                    .get()
                    .cloned()
                    .unwrap_or_else(|| Arc::new(Prices)),
            )
            .unwrap(),
        )
    }
    fn accounts(&self, used: bool, atoms: u64) -> Vec<Value> {
        let program = protocol::TOKEN_MESSENGER_MINTER;
        let mint = key(protocol::USDC_MINT);
        let custody = pda(program, &[b"custody", &mint]);
        let minter = pda(program, &[b"token_minter"]);
        let local = pda(program, &[b"local_token", &mint]);
        let mut transmitter = vec![0; 128];
        transmitter.push(0);
        for n in [5u32, 1, 1, 1] {
            transmitter.extend(n.to_le_bytes());
        }
        transmitter.extend(attester().address().into_word());
        transmitter.extend(8192u64.to_le_bytes());
        let mut messenger = vec![0; 96];
        messenger.extend(1u32.to_le_bytes());
        messenger.push(0);
        messenger.extend([43; 32]);
        let mut remote = 6u32.to_le_bytes().to_vec();
        remote.extend(address(TOKEN_MESSENGER).unwrap().into_word());
        let mut minter_data = vec![0; 64];
        minter_data.push(0);
        let mut local_data = key(&custody);
        local_data.extend(&mint);
        let mut pair = 6u32.to_le_bytes().to_vec();
        pair.extend(
            address(&usdc_asset(8453).unwrap().token.unwrap())
                .unwrap()
                .into_word(),
        );
        pair.extend(key(&local));
        let mut mint_data = vec![0; 82];
        mint_data[44] = 6;
        mint_data[45] = 1;
        vec![
            account("BPFLoaderUpgradeab1e11111111111111111111111", vec![], true),
            account("BPFLoaderUpgradeab1e11111111111111111111111", vec![], true),
            state_account(
                protocol::MESSAGE_TRANSMITTER,
                "MessageTransmitter",
                transmitter,
            ),
            state_account(program, "TokenMessenger", messenger),
            state_account(program, "RemoteTokenMessenger", remote),
            state_account(program, "TokenMinter", minter_data),
            state_account(program, "LocalToken", local_data),
            state_account(program, "TokenPair", pair),
            token_account(&minter, 100_000_000),
            account(TOKEN_PROGRAM_ID, mint_data, false),
            token_account(&self.owner, atoms),
            token_account(&bs58::encode([43; 32]).into_string(), 0),
            if used {
                state_account(protocol::MESSAGE_TRANSMITTER, "UsedNonce", vec![1])
            } else {
                Value::Null
            },
        ]
    }
    fn finalized(&self, wire: &str, s: &SolanaState) -> Value {
        let raw = STANDARD.decode(wire).unwrap();
        let tx: VersionedTransaction = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .reject_trailing_bytes()
            .deserialize(&raw)
            .unwrap();
        tx.verify_and_hash_message().unwrap();
        let keys: Vec<_> = tx
            .message
            .static_account_keys()
            .iter()
            .map(ToString::to_string)
            .collect();
        let receive = tx
            .message
            .instructions()
            .iter()
            .enumerate()
            .find(|(_, i)| keys[i.program_id_index as usize] == protocol::MESSAGE_TRANSMITTER)
            .unwrap();
        let raw_message = self.source.lock().unwrap().attested().unwrap().0;
        let m = decode_message(&raw_message).unwrap();
        let net = m
            .amount
            .checked_sub(&m.fee_executed)
            .unwrap()
            .as_str()
            .parse::<u64>()
            .unwrap();
        let event_prefix = [228, 69, 165, 46, 81, 203, 154, 29];
        let mut received = event_prefix.to_vec();
        received.extend(discriminator("event", "MessageReceived"));
        received.extend(key(&self.owner));
        received.extend(m.source_domain.to_le_bytes());
        received.extend(gofin_evm::hash(&m.nonce).unwrap());
        received.extend(address(TOKEN_MESSENGER).unwrap().into_word());
        received.extend(m.finality_threshold_executed.to_le_bytes());
        let body = &gofin_evm::rpc_bytes(&json!(raw_message)).unwrap()[148..];
        received.extend((body.len() as u32).to_le_bytes());
        received.extend(body);
        let mut minted = event_prefix.to_vec();
        minted.extend(discriminator("event", "MintAndWithdraw"));
        minted.extend(key(&self.ata));
        minted.extend((net - u64::from(s.wrong_event)).to_le_bytes());
        minted.extend(key(protocol::USDC_MINT));
        minted.extend(
            m.fee_executed
                .as_str()
                .parse::<u64>()
                .unwrap()
                .to_le_bytes(),
        );
        let idx = |key: &str| keys.iter().position(|k| k == key).unwrap();
        let nonce = protocol::used_nonce_address(&m.nonce).unwrap();
        let nonce_index = idx(&nonce);
        let mut create = 0u32.to_le_bytes().to_vec();
        create.extend(RENT.to_le_bytes());
        create.extend(9u64.to_le_bytes());
        create.extend(key(protocol::MESSAGE_TRANSMITTER));
        let inner_call = |program: &str, accounts: Vec<usize>, data: Vec<u8>| json!({"programIdIndex":idx(program),"accounts":accounts,"data":bs58::encode(data).into_string()});
        let inner = vec![json!({"index":receive.0,"instructions":[
            inner_call("11111111111111111111111111111111",vec![0,nonce_index],create),
            inner_call(protocol::MESSAGE_TRANSMITTER,vec![idx(&pda(protocol::MESSAGE_TRANSMITTER,&[b"__event_authority"]))],received),
            inner_call(protocol::TOKEN_MESSENGER_MINTER,vec![idx(&pda(protocol::TOKEN_MESSENGER_MINTER,&[b"__event_authority"]))],minted)
        ]})];
        let mut pre = vec![RENT; keys.len()];
        let mut post = pre.clone();
        pre[0] = SOL_BALANCE;
        post[0] = SOL_BALANCE - FEE - RENT;
        pre[nonce_index] = 0;
        post[nonce_index] = RENT;
        let fee_owner = bs58::encode([43; 32]).into_string();
        let fee_ata = gofin_solana::associated_token_address(
            &fee_owner,
            protocol::USDC_MINT,
            TOKEN_PROGRAM_ID,
        )
        .unwrap();
        let custody = pda(
            protocol::TOKEN_MESSENGER_MINTER,
            &[b"custody", &key(protocol::USDC_MINT)],
        );
        let minter = pda(protocol::TOKEN_MESSENGER_MINTER, &[b"token_minter"]);
        let token = |address: &str, owner: &str, atoms: u64| json!({"accountIndex":idx(address),"mint":protocol::USDC_MINT,"owner":owner,"programId":TOKEN_PROGRAM_ID,"uiTokenAmount":{"amount":atoms.to_string(),"decimals":6,"uiAmount":null}});
        let pre_tokens = vec![
            token(&self.ata, &self.owner, 0),
            token(&fee_ata, &fee_owner, 0),
            token(&custody, &minter, 100_000_000),
        ];
        let post_tokens = vec![
            token(&self.ata, &self.owner, net - u64::from(s.wrong_credit)),
            token(
                &fee_ata,
                &fee_owner,
                m.fee_executed.as_str().parse().unwrap(),
            ),
            token(
                &custody,
                &minter,
                100_000_000 - m.amount.as_str().parse::<u64>().unwrap(),
            ),
        ];
        json!({"slot":99,"transaction":[wire,"base64"],"meta":{"fee":FEE,"err":null,"preBalances":pre,"postBalances":post,"preTokenBalances":pre_tokens,"postTokenBalances":post_tokens,"loadedAddresses":{"writable":[],"readonly":[]},"innerInstructions":inner,"logMessages":[]}})
    }
}
#[async_trait]
impl SolanaRpc for SolanaFixture {
    async fn request(&self, method: &str, p: Value) -> Result<Value> {
        let mut s = self.state.lock().unwrap();
        Ok(match method {
            "getGenesisHash"=>json!(gofin_solana::MAINNET_GENESIS_HASH),"getSlot"=>json!(100),"getBlockHeight"=>json!(s.block_height),
            "getEpochInfo"=>json!({"epoch":1,"slotIndex":1,"slotsInEpoch":432000,"absoluteSlot":100}),
            "getBalance"=>{s.block_height+=std::mem::take(&mut s.advance_on_balance);json!({"context":{"slot":100},"value":if s.gas_missing {0}else{SOL_BALANCE}})},
            "getLatestBlockhash"=>{s.latest_blockhash_commitments.push(p[0]["commitment"].as_str().unwrap().into());let mut hash=[0u8;32];hash[..8].copy_from_slice(&s.block_height.to_le_bytes());json!({"context":{"slot":100},"value":{"blockhash":bs58::encode(hash).into_string(),"lastValidBlockHeight":s.block_height+150}})},
            "isBlockhashValid"=>json!({"context":{"slot":100},"value":true}),
            "getFeeForMessage"=>{s.block_height+=std::mem::take(&mut s.advance_on_fee);json!({"context":{"slot":100},"value":FEE})},"getMinimumBalanceForRentExemption"=>json!(RENT),
            "simulateTransaction"=>json!({"context":{"slot":100},"value":{"err":null,"unitsConsumed":200000}}),
            "getAccountInfo"=>{let key=p[0].as_str().unwrap();let accounts=self.accounts(s.signature.is_some(),s.amount);let value=if key==self.ata {accounts[10].clone()}else if key==protocol::USDC_MINT {accounts[9].clone()}else if key==pda(protocol::TOKEN_MESSENGER_MINTER,&[b"token_messenger"]) {accounts[3].clone()}else if self.source.lock().unwrap().attested().is_some_and(|(raw,_)|decode_message(&raw).ok().is_some_and(|m|protocol::used_nonce_address(&m.nonce).unwrap()==key)) {accounts[12].clone()}else{Value::Null};json!({"context":{"slot":100},"value":value})},
            "getMultipleAccounts"=>{let count=p[0].as_array().unwrap().len();let mut accounts=self.accounts(s.signature.is_some(),s.amount);accounts.truncate(count);json!({"context":{"slot":100},"value":accounts})},
            "sendTransaction"=>{s.sends+=1;let wire=p[0].as_str().unwrap();let tx:VersionedTransaction=bincode::DefaultOptions::new().with_fixint_encoding().reject_trailing_bytes().deserialize(&STANDARD.decode(wire).unwrap()).unwrap();let signature=tx.signatures[0].to_string();s.transaction=Some(self.finalized(wire,&s));s.signature=Some(signature.clone());let raw=self.source.lock().unwrap().attested().unwrap().0;let m=decode_message(&raw).unwrap();s.amount=m.amount.checked_sub(&m.fee_executed).unwrap().as_str().parse().unwrap();if s.lose_response {s.lose_response=false;return Err(fail("SOLANA_RPC_UNAVAILABLE"));}json!(signature)},
            "getSignatureStatuses"=>json!({"value":[if s.signature.is_some(){json!({"confirmationStatus":if s.hidden{"confirmed"}else{"finalized"},"err":null})}else{Value::Null}]}),
            "getTransaction"=>if s.hidden{Value::Null}else{s.transaction.clone().unwrap_or(Value::Null)},
            "getSignaturesForAddress"=>json!(s.signature.as_ref().map(|sig|vec![json!({"signature":sig,"slot":99,"err":null,"confirmationStatus":"finalized"})]).unwrap_or_default()),
            other=>return Err(GoFinError::validation(format!("Unexpected CCTP Solana fixture RPC {other}"))),
        })
    }
}
fn harness() -> Harness {
    let chain = Arc::new(Mutex::new(ChainState {
        source_balance: U256::from(20_000_000),
        ..Default::default()
    }));
    let iris = Arc::new(Iris::new(chain.clone()));
    let solana = SolanaFixture::new(chain.clone());
    Harness::build_with_solana(
        tempfile::tempdir().unwrap(),
        chain,
        Arc::new(Approvals::default()),
        iris,
        Some(solana),
    )
}
async fn quote(h: &Harness) -> (String, Vec<Value>, FinancialPlanSpec) {
    quote_amount(h, 10_000_000, true).await
}
async fn quote_amount(
    h: &Harness,
    atoms: u64,
    fast: bool,
) -> (String, Vec<Value>, FinancialPlanSpec) {
    let solana = h.solana.as_ref().unwrap();
    let mut bounds = h.bounds();
    for cap in &mut bounds.fee_caps {
        if cap.asset.network == "ethereum:1" {
            cap.asset.network = "ethereum:8453".into();
        }
    }
    bounds
        .fee_caps
        .retain(|cap| cap.asset.network == "ethereum:8453");
    bounds.fee_caps.push(FeeCap {
        asset: AssetId {
            network: gofin_solana::NETWORK.into(),
            reference: "native".into(),
            decimals: 9,
        },
        kind: FeeKind::Gas,
        maximum: Quantity::from_u64(100_000),
        maximum_usd_micros: None,
    });
    bounds.fee_caps.push(FeeCap {
        asset: AssetId {
            network: gofin_solana::NETWORK.into(),
            reference: "native".into(),
            decimals: 9,
        },
        kind: FeeKind::AdditionalDebit,
        maximum: Quantity::from_u64(2_000_000),
        maximum_usd_micros: None,
    });
    bounds.fee_caps.push(FeeCap {
        asset: protocol::asset(),
        kind: FeeKind::WithheldFromOutput,
        maximum: Quantity::from_u64(if fast { 5_000 } else { 0 }),
        maximum_usd_micros: None,
    });
    if !fast {
        bounds
            .fee_caps
            .retain(|cap| cap.kind != FeeKind::WithheldFromOutput);
    }
    let q=h.read("bridge.quotes",json!({"provider":"cctp","cctp":{"transferMode":if fast {"fast"} else {"standard"}},"intent":{"wallet":"model-wallet","source":{"chainId":8453,"token":usdc_asset(8453).unwrap().token},"solanaDestination":{"network":gofin_solana::NETWORK,"wallet":"model-solana","mint":protocol::USDC_MINT},"recipient":solana.owner,"refundTo":h.wallet,"allowedRefundAssets":[],"amount":atoms.to_string()},"bounds":bounds})).await;
    assert_eq!(q["transfers"].as_array().map(Vec::len), Some(1), "{q}");
    let transfer = q["transfers"][0]["transfer_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let links: Vec<BridgeStepLink> =
        serde_json::from_value(q["transfers"][0]["steps"].clone()).unwrap();
    let inputs:Vec<_>=links.iter().map(|link|json!({"transferId":transfer,"stepId":link.step_id,"stepKind":link.kind,"bounds":bounds})).collect();
    let mut spec = h.plan(&inputs, fast);
    let dest = ResourceId(format!("{}:{}", gofin_solana::NETWORK, solana.owner));
    spec.steps
        .last_mut()
        .unwrap()
        .completion
        .minimum_net_receipt = Some(gofin_domain::ResourceAmount {
        resource: dest.clone(),
        amount: gofin_domain::Amount {
            asset: protocol::asset(),
            atoms: Quantity::from_u64(if fast { atoms - 5_000 } else { atoms }),
        },
    });
    spec.aggregate_bounds
        .maximum_costs
        .retain(|c| c.amount.asset.network == "ethereum:1");
    for cap in &mut spec.aggregate_bounds.maximum_costs {
        cap.amount.asset.network = "ethereum:8453".into();
        cap.location = gofin_domain::AccountingLocation::Cash {
            resource: ResourceId(format!("ethereum:8453:{}", h.wallet)),
        };
    }
    spec.aggregate_bounds.maximum_debits[0] = gofin_domain::ResourceAmount {
        resource: ResourceId(format!("ethereum:8453:{}", h.wallet)),
        amount: usdc_asset(8453).unwrap().amount(Quantity::from_u64(atoms)),
    };
    spec.aggregate_bounds
        .maximum_costs
        .push(gofin_domain::CostBound {
            location: gofin_domain::AccountingLocation::Cash { resource: dest },
            amount: gofin_domain::Amount {
                asset: AssetId {
                    network: gofin_solana::NETWORK.into(),
                    reference: "native".into(),
                    decimals: 9,
                },
                atoms: Quantity::from_u64(2_100_000),
            },
            treatment: gofin_domain::CostTreatment::AdditionalDebit,
        });
    (transfer, inputs, spec)
}
#[tokio::test]
async fn cross_vm_plan_burn_mint_and_finalized_usdc_credit() {
    let h = harness();
    let (transfer, inputs, spec) = quote(&h).await;
    let plan = h.authorize(spec).await;
    for (i, input) in inputs.iter().enumerate().take(2) {
        h.settle(h.execute(&plan, i, input).await.unwrap()).await;
    }
    assert_ne!(h.status(&transfer).await["state"], "completed");
    assert_eq!(h.solana.as_ref().unwrap().state.lock().unwrap().sends, 0);
    h.chain.lock().unwrap().attestation_ready = true;
    assert_eq!(
        h.status(&transfer).await["provider_status"]["status"],
        "mint_ready"
    );
    let op = h
        .execute_public(
            &plan,
            "stage-2",
            OperationRequest {
                request_id: "solana-mint".into(),
                operation: "bridge.transfer".into(),
                input: inputs[2].clone(),
                run_id: None,
            },
        )
        .await;
    h.settle(op).await;
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
    assert_eq!(h.solana.as_ref().unwrap().state.lock().unwrap().sends, 1);
}

async fn burn(h: &Harness) -> (String, Vec<Value>, String) {
    let (transfer, inputs, spec) = quote(h).await;
    let plan = h.authorize(spec).await;
    for (i, input) in inputs.iter().enumerate().take(2) {
        h.settle(h.execute(&plan, i, input).await.unwrap()).await;
    }
    h.chain.lock().unwrap().attestation_ready = true;
    (transfer, inputs, plan)
}

#[tokio::test]
async fn cross_vm_mint_freezes_execution_hash_after_slow_preparation() {
    let h = harness();
    let (_transfer, inputs, _plan) = burn(&h).await;
    let fixture = h.solana.as_ref().unwrap();
    {
        let mut state = fixture.state.lock().unwrap();
        // Each controlled read advances beyond an entire blockhash lifetime.
        // No wall-clock sleeps or weakened expiry checks are needed.
        state.advance_on_balance = 200;
        state.advance_on_fee = 200;
        state.latest_blockhash_commitments.clear();
    }
    let prepared = h
        .bridge
        .prepare("bridge.transfer", &inputs[2])
        .await
        .unwrap();
    assert_eq!(prepared.binding["solana_last_valid_block_height"], 550);
    assert_eq!(prepared.binding["solana_maximum_network_fee_lamports"], FEE);
    assert_eq!(
        fixture.state.lock().unwrap().latest_blockhash_commitments,
        ["confirmed", "confirmed"]
    );
    let validated = h.bridge.revalidate(&prepared).await.unwrap();
    assert_eq!(
        validated.binding["solana_message_sha256"],
        prepared.binding["solana_message_sha256"]
    );
    assert_eq!(validated.binding["solana_last_valid_block_height"], 550);
    fixture.state.lock().unwrap().block_height = 551;
    assert_eq!(
        h.bridge.revalidate(&validated).await.unwrap_err().code,
        "SOLANA_BLOCKHASH_EXPIRED"
    );
    assert_eq!(fixture.state.lock().unwrap().sends, 0);
}

#[tokio::test]
async fn cross_vm_pending_response_loss_restarts_without_duplicate_mint() {
    let h = harness();
    let (transfer, inputs, plan) = burn(&h).await;
    {
        let mut state = h.solana.as_ref().unwrap().state.lock().unwrap();
        state.hidden = true;
        state.lose_response = true;
    }
    let op = h.execute(&plan, 2, &inputs[2]).await.unwrap();
    assert!(op.submission.is_some(), "{:?}", op.error);
    assert_ne!(op.status, Status::Completed);
    let h = h.restart();
    let pending = h.status(&transfer).await;
    assert_ne!(pending["state"], "completed");
    assert!(
        h.state
            .transfer(&transfer)
            .unwrap()
            .unwrap()
            .transit
            .is_some()
    );
    h.solana.as_ref().unwrap().state.lock().unwrap().hidden = false;
    let settled = h.settle(op).await;
    let status = h.status(&transfer).await;
    assert_eq!(status["state"], "completed");
    let proof = &status["settlement"]["proof"];
    assert_eq!(proof["native_cost_accounting"]["kind"], "gofin_operation");
    assert_eq!(proof["native_cost_accounting"]["operation_id"], settled.id);
    assert_eq!(proof["native_costs"][0]["amount"]["atoms"], FEE.to_string());
    assert_eq!(
        proof["native_costs"][1]["amount"]["atoms"],
        RENT.to_string()
    );
    let h = h.restart();
    let repeated = h.status(&transfer).await;
    assert_eq!(
        repeated["settlement"]["proof"]["native_costs"],
        proof["native_costs"]
    );
    assert_eq!(
        repeated["settlement"]["proof"]["native_cost_accounting"],
        proof["native_cost_accounting"]
    );
    assert_eq!(h.solana.as_ref().unwrap().state.lock().unwrap().sends, 1);
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(8453, 2)])
    );
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cross_vm_wrong_credit_or_circle_event_cannot_settle() {
    for wrong_credit in [true, false] {
        let h = harness();
        let (transfer, inputs, plan) = burn(&h).await;
        {
            let mut state = h.solana.as_ref().unwrap().state.lock().unwrap();
            state.wrong_credit = wrong_credit;
            state.wrong_event = !wrong_credit;
        }
        let op = h.execute(&plan, 2, &inputs[2]).await.unwrap();
        assert!(op.submission.is_some(), "{:?}", op.error);
        let observed = h.operation_status(&op.id).await;
        assert_ne!(observed.status, Status::Completed);
        let observed = h
            .bridge
            .read("bridge.status", &json!({"transferId":transfer}))
            .await;
        match observed {
            Ok(status) => assert_ne!(status["state"], "completed"),
            Err(error) => assert_eq!(error.code, "CCTP_VALIDATION_FAILED"),
        }
        assert!(
            h.state
                .transfer(&transfer)
                .unwrap()
                .unwrap()
                .transit
                .is_some()
        );
        assert_eq!(h.solana.as_ref().unwrap().state.lock().unwrap().sends, 1);
    }
}

#[tokio::test]
async fn cross_vm_gas_shortage_and_revocation_block_new_mint() {
    let h = harness();
    let (transfer, inputs, plan) = burn(&h).await;
    h.solana.as_ref().unwrap().state.lock().unwrap().gas_missing = true;
    assert!(
        h.bridge
            .prepare("bridge.transfer", &inputs[2])
            .await
            .is_err()
    );
    assert_eq!(h.solana.as_ref().unwrap().state.lock().unwrap().sends, 0);
    assert!(
        h.state
            .transfer(&transfer)
            .unwrap()
            .unwrap()
            .transit
            .is_some()
    );
    h.solana.as_ref().unwrap().state.lock().unwrap().gas_missing = false;
    h.engine.revoke_financial_plan(&plan).await.unwrap();
    assert!(h.execute(&plan, 2, &inputs[2]).await.is_err());
    assert_eq!(h.solana.as_ref().unwrap().state.lock().unwrap().sends, 0);
}

#[tokio::test]
async fn named_binance_transfer_base_withdrawal_cctp_solana_uses_one_approval() {
    let chain = Arc::new(Mutex::new(ChainState::default()));
    let signer = LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap();
    let owner = lower_address(signer.address());
    let fixture = SolanaFixture::new(chain.clone());
    let price_api = pricing::enable(&fixture);
    assert!(
        fixture
            .binance
            .set(binance::BinanceFixture::new(chain.clone(), owner))
            .is_ok()
    );
    let h = Harness::build_with_solana(
        tempfile::tempdir().unwrap(),
        chain.clone(),
        Arc::new(Approvals::default()),
        Arc::new(Iris::new(chain)),
        Some(fixture),
    );
    let (transfer, inputs, mut spec) = quote_amount(&h, 9_800_000, false).await;
    let coin = AssetId {
        network: "binance".into(),
        reference: "USDC".into(),
        decimals: 36,
    };
    let main = ResourceId("binance:uid:1001:SPOT".into());
    let child = ResourceId("binance:uid:1002:SPOT".into());
    let wallet = ResourceId(format!("ethereum:8453:{}", h.wallet));
    let base_asset = usdc_asset(8453).unwrap().asset_id();
    let qty = Quantity::from_decimal("10", 36).unwrap();
    let child_debit = json!({"resource":child,"amount":{"asset":coin,"atoms":qty}});
    let main_debit = json!({"resource":main,"amount":{"asset":coin,"atoms":qty}});
    let withdrawal_fee = json!({"location":{"kind":"cash","resource":main},"amount":{"asset":coin,"atoms":Quantity::from_decimal("0.2",36).unwrap()},"treatment":"included_in_debit"});
    let transfer_input = json!({"requestId":"child-main-template","authority":"model-main","from":"model-child","to":"model-main","coin":"USDC","amount":"10","fromWallet":"SPOT","toWallet":"SPOT"});
    let withdraw_input = json!({"requestId":"withdraw-template","account":"model-main","destination":"model-wallet","coin":"USDC","amount":"10","network":"BASE","walletType":"SPOT","maxFee":"0.2","minReceiveAmount":"9.8"});
    let first: FinancialPlanStepSpec=serde_json::from_value(json!({"step_id":"consolidate","operation":"binance.transfer","input":transfer_input,"permitted_variations":{"mode":"exact","maximum_debits":[child_debit],"maximum_costs":[],"possible_credits":[{"resource":main,"asset":coin}]},"completion":{"operation_succeeded":true,"minimum_net_receipt":main_debit},"funding_dependencies":[]})).unwrap();
    let second: FinancialPlanStepSpec=serde_json::from_value(json!({"step_id":"withdraw","operation":"binance.withdraw","input":withdraw_input,"permitted_variations":{"mode":"exact","maximum_debits":[main_debit],"maximum_costs":[withdrawal_fee],"possible_credits":[{"resource":wallet,"asset":base_asset}]},"completion":{"operation_succeeded":true,"minimum_net_receipt":{"resource":wallet,"amount":{"asset":base_asset,"atoms":"9800000"}}},"funding_dependencies":[{"producing_step_id":"consolidate","resource":main,"asset":coin,"funding_scope":"principal_only"}]})).unwrap();
    spec.steps[1].funding_dependencies=serde_json::from_value(json!([{"producing_step_id":"withdraw","resource":wallet,"asset":base_asset,"funding_scope":"principal_only"}])).unwrap();
    spec.steps.splice(0..0, [first, second]);
    spec.aggregate_bounds.maximum_debits.extend([
        serde_json::from_value(child_debit).unwrap(),
        serde_json::from_value(main_debit).unwrap(),
    ]);
    spec.aggregate_bounds
        .maximum_costs
        .push(serde_json::from_value(withdrawal_fee).unwrap());
    spec.name = "Synthetic Binance child to main to Base to Solana".into();
    let plan = h.authorize(spec).await;
    let premature = h
        .runtime
        .handle(BrokerRequest::ExecutePlanStep {
            plan_id: plan.clone(),
            step_id: "withdraw".into(),
            request: OperationRequest {
                request_id: "premature-withdraw".into(),
                operation: "binance.withdraw".into(),
                input: withdraw_input.clone(),
                run_id: None,
            },
        })
        .await;
    assert!(
        !premature.ok,
        "An uncredited child transfer cannot fund the withdrawal"
    );
    assert_eq!(h.chain.lock().unwrap().source_balance, U256::ZERO);
    for (id, operation, input) in [
        ("consolidate", "binance.transfer", transfer_input),
        ("withdraw", "binance.withdraw", withdraw_input),
    ] {
        let op = h
            .execute_public(
                &plan,
                id,
                OperationRequest {
                    request_id: format!("execute-{id}"),
                    operation: operation.into(),
                    input,
                    run_id: None,
                },
            )
            .await;
        h.settle(op).await;
    }
    assert_eq!(
        h.chain.lock().unwrap().source_balance,
        U256::from(9_800_000)
    );
    for (i, input) in inputs.iter().enumerate() {
        if i == 2 {
            h.chain.lock().unwrap().attestation_ready = true;
        }
        let op = h
            .execute_public(
                &plan,
                &format!("stage-{i}"),
                OperationRequest {
                    request_id: format!("bridge-{i}"),
                    operation: "bridge.transfer".into(),
                    input: input.clone(),
                    run_id: None,
                },
            )
            .await;
        h.settle(op).await;
    }
    assert_eq!(h.status(&transfer).await["state"], "completed");
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.chain.lock().unwrap().broadcasts,
        BTreeMap::from([(8453, 2)])
    );
    assert_eq!(h.solana.as_ref().unwrap().state.lock().unwrap().sends, 1);
    assert_eq!(
        price_api.count(),
        2,
        "Full review, approval and five writes reuse fresh SOL and USDC prices"
    );
}

#[tokio::test]
async fn cross_vm_used_nonce_without_finalized_receipt_does_not_create_credit() {
    let h = harness();
    let (transfer, inputs, _) = burn(&h).await;
    {
        let mut s = h.solana.as_ref().unwrap().state.lock().unwrap();
        s.signature = Some(bs58::encode([7u8; 64]).into_string());
        s.hidden = true;
    }
    let status = h.status(&transfer).await;
    assert_ne!(status["state"], "completed");
    assert!(
        h.state
            .transfer(&transfer)
            .unwrap()
            .unwrap()
            .transit
            .is_some()
    );
    assert!(
        h.bridge
            .prepare("bridge.transfer", &inputs[2])
            .await
            .is_err()
    );
    assert_eq!(h.solana.as_ref().unwrap().state.lock().unwrap().sends, 0);
}
