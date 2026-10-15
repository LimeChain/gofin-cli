//! Public broker withdrawal rehearsal through the actual Coinbase profile,
//! controlled Coinbase transport, durable store and an independent Base proof.
use async_trait::async_trait;
use fin_broker::{
    BrokerConfigV2, BrokerPolicy, NetworkConfig, RuntimeDependencies, SharedUsdValuator,
    compose_runtime_with_clock,
};
use fin_coinbase::{
    CoinbaseAccount, CoinbaseDestination, CoinbasePayoutRequest, CoinbasePayoutResult,
    CoinbaseProduct, CoinbaseProfile, CoinbaseRecipientVerifier,
};
use fin_domain::{
    AssetId, CostTreatment, FinancialEffect, OperationRecord, ResourceId, Status, clock::Clock,
};
use fin_exchange::{ExchangeAdapter, coinbase::*};
use fin_pricing::{UsdPriceSource, UsdQuote};
use fin_protocol::BrokerRequest;
use fin_store::Store;
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

const PORTFOLIO: &str = "00000000-0000-4000-8000-000000000001";
const ACCOUNT_ID: &str = "00000000-0000-4000-8000-000000000002";
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
impl fin_domain::Authorizer for Approval {
    async fn authorize(
        &self,
        challenge: &fin_domain::AuthorizationChallenge,
    ) -> fin_domain::Result<fin_domain::ApprovalAudit> {
        assert!(challenge.binding.to_string().contains("coinbase.withdraw"));
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(fin_domain::ApprovalAudit {
            mechanism: "synthetic Coinbase withdrawal approval".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: fin_domain::now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}
struct Prices;
#[async_trait]
impl UsdPriceSource for Prices {
    async fn quote(&self, asset: &AssetId) -> fin_domain::Result<UsdQuote> {
        assert_eq!(asset.reference, "USDC");
        Ok(UsdQuote {
            usd_per_unit: "1".into(),
            updated_at: fin_domain::now(),
            observed_at: fin_domain::now(),
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
}
#[derive(Default)]
struct Venue {
    writes: AtomicUsize,
    state: Mutex<TransferState>,
    posted_body: Mutex<Option<Value>>,
    posted_bodies: Mutex<Vec<Value>>,
}
impl Venue {
    fn response(body: Value) -> Result<CoinbaseResponse, CoinbaseError> {
        Ok(CoinbaseResponse::new(
            200,
            serde_json::to_vec(&body).unwrap(),
        ))
    }
    fn transaction(&self) -> Value {
        let state = self.state.lock().unwrap();
        let mut network = json!({
            "network_name":"base", "hash":TX_HASH,
            "transaction_amount":{"amount":"4.000000","currency":"USDC"}
        });
        if state.include_fee {
            network["transaction_fee"] = json!({"amount":"0.300000","currency":"USDC"});
        }
        json!({"data":{
            "id":VENUE_TX,"type":"send","idem":self.posted_body.lock().unwrap().as_ref().unwrap()["idem"],
            "status":if state.completed {"completed"} else {"pending"},
            "amount":{"amount":if state.include_fee {"-4.300000"} else {"-4.000000"},"currency":"USDC"},
            "to":{"address":WALLET},"network":network
        }})
    }
    fn source_balance(&self) -> String {
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
                json!({"portfolio_uuid":PORTFOLIO,"can_view":true,"can_trade":false,"can_transfer":true}),
            ),
            CoinbaseRoute::LegacyAccounts { .. } => Self::response(
                json!({"data":[{"id":ACCOUNT_ID,"currency":{"code":"USDC"}}],"pagination":{"next_starting_after":null}}),
            ),
            CoinbaseRoute::Accounts { .. } => Self::response(
                json!({"accounts":[{"uuid":ACCOUNT_ID,"currency":"USDC","portfolio_id":PORTFOLIO,"active":true,"ready":true,"available_balance":{"value":self.source_balance(),"currency":"USDC"},"hold":{"value":"0","currency":"USDC"}}],"has_next":false}),
            ),
            CoinbaseRoute::LegacyCurrencies => Self::response(json!({"data":[{"id":"USD"}]})),
            CoinbaseRoute::LegacyCryptoCurrencies => {
                Self::response(json!({"data":[{"id":"USDC"}]}))
            }
            CoinbaseRoute::HistoricalOrders { .. } => {
                Self::response(json!({"orders":[],"has_next":false}))
            }
            CoinbaseRoute::Account { account_id } => {
                assert_eq!(account_id, ACCOUNT_ID);
                Self::response(
                    json!({"account":{"uuid":ACCOUNT_ID,"currency":"USDC","active":true,"ready":true,"available_balance":{"value":self.source_balance(),"currency":"USDC"}}}),
                )
            }
            CoinbaseRoute::TransactionSend { account_id } => {
                assert_eq!(account_id, ACCOUNT_ID);
                let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
                assert_eq!(body["type"], "send");
                assert_eq!(body["currency"], "USDC");
                assert_eq!(body["network"], "base");
                assert_eq!(body["to"], WALLET);
                assert_eq!(body["amount"], "4.000000");
                assert!(uuid::Uuid::parse_str(body["idem"].as_str().unwrap()).is_ok());
                *self.posted_body.lock().unwrap() = Some(body.clone());
                self.posted_bodies.lock().unwrap().push(body.clone());
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
                let transaction = self.state.lock().unwrap().transaction.clone();
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
    ) -> fin_domain::Result<Option<CoinbaseDestination>> {
        assert_eq!(
            (network, coin, contract, address, name),
            ("base", "USDC", Some(BASE_USDC), WALLET, Some("evm-dev"))
        );
        Ok(Some(CoinbaseDestination {
            enrolled: true,
            resource: ResourceId("ethereum:8453:evm-dev".into()),
            address: WALLET.into(),
            chain_id: 8453,
            asset: AssetId {
                network: "ethereum:8453".into(),
                reference: BASE_USDC.into(),
                decimals: 6,
            },
            confirmations: 1,
            network: "base".into(),
        }))
    }
    async fn verify(
        &self,
        request: CoinbasePayoutRequest,
    ) -> fin_domain::Result<CoinbasePayoutResult> {
        self.proofs.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.tx_hash, TX_HASH);
        assert_eq!(request.destination.address, WALLET);
        assert_eq!(request.destination.asset.reference, BASE_USDC);
        assert_eq!(request.destination.chain_id, 8453);
        let receipt = self.venue.state.lock().unwrap().receipt.clone();
        let exact_amount = receipt.as_ref().and_then(|r| r["amountAtoms"].as_str());
        let correct = receipt.as_ref().is_some_and(|r| {
            r["status"] == "0x1"
                && r["transactionHash"] == request.tx_hash
                && r["to"] == request.destination.address
                && r["tokenContract"] == request.destination.asset.reference
                && r["chainId"] == 8453
                && exact_amount == Some("4000000")
                && request.minimum == "4.000000"
                && request.maximum == "4.000000"
        });
        if correct {
            self.venue.state.lock().unwrap().wallet_credited = "4.000000".into();
        }
        Ok(CoinbasePayoutResult {
            status: if correct {
                Status::Completed
            } else {
                Status::Pending
            },
            amount: correct.then(|| fin_domain::Quantity::new(exact_amount.unwrap()).unwrap()),
            evidence: json!({"source":"independent synthetic Base receipt observer","receipt":receipt,"confirmations":1}),
        })
    }
}

fn make_runtime(
    path: &std::path::Path,
    venue: Arc<Venue>,
    proof: Arc<IndependentBaseProof>,
    approval: Arc<Approval>,
) -> Arc<fin_broker::BrokerRuntime> {
    let transport: Arc<dyn CoinbaseApi> = venue;
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
    let adapter = Arc::new(ExchangeAdapter::new(profile));
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

fn input() -> Value {
    json!({"account":"coinbase-dev","accountId":ACCOUNT_ID,"destination":"evm-dev","address":WALLET,"network":"base","amount":"4.000000","feeMode":"venue_calculated","feeReserveUsdc":"0.100000","requestId":"synthetic-base-usdc-withdrawal"})
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
    assert!(verified.effects.iter().any(|effect| matches!(effect, FinancialEffect::Transfer {from,to,debit,credit} if from.0 == SOURCE && to.0 == "ethereum:8453:evm-dev" && debit.asset.reference == "USDC" && credit.asset.reference == BASE_USDC && debit.atoms.to_decimal(36) == "4" && credit.atoms.to_decimal(6) == "4")));
    assert!(verified.effects.iter().any(|effect| matches!(effect, FinancialEffect::Cost {amount,treatment:CostTreatment::VenueCalculatedAdditionalDebit,..} if amount.asset.reference == "USDC" && amount.atoms.to_decimal(36) == "0.3")));
    assert_eq!(verified.evidence["feeWithinReserve"], false);
    assert_eq!(verified.evidence["actualFeeUsdc"], "0.300000");
    assert_eq!(verified.evidence["signedSourceDebitUsdc"], "-4.300000");
    assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
    assert!(proof.proofs.load(Ordering::SeqCst) >= 1);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(venue.source_balance(), "5.700000");
    assert_eq!(venue.state.lock().unwrap().wallet_credited, "4.000000");
    let snapshot = client.runtime.handle(BrokerRequest::Snapshot).await;
    assert!(snapshot.ok, "{:?}", snapshot.error);
    let snapshot: fin_domain::StateSnapshot =
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
