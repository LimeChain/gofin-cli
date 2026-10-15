//! Binance Solana funding through the public broker/CLI transport and the real
//! Binance and Solana adapters. All venue and RPC observations are synthetic.
use super::*;
use async_trait::async_trait;
use gofin_binance::{
    AccountRole, BinanceAccount, BinanceAdapter, BinanceConfig, BinanceEndpoint,
    BinancePriceProvider, BinanceRequest, BinanceTransport, CredentialHandle, EthereumDestination,
    PriceQuote, TransportError,
};
use gofin_domain::financial_plans::{
    FinancialPlanAuthorizationRequest, FinancialPlanBounds, FinancialPlanFundingDependency,
    FinancialPlanFundingIdentity, FinancialPlanFundingScope, FinancialPlanSpec,
    FinancialPlanStepSpec, FinancialStepCompletion,
};
use gofin_domain::{
    Amount, ApprovalAudit, AssetId, AssetPrice, AuthorizationChallenge, Authorizer,
    FinancialIntent, OperationRequest, Quantity, ResourceAmount, ResourceId, now,
};
use gofin_protocol::BrokerRequest;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use crate::test_support::{
    cli::{Client, args},
    solana::{SolanaFundingFixture, USDC_MINT},
};

const ACCOUNT: &str = "synthetic-master";
const ACCOUNT_RESOURCE: &str = "binance:uid:271828:SPOT";

