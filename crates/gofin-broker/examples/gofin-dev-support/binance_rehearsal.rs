//! Public-CLI USD-M rehearsal: real adapter, isolated persisted exchange transport.
//! Compiled only into the development example; never reads credentials or uses HTTP.
use async_trait::async_trait;
use gofin_binance::{
    AccountRole, BinanceAccount, BinanceAdapter, BinanceConfig, BinanceEndpoint as E,
    BinancePriceProvider, BinanceRequest, BinanceTransport, CredentialHandle, PriceQuote,
    TransportError,
};
use gofin_broker::{
    AssetValuator, BrokerConfigV2, BrokerPolicy, ConnectionConfig, DeploymentConfig, NetworkConfig,
    ProtectedApplications,
};
use gofin_domain::{Amount, GoFinError, Quantity, ResourceId, Result, now};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const ACCOUNT: &str = "binance-rehearsal";
const SUBACCOUNT: &str = "binance-subaccount-rehearsal";
const RESOURCE: &str = "binance:uid:7007:SPOT";
const CREDENTIAL: &str = "synthetic-binance-only";

pub fn applications(
    directory: &Path,
    standalone_subaccount: bool,
) -> Result<(BrokerConfigV2, ProtectedApplications)> {
    let account = if standalone_subaccount {
        SUBACCOUNT
    } else {
        ACCOUNT
    };
    let path = directory.join("binance-fixture.json");
    let mut state: State = if path.exists() {
        serde_json::from_slice(&fs::read(&path).map_err(fixture_error)?).map_err(fixture_error)?
    } else {
        State::default()
    };
    // Response-loss evidence becomes visible only after restarting this fixture.
    state.proof_visible = true;
    state.transfer_proof_visible = true;
    let venue = Arc::new(Venue {
        path,
        standalone_subaccount,
        state: Mutex::new(state),
    });
    venue.save(&*venue.state.lock().map_err(fixture_error)?)?;
    let adapter = Arc::new(BinanceAdapter::new(
        BinanceConfig {
            accounts: vec![BinanceAccount {
                name: account.into(),
                resource: ResourceId(RESOURCE.into()),
                credential: CredentialHandle::new(CREDENTIAL).map_err(fixture_error)?,
                role: if standalone_subaccount {
                    AccountRole::Sub
                } else {
                    AccountRole::Main
                },
                parent: None,
                email: None,
                wallet_type: "SPOT".into(),
            }],
            destinations: vec![],
        },
        venue,
        Some(Arc::new(Prices)),
        None,
    )?);
    let config = BrokerConfigV2 {
        version: 2,
        networks: NetworkConfig::reviewed_evm_networks(),
        deployments: vec![DeploymentConfig::ethereum_mainnet()],
        connections: vec![ConnectionConfig::Binance {
            name: account.into(),
            resource: ResourceId(RESOURCE.into()),
            credential_ref: "00000000-0000-4000-8000-000000007007".into(),
            role: if standalone_subaccount { "sub" } else { "main" }.into(),
            parent: None,
            email: None,
            wallet_type: "SPOT".into(),
            physical_identity: "7007".into(),
            physical_identity_verified: true,
        }],
        policy: BrokerPolicy::beta(),
        approver_uid: unsafe { libc::geteuid() }.max(501),
        extensions: Default::default(),
    };
    Ok((
        config,
        ProtectedApplications {
            adapters: vec![adapter],
            valuator: Arc::new(Prices),
            bridge: None,
        },
    ))
}
fn fixture_error(_: impl std::fmt::Display) -> GoFinError {
    GoFinError::validation("Synthetic Binance fixture state is unavailable or invalid.")
}

