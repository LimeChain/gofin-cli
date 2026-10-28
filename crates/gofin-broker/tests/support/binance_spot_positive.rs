//! Positive Binance IOC roundtrip with a shared venue ledger and chain receipts.
use super::*;
use gofin_domain::SubmissionJournal;
use gofin_evm::alloy::consensus::{Transaction, TxEnvelope, transaction::SignerRecoverable};
use gofin_evm::alloy::eips::eip2718::Decodable2718;
use gofin_evm::alloy::primitives::U256;

include!("kraken_plan_client.rs");

const TOKEN: &str = "0xaf88d065e77c8cc2239327c5edb3a432268e5831";
const PAYOUT: &str = "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const BLOCK: &str = "0xcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";

struct SpotVenue {
    inner: Arc<SyntheticVenue>,
    orders: Mutex<BTreeMap<String, (Value, Value)>>,
}

impl SpotVenue {
    fn dispatch_order(&self, request: &BinanceRequest) -> Value {
        let id = request.get("newClientOrderId").unwrap();
        assert_eq!(request.get("symbol"), Some("ETHUSDC"));
        assert_eq!(request.get("type"), Some("LIMIT"));
        assert_eq!(request.get("timeInForce"), Some("IOC"));
        assert_eq!(request.get("price"), Some("2000"));
        let buy = match request.get("side").unwrap() {
            "BUY" => true,
            "SELL" => false,
            other => panic!("unexpected side {other}"),
        };
        let quantity = micros(request.get("quantity").unwrap());
        let quote = quantity * 2000;
        let fee = if buy { quantity / 1000 } else { quote / 1000 };
        assert!(quantity > 0 && quote >= 5_000_000);
        let mut orders = self.orders.lock().unwrap();
        assert!(!orders.contains_key(id), "duplicate venue submission");
        let order_id = 9007199254741000u64 + orders.len() as u64;
        let mut state = self.inner.state.lock().unwrap();
        let eth = micros(&state.balances["main"]);
        let usdc = micros(&state.usdc_balances["main"]);
        let (eth, usdc) = if buy {
            (eth + quantity - fee, usdc - quote)
        } else {
            (eth - quantity, usdc + quote - fee)
        };
        assert!(eth >= 0 && usdc >= 0);
        state.balances.insert("main".into(), format_micros(eth));
        state
            .usdc_balances
            .insert("main".into(), format_micros(usdc));
        let timestamp = chrono::Utc::now().timestamp_millis();
        let row = json!({"symbol":"ETHUSDC","orderId":order_id,"clientOrderId":id,"status":"FILLED","side":request.get("side"),"type":"LIMIT","timeInForce":"IOC","price":"2000","origQty":format_micros(quantity),"executedQty":format_micros(quantity),"cummulativeQuoteQty":format_micros(quote),"time":timestamp,"updateTime":timestamp});
        let fills = json!([{"symbol":"ETHUSDC","id":order_id+100,"orderId":order_id,"price":"2000","qty":format_micros(quantity),"quoteQty":format_micros(quote),"commission":format_micros(fee),"commissionAsset":if buy {"ETH"} else {"USDC"},"time":timestamp,"isBuyer":buy,"isMaker":false}]);
        orders.insert(id.into(), (row.clone(), fills));
        row
    }
}

