//! Public broker withdrawal rehearsal through the actual Coinbase profile,
//! controlled Coinbase transport, durable store and independent selected-chain proof.

#[cfg(target_os = "macos")]
#[path = "support/native_approval.rs"]
mod native_approval;

use async_trait::async_trait;
use gofin_broker::{
    BrokerConfigV2, BrokerPolicy, NetworkConfig, RuntimeDependencies, SharedUsdValuator,
    compose_runtime_with_clock,
};
use gofin_coinbase::{
    CoinbaseAccount, CoinbaseDestination, CoinbasePayoutRequest, CoinbasePayoutResult,
    CoinbaseProduct, CoinbaseProfile, CoinbaseRecipientVerifier,
};
use gofin_domain::{
    Adapter, AssetId, CostTreatment, FinancialEffect, OperationRecord, OperationRequest,
    ResourceId, Status, clock::Clock, financial_plans::*,
};
use gofin_exchange::{ExchangeAdapter, coinbase::*};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use gofin_protocol::BrokerRequest;
use gofin_store::Store;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

#[path = "support/coinbase_withdrawal_client.rs"]
mod coinbase_withdrawal_client;
use coinbase_withdrawal_client::Client;
#[path = "support/coinbase_http_fixture.rs"]
mod coinbase_http_fixture;

const PORTFOLIO: &str = "00000000-0000-4000-8000-000000000001";
const ACCOUNT_ID: &str = "00000000-0000-4000-8000-000000000002";
const HBAR_ACCOUNT_ID: &str = "00000000-0000-4000-8000-000000000003";
const WALLET: &str = "0x1111111111111111111111111111111111111111";
const WRONG_WALLET: &str = "0x2222222222222222222222222222222222222222";
const BASE_USDC: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
const TX_HASH: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const VENUE_TX: &str = "coinbase-app-send-1";
const SOURCE: &str = "coinbase:portfolio:00000000-0000-4000-8000-000000000001";

