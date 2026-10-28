//! Stateful venue for the combined Advanced funding and trading rehearsal.
use super::*;
use std::sync::atomic::AtomicUsize;

#[derive(Default)]
struct Ledger {
    usdc: i128,
    btc: i128,
    orders: BTreeMap<String, Value>,
    withdrawal: Option<Value>,
    writes: usize,
    receipt: Option<Value>,
}
fn units(value: &str) -> i128 {
    Quantity::from_decimal(value, 6)
        .unwrap()
        .as_str()
        .parse()
        .unwrap()
}
fn decimal(value: i128) -> String {
    Quantity::new(value.to_string()).unwrap().to_decimal(6)
}
struct LifecycleVenue {
    funding: Arc<Coinbase>,
    ledger: Mutex<Ledger>,
}
impl LifecycleVenue {
    fn response(value: Value) -> std::result::Result<CoinbaseResponse, CoinbaseError> {
        Ok(CoinbaseResponse::new(
            200,
            serde_json::to_vec(&value).unwrap(),
        ))
    }
}
#[async_trait]
impl CoinbaseApi for LifecycleVenue {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> std::result::Result<CoinbaseResponse, CoinbaseError> {
        if endpoint.mutating() {
            assert!(
                guard.as_ref().is_some_and(|g| g()),
                "financial dispatch must be authorized"
            );
        } else {
            assert!(guard.is_none());
        }
        match endpoint {
            CoinbaseRoute::KeyPermissions => Self::response(
                json!({"portfolio_uuid":"portfolio-uuid","can_view":true,"can_trade":true,"can_transfer":true}),
            ),
            CoinbaseRoute::Accounts { .. } => {
                let state = self.ledger.lock().unwrap();
                let held = state.orders.values().any(|o| o["status"] == "OPEN");
                Self::response(
                    json!({"accounts":[{"uuid":"00000000-0000-4000-8000-000000000002","currency":"USDC","portfolio_id":"portfolio-uuid","active":true,"ready":true,"available_balance":{"value":decimal(state.usdc),"currency":"USDC"},"hold":{"value":"0","currency":"USDC"}},{"uuid":"btc-account","currency":"BTC","portfolio_id":"portfolio-uuid","active":true,"ready":true,"available_balance":{"value":decimal(state.btc - if held {units("0.05")} else {0}),"currency":"BTC"},"hold":{"value":if held {"0.05"} else {"0"},"currency":"BTC"}}],"has_next":false}),
                )
            }
            CoinbaseRoute::Account { ref account_id }
                if account_id == "00000000-0000-4000-8000-000000000002"
                    || account_id == "btc-account" =>
            {
                let state = self.ledger.lock().unwrap();
                let (currency, balance) = if account_id == "00000000-0000-4000-8000-000000000002" {
                    ("USDC", state.usdc)
                } else {
                    ("BTC", state.btc)
                };
                Self::response(
                    json!({"account":{"uuid":account_id,"currency":currency,"active":true,"ready":true,"available_balance":{"value":decimal(balance),"currency":currency},"hold":{"value":"0","currency":currency}}}),
                )
            }
            CoinbaseRoute::LegacyAccounts { .. } => Self::response(
                json!({"data":[{"id":"00000000-0000-4000-8000-000000000002","currency":{"code":"USDC"}},{"id":"btc-account","currency":{"code":"BTC"}}],"pagination":{"next_starting_after":null}}),
            ),
            CoinbaseRoute::Product { product_id } => {
                assert_eq!(product_id, "BTC-USDC");
                Self::response(
                    json!({"product":{"product_type":"SPOT","trading_disabled":false,"base_currency_id":"BTC","quote_currency_id":"USDC","base_increment":"0.001","quote_increment":"0.01","price_increment":"0.01","base_min_size":"0.001","quote_min_size":"0.01"}}),
                )
            }
            CoinbaseRoute::HistoricalOrders { .. } => {
                Self::response(json!({"orders":[],"has_next":false}))
            }
            CoinbaseRoute::OrderPreview => {
                let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
                let (qty, price) = order_terms(&body);
                let value = qty * price / 1_000_000;
                let fee = value * 2 / 1000;
                Self::response(
                    json!({"base_size":decimal(qty),"quote_size":decimal(value),"order_total":decimal(value),"commission_total":decimal(fee),"errs":[]}),
                )
            }
            CoinbaseRoute::Orders => {
                let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
                let (qty, price) = order_terms(&body);
                let value = qty * price / 1_000_000;
                let fee = value * 2 / 1000;
                let side = body["side"].as_str().unwrap();
                let resting = price == units("120");
                let mut s = self.ledger.lock().unwrap();
                s.writes += 1;
                let id = format!("advanced-order-{}", s.writes);
                if !resting {
                    if side == "BUY" {
                        s.usdc -= value + fee;
                        s.btc += qty;
                    } else {
                        s.usdc += value - fee;
                        s.btc -= qty;
                    }
                    assert!(s.usdc >= 0 && s.btc >= 0);
                }
                let order = json!({"order_id":id,"client_order_id":body["client_order_id"],"product_id":"BTC-USDC","side":side,"status":if resting {"OPEN"} else {"FILLED"},"size_in_quote":false,"filled_size":if resting {"0".into()} else {decimal(qty)},"filled_value":if resting {"0".into()} else {decimal(value)},"total_fees":if resting {"0".into()} else {decimal(fee)},"settled":!resting,"price":decimal(price)});
                s.orders.insert(id.clone(), order);
                Self::response(json!({"success":true,"success_response":{"order_id":id}}))
            }
            CoinbaseRoute::Order { order_id } => {
                Self::response(json!({"order":self.ledger.lock().unwrap().orders[&order_id]}))
            }
            CoinbaseRoute::Fills { query } => {
                let id = query
                    .order_ids
                    .first()
                    .cloned()
                    .or(query.order_id)
                    .unwrap_or_default();
                let s = self.ledger.lock().unwrap();
                let o = &s.orders[&id];
                let fills = if o["filled_size"] == "0" {
                    vec![]
                } else {
                    vec![
                        json!({"entry_id":format!("fill-{id}"),"order_id":id,"product_id":"BTC-USDC","side":o["side"],"price":o["price"],"size":o["filled_size"],"size_in_quote":false,"commission":o["total_fees"]}),
                    ]
                };
                Self::response(json!({"fills":fills,"has_next":false}))
            }
            CoinbaseRoute::OrderCancel => {
                let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
                let id = body["order_ids"][0].as_str().unwrap();
                let mut s = self.ledger.lock().unwrap();
                s.writes += 1;
                let o = s.orders.get_mut(id).unwrap();
                assert_eq!(o["status"], "OPEN");
                o["status"] = json!("CANCELLED");
                o["settled"] = json!(true);
                Self::response(json!({"results":[{"success":true,"order_id":id}]}))
            }
            CoinbaseRoute::TransactionSend { account_id } => {
                assert_eq!(account_id, "00000000-0000-4000-8000-000000000002");
                let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
                assert_eq!(body["type"], "send");
                assert_eq!(body["currency"], "USDC");
                assert_eq!(body["network"], "base");
                assert_eq!(body["to"], WALLET);
                assert!(uuid::Uuid::parse_str(body["idem"].as_str().unwrap()).is_ok());
                let amount = units(body["amount"].as_str().unwrap());
                let fee = units("0.1");
                let mut s = self.ledger.lock().unwrap();
                assert!(s.usdc >= amount + fee);
                s.usdc -= amount + fee;
                s.writes += 1;
                s.withdrawal = Some(body.clone());
                Self::response(
                    json!({"data":{"id":"withdrawal-1","type":"send","idem":body["idem"]}}),
                )
            }
            CoinbaseRoute::Transaction {
                account_id,
                transaction_id,
            } => {
                assert_eq!(account_id, "00000000-0000-4000-8000-000000000002");
                assert_eq!(transaction_id, "withdrawal-1");
                let s = self.ledger.lock().unwrap();
                let body = s.withdrawal.as_ref().unwrap();
                let amount = body["amount"].as_str().unwrap();
                Self::response(
                    json!({"data":{"id":"withdrawal-1","type":"send","idem":body["idem"],"status":"completed","amount":{"amount":format!("-{}", decimal(units(amount)+units("0.1"))),"currency":"USDC"},"to":{"address":WALLET},"network":{"network_name":"base","hash":format!("0x{}","aa".repeat(32)),"transaction_amount":{"amount":amount,"currency":"USDC"},"transaction_fee":{"amount":"0.1","currency":"USDC"}}}}),
                )
            }
            CoinbaseRoute::Transactions { ref account_id, .. }
                if self.ledger.lock().unwrap().withdrawal.is_some() =>
            {
                assert_eq!(account_id, "00000000-0000-4000-8000-000000000002");
                let s = self.ledger.lock().unwrap();
                let body = s.withdrawal.as_ref().unwrap();
                Self::response(
                    json!({"data":[{"id":"withdrawal-1","type":"send","idem":body["idem"]}],"pagination":{"next_starting_after":null}}),
                )
            }
            _ => self.funding.request(endpoint, request, guard).await,
        }
    }
}
fn order_terms(body: &Value) -> (i128, i128) {
    assert_eq!(body["product_id"], "BTC-USDC");
    let cfg = &body["order_configuration"];
    let c = cfg
        .get("limit_limit_fok")
        .or_else(|| cfg.get("limit_limit_gtc"))
        .or_else(|| cfg.get("market_market_ioc"))
        .unwrap();
    let size = c["base_size"].as_str().unwrap();
    let price = c["limit_price"].as_str().unwrap_or("100");
    (units(size), units(price))
}