#[async_trait]
impl BinanceTransport for SpotVenue {
    async fn request(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, TransportError> {
        use BinanceEndpoint as E;
        assert_eq!(credential.as_str(), "test-only:main");
        request.validate().unwrap();
        match request.endpoint {
            E::SpotPlaceOrder | E::StandardWithdrawal => panic!("unguarded financial write"),
            E::SpotExchangeInfo => Ok(
                json!({"symbols":[{"symbol":"ETHUSDC","status":"TRADING","baseAsset":"ETH","quoteAsset":"USDC","isSpotTradingAllowed":true,"orderTypes":["LIMIT"],"filters":[{"filterType":"PRICE_FILTER","minPrice":"0.01","maxPrice":"1000000","tickSize":"0.01"},{"filterType":"LOT_SIZE","minQty":"0.000001","maxQty":"100","stepSize":"0.000001"},{"filterType":"MIN_NOTIONAL","minNotional":"5","applyToMarket":true}]}]}),
            ),
            E::SpotCommission => Ok(
                json!({"symbol":"ETHUSDC","standardCommission":{"maker":"0.001","taker":"0.001","buyer":"0","seller":"0"},"taxCommission":{"maker":"0","taker":"0","buyer":"0","seller":"0"},"specialCommission":{"maker":"0","taker":"0","buyer":"0","seller":"0"},"discount":{"enabledForAccount":false,"enabledForSymbol":false,"discountAsset":"BNB","discount":"0.25"}}),
            ),
            E::SpotOpenOrders => Ok(json!([])),
            E::SpotOrder | E::SpotTrades => {
                let orders = self.orders.lock().unwrap();
                let order = orders
                    .values()
                    .find(|(row, _)| {
                        request
                            .get("orderId")
                            .is_some_and(|id| row["orderId"].as_u64().unwrap().to_string() == id)
                            || request
                                .get("origClientOrderId")
                                .is_some_and(|id| row["clientOrderId"] == id)
                    })
                    .expect("original immutable order identity");
                Ok(if request.endpoint == E::SpotTrades {
                    order.1.clone()
                } else {
                    order.0.clone()
                })
            }
            E::Account => {
                let mut row = self.inner.request(credential, request).await?;
                row["canTrade"] = json!(true);
                Ok(row)
            }
            E::ApiRestrictions => Ok(
                json!({"enableReading":true,"enableSpotAndMarginTrading":true,"enableWithdrawals":true,"enableInternalTransfer":true,"permitsUniversalTransfer":true}),
            ),
            _ => self.inner.request(credential, request).await,
        }
    }
    async fn request_authorized(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
        journal: Arc<dyn SubmissionJournal>,
    ) -> std::result::Result<Value, TransportError> {
        journal
            .assert_authorized()
            .map_err(|_| TransportError::PermissionDenied)?;
        assert_eq!(credential.as_str(), "test-only:main");
        request.validate().unwrap();
        if request.endpoint == BinanceEndpoint::SpotPlaceOrder {
            return Ok(self.dispatch_order(&request));
        }
        assert_eq!(request.endpoint, BinanceEndpoint::StandardWithdrawal);
        assert_eq!(request.get("coin"), Some("USDC"));
        assert_eq!(request.get("network"), Some("ARBITRUM"));
        let amount = request.get("amount").unwrap();
        let mut state = self.inner.state.lock().unwrap();
        assert!(state.withdrawal_history.is_empty());
        let remaining = micros(&state.usdc_balances["main"]) - micros(amount);
        assert!(remaining >= 0);
        state
            .usdc_balances
            .insert("main".into(), format_micros(remaining));
        state.withdrawal_history.push(json!({"id":"spot-withdrawal","withdrawOrderId":request.get("withdrawOrderId"),"coin":"USDC","network":"ARBITRUM","address":request.get("address"),"amount":amount,"transactionFee":"0","status":6,"txId":PAYOUT,"transferType":0,"walletType":0,"applyTime":chrono::Utc::now().to_rfc3339()}));
        Ok(json!({"id":"spot-withdrawal"}))
    }
}

struct SpotChain {
    inner: Arc<ArbitrumErc20ExecutionRpc>,
    path: std::path::PathBuf,
}
#[async_trait]
impl EvmRpc for SpotChain {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "eth_sendRawTransaction" {
            let raw = gofin_evm::rpc_bytes(&params[0])?;
            let signed = TxEnvelope::decode_2718(&mut raw.as_ref()).unwrap();
            assert_eq!(
                gofin_evm::lower_address(signed.recover_signer().unwrap()),
                self.inner.wallet_address
            );
            assert_eq!(signed.chain_id(), Some(42161));
            assert_eq!(gofin_evm::lower_address(signed.to().unwrap()), TOKEN);
            assert_eq!(signed.value(), U256::ZERO);
            assert_eq!(
                hex::encode(signed.input()),
                format!(
                    "a9059cbb{}{}{:064x}",
                    "0".repeat(24),
                    "11".repeat(20),
                    100_000_000u64
                )
            );
            let record = Store::open(&self.path)
                .unwrap()
                .list_records()
                .unwrap()
                .into_iter()
                .find(|r| r["request_id"] == "binance-spot-deposit")
                .unwrap();
            *self.inner.journal.lock().unwrap() =
                Some(serde_json::from_value(record["submission"].clone()).unwrap());
            let result = self.inner.request(method, params).await?;
            let mut state = self.inner.venue.state.lock().unwrap();
            let credit = state.usdc_balances[EMAIL_A].clone();
            assert_eq!(credit, "100.000000");
            state.usdc_balances.insert(EMAIL_A.into(), "0".into());
            state.usdc_balances.insert("main".into(), credit);
            return Ok(result);
        }
        let payout = self
            .inner
            .venue
            .state
            .lock()
            .unwrap()
            .withdrawal_history
            .first()
            .cloned();
        let sent = self.inner.accepted.load(Ordering::SeqCst) != 0;
        if method == "eth_getBalance" && params[0] == self.inner.wallet_address {
            return Ok(json!(format!(
                "0x{:x}",
                100_000_000_000_000_000u128 - if sent { 140_000_000_000_000 } else { 0 }
            )));
        }
        if method == "eth_call"
            && params[0]["to"]
                .as_str()
                .is_some_and(|to| to.eq_ignore_ascii_case(TOKEN))
            && params[0]["data"]
                .as_str()
                .is_some_and(|data| data.starts_with("0x70a08231"))
        {
            let received = payout
                .as_ref()
                .map(|row| micros(row["amount"].as_str().unwrap()))
                .unwrap_or(0);
            let balance = if sent { 0 } else { 100_000_000 } + received;
            return Ok(json!(format!("0x{balance:064x}")));
        }
        if let Some(payout) = payout {
            let recipient = payout["address"].as_str().unwrap();
            let amount = U256::from(micros(payout["amount"].as_str().unwrap()) as u128);
            let sender = "0x3333333333333333333333333333333333333333";
            let calldata = format!(
                "0xa9059cbb{}{}{:064x}",
                "0".repeat(24),
                recipient.trim_start_matches("0x"),
                amount
            );
            if params[0] == PAYOUT && method == "eth_getTransactionByHash" {
                return Ok(
                    json!({"hash":PAYOUT,"from":sender,"to":TOKEN,"nonce":"0x1","chainId":"0xa4b1","type":"0x2","gas":"0x11170","value":"0x0","input":calldata,"maxFeePerGas":"0x77359400","maxPriorityFeePerGas":"0x989680","accessList":[],"blockHash":BLOCK,"blockNumber":"0x64"}),
                );
            }
            if params[0] == PAYOUT && method == "eth_getTransactionReceipt" {
                return Ok(
                    json!({"transactionHash":PAYOUT,"blockHash":BLOCK,"blockNumber":"0x64","status":"0x1","gasUsed":"0x11170","effectiveGasPrice":"0x77359400","logs":[{"address":TOKEN,"topics":[format!("{:#x}",gofin_evm::alloy::primitives::keccak256("Transfer(address,address,uint256)")),format!("0x{}{}","0".repeat(24),sender.trim_start_matches("0x")),format!("0x{}{}","0".repeat(24),recipient.trim_start_matches("0x"))],"data":format!("0x{amount:064x}"),"blockHash":BLOCK,"blockNumber":"0x64","transactionHash":PAYOUT,"logIndex":"0x0","removed":false}]}),
                );
            }
        }
        self.inner.request(method, params).await
    }
}