struct TestClock;
impl Clock for TestClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }
}
struct Approval(AtomicUsize);
#[async_trait]
impl gofin_domain::Authorizer for Approval {
    async fn authorize(
        &self,
        challenge: &gofin_domain::AuthorizationChallenge,
    ) -> gofin_domain::Result<gofin_domain::ApprovalAudit> {
        assert!(challenge.binding.to_string().contains("coinbase.withdraw"));
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(gofin_domain::ApprovalAudit {
            mechanism: "synthetic Coinbase withdrawal approval".into(),
            source: None,
            yolo_provenance: None,
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: gofin_domain::now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}
struct Prices;
#[async_trait]
impl UsdPriceSource for Prices {
    async fn quote(&self, asset: &AssetId) -> gofin_domain::Result<UsdQuote> {
        assert!(matches!(asset.reference.as_str(), "USDC" | "HBAR"));
        Ok(UsdQuote {
            usd_per_unit: if asset.reference == "HBAR" {
                "0.10"
            } else {
                "1"
            }
            .into(),
            updated_at: gofin_domain::now(),
            observed_at: gofin_domain::now(),
            source: "synthetic independent USDC quote".into(),
        })
    }
}

#[derive(Default)]
struct TransferState {
    transaction: Option<Value>,
    lose_send_response: bool,
    reject_travel_rule: bool,
    completed: bool,
    include_fee: bool,
    receipt: Option<Value>,
    wallet_credited: String,
    principal: String,
}
struct Venue {
    transport: Mutex<Option<Arc<dyn CoinbaseApi>>>,
    network: &'static str,
    writes: AtomicUsize,
    hbar_funding: std::sync::atomic::AtomicBool,
    hbar_sold: std::sync::atomic::AtomicBool,
    order: Mutex<Option<Value>>,
    state: Mutex<TransferState>,
    posted_body: Mutex<Option<Value>>,
    posted_bodies: Mutex<Vec<Value>>,
}
impl Default for Venue {
    fn default() -> Self {
        Self {
            transport: Mutex::new(None),
            network: "base",
            writes: AtomicUsize::new(0),
            hbar_funding: std::sync::atomic::AtomicBool::new(false),
            hbar_sold: std::sync::atomic::AtomicBool::new(false),
            order: Mutex::new(None),
            state: Mutex::new(TransferState::default()),
            posted_body: Mutex::new(None),
            posted_bodies: Mutex::new(Vec::new()),
        }
    }
}
impl Venue {
    fn route(&self) -> &'static gofin_coinbase::funding::CoinbaseNetwork {
        gofin_coinbase::funding::coinbase_network(self.network).unwrap()
    }
    fn response(body: Value) -> Result<CoinbaseResponse, CoinbaseError> {
        Ok(CoinbaseResponse::new(
            200,
            serde_json::to_vec(&body).unwrap(),
        ))
    }
    fn transaction(&self) -> Value {
        let state = self.state.lock().unwrap();
        let principal = state.principal.as_str();
        let amount = principal.parse::<f64>().unwrap_or(4.0);
        let fee = if state.include_fee { 0.3 } else { 0.0 };
        let mut network = json!({
            "network_name":self.network, "hash":TX_HASH,
            "transaction_amount":{"amount":principal,"currency":"USDC"}
        });
        if state.include_fee {
            network["transaction_fee"] = json!({"amount":"0.300000","currency":"USDC"});
        }
        json!({"data":{
            "id":VENUE_TX,"type":"send","idem":self.posted_body.lock().unwrap().as_ref().unwrap()["idem"],
            "status":if state.completed {"completed"} else {"pending"},
            "amount":{"amount":format!("-{:.6}", amount + fee),"currency":"USDC"},
            "to":{"address":WALLET},"network":network
        }})
    }
    fn source_balance(&self) -> String {
        if self.hbar_funding.load(Ordering::SeqCst) {
            let state = self.state.lock().unwrap();
            if state.completed && state.include_fee {
                return "1.100000".into();
            }
            return if self.hbar_sold.load(Ordering::SeqCst) {
                "2.400000".into()
            } else {
                "0.000000".into()
            };
        }
        let state = self.state.lock().unwrap();
        if state.completed && state.include_fee {
            "5.700000"
        } else {
            "10.000000"
        }
        .into()
    }
}
#[async_trait]
impl CoinbaseApi for Venue {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        if endpoint.mutating() {
            assert!(
                guard.as_ref().is_some_and(|check| check()),
                "Coinbase send requires dispatch authority"
            );
            self.writes.fetch_add(1, Ordering::SeqCst);
        } else {
            assert!(guard.is_none());
        }
        match endpoint {
            CoinbaseRoute::KeyPermissions => Self::response(
                json!({"portfolio_uuid":PORTFOLIO,"can_view":true,"can_trade":true,"can_transfer":true}),
            ),
            CoinbaseRoute::LegacyAccounts { .. } => Self::response(
                json!({"data":[{"id":ACCOUNT_ID,"currency":{"code":"USDC"}},{"id":HBAR_ACCOUNT_ID,"currency":{"code":"HBAR"}}],"pagination":{"next_starting_after":null}}),
            ),
            CoinbaseRoute::Accounts { .. } => {
                if self.hbar_funding.load(Ordering::SeqCst) {
                    let hbar = if self.hbar_sold.load(Ordering::SeqCst) {
                        "0"
                    } else {
                        "25"
                    };
                    return Self::response(json!({"accounts":[
                        {"uuid":ACCOUNT_ID,"currency":"USDC","portfolio_id":PORTFOLIO,"active":true,"ready":true,"available_balance":{"value":self.source_balance(),"currency":"USDC"},"hold":{"value":"0","currency":"USDC"}},
                        {"uuid":HBAR_ACCOUNT_ID,"currency":"HBAR","portfolio_id":PORTFOLIO,"active":true,"ready":true,"available_balance":{"value":hbar,"currency":"HBAR"},"hold":{"value":"0","currency":"HBAR"}}
                    ],"has_next":false}));
                }
                Self::response(
                    json!({"accounts":[{"uuid":ACCOUNT_ID,"currency":"USDC","portfolio_id":PORTFOLIO,"active":true,"ready":true,"available_balance":{"value":self.source_balance(),"currency":"USDC"},"hold":{"value":"0","currency":"USDC"}}],"has_next":false}),
                )
            }
            CoinbaseRoute::LegacyCurrencies => Self::response(json!({"data":[{"id":"USD"}]})),
            CoinbaseRoute::LegacyCryptoCurrencies => {
                Self::response(json!({"data":[{"id":"USDC"},{"id":"HBAR"}]}))
            }
            CoinbaseRoute::HistoricalOrders { .. } => {
                Self::response(json!({"orders":[],"has_next":false}))
            }
            CoinbaseRoute::Account { account_id } => {
                let (currency, value) = match account_id.as_str() {
                    ACCOUNT_ID => ("USDC", self.source_balance()),
                    HBAR_ACCOUNT_ID => (
                        "HBAR",
                        if self.hbar_sold.load(Ordering::SeqCst) {
                            "0".into()
                        } else {
                            "25".into()
                        },
                    ),
                    _ => panic!("unexpected account ID {account_id}"),
                };
                Self::response(
                    json!({"account":{"uuid":account_id,"currency":currency,"active":true,"ready":true,"available_balance":{"value":value,"currency":currency}}}),
                )
            }
            CoinbaseRoute::Product { product_id } => {
                assert_eq!(product_id, "HBAR-USDC");
                Self::response(
                    json!({"product":{"product_type":"SPOT","trading_disabled":false,"base_currency_id":"HBAR","quote_currency_id":"USDC","base_increment":"0.001","quote_increment":"0.01","price_increment":"0.01","base_min_size":"0.001","quote_min_size":"0.01"}}),
                )
            }
            CoinbaseRoute::OrderPreview => Self::response(
                json!({"base_size":"25","quote_size":"2.50","order_total":"2.50","commission_total":"0.10","errs":[]}),
            ),
            CoinbaseRoute::Orders => {
                let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
                assert_eq!(body["product_id"], "HBAR-USDC");
                assert_eq!(body["side"], "SELL");
                let order_id = "venue-hbar-sale-1";
                *self.order.lock().unwrap() = Some(
                    json!({"order_id":order_id,"client_order_id":body["client_order_id"],"product_id":"HBAR-USDC","side":"SELL","status":"FILLED","filled_size":"25","filled_value":"2.50","total_fees":"0.10","size_in_quote":false,"settled":true}),
                );
                self.hbar_sold.store(true, Ordering::SeqCst);
                Self::response(json!({"success":true,"success_response":{"order_id":order_id}}))
            }
            CoinbaseRoute::Order { order_id } => {
                assert_eq!(order_id, "venue-hbar-sale-1");
                Self::response(json!({"order":self.order.lock().unwrap().clone().unwrap()}))
            }
            CoinbaseRoute::Fills { .. } => {
                let order = self.order.lock().unwrap().clone().unwrap();
                Self::response(
                    json!({"fills":[{"entry_id":"venue-hbar-fill-1","order_id":"venue-hbar-sale-1","product_id":"HBAR-USDC","side":"SELL","price":"0.10","size":"25","size_in_quote":false,"commission":"0.10"}],"has_next":false,"order":order}),
                )
            }
            CoinbaseRoute::TransactionSend { account_id } => {
                assert_eq!(account_id, ACCOUNT_ID);
                let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
                assert_eq!(body["type"], "send");
                assert_eq!(body["currency"], "USDC");
                assert_eq!(body["network"], self.network);
                assert_eq!(body["to"], WALLET);
                let amount = body["amount"].as_str().unwrap().to_owned();
                assert!(uuid::Uuid::parse_str(body["idem"].as_str().unwrap()).is_ok());
                *self.posted_body.lock().unwrap() = Some(body.clone());
                self.posted_bodies.lock().unwrap().push(body.clone());
                self.state.lock().unwrap().principal = amount;
                self.state.lock().unwrap().transaction =
                    Some(json!({"id":VENUE_TX,"type":"send","idem":body["idem"]}));
                if self.state.lock().unwrap().reject_travel_rule {
                    return Ok(CoinbaseResponse::new(
                        400,
                        serde_json::to_vec(&json!({"errors":[{"id":"TRAVEL_RULE_REQUIRED","message":"Travel Rule beneficiary fields required"}]})).unwrap(),
                    ));
                }
                if self.state.lock().unwrap().lose_send_response {
                    return Err(CoinbaseError::Transport);
                }
                Self::response(json!({"data":{"id":VENUE_TX,"type":"send","idem":body["idem"]}}))
            }
            CoinbaseRoute::Transactions { account_id, .. } => {
                assert_eq!(account_id, ACCOUNT_ID);
                let state = self.state.lock().unwrap();
                // Keep the outage active through the engine's automatic first
                // reconciliation. Later recovery must use the durable idem,
                // even when neither dispatch nor the initial read responded.
                if state.lose_send_response && !state.completed {
                    return Err(CoinbaseError::Transport);
                }
                let transaction = state.transaction.clone();
                Self::response(
                    json!({"data":transaction.into_iter().collect::<Vec<_>>(),"pagination":{"next_starting_after":null}}),
                )
            }
            CoinbaseRoute::Transaction {
                account_id,
                transaction_id,
            } => {
                assert_eq!(account_id, ACCOUNT_ID);
                assert_eq!(transaction_id, VENUE_TX);
                Self::response(self.transaction())
            }
            other => panic!("unexpected Coinbase route {other:?}"),
        }
    }
}

struct IndependentBaseProof {
    venue: Arc<Venue>,
    proofs: AtomicUsize,
}
#[async_trait]
impl CoinbaseRecipientVerifier for IndependentBaseProof {
    async fn resolve(
        &self,
        network: &str,
        coin: &str,
        contract: Option<&str>,
        address: &str,
        name: Option<&str>,
    ) -> gofin_domain::Result<Option<CoinbaseDestination>> {
        if (network, coin, contract, address, name)
            != (
                self.venue.network,
                "USDC",
                Some(self.venue.route().usdc),
                WALLET,
                Some("evm-dev"),
            )
        {
            return Ok(None);
        }
        Ok(Some(CoinbaseDestination {
            enrolled: true,
            resource: ResourceId(format!(
                "ethereum:{}:{}",
                self.venue.route().chain_id(),
                WALLET.to_ascii_lowercase()
            )),
            address: WALLET.into(),
            chain_id: self.venue.route().chain_id(),
            asset: AssetId {
                network: format!("ethereum:{}", self.venue.route().chain_id()),
                reference: self.venue.route().usdc.into(),
                decimals: 6,
            },
            confirmations: 1,
            network: self.venue.network.into(),
        }))
    }
    async fn verify(
        &self,
        request: CoinbasePayoutRequest,
    ) -> gofin_domain::Result<CoinbasePayoutResult> {
        self.proofs.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.tx_hash, TX_HASH);
        assert_eq!(request.destination.address, WALLET);
        assert_eq!(request.destination.asset.reference, self.venue.route().usdc);
        assert_eq!(request.destination.chain_id, self.venue.route().chain_id());
        let receipt = self.venue.state.lock().unwrap().receipt.clone();
        let exact_amount = receipt.as_ref().and_then(|r| r["amountAtoms"].as_str());
        let expected_atoms = if request.minimum == "1.000000" {
            "1000000"
        } else {
            "4000000"
        };
        let correct = receipt.as_ref().is_some_and(|r| {
            r["status"] == "0x1"
                && r["transactionHash"] == request.tx_hash
                && r["to"] == request.destination.address
                && r["tokenContract"] == request.destination.asset.reference
                && r["chainId"] == self.venue.route().chain_id()
                && exact_amount == Some(expected_atoms)
                && request.minimum == request.maximum
        });
        if correct {
            self.venue.state.lock().unwrap().wallet_credited = request.minimum.clone();
        }
        Ok(CoinbasePayoutResult {
            status: if correct {
                Status::Completed
            } else {
                Status::Pending
            },
            amount: correct.then(|| gofin_domain::Quantity::new(exact_amount.unwrap()).unwrap()),
            evidence: json!({"source":"independent synthetic selected-network receipt observer","receipt":receipt,"confirmations":1}),
        })
    }
}