#[derive(Default)]
struct VenueState {
    sol_free: String,
    usdc_free: String,
    history: Vec<Value>,
    withdrawal_history: Vec<Value>,
}
struct Venue(Mutex<VenueState>, Mutex<Option<SolanaFundingFixture>>);
impl Venue {
    fn new(fixture: SolanaFundingFixture) -> Self {
        Self(
            Mutex::new(VenueState {
                sol_free: "0".into(),
                usdc_free: "0".into(),
                history: vec![],
                withdrawal_history: vec![],
            }),
            Mutex::new(Some(fixture)),
        )
    }
    fn set_free(&self, coin: &str, free: &str) {
        let mut state = self.0.lock().unwrap();
        if coin == "SOL" {
            state.sol_free = free.into();
        } else {
            state.usdc_free = free.into();
        }
    }
    fn settle(&self, coin: &str, amount: &str, signature: &str, address: &str) {
        let mut state = self.0.lock().unwrap();
        let free = if coin == "SOL" {
            &mut state.sol_free
        } else {
            &mut state.usdc_free
        };
        *free = amount.to_owned();
        state
            .history
            .push(Self::credit_row(coin, amount, signature, address));
    }
    fn record_credit(&self, coin: &str, amount: &str, signature: &str, address: &str) {
        self.0
            .lock()
            .unwrap()
            .history
            .push(Self::credit_row(coin, amount, signature, address));
    }
    fn credit_row(coin: &str, amount: &str, signature: &str, address: &str) -> Value {
        json!({
            "id":"synthetic-credit", "coin":coin, "amount":amount,
            "transactionFee":"0", "network":"SOL", "address":address,
            "addressTag":"", "txId":signature, "status":6,
            "insertTime":chrono::Utc::now().timestamp_millis(), "confirmTimes":"32/32"
        })
    }
}
#[async_trait]
impl BinanceTransport for Venue {
    async fn request(
        &self,
        _: &CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, TransportError> {
        let state = self.0.lock().unwrap();
        Ok(match request.endpoint {
            BinanceEndpoint::Account => json!({"uid":271828,"accountType":"SPOT","balances":[
                {"asset":"SOL","free":state.sol_free,"locked":"0"},
                {"asset":"USDC","free":state.usdc_free,"locked":"0"}
            ]}),
            BinanceEndpoint::CapitalConfig => json!([
                {"coin":"SOL","depositAllEnable":true,"withdrawAllEnable":true,"networkList":[
                    {"network":"SOL","coin":"SOL","depositEnable":true,"withdrawEnable":true,"depositDust":"0.001","withdrawIntegerMultiple":"0.000000001","withdrawFee":"0.000005","withdrawMin":"0.001","withdrawMax":"1000000","denomination":1}
                ]},
                {"coin":"USDC","depositAllEnable":true,"withdrawAllEnable":true,"networkList":[
                    {"network":"SOL","coin":"USDC","depositEnable":true,"withdrawEnable":true,"depositDust":"0.1","withdrawIntegerMultiple":"0.000001","withdrawFee":"0.1","withdrawMin":"0.1","withdrawMax":"1000000","denomination":1,"contractAddress":USDC_MINT}
                ]}
            ]),
            BinanceEndpoint::DepositAddress => {
                let network = request
                    .params
                    .iter()
                    .find(|v| v.key == "network")
                    .map(|v| v.value.as_str())
                    .unwrap_or("SOL");
                let coin = request
                    .params
                    .iter()
                    .find(|v| v.key == "coin")
                    .map(|v| v.value.as_str())
                    .unwrap_or("SOL");
                json!({"coin":coin,"network":network,"address":exchange_address(),"memo":"","minimumDeposit":if coin=="SOL" {"0.001"} else {"0.1"},"denomination":1})
            }
            BinanceEndpoint::DepositHistory => json!(state.history),
            BinanceEndpoint::ApiRestrictions => json!({"enableWithdrawals":true}),
            BinanceEndpoint::TravelRuleRequirements => json!({"questionnaireCountryCode":"NIL"}),
            BinanceEndpoint::WithdrawAddressList => json!([]),
            BinanceEndpoint::WithdrawalHistory => json!(state.withdrawal_history),
            BinanceEndpoint::StandardWithdrawal => {
                drop(state);
                let params = request
                    .params
                    .iter()
                    .map(|p| (p.key.as_str(), p.value.as_str()))
                    .collect::<BTreeMap<_, _>>();
                let coin = *params.get("coin").ok_or(TransportError::InvalidResponse)?;
                let gross = *params
                    .get("amount")
                    .ok_or(TransportError::InvalidResponse)?;
                let network = *params
                    .get("network")
                    .ok_or(TransportError::InvalidResponse)?;
                let address = *params
                    .get("address")
                    .ok_or(TransportError::InvalidResponse)?;
                let order = *params
                    .get("withdrawOrderId")
                    .ok_or(TransportError::InvalidResponse)?;
                let (decimals, fee, asset) = if coin == "SOL" {
                    (9, "0.000005", "native")
                } else {
                    (6, "0.1", USDC_MINT)
                };
                let gross_atoms = Quantity::from_decimal(gross, decimals)
                    .map_err(|_| TransportError::InvalidResponse)?;
                let fee_atoms = Quantity::from_decimal(fee, decimals)
                    .map_err(|_| TransportError::InvalidResponse)?;
                let net = gross_atoms
                    .checked_sub(&fee_atoms)
                    .map_err(|_| TransportError::Rejected)?
                    .to_decimal(decimals);
                let amount_atoms = Quantity::from_decimal(&net, decimals)
                    .map_err(|_| TransportError::InvalidResponse)?
                    .as_str()
                    .parse::<u64>()
                    .map_err(|_| TransportError::InvalidResponse)?;
                let fixture = self
                    .1
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or(TransportError::InvalidResponse)?;
                let signature = fixture
                    .inject_finalized_transfer(asset, address, amount_atoms)
                    .map_err(|_| TransportError::InvalidResponse)?;
                let mut state = self.0.lock().unwrap();
                let free = if coin == "SOL" {
                    &mut state.sol_free
                } else {
                    &mut state.usdc_free
                };
                let balance_atoms = Quantity::from_decimal(free, decimals)
                    .map_err(|_| TransportError::InvalidResponse)?;
                *free = balance_atoms
                    .checked_sub(&gross_atoms)
                    .map_err(|_| TransportError::Rejected)?
                    .to_decimal(decimals);
                state.withdrawal_history.push(json!({"id":"synthetic-withdrawal","withdrawOrderId":order,"coin":coin,"network":network,"address":address,"addressTag":"","amount":net,"transactionFee":fee,"status":6,"txId":signature,"transferType":0,"walletType":0,"completeTime":chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()}));
                return Ok(json!({"id":"synthetic-withdrawal","withdrawOrderId":order}));
            }
            _ => return Err(TransportError::InvalidResponse),
        })
    }
}

fn exchange_address() -> String {
    solana_pubkey::Pubkey::new_from_array([7; 32]).to_string()
}

struct Allow(AtomicUsize);
#[async_trait]
impl Authorizer for Allow {
    async fn authorize(
        &self,
        challenge: &AuthorizationChallenge,
    ) -> gofin_domain::Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "synthetic test approval".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}
struct Prices;
struct BinancePrices;
#[async_trait]
impl BinancePriceProvider for BinancePrices {
    async fn quote_usd(&self, coin: &str) -> gofin_domain::Result<PriceQuote> {
        Ok(PriceQuote {
            usd_per_unit: if coin == "SOL" { "20" } else { "1" }.into(),
            observed_at: now(),
            source: "synthetic SOL=$20, USDC=$1".into(),
        })
    }
}
#[async_trait]
impl AssetValuator for Prices {
    async fn usd_micros(&self, amount: &Amount) -> gofin_domain::Result<(String, String, String)> {
        let price = if amount.asset.reference == "native" || amount.asset.reference == "SOL" {
            20_000_000
        } else {
            1_000_000
        };
        let unit = Quantity::from_decimal("1", amount.asset.decimals)?;
        let value = amount
            .atoms
            .mul_div_floor(&Quantity::from_u64(price), &unit)?;
        Ok((
            value.to_string(),
            now(),
            "synthetic SOL=$20, USDC=$1".into(),
        ))
    }
    async fn asset_price(&self, asset: &AssetId) -> gofin_domain::Result<AssetPrice> {
        let unit_price = if asset.reference == "native" || asset.reference == "SOL" {
            20_000_000
        } else {
            1_000_000
        };
        Ok(AssetPrice {
            asset: asset.clone(),
            usd_micros_per_unit: Quantity::from_u64(unit_price),
            unit_atoms: Quantity::from_decimal("1", asset.decimals)?,
            observed_at: now(),
            source: "synthetic fixed price".into(),
        })
    }
}

fn runtime(
    path: &std::path::Path,
    venue: Arc<Venue>,
    fixture: &SolanaFundingFixture,
    approval: Arc<Allow>,
) -> Arc<BrokerRuntime> {
    runtime_and_adapters(path, venue, fixture, approval).0
}

fn runtime_and_adapters(
    path: &std::path::Path,
    venue: Arc<Venue>,
    fixture: &SolanaFundingFixture,
    approval: Arc<Allow>,
) -> (
    Arc<BrokerRuntime>,
    Arc<BinanceAdapter>,
    Arc<crate::BinanceEthereumDepositAdapter>,
) {
    let wallet_address = fixture.signer_address();
    let destination = EthereumDestination {
        name: "synthetic-solana".into(),
        resource: ResourceId(format!("solana:mainnet-beta:{wallet_address}")),
        address: wallet_address.clone(),
        chain_id: 0,
        assets: BTreeMap::new(),
        confirmations: 1,
    };
    let verifier: Arc<dyn gofin_binance::BinanceRecipientVerifier> = Arc::new(
        crate::solana_cex::SolanaCexPayoutVerifier::new(
            None,
            BTreeMap::from([("synthetic-solana".into(), fixture.wallet())]),
        )
        .unwrap(),
    );
    let credential = CredentialHandle::new("synthetic-binance").unwrap();
    let binance = Arc::new(
        BinanceAdapter::new(
            BinanceConfig {
                accounts: vec![BinanceAccount {
                    name: ACCOUNT.into(),
                    resource: ResourceId(ACCOUNT_RESOURCE.into()),
                    credential,
                    role: AccountRole::Main,
                    parent: None,
                    email: None,
                    wallet_type: "SPOT".into(),
                }],
                destinations: vec![destination],
            },
            venue,
            Some(Arc::new(BinancePrices)),
            Some(verifier),
        )
        .unwrap(),
    );
    let solana = fixture.wallet();
    let deposit = Arc::new(
        crate::BinanceEthereumDepositAdapter::with_networks(binance.clone(), Vec::new())
            .with_solana_wallets([("synthetic-solana".into(), solana.clone())]),
    );
    let runtime = compose_runtime(
        BrokerConfigV2 {
            version: 2,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![ConnectionConfig::Solana {
                name: "synthetic-solana".into(),
                resource: ResourceId(format!("solana:mainnet-beta:{wallet_address}")),
                address: wallet_address,
                credential_ref: Some("550e8400-e29b-41d4-a716-446655440099".into()),
                signing_enabled: true,
                network: "solana:mainnet-beta".into(),
            }],
            policy: BrokerPolicy::beta(),
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        RuntimeDependencies {
            store: gofin_store::Store::open(path).unwrap(),
            adapters: vec![binance.clone(), deposit.clone(), solana],
            authorizer: approval,
            valuator: Arc::new(Prices),
            bridge: None,
        },
    )
    .unwrap();
    (runtime, binance, deposit)
}

fn deposit_input(id: &str, coin: &str, amount: &str) -> Value {
    json!({"requestId":id,"account":ACCOUNT,"wallet":"synthetic-solana","coin":coin,
        "network":"SOL","amount":amount,"maxFeeSol":"0.001",
        "maxAccountFundingSol":"0.01","token":if coin=="USDC" {USDC_MINT} else {"native"}})
}

fn withdrawal_input(id: &str, coin: &str, amount: &str, fee: &str, minimum: &str) -> Value {
    json!({"requestId":id,"account":ACCOUNT,"destination":"synthetic-solana",
        "coin":coin,"network":"SOL","walletType":"SPOT","amount":amount,
        "maxFee":fee,"minReceiveAmount":minimum})
}

#[test]
fn binance_deposit_rpc_scope_keeps_solana_out_of_evm_deadline_policy() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = SolanaFundingFixture::new("synthetic-solana", 10_000_000_000, 5_000_000);
    let venue = Arc::new(Venue::new(fixture.clone()));
    let approval = Arc::new(Allow(AtomicUsize::new(0)));
    let (_, _, adapter) =
        runtime_and_adapters(&temp.path().join("scope.sqlite"), venue, &fixture, approval);
    for coin in ["SOL", "USDC"] {
        assert!(!adapter.uses_rpc_reads("binance.deposit", &deposit_input("scope", coin, "0.01")));
    }
    assert!(adapter.uses_rpc_reads(
        "binance.deposit",
        &json!({
            "requestId":"scope-evm", "account":ACCOUNT, "wallet":"evm-wallet", "amount":"0.01", "maxFeeEth":"0.001",
        })
    ));
    assert!(!adapter.uses_rpc_reads("binance.deposit", &json!({})));
}

#[tokio::test]
async fn solana_funding_cli_binance_withdrawals_verify_native_and_exact_usdc_payouts() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("binance-solana-withdraw.sqlite");
    let fixture = SolanaFundingFixture::new("synthetic-solana", 10_000_000_000, 5_000_000);
    let venue = Arc::new(Venue::new(fixture.clone()));
    let approval = Arc::new(Allow(AtomicUsize::new(0)));
    venue.set_free("SOL", "1");
    venue.set_free("USDC", "2");
    let client = Client::new(runtime(&db, venue.clone(), &fixture, approval), temp.path()).await;
    for (id, coin, amount, fee, minimum) in [
        (
            "SYNTHETIC-SOL-WITHDRAW",
            "SOL",
            "0.05",
            "0.00001",
            "0.04999",
        ),
        ("SYNTHETIC-USDC-WITHDRAW", "USDC", "0.5", "0.1", "0.39"),
    ] {
        let prior_dispatches = venue.0.lock().unwrap().withdrawal_history.len();
        let input = withdrawal_input(id, coin, amount, fee, minimum);
        let preview = client
            .call(
                args(&[
                    "preview",
                    "binance.withdraw",
                    "--input",
                    &input.to_string(),
                    "--json",
                ]),
                BrokerRequest::Preview {
                    operation: "binance.withdraw".into(),
                    input: input.clone(),
                },
                true,
            )
            .await;
        assert!(preview["summary"].as_str().unwrap().contains("SOL"));
        let result = client
            .call(
                args(&[
                    "execute",
                    "binance.withdraw",
                    "--input",
                    &input.to_string(),
                    "--id",
                    id,
                    "--json",
                ]),
                BrokerRequest::Execute {
                    request: OperationRequest {
                        request_id: id.into(),
                        operation: "binance.withdraw".into(),
                        input,
                        run_id: None,
                    },
                },
                true,
            )
            .await;
        assert_eq!(result["status"], "completed", "{result}");
        assert_eq!(
            venue.0.lock().unwrap().withdrawal_history.len(),
            prior_dispatches + 1
        );
        assert!(
            result["effects"]
                .to_string()
                .contains("solana:mainnet-beta")
        );
    }
}

