use async_trait::async_trait;
use fin_domain::{
    Adapter, ApprovalAudit, AssetPrice, AuthorizationChallenge, FinError, OperationRequest,
    Quantity, Status,
};
use fin_engine::{ConfigurationAuthorizer, Engine, EngineOptions};
use fin_exchange::{
    ExchangeAdapter,
    ccxt::{
        HyperliquidAction, HyperliquidActionEnvelope, HyperliquidExplorerRequest,
        HyperliquidInfoRequest,
    },
};
use fin_hyperliquid::{
    DeferredExecutionState, HyperliquidAccount, HyperliquidExecutionScope, HyperliquidNonces,
    HyperliquidPrices, HyperliquidProfile, HyperliquidTransport,
    account_actions::{HyperliquidWithdrawalProof, WithdrawalProofRequest, WithdrawalReceiptProof},
    usdc,
};
use fin_store::{Store, execution_state::ExecutionStateRepository};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

const OWNER: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CHILD: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const BUILDER: &str = "0xcccccccccccccccccccccccccccccccccccccccc";
const DESTINATION: &str = "0xdddddddddddddddddddddddddddddddddddddddd";
const TX: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";

#[derive(Default)]
struct ObservedState {
    ledger_rows: Vec<Value>,
    recipient_ledger_rows: Vec<Value>,
    transactions: Vec<Value>,
    by_hash: std::collections::BTreeMap<String, Value>,
    abstraction: Value,
    dex_abstraction: Value,
    referral: Value,
    builder_fee: Value,
    subaccounts: Value,
    child_role: Value,
    owner_role: Value,
    destination_role: Value,
    omit_class_transfer_receipt: bool,
    class_transfer_error: Option<Value>,
    leverage_owner: String,
    leverage_coin: String,
    leverage_type: String,
    leverage_value: u64,
    mismatched_leverage_after_dispatch: bool,
    margin_owner: String,
    margin_coin: String,
    isolated_margin: String,
    update_margin_state: bool,
    lose_response: bool,
    transfer_fixture: TransferFixture,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TransferFixture {
    #[default]
    Valid,
    WrongRecipient,
    WrongAmount,
    WrongSourceHash,
    DuplicateAction,
    MissingRecipientRow,
    FeeOverCeiling,
    FeeWithinCeiling,
    ActivationFee,
}

#[derive(Default)]
struct Venue {
    dispatches: AtomicUsize,
    state: Mutex<ObservedState>,
    last_action: Mutex<Option<HyperliquidActionEnvelope>>,
}

impl Venue {
    fn lose_next_response(&self) {
        self.state.lock().unwrap().lose_response = true;
    }

    fn lose_next_response_without_class_transfer_receipt(&self) {
        let mut state = self.state.lock().unwrap();
        state.lose_response = true;
        state.omit_class_transfer_receipt = true;
    }

    fn set_class_transfer_error(&self, error: Value) {
        self.state.lock().unwrap().class_transfer_error = Some(error);
    }

    fn add_contradictory_class_transfer_ledger_row(&self) {
        self.state.lock().unwrap().ledger_rows.push(json!({
            "hash":TX,
            "delta":{"type":"accountClassTransfer","toPerp":true,"usdc":"1"}
        }));
    }

    fn mismatch_class_transfer_detail_error(&self) {
        self.state.lock().unwrap().by_hash.get_mut(TX).unwrap()["error"] =
            json!("different error from details");
    }

    fn zero_class_transfer_explorer_time(&self) {
        let mut state = self.state.lock().unwrap();
        state.transactions.last_mut().unwrap()["time"] = json!(0);
        state.by_hash.get_mut(TX).unwrap()["time"] = json!(0);
    }

    fn add_conflicting_class_transfer_with_same_nonce(&self) {
        let mut state = self.state.lock().unwrap();
        let mut tx = state.transactions.last().unwrap().clone();
        let conflicting_hash = "0x2222222222222222222222222222222222222222222222222222222222222222";
        tx["hash"] = json!(conflicting_hash);
        tx["action"]["amount"] = json!("0.9");
        state.transactions.push(tx.clone());
        state.by_hash.insert(conflicting_hash.into(), tx);
    }

    fn set_transfer_fixture(&self, fixture: TransferFixture) {
        self.state.lock().unwrap().transfer_fixture = fixture;
    }

    fn set_destination_role(&self, role: &str) {
        self.state.lock().unwrap().destination_role = json!({"role":role});
    }

    fn set_owner_role(&self, role: Value) {
        self.state.lock().unwrap().owner_role = role;
    }

    fn set_abstraction(&self, mode: &str) {
        self.state.lock().unwrap().abstraction = json!({"abstraction":mode});
    }

    fn add_native_transaction(
        &self,
        account: &HyperliquidAccount,
        envelope: &HyperliquidActionEnvelope,
    ) -> Value {
        let hash = TX.to_owned();
        let action = envelope.action_wire_json().unwrap()["action"].clone();
        let tx = json!({
            "hash":hash,
            "user":account.owner,
            "action":action,
            "error":null,
            "block":123,
            "time":action.get("nonce").or_else(|| action.get("time")).cloned().unwrap_or(json!(1))
        });
        let mut state = self.state.lock().unwrap();
        state
            .transactions
            .push(json!({"hash":hash,"user":account.owner,"action":action,"error":null,"block":123,"time":tx["time"]}));
        state.by_hash.insert(hash, tx.clone());
        tx
    }
}

#[async_trait]
impl HyperliquidTransport for Venue {
    async fn info(&self, request: HyperliquidInfoRequest) -> Result<Value, FinError> {
        let state = self.state.lock().unwrap();
        Ok(match request {
            HyperliquidInfoRequest::MetaAndAssetCtxs => json!([
                {"universe":[{"name":"BTC","szDecimals":3,"maxLeverage":50}]},
                [{"markPx":"10","midPx":"10","oraclePx":"10"}]
            ]),
            HyperliquidInfoRequest::PerpDexs => json!([{"name":"xyz"},{"name":"nonusd"}]),
            HyperliquidInfoRequest::MetaAndAssetCtxsForDex { dex } if dex == "xyz" => json!([
                {"collateralToken":0,"universe":[{"name":"BTC","szDecimals":3,"maxLeverage":20}]},
                [{"markPx":"10","midPx":"10","oraclePx":"10"}]
            ]),
            HyperliquidInfoRequest::MetaAndAssetCtxsForDex { dex } if dex == "nonusd" => json!([
                {"collateralToken":1,"universe":[{"name":"BTC","szDecimals":3,"maxLeverage":20}]},
                [{"markPx":"10","midPx":"10","oraclePx":"10"}]
            ]),
            HyperliquidInfoRequest::SpotMetaAndAssetCtxs => json!([
                {"tokens":[
                    {"name":"USDC","index":0,"weiDecimals":8,"szDecimals":2,"tokenId":"0x6d1e7cde53ba9467b783cb7c530ce054"},
                    {"name":"ETH","index":1,"weiDecimals":18,"szDecimals":4,"tokenId":"0xeeee"}
                ],"universe":[{"name":"ETH/USDC","index":0,"tokens":[1,0]}]},
                [{"midPx":"10"}]
            ]),
            HyperliquidInfoRequest::ClearinghouseState { owner } => json!({
                "user": if state.margin_owner.is_empty() { owner } else { state.margin_owner.clone() },
                "assetPositions":[{
                    "position":{"coin":if state.margin_coin.is_empty() { "BTC" } else { &state.margin_coin },"szi":"1","entryPx":"10","leverage":{"type":"isolated","value":5},"marginUsed":"10","isolatedMargin":if state.isolated_margin.is_empty() { "10" } else { &state.isolated_margin }}
                }]
            }),
            HyperliquidInfoRequest::ClearinghouseStateForDex { owner, dex } => json!({
                "user": if state.margin_owner.is_empty() { owner } else { state.margin_owner.clone() },
                "assetPositions":[{
                    "position":{"coin":if state.margin_coin.is_empty() { format!("{dex}:BTC") } else { state.margin_coin.clone() },"szi":"1","entryPx":"10","leverage":{"type":"isolated","value":5},"marginUsed":"10","isolatedMargin":if state.isolated_margin.is_empty() { "10" } else { &state.isolated_margin }}
                }]
            }),
            HyperliquidInfoRequest::UserNonFundingLedgerUpdates { owner, .. } => {
                Value::Array(if owner.eq_ignore_ascii_case(DESTINATION) {
                    state.recipient_ledger_rows.clone()
                } else {
                    state.ledger_rows.clone()
                })
            }
            HyperliquidInfoRequest::UserVaultEquities { .. } => json!([]),
            HyperliquidInfoRequest::UserRateLimit { .. } => {
                json!({"nRequestsUsed":1,"nRequestsCap":1200,"cumVlm":"1"})
            }
            HyperliquidInfoRequest::UserAbstractionState { .. } => {
                if state.abstraction.is_null() {
                    json!({"abstraction":"disabled"})
                } else {
                    state.abstraction.clone()
                }
            }
            HyperliquidInfoRequest::UserDexAbstractionState { .. } => state.dex_abstraction.clone(),
            HyperliquidInfoRequest::ReferralState { .. } => {
                if state.referral.is_null() {
                    json!({"referredBy":{"code":""}})
                } else {
                    state.referral.clone()
                }
            }
            HyperliquidInfoRequest::MaxBuilderFee { .. } => {
                if state.builder_fee.is_null() {
                    json!(0)
                } else {
                    state.builder_fee.clone()
                }
            }
            HyperliquidInfoRequest::SubAccounts { .. } => {
                if state.subaccounts.is_null() {
                    json!([])
                } else {
                    state.subaccounts.clone()
                }
            }
            HyperliquidInfoRequest::UserRole { owner } => {
                if owner.eq_ignore_ascii_case(OWNER) {
                    if state.owner_role.is_null() {
                        json!({"role":"user"})
                    } else {
                        state.owner_role.clone()
                    }
                } else if owner.eq_ignore_ascii_case(DESTINATION) {
                    if state.destination_role.is_null() {
                        json!({"role":"user"})
                    } else {
                        state.destination_role.clone()
                    }
                } else {
                    if state.child_role.is_null() {
                        json!({"role":"user"})
                    } else {
                        state.child_role.clone()
                    }
                }
            }
            HyperliquidInfoRequest::ActiveAssetData { owner, coin } => json!({
                "user": if state.leverage_owner.is_empty() { owner } else { state.leverage_owner.clone() },
                "coin": if state.leverage_coin.is_empty() { coin } else { state.leverage_coin.clone() },
                "leverage": {
                    "type": if state.leverage_type.is_empty() { "isolated" } else { &state.leverage_type },
                    "value": if state.leverage_value == 0 { 5 } else { state.leverage_value }
                }
            }),
            other => {
                return Err(FinError::new(
                    "UNEXPECTED_FIXTURE_QUERY",
                    "test",
                    format!("unexpected query: {other:?}"),
                    "fix the synthetic fixture",
                ));
            }
        })
    }