fn make_runtime(
    path: &std::path::Path,
    venue: Arc<Venue>,
    proof: Arc<IndependentBaseProof>,
    approval: Arc<Approval>,
) -> Arc<gofin_broker::BrokerRuntime> {
    let adapter = exchange_adapter(venue, proof);
    compose_runtime_with_clock(
        BrokerConfigV2 {
            version: 2,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy: BrokerPolicy::beta(),
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        RuntimeDependencies {
            store: Store::open(path).unwrap(),
            adapters: vec![adapter],
            authorizer: approval,
            valuator: Arc::new(SharedUsdValuator::with_source(Arc::new(Prices))),
            bridge: None,
        },
        Arc::new(TestClock),
    )
    .unwrap()
}

fn exchange_adapter(
    venue: Arc<Venue>,
    proof: Arc<IndependentBaseProof>,
) -> Arc<ExchangeAdapter<CoinbaseProfile>> {
    let transport: Arc<dyn CoinbaseApi> = venue
        .transport
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| venue.clone());
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "coinbase-dev".into(),
            resource: ResourceId(SOURCE.into()),
            credential_ref: "synthetic-coinbase".into(),
            physical_identity: PORTFOLIO.into(),
            identity_verified: true,
            portfolio_id: Some(PORTFOLIO.into()),
            product: CoinbaseProduct::Advanced,
        }],
        BTreeMap::from([("synthetic-coinbase".into(), transport)]),
    )
    .unwrap()
    .with_prices(Arc::new(Prices))
    .with_recipient_verifier(proof);
    Arc::new(ExchangeAdapter::new(profile))
}