#[tokio::test]
async fn solana_funding_named_plan_withdraws_usdc_then_funds_binance_deposit() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("binance-solana-future-funding.sqlite");
    let fixture = SolanaFundingFixture::new("synthetic-solana", 5_000_000_000, 0);
    fixture.register_token_owner(&exchange_address());
    let venue = Arc::new(Venue::new(fixture.clone()));
    venue.set_free("USDC", "5");
    let approval = Arc::new(Allow(AtomicUsize::new(0)));
    // Obtain exact reviewed scopes from the production adapters with sufficient
    // synthetic balance, then remove that token balance before the plan is run.
    // The approved funding edge must supply it from the preceding Binance payout.
    fixture.set_balance(5_000_000_000, 1_000_000);
    let withdraw_input =
        withdrawal_input("SYNTHETIC-PLAN-USDC-WITHDRAW", "USDC", "0.6", "0.1", "0.5");
    let deposit = deposit_input("SYNTHETIC-PLAN-USDC-DEPOSIT", "USDC", "0.5");
    let (_, binance, deposit_adapter) =
        runtime_and_adapters(&db, venue.clone(), &fixture, approval.clone());
    let withdraw_prepared = binance
        .prepare("binance.withdraw", &withdraw_input)
        .await
        .unwrap();
    let deposit_prepared = deposit_adapter
        .prepare("binance.deposit", &deposit)
        .await
        .unwrap();
    fixture.set_balance(5_000_000_000, 0);

    let wallet_resource = ResourceId(format!("solana:mainnet-beta:{}", fixture.signer_address()));
    let sol_usdc = AssetId {
        network: "solana:mainnet-beta".into(),
        reference: USDC_MINT.into(),
        decimals: 6,
    };
    let usdc_identity = |intent: &FinancialIntent| {
        intent
            .expected
            .iter()
            .filter_map(|effect| match effect {
                gofin_domain::EffectConstraint::Transfer {
                    to, credit_asset, ..
                } => Some(FinancialPlanFundingIdentity {
                    resource: to.clone(),
                    asset: credit_asset.clone(),
                }),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let exact_scope = |prepared: &PreparedOperation| {
        json!({"mode":"exact", "maximum_debits":prepared.intent.debits,
            "maximum_costs":prepared.intent.maximum_costs,
            "possible_credits":usdc_identity(&prepared.intent)})
    };
    let wallet_minimum = ResourceAmount {
        resource: wallet_resource.clone(),
        amount: Amount {
            asset: sol_usdc.clone(),
            atoms: Quantity::from_decimal("0.5", 6).unwrap(),
        },
    };
    let cex_minimum = ResourceAmount {
        resource: ResourceId(ACCOUNT_RESOURCE.into()),
        amount: Amount {
            asset: AssetId {
                network: "binance".into(),
                reference: "USDC".into(),
                decimals: 36,
            },
            atoms: Quantity::from_decimal("0.5", 36).unwrap(),
        },
    };
    let plan = FinancialPlanSpec {
        version: 1,
        name: "Synthetic Binance Solana USDC funding".into(),
        summary: Some("Withdraw USDC to Solana, then deposit the verified proceeds.".into()),
        steps: vec![
            FinancialPlanStepSpec {
                step_id: "withdraw".into(),
                operation: "binance.withdraw".into(),
                input: withdraw_input.clone(),
                permitted_variations: exact_scope(&withdraw_prepared),
                funding_dependencies: vec![],
                action_deadline_seconds: None,
                completion: FinancialStepCompletion {
                    operation_succeeded: true,
                    principal_spend: None,
                    minimum_net_receipt: Some(wallet_minimum.clone()),
                },
            },
            FinancialPlanStepSpec {
                step_id: "deposit".into(),
                operation: "binance.deposit".into(),
                input: deposit.clone(),
                permitted_variations: exact_scope(&deposit_prepared),
                funding_dependencies: vec![FinancialPlanFundingDependency {
                    producing_step_id: "withdraw".into(),
                    resource: wallet_resource,
                    asset: sol_usdc,
                    funding_scope: Some(FinancialPlanFundingScope::PrincipalOnly),
                    maximum_fraction_bps: None,
                }],
                action_deadline_seconds: None,
                completion: FinancialStepCompletion {
                    operation_succeeded: true,
                    principal_spend: None,
                    minimum_net_receipt: Some(cex_minimum),
                },
            },
        ],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: withdraw_prepared
                .intent
                .debits
                .iter()
                .chain(&deposit_prepared.intent.debits)
                .cloned()
                .collect(),
            maximum_costs: withdraw_prepared
                .intent
                .maximum_costs
                .iter()
                .chain(&deposit_prepared.intent.maximum_costs)
                .cloned()
                .collect(),
        },
    };
    let client = Client::new(
        runtime(&db, venue.clone(), &fixture, approval.clone()),
        temp.path(),
    )
    .await;
    let plan_json = serde_json::to_string(&plan).unwrap();
    let mut auth = FinancialPlanAuthorizationRequest {
        request_id: "SYNTHETIC-BINANCE-SOLANA-FUNDING-PLAN".into(),
        plan: plan.clone(),
        valid_for_seconds: Some(1800),
        replaces_revision_id: None,
        expected_review_hash: None,
    };
    let preview = client
        .call(
            args(&[
                "plans",
                "preview",
                "--input",
                &plan_json,
                "--valid-for",
                "30m",
                "--json",
            ]),
            BrokerRequest::PreviewPlan {
                request: auth.clone(),
            },
            true,
        )
        .await;
    auth.expected_review_hash = Some(preview["review_hash"].as_str().unwrap().into());
    let authorized = client
        .call(
            args(&[
                "plans",
                "authorize",
                "--input",
                &plan_json,
                "--id",
                &auth.request_id,
                "--review-hash",
                auth.expected_review_hash.as_ref().unwrap(),
                "--valid-for",
                "30m",
                "--json",
            ]),
            BrokerRequest::AuthorizePlan { request: auth },
            true,
        )
        .await;
    let plan_id = authorized["plan_id"].as_str().unwrap().to_owned();
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.token_balance(&fixture.signer_address()), 0);

    let withdraw_id = "SYNTHETIC-PLAN-USDC-WITHDRAW-ATTEMPT";
    let withdraw_argv = args(&[
        "execute",
        "binance.withdraw",
        "--input",
        &withdraw_input.to_string(),
        "--id",
        withdraw_id,
        "--plan",
        &plan_id,
        "--step",
        "withdraw",
        "--json",
    ]);
    let payout = client
        .call(
            withdraw_argv,
            BrokerRequest::ExecutePlanStep {
                plan_id: plan_id.clone(),
                step_id: "withdraw".into(),
                request: OperationRequest {
                    request_id: withdraw_id.into(),
                    operation: "binance.withdraw".into(),
                    input: withdraw_input,
                    run_id: None,
                },
            },
            true,
        )
        .await;
    assert_eq!(payout["status"], "completed", "{payout}");
    assert_eq!(fixture.token_balance(&fixture.signer_address()), 500_000);
    let deposit_id = "SYNTHETIC-PLAN-USDC-DEPOSIT-ATTEMPT";
    let deposit_argv = args(&[
        "execute",
        "binance.deposit",
        "--input",
        &deposit.to_string(),
        "--id",
        deposit_id,
        "--plan",
        &plan_id,
        "--step",
        "deposit",
        "--json",
    ]);
    let request = BrokerRequest::ExecutePlanStep {
        plan_id,
        step_id: "deposit".into(),
        request: OperationRequest {
            request_id: deposit_id.into(),
            operation: "binance.deposit".into(),
            input: deposit.clone(),
            run_id: None,
        },
    };
    let pending = client
        .call(deposit_argv.clone(), request.clone(), true)
        .await;
    assert_eq!(pending["status"], "unknown", "{pending}");
    assert_eq!(
        pending["last_reconciliation_evidence"]["matchingVenueRows"],
        0
    );
    let signature = pending["submission"]["data"]["wallet_signature"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(fixture.rpc().send_count(), 1);
    drop(client);
    let resumed = Client::new(
        runtime(&db, venue.clone(), &fixture, approval.clone()),
        temp.path(),
    )
    .await;
    venue.set_free("USDC", "4.9");
    venue.record_credit("USDC", "0.5", &signature, &exchange_address());
    let completed = resumed.call(deposit_argv, request, true).await;
    assert_eq!(completed["status"], "completed", "{completed}");
    assert_eq!(
        fixture.rpc().send_count(),
        1,
        "restart must not replay the wallet source write"
    );
    assert_eq!(venue.0.lock().unwrap().withdrawal_history.len(), 1);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn solana_funding_cli_deposit_sol_and_usdc_require_exact_usable_credit_and_recover_without_resubmit()
 {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("binance-solana.sqlite");
    let fixture = SolanaFundingFixture::new("synthetic-solana", 5_000_000_000, 5_000_000);
    fixture.register_token_owner(&exchange_address());
    let venue = Arc::new(Venue::new(fixture.clone()));
    let approval = Arc::new(Allow(AtomicUsize::new(0)));
    let make_runtime = || runtime(&db, venue.clone(), &fixture, approval.clone());
    for (id, coin, amount, _atoms) in [
        ("SYNTHETIC-SOL-DEPOSIT", "SOL", "0.1", 100_000_000u64),
        ("SYNTHETIC-USDC-DEPOSIT", "USDC", "0.5", 500_000u64),
    ] {
        let client = Client::new(make_runtime(), temp.path()).await;
        let input = deposit_input(id, coin, amount);
        let preview = client
            .call(
                args(&[
                    "preview",
                    "binance.deposit",
                    "--input",
                    &input.to_string(),
                    "--json",
                ]),
                BrokerRequest::Preview {
                    operation: "binance.deposit".into(),
                    input: input.clone(),
                },
                true,
            )
            .await;
        assert!(preview["summary"].as_str().unwrap().contains("Binance"));
        let executed = client
            .call(
                args(&[
                    "execute",
                    "binance.deposit",
                    "--input",
                    &input.to_string(),
                    "--id",
                    id,
                    "--json",
                ]),
                BrokerRequest::Execute {
                    request: OperationRequest {
                        request_id: id.into(),
                        operation: "binance.deposit".into(),
                        input: input.clone(),
                        run_id: None,
                    },
                },
                true,
            )
            .await;
        assert_eq!(
            executed["status"], "unknown",
            "source finality without venue credit must remain unresolved: {executed}"
        );
        let tx = executed["submission"]["data"]["wallet_signature"]
            .as_str()
            .unwrap()
            .to_owned();
        let source_calls = fixture.rpc().send_count();
        assert!(source_calls > 0);
        let operation_id = executed["id"].as_str().unwrap().to_owned();
        drop(client);
        // Reopen the same durable journal before the destination credit exists.
        // Repeating the public request ID returns the original submission and
        // must not dispatch another Solana transaction.
        let recovered = Client::new(make_runtime(), temp.path()).await;
        let replay = recovered
            .call(
                args(&[
                    "execute",
                    "binance.deposit",
                    "--input",
                    &input.to_string(),
                    "--id",
                    id,
                    "--json",
                ]),
                BrokerRequest::Execute {
                    request: OperationRequest {
                        request_id: id.into(),
                        operation: "binance.deposit".into(),
                        input: input.clone(),
                        run_id: None,
                    },
                },
                true,
            )
            .await;
        assert_eq!(replay["id"], operation_id);
        assert_eq!(replay["submission"], executed["submission"]);
        assert_eq!(fixture.rpc().send_count(), source_calls);
        // A correct-amount credit row for another signature must not settle
        // this withdrawal. Binance history is correlated to the saved wallet
        // signature, not the immutable message hash or a near match.
        venue.settle(coin, amount, &tx.to_ascii_lowercase(), &exchange_address());
        let mismatched = recovered
            .call(
                args(&["status", &operation_id, "--json"]),
                BrokerRequest::Status {
                    id: operation_id.clone(),
                },
                true,
            )
            .await;
        assert_eq!(mismatched["status"], "unknown", "{mismatched}");
        // The credited-but-locked status 6 is usable, but this exact row and
        // free-balance delta must match the finalized wallet signature.
        venue.settle(coin, amount, &tx, &exchange_address());
        let status = recovered
            .call(
                args(&["status", &operation_id, "--json"]),
                BrokerRequest::Status { id: operation_id },
                true,
            )
            .await;
        assert_eq!(status["status"], "completed", "{status}");
        assert_eq!(status["effects"]["evidence"]["withdrawal_locked"], true);
        assert_eq!(
            fixture.rpc().send_count(),
            source_calls,
            "reconciliation must never resubmit"
        );
    }
}
