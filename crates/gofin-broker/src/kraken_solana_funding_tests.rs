//! Public CLI funding withdrawal rehearsal against synthetic Kraken and Solana
//! transports. All transfers, credentials, and settlement evidence are local.
use super::*;
use crate::test_support::{
    cli::{Client, args},
    solana::{SolanaFundingFixture, USDC_MINT},
};
use async_trait::async_trait;
use gofin_domain::{
    ApprovalAudit, AuthorizationChallenge, Authorizer, OperationRequest, Result as GoFinResult,
};
use gofin_domain::{AssetId, Quantity, ResourceId, Status};
use gofin_exchange::kraken::DispatchGuard;
use gofin_kraken::funding::{
    KrakenDestination, KrakenPayoutRequest, KrakenPayoutResult, KrakenRecipientVerifier,
};
use gofin_kraken::{
    KrakenAccount, KrakenEndpoint as E, KrakenProduct, KrakenProfile, KrakenResponse,
    KrakenTransport,
};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use gofin_solana::funding::{AmountBounds, ExternalPayoutRequest, ExternalPayoutStatus};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

const ACCOUNT: &str = "synthetic-kraken-spot";
const REQUEST: &str = "SYNTHETIC-KRAKEN-SOLANA-WITHDRAWAL";
const WITHDRAWAL_ID: &str = "synthetic-kraken-withdrawal-1";
const PENDING_SIGNATURE: &str = "1111111111111111111111111111111111111111111111111111111111111111";

pub(crate) struct SolanaWithdrawalVenue {
    fixture: SolanaFundingFixture,
    coin: String,
    amount: String,
    net: String,
    fee: String,
    address: String,
    signature: Mutex<String>,
    finalized: AtomicBool,
    auto_finalize: bool,
    writes: AtomicUsize,
    duplicate_ledger: AtomicBool,
}

impl SolanaWithdrawalVenue {
    pub(crate) fn signature(&self) -> String {
        self.signature.lock().unwrap().clone()
    }
    pub(crate) fn writes(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }
    pub(crate) fn finalize(&self) -> String {
        if self.finalized.swap(true, Ordering::SeqCst) {
            return self.signature();
        }
        let atoms = Quantity::from_decimal(&self.net, if self.coin == "SOL" { 9 } else { 6 })
            .unwrap()
            .as_str()
            .parse()
            .unwrap();
        let asset = if self.coin == "SOL" {
            "native"
        } else {
            USDC_MINT
        };
        let signature = self
            .fixture
            .inject_finalized_transfer(asset, &self.address, atoms)
            .unwrap();
        *self.signature.lock().unwrap() = signature.clone();
        signature
    }
    pub(crate) fn duplicate_ledger(&self, duplicate: bool) {
        self.duplicate_ledger.store(duplicate, Ordering::SeqCst);
    }
}