fn input() -> Value {
    json!({"account":"coinbase-dev","accountId":ACCOUNT_ID,"destination":"evm-dev","address":WALLET,"network":"base","amount":"4.000000","feeMode":"venue_calculated","feeReserveUsdc":"0.100000","requestId":"synthetic-base-usdc-withdrawal"})
}

fn exact_step(
    step_id: &str,
    input: Value,
    debits: Vec<gofin_domain::ResourceAmount>,
    costs: Vec<gofin_domain::CostBound>,
    credits: Vec<FinancialPlanFundingIdentity>,
    dependencies: Vec<FinancialPlanFundingDependency>,
) -> FinancialPlanStepSpec {
    FinancialPlanStepSpec {
        step_id: step_id.into(),
        operation: if step_id == "sell-hbar" {
            "coinbase.create-order".into()
        } else {
            "coinbase.withdraw".into()
        },
        input,
        permitted_variations: json!({
            "mode":"exact",
            "maximum_debits":debits,
            "maximum_costs":costs,
            "possible_credits":credits
        }),
        funding_dependencies: dependencies,
        action_deadline_seconds: None,
        completion: FinancialStepCompletion {
            operation_succeeded: true,
            principal_spend: None,
            minimum_net_receipt: None,
        },
    }
}

fn plan(
    sale: &gofin_domain::PreparedOperation,
    withdrawal: &gofin_domain::PreparedOperation,
    sale_input: Value,
    withdrawal_input: Value,
) -> FinancialPlanSpec {
    let route =
        gofin_coinbase::funding::coinbase_network(withdrawal_input["network"].as_str().unwrap())
            .unwrap();
    let usdc = AssetId {
        network: "coinbase".into(),
        reference: "USDC".into(),
        decimals: 36,
    };
    let sale_credit = FinancialPlanFundingIdentity {
        resource: ResourceId(SOURCE.into()),
        asset: usdc.clone(),
    };
    let withdrawal_credit = FinancialPlanFundingIdentity {
        resource: ResourceId(format!(
            "ethereum:{}:{}",
            route.chain_id(),
            WALLET.to_ascii_lowercase()
        )),
        asset: AssetId {
            network: format!("ethereum:{}", route.chain_id()),
            reference: route.usdc.into(),
            decimals: 6,
        },
    };
    let dependency = FinancialPlanFundingDependency {
        producing_step_id: "sell-hbar".into(),
        resource: ResourceId(SOURCE.into()),
        asset: usdc,
        funding_scope: Some(FinancialPlanFundingScope::PrincipalOnly),
        maximum_fraction_bps: None,
    };
    let sell = exact_step(
        "sell-hbar",
        sale_input,
        sale.intent.debits.clone(),
        sale.intent.maximum_costs.clone(),
        vec![sale_credit],
        vec![],
    );
    let withdraw = exact_step(
        "withdraw-usdc",
        withdrawal_input,
        withdrawal.intent.debits.clone(),
        withdrawal.intent.maximum_costs.clone(),
        vec![withdrawal_credit],
        vec![dependency],
    );
    let mut maximum_debits = sale.intent.debits.clone();
    maximum_debits.extend(withdrawal.intent.debits.clone());
    let mut maximum_costs = sale.intent.maximum_costs.clone();
    maximum_costs.extend(withdrawal.intent.maximum_costs.clone());
    FinancialPlanSpec {
        version: 1,
        name: format!("Sell HBAR and withdraw {} USDC", route.network),
        summary: Some(format!(
            "Sell 25 HBAR, then withdraw 1 USDC to the enrolled {} wallet.",
            route.network
        )),
        steps: vec![sell, withdraw],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits,
            maximum_costs,
        },
    }
}

fn plan_cli_args(command: &str, plan: &FinancialPlanSpec) -> Vec<String> {
    vec![
        "plans".into(),
        command.into(),
        "--input".into(),
        serde_json::to_string(plan).unwrap(),
        "--json".into(),
    ]
}

fn rejection_code(value: &Value) -> Option<&str> {
    value["error"]["code"]
        .as_str()
        .or_else(|| value["code"].as_str())
        .or_else(|| value["result"]["error"]["code"].as_str())
}

fn hbar_sale_input() -> Value {
    json!({"account":"coinbase-dev","requestId":"plan-hbar-sale","productId":"HBAR-USDC","side":"SELL","orderConfiguration":{"market_market_ioc":{"base_size":"25"}},"maximumSpend":"25","minimumReceive":"2.40","maximumFee":"0.10"})
}

fn withdrawal_input() -> Value {
    json!({"account":"coinbase-dev","accountId":ACCOUNT_ID,"destination":"evm-dev","address":WALLET,"network":"base","amount":"1.000000","feeMode":"venue_calculated","feeReserveUsdc":"0.100000","requestId":"plan-base-withdrawal"})
}