    async fn explorer(&self, request: HyperliquidExplorerRequest) -> Result<Value, FinError> {
        let state = self.state.lock().unwrap();
        Ok(match request {
            HyperliquidExplorerRequest::UserDetails { .. } => json!({"txs":state.transactions}),
            HyperliquidExplorerRequest::TxDetails { hash } => {
                json!({"tx":state.by_hash.get(&hash).cloned().unwrap_or(Value::Null)})
            }
        })
    }

    async fn dispatch(
        &self,
        account: &HyperliquidAccount,
        envelope: &HyperliquidActionEnvelope,
        _journal: Arc<dyn fin_domain::SubmissionJournal>,
    ) -> Result<Value, FinError> {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        *self.last_action.lock().unwrap() = Some(envelope.clone());
        match &envelope.action {
            HyperliquidAction::UsdClassTransfer {
                amount_usdc,
                to_perp,
                ..
            } => {
                let omit_receipt =
                    std::mem::take(&mut self.state.lock().unwrap().omit_class_transfer_receipt);
                if !omit_receipt {
                    let error = self.state.lock().unwrap().class_transfer_error.take();
                    self.add_native_transaction(account, envelope);
                    if let Some(error) = error {
                        let mut state = self.state.lock().unwrap();
                        state.by_hash.get_mut(TX).unwrap()["error"] = error.clone();
                        state.transactions.last_mut().unwrap()["error"] = error;
                    } else {
                        self.state.lock().unwrap().ledger_rows.push(json!({
                            "hash":TX,"delta":{"type":"accountClassTransfer","toPerp":to_perp,"usdc":amount_usdc}
                        }));
                    }
                }
            }
            HyperliquidAction::Withdraw {
                amount_usdc,
                time_ms,
                ..
            } => {
                self.add_native_transaction(account, envelope);
                let gross = Quantity::from_decimal(amount_usdc, 36).unwrap();
                let fee = Quantity::from_decimal("0.01", 36).unwrap();
                let net = gross.checked_sub(&fee).unwrap();
                let ledger_hash = format!("0x{}", "b6".repeat(32));
                self.state.lock().unwrap().ledger_rows.push(json!({
                    "time":time_ms + 1,
                    "hash":ledger_hash,
                    "delta":{
                        "type":"withdraw",
                        "usdc":net.to_decimal(36),
                        "nonce":time_ms * 1000,
                        "fee":fee.to_decimal(36)
                    }
                }));
            }
            HyperliquidAction::UsdSend {
                destination,
                amount_usdc,
                ..
            } => {
                let fixture = self.state.lock().unwrap().transfer_fixture;
                self.add_native_transaction(account, envelope);
                if fixture == TransferFixture::DuplicateAction {
                    self.add_native_transaction(account, envelope);
                }
                let row_amount = if fixture == TransferFixture::WrongAmount {
                    "0.9"
                } else {
                    amount_usdc.as_str()
                };
                let fee = if fixture == TransferFixture::FeeOverCeiling {
                    "0.02"
                } else if fixture == TransferFixture::FeeWithinCeiling {
                    "0.005"
                } else {
                    "0.0"
                };
                let receipt_destination = if fixture == TransferFixture::WrongRecipient {
                    CHILD
                } else {
                    destination.as_str()
                };
                let mut state = self.state.lock().unwrap();
                let source_hash = if fixture == TransferFixture::WrongSourceHash {
                    "0x9999999999999999999999999999999999999999999999999999999999999999"
                } else {
                    TX
                };
                state.ledger_rows.push(json!({
                    "hash":source_hash,
                    "delta":{"type":"internalTransfer","usdc":row_amount,"user":OWNER,"destination":receipt_destination,"fee":fee}
                }));
                if fixture == TransferFixture::ActivationFee {
                    state.ledger_rows.push(json!({
                        "hash":TX,
                        "delta":{"type":"accountActivationGas","amount":"0.01","token":"HYPE"}
                    }));
                }
                if fixture != TransferFixture::MissingRecipientRow {
                    state.recipient_ledger_rows.push(json!({
                        "hash":TX,
                        "delta":{"type":"internalTransfer","usdc":row_amount,"user":OWNER,"destination":receipt_destination,"fee":fee}
                    }));
                }
            }
            HyperliquidAction::UserAbstraction { abstraction, .. } => {
                let value = match abstraction {
                    fin_exchange::ccxt::HyperliquidAbstraction::Disabled => "disabled",
                    fin_exchange::ccxt::HyperliquidAbstraction::UnifiedAccount => "unifiedAccount",
                    fin_exchange::ccxt::HyperliquidAbstraction::PortfolioMargin => {
                        "portfolioMargin"
                    }
                };
                self.state.lock().unwrap().abstraction = json!({"abstraction":value});
            }
            HyperliquidAction::UserDexAbstraction { enabled, .. } => {
                self.state.lock().unwrap().dex_abstraction = json!(enabled);
            }
            HyperliquidAction::SetRef { code } => {
                self.state.lock().unwrap().referral = json!({"referredBy":{"code":code}});
            }
            HyperliquidAction::ApproveBuilderFee { max_fee_rate, .. } => {
                let value = Quantity::from_decimal(max_fee_rate.trim_end_matches('%'), 6)
                    .unwrap()
                    .integer()
                    / 1000u64;
                self.state.lock().unwrap().builder_fee = json!(value.to_string());
            }
            HyperliquidAction::CreateSubAccount { name, .. } => {
                self.state.lock().unwrap().subaccounts =
                    json!([{"name":name,"subAccountUser":CHILD}]);
                self.state.lock().unwrap().child_role =
                    json!({"role":"subAccount","data":{"master":OWNER}});
            }
            HyperliquidAction::UpdateLeverage {
                leverage, is_cross, ..
            } => {
                let mut state = self.state.lock().unwrap();
                state.leverage_owner = account.execution_address().into();
                if state.leverage_coin.is_empty() {
                    state.leverage_coin = "BTC".into();
                }
                state.leverage_type = if *is_cross { "cross" } else { "isolated" }.into();
                state.leverage_value = if state.mismatched_leverage_after_dispatch {
                    leverage.saturating_sub(1).max(1) as u64
                } else {
                    *leverage as u64
                };
            }
            HyperliquidAction::UpdateIsolatedMargin { amount_usdc, .. } => {
                let mut state = self.state.lock().unwrap();
                if state.update_margin_state {
                    let current = Quantity::from_decimal(
                        if state.isolated_margin.is_empty() {
                            "10"
                        } else {
                            &state.isolated_margin
                        },
                        36,
                    )
                    .unwrap();
                    let amount =
                        Quantity::from_decimal(amount_usdc.trim_start_matches('-'), 36).unwrap();
                    let updated = if amount_usdc.starts_with('-') {
                        current.checked_sub(&amount).unwrap()
                    } else {
                        current.checked_add(&amount).unwrap()
                    };
                    state.isolated_margin = updated.to_decimal(36);
                }
            }
            _ => {}
        }
        if std::mem::take(&mut self.state.lock().unwrap().lose_response) {
            return Err(FinError::new(
                "SYNTHETIC_TIMEOUT",
                "submission",
                "synthetic response lost after dispatch",
                "reconcile the exact original operation",
            ));
        }
        Ok(json!({"status":"ok"}))
    }
}

#[derive(Default)]
struct Prices;

#[async_trait]
impl HyperliquidPrices for Prices {
    async fn usdc_price(&self) -> Result<AssetPrice, FinError> {
        Ok(AssetPrice {
            asset: usdc(),
            usd_micros_per_unit: Quantity::from_decimal("1000000", 0).unwrap(),
            unit_atoms: Quantity::from_decimal("1", 36).unwrap(),
            observed_at: fin_domain::now(),
            source: "synthetic-usdc-price".into(),
        })
    }
}

struct FinancialApproval;

#[async_trait]
impl fin_domain::Authorizer for FinancialApproval {
    async fn authorize(
        &self,
        challenge: &AuthorizationChallenge,
    ) -> Result<ApprovalAudit, FinError> {
        Ok(ApprovalAudit {
            mechanism: "synthetic-financial-approval".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: fin_domain::now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

struct ConfigurationApproval;

#[async_trait]
impl ConfigurationAuthorizer for ConfigurationApproval {
    async fn authorize_configuration(
        &self,
        challenge: &AuthorizationChallenge,
    ) -> Result<ApprovalAudit, FinError> {
        assert_eq!(challenge.purpose, "configuration_write");
        Ok(ApprovalAudit {
            mechanism: "synthetic-human-configuration-approval".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: fin_domain::now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

struct WithdrawalProof {
    wrong_recipient: bool,
}

#[async_trait]
impl HyperliquidWithdrawalProof for WithdrawalProof {
    async fn prove_sequenced_arbitrum_withdrawal(
        &self,
        request: &WithdrawalProofRequest,
    ) -> Result<Option<WithdrawalReceiptProof>, FinError> {
        Ok(Some(WithdrawalReceiptProof {
            transaction_hash: request.ledger_hash.clone(),
            sender: "0x2df1c51e09aecf9cacb7bc98cb1742757f163df7".into(),
            recipient: if self.wrong_recipient {
                CHILD.into()
            } else {
                request.destination.clone()
            },
            token: request.token.clone(),
            net_amount: Quantity::from_decimal(&request.net_amount.to_decimal(36), 6).unwrap(),
            sequenced: true,
            block_number: 321,
            event_digest: "synthetic-arbitrum-log-digest".into(),
        }))
    }
}

async fn engine(
    path: &std::path::Path,
    venue: Arc<Venue>,
    proof: Option<Arc<dyn HyperliquidWithdrawalProof>>,
) -> Engine {
    engine_with_scope(path, venue, proof, HyperliquidExecutionScope::Owner).await
}

async fn engine_with_scope(
    path: &std::path::Path,
    venue: Arc<Venue>,
    proof: Option<Arc<dyn HyperliquidWithdrawalProof>>,
    execution_scope: HyperliquidExecutionScope,
) -> Engine {
    let shared = Store::open(path).unwrap().share();
    let nonce_state = Arc::new(DeferredExecutionState::default());
    nonce_state
        .attach(Arc::new(ExecutionStateRepository(shared.clone())))
        .unwrap();
    let mut profile = HyperliquidProfile::new(
        vec![HyperliquidAccount {
            name: "main".into(),
            owner: OWNER.into(),
            credential_ref: Some("synthetic-signer".into()),
            signing_enabled: true,
            execution_scope,
        }],
        venue,
        Arc::new(Prices),
        nonce_state,
    )
    .unwrap();
    if let Some(proof) = proof {
        profile = profile.with_withdrawal_proof(proof);
    }
    let adapter: Arc<dyn Adapter> = Arc::new(ExchangeAdapter::new(profile));
    Engine::new_with_shared_store(
        shared,
        vec![adapter],
        Arc::new(FinancialApproval),
        EngineOptions::default(),
    )
    .unwrap()
}

fn request(request_id: &str, operation: &str, input: Value) -> OperationRequest {
    OperationRequest {
        request_id: request_id.into(),
        operation: operation.into(),
        input,
        run_id: None,
    }
}

fn transfer_input(request_id: &str, destination: &str) -> Value {
    json!({"request_id":request_id,"account":"main","asset":"USDC","amount":"1","destination":destination,"max_amount_usdc":"1","max_fee_usdc":"0.01"})
}

fn address_transfer_input(request_id: &str) -> Value {
    json!({"request_id":request_id,"account":"main","asset":"USDC","amount":"1","destination":"address","destination_address":DESTINATION,"max_amount_usdc":"1","max_fee_usdc":"0.01"})
}

#[tokio::test]
async fn class_transfer_requires_nonce_linked_native_and_ledger_proof() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("class-transfer.sqlite");
    let venue = Arc::new(Venue::default());
    let engine = engine(&path, venue.clone(), None).await;
    let req = request(
        "class-transfer",
        "hyperliquid.account.transfer",
        transfer_input("class-transfer", "perpetual"),
    );

    let settled = engine.execute(req.clone()).await.unwrap();
    assert_eq!(settled.status, Status::Completed, "{settled:#?}");
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
    let proof = settled.effects.as_ref().unwrap();
    assert!(matches!(
        proof.effects.as_slice(),
        [fin_domain::FinancialEffect::Transfer { debit, credit, .. }]
            if debit.atoms == Quantity::from_decimal("1", 36).unwrap()
                && credit.atoms == Quantity::from_decimal("1", 36).unwrap()
    ));
    assert_eq!(
        proof.evidence["method"],
        "native-class-transfer-nonce-and-ledger-hash"
    );
    let again = engine.execute(req).await.unwrap();
    assert_eq!(again.id, settled.id);
    assert_eq!(again.submission, settled.submission);
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn class_transfer_response_loss_recovers_from_exact_native_proof_without_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("class-transfer-lost.sqlite");
    let venue = Arc::new(Venue::default());
    venue.lose_next_response();
    let first = engine(&path, venue.clone(), None).await;
    let req = request(
        "class-transfer-lost",
        "hyperliquid.account.transfer",
        transfer_input("class-transfer-lost", "perpetual"),
    );
    let unknown = first.execute(req.clone()).await.unwrap();
    assert_eq!(unknown.status, Status::Unknown);
    let original_submission = unknown.submission.clone().unwrap();
    let nonce = original_submission.data["envelope"]["nonce_ms"]
        .as_u64()
        .unwrap();
    assert!(nonce > 0);
    drop(first);

    let restarted = engine(&path, venue.clone(), None).await;
    let recovered = restarted.status(&unknown.id, true).await.unwrap();
    assert_eq!(recovered.status, Status::Completed, "{recovered:#?}");
    assert_eq!(recovered.submission.as_ref(), Some(&original_submission));
    assert_eq!(
        recovered.effects.as_ref().unwrap().evidence["method"],
        "native-class-transfer-nonce-and-ledger-hash"
    );
    let duplicate = restarted.execute(req).await.unwrap();
    assert_eq!(duplicate.id, unknown.id);
    assert_eq!(duplicate.submission.as_ref(), Some(&original_submission));
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn class_transfer_response_loss_without_nonce_receipt_stays_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("class-transfer-no-proof.sqlite");
    let venue = Arc::new(Venue::default());
    venue.lose_next_response_without_class_transfer_receipt();
    let first = engine(&path, venue.clone(), None).await;
    let req = request(
        "class-transfer-no-proof",
        "hyperliquid.account.transfer",
        transfer_input("class-transfer-no-proof", "perpetual"),
    );
    let unknown = first.execute(req.clone()).await.unwrap();
    assert_eq!(unknown.status, Status::Unknown);
    let original_submission = unknown.submission.clone().unwrap();
    drop(first);

    let restarted = engine(&path, venue.clone(), None).await;
    let recovered = restarted.status(&unknown.id, true).await.unwrap();
    assert_eq!(recovered.status, Status::Unknown, "{recovered:#?}");
    assert_eq!(recovered.submission.as_ref(), Some(&original_submission));
    assert!(recovered.effects.is_none());
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
    let duplicate = restarted.execute(req).await.unwrap();
    assert_eq!(duplicate.id, unknown.id);
    assert_eq!(duplicate.submission.as_ref(), Some(&original_submission));
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn exact_committed_class_transfer_rejection_resolves_failed_without_effects() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("class-transfer-rejected.sqlite");
    let venue = Arc::new(Venue::default());
    venue.set_class_transfer_error(json!("Insufficient balance for token transfer"));
    venue.lose_next_response();
    let first = engine(&path, venue.clone(), None).await;
    let req = request(
        "class-transfer-rejected",
        "hyperliquid.account.transfer",
        transfer_input("class-transfer-rejected", "perpetual"),
    );
    let unknown = first.execute(req.clone()).await.unwrap();
    assert_eq!(unknown.status, Status::Unknown);
    let original_submission = unknown.submission.clone().unwrap();
    drop(first);

    let restarted = engine(&path, venue.clone(), None).await;
    let rejected = restarted.status(&unknown.id, true).await.unwrap();
    assert_eq!(rejected.status, Status::Failed, "{rejected:#?}");
    assert_eq!(rejected.submission.as_ref(), Some(&original_submission));
    let proof = rejected.effects.as_ref().unwrap();
    assert!(proof.effects.is_empty());
    assert_eq!(
        proof.evidence["method"],
        "exact-committed-native-class-transfer-rejection"
    );
    assert_eq!(
        proof.evidence["native_error"],
        "Insufficient balance for token transfer"
    );
    assert_eq!(proof.evidence["native_tx"]["block"], 123);
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
    let duplicate = restarted.execute(req).await.unwrap();
    assert_eq!(duplicate.id, unknown.id);
    assert_eq!(duplicate.submission.as_ref(), Some(&original_submission));
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unrecognized_or_malformed_native_class_transfer_error_stays_unknown() {
    for (suffix, error) in [
        ("other", json!("Unknown native execution error")),
        ("empty", json!("  ")),
        (
            "wrong-type",
            json!({"message":"Insufficient balance for token transfer"}),
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join(format!("class-transfer-error-{suffix}.sqlite"));
        let venue = Arc::new(Venue::default());
        venue.set_class_transfer_error(error);
        venue.lose_next_response();
        let first = engine(&path, venue.clone(), None).await;
        let request_id = format!("class-transfer-error-{suffix}");
        let req = request(
            &request_id,
            "hyperliquid.account.transfer",
            transfer_input(&request_id, "perpetual"),
        );
        let unknown = first.execute(req).await.unwrap();
        assert_eq!(unknown.status, Status::Unknown, "case {suffix}");
        let original_submission = unknown.submission.clone().unwrap();
        drop(first);

        let restarted = engine(&path, venue.clone(), None).await;
        let still_unknown = restarted.status(&unknown.id, true).await.unwrap();
        assert_eq!(still_unknown.status, Status::Unknown, "case {suffix}");
        assert_eq!(
            still_unknown.submission.as_ref(),
            Some(&original_submission)
        );
        assert!(still_unknown.effects.is_none());
        assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn rejected_class_transfer_requires_consistent_explorer_views_and_no_ledger_effect() {
    for (suffix, contradiction) in [("ledger", true), ("explorer", false)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join(format!("class-transfer-contradiction-{suffix}.sqlite"));
        let venue = Arc::new(Venue::default());
        venue.set_class_transfer_error(json!("Insufficient balance for token transfer"));
        venue.lose_next_response();
        if contradiction {
            venue.add_contradictory_class_transfer_ledger_row();
        }
        let first = engine(&path, venue.clone(), None).await;
        let request_id = format!("class-transfer-contradiction-{suffix}");
        let unknown = first
            .execute(request(
                &request_id,
                "hyperliquid.account.transfer",
                transfer_input(&request_id, "perpetual"),
            ))
            .await
            .unwrap();
        assert_eq!(unknown.status, Status::Unknown);
        if !contradiction {
            venue.mismatch_class_transfer_detail_error();
        }
        drop(first);

        let restarted = engine(&path, venue.clone(), None).await;
        let still_unknown = restarted.status(&unknown.id, true).await.unwrap();
        assert_eq!(still_unknown.status, Status::Unknown, "case {suffix}");
        assert_eq!(
            still_unknown.submission.as_ref(),
            unknown.submission.as_ref()
        );
        assert!(still_unknown.effects.is_none());
        assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn conflicting_same_nonce_action_prevents_rejected_transfer_settlement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("class-transfer-conflicting-nonce.sqlite");
    let venue = Arc::new(Venue::default());
    venue.set_class_transfer_error(json!("Insufficient balance for token transfer"));
    venue.lose_next_response();
    let first = engine(&path, venue.clone(), None).await;
    let request_id = "class-transfer-conflicting-nonce";
    let unknown = first
        .execute(request(
            request_id,
            "hyperliquid.account.transfer",
            transfer_input(request_id, "perpetual"),
        ))
        .await
        .unwrap();
    assert_eq!(unknown.status, Status::Unknown);
    venue.add_conflicting_class_transfer_with_same_nonce();
    drop(first);

    let restarted = engine(&path, venue.clone(), None).await;
    let still_unknown = restarted.status(&unknown.id, true).await.unwrap();
    assert_eq!(still_unknown.status, Status::Unknown, "{still_unknown:#?}");
    assert_eq!(still_unknown.submission, unknown.submission);
    assert!(still_unknown.effects.is_none());
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn zero_timestamp_cannot_prove_class_transfer_rejection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("class-transfer-zero-time.sqlite");
    let venue = Arc::new(Venue::default());
    venue.set_class_transfer_error(json!("Insufficient balance for token transfer"));
    venue.lose_next_response();
    let first = engine(&path, venue.clone(), None).await;
    let request_id = "class-transfer-zero-time";
    let unknown = first
        .execute(request(
            request_id,
            "hyperliquid.account.transfer",
            transfer_input(request_id, "perpetual"),
        ))
        .await
        .unwrap();
    assert_eq!(unknown.status, Status::Unknown);
    venue.zero_class_transfer_explorer_time();
    drop(first);

    let restarted = engine(&path, venue.clone(), None).await;
    let still_unknown = restarted.status(&unknown.id, true).await.unwrap();
    assert_eq!(still_unknown.status, Status::Unknown, "{still_unknown:#?}");
    assert!(still_unknown.effects.is_none());
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn withdrawal_waits_for_sequenced_arbitrum_proof_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("withdrawal.sqlite");
    let venue = Arc::new(Venue::default());
    let withdrawal_request = request(
        "withdrawal-proof",
        "hyperliquid.account.withdraw",
        json!({
            "request_id":"withdrawal-proof","account":"main","asset":"USDC",
            "amount":"1","address":DESTINATION,"max_amount_usdc":"1","max_fee_usdc":"0.01"
        }),
    );
    let without_proof = engine(&path, venue.clone(), None).await;
    let pending = without_proof
        .execute(withdrawal_request.clone())
        .await
        .unwrap();
    assert_eq!(pending.status, Status::Unknown, "{pending:#?}");
    assert!(pending.effects.is_none());
    let original_submission = pending.submission.clone();
    drop(without_proof);

    let with_proof = engine(
        &path,
        venue.clone(),
        Some(Arc::new(WithdrawalProof {
            wrong_recipient: false,
        })),
    )
    .await;
    let settled = with_proof.status(&pending.id, true).await.unwrap();
    assert_eq!(settled.status, Status::Completed, "{settled:#?}");
    let proof = settled.effects.as_ref().unwrap();
    assert_eq!(proof.submission, original_submission.unwrap());
    assert_eq!(
        proof.evidence["ledger_nonce"],
        settled.submission.as_ref().unwrap().data["envelope"]["nonce_ms"]
            .as_u64()
            .unwrap()
            * 1000
    );
    assert_ne!(
        proof.evidence["native_ledger"]["hash"],
        proof.evidence["explorer_action_hash"]
    );
    assert_eq!(proof.evidence["native_ledger"]["delta"]["usdc"], "0.99");
    assert_eq!(proof.evidence["native_ledger"]["delta"]["fee"], "0.01");
    assert!(proof.effects.iter().any(|effect| matches!(
        effect,
        fin_domain::FinancialEffect::Transfer { to, credit, .. }
            if to.0 == format!("ethereum:42161:{DESTINATION}") && credit.asset.decimals == 6
    )));
    assert!(proof.effects.iter().any(|effect| matches!(
        effect,
        fin_domain::FinancialEffect::Cost { category, .. }
            if category == "hyperliquid_withdrawal_fee"
    )));
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn withdrawal_recipient_mismatch_stays_unknown_without_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("withdrawal-wrong-recipient.sqlite");
    let venue = Arc::new(Venue::default());
    let engine = engine(
        &path,
        venue.clone(),
        Some(Arc::new(WithdrawalProof {
            wrong_recipient: true,
        })),
    )
    .await;
    let req = request(
        "withdrawal-wrong-recipient",
        "hyperliquid.account.withdraw",
        json!({
            "request_id":"withdrawal-wrong-recipient","account":"main","asset":"USDC",
            "amount":"1","address":DESTINATION,"max_amount_usdc":"1","max_fee_usdc":"0.01"
        }),
    );
    let unresolved = engine.execute(req.clone()).await.unwrap();
    assert_eq!(unresolved.status, Status::Unknown, "{unresolved:#?}");
    assert!(unresolved.effects.is_none());
    let duplicate = engine.execute(req).await.unwrap();
    assert_eq!(duplicate.id, unresolved.id);
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn isolated_margin_and_vault_actions_remain_unresolved_without_post_state_proof() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unresolved-account-actions.sqlite");
    let venue = Arc::new(Venue::default());
    let margin_engine = engine(&path, venue.clone(), None).await;

    let margin = request(
        "margin-add",
        "hyperliquid.account.add_margin",
        json!({"request_id":"margin-add","account":"main","symbol":"BTC/USDC:USDC","amount":"1","max_amount_usdc":"1","max_fee_usdc":"0.01"}),
    );
    let margin_result = margin_engine.execute(margin.clone()).await.unwrap();
    assert_eq!(margin_result.status, Status::Unknown, "{margin_result:#?}");
    assert!(margin_result.effects.is_none());
    let margin_again = margin_engine.execute(margin).await.unwrap();
    assert_eq!(margin_again.id, margin_result.id);
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);

    let vault_venue = Arc::new(Venue::default());
    let vault_engine = engine(&dir.path().join("vault.sqlite"), vault_venue.clone(), None).await;
    let vault = request(
        "vault-deposit",
        "hyperliquid.vault.deposit",
        json!({"request_id":"vault-deposit","account":"main","vault_address":BUILDER,"amount":"1","max_amount_usdc":"1","max_fee_usdc":"0.01"}),
    );
    let vault_result = vault_engine.execute(vault).await.unwrap();
    assert_eq!(vault_result.status, Status::Unknown, "{vault_result:#?}");
    assert!(vault_result.effects.is_none());
    assert_eq!(vault_venue.dispatches.load(Ordering::SeqCst), 1);

    let weight_venue = Arc::new(Venue::default());
    let weight_engine = engine(
        &dir.path().join("weight.sqlite"),
        weight_venue.clone(),
        None,
    )
    .await;
    let request_weight = request(
        "request-weight",
        "hyperliquid.account.reserve_request_weight",
        json!({"request_id":"request-weight","account":"main","weight":10}),
    );
    let weight_result = weight_engine.execute(request_weight).await.unwrap();
    assert_eq!(weight_result.status, Status::Unknown);
    assert!(weight_result.effects.is_none());
    assert_eq!(weight_venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn isolated_margin_completes_only_from_the_exact_owner_coin_and_collateral_transition() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("margin-exact.sqlite");
    let venue = Arc::new(Venue::default());
    venue.state.lock().unwrap().update_margin_state = true;
    let margin_engine = engine(&path, venue.clone(), None).await;
    let req = request(
        "margin-exact",
        "hyperliquid.account.add_margin",
        json!({"request_id":"margin-exact","account":"main","symbol":"BTC/USDC:USDC","amount":"1","max_amount_usdc":"1","max_fee_usdc":"0.01"}),
    );
    let completed = margin_engine.execute(req.clone()).await.unwrap();
    assert_eq!(completed.status, Status::Completed, "{completed:#?}");
    assert!(
        completed
            .effects
            .as_ref()
            .unwrap()
            .effects
            .iter()
            .any(|effect| {
                matches!(effect, fin_domain::FinancialEffect::OriginMovement { debit, credit, .. }
            if debit.atoms == Quantity::from_decimal("1", 36).unwrap()
                && credit.atoms == Quantity::from_decimal("1", 36).unwrap())
            })
    );
    assert_eq!(margin_engine.execute(req).await.unwrap().id, completed.id);
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);

    let wrong_owner_path = dir.path().join("margin-wrong-owner.sqlite");
    let wrong_owner_venue = Arc::new(Venue::default());
    wrong_owner_venue.state.lock().unwrap().margin_owner = CHILD.into();
    let wrong_owner_engine = engine(&wrong_owner_path, wrong_owner_venue.clone(), None).await;
    let wrong_owner = request(
        "margin-wrong-owner",
        "hyperliquid.account.add_margin",
        json!({"request_id":"margin-wrong-owner","account":"main","symbol":"BTC/USDC:USDC","amount":"1","max_amount_usdc":"1","max_fee_usdc":"0.01"}),
    );
    assert!(
        wrong_owner_engine
            .preview("hyperliquid.account.add_margin", &wrong_owner.input)
            .await
            .is_err()
    );
    assert_eq!(wrong_owner_venue.dispatches.load(Ordering::SeqCst), 0);

    let lost_path = dir.path().join("margin-response-loss.sqlite");
    let lost_venue = Arc::new(Venue::default());
    {
        let mut state = lost_venue.state.lock().unwrap();
        state.update_margin_state = true;
        state.lose_response = true;
    }
    let lost_engine = engine(&lost_path, lost_venue.clone(), None).await;
    let lost = request(
        "margin-response-loss",
        "hyperliquid.account.add_margin",
        json!({"request_id":"margin-response-loss","account":"main","symbol":"BTC/USDC:USDC","amount":"1","max_amount_usdc":"1","max_fee_usdc":"0.01"}),
    );
    let unknown = lost_engine.execute(lost.clone()).await.unwrap();
    assert_eq!(unknown.status, Status::Unknown, "{unknown:#?}");
    assert_eq!(lost_engine.execute(lost).await.unwrap().id, unknown.id);
    assert_eq!(lost_venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn subaccount_creation_uses_human_route_but_transfer_stays_pending_without_proof() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("subaccount.sqlite");
    let venue = Arc::new(Venue::default());
    let engine = engine(&path, venue.clone(), None).await;

    let config_request = request(
        "create-subaccount",
        "hyperliquid.account.create_subaccount",
        json!({"request_id":"create-subaccount","account":"main","name":"synthetic child"}),
    );
    assert!(engine.execute(config_request.clone()).await.is_err());
    let created = engine
        .execute_configuration(config_request, Arc::new(ConfigurationApproval))
        .await
        .unwrap();
    assert_eq!(created.status, Status::Completed);
    assert!(
        created
            .effects
            .as_ref()
            .unwrap()
            .effects
            .iter()
            .any(|effect| matches!(
                effect,
                fin_domain::FinancialEffect::AuthorityChange { authority, after, .. }
                    if authority == "subaccount_created" && after == "synthetic child"
            ))
    );
    assert_eq!(
        created.effects.as_ref().unwrap().evidence["child"],
        CHILD,
        "the human-authorized authority receipt must preserve the exact newly created child"
    );

    let transfer = request(
        "child-transfer",
        "hyperliquid.account.transfer",
        json!({"request_id":"child-transfer","account":"main","asset":"USDC","amount":"1","destination":"subaccount","subaccount_address":CHILD,"subaccount_direction":"to_subaccount","max_amount_usdc":"1","max_fee_usdc":"0.01"}),
    );
    let result = engine.execute(transfer.clone()).await.unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(result.effects.is_none());
    let duplicate = engine.execute(transfer).await.unwrap();
    assert_eq!(duplicate.id, result.id);
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn builder_referral_and_abstraction_require_separate_configuration_authorization() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("configuration-actions.sqlite");
    let venue = Arc::new(Venue::default());
    let engine = engine(&path, venue.clone(), None).await;

    let builder = request(
        "builder-fee",
        "hyperliquid.builder.approve_fee",
        json!({"request_id":"builder-fee","account":"main","builder_address":BUILDER,"fee_tenths_bps":10}),
    );
    assert!(engine.execute(builder.clone()).await.is_err());
    let builder_result = engine
        .execute_configuration(builder.clone(), Arc::new(ConfigurationApproval))
        .await
        .unwrap();
    assert_eq!(builder_result.status, Status::Completed);
    assert!(
        builder_result
            .effects
            .as_ref()
            .unwrap()
            .effects
            .iter()
            .any(|effect| matches!(
                effect,
                fin_domain::FinancialEffect::AuthorityChange { authority, after, .. }
                    if authority == &format!("builder_fee:{BUILDER}") && after == "10"
            ))
    );
    assert_eq!(
        engine
            .execute_configuration(builder, Arc::new(ConfigurationApproval))
            .await
            .unwrap()
            .id,
        builder_result.id
    );

    let abstraction = request(
        "set-abstraction",
        "hyperliquid.account.set_abstraction",
        json!({"request_id":"set-abstraction","account":"main","mode":"unified"}),
    );
    let abstraction_result = engine
        .execute_configuration(abstraction, Arc::new(ConfigurationApproval))
        .await
        .unwrap();
    assert_eq!(
        abstraction_result.status,
        Status::Completed,
        "{abstraction_result:#?}"
    );
    assert!(
        abstraction_result
            .effects
            .as_ref()
            .unwrap()
            .effects
            .iter()
            .any(|effect| matches!(
                effect,
                fin_domain::FinancialEffect::AuthorityChange { authority, after, .. }
                    if authority == "unified_account" && after == "unifiedAccount"
            ))
    );

    let referral = request(
        "set-referral",
        "hyperliquid.referral.set",
        json!({"request_id":"set-referral","account":"main","referral_code":"synthetic-code"}),
    );
    let referral_result = engine
        .execute_configuration(referral, Arc::new(ConfigurationApproval))
        .await
        .unwrap();
    assert_eq!(
        referral_result.status,
        Status::Completed,
        "{referral_result:#?}"
    );
    assert!(
        referral_result
            .effects
            .as_ref()
            .unwrap()
            .effects
            .iter()
            .any(|effect| matches!(
                effect,
                fin_domain::FinancialEffect::AuthorityChange { authority, after, .. }
                    if authority == "referral_code" && after == "synthetic-code"
            ))
    );

    let portfolio = request(
        "set-portfolio-margin",
        "hyperliquid.account.set_abstraction",
        json!({"request_id":"set-portfolio-margin","account":"main","mode":"portfolio_margin"}),
    );
    let portfolio_result = engine
        .execute_configuration(portfolio, Arc::new(ConfigurationApproval))
        .await
        .unwrap();
    assert_eq!(
        portfolio_result.status,
        Status::Completed,
        "{portfolio_result:#?}"
    );
    assert!(portfolio_result.effects.as_ref().unwrap().effects.iter().any(|effect| {
        matches!(effect, fin_domain::FinancialEffect::AuthorityChange { authority, after, .. }
            if authority == "unified_account" && after == "portfolioMargin")
    }));
    let leave_portfolio = request(
        "leave-portfolio-margin",
        "hyperliquid.account.set_abstraction",
        json!({"request_id":"leave-portfolio-margin","account":"main","mode":"unified"}),
    );
    let left_portfolio = engine
        .execute_configuration(leave_portfolio, Arc::new(ConfigurationApproval))
        .await
        .unwrap();
    assert_eq!(
        left_portfolio.status,
        Status::Completed,
        "{left_portfolio:#?}"
    );

    let agent = request(
        "set-agent-abstraction",
        "hyperliquid.account.set_agent_abstraction",
        json!({"request_id":"set-agent-abstraction","account":"main","mode":"unified"}),
    );
    assert!(engine.execute(agent.clone()).await.is_err());
    let agent_result = engine
        .execute_configuration(agent, Arc::new(ConfigurationApproval))
        .await
        .unwrap();
    assert_eq!(agent_result.status, Status::Completed, "{agent_result:#?}");
    assert!(agent_result.effects.as_ref().unwrap().effects.iter().any(|effect| {
        matches!(effect, fin_domain::FinancialEffect::AuthorityChange { authority, after, .. }
            if authority == "agent_abstraction" && after == "unifiedAccount")
    }));
    let dex_abstraction = request(
        "enable-dex-abstraction",
        "hyperliquid.account.set_dex_abstraction",
        json!({"request_id":"enable-dex-abstraction","account":"main","enabled":true}),
    );
    let dex_result = engine
        .execute_configuration(dex_abstraction, Arc::new(ConfigurationApproval))
        .await
        .unwrap();
    assert_eq!(dex_result.status, Status::Completed, "{dex_result:#?}");
    assert!(dex_result.effects.as_ref().unwrap().effects.iter().any(|effect| {
        matches!(effect, fin_domain::FinancialEffect::AuthorityChange { authority, before, after, .. }
            if authority == "dex_abstraction" && before == "null" && after == "true")
    }));
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 7);
}

#[tokio::test]
async fn leverage_requires_exact_owner_coin_mode_and_post_action_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("leverage.sqlite");
    let venue = Arc::new(Venue::default());
    let primary_engine = engine(&path, venue.clone(), None).await;
    let leverage_request = request(
        "set-leverage",
        "hyperliquid.account.set_leverage",
        json!({"request_id":"set-leverage","account":"main","symbol":"BTC/USDC:USDC","leverage":8,"cross":true}),
    );
    let completed = primary_engine
        .execute(leverage_request.clone())
        .await
        .unwrap();
    assert_eq!(completed.status, Status::Completed);
    assert!(
        completed
            .effects
            .as_ref()
            .unwrap()
            .effects
            .iter()
            .any(|effect| matches!(
                effect,
                fin_domain::FinancialEffect::AuthorityChange { authority, before, after, .. }
                    if authority == "leverage:BTC" && before == "isolated:5" && after == "cross:8"
            ))
    );
    assert_eq!(
        primary_engine.execute(leverage_request).await.unwrap().id,
        completed.id
    );
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);

    let dir = tempfile::tempdir().unwrap();
    let wrong_owner_path = dir.path().join("wrong-owner.sqlite");
    let wrong_owner_venue = Arc::new(Venue::default());
    wrong_owner_venue.state.lock().unwrap().leverage_owner = CHILD.into();
    let wrong_owner_engine = engine(&wrong_owner_path, wrong_owner_venue.clone(), None).await;
    let wrong_owner = request(
        "wrong-owner-leverage",
        "hyperliquid.account.set_leverage",
        json!({"request_id":"wrong-owner-leverage","account":"main","symbol":"BTC/USDC:USDC","leverage":8,"cross":true}),
    );
    let wrong_owner_result = wrong_owner_engine.execute(wrong_owner).await.unwrap();
    assert_eq!(wrong_owner_result.status, Status::Failed);
    assert_eq!(wrong_owner_venue.dispatches.load(Ordering::SeqCst), 0);

    let dir = tempfile::tempdir().unwrap();
    let mismatch_path = dir.path().join("wrong-post-state.sqlite");
    let mismatch_venue = Arc::new(Venue::default());
    mismatch_venue
        .state
        .lock()
        .unwrap()
        .mismatched_leverage_after_dispatch = true;
    let mismatch_engine = engine(&mismatch_path, mismatch_venue.clone(), None).await;
    let mismatch = request(
        "wrong-post-leverage",
        "hyperliquid.account.set_leverage",
        json!({"request_id":"wrong-post-leverage","account":"main","symbol":"BTC/USDC:USDC","leverage":8,"cross":true}),
    );
    let unresolved = mismatch_engine.execute(mismatch).await.unwrap();
    assert_eq!(unresolved.status, Status::Unknown);
    assert!(unresolved.effects.is_none());
    assert_eq!(mismatch_venue.dispatches.load(Ordering::SeqCst), 1);

    let dir = tempfile::tempdir().unwrap();
    let lost_path = dir.path().join("response-loss.sqlite");
    let lost_venue = Arc::new(Venue::default());
    lost_venue.lose_next_response();
    let lost_engine = engine(&lost_path, lost_venue.clone(), None).await;
    let lost_request = request(
        "lost-leverage-response",
        "hyperliquid.account.set_leverage",
        json!({"request_id":"lost-leverage-response","account":"main","symbol":"BTC/USDC:USDC","leverage":8,"cross":true}),
    );
    let lost = lost_engine.execute(lost_request.clone()).await.unwrap();
    assert_eq!(lost.status, Status::Unknown);
    let recovered = lost_engine.execute(lost_request).await.unwrap();
    assert_eq!(recovered.id, lost.id);
    assert_eq!(recovered.status, Status::Unknown);
    assert_eq!(lost_venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn hip3_usdc_leverage_and_isolated_margin_use_scoped_assets_and_resources() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hip3-scoped-margin.sqlite");
    let venue = Arc::new(Venue::default());
    {
        let mut state = venue.state.lock().unwrap();
        state.subaccounts = json!([{"master":OWNER,"subAccountUser":CHILD}]);
        state.leverage_owner = CHILD.into();
        state.leverage_coin = "xyz:BTC".into();
        state.margin_owner = CHILD.into();
        state.margin_coin = "xyz:BTC".into();
        state.update_margin_state = true;
    }
    let engine = engine_with_scope(
        &path,
        venue.clone(),
        None,
        HyperliquidExecutionScope::Subaccount {
            address: CHILD.into(),
        },
    )
    .await;

    let leverage = request(
        "hip3-leverage",
        "hyperliquid.account.set_leverage",
        json!({"request_id":"hip3-leverage","account":"main","symbol":"xyz:BTC/USDC:USDC","dex":"xyz","leverage":8,"cross":true}),
    );
    let leverage_result = engine.execute(leverage).await.unwrap();
    assert_eq!(
        leverage_result.status,
        Status::Completed,
        "{leverage_result:#?}"
    );
    let leverage_action = venue.last_action.lock().unwrap().clone().unwrap();
    assert_eq!(leverage_action.vault_address.as_deref(), Some(CHILD));
    assert!(matches!(
        leverage_action.action,
        HyperliquidAction::UpdateLeverage {
            asset: 100_000,
            leverage: 8,
            is_cross: true
        }
    ));
    assert!(leverage_result.effects.as_ref().unwrap().effects.iter().any(|effect| matches!(
        effect,
        fin_domain::FinancialEffect::AuthorityChange { resource, authority, .. }
            if resource.0 == format!("hyperliquid:mainnet:{CHILD}:perpetual:xyz") && authority == "leverage:xyz:BTC"
    )));

    let margin = request(
        "hip3-margin",
        "hyperliquid.account.add_margin",
        json!({"request_id":"hip3-margin","account":"main","symbol":"xyz:BTC/USDC:USDC","dex":"xyz","amount":"1","max_amount_usdc":"1","max_fee_usdc":"0.01"}),
    );
    let margin_result = engine.execute(margin).await.unwrap();
    assert_eq!(
        margin_result.status,
        Status::Completed,
        "{margin_result:#?}"
    );
    let margin_action = venue.last_action.lock().unwrap().clone().unwrap();
    assert_eq!(margin_action.vault_address.as_deref(), Some(CHILD));
    assert!(
        matches!(margin_action.action, HyperliquidAction::UpdateIsolatedMargin { asset: 100_000, ref amount_usdc } if amount_usdc == "1")
    );
    assert!(margin_result.effects.as_ref().unwrap().effects.iter().any(|effect| matches!(
        effect,
        fin_domain::FinancialEffect::OriginMovement { from, to, .. }
            if matches!(from, fin_domain::AccountingLocation::Cash { resource } if resource.0 == format!("hyperliquid:mainnet:{CHILD}:perpetual:xyz"))
                && matches!(to, fin_domain::AccountingLocation::Position { resource, position, .. } if resource.0 == format!("hyperliquid:mainnet:{CHILD}:perpetual:xyz") && position == "xyz:BTC/USDC:USDC:isolated-collateral")
    )));

    let non_usdc = request(
        "hip3-nonusdc-margin",
        "hyperliquid.account.add_margin",
        json!({"request_id":"hip3-nonusdc-margin","account":"main","symbol":"nonusd:BTC/ETH:ETH","dex":"nonusd","amount":"1","max_amount_usdc":"1","max_fee_usdc":"0.01"}),
    );
    let rejected = engine.execute(non_usdc).await.unwrap();
    assert_eq!(rejected.status, Status::Failed);
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn address_send_requires_nonce_and_two_sided_hash_linked_ledger_proof() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("address-send.sqlite");
    let venue = Arc::new(Venue::default());
    let engine = engine(&path, venue.clone(), None).await;
    let send = request(
        "address-send",
        "hyperliquid.account.transfer",
        address_transfer_input("address-send"),
    );
    let settled = engine.execute(send.clone()).await.unwrap();
    assert_eq!(settled.status, Status::Completed, "{settled:#?}");
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
    let submission = settled.submission.as_ref().unwrap();
    let nonce = submission.data["envelope"]["nonce_ms"].as_u64().unwrap();
    assert_eq!(submission.data["wire"]["action"]["type"], "usdSend");
    assert_eq!(submission.data["wire"]["action"]["time"], nonce);
    assert_eq!(
        submission.data["wire"]["action"]["destination"],
        DESTINATION
    );
    let proof = settled.effects.as_ref().unwrap();
    assert_eq!(
        proof.evidence["method"],
        "nonce-and-hash-correlated-two-sided-internal-transfer-ledger"
    );
    assert!(matches!(
        proof.effects.as_slice(),
        [fin_domain::FinancialEffect::Transfer { debit, credit, .. }]
            if debit.atoms == Quantity::from_decimal("1", 36).unwrap()
                && credit.atoms == Quantity::from_decimal("1", 36).unwrap()
    ));
    let duplicate = engine.execute(send).await.unwrap();
    assert_eq!(duplicate.id, settled.id);
    assert_eq!(duplicate.submission.as_ref(), settled.submission.as_ref());
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn address_send_response_loss_recovers_from_exact_proof_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("address-send-response-loss.sqlite");
    let venue = Arc::new(Venue::default());
    venue.lose_next_response();
    let first = engine(&path, venue.clone(), None).await;
    let send = request(
        "address-send-response-loss",
        "hyperliquid.account.transfer",
        address_transfer_input("address-send-response-loss"),
    );
    let unknown = first.execute(send.clone()).await.unwrap();
    assert_eq!(unknown.status, Status::Unknown, "{unknown:#?}");
    let submission = unknown.submission.clone().unwrap();
    let recovered = first.status(&unknown.id, true).await.unwrap();
    assert_eq!(recovered.status, Status::Completed, "{recovered:#?}");
    assert_eq!(recovered.submission.as_ref(), Some(&submission));
    drop(first);

    let restarted = engine(&path, venue.clone(), None).await;
    let status = restarted.status(&recovered.id, true).await.unwrap();
    assert_eq!(status.status, Status::Completed);
    assert_eq!(status.submission.as_ref(), Some(&submission));
    let duplicate = restarted.execute(send).await.unwrap();
    assert_eq!(duplicate.status, Status::Completed);
    assert_eq!(duplicate.submission.as_ref(), Some(&submission));
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn address_send_rejects_non_custodial_recipient_before_dispatch() {
    for role in ["missing", "agent", "vault", "unknown"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("address-send-{role}.sqlite"));
        let venue = Arc::new(Venue::default());
        venue.set_destination_role(role);
        let engine = engine(&path, venue.clone(), None).await;
        let send = request(
            &format!("address-send-{role}"),
            "hyperliquid.account.transfer",
            address_transfer_input(&format!("address-send-{role}")),
        );
        let rejected = engine.execute(send).await.unwrap();
        assert_eq!(
            rejected.status,
            Status::Failed,
            "role={role}: {rejected:#?}"
        );
        assert!(rejected.submission.is_none());
        assert_eq!(venue.dispatches.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn address_send_requires_exact_recipient_and_unique_source_and_destination_receipts() {
    for fixture in [
        TransferFixture::WrongRecipient,
        TransferFixture::WrongAmount,
        TransferFixture::WrongSourceHash,
        TransferFixture::DuplicateAction,
        TransferFixture::MissingRecipientRow,
        TransferFixture::FeeOverCeiling,
        TransferFixture::ActivationFee,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("address-send-{fixture:?}.sqlite"));
        let venue = Arc::new(Venue::default());
        venue.set_transfer_fixture(fixture);
        let engine = engine(&path, venue.clone(), None).await;
        let send = request(
            &format!("address-send-{fixture:?}"),
            "hyperliquid.account.transfer",
            address_transfer_input(&format!("address-send-{fixture:?}")),
        );
        let unresolved = engine.execute(send).await.unwrap();
        assert_eq!(
            unresolved.status,
            Status::Unknown,
            "{fixture:?}: {unresolved:#?}"
        );
        assert!(unresolved.effects.is_none());
        assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn address_send_accounts_for_proved_usdc_fee_within_approved_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("address-send-fee.sqlite");
    let venue = Arc::new(Venue::default());
    venue.set_transfer_fixture(TransferFixture::FeeWithinCeiling);
    let engine = engine(&path, venue.clone(), None).await;
    let send = request(
        "address-send-fee",
        "hyperliquid.account.transfer",
        address_transfer_input("address-send-fee"),
    );
    let settled = engine.execute(send).await.unwrap();
    assert_eq!(settled.status, Status::Completed, "{settled:#?}");
    let effects = &settled.effects.as_ref().unwrap().effects;
    assert!(matches!(
        effects.as_slice(),
        [
            fin_domain::FinancialEffect::Transfer { debit, credit, .. },
            fin_domain::FinancialEffect::Cost { amount, treatment: fin_domain::CostTreatment::AdditionalDebit, .. }
        ] if debit.atoms == Quantity::from_decimal("1", 36).unwrap()
            && credit.atoms == Quantity::from_decimal("1", 36).unwrap()
            && amount.atoms == Quantity::from_decimal("0.005", 36).unwrap()
    ));
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cash_actions_freeze_account_mode_and_reject_unsupported_or_non_master_scopes() {
    let dir = tempfile::tempdir().unwrap();

    let unified_path = dir.path().join("unified-margin.sqlite");
    let unified_venue = Arc::new(Venue::default());
    unified_venue.set_abstraction("unifiedAccount");
    unified_venue.state.lock().unwrap().update_margin_state = true;
    let unified_engine = engine(&unified_path, unified_venue.clone(), None).await;
    let margin = request(
        "unified-margin",
        "hyperliquid.account.add_margin",
        json!({"request_id":"unified-margin","account":"main","symbol":"BTC/USDC:USDC","amount":"1","max_amount_usdc":"1","max_fee_usdc":"0.01"}),
    );
    let settled = unified_engine.execute(margin).await.unwrap();
    assert_eq!(settled.status, Status::Completed, "{settled:#?}");
    assert!(
        settled
            .effects
            .as_ref()
            .unwrap()
            .effects
            .iter()
            .any(|effect| {
                matches!(effect, fin_domain::FinancialEffect::OriginMovement {
            from: fin_domain::AccountingLocation::Cash { resource }, ..
        } if resource.0.ends_with(":unified:primary"))
            })
    );

    let pm_path = dir.path().join("portfolio-margin.sqlite");
    let pm_venue = Arc::new(Venue::default());
    pm_venue.set_abstraction("portfolioMargin");
    let pm_engine = engine(&pm_path, pm_venue.clone(), None).await;
    let rejected = pm_engine
        .preview(
            "hyperliquid.account.add_margin",
            &json!({"request_id":"pm-margin","account":"main","symbol":"BTC/USDC:USDC","amount":"1","max_amount_usdc":"1","max_fee_usdc":"0.01"}),
        )
        .await;
    assert!(rejected.is_err());
    assert_eq!(pm_venue.dispatches.load(Ordering::SeqCst), 0);

    let agent_path = dir.path().join("agent-role.sqlite");
    let agent_venue = Arc::new(Venue::default());
    agent_venue.set_owner_role(json!({"role":"agent","data":{"master":OWNER}}));
    let agent_engine = engine(&agent_path, agent_venue.clone(), None).await;
    let rejected = agent_engine
        .preview(
            "hyperliquid.account.add_margin",
            &json!({"request_id":"agent-margin","account":"main","symbol":"BTC/USDC:USDC","amount":"1","max_amount_usdc":"1","max_fee_usdc":"0.01"}),
        )
        .await;
    assert!(rejected.is_err());
    assert_eq!(agent_venue.dispatches.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn request_weight_gift_binds_existing_recipient_and_charges_frozen_cash_wallet() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("request-weight-gift.sqlite");
    let venue = Arc::new(Venue::default());
    venue.set_abstraction("unifiedAccount");
    let weight_engine = engine(&path, venue.clone(), None).await;
    let input = json!({
        "request_id":"request-weight-gift",
        "account":"main",
        "weight":10,
        "destination":DESTINATION
    });
    let preview = weight_engine
        .preview("hyperliquid.account.reserve_request_weight", &input)
        .await
        .unwrap();
    assert!(preview.summary.contains(DESTINATION));
    assert!(preview.summary.contains("costing 0.005 USDC"));
    assert_eq!(
        preview.intent.maximum_costs[0].amount.atoms,
        Quantity::from_decimal("0.005", 36).unwrap()
    );
    assert!(matches!(
        &preview.intent.maximum_costs[0].location,
        fin_domain::AccountingLocation::Cash { resource }
            if resource.0.ends_with(":unified:primary")
    ));

    let result = weight_engine
        .execute(request(
            "request-weight-gift",
            "hyperliquid.account.reserve_request_weight",
            input,
        ))
        .await
        .unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
    assert!(matches!(
        venue.last_action.lock().unwrap().as_ref().unwrap().action,
        HyperliquidAction::ReserveRequestWeight {
            weight: 10,
            destination: Some(ref destination),
        } if destination == DESTINATION
    ));

    let rejected_path = dir.path().join("request-weight-missing-recipient.sqlite");
    let rejected_venue = Arc::new(Venue::default());
    rejected_venue.set_destination_role("missing");
    let rejected_engine = engine(&rejected_path, rejected_venue.clone(), None).await;
    let rejected = rejected_engine
        .preview(
            "hyperliquid.account.reserve_request_weight",
            &json!({"request_id":"request-weight-missing","account":"main","weight":10,"destination":DESTINATION}),
        )
        .await;
    assert!(rejected.is_err());
    assert_eq!(rejected_venue.dispatches.load(Ordering::SeqCst), 0);
}