#[async_trait]
impl KrakenTransport for SolanaWithdrawalVenue {
    async fn request(
        &self,
        endpoint: E,
        _account: Option<&KrakenAccount>,
        parameters: Value,
        guard: Option<DispatchGuard>,
    ) -> GoFinResult<KrakenResponse> {
        let response = match endpoint {
            E::KrakenPrivatePostGetApiKeyInfo => {
                json!({"error":[],"result":{"permissions":["query-funds","withdraw-funds","query-ledger"]}})
            }
            E::KrakenPrivatePostBalance => json!({"error":[],"result":{self.coin.clone():"10"}}),
            E::KrakenPrivatePostBalanceEx => {
                json!({"error":[],"result":{self.coin.clone():{"balance":"10","hold_trade":"0"}}})
            }
            E::KrakenPublicGetAssets => {
                let decimals = if self.coin == "SOL" { 9 } else { 6 };
                json!({"error":[],"result":{self.coin.clone():{"altname":self.coin,"decimals":decimals}}})
            }
            E::KrakenPrivatePostWithdrawInfo => {
                json!({"error":[],"result":{"method":format!("{} on Solana",self.coin),"amount":self.net,"fee":self.fee,"limit":"100"}})
            }
            E::KrakenPrivatePostWithdrawAddresses => {
                json!({"error":[],"result":[{"key":parameters["key"],"method":format!("{} on Solana",self.coin),"verified":true,"address":self.address}]})
            }
            E::KrakenPrivatePostWithdrawMethods => {
                json!({"error":[],"result":[{"method":format!("{} on Solana",self.coin),"network":"SOL","contract_address":if self.coin=="USDC" {Value::String(USDC_MINT.into())} else {Value::Null},"minimum":"0.000001"}]})
            }
            E::KrakenPrivatePostWithdraw => {
                let guard = guard.ok_or_else(|| {
                    GoFinError::validation("Synthetic write lacked dispatch authority.")
                })?;
                if !guard() {
                    return Err(GoFinError::validation(
                        "Synthetic dispatch authority expired.",
                    ));
                }
                if parameters["asset"] != self.coin || parameters["amount"] != self.amount {
                    return Err(GoFinError::validation(
                        "Synthetic withdrawal differs from its frozen asset or amount.",
                    ));
                }
                self.writes.fetch_add(1, Ordering::SeqCst);
                if self.auto_finalize {
                    self.finalize();
                }
                json!({"error":[],"result":{"refid":WITHDRAWAL_ID}})
            }
            E::KrakenPrivatePostWithdrawStatus => {
                json!({"error":[],"result":[{"refid":WITHDRAWAL_ID,"status":"Settled","asset":self.coin,"method":format!("{} on Solana",self.coin),"key":format!("{}-key",self.coin),"info":self.address,"amount":self.net,"fee":self.fee,"txid":self.signature()}]})
            }
            E::KrakenPrivatePostLedgers => {
                let row = json!({"refid":WITHDRAWAL_ID,"type":"withdrawal","asset":self.coin,"amount":format!("-{}",self.net),"fee":self.fee,"balance":"9"});
                let mut rows = json!({"synthetic-ledger-id":row});
                if self.duplicate_ledger.load(Ordering::SeqCst) {
                    rows["duplicate-ledger-id"] = row;
                }
                json!({"error":[],"result":{"ledger":rows}})
            }
            E::KrakenPublicGetAssetPairs => json!({"error":[],"result":{}}),
            other => {
                return Err(GoFinError::validation(format!(
                    "Unexpected synthetic Kraken endpoint: {other:?}"
                )));
            }
        };
        Ok(KrakenResponse::Json(response))
    }
}

struct SolanaRecipient {
    fixture: SolanaFundingFixture,
    coin: String,
}
#[async_trait]
impl KrakenRecipientVerifier for SolanaRecipient {
    async fn resolve(
        &self,
        network: &str,
        coin: &str,
        contract: Option<&str>,
        address: &str,
        _destination_name: Option<&str>,
    ) -> GoFinResult<Option<KrakenDestination>> {
        if network != "SOL" || coin != self.coin || address != self.fixture.signer_address() {
            return Ok(None);
        }
        let mint = if coin == "SOL" { None } else { Some(USDC_MINT) };
        if contract.filter(|s| !s.is_empty()) != mint {
            return Ok(None);
        }
        let asset = if coin == "SOL" {
            AssetId {
                network: gofin_solana::NETWORK.into(),
                reference: "native".into(),
                decimals: 9,
            }
        } else {
            gofin_solana::funding::resolve_external_asset(
                &self.fixture.wallet().runtime(),
                coin,
                contract,
            )
            .await?
        };
        Ok(Some(KrakenDestination {
            enrolled: true,
            resource: ResourceId(format!("solana:mainnet-beta:{address}")),
            address: address.into(),
            chain_id: 0,
            network: Some(gofin_solana::NETWORK.into()),
            cluster: Some(gofin_solana::CLUSTER.into()),
            mint: mint.map(str::to_owned),
            asset,
            confirmations: 1,
        }))
    }
    async fn verify(&self, request: KrakenPayoutRequest) -> GoFinResult<KrakenPayoutResult> {
        let runtime = self.fixture.wallet().runtime();
        let min = Quantity::from_decimal(&request.minimum, request.destination.asset.decimals)?
            .as_str()
            .parse::<u64>()
            .map_err(|_| GoFinError::validation("bad synthetic amount"))?;
        let max = Quantity::from_decimal(&request.maximum, request.destination.asset.decimals)?
            .as_str()
            .parse::<u64>()
            .map_err(|_| GoFinError::validation("bad synthetic amount"))?;
        let proof = ExternalPayoutRequest {
            signature: request.tx_hash,
            asset: request.destination.asset.clone(),
            recipient: request.destination.address,
            recipient_owner: None,
            source_owner: None,
            amount: AmountBounds {
                minimum_atoms: min,
                maximum_atoms: max,
            },
        };
        match runtime.verify_external_payout(&proof).await? {
            ExternalPayoutStatus::Pending { reason } => Ok(KrakenPayoutResult {
                status: Status::Pending,
                amount: None,
                evidence: json!({"state":"pending","reason":reason}),
            }),
            ExternalPayoutStatus::Finalized { evidence } => Ok(KrakenPayoutResult {
                status: Status::Completed,
                amount: Some(Quantity::from_u64(evidence.credited_atoms)),
                evidence: json!({"signature":evidence.signature,"slot":evidence.slot,"asset":evidence.asset,"recipient":evidence.recipient,"creditedAtoms":evidence.credited_atoms,"accounts":evidence.recipient_accounts}),
            }),
        }
    }
}