struct LifecycleProof {
    venue: Arc<LifecycleVenue>,
    proofs: AtomicUsize,
}
#[async_trait]
impl gofin_coinbase::CoinbaseRecipientVerifier for LifecycleProof {
    async fn resolve(
        &self,
        network: &str,
        coin: &str,
        contract: Option<&str>,
        address: &str,
        name: Option<&str>,
    ) -> Result<Option<gofin_coinbase::CoinbaseDestination>> {
        if (network, coin, contract, address, name)
            != ("base", "USDC", Some(SOURCE), WALLET, Some("wallet"))
        {
            return Ok(None);
        }
        Ok(Some(gofin_coinbase::CoinbaseDestination {
            enrolled: true,
            resource: ResourceId(format!("ethereum:8453:{WALLET}")),
            address: WALLET.into(),
            chain_id: 8453,
            asset: AssetId {
                network: "ethereum:8453".into(),
                reference: SOURCE.into(),
                decimals: 6,
            },
            confirmations: 1,
            network: "base".into(),
        }))
    }
    async fn verify(
        &self,
        request: gofin_coinbase::CoinbasePayoutRequest,
    ) -> Result<gofin_coinbase::CoinbasePayoutResult> {
        self.proofs.fetch_add(1, Ordering::SeqCst);
        let s = self.venue.ledger.lock().unwrap();
        let receipt = s.receipt.as_ref();
        let valid = receipt.is_some_and(|r| {
            r["status"] == "0x1"
                && r["transactionHash"] == request.tx_hash
                && r["to"] == request.destination.address
                && r["tokenContract"] == request.destination.asset.reference
                && r["chainId"] == request.destination.chain_id
                && r["amountAtoms"]
                    == Quantity::from_decimal(&request.minimum, 6)
                        .unwrap()
                        .as_str()
                && request.minimum == request.maximum
        });
        Ok(gofin_coinbase::CoinbasePayoutResult {
            status: if valid {
                Status::Completed
            } else {
                Status::Pending
            },
            amount: valid.then(|| Quantity::from_decimal(&request.minimum, 6).unwrap()),
            evidence: json!({"source":"independent synthetic Base receipt","receipt":receipt}),
        })
    }
}
struct LifecycleApproval(AtomicUsize);
#[async_trait]
impl Authorizer for LifecycleApproval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        assert_eq!(challenge.purpose, "financial_write");
        assert!(challenge.binding.is_object());
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "synthetic lifecycle approval".into(),
            source: None,
            yolo_provenance: None,
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires GOFIN_COINBASE_PLAN_CLI built with synthetic-coinbase-build"]
async fn positive_coinbase_advanced_funding_trading_withdrawal_via_cli() {
    let cli = std::env::var_os("GOFIN_COINBASE_PLAN_CLI").expect("public CLI required");
    assert!(std::path::Path::new(&cli).is_file());
    let temp = tempfile::tempdir().unwrap();
    let (_, funding, rpc, ethereum) = fixture_parts(Some("base"), None, DESTINATION, "base", 8453);
    rpc.token_balance.store(150, Ordering::SeqCst);
    *funding.account_id.lock().unwrap() = "00000000-0000-4000-8000-000000000002".into();
    let venue = Arc::new(LifecycleVenue {
        funding: funding.clone(),
        ledger: Mutex::new(Ledger::default()),
    });
    let http = coinbase_http_fixture::Fixture::new(venue.clone()).await;
    let proof = Arc::new(LifecycleProof {
        venue: venue.clone(),
        proofs: AtomicUsize::new(0),
    });
    let profile = Arc::new(
        CoinbaseProfile::new(
            vec![CoinbaseAccount {
                name: "coinbase".into(),
                resource: ResourceId("coinbase:portfolio:portfolio-uuid".into()),
                credential_ref: "synthetic".into(),
                physical_identity: "portfolio-uuid".into(),
                identity_verified: true,
                portfolio_id: Some("portfolio-uuid".into()),
                product: CoinbaseProduct::Advanced,
            }],
            BTreeMap::from([("synthetic".into(), http.api.clone() as Arc<dyn CoinbaseApi>)]),
        )
        .unwrap()
        .with_prices(Arc::new(Price))
        .with_recipient_verifier(proof.clone()),
    );
    let deposit = Arc::new(
        CoinbaseBaseDepositAdapter::with_networks(profile.clone(), vec![ethereum]).unwrap(),
    );
    let exchange = Arc::new(gofin_exchange::ExchangeAdapter::new((*profile).clone()));
    let approval = Arc::new(LifecycleApproval(AtomicUsize::new(0)));
    let mut policy = BrokerPolicy::beta();
    policy.max_movement_usd_micros = "300000000".into();
    policy.max_run_allocation_usd_micros = "500000000".into();
    let runtime = compose_runtime_with_clock(
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
            store: Store::open(temp.path().join("lifecycle.sqlite")).unwrap(),
            adapters: vec![deposit, exchange],
            authorizer: approval.clone(),
            valuator: Arc::new(SharedUsdValuator::with_source(Arc::new(Price))),
            bridge: None,
        },
        Arc::new(TestClock),
    )
    .unwrap();
    let client = Client::new(runtime, temp.path()).await;
    async fn execute(client: &Client, op: &str, input: Value) -> Value {
        let id = input["requestId"].as_str().unwrap().to_owned();
        let preview = client
            .call(
                vec![
                    "preview".into(),
                    op.into(),
                    "--input".into(),
                    input.to_string(),
                    "--json".into(),
                ],
                BrokerRequest::Preview {
                    operation: op.into(),
                    input: input.clone(),
                },
                true,
            )
            .await;
        #[cfg(target_os = "macos")]
        native_approval::assert_prepared_renders(&serde_json::from_value(preview).unwrap());
        client
            .call(
                vec![
                    "execute".into(),
                    op.into(),
                    "--input".into(),
                    input.to_string(),
                    "--id".into(),
                    id.clone(),
                    "--json".into(),
                ],
                BrokerRequest::Execute {
                    request: OperationRequest {
                        request_id: id,
                        operation: op.into(),
                        input,
                        run_id: None,
                    },
                },
                true,
            )
            .await
    }
    let input = json!({"account":"coinbase","wallet":"wallet","network":"base","accountId":"00000000-0000-4000-8000-000000000002","addressId":"base-address","amount":"100","requestId":"deposit","maxFeeEth":"0.001"});
    let deposited = execute(&client, "coinbase.deposit", input).await;
    assert_eq!(deposited["status"], "pending", "{deposited}");
    funding.credited.store(true, Ordering::SeqCst);
    *funding.hash.lock().unwrap() = rpc.hash.lock().unwrap().clone();
    *funding.receive_amount.lock().unwrap() = "100".into();
    *funding.credit_balance.lock().unwrap() = "100".into();
    venue.ledger.lock().unwrap().usdc = units("100");
    let done = client.status(deposited["id"].as_str().unwrap()).await;
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(rpc.submissions.load(Ordering::SeqCst), 1);
    for (id, side, size, price, kind, status) in [
        ("buy", "BUY", "0.1", "100", "limit_limit_fok", "completed"),
        (
            "part",
            "SELL",
            "0.05",
            "100",
            "market_market_ioc",
            "completed",
        ),
        ("rest", "SELL", "0.05", "120", "limit_limit_gtc", "pending"),
        (
            "final",
            "SELL",
            "0.05",
            "100",
            "market_market_ioc",
            "completed",
        ),
    ] {
        let mut config = json!({"base_size":size});
        if kind != "market_market_ioc" {
            config["limit_price"] = json!(price);
        }
        let input = json!({"account":"coinbase","requestId":id,"productId":"BTC-USDC","side":side,"orderConfiguration":{kind:config},"maximumSpend":if side=="BUY" {"10.1"} else {size},"minimumReceive":if side=="BUY" {size} else {"4.9"},"maximumFee":"0.03"});
        let order = execute(&client, "coinbase.create-order", input).await;
        assert_eq!(order["status"], status, "{order}");
        if id == "rest" {
            let row = venue
                .ledger
                .lock()
                .unwrap()
                .orders
                .values()
                .find(|o| o["status"] == "OPEN")
                .unwrap()
                .clone();
            let cancel = json!({"account":"coinbase","requestId":"cancel","venueOrderId":row["order_id"],"clientOrderId":row["client_order_id"],"productId":"BTC-USDC"});
            let result = execute(&client, "coinbase.cancel-order", cancel).await;
            assert_eq!(result["status"], "completed", "{result}");
            let parent = client.status(order["id"].as_str().unwrap()).await;
            assert!(
                parent["status"] == "failed" || parent["status"] == "completed",
                "{parent}"
            );
        }
    }
    assert_eq!(venue.ledger.lock().unwrap().btc, 0);
    assert_eq!(venue.ledger.lock().unwrap().usdc, units("99.96"));
    let input = json!({"account":"coinbase","accountId":"00000000-0000-4000-8000-000000000002","destination":"wallet","address":WALLET,"network":"base","amount":"99.86","feeMode":"venue_calculated","feeReserveUsdc":"0.1","requestId":"withdraw"});
    let withdrawal = execute(&client, "coinbase.withdraw", input).await;
    assert!(
        withdrawal["status"] == "pending" || withdrawal["status"] == "unknown",
        "withdrawal must await independent receipt: {withdrawal}"
    );
    assert!(withdrawal["effects"].is_null());
    assert_eq!(
        venue.ledger.lock().unwrap().withdrawal.as_ref().unwrap()["amount"],
        "99.86"
    );
    venue.ledger.lock().unwrap().receipt = Some(
        json!({"status":"0x1","transactionHash":format!("0x{}","aa".repeat(32)),"to":WALLET,"tokenContract":SOURCE,"chainId":8453,"amountAtoms":"99860000"}),
    );
    let done = client.status(withdrawal["id"].as_str().unwrap()).await;
    assert_eq!(done["status"], "completed", "{done}");
    assert!(proof.proofs.load(Ordering::SeqCst) > 0);
    assert_eq!(venue.ledger.lock().unwrap().usdc, 0);
    assert_eq!(venue.ledger.lock().unwrap().writes, 6);
    assert_eq!(approval.0.load(Ordering::SeqCst), 7);
}