struct SpotPrices;
#[async_trait]
impl gofin_ethereum::WalletValuation for SpotPrices {
    async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
        let amount = Amount {
            asset: asset.clone(),
            atoms: units,
        };
        let (value, _, source) = self.usd_micros(&amount).await?;
        Ok((Quantity::from_decimal(&value, 0)?, source))
    }
}

struct SpotApproval(AtomicUsize);
#[async_trait]
impl Authorizer for SpotApproval {
    fn requires_financial_review(&self) -> bool {
        true
    }
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        #[cfg(target_os = "macos")]
        native_approval::assert_review_renders(
            challenge.review.clone().expect("typed review"),
            &challenge.purpose,
            &challenge.operation_id,
            challenge.binding.clone(),
            &challenge.summary,
        );
        SyntheticApproval(AtomicUsize::new(0))
            .authorize(challenge)
            .await
    }
}
#[async_trait]
impl BinancePriceProvider for SpotPrices {
    async fn quote_usd(&self, coin: &str) -> Result<PriceQuote> {
        Ok(PriceQuote {
            usd_per_unit: match coin {
                "ETH" => "2000",
                "USDC" => "1",
                _ => panic!("unexpected asset"),
            }
            .into(),
            observed_at: now(),
            source: "synthetic-spot".into(),
        })
    }
}
#[async_trait]
impl AssetValuator for SpotPrices {
    async fn usd_micros(&self, amount: &Amount) -> Result<(String, String, String)> {
        let price = if amount.asset.reference == "native" || amount.asset.reference == "ETH" {
            2_000_000_000u64
        } else {
            1_000_000
        };
        Ok((
            (amount.atoms.integer() * num_bigint::BigUint::from(price)
                / num_bigint::BigUint::from(10u8).pow(amount.asset.decimals as u32))
            .to_string(),
            now(),
            "synthetic-spot".into(),
        ))
    }
}