pub(crate) fn signed_solana_withdrawal_profile(
    coin: &str,
    auto_finalize: bool,
) -> (
    Arc<KrakenProfile>,
    Arc<SolanaWithdrawalVenue>,
    SolanaFundingFixture,
) {
    let fixture = SolanaFundingFixture::new("synthetic-solana-recipient", 0, 0);
    let address = fixture.signer_address();
    let (amount, net, fee) = if coin == "SOL" {
        ("0.1", "0.099", "0.001")
    } else {
        ("1", "1", "0")
    };
    let venue = Arc::new(SolanaWithdrawalVenue {
        fixture: fixture.clone(),
        coin: coin.into(),
        amount: amount.into(),
        net: net.into(),
        fee: fee.into(),
        address,
        signature: Mutex::new(PENDING_SIGNATURE.into()),
        finalized: AtomicBool::new(false),
        auto_finalize,
        writes: AtomicUsize::new(0),
        duplicate_ledger: AtomicBool::new(false),
    });
    let account = KrakenAccount {
        name: ACCOUNT.into(),
        resource: ResourceId(format!("kraken:{ACCOUNT}")),
        credential_ref: "synthetic-key-reference".into(),
        physical_identity: "synthetic-kraken-uid".into(),
        wallet_account_id: None,
        default_wallet_account_id: None,
        identity_verified: true,
        product: KrakenProduct::Spot,
    };
    let recipient: Arc<dyn KrakenRecipientVerifier> = Arc::new(SolanaRecipient {
        fixture: fixture.clone(),
        coin: coin.into(),
    });
    let profile = Arc::new(
        KrakenProfile::new(KrakenProduct::Spot, vec![account], venue.clone())
            .unwrap()
            .with_prices(Arc::new(Prices))
            .with_recipient_verifier(recipient),
    );
    (profile, venue, fixture)
}

struct Approval(AtomicUsize);
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> GoFinResult<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            source: Some(gofin_domain::AuthorizationSource::Development),
            yolo_provenance: None,
            mechanism: "synthetic CLI test approval".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}
struct Prices;
#[async_trait]
impl UsdPriceSource for Prices {
    async fn quote(&self, asset: &AssetId) -> GoFinResult<UsdQuote> {
        Ok(UsdQuote {
            usd_per_unit: if asset.reference == "native" {
                "100"
            } else {
                "1"
            }
            .into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic-solana-funding".into(),
        })
    }
}

