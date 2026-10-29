//! Public CLI Spot lifecycle, native market/resting orders and verified chain receipts.
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
    cancels: AtomicUsize,
}

impl SpotVenue {
    fn dispatch_order(&self, request: &BinanceRequest) -> Value {
        let id = request.get("newClientOrderId").unwrap();
        assert_eq!(request.get("symbol"), Some("ETHUSDC"));
        let mut orders = self.orders.lock().unwrap();
        assert!(!orders.contains_key(id), "duplicate venue submission");
        // Only venue behavior is scripted; the public CLI, broker, authorization,
        // adapters, durable identities and settlement all use real implementation.
        let (kind, side, requested, filled, price, tif, status) = match orders.len() {
            0 => ("LIMIT", "BUY", 20_000, 20_000, 2000, Some("IOC"), "FILLED"),
            1 => ("LIMIT", "SELL", 10_000, 5_000, 2000, Some("IOC"), "EXPIRED"),
            2 => ("MARKET", "BUY", 8_000, 8_000, 2500, None, "FILLED"),
            3 => ("MARKET", "SELL", 5_000, 5_000, 1800, None, "FILLED"),
            4 => (
                "LIMIT",
                "SELL",
                6_000,
                3_000,
                2100,
                Some("GTC"),
                "PARTIALLY_FILLED",
            ),
            5 => ("LIMIT_MAKER", "SELL", 6_000, 0, 2200, None, "NEW"),
            6 => ("LIMIT", "SELL", 14_972, 14_972, 2000, Some("IOC"), "FILLED"),
            other => panic!("unexpected additional order {other}"),
        };
        assert_eq!(request.get("type"), Some(kind));
        assert_eq!(request.get("side"), Some(side));
        assert_eq!(request.get("timeInForce"), tif);
        if kind == "MARKET" {
            assert!(request.get("price").is_none());
        } else {
            assert_eq!(
                request.get("price").unwrap().parse::<i128>().unwrap(),
                price
            );
        }
        if kind == "MARKET" && side == "BUY" {
            assert_eq!(request.get("quoteOrderQty"), Some("20"));
            assert!(request.get("quantity").is_none());
        } else {
            assert_eq!(micros(request.get("quantity").unwrap()), requested);
            assert!(request.get("quoteOrderQty").is_none());
        }
        let buy = side == "BUY";
        let quote = filled * price;
        let fee = if buy { filled / 1000 } else { quote / 1000 };
        let order_id = 9007199254741000u64 + orders.len() as u64;
        let mut state = self.inner.state.lock().unwrap();
        let eth = micros(&state.balances["main"]);
        let usdc = micros(&state.usdc_balances["main"]);
        let (eth, usdc) = if buy {
            (eth + filled - fee, usdc - quote)
        } else {
            (eth - filled, usdc + quote - fee)
        };
        assert!(eth >= 0 && usdc >= 0);
        state.balances.insert("main".into(), format_micros(eth));
        state
            .usdc_balances
            .insert("main".into(), format_micros(usdc));
        let timestamp = chrono::Utc::now().timestamp_millis();
        let row = json!({"symbol":"ETHUSDC","orderId":order_id,"clientOrderId":id,"status":status,"side":side,"type":kind,"timeInForce":tif.unwrap_or("GTC"),"price":if kind == "MARKET" {"0".to_owned()} else {price.to_string()},"origQty":format_micros(requested),"origQuoteOrderQty":request.get("quoteOrderQty").unwrap_or("0"),"executedQty":format_micros(filled),"cummulativeQuoteQty":format_micros(quote),"time":timestamp,"updateTime":timestamp});
        let fills = if filled == 0 {
            json!([])
        } else {
            json!([{"symbol":"ETHUSDC","id":order_id+100,"orderId":order_id,"price":price.to_string(),"qty":format_micros(filled),"quoteQty":format_micros(quote),"commission":format_micros(fee),"commissionAsset":if buy {"ETH"} else {"USDC"},"time":timestamp,"isBuyer":buy,"isMaker":status=="PARTIALLY_FILLED"}])
        };
        orders.insert(id.into(), (row.clone(), fills));
        row
    }