async fn build_plan_candidates(
    venue: Arc<Venue>,
    proof: Arc<IndependentBaseProof>,
) -> (
    Arc<ExchangeAdapter<CoinbaseProfile>>,
    gofin_domain::PreparedOperation,
    gofin_domain::PreparedOperation,
    Value,
    Value,
) {
    let adapter = exchange_adapter(venue.clone(), proof);
    let sale_input = hbar_sale_input();
    let mut withdraw_input = withdrawal_input();
    withdraw_input["network"] = json!(venue.network);
    let sale = adapter
        .prepare("coinbase.create-order", &sale_input)
        .await
        .unwrap();
    // Candidate preparation represents the proceeds available after the
    // first step. The venue starts at zero USDC and makes this balance only
    // after the HBAR sale is submitted and settled.
    venue.hbar_sold.store(true, Ordering::SeqCst);
    let withdrawal = adapter
        .prepare("coinbase.withdraw", &withdraw_input)
        .await
        .unwrap();
    venue.hbar_sold.store(false, Ordering::SeqCst);
    #[cfg(target_os = "macos")]
    {
        native_approval::assert_prepared_renders(&sale);
        let reason = native_approval::assert_prepared_renders(&withdrawal);
        assert!(
            reason.contains("not capped") || reason.contains("not a fee cap"),
            "{reason}"
        );
    }
    (adapter, sale, withdrawal, sale_input, withdraw_input)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_cli_hbar_sale_funds_one_approved_base_withdrawal_and_recovers_lost_send_without_replay()
 {
    planned_withdrawal_recovery("base").await;
}

#[tokio::test]
async fn public_cli_coinbase_plan_withdrawals_keep_each_selected_network_through_recovery() {
    for network in ["ethereum", "arbitrum", "optimism", "polygon", "avalanche"] {
        planned_withdrawal_recovery(network).await;
    }
}

async fn planned_withdrawal_recovery(network: &'static str) {
    let route = gofin_coinbase::funding::coinbase_network(network).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let store_path = temp.path().join("coinbase-hbar-withdrawal-plan.sqlite");
    let venue = Arc::new(Venue {
        network,
        ..Venue::default()
    });
    venue.hbar_funding.store(true, Ordering::SeqCst);
    let http = if network == "base" {
        let http = coinbase_http_fixture::Fixture::new(venue.clone()).await;
        *venue.transport.lock().unwrap() = Some(http.api.clone());
        Some(http)
    } else {
        None
    };
    let proof = Arc::new(IndependentBaseProof {
        venue: venue.clone(),
        proofs: AtomicUsize::new(0),
    });
    let approval = Arc::new(Approval(AtomicUsize::new(0)));
    let (_adapter, sale, withdrawal, sale_input, withdrawal_input) =
        build_plan_candidates(venue.clone(), proof.clone()).await;
    let plan = plan(
        &sale,
        &withdrawal,
        sale_input.clone(),
        withdrawal_input.clone(),
    );
    #[cfg(target_os = "macos")]
    {
        let reason = native_approval::assert_review_renders(
            gofin_engine::financial_plan_approval_review(&plan, 1800),
            "financial_plan",
            "plan",
            serde_json::json!({"kind":"financial_plan"}),
            "plan",
        );
        assert!(
            reason.contains("Coinbase fee uncapped; reserve is not cap."),
            "{reason}"
        );
    }
    assert_eq!(
        plan.steps[1].funding_dependencies[0].funding_scope,
        Some(FinancialPlanFundingScope::PrincipalOnly)
    );
    let mut request = FinancialPlanAuthorizationRequest {
        request_id: "coinbase-hbar-withdrawal-plan".into(),
        plan: plan.clone(),
        valid_for_seconds: Some(1800),
        replaces_revision_id: None,
        expected_review_hash: None,
    };
    let client = Client::new(
        make_runtime(&store_path, venue.clone(), proof.clone(), approval.clone()),
        temp.path(),
    )
    .await;

    let mut preview_args = plan_cli_args("preview", &plan);
    preview_args.splice(4..4, ["--valid-for".into(), "30m".into()]);
    let preview = client
        .call(
            preview_args,
            BrokerRequest::PreviewPlan {
                request: request.clone(),
            },
            true,
        )
        .await;
    request.expected_review_hash = Some(preview["review_hash"].as_str().unwrap().into());
    let mut authorize_args = plan_cli_args("authorize", &plan);
    authorize_args.splice(
        4..4,
        [
            "--id".into(),
            request.request_id.clone(),
            "--valid-for".into(),
            "30m".into(),
            "--review-hash".into(),
            request.expected_review_hash.clone().unwrap(),
        ],
    );
    let authorized = client
        .call(
            authorize_args,
            BrokerRequest::AuthorizePlan {
                request: request.clone(),
            },
            true,
        )
        .await;
    let plan_id = authorized["plan_id"].as_str().unwrap().to_owned();
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);

    let early_withdrawal = client
        .call(
            vec![
                "execute".into(),
                "coinbase.withdraw".into(),
                "--input".into(),
                withdrawal_input.to_string(),
                "--id".into(),
                "withdraw-too-early".into(),
                "--plan".into(),
                plan_id.clone(),
                "--step".into(),
                "withdraw-usdc".into(),
                "--json".into(),
            ],
            BrokerRequest::ExecutePlanStep {
                plan_id: plan_id.clone(),
                step_id: "withdraw-usdc".into(),
                request: OperationRequest {
                    request_id: "withdraw-too-early".into(),
                    operation: "coinbase.withdraw".into(),
                    input: withdrawal_input.clone(),
                    run_id: None,
                },
            },
            false,
        )
        .await;
    let early_code = early_withdrawal["error"]["code"]
        .as_str()
        .or_else(|| early_withdrawal["code"].as_str());
    assert_eq!(
        early_code,
        Some("PLAN_STEP_NOT_ELIGIBLE"),
        "{early_withdrawal}"
    );
    assert_eq!(venue.writes.load(Ordering::SeqCst), 0);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);

    let sale_request = OperationRequest {
        request_id: "hbar-sale-attempt".into(),
        operation: "coinbase.create-order".into(),
        input: sale_input.clone(),
        run_id: None,
    };
    let sale_result = client
        .call(
            vec![
                "execute".into(),
                "coinbase.create-order".into(),
                "--input".into(),
                sale_input.to_string(),
                "--id".into(),
                sale_request.request_id.clone(),
                "--plan".into(),
                plan_id.clone(),
                "--step".into(),
                "sell-hbar".into(),
                "--json".into(),
            ],
            BrokerRequest::ExecutePlanStep {
                plan_id: plan_id.clone(),
                step_id: "sell-hbar".into(),
                request: sale_request,
            },
            true,
        )
        .await;
    assert_eq!(sale_result["status"], "completed", "{sale_result}");
    assert_eq!(venue.source_balance(), "2.400000");
    assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);

    venue.state.lock().unwrap().lose_send_response = true;
    let withdrawal_request = OperationRequest {
        request_id: "base-withdrawal-attempt".into(),
        operation: "coinbase.withdraw".into(),
        input: withdrawal_input.clone(),
        run_id: None,
    };
    let first = client
        .call(
            vec![
                "execute".into(),
                "coinbase.withdraw".into(),
                "--input".into(),
                withdrawal_input.to_string(),
                "--id".into(),
                withdrawal_request.request_id.clone(),
                "--plan".into(),
                plan_id.clone(),
                "--step".into(),
                "withdraw-usdc".into(),
                "--json".into(),
            ],
            BrokerRequest::ExecutePlanStep {
                plan_id: plan_id.clone(),
                step_id: "withdraw-usdc".into(),
                request: withdrawal_request.clone(),
            },
            true,
        )
        .await;
    assert_eq!(first["status"], "unknown", "{first}");
    let operation_id = first["id"].as_str().unwrap().to_owned();
    assert_eq!(venue.writes.load(Ordering::SeqCst), 2);
    drop(client);

    let client = Client::new(
        make_runtime(&store_path, venue.clone(), proof.clone(), approval.clone()),
        temp.path(),
    )
    .await;
    let replay = client
        .call(
            vec![
                "execute".into(),
                "coinbase.withdraw".into(),
                "--input".into(),
                withdrawal_input.to_string(),
                "--id".into(),
                withdrawal_request.request_id.clone(),
                "--plan".into(),
                plan_id.clone(),
                "--step".into(),
                "withdraw-usdc".into(),
                "--json".into(),
            ],
            BrokerRequest::ExecutePlanStep {
                plan_id: plan_id.clone(),
                step_id: "withdraw-usdc".into(),
                request: withdrawal_request,
            },
            true,
        )
        .await;
    assert_eq!(replay["id"], operation_id);
    assert_eq!(
        venue.writes.load(Ordering::SeqCst),
        2,
        "recovery must not replay Coinbase's send POST"
    );
    assert_eq!(venue.posted_bodies.lock().unwrap().len(), 1);

    {
        let mut state = venue.state.lock().unwrap();
        state.completed = true;
        state.include_fee = true;
        state.receipt = Some(
            json!({"status":"0x1","transactionHash":TX_HASH,"tokenContract":route.usdc,"to":WALLET,"chainId":route.chain_id(),"amountAtoms":"1000000"}),
        );
    }
    let settled = client.status(&operation_id).await;
    assert_eq!(settled["status"], "completed", "{settled}");
    let record: OperationRecord = serde_json::from_value(settled.clone()).unwrap();
    let verified = record.effects.unwrap();
    assert!(verified.effects.iter().any(|effect| matches!(effect, FinancialEffect::Transfer {from,to,debit,credit} if from.0 == SOURCE && to.0 == format!("ethereum:{}:{}", route.chain_id(), WALLET.to_ascii_lowercase()) && debit.asset.reference == "USDC" && credit.asset.reference == route.usdc && debit.atoms.to_decimal(36) == "1" && credit.atoms.to_decimal(6) == "1")));
    assert!(verified.effects.iter().any(|effect| matches!(effect, FinancialEffect::Cost {amount,treatment:CostTreatment::VenueCalculatedAdditionalDebit,..} if amount.asset.reference == "USDC" && amount.atoms.to_decimal(36) == "0.3")));
    assert_eq!(verified.evidence["feeWithinReserve"], false);
    assert_eq!(verified.evidence["actualFee"], "0.300000");
    assert_eq!(venue.source_balance(), "1.100000");
    assert_eq!(venue.writes.load(Ordering::SeqCst), 2);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(proof.proofs.load(Ordering::SeqCst), 1);
    let plan_status = client
        .call(
            vec![
                "plans".into(),
                "status".into(),
                plan_id.clone(),
                "--json".into(),
            ],
            BrokerRequest::PlanStatus { plan_id },
            true,
        )
        .await;
    assert_eq!(
        plan_status["steps"][1]["state"], "completed",
        "{plan_status}"
    );
    if let Some(http) = http {
        http.assert_dispatched();
        let calls = http.calls.lock().unwrap();
        let writes = calls
            .iter()
            .filter(|call| call.mutating)
            .collect::<Vec<_>>();
        assert_eq!(
            writes.len(),
            2,
            "One sale and one withdrawal, with no replay after response loss."
        );
        let send = writes
            .iter()
            .find(|call| call.path.ends_with("/transactions"))
            .unwrap();
        let body: Value = serde_json::from_str(&send.body).unwrap();
        assert_eq!(
            body["idem"],
            venue.posted_body.lock().unwrap().as_ref().unwrap()["idem"]
        );
        assert_eq!(body["to"], WALLET);
    }
}