#[tokio::test]
#[ignore = "required positive scenario; runner supplies the actual CLI"]
async fn binance_spot_deposit_trade_withdraw_via_cli() {
    assert!(
        std::env::var_os("GOFIN_KRAKEN_PLAN_CLI").is_some(),
        "actual CLI required"
    );
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("spot.sqlite");
    let inner = Arc::new(SyntheticVenue::new());
    let venue = Arc::new(SpotVenue {
        inner: inner.clone(),
        orders: Mutex::new(BTreeMap::new()),
    });
    let signer = gofin_evm::LocalEnvelopeSigner::from_private_key(
        "0x0000000000000000000000000000000000000000000000000000000000000001",
    )
    .unwrap();
    let address = format!("{:#x}", signer.address());
    let wallet = ResourceId(format!("ethereum:42161:{address}"));
    let token = AssetId {
        network: "ethereum:42161".into(),
        reference: TOKEN.into(),
        decimals: 6,
    };
    let transaction = Arc::new(Mutex::new(None));
    let source = Arc::new(ArbitrumErc20ExecutionRpc {
        token: TOKEN.into(),
        transaction: transaction.clone(),
        hash: Mutex::new(None),
        accepted: AtomicUsize::new(0),
        venue: inner.clone(),
        journal: Arc::new(Mutex::new(None)),
        durable_store_path: None,
        delay_credit: false,
        wallet_address: address.clone(),
    });
    let chain = Arc::new(SpotChain {
        inner: source.clone(),
        path: path.clone(),
    });
    let ethereum = Arc::new(
        gofin_ethereum::EthereumAdapter::new(
            gofin_ethereum::EthereumConfig::wallet_only(42161, 1, vec![TOKEN.into()]),
            vec![gofin_ethereum::WalletConnection {
                name: "synthetic-wallet".into(),
                address: address.clone(),
                signing_enabled: true,
            }],
            Arc::new(gofin_evm::EvmRuntime::new(
                chain,
                42161,
                Some(Arc::new(RecordingSigner {
                    signer,
                    transaction,
                })),
            )),
        )
        .unwrap()
        .with_wallet_valuation(Arc::new(SpotPrices)),
    );
    let mut config = adapter_config();
    config.accounts.retain(|a| a.name == MAIN);
    config.destinations = vec![gofin_binance::EthereumDestination {
        name: "synthetic-wallet".into(),
        resource: wallet.clone(),
        address: address.clone(),
        chain_id: 42161,
        assets: BTreeMap::from([("USDC".into(), token.clone())]),
        confirmations: 1,
    }];
    let payout = Arc::new(
        gofin_broker::EthereumPayoutVerifier::new(BTreeMap::from([(42161, ethereum.clone())]))
            .unwrap(),
    );
    let binance = Arc::new(
        BinanceAdapter::new(
            config,
            venue.clone(),
            Some(Arc::new(SpotPrices)),
            Some(payout),
        )
        .unwrap(),
    );
    let deposit = Arc::new(gofin_broker::BinanceEthereumDepositAdapter::new(
        binance.clone(),
        ethereum.clone(),
    ));
    let mut config = runtime_config();
    config.policy.max_movement_usd_micros = "200000000".into();
    config.networks = gofin_broker::NetworkConfig::reviewed_evm_networks();
    config
        .connections
        .retain(|c| !matches!(c,ConnectionConfig::Binance {name,..} if name!=MAIN));
    for connection in &mut config.connections {
        if let ConnectionConfig::Ethereum {
            resource,
            network,
            address: configured,
            assets,
            ..
        } = connection
        {
            *resource = wallet.clone();
            *network = "ethereum:42161".into();
            *configured = address.clone();
            *assets = vec![
                token.clone(),
                AssetId {
                    network: "ethereum:42161".into(),
                    reference: "native".into(),
                    decimals: 18,
                },
            ];
        }
    }
    let approval = Arc::new(SpotApproval(AtomicUsize::new(0)));
    let runtime = compose_runtime(
        config,
        RuntimeDependencies {
            store: Store::open(&path).unwrap(),
            adapters: vec![binance, deposit, ethereum],
            authorizer: approval.clone(),
            valuator: Arc::new(SpotPrices),
            bridge: None,
        },
    )
    .unwrap();
    let client = Client::new(runtime, temporary.path()).await;
    async fn perform(client: &Client, id: &str, operation: &str, input: Value) -> Value {
        let result = client
            .call(
                args(&[
                    "execute",
                    operation,
                    "--id",
                    id,
                    "--input",
                    &input.to_string(),
                    "--json",
                ]),
                BrokerRequest::Execute {
                    request: OperationRequest {
                        request_id: id.into(),
                        operation: operation.into(),
                        input,
                        run_id: None,
                    },
                },
                true,
            )
            .await;
        assert_eq!(result["status"], "completed", "{result}");
        assert!(result["submission"].is_object());
        result
    }
    perform(&client,"binance-spot-deposit","binance.deposit",json!({"requestId":"binance-spot-deposit","account":"main","wallet":"synthetic-wallet","amount":"100","maxFeeEth":"0.001","coin":"USDC","network":"ARBITRUM","chainId":42161,"token":TOKEN})).await;
    assert_eq!(
        inner.state.lock().unwrap().usdc_balances["main"],
        "100.000000"
    );
    let buy=perform(&client,"spot-buy","binance.spot.order-place",json!({"request_id":"spot-buy","account":"main","symbol":"ETHUSDC","side":"BUY","quantity":"0.02","limit_price":"2000","time_in_force":"IOC","maximum_fees":{"ETH":"0.00003"}})).await;
    assert_eq!(inner.state.lock().unwrap().balances["main"], "0.019980");
    let sell=perform(&client,"spot-sell","binance.spot.order-place",json!({"request_id":"spot-sell","account":"main","symbol":"ETHUSDC","side":"SELL","quantity":"all","maximum_quantity":"0.02","limit_price":"2000","time_in_force":"IOC","maximum_fees":{"USDC":"0.05"}})).await;
    assert_ne!(
        buy["submission"]["reference"],
        sell["submission"]["reference"]
    );
    assert_eq!(inner.state.lock().unwrap().balances["main"], "0.000000");
    assert_eq!(
        inner.state.lock().unwrap().usdc_balances["main"],
        "99.920040"
    );
    perform(&client,"spot-withdraw","binance.withdraw",json!({"requestId":"spot-withdraw","account":"main","destination":"synthetic-wallet","coin":"USDC","amount":"99.92004","network":"ARBITRUM","walletType":"SPOT","maxFee":"0","minReceiveAmount":"99.92004"})).await;
    assert_eq!(
        inner.state.lock().unwrap().usdc_balances["main"],
        "0.000000"
    );
    assert_eq!(
        inner.state.lock().unwrap().withdrawal_history[0]["amount"],
        "99.92004"
    );
    assert_eq!(source.accepted.load(Ordering::SeqCst), 1);
    assert_eq!(venue.orders.lock().unwrap().len(), 2);
    assert_eq!(approval.0.load(Ordering::SeqCst), 4);
    let input =
        json!({"wallet":"synthetic-wallet","network":"evm","chain_id":42161,"tokens":[TOKEN]});
    let observed = client
        .call(
            args(&[
                "read",
                "wallet.balances",
                "--input",
                &input.to_string(),
                "--json",
            ]),
            BrokerRequest::Read {
                operation: "wallet.balances".into(),
                input,
            },
            true,
        )
        .await;
    let balances = observed["data"].as_array().expect("observed balances");
    let token_balance = balances
        .iter()
        .find(|row| row["amount"]["asset"]["reference"] == TOKEN)
        .unwrap();
    assert_eq!(token_balance["amount"]["atoms"], "99920040");
    let native_balance = balances
        .iter()
        .find(|row| row["amount"]["asset"]["reference"] == "native")
        .unwrap();
    assert_eq!(native_balance["amount"]["atoms"], "99860000000000000");
}