#[derive(Serialize, Deserialize)]
struct State {
    spot: String,
    futures: String,
    position: String,
    leverage: u32,
    margin_type: String,
    posts: u64,
    order_posts: u64,
    transfer_posts: u64,
    orders: BTreeMap<String, Value>,
    fills: BTreeMap<String, Value>,
    transfers: Vec<Value>,
    lost_response_injected: bool,
    #[serde(default)]
    suppress_faults: bool,
    proof_visible: bool,
    #[serde(default)]
    transfer_proof_visible: bool,
    #[serde(default)]
    wallet_transfer_posts: u64,
    #[serde(default)]
    universal_transfer_posts: u64,
    #[serde(default)]
    lost_transfer_response_injected: bool,
}
impl Default for State {
    fn default() -> Self {
        Self {
            spot: "100".into(),
            futures: "0".into(),
            position: "0".into(),
            leverage: 1,
            margin_type: "CROSSED".into(),
            posts: 0,
            order_posts: 0,
            transfer_posts: 0,
            orders: BTreeMap::new(),
            fills: BTreeMap::new(),
            transfers: vec![],
            lost_response_injected: false,
            suppress_faults: false,
            proof_visible: true,
            transfer_proof_visible: true,
            wallet_transfer_posts: 0,
            universal_transfer_posts: 0,
            lost_transfer_response_injected: false,
        }
    }
}
struct Venue {
    path: PathBuf,
    standalone_subaccount: bool,
    state: Mutex<State>,
}
impl Venue {
    fn save(&self, state: &State) -> Result<()> {
        let staging = self.path.with_extension("tmp");
        fs::write(
            &staging,
            serde_json::to_vec_pretty(state).map_err(fixture_error)?,
        )
        .map_err(fixture_error)?;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o600)).map_err(fixture_error)?;
        fs::rename(staging, &self.path).map_err(fixture_error)
    }
}
fn units(value: &str) -> i128 {
    let negative = value.starts_with('-');
    let magnitude = Quantity::from_decimal(value.trim_start_matches('-'), 6)
        .expect("synthetic exact decimal")
        .as_str()
        .parse::<i128>()
        .expect("small synthetic amount");
    if negative { -magnitude } else { magnitude }
}
fn decimal(value: i128) -> String {
    let amount = Quantity::new(value.unsigned_abs().to_string())
        .unwrap()
        .to_decimal(6);
    if value < 0 {
        format!("-{amount}")
    } else {
        amount
    }
}
#[async_trait]
impl BinanceTransport for Venue {
    async fn request(
        &self,
        credential: &CredentialHandle,
        r: BinanceRequest,
    ) -> std::result::Result<Value, TransportError> {
        assert_eq!(
            credential.as_str(),
            CREDENTIAL,
            "fixture must never receive imported credentials"
        );
        r.validate()
            .expect("adapter requests must use reviewed endpoint parameters");
        let mut s = self.state.lock().unwrap();
        let p = |key| r.get(key).expect("required synthetic request field");
        let time = chrono::Utc::now().timestamp_millis();
        let position_margin =
            (units(&s.position).abs() * 2000 + i128::from(s.leverage) - 1) / i128::from(s.leverage);
        let response = match r.endpoint {
            E::Account => {
                json!({"uid":7007,"accountType":"SPOT","canTrade":true,"balances":[{"asset":"USDT","free":s.spot,"locked":"0"}]})
            }
            E::ApiRestrictions => {
                json!({"enableReading":true,"enableFutures":true,"enableSpotAndMarginTrading":true,"enableInternalTransfer":!self.standalone_subaccount,"permitsUniversalTransfer":true,"ipRestrict":true})
            }
            E::FuturesAccountConfig => {
                json!({"dualSidePosition":false,"multiAssetsMargin":false,"canTrade":true,"canDeposit":true,"canWithdraw":true})
            }
            E::FuturesSymbolConfig => {
                json!([{"symbol":"ETHUSDT","leverage":s.leverage,"marginType":s.margin_type,"isAutoAddMargin":false,"maxNotionalValue":"100000"}])
            }
            E::FuturesPositionMode => json!({"dualSidePosition":false}),
            E::FuturesExchangeInfo => {
                json!({"symbols":[{"symbol":"ETHUSDT","status":"TRADING","contractType":"PERPETUAL","baseAsset":"ETH","quoteAsset":"USDT","marginAsset":"USDT","pricePrecision":2,"quantityPrecision":3,"quotePrecision":8,"orderTypes":["LIMIT"],"timeInForce":["IOC","FOK"],"filters":[{"filterType":"PRICE_FILTER","minPrice":"0.01","maxPrice":"2000","tickSize":"0.01"},{"filterType":"LOT_SIZE","minQty":"0.001","maxQty":"10","stepSize":"0.001"},{"filterType":"MIN_NOTIONAL","notional":"5"},{"filterType":"PERCENT_PRICE","multiplierUp":"1.1","multiplierDown":"0.9","multiplierDecimal":"4"}]}]})
            }
            E::FuturesPositions => {
                json!([{"symbol":"ETHUSDT","positionSide":"BOTH","positionAmt":s.position,"entryPrice":if s.position=="0" {"0"} else {"2000"},"breakEvenPrice":"2000","markPrice":"2000","unRealizedProfit":"0","liquidationPrice":"0","notional":decimal(units(&s.position)*2000),"isolatedWallet":decimal(position_margin),"isolatedMargin":decimal(position_margin),"initialMargin":decimal(position_margin),"maintMargin":"0.01","marginType":s.margin_type,"leverage":s.leverage.to_string(),"updateTime":time}])
            }
            E::FuturesAccount => {
                json!({"assets":[{"asset":"USDT","walletBalance":s.futures,"marginBalance":s.futures,"availableBalance":decimal(units(&s.futures)-position_margin),"maxWithdrawAmount":decimal(units(&s.futures)-position_margin),"unrealizedProfit":"0","initialMargin":decimal(position_margin),"maintMargin":"0.01"}]})
            }
            E::FuturesLeverageBrackets => {
                json!([{"symbol":"ETHUSDT","brackets":[{"bracket":1,"initialLeverage":50,"notionalCap":100000,"notionalFloor":0,"maintMarginRatio":"0.005","cum":"0"}]}])
            }
            E::FuturesOpenOrders
            | E::FuturesOpenAlgoOrders
            | E::SpotOpenOrders
            | E::FuturesIncome
            | E::DepositHistory
            | E::WithdrawalHistory => json!([]),
            E::FuturesCommission => {
                json!({"symbol":"ETHUSDT","makerCommissionRate":"0.0002","takerCommissionRate":"0.0005"})
            }
            E::FuturesFeeBurn => json!({"feeBurn":false}),
            E::FuturesMarkPrice => {
                json!({"symbol":"ETHUSDT","markPrice":"2000","indexPrice":"2000","time":time})
            }
            E::SubAccountList => {
                assert!(
                    !self.standalone_subaccount,
                    "A standalone child must never query a master relationship"
                );
                json!({"subAccounts":[]})
            }
            E::FuturesLeverage => {
                assert_eq!(s.position, "0");
                assert_eq!(p("symbol"), "ETHUSDT");
                s.leverage = p("leverage").parse().unwrap();
                s.posts += 1;
                json!({"symbol":"ETHUSDT","leverage":s.leverage,"maxNotionalValue":"100000"})
            }
            E::FuturesMarginType => {
                assert_eq!(s.position, "0");
                assert_eq!(p("symbol"), "ETHUSDT");
                s.margin_type = p("marginType").into();
                s.posts += 1;
                json!({"code":200,"msg":"success"})
            }
            E::UniversalTransfer => {
                assert!(
                    !self.standalone_subaccount,
                    "Child collateral cannot use master-authorized universal transfers"
                );
                assert_eq!(p("asset"), "USDT");
                assert!(
                    !s.transfers
                        .iter()
                        .any(|row| row["clientTranId"] == p("clientTranId")),
                    "duplicate transfer dispatch"
                );
                let amount = units(p("amount"));
                assert!(amount > 0);
                match (p("fromAccountType"), p("toAccountType")) {
                    ("SPOT", "USDT_FUTURE") => {
                        assert!(units(&s.spot) >= amount);
                        s.spot = decimal(units(&s.spot) - amount);
                        s.futures = decimal(units(&s.futures) + amount);
                    }
                    ("USDT_FUTURE", "SPOT") => {
                        assert_eq!(s.position, "0");
                        assert!(units(&s.futures) >= amount);
                        s.futures = decimal(units(&s.futures) - amount);
                        s.spot = decimal(units(&s.spot) + amount);
                    }
                    _ => panic!("unexpected transfer wallets"),
                }
                s.posts += 1;
                s.transfer_posts += 1;
                s.universal_transfer_posts += 1;
                let id = s.posts;
                s.transfers.push(json!({"tranId":id,"clientTranId":p("clientTranId"),"asset":"USDT","amount":p("amount"),"fromAccountType":p("fromAccountType"),"toAccountType":p("toAccountType"),"status":"SUCCESS","createTimeStamp":time,"fromEmail":"master@example.invalid","toEmail":"master@example.invalid"}));
                json!({"tranId":id,"clientTranId":p("clientTranId")})
            }
            E::UniversalTransferHistory => {
                assert!(
                    !self.standalone_subaccount,
                    "Child transfer evidence must use its own native wallet history"
                );
                let rows: Vec<_> = s
                    .transfers
                    .iter()
                    .filter(|row| {
                        r.get("clientTranId")
                            .is_none_or(|id| row["clientTranId"] == id)
                    })
                    .cloned()
                    .collect();
                json!({"totalCount":rows.len(),"result":rows})
            }
            E::WalletTransfer => {
                assert!(self.standalone_subaccount);
                assert_eq!(p("asset"), "USDT");
                assert!(
                    r.get("clientTranId").is_none(),
                    "User wallet transfer has no client identity parameter"
                );
                assert!(r.get("fromEmail").is_none() && r.get("toEmail").is_none());
                let amount = units(p("amount"));
                assert!(amount > 0);
                match p("type") {
                    "MAIN_UMFUTURE" => {
                        assert!(units(&s.spot) >= amount);
                        s.spot = decimal(units(&s.spot) - amount);
                        s.futures = decimal(units(&s.futures) + amount);
                    }
                    "UMFUTURE_MAIN" => {
                        assert_eq!(s.position, "0");
                        assert!(units(&s.futures) >= amount);
                        s.futures = decimal(units(&s.futures) - amount);
                        s.spot = decimal(units(&s.spot) + amount);
                    }
                    _ => panic!("Unsupported native user wallet transfer type"),
                }
                s.posts += 1;
                s.transfer_posts += 1;
                s.wallet_transfer_posts += 1;
                let id = s.posts;
                s.transfers.push(json!({"tranId":id,"asset":"USDT","amount":p("amount"),"type":p("type"),"status":"CONFIRMED","timestamp":time / 1000 * 1000}));
                if s.wallet_transfer_posts == 1 && !s.suppress_faults {
                    // A received ID is durably journaled, but its authenticated
                    // history is temporarily unavailable until broker restart.
                    s.transfer_proof_visible = false;
                }
                if s.wallet_transfer_posts == 5
                    && !s.suppress_faults
                    && !s.lost_transfer_response_injected
                {
                    // Separate deliberate fault after both successful cycles:
                    // no returned tranId means history cannot safely identify it.
                    s.lost_transfer_response_injected = true;
                    self.save(&s).unwrap();
                    return Err(TransportError::Timeout);
                }
                json!({"tranId":id})
            }
            E::WalletTransferHistory => {
                assert!(self.standalone_subaccount);
                if !s.transfer_proof_visible {
                    return Err(TransportError::Timeout);
                }
                assert!(r.get("clientTranId").is_none());
                let rows: Vec<_> = s
                    .transfers
                    .iter()
                    .filter(|row| row["type"] == p("type"))
                    .filter(|row| {
                        let timestamp = row["timestamp"].as_i64().unwrap();
                        r.get("startTime")
                            .is_none_or(|start| timestamp >= start.parse::<i64>().unwrap())
                            && r.get("endTime")
                                .is_none_or(|end| timestamp <= end.parse::<i64>().unwrap())
                    })
                    .cloned()
                    .collect();
                if rows.is_empty() {
                    json!({"total":0})
                } else {
                    json!({"total":rows.len(),"rows":rows})
                }
            }
            E::FuturesPlaceOrder => {
                assert_eq!(p("symbol"), "ETHUSDT");
                assert_eq!(p("type"), "LIMIT");
                assert_eq!(p("timeInForce"), "FOK");
                assert_eq!(p("positionSide"), "BOTH");
                let quantity = units(p("quantity"));
                assert!([1000, 2000, 3000].contains(&quantity));
                assert_eq!(units(p("price")), 2_000_000_000);
                assert_eq!(s.leverage, 3);
                let client = p("newClientOrderId");
                assert!(!s.orders.contains_key(client), "duplicate order dispatch");
                let change = if p("side") == "BUY" {
                    quantity
                } else {
                    -quantity
                };
                let prior = units(&s.position);
                if p("reduceOnly") == "true" {
                    assert_ne!(prior.signum(), change.signum());
                    assert!(change.abs() <= prior.abs());
                } else {
                    assert_eq!(prior, 0);
                }
                s.position = decimal(prior + change);
                // At 2000 USDT/ETH and 0.05%, commission atoms equal ETH
                // quantity atoms in this fixture's shared six-decimal scale.
                let commission = quantity;
                let loss = if s.suppress_faults && p("reduceOnly") == "true" && prior + change == 0
                {
                    1000
                } else {
                    0
                };
                s.futures = decimal(units(&s.futures) - commission - loss);
                s.posts += 1;
                s.order_posts += 1;
                let id = s.posts;
                let order = json!({"symbol":"ETHUSDT","clientOrderId":client,"orderId":id,"side":p("side"),"positionSide":"BOTH","type":"LIMIT","timeInForce":"FOK","reduceOnly":p("reduceOnly")=="true","price":"2000","origQty":p("quantity"),"executedQty":p("quantity"),"cumQuote":decimal(quantity * 2000),"status":"FILLED","time":time,"updateTime":time});
                s.orders.insert(client.into(), order.clone());
                s.fills.insert(id.to_string(),json!([{"id":id,"orderId":id,"symbol":"ETHUSDT","side":p("side"),"positionSide":"BOTH","buyer":p("side")=="BUY","maker":false,"price":"2000","qty":p("quantity"),"quoteQty":decimal(quantity * 2000),"commission":decimal(commission),"commissionAsset":"USDT","realizedPnl":decimal(-loss),"time":time}]));
                if !s.lost_response_injected {
                    s.lost_response_injected = true;
                    s.proof_visible = false;
                    self.save(&s).unwrap();
                    return Err(TransportError::Timeout);
                }
                order
            }
            E::FuturesOrder => {
                if !s.proof_visible {
                    return Err(TransportError::Timeout);
                }
                s.orders
                    .get(p("origClientOrderId"))
                    .expect("original persisted order")
                    .clone()
            }
            E::FuturesTrades => s.fills.get(p("orderId")).cloned().unwrap_or(json!([])),
            other => panic!("unexpected synthetic Binance endpoint {other:?}"),
        };
        self.save(&s).unwrap();
        Ok(response)
    }
}
struct Prices;
#[async_trait]
impl BinancePriceProvider for Prices {
    async fn quote_usd(&self, coin: &str) -> Result<PriceQuote> {
        let price = match coin {
            "USDT" => "1",
            "ETH" => "2000",
            _ => return Err(fixture_error("unknown synthetic asset")),
        };
        Ok(PriceQuote {
            usd_per_unit: price.into(),
            observed_at: now(),
            source: "synthetic-binance-rehearsal".into(),
        })
    }
}
#[async_trait]
impl AssetValuator for Prices {
    async fn usd_micros(&self, amount: &Amount) -> Result<(String, String, String)> {
        if amount.asset.network != "binance" {
            return Err(fixture_error("unknown synthetic network"));
        }
        let unit = match amount.asset.reference.as_str() {
            "USDT" => 1_000_000,
            "ETH" => 2_000_000_000,
            _ => return Err(fixture_error("unknown synthetic asset")),
        };
        let scale = Quantity::new(format!("1{}", "0".repeat(amount.asset.decimals as usize)))?;
        let value = amount
            .atoms
            .mul_div_ceil(&Quantity::from_u64(unit), &scale)?;
        Ok((
            value.to_string(),
            now(),
            "synthetic-binance-rehearsal".into(),
        ))
    }
}