#[tokio::test]
async fn exact_plan_rejects_insufficient_aggregate_bounds_and_unenrolled_coinbase_destinations() {
    let temp = tempfile::tempdir().unwrap();
    let store_path = temp
        .path()
        .join("coinbase-hbar-withdrawal-rejections.sqlite");
    let venue = Arc::new(Venue::default());
    venue.hbar_funding.store(true, Ordering::SeqCst);
    let proof = Arc::new(IndependentBaseProof {
        venue: venue.clone(),
        proofs: AtomicUsize::new(0),
    });
    let approval = Arc::new(Approval(AtomicUsize::new(0)));
    let (_adapter, sale, withdrawal, sale_input, withdrawal_input) =
        build_plan_candidates(venue.clone(), proof.clone()).await;
    let client = Client::new(
        make_runtime(&store_path, venue.clone(), proof, approval.clone()),
        temp.path(),
    )
    .await;

    let mut too_small = plan(&sale, &withdrawal, sale_input, withdrawal_input.clone());
    too_small.aggregate_bounds.maximum_debits.pop();
    let rejected_plan = client
        .call(
            plan_cli_args("preview", &too_small),
            BrokerRequest::PreviewPlan {
                request: FinancialPlanAuthorizationRequest {
                    request_id: "coinbase-underbounded-plan".into(),
                    plan: too_small,
                    valid_for_seconds: Some(1800),
                    replaces_revision_id: None,
                    expected_review_hash: None,
                },
            },
            false,
        )
        .await;
    assert!(rejection_code(&rejected_plan).is_some(), "{rejected_plan}");
    assert_eq!(approval.0.load(Ordering::SeqCst), 0);
    assert_eq!(venue.writes.load(Ordering::SeqCst), 0);

    let mut wrong_address = withdrawal_input.clone();
    wrong_address["address"] = json!(WRONG_WALLET);
    let rejected_address = client
        .call(
            vec![
                "preview".into(),
                "coinbase.withdraw".into(),
                "--input".into(),
                wrong_address.to_string(),
                "--json".into(),
            ],
            BrokerRequest::Preview {
                operation: "coinbase.withdraw".into(),
                input: wrong_address,
            },
            false,
        )
        .await;
    assert!(
        rejection_code(&rejected_address).is_some(),
        "{rejected_address}"
    );

    let mut wrong_account = withdrawal_input;
    wrong_account["account"] = json!("un-enrolled-coinbase-account");
    let rejected_account = client
        .call(
            vec![
                "preview".into(),
                "coinbase.withdraw".into(),
                "--input".into(),
                wrong_account.to_string(),
                "--json".into(),
            ],
            BrokerRequest::Preview {
                operation: "coinbase.withdraw".into(),
                input: wrong_account,
            },
            false,
        )
        .await;
    assert!(
        rejection_code(&rejected_account).is_some(),
        "{rejected_account}"
    );
    assert_eq!(approval.0.load(Ordering::SeqCst), 0);
    assert_eq!(venue.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn app_send_survives_response_loss_restart_and_books_actual_fee_above_reserve_after_independent_base_receipt()
 {
    let temp = tempfile::tempdir().unwrap();
    let store_path = temp.path().join("coinbase-withdrawal.sqlite");
    let venue = Arc::new(Venue::default());
    venue.state.lock().unwrap().lose_send_response = true;
    let proof = Arc::new(IndependentBaseProof {
        venue: venue.clone(),
        proofs: AtomicUsize::new(0),
    });
    let approval = Arc::new(Approval(AtomicUsize::new(0)));
    let client = Client::new(
        make_runtime(&store_path, venue.clone(), proof.clone(), approval.clone()),
        temp.path(),
    )
    .await;

    // Coinbase accepts the POST, then the controlled transport loses its HTTP
    // response. Recovery must find the original send by its persisted idem.
    let first = client.execute(input(), true).await;
    assert_eq!(first["status"], "unknown", "{first}");
    let operation_id = first["id"].as_str().unwrap().to_owned();
    assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
    assert!(first["submission"]["reference"].as_str().is_some());
    drop(client);

    let client = Client::new(
        make_runtime(&store_path, venue.clone(), proof.clone(), approval.clone()),
        temp.path(),
    )
    .await;
    let replay = client.execute(input(), true).await;
    assert_eq!(replay["id"], operation_id);
    assert_eq!(
        venue.writes.load(Ordering::SeqCst),
        1,
        "recovery must preserve original Coinbase idem and never replay the POST"
    );

    // Coinbase now reports completion and a 0.30 actual fee, while the user's
    // finite fee accounting reserve was only 0.10. The independent receipt is
    // set separately from the venue's response and must prove the exact Base
    // USDC transfer before the engine records completion.
    {
        let mut state = venue.state.lock().unwrap();
        state.completed = true;
        state.include_fee = true;
        state.receipt = Some(
            json!({"status":"0x1","transactionHash":TX_HASH,"tokenContract":BASE_USDC,"to":WALLET,"chainId":8453,"amountAtoms":"4000000"}),
        );
    }
    let settled = client.status(&operation_id).await;
    assert_eq!(settled["status"], "completed", "{settled}");
    let record: OperationRecord = serde_json::from_value(settled.clone()).unwrap();
    let verified = record.effects.unwrap();
    assert!(verified.effects.iter().any(|effect| matches!(effect, FinancialEffect::Transfer {from,to,debit,credit} if from.0 == SOURCE && to.0 == format!("ethereum:8453:{}", WALLET.to_ascii_lowercase()) && debit.asset.reference == "USDC" && credit.asset.reference == BASE_USDC && debit.atoms.to_decimal(36) == "4" && credit.atoms.to_decimal(6) == "4")));
    assert!(verified.effects.iter().any(|effect| matches!(effect, FinancialEffect::Cost {amount,treatment:CostTreatment::VenueCalculatedAdditionalDebit,..} if amount.asset.reference == "USDC" && amount.atoms.to_decimal(36) == "0.3")));
    assert_eq!(verified.evidence["feeWithinReserve"], false);
    assert_eq!(verified.evidence["actualFee"], "0.300000");
    assert_eq!(verified.evidence["signedSourceDebit"], "-4.300000");
    assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
    assert!(proof.proofs.load(Ordering::SeqCst) >= 1);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(venue.source_balance(), "5.700000");
    assert_eq!(venue.state.lock().unwrap().wallet_credited, "4.000000");
    let snapshot = client.runtime.handle(BrokerRequest::Snapshot).await;
    assert!(snapshot.ok, "{:?}", snapshot.error);
    let snapshot: gofin_domain::StateSnapshot =
        serde_json::from_value(snapshot.result.unwrap()).unwrap();
    assert!(snapshot.state.holdings.iter().any(|holding| {
        holding.resource.0 == SOURCE
            && holding.amount.asset.reference == "USDC"
            && holding.amount.atoms.to_decimal(36) == "5.7"
    }));
}

#[tokio::test]
async fn missing_fee_or_wrong_independent_receipt_keeps_send_pending_without_replay() {
    let temp = tempfile::tempdir().unwrap();
    let store_path = temp.path().join("coinbase-withdrawal-pending.sqlite");
    let venue = Arc::new(Venue::default());
    let proof = Arc::new(IndependentBaseProof {
        venue: venue.clone(),
        proofs: AtomicUsize::new(0),
    });
    let approval = Arc::new(Approval(AtomicUsize::new(0)));
    let client = Client::new(
        make_runtime(&store_path, venue.clone(), proof.clone(), approval),
        temp.path(),
    )
    .await;
    let first = client.execute(input(), true).await;
    let id = first["id"].as_str().unwrap().to_owned();
    assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
    {
        let mut state = venue.state.lock().unwrap();
        state.completed = true;
        state.include_fee = false;
        state.receipt = Some(
            json!({"status":"0x1","transactionHash":TX_HASH,"tokenContract":BASE_USDC,"to":WALLET,"chainId":8453,"amountAtoms":"4000000"}),
        );
    }
    let missing_fee = client.status(&id).await;
    assert_eq!(
        missing_fee["status"], "unknown",
        "missing native fee must never be treated as zero: {missing_fee}"
    );
    {
        let mut state = venue.state.lock().unwrap();
        state.include_fee = true;
        state.receipt = Some(
            json!({"status":"0x1","transactionHash":TX_HASH,"tokenContract":BASE_USDC,"to":WRONG_WALLET,"chainId":8453,"amountAtoms":"4000000"}),
        );
    }
    let wrong_receipt = client.status(&id).await;
    assert_eq!(
        wrong_receipt["status"], "unknown",
        "a mismatched destination receipt cannot settle: {wrong_receipt}"
    );
    {
        venue.state.lock().unwrap().receipt = Some(
            json!({"status":"0x1","transactionHash":TX_HASH,"tokenContract":BASE_USDC,"to":WALLET,"chainId":8453,"amountAtoms":"4000000"}),
        );
    }
    let settled = client.status(&id).await;
    assert_eq!(settled["status"], "completed", "{settled}");
    assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
    assert_eq!(venue.source_balance(), "5.700000");
    assert_eq!(venue.state.lock().unwrap().wallet_credited, "4.000000");
    assert_eq!(
        client.runtime.engine.allocation_used_usd_micros().unwrap(),
        "4300000"
    );
    // Reconciliation is idempotent and cannot charge the additional fee twice.
    assert_eq!(client.status(&id).await["status"], "completed");
    assert_eq!(
        client.runtime.engine.allocation_used_usd_micros().unwrap(),
        "4300000"
    );
}

#[tokio::test]
async fn captured_travel_rule_rejection_fails_without_effects_and_corrected_new_request_is_distinct()
 {
    let temp = tempfile::tempdir().unwrap();
    let store_path = temp.path().join("coinbase-withdrawal-travel-rule.sqlite");
    let venue = Arc::new(Venue::default());
    venue.state.lock().unwrap().reject_travel_rule = true;
    let proof = Arc::new(IndependentBaseProof {
        venue: venue.clone(),
        proofs: AtomicUsize::new(0),
    });
    let approval = Arc::new(Approval(AtomicUsize::new(0)));
    let client = Client::new(
        make_runtime(&store_path, venue.clone(), proof.clone(), approval.clone()),
        temp.path(),
    )
    .await;

    let mut missing_data = input();
    missing_data["requestId"] = json!("withdrawal-travel-rule-missing");
    let rejected = client.execute(missing_data.clone(), true).await;
    let original_id = rejected["id"].as_str().unwrap().to_owned();
    assert_eq!(venue.writes.load(Ordering::SeqCst), 1);

    // Reopening the durable runtime and repeating the same CLI request must
    // surface the captured venue rejection, never resend the original POST.
    drop(client);
    let client = Client::new(
        make_runtime(&store_path, venue.clone(), proof.clone(), approval.clone()),
        temp.path(),
    )
    .await;
    let replay = client.execute(missing_data, true).await;
    assert_eq!(replay["id"], original_id);
    let record_value = client.status(&original_id).await;
    let record: OperationRecord = serde_json::from_value(record_value).unwrap();
    assert_eq!(record.id, original_id);
    assert_eq!(record.status, Status::Failed);
    let effects = record
        .effects
        .expect("verified rejection has durable effects record");
    assert!(
        effects.effects.is_empty(),
        "rejected send has no financial effects"
    );
    assert_eq!(effects.evidence["httpStatus"], 400);
    assert_eq!(
        effects.evidence["nativeRejection"]["errors"][0]["id"],
        "TRAVEL_RULE_REQUIRED"
    );
    assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
    assert_eq!(proof.proofs.load(Ordering::SeqCst), 0);

    // After deliberate correction, the user supplies a new request identity
    // and explicit Travel Rule data. This is a new write with a new Coinbase
    // idempotency UUID rather than a replay of the rejected submission.
    venue.state.lock().unwrap().reject_travel_rule = false;
    let mut corrected = input();
    corrected["requestId"] = json!("withdrawal-travel-rule-corrected");
    corrected["travelRuleData"] = json!({
        "beneficiary_wallet_type":"WALLET_TYPE_SELF_HOSTED",
        "is_self":"IS_SELF_TRUE",
        "beneficiary_name":"Synthetic Test Beneficiary",
        "beneficiary_address":{"country":"US"}
    });
    let accepted = client.execute(corrected, true).await;
    assert_eq!(accepted["status"], "pending", "{accepted}");
    assert_eq!(venue.writes.load(Ordering::SeqCst), 2);
    let bodies = venue.posted_bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0].get("travel_rule_data"), None);
    assert_eq!(
        bodies[1]["travel_rule_data"]["beneficiary_wallet_type"],
        "WALLET_TYPE_SELF_HOSTED"
    );
    assert_ne!(bodies[0]["idem"], bodies[1]["idem"]);
    drop(bodies);
    assert_eq!(approval.0.load(Ordering::SeqCst), 2);
}