fn runtime(
    path: &std::path::Path,
    profile: Arc<KrakenProfile>,
    approval: Arc<Approval>,
) -> Arc<BrokerRuntime> {
    let mut policy = BrokerPolicy::beta();
    policy.max_movement_usd_micros = "100000000000".into();
    policy.max_run_allocation_usd_micros = "100000000000".into();
    compose_runtime(
        BrokerConfigV2 {
            version: 2,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy,
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        RuntimeDependencies {
            store: gofin_store::Store::open(path).unwrap(),
            adapters: vec![Arc::new(gofin_exchange::ExchangeAdapter::new(
                (*profile).clone(),
            ))],
            authorizer: approval,
            valuator: Arc::new(SharedUsdValuator::with_source(Arc::new(Prices))),
            bridge: None,
        },
    )
    .unwrap()
}

pub(crate) fn request(coin: &str) -> Value {
    json!({"requestId":REQUEST,"account":ACCOUNT,"asset":coin,"key":format!("{coin}-key"),"amount":if coin=="SOL" {"0.1"} else {"1"},"max_fee":if coin=="SOL" {"0.001"} else {"0"}})
}

#[test]
fn solana_funding_kraken_withdrawal_cli_requires_finalized_exact_recipient_and_recovers_original() {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(rehearse("SOL"))
        })
        .unwrap()
        .join()
        .unwrap();
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(rehearse("USDC"))
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn rehearse(coin: &str) {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join(format!("kraken-solana-{coin}.sqlite"));
    let (profile, venue, fixture) = signed_solana_withdrawal_profile(coin, false);
    let approval = Arc::new(Approval(AtomicUsize::new(0)));
    let make_runtime = || runtime(&db, profile.clone(), approval.clone());
    let cli = Client::new(make_runtime(), temp.path()).await;
    let input = request(coin);
    let preview = cli
        .call(
            args(&[
                "preview",
                "kraken.withdraw-funds",
                "--input",
                &input.to_string(),
                "--json",
            ]),
            BrokerRequest::Preview {
                operation: "kraken.withdraw-funds".into(),
                input: input.clone(),
            },
            true,
        )
        .await;
    assert!(
        preview["summary"].as_str().unwrap().contains("on SOL"),
        "{preview}"
    );
    let executed = cli
        .call(
            args(&[
                "execute",
                "kraken.withdraw-funds",
                "--input",
                &input.to_string(),
                "--id",
                REQUEST,
                "--json",
            ]),
            BrokerRequest::Execute {
                request: OperationRequest {
                    request_id: REQUEST.into(),
                    operation: "kraken.withdraw-funds".into(),
                    input: input.clone(),
                    run_id: None,
                },
            },
            true,
        )
        .await;
    assert_eq!(
        executed["status"], "pending",
        "missing finalized Solana history must remain pending: {executed}"
    );
    assert_eq!(venue.writes(), 1);
    assert_eq!(
        executed["dispatch_evidence"]["response"]["result"]["refid"], WITHDRAWAL_ID,
        "the durable venue reference comes from the original dispatch receipt: {executed}"
    );
    let id = executed["id"].as_str().unwrap().to_owned();
    let finalized_signature = venue.finalize();
    drop(cli);
    let restarted = Client::new(make_runtime(), temp.path()).await;
    venue.duplicate_ledger(true);
    let ambiguous = restarted
        .call(
            args(&["status", &id, "--json"]),
            BrokerRequest::Status { id: id.clone() },
            true,
        )
        .await;
    assert_eq!(
        ambiguous["status"], "unknown",
        "duplicate correlated ledger evidence must remain ambiguous: {ambiguous}"
    );
    venue.duplicate_ledger(false);
    let before_restart = restarted
        .call(
            args(&["status", &id, "--json"]),
            BrokerRequest::Status { id: id.clone() },
            true,
        )
        .await;
    assert_eq!(before_restart["status"], "completed", "{before_restart}");
    let proof =
        &before_restart["last_reconciliation_evidence"]["verification"]["destinationReceipt"];
    assert_eq!(proof["signature"], finalized_signature);
    let submission = executed["submission"].clone();
    let replay = restarted
        .call(
            args(&[
                "execute",
                "kraken.withdraw-funds",
                "--input",
                &input.to_string(),
                "--id",
                REQUEST,
                "--json",
            ]),
            BrokerRequest::Execute {
                request: OperationRequest {
                    request_id: REQUEST.into(),
                    operation: "kraken.withdraw-funds".into(),
                    input,
                    run_id: None,
                },
            },
            true,
        )
        .await;
    assert_eq!(replay["id"], id);
    assert_eq!(replay["submission"], submission);
    assert_eq!(replay["status"], "completed");
    assert_eq!(
        venue.writes(),
        1,
        "restart must never replay the Kraken write"
    );
    assert_eq!(
        fixture.rpc().submission_count(),
        1,
        "one synthetic external payout signature is finalized"
    );
    assert_eq!(
        fixture.rpc().send_count(),
        0,
        "receipt observation never broadcasts"
    );
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
}