    fn cancel_order(&self, request: &BinanceRequest) -> Value {
        assert_eq!(request.get("symbol"), Some("ETHUSDC"));
        let id = request
            .get("orderId")
            .expect("cancel exact immutable venue ID");
        let mut orders = self.orders.lock().unwrap();
        let (row, _) = orders
            .values_mut()
            .find(|(row, _)| row["orderId"].as_u64().unwrap().to_string() == id)
            .unwrap();
        assert!(matches!(
            row["status"].as_str(),
            Some("NEW" | "PARTIALLY_FILLED")
        ));
        row["status"] = json!("CANCELED");
        row["origClientOrderId"] = row["clientOrderId"].clone();
        row["clientOrderId"] = json!(request.get("newClientOrderId").unwrap());
        self.cancels.fetch_add(1, Ordering::SeqCst);
        row.clone()
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
            E::SpotPlaceOrder | E::SpotCancelOrder | E::StandardWithdrawal => {
                panic!("unguarded financial write")
            }
            E::SpotExchangeInfo => Ok(
                json!({"symbols":[{"symbol":"ETHUSDC","status":"TRADING","baseAsset":"ETH","quoteAsset":"USDC","isSpotTradingAllowed":true,"quoteOrderQtyMarketAllowed":true,"orderTypes":["LIMIT","MARKET","LIMIT_MAKER"],"filters":[{"filterType":"PRICE_FILTER","minPrice":"0.01","maxPrice":"1000000","tickSize":"0.01"},{"filterType":"LOT_SIZE","minQty":"0.000001","maxQty":"100","stepSize":"0.000001"},{"filterType":"MIN_NOTIONAL","minNotional":"5","applyToMarket":true}]}]}),
            ),
            E::SpotCommission => Ok(
                json!({"symbol":"ETHUSDC","standardCommission":{"maker":"0.001","taker":"0.001","buyer":"0","seller":"0"},"taxCommission":{"maker":"0","taker":"0","buyer":"0","seller":"0"},"specialCommission":{"maker":"0","taker":"0","buyer":"0","seller":"0"},"discount":{"enabledForAccount":false,"enabledForSymbol":false,"discountAsset":"BNB","discount":"0.25"}}),
            ),
            E::SpotOpenOrders => Ok(json!(
                self.orders
                    .lock()
                    .unwrap()
                    .values()
                    .filter(|(row, _)| matches!(
                        row["status"].as_str(),
                        Some("NEW" | "PARTIALLY_FILLED")
                    ))
                    .map(|(row, _)| row.clone())
                    .collect::<Vec<_>>()
            )),
            E::SpotOrder | E::SpotTrades => {
                let orders = self.orders.lock().unwrap();
                let Some(order) = orders.values().find(|(row, _)| {
                    request
                        .get("orderId")
                        .is_some_and(|id| row["orderId"].as_u64().unwrap().to_string() == id)
                        || request
                            .get("origClientOrderId")
                            .is_some_and(|id| row["clientOrderId"] == id)
                }) else {
                    // Binance replaces clientOrderId after cancellation. The original
                    // operation must recover via its persisted immutable venue ID.
                    return Err(TransportError::Rejected);
                };
                Ok(if request.endpoint == E::SpotTrades {
                    order.1.clone()
                } else {
                    order.0.clone()
                })
            }
            E::Account => {
                let mut row = self.inner.request(credential, request).await?;
                row["canTrade"] = json!(true);
                let locked: i128 = self
                    .orders
                    .lock()
                    .unwrap()
                    .values()
                    .filter(|(order, _)| {
                        matches!(order["status"].as_str(), Some("NEW" | "PARTIALLY_FILLED"))
                    })
                    .map(|(order, _)| {
                        assert_eq!(order["side"], "SELL");
                        micros(order["origQty"].as_str().unwrap())
                            - micros(order["executedQty"].as_str().unwrap())
                    })
                    .sum();
                let eth = row["balances"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|b| b["asset"] == "ETH")
                    .unwrap();
                eth["free"] = json!(format_micros(
                    micros(eth["free"].as_str().unwrap()) - locked
                ));
                eth["locked"] = json!(format_micros(locked));
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
        if request.endpoint == BinanceEndpoint::SpotCancelOrder {
            return Ok(self.cancel_order(&request));
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
        let review = challenge.review.as_ref().expect("typed financial review");
        if review.action == gofin_domain::approval_review::ApprovalAction::PlaceOrder {
            let details = review.other_details.as_deref().unwrap();
            let principal = review.input.as_ref().unwrap();
            let buy = details.contains("BUY ");
            assert_eq!(
                principal.asset.as_ref().unwrap().identifier.reference,
                if buy { "USDC" } else { "ETH" }
            );
            assert_eq!(principal.account.as_ref().unwrap().name, "main");
            assert!(
                details.contains(if buy {
                    "Deducted cost ≤: 0.00003 ETH"
                } else {
                    "Deducted cost ≤: 0.05 USDC"
                }),
                "{details}"
            );
            if details.contains("MARKET") {
                assert_eq!(
                    principal.amount.as_deref(),
                    Some(if buy { "20" } else { "0.005" })
                );
                assert!(details.contains(if buy {
                    "Quote budget: 20"
                } else {
                    "Base quantity: 0.005"
                }));
                assert!(details.contains("No price or minimum"));
                assert!(!details.contains("Limit:"));
            } else if details.contains("GTC") || details.contains("LIMIT_MAKER") {
                assert_eq!(principal.amount.as_deref(), Some("0.006"));
                assert!(
                    details
                        .to_lowercase()
                        .contains("rests until filled or canceled")
                );
                assert_eq!(
                    details.contains("Maker only"),
                    details.contains("LIMIT_MAKER")
                );
            }
        }
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
        cancels: AtomicUsize::new(0),
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
    async fn execute(
        client: &Client,
        id: &str,
        operation: &str,
        input: Value,
        status: &str,
    ) -> Value {
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
        assert_eq!(result["status"], status, "{result}");
        assert!(result["submission"].is_object());
        if status == "completed" {
            assert!(
                result["effects"].is_object(),
                "Terminal success needs a typed receipt"
            );
        }
        result
    }
    async fn read(client: &Client, operation: &str, input: Value) -> Value {
        client
            .call(
                args(&["read", operation, "--input", &input.to_string(), "--json"]),
                BrokerRequest::Read {
                    operation: operation.into(),
                    input,
                },
                true,
            )
            .await
    }
    async fn balances(client: &Client, eth: &str, held: &str, usdc: &str) {
        let result = read(client, "binance.balances", json!({"account":"main"})).await;
        let rows = result["balances"]
            .as_array()
            .expect("public observed spot balances");
        let base = rows.iter().find(|row| row["asset"] == "ETH").unwrap();
        let quote = rows.iter().find(|row| row["asset"] == "USDC").unwrap();
        assert_eq!(micros(base["free"].as_str().unwrap()), micros(eth));
        assert_eq!(micros(base["locked"].as_str().unwrap()), micros(held));
        assert_eq!(micros(quote["free"].as_str().unwrap()), micros(usdc));
        assert_eq!(micros(quote["locked"].as_str().unwrap()), 0);
    }
    fn receipt(
        record: &Value,
        spent_asset: &str,
        spent: &str,
        received_asset: &str,
        received: &str,
        fee: &str,
    ) {
        let record: gofin_domain::OperationRecord = serde_json::from_value(record.clone()).unwrap();
        let verified = record.effects.unwrap();
        assert_eq!(verified.evidence["balanceChangesVerified"], true);
        assert_eq!(
            verified.effects.len(),
            2,
            "One conversion and one exact commission"
        );
        assert!(verified.effects.iter().any(|effect| matches!(effect,
            gofin_domain::FinancialEffect::Conversion { spent: debit, received: credit, .. }
                if debit.asset.reference == spent_asset && debit.atoms.to_decimal(36) == spent
                    && credit.asset.reference == received_asset && credit.atoms.to_decimal(36) == received
        )));
        assert!(verified.effects.iter().any(|effect| matches!(effect,
            gofin_domain::FinancialEffect::Cost { amount, treatment: gofin_domain::CostTreatment::DeductedFromCredit, .. }
                if amount.asset.reference == received_asset && amount.atoms.to_decimal(36) == fee
        )));
    }
    let deposit = execute(&client,"binance-spot-deposit","binance.deposit",json!({"requestId":"binance-spot-deposit","account":"main","wallet":"synthetic-wallet","amount":"100","maxFeeEth":"0.001","coin":"USDC","network":"ARBITRUM","chainId":42161,"token":TOKEN}), "completed").await;
    balances(&client, "0", "0", "100").await;
    let mut identities = BTreeSet::from([deposit["submission"]["reference"]
        .as_str()
        .unwrap()
        .to_owned()]);
    let buy = execute(&client,"spot-buy","binance.spot.order-place",json!({"request_id":"spot-buy","account":"main","symbol":"ETHUSDC","side":"BUY","quantity":"0.02","limit_price":"2000","time_in_force":"IOC","maximum_fees":{"ETH":"0.00003"}}), "completed").await;
    receipt(&buy, "USDC", "40", "ETH", "0.02", "0.00002");
    balances(&client, "0.01998", "0", "60").await;
    let partial = execute(&client,"spot-partial-sell","binance.spot.order-place",json!({"request_id":"spot-partial-sell","account":"main","symbol":"ETHUSDC","side":"SELL","quantity":"0.01","limit_price":"2000","time_in_force":"IOC","maximum_fees":{"USDC":"0.05"}}), "completed").await;
    receipt(&partial, "ETH", "0.005", "USDC", "10", "0.01");
    balances(&client, "0.01498", "0", "69.99").await;
    let market_buy = execute(&client,"spot-market-buy","binance.spot.order-place",json!({"request_id":"spot-market-buy","account":"main","symbol":"ETHUSDC","side":"BUY","order_type":"MARKET","quote_order_quantity":"20","maximum_fees":{"ETH":"0.00003"}}), "completed").await;
    receipt(&market_buy, "USDC", "20", "ETH", "0.008", "0.000008");
    balances(&client, "0.022972", "0", "49.99").await;
    let market_sell = execute(&client,"spot-market-sell","binance.spot.order-place",json!({"request_id":"spot-market-sell","account":"main","symbol":"ETHUSDC","side":"SELL","order_type":"MARKET","quantity":"0.005","maximum_fees":{"USDC":"0.05"}}), "completed").await;
    receipt(&market_sell, "ETH", "0.005", "USDC", "9", "0.009");
    balances(&client, "0.017972", "0", "58.981").await;
    for record in [&buy, &partial, &market_buy, &market_sell] {
        assert!(
            identities.insert(
                record["submission"]["reference"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            )
        );
    }
    for (kind, id, price, venue_status, free, held, cash) in [
        (
            "LIMIT",
            "spot-resting-gtc",
            "2100",
            "PARTIALLY_FILLED",
            "0.011972",
            "0.003",
            "65.2747",
        ),
        (
            "LIMIT_MAKER",
            "spot-resting-maker",
            "2200",
            "NEW",
            "0.008972",
            "0.006",
            "65.2747",
        ),
    ] {
        let mut input = json!({"request_id":id,"account":"main","symbol":"ETHUSDC","side":"SELL","order_type":kind,"quantity":"0.006","limit_price":price,"maximum_fees":{"USDC":"0.05"}});
        if kind == "LIMIT" {
            input["time_in_force"] = json!("GTC");
        }
        let resting = execute(&client, id, "binance.spot.order-place", input, "pending").await;
        assert!(resting["effects"].is_null());
        assert!(
            identities.insert(
                resting["submission"]["reference"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            )
        );
        let operation_id = resting["id"].as_str().unwrap();
        assert!(
            Store::open(&path)
                .unwrap()
                .reservation_count(operation_id)
                .unwrap()
                > 0
        );
        balances(&client, free, held, cash).await;
        let orders = read(
            &client,
            "binance.spot.orders",
            json!({"account":"main","symbol":"ETHUSDC"}),
        )
        .await;
        let open = orders["orders"].as_array().unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0]["status"], venue_status);
        assert_eq!(open[0]["type"], kind);
        let observed = read(
            &client,
            "binance.spot.order",
            json!({"account":"main","symbol":"ETHUSDC","order_id":open[0]["orderId"]}),
        )
        .await;
        assert_eq!(
            observed["order"]["clientOrderId"],
            resting["submission"]["reference"]
        );
        assert_eq!(observed["order"]["status"], venue_status);
        let cancel_id = format!("{id}-cancel");
        let cancellation = execute(&client, &cancel_id, "binance.spot.order-cancel", json!({"request_id":cancel_id,"account":"main","symbol":"ETHUSDC","operation_id":operation_id,"original_submission":resting["submission"]}), "completed").await;
        assert!(
            identities.insert(
                cancellation["submission"]["reference"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            )
        );
        let typed: gofin_domain::OperationRecord = serde_json::from_value(cancellation).unwrap();
        assert!(matches!(
            typed.effects.unwrap().effects.as_slice(),
            [gofin_domain::FinancialEffect::SubmissionCancellation { .. }]
        ));
        balances(&client, "0.014972", "0", "65.2747").await;
        let settled = client
            .call(
                args(&["status", operation_id, "--json"]),
                BrokerRequest::Status {
                    id: operation_id.into(),
                },
                true,
            )
            .await;
        assert_eq!(settled["submission"], resting["submission"]);
        if kind == "LIMIT" {
            assert_eq!(settled["status"], "completed", "{settled}");
            receipt(&settled, "ETH", "0.003", "USDC", "6.3", "0.0063");
        } else {
            assert_eq!(settled["status"], "failed", "{settled}");
            assert_eq!(settled["effects"]["evidence"]["noFill"], true);
        }
        assert_eq!(
            Store::open(&path)
                .unwrap()
                .reservation_count(operation_id)
                .unwrap(),
            0
        );
        let orders = read(
            &client,
            "binance.spot.orders",
            json!({"account":"main","symbol":"ETHUSDC"}),
        )
        .await;
        assert!(orders["orders"].as_array().unwrap().is_empty());
    }
    let sold = execute(&client,"spot-sell-remainder","binance.spot.order-place",json!({"request_id":"spot-sell-remainder","account":"main","symbol":"ETHUSDC","side":"SELL","quantity":"all","maximum_quantity":"0.015","limit_price":"2000","time_in_force":"IOC","maximum_fees":{"USDC":"0.05"}}), "completed").await;
    receipt(&sold, "ETH", "0.014972", "USDC", "29.944", "0.029944");
    assert!(identities.insert(sold["submission"]["reference"].as_str().unwrap().to_owned()));
    balances(&client, "0", "0", "95.188756").await;
    let withdrawal = execute(&client,"spot-withdraw","binance.withdraw",json!({"requestId":"spot-withdraw","account":"main","destination":"synthetic-wallet","coin":"USDC","amount":"95.188756","network":"ARBITRUM","walletType":"SPOT","maxFee":"0","minReceiveAmount":"95.188756"}), "completed").await;
    assert!(
        identities.insert(
            withdrawal["submission"]["reference"]
                .as_str()
                .unwrap()
                .to_owned()
        )
    );
    balances(&client, "0", "0", "0").await;
    assert_eq!(
        inner.state.lock().unwrap().withdrawal_history[0]["amount"],
        "95.188756"
    );
    assert_eq!(source.accepted.load(Ordering::SeqCst), 1);
    assert_eq!(venue.orders.lock().unwrap().len(), 7);
    assert_eq!(venue.cancels.load(Ordering::SeqCst), 2);
    assert_eq!(approval.0.load(Ordering::SeqCst), 11);
    assert_eq!(identities.len(), 11);
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
    assert_eq!(token_balance["amount"]["atoms"], "95188756");
    let native_balance = balances
        .iter()
        .find(|row| row["amount"]["asset"]["reference"] == "native")
        .unwrap();
    assert_eq!(native_balance["amount"]["atoms"], "99860000000000000");
}
