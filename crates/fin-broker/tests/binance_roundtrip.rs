//! Synthetic end-to-end Binance SPOT transfer run. This fixture never builds a
//! production Binance client and accepts no environment-provided endpoints.
use async_trait::async_trait;
use fin_binance::{
    AccountRole, BinanceAccount, BinanceAdapter, BinanceConfig, BinanceEndpoint,
    BinancePriceProvider, BinanceRequest, BinanceTransport, CredentialHandle, PriceQuote,
    TransportError,
};
use fin_broker::{
    AssetValuator, BrokerConfigV2, BrokerPolicy, ConnectionConfig, RuntimeDependencies,
    compose_runtime,
};
use fin_domain::{
    Adapter, Amount, ApprovalAudit, AssetId, AuthorizationChallenge, Authorizer, FinError,
    OperationRequest, Quantity, ResourceAmount, ResourceId, Result, now,
};
use fin_evm::EvmRpc;
use fin_protocol::BrokerRequest;
use fin_store::Store;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

const MAIN: &str = "main";
const CHILD_A: &str = "child-a";
const CHILD_B: &str = "child-b";
const EMAIL_A: &str = "a@example.invalid";
const EMAIL_B: &str = "b@example.invalid";
const MASTER_EMAIL: &str = "synthetic-master@example.invalid";
const ASSET: &str = "ETH";
const STEP_A_IN: &str = "a-to-main";
const STEP_B_IN: &str = "b-to-main";
const STEP_B_OUT: &str = "main-to-b";
const STEP_A_OUT: &str = "main-to-a";

fn resource(uid: u64) -> ResourceId {
    ResourceId(format!("binance:uid:{uid}:SPOT"))
}
fn micros(value: &str) -> i128 {
    let mut parts = value.split('.');
    let whole = parts.next().unwrap_or("0").parse::<i128>().unwrap();
    let fraction = parts.next().unwrap_or("0");
    assert!(fraction.len() <= 6);
    whole * 1_000_000 + format!("{fraction:0<6}").parse::<i128>().unwrap()
}
fn format_micros(value: i128) -> String {
    format!("{}.{:06}", value / 1_000_000, value % 1_000_000)
}
fn transfer(id: &str, from: &str, to: &str, amount: &str) -> Value {
    json!({"requestId":id,"authority":MAIN,"from":from,"to":to,"coin":ASSET,"amount":amount,"fromWallet":"SPOT","toWallet":"SPOT"})
}
#[derive(Default)]
struct VenueState {
    balances: BTreeMap<String, String>,
    usdc_balances: BTreeMap<String, String>,
    history: Vec<Value>,
    deposit_history: Vec<Value>,
    withdrawal_history: Vec<Value>,
    requests: Vec<(BinanceEndpoint, Vec<(String, String)>)>,
    posts: usize,
    reject_request_id: Option<String>,
    rate_limit_request_id: Option<String>,
    accept_then_timeout: Option<String>,
    restrictions: Option<bool>,
    withdraw_fee: String,
    deposit_enabled: bool,
    deposit_memo_regex: Option<String>,
    deposit_memo: Option<String>,
    deposit_denomination: Option<u64>,
}
struct SyntheticVenue {
    state: Mutex<VenueState>,
    wallet_balance: Mutex<Option<Arc<Mutex<String>>>>,
}
impl SyntheticVenue {
    fn new() -> Self {
        Self {
            state: Mutex::new(VenueState {
                balances: BTreeMap::from([
                    ("main".into(), "0".into()),
                    (EMAIL_A.into(), "0.005".into()),
                    (EMAIL_B.into(), "0.010".into()),
                ]),
                usdc_balances: BTreeMap::from([
                    ("main".into(), "0".into()),
                    (EMAIL_A.into(), "0".into()),
                    (EMAIL_B.into(), "0".into()),
                ]),
                restrictions: Some(true),
                withdraw_fee: "0.001".into(),
                deposit_enabled: true,
                deposit_memo_regex: None,
                deposit_memo: None,
                deposit_denomination: None,
                ..VenueState::default()
            }),
            wallet_balance: Mutex::new(None),
        }
    }
    fn free(s: &VenueState, key: &str) -> String {
        s.balances.get(key).cloned().unwrap_or_else(|| "0".into())
    }
    fn alter(s: &mut VenueState, key: &str, delta: i64) {
        // All amounts in this fixture are exact multiples of 0.001 ETH.
        let current = milli(&Self::free(s, key));
        let next = current
            .checked_add(delta)
            .expect("synthetic balance arithmetic");
        assert!(next >= 0);
        s.balances
            .insert(key.into(), format!("{}.{:03}", next / 1000, next % 1000));
    }
    fn format_milli(value: i64) -> String {
        format!("{}.{:03}", value / 1000, value % 1000)
    }
    fn record_deposit(&self, tx_hash: &str, amount: &str) {
        self.record_coin_deposit_for("main", ASSET, "ETH", tx_hash, amount);
    }
    fn record_coin_deposit_for(
        &self,
        account: &str,
        coin: &str,
        network: &str,
        tx_hash: &str,
        amount: &str,
    ) {
        let mut state = self.state.lock().unwrap();
        if coin == "ETH" {
            let milli_amount = milli(amount);
            Self::alter(&mut state, account, milli_amount);
        } else if coin == "USDC" {
            let current = micros(state.usdc_balances.get(account).unwrap());
            let next = current + micros(amount);
            state
                .usdc_balances
                .insert(account.into(), format_micros(next));
        }
        state.deposit_history.push(json!({
            "id":"synthetic-deposit-row-1",
            "coin":coin,
            "amount":amount,
            "transactionFee":"0",
            "network":network,
            "address":"0x1111111111111111111111111111111111111111",
            "addressTag":"",
            "txId":tx_hash,
            "status":1,
            "insertTime":chrono::Utc::now().timestamp_millis() as u64,
            "confirmTimes":"12/12"
        }));
    }
    fn owner(credential: &CredentialHandle) -> &'static str {
        match credential.as_str() {
            "test-only:main" => "main",
            "test-only:a" => EMAIL_A,
            "test-only:b" => EMAIL_B,
            _ => panic!("unexpected synthetic credential"),
        }
    }
    fn params(request: &BinanceRequest) -> BTreeMap<String, String> {
        request
            .params
            .iter()
            .map(|p| (p.key.clone(), p.value.clone()))
            .collect()
    }
}

#[async_trait]
impl BinanceTransport for SyntheticVenue {
    async fn request(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, TransportError> {
        use BinanceEndpoint as E;
        let mut s = self.state.lock().unwrap();
        let params = Self::params(&request);
        s.requests.push((
            request.endpoint,
            params.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        ));
        let owner = Self::owner(credential);
        match request.endpoint {
            E::ExchangeInfo
            | E::SpotCommission
            | E::SpotOrder
            | E::SpotOrderStatus
            | E::SpotTrades => Err(TransportError::InvalidResponse),
            E::Account => {
                let uid = match owner {
                    "main" => 1001,
                    EMAIL_A => 1002,
                    _ => 1003,
                };
                Ok(
                    json!({"uid":uid,"accountType":"SPOT","balances":[{"asset":ASSET,"free":Self::free(&s, owner),"locked":"0"},{"asset":"USDC","free":s.usdc_balances.get(owner).cloned().unwrap_or_else(||"0".into()),"locked":"0"}]}),
                )
            }
            E::SubAccountSpotAssets => {
                let email = params.get("email").ok_or(TransportError::InvalidResponse)?;
                Ok(json!({"balances":[{"asset":ASSET,"free":Self::free(&s,email),"locked":"0"}]}))
            }
            E::ApiRestrictions => Ok(
                json!({"enableInternalTransfer":s.restrictions,"permitsUniversalTransfer":s.restrictions,"enableWithdrawals":true}),
            ),
            E::SubAccountList => Ok(
                json!({"subAccounts":[{"email":EMAIL_A,"isFreeze":false,"subAccountId":1002},{"email":EMAIL_B,"isFreeze":false,"subAccountId":1003}]}),
            ),
            E::SubAccountTransferHistory => {
                let direction = params.get("type").ok_or(TransportError::InvalidResponse)?;
                let start = params
                    .get("startTime")
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or(TransportError::InvalidResponse)?;
                let end = params
                    .get("endTime")
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or(TransportError::InvalidResponse)?;
                let limit = params
                    .get("limit")
                    .and_then(|v| v.parse::<usize>().ok())
                    .ok_or(TransportError::InvalidResponse)?;
                let rows = s.history.iter().filter_map(|row| {
                    let child_sends = row["fromEmail"] == owner && row["toEmail"] == MASTER_EMAIL;
                    let child_receives = row["toEmail"] == owner && row["fromEmail"] == MASTER_EMAIL;
                    let kind = if child_sends { 2 } else if child_receives { 1 } else { return None; };
                    let time = row["createTimeStamp"].as_u64()?;
                    if kind.to_string() != *direction || time < start || time > end {
                        return None;
                    }
                    Some(json!({
                        "counterParty":"master", "email":MASTER_EMAIL, "type":kind,
                        "asset":row["asset"], "qty":row["amount"],
                        "fromAccountType":row["fromAccountType"], "toAccountType":row["toAccountType"],
                        "status":row["status"], "tranId":row["tranId"], "time":time
                    }))
                }).take(limit).collect::<Vec<_>>();
                Ok(json!(rows))
            }
            E::CapitalConfig => {
                let mut assets = json!([
                {"coin":ASSET,"depositAllEnable":true,"withdrawAllEnable":true,"networkList":[{"network":"ETH","coin":ASSET,"withdrawIntegerMultiple":"0.001","withdrawFee":s.withdraw_fee,"withdrawMin":"0.001","withdrawMax":"100","depositDust":"0.001","withdrawEnable":true,"depositEnable":s.deposit_enabled,"memoRegex":s.deposit_memo_regex,"minConfirm":12,"unLockConfirm":64},{"network":"ARBITRUM","coin":ASSET,"withdrawIntegerMultiple":"0.001","withdrawFee":"0","withdrawMin":"0.001","withdrawMax":"100","depositDust":"0.001","withdrawEnable":true,"depositEnable":s.deposit_enabled,"memoRegex":s.deposit_memo_regex,"minConfirm":12,"unLockConfirm":64}]},
                {"coin":"USDC","depositAllEnable":true,"withdrawAllEnable":true,"networkList":[{"network":"ARBITRUM","coin":"USDC","withdrawIntegerMultiple":"0.000001","withdrawFee":"0","withdrawMin":"0.001","withdrawMax":"1000000","depositDust":"0.001","withdrawEnable":true,"depositEnable":s.deposit_enabled,"memoRegex":s.deposit_memo_regex,"contractAddress":"0xaf88d065e77c8cC2239327C5EDb3A432268e5831","denomination":s.deposit_denomination,"minConfirm":12,"unLockConfirm":64}]}
                ]);
                for asset in assets.as_array_mut().unwrap() {
                    let routes = asset["networkList"].as_array_mut().unwrap();
                    if let Some(arbitrum) = routes.iter().find(|r| r["network"] == "ARBITRUM") {
                        let mut base = arbitrum.clone();
                        base["network"] = json!("BASE");
                        if base["coin"] == "USDC" {
                            base["contractAddress"] =
                                json!("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913");
                        }
                        routes.push(base);
                    }
                }
                Ok(assets)
            }
            E::DepositAddress => {
                if params.get("coin").is_some_and(|coin| coin == "USDC") {
                    Ok(
                        json!({"coin":"USDC","network":params.get("network").map(String::as_str).unwrap_or("ARBITRUM"),"address":"0x1111111111111111111111111111111111111111","tag":s.deposit_memo.clone().unwrap_or_default()}),
                    )
                } else if params
                    .get("network")
                    .is_some_and(|network| network == "ARBITRUM" || network == "BASE")
                {
                    Ok(
                        json!({"coin":ASSET,"network":params.get("network").unwrap(),"address":"0x1111111111111111111111111111111111111111","tag":""}),
                    )
                } else {
                    Ok(
                        json!({"coin":ASSET,"network":"ETH","address":"0x1111111111111111111111111111111111111111","tag":""}),
                    )
                }
            }
            E::WithdrawAddressList => Ok(json!([])),
            E::DepositHistory => {
                let records = s
                    .deposit_history
                    .iter()
                    .filter(|row| {
                        params.get("txId").is_none_or(|tx| {
                            row["txId"]
                                .as_str()
                                .is_some_and(|value| value.eq_ignore_ascii_case(tx))
                        })
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                Ok(json!(records))
            }
            E::WithdrawalHistory => {
                let filtered = s
                    .withdrawal_history
                    .iter()
                    .filter(|row| {
                        params
                            .get("withdrawOrderId")
                            .is_none_or(|id| row["withdrawOrderId"] == *id)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                Ok(json!(filtered))
            }
            E::TravelRuleRequirements => Ok(json!({"questionnaireCountryCode":"NIL"})),
            E::StandardWithdrawal => {
                if owner != "main" || s.restrictions != Some(true) {
                    return Err(TransportError::PermissionDenied);
                }
                s.posts += 1;
                let order_id = params
                    .get("withdrawOrderId")
                    .cloned()
                    .ok_or(TransportError::InvalidResponse)?;
                let amount = params
                    .get("amount")
                    .cloned()
                    .ok_or(TransportError::InvalidResponse)?;
                let fee = s.withdraw_fee.clone();
                let gross = milli(&amount);
                let fee_milli = milli(&fee);
                if gross < fee_milli || milli(&Self::free(&s, "main")) < gross {
                    return Err(TransportError::Rejected);
                }
                Self::alter(&mut s, "main", -gross);
                let net = gross - fee_milli;
                let net_amount = Self::format_milli(net);
                if let Some(wallet_balance) = self.wallet_balance.lock().unwrap().as_ref() {
                    adjust_native_balance(wallet_balance, &net_amount, true);
                }
                let hash = format!("0x{}", "ee".repeat(32));
                let address = params
                    .get("address")
                    .cloned()
                    .ok_or(TransportError::InvalidResponse)?;
                s.withdrawal_history.push(json!({"id":"synthetic-withdrawal","withdrawOrderId":order_id,"coin":"ETH","network":"ETH","address":address,"amount":net_amount,"transactionFee":fee,"status":6,"txId":hash,"transferType":0,"walletType":0,"applyTime":chrono::Utc::now().to_rfc3339()}));
                Ok(json!({"id":"synthetic-withdrawal","withdrawOrderId":order_id}))
            }
            E::UniversalTransferHistory => {
                let filtered: Vec<_> = s
                    .history
                    .iter()
                    .filter(|h| {
                        if let Some(id) = params.get("clientTranId") {
                            h["clientTranId"] == *id
                        } else if let Some(email) = params.get("fromEmail") {
                            h["fromEmail"] == *email
                        } else {
                            h["fromEmail"].is_null()
                        }
                    })
                    .cloned()
                    .collect();
                Ok(json!({"totalCount":filtered.len(),"result":filtered}))
            }
            E::UniversalTransfer => {
                if owner != "main" {
                    return Err(TransportError::PermissionDenied);
                }
                s.posts += 1;
                let id = params
                    .get("clientTranId")
                    .cloned()
                    .ok_or(TransportError::InvalidResponse)?;
                if s.rate_limit_request_id.as_deref() == Some(id.as_str()) {
                    return Err(TransportError::RateLimited);
                }
                let from_email = params.get("fromEmail").cloned();
                let to_email = params.get("toEmail").cloned();
                if s.reject_request_id.as_deref() == Some(id.as_str()) {
                    let tran = s.posts.to_string();
                    s.history.push(json!({"tranId":tran,"clientTranId":id,"asset":ASSET,"amount":params.get("amount").ok_or(TransportError::InvalidResponse)?,"fromAccountType":"SPOT","toAccountType":"SPOT","fromEmail":from_email.as_deref().unwrap_or(MASTER_EMAIL),"toEmail":to_email.as_deref().unwrap_or(MASTER_EMAIL),"status":"FAILED","createTimeStamp":chrono::Utc::now().timestamp_millis() as u64}));
                    return Err(TransportError::Rejected);
                }
                let from = from_email.as_deref().unwrap_or("main");
                let to = to_email.as_deref().unwrap_or("main");
                let amount = milli(
                    params
                        .get("amount")
                        .ok_or(TransportError::InvalidResponse)?,
                );
                if milli(&Self::free(&s, from)) < amount {
                    return Err(TransportError::Rejected);
                }
                let millis = amount;
                Self::alter(&mut s, from, -millis);
                Self::alter(&mut s, to, millis);
                let tran = s.posts.to_string();
                let row = json!({"tranId":tran,"clientTranId":id,"asset":ASSET,"amount":params["amount"],"fromAccountType":"SPOT","toAccountType":"SPOT","fromEmail":from_email.as_deref().unwrap_or(MASTER_EMAIL),"toEmail":to_email.as_deref().unwrap_or(MASTER_EMAIL),"status":"SUCCESS","createTimeStamp":chrono::Utc::now().timestamp_millis() as u64});
                s.history.push(row);
                if s.accept_then_timeout.as_deref() == Some(id.as_str()) {
                    s.accept_then_timeout = None;
                    return Err(TransportError::Timeout);
                }
                Ok(json!({"tranId":tran,"clientTranId":id}))
            }
            _ => Err(TransportError::InvalidResponse),
        }
    }
}
fn milli(value: &str) -> i64 {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    assert!(fraction.len() <= 3 && fraction.bytes().all(|b| b.is_ascii_digit()));
    whole.parse::<i64>().unwrap() * 1000
        + fraction.parse::<i64>().unwrap_or(0) * 10_i64.pow((3 - fraction.len()) as u32)
}

fn adjust_native_balance(balance: &Arc<Mutex<String>>, amount: &str, increase: bool) {
    let current = Quantity::from_decimal(&balance.lock().unwrap(), 18).unwrap();
    let delta = Quantity::from_decimal(amount, 18).unwrap();
    let next = if increase {
        current.checked_add(&delta).unwrap()
    } else {
        current.checked_sub(&delta).unwrap()
    };
    *balance.lock().unwrap() = next.to_decimal(18);
}

struct SyntheticApproval(AtomicUsize);
#[async_trait]
impl Authorizer for SyntheticApproval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "synthetic-fixture-only".into(),
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
struct SyntheticBinancePrice;
struct SyntheticWalletPrice;

/// CoinGecko-shaped synthetic valuation source for composite deposit approval
/// boundary tests. Price and publication timestamp move independently of the
/// actual observation time, as with a refreshed provider quote.
struct MutableEthereumUsd {
    usd_micros_per_eth: AtomicU64,
    updated_at: Mutex<String>,
}

impl MutableEthereumUsd {
    fn new(usd_micros_per_eth: u64) -> Self {
        Self {
            usd_micros_per_eth: AtomicU64::new(usd_micros_per_eth),
            updated_at: Mutex::new(
                (chrono::Utc::now() - chrono::Duration::seconds(1))
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
        }
    }

    fn update(&self, usd_micros_per_eth: u64) {
        self.usd_micros_per_eth
            .store(usd_micros_per_eth, Ordering::SeqCst);
        *self.updated_at.lock().unwrap() =
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    }
}

#[async_trait]
impl fin_ethereum::WalletValuation for MutableEthereumUsd {
    async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
        let (value, source, _) = self.quote_observation(asset, units).await?;
        Ok((value, source))
    }

    async fn quote_observation(
        &self,
        asset: &AssetId,
        units: Quantity,
    ) -> Result<(Quantity, String, String)> {
        assert_eq!(asset.reference, "native");
        assert_eq!(asset.decimals, 18);
        let micros = fin_evm::u256(&units)?
            * fin_evm::alloy::primitives::U256::from(
                self.usd_micros_per_eth.load(Ordering::SeqCst),
            )
            / fin_evm::alloy::primitives::U256::from(10u8)
                .pow(fin_evm::alloy::primitives::U256::from(18u64));
        let source = format!(
            "coingecko:ethereum-usd; updated:{}",
            self.updated_at.lock().unwrap()
        );
        Ok((fin_evm::quantity(micros), source, now()))
    }
}

#[async_trait]
impl fin_ethereum::WalletValuation for SyntheticWalletPrice {
    async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
        let micros = fin_evm::u256(&units)? * fin_evm::alloy::primitives::U256::from(1_000_000u64)
            / fin_evm::alloy::primitives::U256::from(10u64).pow(
                fin_evm::alloy::primitives::U256::from(asset.decimals as u64),
            );
        Ok((fin_evm::quantity(micros), "synthetic-arbitrum-usd".into()))
    }
}
#[async_trait]
impl BinancePriceProvider for SyntheticBinancePrice {
    async fn quote_usd(&self, coin: &str) -> std::result::Result<PriceQuote, FinError> {
        if coin != ASSET {
            return Err(FinError::new(
                "PRICE_UNAVAILABLE",
                "test",
                "Unknown synthetic asset.",
                "Use the fixed ETH fixture.",
            ));
        }
        Ok(PriceQuote {
            usd_per_unit: "2000".into(),
            observed_at: now(),
            source: "synthetic-fixture:ETH-USD-2000".into(),
        })
    }
}
struct StaleBinancePrice;
#[async_trait]
impl BinancePriceProvider for StaleBinancePrice {
    async fn quote_usd(&self, coin: &str) -> std::result::Result<PriceQuote, FinError> {
        if coin != ASSET {
            return Err(FinError::validation("Unknown synthetic asset."));
        }
        Ok(PriceQuote {
            usd_per_unit: "2000".into(),
            observed_at: "2020-01-01T00:00:00Z".into(),
            source: "synthetic-stale-fixture".into(),
        })
    }
}
struct SyntheticValuator;
#[async_trait]
impl AssetValuator for SyntheticValuator {
    async fn usd_micros(&self, amount: &Amount) -> Result<(String, String, String)> {
        let atoms = amount.atoms.integer();
        let unit = num_bigint::BigUint::from(10u8).pow(amount.asset.decimals as u32);
        let usd = atoms * num_bigint::BigUint::from(2_000_000_000u64) / unit;
        Ok((
            usd.to_string(),
            now(),
            "synthetic-fixture:ETH-USD-2000".into(),
        ))
    }
}

fn runtime_config() -> BrokerConfigV2 {
    let mut config = BrokerConfigV2 {
        version: 2,
        networks: vec![fin_broker::NetworkConfig::ethereum_mainnet()],
        deployments: vec![fin_broker::DeploymentConfig::ethereum_mainnet()],
        connections: vec![],
        policy: BrokerPolicy {
            require_run_for_writes: true, // Legacy config is migrated semantically; ordinary execution remains available.
            max_run_allocation_usd_micros: "1000000000".into(),
            ..BrokerPolicy::beta()
        },
        approver_uid: 501,
        extensions: BTreeMap::new(),
    };
    for (name, uid, role, parent, email) in [
        (MAIN, 1001, "main", None, None),
        (CHILD_A, 1002, "sub", Some(MAIN), Some(EMAIL_A)),
        (CHILD_B, 1003, "sub", Some(MAIN), Some(EMAIL_B)),
    ] {
        config.connections.push(ConnectionConfig::Binance {
            name: name.into(),
            resource: resource(uid),
            credential_ref: format!("00000000-0000-4000-8000-{uid:012}"),
            role: role.into(),
            parent: parent.map(str::to_owned),
            email: email.map(str::to_owned),
            wallet_type: "SPOT".into(),
            physical_identity: uid.to_string(),
            physical_identity_verified: true,
        });
    }
    config.connections.push(ConnectionConfig::Ethereum {
        name: "synthetic-wallet".into(),
        resource: wallet_resource(),
        credential_ref: Some("00000000-0000-4000-8000-000000000001".into()),
        network: "ethereum:1".into(),
        address: "0x2222222222222222222222222222222222222222".into(),
        signing_enabled: true,
        assets: vec![native_asset()],
    });
    config
}

fn wallet_resource() -> ResourceId {
    ResourceId("ethereum:1:0x2222222222222222222222222222222222222222".into())
}

fn native_asset() -> AssetId {
    AssetId {
        network: "ethereum:1".into(),
        reference: "native".into(),
        decimals: 18,
    }
}

fn adapter_config() -> BinanceConfig {
    let accounts = vec![
        BinanceAccount {
            name: MAIN.into(),
            resource: resource(1001),
            credential: CredentialHandle::new("test-only:main").unwrap(),
            role: AccountRole::Main,
            parent: None,
            email: None,
            wallet_type: "SPOT".into(),
        },
        BinanceAccount {
            name: CHILD_A.into(),
            resource: resource(1002),
            credential: CredentialHandle::new("test-only:a").unwrap(),
            role: AccountRole::Sub,
            parent: Some(MAIN.into()),
            email: Some(EMAIL_A.into()),
            wallet_type: "SPOT".into(),
        },
        BinanceAccount {
            name: CHILD_B.into(),
            resource: resource(1003),
            credential: CredentialHandle::new("test-only:b").unwrap(),
            role: AccountRole::Sub,
            parent: Some(MAIN.into()),
            email: Some(EMAIL_B.into()),
            wallet_type: "SPOT".into(),
        },
    ];
    BinanceConfig {
        accounts,
        destinations: vec![fin_binance::EthereumDestination {
            name: "synthetic-wallet".into(),
            resource: wallet_resource(),
            address: "0x2222222222222222222222222222222222222222".into(),
            chain_id: 1,
            assets: BTreeMap::from([("ETH".into(), native_asset())]),
            confirmations: 2,
        }],
    }
}
fn adapter(venue: Arc<SyntheticVenue>) -> Arc<BinanceAdapter> {
    Arc::new(
        BinanceAdapter::new(
            adapter_config(),
            venue,
            Some(Arc::new(SyntheticBinancePrice)),
            None,
        )
        .unwrap(),
    )
}

#[test]
fn composite_deposit_hides_route_rows_publicly_but_binds_binance_and_evm_identities() {
    let compose = |main_resource: &str, wallet_address: &str| {
        let mut config = adapter_config();
        config.accounts[0].resource = ResourceId(main_resource.into());
        let binance = Arc::new(
            BinanceAdapter::new(
                config,
                Arc::new(SyntheticVenue::new()),
                Some(Arc::new(SyntheticBinancePrice)),
                None,
            )
            .unwrap(),
        );
        let ethereum = Arc::new(
            fin_ethereum::EthereumAdapter::new(
                fin_ethereum::EthereumConfig::default(),
                vec![fin_ethereum::WalletConnection {
                    name: "synthetic-wallet".into(),
                    address: wallet_address.into(),
                    signing_enabled: true,
                }],
                Arc::new(fin_evm::EvmRuntime::new(
                    Arc::new(DepositPreviewRpc::new()),
                    1,
                    None,
                )),
            )
            .unwrap(),
        );
        fin_broker::BinanceEthereumDepositAdapter::new(binance, ethereum)
    };

    let original = compose(
        "binance:uid:1001:SPOT",
        "0x2222222222222222222222222222222222222222",
    );
    assert!(original.connections().is_empty());
    let identities = original.authorization_identities();
    assert_eq!(identities.len(), 5);
    assert_eq!(identities[0].resource.0, "binance:uid:1001:SPOT");
    assert_eq!(identities[0].provider, "binance");
    assert_eq!(identities[1].resource.0, "binance:uid:1002:SPOT");
    assert_eq!(identities[2].resource.0, "binance:uid:1003:SPOT");
    assert_eq!(identities[3].provider, "binance-ethereum-destination");
    assert_eq!(identities[3].role.as_deref(), Some("deposit-destination"));
    assert_eq!(identities[3].network.as_deref(), Some("ethereum:1"));
    assert_eq!(
        identities[3].details["assets"]["ETH"]["reference"],
        "native"
    );
    assert_eq!(
        identities[3].address.as_deref(),
        Some("0x2222222222222222222222222222222222222222")
    );
    assert_eq!(identities[4].provider, "evm");
    assert_eq!(identities[4].network.as_deref(), Some("ethereum:1"));
    assert!(
        identities
            .iter()
            .all(|connection| connection.credential_ref.is_none())
    );
    let original_hash_input = serde_json::to_value(&identities).unwrap();

    let changed_binance = compose(
        "binance:uid:9001:SPOT",
        "0x2222222222222222222222222222222222222222",
    );
    let changed_wallet = compose(
        "binance:uid:1001:SPOT",
        "0x3333333333333333333333333333333333333333",
    );
    assert_ne!(
        original_hash_input,
        serde_json::to_value(changed_binance.authorization_identities()).unwrap(),
        "a changed Binance account resource must change the adapter's authorization identity"
    );
    assert_ne!(
        original_hash_input,
        serde_json::to_value(changed_wallet.authorization_identities()).unwrap(),
        "a changed EVM wallet address/resource must change the adapter's authorization identity"
    );
}

fn subaccount_only_adapter(venue: Arc<SyntheticVenue>) -> Arc<BinanceAdapter> {
    let mut config = adapter_config();
    let mut child = config.accounts.remove(1);
    child.parent = None;
    child.email = None;
    config.accounts = vec![child];
    Arc::new(
        BinanceAdapter::new(config, venue, Some(Arc::new(SyntheticBinancePrice)), None).unwrap(),
    )
}

struct WalletSnapshotObserver {
    resource: ResourceId,
    address: String,
    balance: Arc<Mutex<String>>,
}

#[async_trait]
impl Adapter for WalletSnapshotObserver {
    fn definitions(&self) -> Vec<fin_domain::Definition> {
        vec![]
    }
    fn connections(&self) -> Vec<fin_domain::ConnectionMetadata> {
        vec![fin_domain::ConnectionMetadata {
            name: "synthetic-wallet".into(),
            provider: "ethereum".into(),
            resource: self.resource.clone(),
            role: Some("wallet".into()),
            parent: None,
            network: Some("ethereum:1".into()),
            address: Some(self.address.clone()),
            credential_ref: None,
            signing_enabled: true,
            details: json!({}),
        }]
    }
    async fn snapshot(&self) -> Result<fin_domain::StateSnapshot> {
        Ok(fin_domain::StateSnapshot {
            state: fin_domain::FinancialState {
                holdings: vec![ResourceAmount {
                    resource: self.resource.clone(),
                    amount: Amount {
                        asset: native_asset(),
                        atoms: Quantity::from_decimal(&self.balance.lock().unwrap(), 18).unwrap(),
                    },
                }],
                ..Default::default()
            },
            observed_at: now(),
            complete: true,
            coverage: vec![self.resource.0.clone()],
            pending: vec![],
        })
    }
    async fn read(&self, _: &str, _: &Value) -> Result<Value> {
        Err(FinError::validation("Snapshot-only fixture."))
    }
    async fn prepare(&self, _: &str, _: &Value) -> Result<fin_domain::PreparedOperation> {
        Err(FinError::validation("Snapshot-only fixture."))
    }
    async fn revalidate(
        &self,
        _: &fin_domain::PreparedOperation,
    ) -> Result<fin_domain::PreparedOperation> {
        Err(FinError::validation("Snapshot-only fixture."))
    }
    async fn submit(
        &self,
        _: &fin_domain::PreparedOperation,
        _: Arc<dyn fin_domain::SubmissionJournal>,
    ) -> Result<fin_domain::Submission> {
        Err(FinError::validation("Snapshot-only fixture."))
    }
    async fn reconcile(
        &self,
        _: &fin_domain::OperationRecord,
    ) -> Result<fin_domain::Reconciliation> {
        Err(FinError::validation("Snapshot-only fixture."))
    }
}
fn runtime(
    db: &std::path::Path,
    venue: Arc<SyntheticVenue>,
    approval: Arc<SyntheticApproval>,
) -> Arc<fin_broker::BrokerRuntime> {
    compose_runtime(
        runtime_config(),
        RuntimeDependencies {
            store: Store::open(db).unwrap(),
            adapters: vec![adapter(venue)],
            authorizer: approval,
            valuator: Arc::new(SyntheticValuator),
            bridge: None,
        },
    )
    .unwrap()
}

async fn execute(broker: &fin_broker::BrokerRuntime, id: &str, input: Value) -> Value {
    let response = broker
        .handle(BrokerRequest::Execute {
            request: OperationRequest {
                request_id: id.into(),
                operation: "binance.transfer".into(),
                input,
                run_id: None,
            },
        })
        .await;
    assert!(response.ok, "{:?}", response.error);
    let result = response.result.unwrap();
    assert_eq!(result["status"], "completed", "{result}");
    result
}

#[tokio::test]
async fn two_child_spot_roundtrip_restores_allocations_and_reconciles_history() {
    assert!(!fin_binance::ExactDecimal::parse("0.005").unwrap().is_zero());
    let venue = Arc::new(SyntheticVenue::new());
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("synthetic.sqlite");
    let approval = Arc::new(SyntheticApproval(AtomicUsize::new(0)));
    let adapter = adapter(venue.clone());
    let preview = fin_domain::Adapter::prepare(
        &*adapter,
        "binance.transfer",
        &transfer(STEP_A_IN, CHILD_A, MAIN, "0.005"),
    )
    .await;
    assert!(
        preview.is_ok(),
        "{preview:?}; transcript={:?}",
        venue.state.lock().unwrap().requests
    );
    drop(preview);
    let broker = compose_runtime(
        runtime_config(),
        RuntimeDependencies {
            store: Store::open(&db).unwrap(),
            adapters: vec![adapter],
            authorizer: approval.clone(),
            valuator: Arc::new(SyntheticValuator),
            bridge: None,
        },
    )
    .unwrap();
    let one = execute(
        &broker,
        STEP_A_IN,
        transfer(STEP_A_IN, CHILD_A, MAIN, "0.005"),
    )
    .await;
    assert_eq!(one["status"], "completed", "{one}");
    let duplicate = execute(
        &broker,
        STEP_A_IN,
        transfer(STEP_A_IN, CHILD_A, MAIN, "0.005"),
    )
    .await;
    assert_eq!(duplicate["id"], one["id"]);
    execute(
        &broker,
        STEP_B_IN,
        transfer(STEP_B_IN, CHILD_B, MAIN, "0.010"),
    )
    .await;
    execute(
        &broker,
        STEP_A_OUT,
        transfer(STEP_A_OUT, MAIN, CHILD_A, "0.005"),
    )
    .await;
    execute(
        &broker,
        STEP_B_OUT,
        transfer(STEP_B_OUT, MAIN, CHILD_B, "0.010"),
    )
    .await;
    let s = venue.state.lock().unwrap();
    assert_eq!(s.balances[EMAIL_A], "0.005");
    assert_eq!(s.balances[EMAIL_B], "0.010");
    assert_eq!(s.balances["main"], "0.000");
    assert_eq!(
        s.posts, 4,
        "duplicate request must not submit a second transfer"
    );
    assert_eq!(s.history.len(), 4);
    assert!(s.history.iter().all(|row| row["status"] == "SUCCESS"
        && row["clientTranId"].as_str().unwrap().starts_with("fin")));
    assert_eq!(approval.0.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn concurrent_duplicate_transfer_uses_one_approval_and_one_venue_submission() {
    let venue = Arc::new(SyntheticVenue::new());
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("concurrent-duplicate.sqlite");
    let approval = Arc::new(SyntheticApproval(AtomicUsize::new(0)));
    let broker = runtime(&db, venue.clone(), approval.clone());
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let call = |broker: Arc<fin_broker::BrokerRuntime>, barrier: Arc<tokio::sync::Barrier>| {
        tokio::spawn(async move {
            barrier.wait().await;
            broker
                .handle(BrokerRequest::Execute {
                    request: OperationRequest {
                        request_id: "concurrent-in".into(),
                        operation: "binance.transfer".into(),
                        input: transfer("concurrent-in", CHILD_A, MAIN, "0.005"),
                        run_id: None,
                    },
                })
                .await
        })
    };
    let first = call(broker.clone(), barrier.clone());
    let second = call(broker.clone(), barrier.clone());
    barrier.wait().await;
    let (first, second) = tokio::join!(first, second);
    let first = first.unwrap();
    let second = second.unwrap();
    assert!(first.ok, "{:?}", first.error);
    assert!(second.ok, "{:?}", second.error);
    let first = first.result.unwrap();
    let second = second.result.unwrap();
    assert_eq!(first["id"], second["id"]);
    assert_eq!(first["status"], "completed");
    assert_eq!(second["status"], "completed");
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rate_limited_transfer_retry_returns_original_record_without_reposting() {
    let venue = Arc::new(SyntheticVenue::new());
    let client_id = fin_binance::ClientIdentity::transfer("rate-limited-in").unwrap();
    venue.state.lock().unwrap().rate_limit_request_id = Some(client_id.as_str().into());
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("rate-limited.sqlite");
    let broker = runtime(
        &db,
        venue.clone(),
        Arc::new(SyntheticApproval(AtomicUsize::new(0))),
    );
    let first = broker
        .handle(BrokerRequest::Execute {
            request: OperationRequest {
                request_id: "rate-limited-in".into(),
                operation: "binance.transfer".into(),
                input: transfer("rate-limited-in", CHILD_A, MAIN, "0.005"),
                run_id: None,
            },
        })
        .await;
    let second = broker
        .handle(BrokerRequest::Execute {
            request: OperationRequest {
                request_id: "rate-limited-in".into(),
                operation: "binance.transfer".into(),
                input: transfer("rate-limited-in", CHILD_A, MAIN, "0.005"),
                run_id: None,
            },
        })
        .await;
    assert_eq!(first.ok, second.ok, "first={first:?}; second={second:?}");
    assert_eq!(
        first.result.as_ref().unwrap()["id"],
        second.result.as_ref().unwrap()["id"],
        "first={first:?}; second={second:?}"
    );
    assert_eq!(venue.state.lock().unwrap().posts, 1);
}

#[tokio::test]
async fn accepted_but_lost_transfer_response_recovers_original_client_id_after_restart() {
    let venue = Arc::new(SyntheticVenue::new());
    let client_id = fin_binance::ClientIdentity::transfer("lost-a-to-main").unwrap();
    venue.state.lock().unwrap().accept_then_timeout = Some(client_id.as_str().into());
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("restart.sqlite");
    let approval = Arc::new(SyntheticApproval(AtomicUsize::new(0)));
    let broker = runtime(&db, venue.clone(), approval.clone());
    let unknown_response = broker
        .handle(BrokerRequest::Execute {
            request: OperationRequest {
                request_id: "lost-a-to-main".into(),
                operation: "binance.transfer".into(),
                input: transfer("lost-a-to-main", CHILD_A, MAIN, "0.005"),
                run_id: None,
            },
        })
        .await;
    assert!(unknown_response.ok, "{:?}", unknown_response.error);
    let unknown = unknown_response.result.unwrap();
    assert_eq!(unknown["status"], "unknown", "{unknown}");
    drop(broker);

    let recovered = runtime(&db, venue.clone(), approval.clone());
    recovered.engine.recover_interrupted().unwrap();
    let status = recovered
        .handle(BrokerRequest::Status {
            id: unknown["id"].as_str().unwrap().into(),
        })
        .await;
    assert!(status.ok, "{:?}", status.error);
    let settled = status.result.unwrap();
    assert_eq!(settled["status"], "completed", "{settled}");
    assert_eq!(settled["submission"]["reference"], client_id.as_str());
    let duplicate = execute(
        &recovered,
        "lost-a-to-main",
        transfer("lost-a-to-main", CHILD_A, MAIN, "0.005"),
    )
    .await;
    assert_eq!(duplicate["id"], settled["id"]);
    assert_eq!(
        venue.state.lock().unwrap().posts,
        1,
        "recovery must never replay an accepted POST"
    );
    execute(
        &recovered,
        "restore-a",
        transfer("restore-a", MAIN, CHILD_A, "0.005"),
    )
    .await;
    assert_eq!(venue.state.lock().unwrap().balances[EMAIL_A], "0.005");
}

#[tokio::test]
async fn permission_precision_and_stale_price_guards_block_before_any_post() {
    use fin_domain::Adapter;
    let venue = Arc::new(SyntheticVenue::new());
    let approval = Arc::new(SyntheticApproval(AtomicUsize::new(0)));
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("guards.sqlite");
    let adapter = adapter(venue.clone());
    let broker = compose_runtime(
        runtime_config(),
        RuntimeDependencies {
            store: Store::open(&db).unwrap(),
            adapters: vec![adapter.clone()],
            authorizer: approval.clone(),
            valuator: Arc::new(SyntheticValuator),
            bridge: None,
        },
    )
    .unwrap();

    for (id, value) in [
        ("guard-permission", Some(false)),
        ("guard-unknown-permission", None),
    ] {
        venue.state.lock().unwrap().restrictions = value;
        let permission = broker
            .handle(BrokerRequest::Preview {
                operation: "binance.transfer".into(),
                input: transfer(id, CHILD_A, MAIN, "0.005"),
            })
            .await;
        assert!(!permission.ok && permission.error.as_ref().unwrap().code == "MISSING_PERMISSION");
    }
    venue.state.lock().unwrap().restrictions = Some(true);
    let precision = broker
        .handle(BrokerRequest::Preview {
            operation: "binance.transfer".into(),
            input: transfer("guard-precision", CHILD_A, MAIN, "0.0055"),
        })
        .await;
    assert!(!precision.ok && precision.error.as_ref().unwrap().code == "INVALID_PRECISION");
    let stale_adapter = Arc::new(
        BinanceAdapter::new(
            adapter_config(),
            venue.clone(),
            Some(Arc::new(StaleBinancePrice)),
            None,
        )
        .unwrap(),
    );
    let stale = stale_adapter
        .prepare(
            "binance.transfer",
            &transfer("guard-stale", CHILD_A, MAIN, "0.005"),
        )
        .await;
    assert!(stale.is_err() && stale.unwrap_err().code == "STALE_VALUATION");
    assert_eq!(venue.state.lock().unwrap().posts, 0);
    assert_eq!(approval.0.load(Ordering::SeqCst), 0);
}

struct CanonicalNativeCreditRpc {
    value: Mutex<String>,
    latest: Mutex<String>,
    rpc_chain: AtomicU64,
    transaction_chain: AtomicU64,
    recipient: Mutex<String>,
    finalized: Mutex<Option<String>>,
    finalized_seen: AtomicUsize,
    change_inclusion_after_finality: AtomicUsize,
}
impl CanonicalNativeCreditRpc {
    fn new(chain_id: u64) -> Self {
        Self {
            value: Mutex::new("0x2386f26fc10000".into()), // 0.01 ETH exactly.
            latest: Mutex::new("0x65".into()),
            rpc_chain: AtomicU64::new(chain_id),
            transaction_chain: AtomicU64::new(chain_id),
            recipient: Mutex::new("0x1111111111111111111111111111111111111111".into()),
            finalized: Mutex::new(Some("0x65".into())),
            finalized_seen: AtomicUsize::new(0),
            change_inclusion_after_finality: AtomicUsize::new(0),
        }
    }
}

fn synthetic_oracle_updated_at() -> u128 {
    // Round 1 has one immutable publication time; advancing the observer must
    // not synthesize a different source for that same round on every read.
    (chrono::Utc::now().timestamp() - 30).try_into().unwrap()
}

struct SyntheticOracleClock(AtomicU64);

#[async_trait]
impl fin_ethereum::OracleObservationClock for SyntheticOracleClock {
    async fn unix_seconds(&self) -> Result<u64> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

struct DepositPreviewRpc {
    oracle_updated_at: u128,
}
impl DepositPreviewRpc {
    fn new() -> Self {
        Self {
            oracle_updated_at: synthetic_oracle_updated_at(),
        }
    }
    fn abi_word(value: u128) -> [u8; 32] {
        let mut word = [0u8; 32];
        word[16..].copy_from_slice(&value.to_be_bytes());
        word
    }
    fn selector(signature: &str) -> [u8; 4] {
        fin_evm::alloy::primitives::keccak256(signature.as_bytes())[..4]
            .try_into()
            .unwrap()
    }
}

struct ArbitrumTokenRpc {
    token: String,
}

#[async_trait]
impl EvmRpc for ArbitrumTokenRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let selector = |signature: &str| DepositPreviewRpc::selector(signature);
        Ok(match method {
            "eth_chainId" => json!("0xa4b1"),
            "eth_blockNumber" => json!("0x65"),
            "eth_getCode" => json!(if params[0]
                .as_str()
                .unwrap()
                .eq_ignore_ascii_case("0x1111111111111111111111111111111111111111")
            {
                "0x"
            } else {
                "0x6000"
            }),
            "eth_getBalance" => json!("0x16345785d8a0000"),
            "eth_getTransactionCount" => json!("0x0"),
            "eth_estimateGas" => json!("0x11170"),
            "eth_maxPriorityFeePerGas" => json!("0x989680"),
            "eth_getBlockByNumber" => {
                json!({"number":"0x65","hash":format!("0x{}","cd".repeat(32)),"baseFeePerGas":"0x3b9aca00"})
            }
            "eth_call" => {
                let to = params[0]["to"].as_str().unwrap_or_default();
                let data = params[0]["data"].as_str().unwrap_or("0x");
                if !to.eq_ignore_ascii_case(&self.token) || data.len() < 10 {
                    json!("0x")
                } else {
                    let bytes = hex::decode(&data[2..10]).unwrap();
                    let selected: [u8; 4] = bytes.try_into().unwrap();
                    if selected == selector("decimals()") {
                        json!(format!("0x{}", "00".repeat(31) + "06"))
                    } else if selected == selector("symbol()") {
                        let mut result = Vec::from(DepositPreviewRpc::abi_word(32));
                        result.extend_from_slice(&DepositPreviewRpc::abi_word(4));
                        result.extend_from_slice(b"USDC");
                        result.resize(96, 0);
                        json!(fin_evm::hex_bytes(&result))
                    } else if selected == selector("balanceOf(address)") {
                        json!(format!("0x{}", "00".repeat(28) + "05f5e100"))
                    } else if selected == selector("transfer(address,uint256)") {
                        json!(fin_evm::hex_bytes(&DepositPreviewRpc::abi_word(1)))
                    } else {
                        json!("0x")
                    }
                }
            }
            other => panic!("unexpected Arbitrum token preview RPC method {other}"),
        })
    }
}

struct BaseTokenPreviewRpc {
    inner: ArbitrumTokenRpc,
}
#[async_trait]
impl EvmRpc for BaseTokenPreviewRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "eth_chainId" {
            return Ok(json!("0x2105"));
        }
        if method == "eth_call"
            && params[0]["to"].as_str().is_some_and(|to| {
                to.eq_ignore_ascii_case("0x420000000000000000000000000000000000000f")
            })
        {
            let data = params[0]["data"].as_str().unwrap();
            let flag = ["isFjord()", "isIsthmus()", "isJovian()"]
                .iter()
                .any(|sig| {
                    data.starts_with(&format!(
                        "0x{}",
                        hex::encode(DepositPreviewRpc::selector(sig))
                    ))
                });
            return Ok(json!(format!(
                "0x{:064x}",
                if flag { 1u64 } else { 1000u64 }
            )));
        }
        let mut result = self.inner.request(method, params).await?;
        if method == "eth_getBlockByNumber" {
            // The synthetic oracle advertises Jovian: canonical fee evidence
            // must include its complete version-1 header, never omit it.
            result["extraData"] = json!(format!("0x01{}", "00".repeat(16)));
        }
        Ok(result)
    }
}

struct DepositExecutionRpc {
    oracle_updated_at: u128,
    priority_fee: AtomicU64,
    nonce: AtomicU64,
    gas_estimate: AtomicU64,
    simulation_failure: AtomicBool,
    transaction: Arc<Mutex<Option<fin_evm::FrozenTransaction>>>,
    hash: Mutex<Option<String>>,
    accepted: AtomicUsize,
    venue: Arc<SyntheticVenue>,
    journal: Arc<Mutex<Option<fin_domain::Submission>>>,
    durable_store_path: Option<std::path::PathBuf>,
    wallet_address: String,
    wallet_balance: Arc<Mutex<String>>,
    payout_amount: String,
    lose_submit_response: AtomicBool,
}

struct ArbitrumErc20ExecutionRpc {
    token: String,
    transaction: Arc<Mutex<Option<fin_evm::FrozenTransaction>>>,
    hash: Mutex<Option<String>>,
    accepted: AtomicUsize,
    venue: Arc<SyntheticVenue>,
    journal: Arc<Mutex<Option<fin_domain::Submission>>>,
    wallet_address: String,
}

#[async_trait]
impl EvmRpc for ArbitrumErc20ExecutionRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let transaction = self.transaction.lock().unwrap().clone();
        match method {
            "eth_chainId" => Ok(json!("0xa4b1")),
            "eth_blockNumber" => Ok(json!("0x65")),
            "eth_getCode" => Ok(json!(if params[0]
                .as_str()
                .unwrap()
                .eq_ignore_ascii_case("0x1111111111111111111111111111111111111111")
            {
                "0x"
            } else {
                "0x6000"
            })),
            "eth_getBalance" => Ok(json!("0x16345785d8a0000")),
            "eth_getTransactionCount" => Ok(json!("0x0")),
            "eth_estimateGas" => Ok(json!("0x11170")),
            "eth_maxPriorityFeePerGas" => Ok(json!("0x989680")),
            "eth_getBlockByNumber" => {
                let number = if params[0] == "latest" || params[0] == "finalized" {
                    "0x65"
                } else {
                    params[0].as_str().unwrap()
                };
                Ok(
                    json!({"number":number,"hash":format!("0x{}","cd".repeat(32)),"baseFeePerGas":"0x3b9aca00"}),
                )
            }
            "eth_call" => {
                let to = params[0]["to"].as_str().unwrap_or_default();
                let data = params[0]["data"].as_str().unwrap_or("0x");
                if !to.eq_ignore_ascii_case(&self.token) || data.len() < 10 {
                    return Ok(json!("0x"));
                }
                let selector: [u8; 4] = hex::decode(&data[2..10]).unwrap().try_into().unwrap();
                if selector == DepositPreviewRpc::selector("decimals()") {
                    Ok(json!(format!("0x{}", "00".repeat(31) + "06")))
                } else if selector == DepositPreviewRpc::selector("symbol()") {
                    let mut result = Vec::from(DepositPreviewRpc::abi_word(32));
                    result.extend_from_slice(&DepositPreviewRpc::abi_word(4));
                    result.extend_from_slice(b"USDC");
                    result.resize(96, 0);
                    Ok(json!(fin_evm::hex_bytes(&result)))
                } else if selector == DepositPreviewRpc::selector("balanceOf(address)") {
                    Ok(json!(format!("0x{}", "00".repeat(28) + "05f5e100")))
                } else if selector == DepositPreviewRpc::selector("transfer(address,uint256)") {
                    Ok(json!(fin_evm::hex_bytes(&DepositPreviewRpc::abi_word(1))))
                } else {
                    Ok(json!("0x"))
                }
            }
            "eth_sendRawTransaction" => {
                let persisted = self.journal.lock().unwrap().clone().ok_or_else(|| {
                    FinError::new(
                        "JOURNAL_ORDER_INVALID",
                        "test",
                        "No durable submission was stored before broadcast.",
                        "Persist before dispatch.",
                    )
                })?;
                if self.accepted.fetch_add(1, Ordering::SeqCst) != 0 {
                    return Err(FinError::new(
                        "DUPLICATE_BROADCAST",
                        "test",
                        "ERC20 transfer was broadcast twice.",
                        "Reconcile the original hash.",
                    ));
                }
                let raw =
                    hex::decode(params[0].as_str().unwrap().trim_start_matches("0x")).unwrap();
                let hash = format!("{:#x}", fin_evm::alloy::primitives::keccak256(raw));
                if persisted.reference != hash {
                    return Err(FinError::new(
                        "JOURNAL_IDENTITY_MISMATCH",
                        "test",
                        "Persisted ERC20 hash differs from accepted bytes.",
                        "Reconcile the original identity.",
                    ));
                }
                let tx = transaction
                    .ok_or_else(|| FinError::validation("No frozen ERC20 transaction."))?;
                let calldata = hex::decode(tx.calldata.trim_start_matches("0x")).unwrap();
                assert_eq!(
                    &calldata[..4],
                    &DepositPreviewRpc::selector("transfer(address,uint256)")
                );
                let amount = fin_evm::alloy::primitives::U256::from_be_slice(&calldata[36..68]);
                let amount_text = format!(
                    "{}.{:0>6}",
                    amount / fin_evm::alloy::primitives::U256::from(1_000_000u64),
                    amount % fin_evm::alloy::primitives::U256::from(1_000_000u64)
                );
                self.venue.record_coin_deposit_for(
                    EMAIL_A,
                    "USDC",
                    "ARBITRUM",
                    &hash,
                    &amount_text,
                );
                *self.hash.lock().unwrap() = Some(hash.clone());
                Ok(json!(hash))
            }
            "eth_getTransactionReceipt" => {
                let hash = self.hash.lock().unwrap().clone();
                if hash.as_deref() != params[0].as_str() || transaction.is_none() {
                    return Ok(Value::Null);
                }
                let tx = transaction.unwrap();
                let calldata = hex::decode(tx.calldata.trim_start_matches("0x")).unwrap();
                let recipient = format!("0x{}", hex::encode(&calldata[16..36]));
                let amount = format!("0x{}", hex::encode(&calldata[36..68]));
                let transfer_topic = format!(
                    "{:#x}",
                    fin_evm::alloy::primitives::keccak256("Transfer(address,address,uint256)")
                );
                let from_topic = format!(
                    "0x{}{}",
                    "0".repeat(24),
                    self.wallet_address
                        .trim_start_matches("0x")
                        .to_ascii_lowercase()
                );
                let to_topic = format!(
                    "0x{}{}",
                    "0".repeat(24),
                    recipient.trim_start_matches("0x").to_ascii_lowercase()
                );
                Ok(
                    json!({"transactionHash":hash,"blockHash":format!("0x{}","cd".repeat(32)),"blockNumber":"0x64","status":"0x1","gasUsed":"0x11170","effectiveGasPrice":"0x77359400","logs":[{"address":self.token,"topics":[transfer_topic,from_topic,to_topic],"data":amount,"blockHash":format!("0x{}","cd".repeat(32)),"blockNumber":"0x64","transactionHash":hash,"logIndex":"0x0","removed":false}]}),
                )
            }
            "eth_getTransactionByHash" => {
                let hash = self.hash.lock().unwrap().clone();
                if hash.as_deref() != params[0].as_str() || transaction.is_none() {
                    return Ok(Value::Null);
                }
                let tx = transaction.unwrap();
                Ok(
                    json!({"hash":hash,"from":tx.sender,"to":tx.to,"nonce":format!("{:#x}",tx.nonce),"chainId":format!("{:#x}",tx.chain_id),"type":"0x2","gas":format!("{:#x}",tx.gas),"value":format!("{:#x}",fin_evm::alloy::primitives::U256::from_str(tx.value.as_str()).unwrap()),"input":tx.calldata,"maxFeePerGas":format!("{:#x}",fin_evm::alloy::primitives::U256::from_str(tx.max_fee_per_gas.as_str()).unwrap()),"maxPriorityFeePerGas":format!("{:#x}",fin_evm::alloy::primitives::U256::from_str(tx.max_priority_fee_per_gas.as_str()).unwrap()),"accessList":[],"blockHash":format!("0x{}","cd".repeat(32)),"blockNumber":"0x64"}),
                )
            }
            other => panic!("unexpected ERC20 deposit RPC method {other}"),
        }
    }
}

struct RecordingSigner {
    signer: fin_evm::LocalEnvelopeSigner,
    transaction: Arc<Mutex<Option<fin_evm::FrozenTransaction>>>,
}

#[async_trait]
impl fin_evm::EnvelopeSigner for RecordingSigner {
    async fn sign_prepared(
        &self,
        transaction: &fin_evm::FrozenTransaction,
    ) -> Result<fin_evm::alloy::primitives::Bytes> {
        *self.transaction.lock().unwrap() = Some(transaction.clone());
        self.signer.sign_prepared(transaction).await
    }
}

#[derive(Default)]
struct SyntheticSubmissionJournal {
    inner: Arc<Mutex<Option<fin_domain::Submission>>>,
}

impl fin_domain::SubmissionJournal for SyntheticSubmissionJournal {
    fn assert_authorized(&self) -> Result<()> {
        Ok(())
    }
    fn persist_submission(&self, submission: &fin_domain::Submission) -> Result<()> {
        let mut slot = self.inner.lock().unwrap();
        if slot.is_some() {
            return Err(FinError::new(
                "SYNTHETIC_DUPLICATE_SUBMISSION",
                "test",
                "A submission identity was persisted twice.",
                "Do not replay an accepted write.",
            ));
        }
        *slot = Some(submission.clone());
        Ok(())
    }
}

#[async_trait]
impl EvmRpc for DepositExecutionRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let deposit = "0x1111111111111111111111111111111111111111";
        let wallet = self.wallet_address.as_str();
        let payout_hash = format!("0x{}", "ee".repeat(32));
        let oracle = fin_ethereum::EthereumConfig::default()
            .aave
            .expect("mainnet Aave")
            .oracle;
        match method {
            "eth_sendRawTransaction" => {
                let persisted = self.journal.lock().unwrap().clone().or_else(|| {
                    let path = self.durable_store_path.as_ref()?;
                    let store = fin_store::Store::open(path).ok()?;
                    let records = store.list_records().ok()?;
                    records.into_iter().find_map(|record| {
                        (record["request_id"] == "scoped-deposit-forward")
                            .then(|| serde_json::from_value(record["submission"].clone()).ok())
                            .flatten()
                    })
                });
                if persisted.is_none() {
                    return Err(FinError::new(
                        "JOURNAL_ORDER_INVALID",
                        "test",
                        "No pre-dispatch original identity was persisted.",
                        "Persist before sending.",
                    ));
                }
                if self.accepted.fetch_add(1, Ordering::SeqCst) != 0 {
                    return Err(FinError::new(
                        "DUPLICATE_BROADCAST",
                        "test",
                        "Synthetic write was broadcast twice.",
                        "Reconcile the original hash.",
                    ));
                }
                let raw =
                    hex::decode(params[0].as_str().unwrap().trim_start_matches("0x")).unwrap();
                let hash = format!("{:#x}", fin_evm::alloy::primitives::keccak256(raw));
                if persisted
                    .as_ref()
                    .is_some_and(|submission| submission.reference != hash)
                {
                    return Err(FinError::new(
                        "JOURNAL_IDENTITY_MISMATCH",
                        "test",
                        "Persisted transaction hash differs from accepted bytes.",
                        "Reconcile the original identity.",
                    ));
                }
                *self.hash.lock().unwrap() = Some(hash.clone());
                let transaction = self.transaction.lock().unwrap().clone().unwrap();
                self.venue
                    .record_deposit(&hash, &transaction.value.to_decimal(18));
                adjust_native_balance(
                    &self.wallet_balance,
                    &transaction.value.to_decimal(18),
                    false,
                );
                adjust_native_balance(&self.wallet_balance, "0.000042", false);
                if self.lose_submit_response.swap(false, Ordering::SeqCst) {
                    self.venue.state.lock().unwrap().deposit_history[0]["status"] = json!(0);
                    return Err(FinError::new(
                        "SUBMISSION_RESPONSE_LOST",
                        "submission",
                        "Synthetic RPC accepted the durable transaction but lost its response.",
                        "Reconcile the original submission identity.",
                    ));
                }
                Ok(json!(hash))
            }
            "eth_chainId" => Ok(json!("0x1")),
            "eth_blockNumber" => Ok(json!("0x65")),
            "eth_getTransactionReceipt" => {
                let hash = self.hash.lock().unwrap().clone();
                if payout_hash == params[0].as_str().unwrap_or_default() {
                    return Ok(
                        json!({"transactionHash":payout_hash,"blockHash":format!("0x{}", "cd".repeat(32)),"blockNumber":"0x64","status":"0x1","gasUsed":"0x5208","effectiveGasPrice":"0x77359400","logs":[]}),
                    );
                }
                if hash.as_deref() != params[0].as_str() {
                    return Ok(Value::Null);
                }
                Ok(
                    json!({"transactionHash":hash,"blockHash":format!("0x{}", "cd".repeat(32)),"blockNumber":"0x64","status":"0x1","gasUsed":"0x5208","effectiveGasPrice":"0x77359400","logs":[]}),
                )
            }
            "eth_getTransactionByHash" => {
                let hash = self.hash.lock().unwrap().clone();
                let transaction = self.transaction.lock().unwrap().clone();
                if payout_hash == params[0].as_str().unwrap_or_default() {
                    let value =
                        fin_evm::u256(&Quantity::from_decimal(&self.payout_amount, 18).unwrap())
                            .unwrap();
                    return Ok(
                        json!({"hash":payout_hash,"from":"0x3333333333333333333333333333333333333333","to":wallet,"nonce":"0x2","chainId":"0x1","type":"0x2","gas":"0x5208","value":format!("{value:#x}"),"input":"0x","maxFeePerGas":"0x77359400","maxPriorityFeePerGas":"0x989680","accessList":[],"blockHash":format!("0x{}", "cd".repeat(32)),"blockNumber":"0x64"}),
                    );
                }
                if hash.as_deref() != params[0].as_str() || transaction.is_none() {
                    return Ok(Value::Null);
                }
                let tx = transaction.unwrap();
                let to_hex = |quantity: &str| {
                    let value = fin_evm::alloy::primitives::U256::from_str(quantity).unwrap();
                    format!("{value:#x}")
                };
                Ok(
                    json!({"hash":hash,"from":tx.sender,"to":tx.to,"nonce":format!("{:#x}",tx.nonce),"chainId":format!("{:#x}",tx.chain_id),"type":"0x2","gas":format!("{:#x}",tx.gas),"value":to_hex(tx.value.as_str()),"input":tx.calldata,"maxFeePerGas":to_hex(tx.max_fee_per_gas.as_str()),"maxPriorityFeePerGas":to_hex(tx.max_priority_fee_per_gas.as_str()),"accessList":[],"blockHash":format!("0x{}", "cd".repeat(32)),"blockNumber":"0x64"}),
                )
            }
            "eth_getBlockByNumber" => {
                let number = if params[0] == "latest" {
                    "0x65"
                } else {
                    params[0].as_str().unwrap()
                };
                let mut value = json!({"hash":format!("0x{}", "cd".repeat(32)),"number":number});
                value["baseFeePerGas"] = json!("0x3b9aca00");
                Ok(value)
            }
            "eth_getCode" => Ok(json!(if [deposit, wallet]
                .iter()
                .any(|address| params[0].as_str().unwrap().eq_ignore_ascii_case(address))
            {
                "0x"
            } else {
                "0x6000"
            })),
            "eth_getBalance" => Ok(json!("0x16345785d8a0000")),
            "eth_getTransactionCount" => {
                Ok(json!(format!("{:#x}", self.nonce.load(Ordering::SeqCst))))
            }
            "eth_estimateGas" => Ok(json!(format!(
                "{:#x}",
                self.gas_estimate.load(Ordering::SeqCst)
            ))),
            "eth_maxPriorityFeePerGas" => Ok(json!(format!(
                "{:#x}",
                self.priority_fee.load(Ordering::SeqCst)
            ))),
            "eth_call" => {
                if self.simulation_failure.load(Ordering::SeqCst) {
                    return Err(FinError::new(
                        "SIMULATION_REVERTED",
                        "test",
                        "Synthetic transfer simulation reverted.",
                        "Keep the approved operation blocked.",
                    ));
                }
                let to = params[0]["to"].as_str().unwrap_or("");
                let data = params[0]["data"].as_str().unwrap_or("0x");
                if to.eq_ignore_ascii_case(&oracle) && data.len() >= 10 {
                    let selector = hex::decode(&data[2..10]).unwrap();
                    let result = if selector == DepositPreviewRpc::selector("BASE_CURRENCY()") {
                        vec![0u8; 32]
                    } else if selector == DepositPreviewRpc::selector("BASE_CURRENCY_UNIT()") {
                        DepositPreviewRpc::abi_word(100_000_000).to_vec()
                    } else if selector == DepositPreviewRpc::selector("getAssetPrice(address)") {
                        DepositPreviewRpc::abi_word(200_000_000_000).to_vec()
                    } else if selector == DepositPreviewRpc::selector("getSourceOfAsset(address)") {
                        let mut word = [0u8; 32];
                        word[12..].copy_from_slice(&[0x33; 20]);
                        word.to_vec()
                    } else {
                        return Err(FinError::validation(
                            "Unexpected synthetic oracle selector.",
                        ));
                    };
                    Ok(json!(fin_evm::hex_bytes(&result)))
                } else if data.len() >= 10
                    && hex::decode(&data[2..10]).unwrap()
                        == DepositPreviewRpc::selector("latestRoundData()")
                {
                    let mut result = Vec::new();
                    for value in [
                        1,
                        200_000_000_000,
                        self.oracle_updated_at,
                        self.oracle_updated_at,
                        1,
                    ] {
                        result.extend_from_slice(&DepositPreviewRpc::abi_word(value));
                    }
                    Ok(json!(fin_evm::hex_bytes(&result)))
                } else {
                    Ok(json!("0x"))
                }
            }
            method => panic!("unexpected synthetic deposit execution RPC method {method}"),
        }
    }
}

#[async_trait]
impl EvmRpc for DepositPreviewRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let deposit = "0x1111111111111111111111111111111111111111";
        let oracle = fin_ethereum::EthereumConfig::default()
            .aave
            .expect("mainnet Aave")
            .oracle;
        Ok(match method {
            "eth_chainId" => json!("0x1"),
            "eth_getCode" => json!(
                if params[0].as_str().unwrap().eq_ignore_ascii_case(deposit) {
                    "0x"
                } else {
                    "0x6000"
                }
            ),
            "eth_getBalance" => json!("0x16345785d8a0000"),
            "eth_getTransactionCount" => json!("0x0"),
            "eth_estimateGas" => json!("0x5208"),
            "eth_maxPriorityFeePerGas" => json!("0x989680"),
            "eth_getBlockByNumber" => {
                json!({"number":"0x65","hash":format!("0x{}", "cd".repeat(32)),"baseFeePerGas":"0x3b9aca00"})
            }
            "eth_call" => {
                let request = &params[0];
                let data = request["data"].as_str().unwrap_or("0x");
                if request["to"]
                    .as_str()
                    .is_some_and(|to| to.eq_ignore_ascii_case(&oracle))
                    && data.len() >= 10
                {
                    let selector = hex::decode(&data[2..10]).unwrap();
                    let result = if selector == Self::selector("BASE_CURRENCY()") {
                        vec![0u8; 32]
                    } else if selector == Self::selector("BASE_CURRENCY_UNIT()") {
                        Self::abi_word(100_000_000).to_vec()
                    } else if selector == Self::selector("getAssetPrice(address)") {
                        Self::abi_word(200_000_000_000).to_vec()
                    } else if selector == Self::selector("getSourceOfAsset(address)") {
                        let mut word = [0u8; 32];
                        word[12..].copy_from_slice(&[0x33; 20]);
                        word.to_vec()
                    } else {
                        return Err(FinError::validation(
                            "Unexpected synthetic oracle selector.",
                        ));
                    };
                    json!(fin_evm::hex_bytes(&result))
                } else if data.len() >= 10 {
                    let selector = hex::decode(&data[2..10]).unwrap();
                    if selector == Self::selector("latestRoundData()") {
                        let mut result = Vec::new();
                        for value in [
                            1,
                            200_000_000_000,
                            self.oracle_updated_at,
                            self.oracle_updated_at,
                            1,
                        ] {
                            result.extend_from_slice(&Self::abi_word(value));
                        }
                        json!(fin_evm::hex_bytes(&result))
                    } else {
                        json!("0x")
                    }
                } else {
                    json!("0x")
                }
            }
            method => panic!("unexpected synthetic preview RPC method {method}"),
        })
    }
}

#[async_trait]
impl EvmRpc for CanonicalNativeCreditRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let tx_hash = format!("0x{}", "ab".repeat(32));
        let block_hash = format!("0x{}", "cd".repeat(32));
        Ok(match method {
            "eth_chainId" => json!(format!("{:#x}", self.rpc_chain.load(Ordering::SeqCst))),
            "eth_blockNumber" => json!(*self.latest.lock().unwrap()),
            "eth_getTransactionReceipt" => json!({
                "transactionHash":tx_hash,
                "blockHash":block_hash,
                "blockNumber":"0x64",
                "status":"0x1",
                "gasUsed":"0x5208",
                "effectiveGasPrice":"0x3b9aca00",
                "logs":[]
            }),
            "eth_getTransactionByHash" => json!({
                "hash":tx_hash,
                "from":"0x2222222222222222222222222222222222222222",
                "to":*self.recipient.lock().unwrap(),
                "value":*self.value.lock().unwrap(),
                "chainId":format!("{:#x}", self.transaction_chain.load(Ordering::SeqCst)),
                "blockHash":block_hash,
                "blockNumber":"0x64"
            }),
            "eth_getBlockByNumber" => {
                if params[0] == "finalized" {
                    self.finalized_seen.store(1, Ordering::SeqCst);
                    match self.finalized.lock().unwrap().as_ref() {
                        Some(number) => json!({"hash":block_hash,"number":number}),
                        None => Value::Null,
                    }
                } else {
                    assert_eq!(params[0], "0x64");
                    let hash = if self.finalized_seen.load(Ordering::SeqCst) == 1
                        && self.change_inclusion_after_finality.load(Ordering::SeqCst) == 1
                    {
                        format!("0x{}", "ef".repeat(32))
                    } else {
                        block_hash
                    };
                    json!({"hash":hash,"number":"0x64"})
                }
            }
            "eth_getCode" => json!("0x"),
            "eth_getBalance" => json!("0x16345785d8a0000"),
            method => panic!("unexpected isolated payout proof RPC method {method}"),
        })
    }
}

#[tokio::test]
async fn canonical_native_payout_proof_checks_inclusion_finality_recipient_and_exact_amount() {
    let rpc = Arc::new(CanonicalNativeCreditRpc::new(1));
    let wallet = "0x1111111111111111111111111111111111111111";
    let ethereum = fin_ethereum::EthereumAdapter::new(
        fin_ethereum::EthereumConfig::default(),
        vec![fin_ethereum::WalletConnection {
            name: "synthetic-payout-recipient".into(),
            address: wallet.into(),
            signing_enabled: false,
        }],
        Arc::new(fin_evm::EvmRuntime::new(rpc.clone(), 1, None)),
    )
    .unwrap();
    let hash = format!("0x{}", "ab".repeat(32));
    let native = AssetId {
        network: "ethereum:1".into(),
        reference: "native".into(),
        decimals: 18,
    };
    let proof = ethereum
        .verify_external_credit(&hash, wallet, &native, "0.01", "0.01", 2)
        .await
        .unwrap();
    assert_eq!(proof.status, fin_domain::Status::Completed);
    assert_eq!(
        proof.amount_atoms.unwrap(),
        Quantity::from_decimal("0.01", 18).unwrap()
    );
    assert_eq!(
        proof.evidence["proof_method"],
        "canonical-direct-native-value"
    );

    let unconfirmed = CanonicalNativeCreditRpc::new(1);
    *unconfirmed.latest.lock().unwrap() = "0x64".into();
    let pending = fin_ethereum::EthereumAdapter::new(
        fin_ethereum::EthereumConfig::default(),
        vec![fin_ethereum::WalletConnection {
            name: "synthetic-payout-recipient".into(),
            address: wallet.into(),
            signing_enabled: false,
        }],
        Arc::new(fin_evm::EvmRuntime::new(Arc::new(unconfirmed), 1, None)),
    )
    .unwrap()
    .verify_external_credit(&hash, wallet, &native, "0.01", "0.01", 2)
    .await
    .unwrap();
    assert_eq!(pending.status, fin_domain::Status::Pending);

    *rpc.value.lock().unwrap() = "0x1".into();
    assert_eq!(
        ethereum
            .verify_external_credit(&hash, wallet, &native, "0.01", "0.01", 2)
            .await
            .unwrap_err()
            .code,
        "FUNDING_AMOUNT_MISMATCH"
    );
}

#[tokio::test]
async fn arbitrum_payout_verifier_recovers_legacy_confirmation_count_at_sequencer_inclusion() {
    use fin_binance::{BinanceRecipientVerifier, PayoutRequest};
    let rpc = Arc::new(CanonicalNativeCreditRpc::new(42161));
    let recipient = "0x1111111111111111111111111111111111111111";
    let ethereum = Arc::new(
        fin_ethereum::EthereumAdapter::new(
            fin_ethereum::EthereumConfig::wallet_only(42161, 2, vec![]),
            vec![fin_ethereum::WalletConnection {
                name: "synthetic-arbitrum-payout".into(),
                address: recipient.into(),
                signing_enabled: false,
            }],
            Arc::new(fin_evm::EvmRuntime::new(rpc.clone(), 42161, None)),
        )
        .unwrap(),
    );
    let verifier =
        fin_broker::EthereumPayoutVerifier::new(BTreeMap::from([(42161, ethereum)])).unwrap();
    let request = PayoutRequest {
        network: "ARBITRUM".into(),
        memo: None,
        tx_hash: format!("0x{}", "ab".repeat(32)),
        chain_id: 42161,
        recipient: recipient.into(),
        asset: AssetId {
            network: "ethereum:42161".into(),
            reference: "native".into(),
            decimals: 18,
        },
        minimum_amount: "0.01".into(),
        maximum_amount: "0.01".into(),
        confirmations: 2,
    };
    // A historical request retains its stored two-confirmation identity, but
    // the reviewed incoming-credit policy requires only sequencer inclusion.
    *rpc.latest.lock().unwrap() = "0x64".into();
    for finalized in [None, Some("0x63".to_owned()), Some("invalid".to_owned())] {
        *rpc.finalized.lock().unwrap() = finalized;
        let completed = verifier.verify(request.clone()).await;
        assert_eq!(completed.status, fin_domain::Status::Completed);
        assert_eq!(
            completed.amount_atoms,
            Some(Quantity::from_decimal("0.01", 18).unwrap())
        );
        assert_eq!(
            completed.evidence["proof_method"],
            "canonical-direct-native-value"
        );
        assert_eq!(completed.evidence["chain_id"], 42161);
        assert_eq!(completed.evidence["finality"], "sequencer-inclusion");
        assert_eq!(rpc.finalized_seen.load(Ordering::SeqCst), 0);
    }
    *rpc.latest.lock().unwrap() = "0x63".into();
    assert_eq!(
        verifier.verify(request.clone()).await.status,
        fin_domain::Status::Pending
    );
    *rpc.latest.lock().unwrap() = "0x64".into();
    *rpc.recipient.lock().unwrap() = "0x3333333333333333333333333333333333333333".into();
    let wrong_recipient = verifier.verify(request.clone()).await;
    assert_eq!(wrong_recipient.status, fin_domain::Status::Unknown);
    assert_eq!(
        wrong_recipient.evidence["code"],
        "FUNDING_RECIPIENT_MISMATCH"
    );
    *rpc.recipient.lock().unwrap() = recipient.into();
    *rpc.value.lock().unwrap() = "0x1".into();
    let partial = verifier.verify(request.clone()).await;
    assert_eq!(partial.status, fin_domain::Status::Unknown);
    assert_eq!(partial.evidence["code"], "FUNDING_AMOUNT_MISMATCH");
    *rpc.value.lock().unwrap() = "0x2386f26fc10000".into();
    rpc.transaction_chain.store(1, Ordering::SeqCst);
    let wrong_transaction_chain = verifier.verify(request.clone()).await;
    assert_eq!(wrong_transaction_chain.status, fin_domain::Status::Unknown);
    assert_eq!(
        wrong_transaction_chain.evidence["code"],
        "FUNDING_IDENTITY_MISMATCH"
    );
    rpc.transaction_chain.store(42161, Ordering::SeqCst);
    rpc.rpc_chain.store(1, Ordering::SeqCst);
    let wrong_rpc_chain = verifier.verify(request.clone()).await;
    assert_eq!(wrong_rpc_chain.status, fin_domain::Status::Unknown);
    assert!(wrong_rpc_chain.amount_atoms.is_none());
    rpc.rpc_chain.store(42161, Ordering::SeqCst);
    let mut wrong_asset = request.clone();
    wrong_asset.asset.network = "ethereum:1".into();
    let wrong_asset = verifier.verify(wrong_asset).await;
    assert_eq!(wrong_asset.status, fin_domain::Status::Unknown);
    assert_eq!(wrong_asset.evidence["code"], "WRONG_CHAIN");
    assert_eq!(
        verifier.verify(request).await.status,
        fin_domain::Status::Completed
    );
}

#[tokio::test]
async fn base_payout_requires_full_canonical_l2_inclusion_and_exact_identity() {
    use fin_binance::{BinanceRecipientVerifier, PayoutRequest};
    let rpc = Arc::new(CanonicalNativeCreditRpc::new(8453));
    let recipient = "0x1111111111111111111111111111111111111111";
    let ethereum = Arc::new(
        fin_ethereum::EthereumAdapter::new(
            fin_ethereum::EthereumConfig::wallet_only(8453, 2, vec![]),
            vec![fin_ethereum::WalletConnection {
                name: "synthetic-base-payout".into(),
                address: recipient.into(),
                signing_enabled: false,
            }],
            Arc::new(fin_evm::EvmRuntime::new(rpc.clone(), 8453, None)),
        )
        .unwrap(),
    );
    let verifier =
        fin_broker::EthereumPayoutVerifier::new(BTreeMap::from([(8453, ethereum)])).unwrap();
    let request = PayoutRequest {
        network: "BASE".into(),
        memo: None,
        tx_hash: format!("0x{}", "ab".repeat(32)),
        chain_id: 8453,
        recipient: recipient.into(),
        asset: AssetId {
            network: "ethereum:8453".into(),
            reference: "native".into(),
            decimals: 18,
        },
        minimum_amount: "0.01".into(),
        maximum_amount: "0.01".into(),
        confirmations: 2,
    };
    // A historical request retains its stored two-confirmation identity, but
    // the reviewed incoming-credit policy requires only sequencer inclusion.
    *rpc.latest.lock().unwrap() = "0x64".into();
    for finalized in [None, Some("0x63".to_owned()), Some("invalid".to_owned())] {
        *rpc.finalized.lock().unwrap() = finalized;
        let completed = verifier.verify(request.clone()).await;
        assert_eq!(completed.status, fin_domain::Status::Completed);
        assert_eq!(
            completed.amount_atoms,
            Some(Quantity::from_decimal("0.01", 18).unwrap())
        );
        assert_eq!(
            completed.evidence["proof_method"],
            "canonical-direct-native-value"
        );
        assert_eq!(completed.evidence["chain_id"], 8453);
        assert_eq!(completed.evidence["finality"], "sequencer-inclusion");
        assert_eq!(rpc.finalized_seen.load(Ordering::SeqCst), 0);
    }
    *rpc.latest.lock().unwrap() = "0x63".into();
    assert_eq!(
        verifier.verify(request.clone()).await.status,
        fin_domain::Status::Pending
    );
    *rpc.latest.lock().unwrap() = "0x64".into();
    *rpc.recipient.lock().unwrap() = "0x3333333333333333333333333333333333333333".into();
    let wrong_recipient = verifier.verify(request.clone()).await;
    assert_eq!(wrong_recipient.status, fin_domain::Status::Unknown);
    assert_eq!(
        wrong_recipient.evidence["code"],
        "FUNDING_RECIPIENT_MISMATCH"
    );
    *rpc.recipient.lock().unwrap() = recipient.into();
    *rpc.value.lock().unwrap() = "0x1".into();
    let partial = verifier.verify(request.clone()).await;
    assert_eq!(partial.status, fin_domain::Status::Unknown);
    assert_eq!(partial.evidence["code"], "FUNDING_AMOUNT_MISMATCH");
    *rpc.value.lock().unwrap() = "0x2386f26fc10000".into();
    rpc.transaction_chain.store(1, Ordering::SeqCst);
    let wrong_transaction_chain = verifier.verify(request.clone()).await;
    assert_eq!(wrong_transaction_chain.status, fin_domain::Status::Unknown);
    assert_eq!(
        wrong_transaction_chain.evidence["code"],
        "FUNDING_IDENTITY_MISMATCH"
    );
    rpc.transaction_chain.store(8453, Ordering::SeqCst);
    rpc.rpc_chain.store(1, Ordering::SeqCst);
    let wrong_rpc_chain = verifier.verify(request.clone()).await;
    assert_eq!(wrong_rpc_chain.status, fin_domain::Status::Unknown);
    assert!(wrong_rpc_chain.amount_atoms.is_none());
    rpc.rpc_chain.store(8453, Ordering::SeqCst);
    let mut wrong_asset = request.clone();
    wrong_asset.asset.network = "ethereum:1".into();
    let wrong_asset = verifier.verify(wrong_asset).await;
    assert_eq!(wrong_asset.status, fin_domain::Status::Unknown);
    assert_eq!(wrong_asset.evidence["code"], "WRONG_CHAIN");
    assert_eq!(
        verifier.verify(request).await.status,
        fin_domain::Status::Completed
    );
}

#[tokio::test]
async fn base_deposit_preview_uses_authenticated_exact_asset_and_total_fee_bound() {
    let venue = Arc::new(SyntheticVenue::new());
    let token = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
    let wallet = Arc::new(
        fin_ethereum::EthereumAdapter::new(
            fin_ethereum::EthereumConfig::wallet_only(8453, 8, vec![]),
            vec![fin_ethereum::WalletConnection {
                name: "synthetic-wallet".into(),
                address: "0x2222222222222222222222222222222222222222".into(),
                signing_enabled: true,
            }],
            Arc::new(fin_evm::EvmRuntime::new(
                Arc::new(BaseTokenPreviewRpc {
                    inner: ArbitrumTokenRpc {
                        token: token.into(),
                    },
                }),
                8453,
                None,
            )),
        )
        .unwrap()
        .with_wallet_valuation(Arc::new(SyntheticWalletPrice)),
    );
    let deposit = fin_broker::BinanceEthereumDepositAdapter::new(
        subaccount_only_adapter(venue.clone()),
        wallet,
    );
    let input = json!({"requestId":"synthetic-base-token","account":"child-a","wallet":"synthetic-wallet",
        "coin":"USDC","network":"BASE","token":token,"amount":"1.25","maxFeeEth":"0.001"});
    let prepared = deposit.prepare("binance.deposit", &input).await.unwrap();
    assert_eq!(prepared.binding["source_asset"]["network"], "ethereum:8453");
    assert_eq!(prepared.binding["source_asset"]["reference"], token);
    assert_eq!(prepared.binding["source_asset"]["decimals"], 6);
    assert_eq!(prepared.binding["target"]["chain_id"], 8453);
    assert_eq!(prepared.binding["target"]["network"], "BASE");
    assert_eq!(prepared.intent.debits[0].amount.atoms.to_decimal(6), "1.25");
    let transaction = &prepared.binding["inner_preview"]["binding"]["transaction"];
    assert_eq!(transaction["rollup_fee"]["version"], 1);
    assert_eq!(transaction["rollup_fee"]["maximum_l1_data_fee"], "1500");
    assert_eq!(transaction["rollup_fee"]["maximum_operator_fee"], "1500");
    let native = json!({"requestId":"synthetic-base-native","account":"child-a","wallet":"synthetic-wallet",
        "coin":"ETH","network":"BASE","token":"native","amount":"0.01","maxFeeEth":"0.001"});
    let prepared_native = deposit.prepare("binance.deposit", &native).await.unwrap();
    assert_eq!(
        prepared_native.binding["source_asset"]["reference"],
        "native"
    );
    assert_eq!(
        prepared_native.binding["source_asset"]["network"],
        "ethereum:8453"
    );
    assert_eq!(prepared_native.binding["source_asset"]["decimals"], 18);
    assert_eq!(prepared_native.binding["target"]["network"], "BASE");
    let mut wrong = input.clone();
    wrong["token"] = json!("0x1111111111111111111111111111111111111111");
    assert_eq!(
        deposit
            .prepare("binance.deposit", &wrong)
            .await
            .unwrap_err()
            .code,
        "ASSET_IDENTITY_MISMATCH"
    );
    venue.state.lock().unwrap().deposit_enabled = false;
    assert_eq!(
        deposit
            .prepare("binance.deposit", &input)
            .await
            .unwrap_err()
            .code,
        "DEPOSIT_DISABLED"
    );
}

#[tokio::test]
async fn binance_deposit_preview_freezes_dynamic_main_or_subaccount_target_and_exact_cross_asset_amount()
 {
    let venue = Arc::new(SyntheticVenue::new());
    let binance = adapter(venue.clone());
    let wallet_address = "0x2222222222222222222222222222222222222222";
    let ethereum = Arc::new(
        fin_ethereum::EthereumAdapter::new(
            fin_ethereum::EthereumConfig::default(),
            vec![fin_ethereum::WalletConnection {
                name: "synthetic-wallet".into(),
                address: wallet_address.into(),
                signing_enabled: true,
            }],
            Arc::new(fin_evm::EvmRuntime::new(
                Arc::new(DepositPreviewRpc::new()),
                1,
                None,
            )),
        )
        .unwrap(),
    );
    let composite = fin_broker::BinanceEthereumDepositAdapter::new(binance, ethereum);
    let input = json!({"requestId":"synthetic-deposit-1","account":"main","wallet":"synthetic-wallet","amount":"0.01","maxFeeEth":"0.001"});
    let prepared = composite.prepare("binance.deposit", &input).await.unwrap();
    assert_eq!(prepared.operation, "binance.deposit");
    assert_eq!(prepared.intent.expected.len(), 1);
    match &prepared.intent.expected[0] {
        fin_domain::EffectConstraint::Transfer {
            from,
            to,
            debit_asset,
            credit_asset,
            maximum_debit,
            minimum_credit,
        } => {
            assert_eq!(from.0, format!("ethereum:1:{wallet_address}"));
            assert_eq!(to.0, resource(1001).0);
            assert_eq!(debit_asset.network, "ethereum:1");
            assert_eq!(debit_asset.decimals, 18);
            assert_eq!(credit_asset.network, "binance");
            assert_eq!(credit_asset.reference, "ETH");
            assert_eq!(credit_asset.decimals, 36);
            assert_eq!(maximum_debit.to_decimal(18), "0.01");
            assert_eq!(minimum_credit.to_decimal(36), "0.01");
        }
        other => panic!("unexpected composite constraint {other:?}"),
    }

    let below_minimum = json!({"requestId":"synthetic-deposit-2","account":"main","wallet":"synthetic-wallet","amount":"0.0009","maxFeeEth":"0.001"});
    assert_eq!(
        composite
            .prepare("binance.deposit", &below_minimum)
            .await
            .unwrap_err()
            .code,
        "DEPOSIT_MINIMUM"
    );
    let child = json!({"requestId":"synthetic-deposit-3","account":"child-a","wallet":"synthetic-wallet","amount":"0.01","maxFeeEth":"0.001"});
    let child_preview = composite.prepare("binance.deposit", &child).await.unwrap();
    assert_eq!(
        child_preview.binding["target"]["account_resource"],
        resource(1002).0
    );
}

#[tokio::test]
async fn subaccount_only_credentials_and_dynamic_arbitrum_usdc_route_are_supported() {
    let venue = Arc::new(SyntheticVenue::new());
    let binance = subaccount_only_adapter(venue.clone());
    let connections = binance.connections();
    let account = connections
        .iter()
        .find(|connection| connection.provider == "binance")
        .unwrap();
    assert_eq!(account.role.as_deref(), Some("sub"));
    assert!(account.parent.is_none());
    assert_eq!(connections.len(), 1);
    let identities = binance.authorization_identities();
    let destination = identities
        .iter()
        .find(|identity| identity.name == "synthetic-wallet")
        .unwrap();
    assert_eq!(destination.role.as_deref(), Some("deposit-destination"));
    assert_eq!(destination.network.as_deref(), Some("ethereum:1"));
    assert_eq!(
        destination.address.as_deref(),
        Some("0x2222222222222222222222222222222222222222")
    );

    let token = "0xaf88d065e77c8cC2239327C5EDb3A432268e5831";
    let wallet_address = "0x2222222222222222222222222222222222222222";
    let ethereum = Arc::new(
        fin_ethereum::EthereumAdapter::new(
            fin_ethereum::EthereumConfig::wallet_only(42161, 2, vec![]),
            vec![fin_ethereum::WalletConnection {
                name: "synthetic-wallet".into(),
                address: wallet_address.into(),
                signing_enabled: true,
            }],
            Arc::new(fin_evm::EvmRuntime::new(
                Arc::new(ArbitrumTokenRpc {
                    token: token.into(),
                }),
                42161,
                None,
            )),
        )
        .unwrap()
        .with_wallet_valuation(Arc::new(SyntheticWalletPrice)),
    );
    let composite = fin_broker::BinanceEthereumDepositAdapter::new(binance, ethereum);
    let input = json!({"requestId":"sub-usdc-arbitrum","account":"child-a","wallet":"synthetic-wallet","amount":"1.25","maxFeeEth":"0.001","coin":"USDC","network":"ARBITRUM","chainId":42161,"token":token});
    let preview = composite.prepare("binance.deposit", &input).await.unwrap();
    assert_eq!(
        preview.binding["target"]["account_resource"],
        resource(1002).0
    );
    assert_eq!(preview.binding["target"]["network"], "ARBITRUM");
    assert_eq!(preview.binding["target"]["chain_id"], 42161);
    assert_eq!(
        preview.binding["source_asset"]["reference"],
        token.to_ascii_lowercase()
    );
    assert_eq!(preview.binding["source_asset"]["decimals"], 6);
    assert_eq!(
        preview.binding["inner_input"]["token"],
        token.to_ascii_lowercase()
    );
    assert_eq!(preview.intent.debits[0].amount.atoms.to_decimal(6), "1.25");
    match &preview.intent.expected[0] {
        fin_domain::EffectConstraint::Transfer {
            minimum_credit,
            credit_asset,
            debit_asset,
            ..
        } => {
            assert_eq!(minimum_credit.to_decimal(36), "1.25");
            assert_eq!(credit_asset.reference, "USDC");
            assert_eq!(debit_asset.reference, token.to_ascii_lowercase());
        }
        other => panic!("unexpected deposit effect constraint {other:?}"),
    }

    let arbitrum_eth = json!({"requestId":"sub-eth-arbitrum","account":"child-a","wallet":"synthetic-wallet","amount":"0.01","maxFeeEth":"0.001","coin":"ETH","network":"ARBITRUM","chainId":42161,"token":"native"});
    let native_preview = composite
        .prepare("binance.deposit", &arbitrum_eth)
        .await
        .unwrap();
    assert_eq!(native_preview.binding["target"]["chain_id"], 42161);
    assert_eq!(native_preview.binding["target"]["network"], "ARBITRUM");
    assert_eq!(
        native_preview.binding["source_asset"]["reference"],
        "native"
    );
    assert_eq!(
        native_preview.intent.debits[0].amount.asset.network,
        "ethereum:42161"
    );
    assert_eq!(
        native_preview.intent.debits[0].amount.atoms.to_decimal(18),
        "0.01"
    );

    let mut wrong_token = input.clone();
    wrong_token["requestId"] = json!("sub-usdc-wrong-token");
    wrong_token["token"] = json!("0x1111111111111111111111111111111111111111");
    assert_eq!(
        composite
            .prepare("binance.deposit", &wrong_token)
            .await
            .unwrap_err()
            .code,
        "ASSET_IDENTITY_MISMATCH"
    );

    let mut below_minimum = input.clone();
    below_minimum["requestId"] = json!("sub-usdc-below-min");
    below_minimum["amount"] = json!("0.0009");
    assert_eq!(
        composite
            .prepare("binance.deposit", &below_minimum)
            .await
            .unwrap_err()
            .code,
        "DEPOSIT_MINIMUM"
    );

    let mut over_precision = input.clone();
    over_precision["requestId"] = json!("sub-usdc-over-precision");
    over_precision["amount"] = json!("1.0000001");
    assert_eq!(
        composite
            .prepare("binance.deposit", &over_precision)
            .await
            .unwrap_err()
            .code,
        "INVALID_AMOUNT"
    );

    let mut wrong_chain = input.clone();
    wrong_chain["requestId"] = json!("sub-usdc-wrong-chain");
    wrong_chain["chainId"] = json!(1);
    assert_eq!(
        composite
            .prepare("binance.deposit", &wrong_chain)
            .await
            .unwrap_err()
            .code,
        "DEPOSIT_ROUTE_UNSUPPORTED"
    );

    let mut unknown_network = input.clone();
    unknown_network["requestId"] = json!("sub-usdc-unknown-network");
    unknown_network["network"] = json!("UNREVIEWED_EVM");
    unknown_network["chainId"] = json!(42161);
    assert_eq!(
        composite
            .prepare("binance.deposit", &unknown_network)
            .await
            .unwrap_err()
            .code,
        "DEPOSIT_ROUTE_UNSUPPORTED"
    );

    venue.state.lock().unwrap().deposit_memo_regex = Some("^[0-9]+$".into());
    let mut memo_required = input.clone();
    memo_required["requestId"] = json!("sub-usdc-memo");
    assert_eq!(
        composite
            .prepare("binance.deposit", &memo_required)
            .await
            .unwrap_err()
            .code,
        "DEPOSIT_MEMO_UNSUPPORTED"
    );

    venue.state.lock().unwrap().deposit_memo_regex = None;
    venue.state.lock().unwrap().deposit_memo = Some("12345".into());
    let mut tagged_address = input.clone();
    tagged_address["requestId"] = json!("sub-usdc-tagged-address");
    assert_eq!(
        composite
            .prepare("binance.deposit", &tagged_address)
            .await
            .unwrap_err()
            .code,
        "DEPOSIT_ADDRESS_UNSUPPORTED"
    );

    venue.state.lock().unwrap().deposit_memo = None;
    venue.state.lock().unwrap().deposit_denomination = Some(10);
    let mut scaled_denomination = input.clone();
    scaled_denomination["requestId"] = json!("sub-usdc-scaled-denomination");
    assert_eq!(
        composite
            .prepare("binance.deposit", &scaled_denomination)
            .await
            .unwrap_err()
            .code,
        "DEPOSIT_DENOMINATION_UNSUPPORTED"
    );

    venue.state.lock().unwrap().deposit_denomination = None;
    venue.state.lock().unwrap().deposit_enabled = false;
    let mut disabled = input;
    disabled["requestId"] = json!("sub-usdc-disabled");
    assert_eq!(
        composite
            .prepare("binance.deposit", &disabled)
            .await
            .unwrap_err()
            .code,
        "DEPOSIT_DISABLED"
    );
}

#[tokio::test]
async fn binance_deposit_return_route_proof_binds_fresh_address_and_cross_asset_projection() {
    use fin_domain::{
        ProjectedCash, ProjectedState, QuantityInterval, ReturnRouteContext, StateSnapshot,
    };

    let venue = Arc::new(SyntheticVenue::new());
    let binance = adapter(venue);
    let wallet_address = "0x2222222222222222222222222222222222222222";
    let ethereum = Arc::new(
        fin_ethereum::EthereumAdapter::new(
            fin_ethereum::EthereumConfig::default(),
            vec![fin_ethereum::WalletConnection {
                name: "synthetic-wallet".into(),
                address: wallet_address.into(),
                signing_enabled: true,
            }],
            Arc::new(fin_evm::EvmRuntime::new(
                Arc::new(DepositPreviewRpc::new()),
                1,
                None,
            )),
        )
        .unwrap(),
    );
    let composite = fin_broker::BinanceEthereumDepositAdapter::new(binance, ethereum);
    let wallet = ResourceId(format!("ethereum:1:{wallet_address}"));
    let venue_resource = resource(1001);
    let native = AssetId {
        network: "ethereum:1".into(),
        reference: "native".into(),
        decimals: 18,
    };
    let venue_eth = AssetId {
        network: "binance".into(),
        reference: "ETH".into(),
        decimals: 36,
    };
    let baseline = StateSnapshot {
        state: fin_domain::FinancialState {
            holdings: vec![
                ResourceAmount {
                    resource: wallet.clone(),
                    amount: Amount {
                        asset: native.clone(),
                        atoms: Quantity::from_decimal("0.1", 18).unwrap(),
                    },
                },
                ResourceAmount {
                    resource: venue_resource.clone(),
                    amount: Amount {
                        asset: venue_eth.clone(),
                        atoms: Quantity::zero(),
                    },
                },
            ],
            ..Default::default()
        },
        observed_at: now(),
        complete: true,
        coverage: vec![wallet.0.clone(), venue_resource.0.clone()],
        pending: vec![],
    };
    let context = ReturnRouteContext {
        baseline,
        projected: ProjectedState {
            cash: vec![
                ProjectedCash {
                    resource: wallet.clone(),
                    asset: native.clone(),
                    amount: QuantityInterval {
                        minimum: Quantity::from_decimal("0.1", 18).unwrap(),
                        maximum: Quantity::from_decimal("0.1", 18).unwrap(),
                    },
                },
                ProjectedCash {
                    resource: venue_resource,
                    asset: venue_eth,
                    amount: QuantityInterval {
                        minimum: Quantity::zero(),
                        maximum: Quantity::zero(),
                    },
                },
            ],
            ..Default::default()
        },
        allocations: vec![ResourceAmount {
            resource: wallet,
            amount: Amount {
                asset: native.clone(),
                atoms: Quantity::from_decimal("0.1", 18).unwrap(),
            },
        }],
        asset_prices: vec![fin_domain::AssetPrice {
            asset: native,
            usd_micros_per_unit: Quantity::from_u64(2_000_000_000),
            unit_atoms: Quantity::from_u64(1_000_000_000_000_000_000),
            observed_at: now(),
            source: "synthetic fixture ETH/USD".into(),
        }],
        step_index: 0,
        prior_proofs: vec![],
        remaining_cost_budget_usd_micros: Quantity::from_u64(10_000_000),
    };
    let input = json!({"requestId":"synthetic-deposit-plan","account":"main","wallet":"synthetic-wallet","amount":"0.01","maxFeeEth":"0.001"});
    let proof = composite
        .preflight_return_route("binance.deposit", &input, &context)
        .await
        .unwrap();
    assert_eq!(proof.operation, "binance.deposit");
    assert_eq!(proof.input_hash, fin_domain::binding_hash(&input).unwrap());
    assert!(proof.expires_at > proof.observed_at);
    assert!(proof.evidence["broker_verified_deposit_target"]["evidence_digest"].is_string());
    assert!(proof.effects.iter().any(|effect| matches!(effect,
        fin_domain::ProjectedEffect::CashDelta { resource, asset, minimum_delta, maximum_delta }
            if resource == &ResourceId(format!("ethereum:1:{wallet_address}"))
                && asset.network == "ethereum:1"
                && minimum_delta.as_str() == "-10000000000000000"
                && maximum_delta.as_str() == "-10000000000000000"
    )));
    assert!(
        proof.effects.iter().any(|effect| matches!(effect,
            fin_domain::ProjectedEffect::CashDelta { resource, asset, minimum_delta, maximum_delta }
                if resource.0 == "binance:uid:1001:SPOT"
                    && asset.network == "binance"
                    && asset.decimals == 36
                    && minimum_delta.as_str() == "10000000000000000000000000000000000"
                    && maximum_delta.as_str() == "10000000000000000000000000000000000"
        )),
        "{:?}",
        proof.effects
    );
}

#[tokio::test]
async fn native_binance_deposit_settles_only_after_original_canonical_hash_and_usable_venue_credit()
{
    let venue = Arc::new(SyntheticVenue::new());
    let binance = adapter(venue.clone());
    let signer = fin_evm::LocalEnvelopeSigner::from_private_key(
        "0x0000000000000000000000000000000000000000000000000000000000000001",
    )
    .unwrap();
    let wallet_address = format!("{:#x}", signer.address());
    let wallet_balance = Arc::new(Mutex::new("0.1".to_owned()));
    let transaction = Arc::new(Mutex::new(None));
    let journal = Arc::new(SyntheticSubmissionJournal::default());
    let oracle_updated_at = synthetic_oracle_updated_at();
    let oracle_clock = Arc::new(SyntheticOracleClock(AtomicU64::new(
        u64::try_from(oracle_updated_at).unwrap() + 30,
    )));
    let rpc = Arc::new(DepositExecutionRpc {
        oracle_updated_at,
        priority_fee: AtomicU64::new(10_000_000),
        nonce: AtomicU64::new(0),
        gas_estimate: AtomicU64::new(21_000),
        simulation_failure: AtomicBool::new(false),
        transaction: transaction.clone(),
        hash: Mutex::new(None),
        accepted: AtomicUsize::new(0),
        venue: venue.clone(),
        journal: journal.inner.clone(),
        durable_store_path: None,
        wallet_address: wallet_address.clone(),
        wallet_balance: wallet_balance.clone(),
        payout_amount: "0.01".into(),
        lose_submit_response: AtomicBool::new(false),
    });
    let ethereum = Arc::new(
        fin_ethereum::EthereumAdapter::new(
            fin_ethereum::EthereumConfig::default(),
            vec![fin_ethereum::WalletConnection {
                name: "synthetic-wallet".into(),
                address: wallet_address,
                signing_enabled: true,
            }],
            Arc::new(fin_evm::EvmRuntime::new(
                rpc.clone(),
                1,
                Some(Arc::new(RecordingSigner {
                    signer,
                    transaction,
                })),
            )),
        )
        .unwrap()
        .with_isolated_oracle_clock(oracle_clock.clone()),
    );
    let composite = fin_broker::BinanceEthereumDepositAdapter::new(binance, ethereum);
    let input = json!({"requestId":"synthetic-deposit-e2e","account":"main","wallet":"synthetic-wallet","amount":"0.01","maxFeeEth":"0.001"});
    let prepared = composite.prepare("binance.deposit", &input).await.unwrap();
    let expected_round = format!(":updated:{oracle_updated_at}");
    assert!(
        prepared
            .valuation
            .as_ref()
            .unwrap()
            .source
            .ends_with(&expected_round),
        "A fixed oracle round must retain its constructor timestamp independently of observation time."
    );
    // Advance only the observation clock, without sleeping or fabricating a
    // new oracle round. Exact prices/source and the frozen authorization must
    // remain identical at the final check.
    oracle_clock.0.fetch_add(1, Ordering::SeqCst);
    // A lower current priority-fee quote is admissible inside the approved
    // envelope and must not replace its frozen transaction binding.
    rpc.priority_fee.store(1, Ordering::SeqCst);
    rpc.gas_estimate.store(20_000, Ordering::SeqCst);
    let refreshed = composite.revalidate(&prepared).await.unwrap();
    assert_eq!(
        fin_domain::authorization_binding(&refreshed),
        fin_domain::authorization_binding(&prepared)
    );
    assert_eq!(
        refreshed.binding["inner_preview"]["binding"], prepared.binding["inner_preview"]["binding"],
        "revalidation must preserve the original signed transaction envelope"
    );
    rpc.gas_estimate.store(21_001, Ordering::SeqCst);
    rpc.priority_fee.store(14_000_000, Ordering::SeqCst);
    let higher_estimate = composite.revalidate(&prepared).await.unwrap();
    assert_eq!(
        higher_estimate.binding["inner_preview"]["binding"],
        prepared.binding["inner_preview"]["binding"],
        "an estimate increase still inside the approved gas limit must keep the frozen gas limit"
    );
    rpc.gas_estimate.store(1_000_000, Ordering::SeqCst);
    assert_eq!(
        composite.revalidate(&prepared).await.unwrap_err().code,
        "GAS_BOUND_CHANGED"
    );
    rpc.gas_estimate.store(21_000, Ordering::SeqCst);
    rpc.nonce.store(1, Ordering::SeqCst);
    assert_eq!(
        composite.revalidate(&prepared).await.unwrap_err().code,
        "NONCE_CHANGED",
        "a changed source nonce must invalidate the approved transfer"
    );
    rpc.nonce.store(0, Ordering::SeqCst);
    rpc.simulation_failure.store(true, Ordering::SeqCst);
    assert_eq!(
        composite.revalidate(&prepared).await.unwrap_err().code,
        "SIMULATION_REVERTED",
        "a failed fresh transfer simulation must block the approved transfer"
    );
    rpc.simulation_failure.store(false, Ordering::SeqCst);
    rpc.priority_fee.store(100_000_000_000, Ordering::SeqCst);
    assert_eq!(
        composite.revalidate(&prepared).await.unwrap_err().code,
        "FEE_BOUND_CHANGED",
        "a gas quote beyond the approved envelope must still block submission"
    );
    rpc.priority_fee.store(1, Ordering::SeqCst);
    let submission = composite.submit(&prepared, journal.clone()).await.unwrap();
    assert_eq!(submission.kind, "ethereum");
    assert_eq!(
        submission.reference,
        rpc.hash.lock().unwrap().clone().unwrap()
    );
    assert_eq!(rpc.accepted.load(Ordering::SeqCst), 1);

    let record = fin_domain::OperationRecord {
        id: "synthetic-operation-id".into(),
        request_id: "synthetic-deposit-e2e".into(),
        fingerprint: "synthetic-fingerprint".into(),
        fingerprint_version: 1,
        operation: "binance.deposit".into(),
        input,
        status: fin_domain::Status::Submitted,
        created_at: now(),
        updated_at: now(),
        preview: Some(prepared),
        approval: None,
        submission: Some(submission.clone()),
        dispatch_evidence: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let result = composite.reconcile(&record).await.unwrap();
    assert_eq!(result.status, fin_domain::Status::Completed);
    let verified = result.verified.unwrap();
    assert_eq!(verified.submission, submission);
    assert_eq!(
        verified.proof_digest,
        fin_domain::verified_effect_digest(&verified).unwrap()
    );
    assert_eq!(verified.effects.len(), 2);
    assert!(verified.effects.iter().any(|effect| matches!(effect,
        fin_domain::FinancialEffect::Transfer { from, to, debit, credit }
            if from.0.starts_with("ethereum:1:")
                && to == &resource(1001)
                && debit.asset.network == "ethereum:1"
                && debit.asset.decimals == 18
                && debit.atoms.to_decimal(18) == "0.01"
                && credit.asset.network == "binance"
                && credit.asset.decimals == 36
                && credit.atoms.to_decimal(36) == "0.01"
    )));
    assert!(verified.effects.iter().any(|effect| matches!(effect,
        fin_domain::FinancialEffect::Cost { category, treatment: fin_domain::CostTreatment::AdditionalDebit, amount, .. }
            if category == "ethereum_gas" && amount.asset.network == "ethereum:1" && !amount.atoms.is_zero()
    )));
    assert_eq!(
        verified.evidence["venueHistory"]["records"][0]["creditState"],
        "credited"
    );
    assert_eq!(venue.state.lock().unwrap().balances["main"], "0.010");

    let mut legacy_record = record.clone();
    let mut legacy_preview = legacy_record.preview.clone().unwrap();
    legacy_preview.operation_version = 1;
    legacy_preview
        .binding
        .as_object_mut()
        .unwrap()
        .remove("coin");
    legacy_preview
        .binding
        .as_object_mut()
        .unwrap()
        .remove("token_symbol");
    legacy_preview
        .binding
        .as_object_mut()
        .unwrap()
        .remove("source_asset");
    legacy_preview
        .binding
        .as_object_mut()
        .unwrap()
        .remove(fin_domain::value_bounds::KEY);
    legacy_preview.binding["inner_preview"]["binding"]
        .as_object_mut()
        .unwrap()
        .remove(fin_domain::value_bounds::KEY);
    legacy_preview.binding["inner_input"]["network"] = json!("ethereum");
    legacy_preview.binding["inner_preview"]["input"]["network"] = json!("ethereum");
    assert_eq!(
        composite
            .revalidate(&legacy_preview)
            .await
            .unwrap_err()
            .code,
        "PREVIEW_CHANGED"
    );
    legacy_record.preview = Some(legacy_preview);
    // Model a durable pre-ceiling record by omitting the newly introduced
    // optional composite ceiling field before serde deserialization.
    let mut legacy_json = serde_json::to_value(&legacy_record).unwrap();
    legacy_json["preview"]["binding"]
        .as_object_mut()
        .unwrap()
        .remove("_fin_usd_ceiling_v1");
    let legacy_record: fin_domain::OperationRecord =
        serde_json::from_value(legacy_json).expect("pre-ceiling records remain deserializable");
    let legacy_recovered = composite.reconcile(&legacy_record).await.unwrap();
    assert_eq!(
        legacy_recovered.status,
        fin_domain::Status::Completed,
        "historical v1 ETH deposit records remain reconcilable"
    );
    assert!(legacy_recovered.verified.is_some());

    // Chain finality alone, locked venue credit, and a partial credit never
    // produce completed composite financial effects.
    venue.state.lock().unwrap().deposit_history[0]["status"] = json!(6);
    let locked = composite.reconcile(&record).await.unwrap();
    assert_eq!(locked.status, fin_domain::Status::Unknown);
    assert!(locked.verified.is_none());
    venue.state.lock().unwrap().deposit_history[0]["status"] = json!(1);
    venue.state.lock().unwrap().deposit_history[0]["amount"] = json!("0.009");
    let partial = composite.reconcile(&record).await.unwrap();
    assert_eq!(partial.status, fin_domain::Status::Unknown);
    assert!(partial.verified.is_none());

    venue.state.lock().unwrap().deposit_history[0]["amount"] = json!("0.01");
    venue.state.lock().unwrap().deposit_history[0]["travelRuleStatus"] = json!(2);
    let unknown_travel_state = composite.reconcile(&record).await.unwrap();
    assert_eq!(unknown_travel_state.status, fin_domain::Status::Unknown);
    assert!(unknown_travel_state.verified.is_none());

    venue.state.lock().unwrap().deposit_history[0]["travelRuleStatus"] = json!("1");
    let malformed_travel_state = composite.reconcile(&record).await.unwrap();
    assert_eq!(malformed_travel_state.status, fin_domain::Status::Unknown);
    assert!(malformed_travel_state.verified.is_none());
}

#[tokio::test]
async fn composite_deposit_keeps_exact_approved_transfer_across_fresh_usd_repricing() {
    let venue = Arc::new(SyntheticVenue::new());
    let binance = adapter(venue.clone());
    let signer = fin_evm::LocalEnvelopeSigner::from_private_key(
        "0x0000000000000000000000000000000000000000000000000000000000000001",
    )
    .unwrap();
    let wallet_address = format!("{:#x}", signer.address());
    let wallet_balance = Arc::new(Mutex::new("0.1".to_owned()));
    let transaction = Arc::new(Mutex::new(None));
    let journal = Arc::new(SyntheticSubmissionJournal::default());
    let usd = Arc::new(MutableEthereumUsd::new(2_000_100_000));
    let rpc = Arc::new(DepositExecutionRpc {
        oracle_updated_at: synthetic_oracle_updated_at(),
        priority_fee: AtomicU64::new(10_000_000),
        nonce: AtomicU64::new(0),
        gas_estimate: AtomicU64::new(21_000),
        simulation_failure: AtomicBool::new(false),
        transaction: transaction.clone(),
        hash: Mutex::new(None),
        accepted: AtomicUsize::new(0),
        venue: venue.clone(),
        journal: journal.inner.clone(),
        durable_store_path: None,
        wallet_address: wallet_address.clone(),
        wallet_balance,
        payout_amount: "0.01".into(),
        lose_submit_response: AtomicBool::new(false),
    });
    let ethereum = Arc::new(
        fin_ethereum::EthereumAdapter::new(
            fin_ethereum::EthereumConfig::default(),
            vec![fin_ethereum::WalletConnection {
                name: "synthetic-wallet".into(),
                address: wallet_address,
                signing_enabled: true,
            }],
            Arc::new(fin_evm::EvmRuntime::new(
                rpc.clone(),
                1,
                Some(Arc::new(RecordingSigner {
                    signer,
                    transaction,
                })),
            )),
        )
        .unwrap()
        .with_wallet_valuation(usd.clone()),
    );
    let composite = fin_broker::BinanceEthereumDepositAdapter::new(binance, ethereum);
    let input = json!({"requestId":"synthetic-deposit-usd-reprice","account":"main","wallet":"synthetic-wallet","amount":"0.01","maxFeeEth":"0.001"});
    let approved = composite.prepare("binance.deposit", &input).await.unwrap();
    let transaction_binding = approved.binding["inner_preview"]["binding"]["transaction"].clone();
    assert_eq!(transaction_binding["value"], "10000000000000000");
    assert_eq!(
        transaction_binding["to"],
        "0x1111111111111111111111111111111111111111"
    );
    assert_eq!(transaction_binding["gas"], 25_200);
    let initial_authorization = fin_domain::authorization_binding(&approved);
    let old_publication = approved.valuation.as_ref().unwrap().prices[0]
        .source
        .clone();
    let old_unit_price = approved.valuation.as_ref().unwrap().prices[0]
        .usd_micros_per_unit
        .clone();

    // Reprice ETH by half a cent on the exact principal and publish a newer
    // RFC3339 CoinGecko-style timestamp. Both changes are real fresh evidence;
    // the authorization still binds the original finite transaction.
    usd.update(2_000_500_000);
    let refreshed = composite.revalidate(&approved).await.unwrap();
    assert_ne!(
        refreshed.valuation.as_ref().unwrap().prices[0].source,
        old_publication
    );
    assert!(
        refreshed.valuation.as_ref().unwrap().prices[0]
            .source
            .contains("coingecko:ethereum-usd; updated:")
    );
    assert_ne!(
        refreshed.valuation.as_ref().unwrap().prices[0].usd_micros_per_unit,
        old_unit_price,
        "the fresh valuation must contain the repriced ETH/USD quote"
    );
    assert_eq!(
        refreshed.valuation.as_ref().unwrap().movement_usd_micros,
        approved.valuation.as_ref().unwrap().movement_usd_micros,
        "the carried approval valuation remains the immutable cent-rounded ceiling"
    );
    assert_eq!(
        fin_domain::authorization_binding(&refreshed),
        initial_authorization,
        "repricing and source publication time must not alter the approved binding"
    );
    assert_eq!(
        refreshed.binding["inner_preview"]["binding"]["transaction"], transaction_binding,
        "gas, principal, and destination remain exactly frozen"
    );

    // A materially higher price crosses the cent-rounded approved USD ceiling.
    usd.update(2_100_000_000);
    let over_ceiling = composite.revalidate(&approved).await.unwrap_err();
    assert_eq!(over_ceiling.code, "VALUE_BOUND_EXCEEDED");
    assert_eq!(rpc.accepted.load(Ordering::SeqCst), 0);

    // Return within the approved ceiling with another fresh source timestamp;
    // actual submission still uses precisely the originally approved fields.
    usd.update(2_000_500_000);
    let submission = composite.submit(&approved, journal.clone()).await.unwrap();
    assert_eq!(submission.kind, "ethereum");
    assert_eq!(rpc.accepted.load(Ordering::SeqCst), 1);
    let submitted = rpc.transaction.lock().unwrap().clone().unwrap();
    assert_eq!(submitted.value.to_decimal(18), "0.01");
    assert_eq!(
        submitted.to.to_string().to_lowercase(),
        "0x1111111111111111111111111111111111111111"
    );
    assert_eq!(submitted.gas, 25_200);
}

#[tokio::test]
async fn arbitrum_usdc_deposit_executes_erc20_and_requires_exact_binance_credit() {
    let venue = Arc::new(SyntheticVenue::new());
    let binance = subaccount_only_adapter(venue.clone());
    let token = "0xaf88d065e77c8cC2239327C5EDb3A432268e5831";
    let signer = fin_evm::LocalEnvelopeSigner::from_private_key(
        "0x0000000000000000000000000000000000000000000000000000000000000001",
    )
    .unwrap();
    let wallet_address = format!("{:#x}", signer.address());
    let transaction = Arc::new(Mutex::new(None));
    let journal = Arc::new(SyntheticSubmissionJournal::default());
    let rpc = Arc::new(ArbitrumErc20ExecutionRpc {
        token: token.into(),
        transaction: transaction.clone(),
        hash: Mutex::new(None),
        accepted: AtomicUsize::new(0),
        venue: venue.clone(),
        journal: journal.inner.clone(),
        wallet_address: wallet_address.clone(),
    });
    let ethereum = Arc::new(
        fin_ethereum::EthereumAdapter::new(
            fin_ethereum::EthereumConfig::wallet_only(42161, 2, vec![]),
            vec![fin_ethereum::WalletConnection {
                name: "synthetic-wallet".into(),
                address: wallet_address,
                signing_enabled: true,
            }],
            Arc::new(fin_evm::EvmRuntime::new(
                rpc.clone(),
                42161,
                Some(Arc::new(RecordingSigner {
                    signer,
                    transaction,
                })),
            )),
        )
        .unwrap()
        .with_wallet_valuation(Arc::new(SyntheticWalletPrice)),
    );
    let composite = fin_broker::BinanceEthereumDepositAdapter::new(binance, ethereum);
    let input = json!({"requestId":"synthetic-arbitrum-usdc","account":"child-a","wallet":"synthetic-wallet","amount":"1.25","maxFeeEth":"0.001","coin":"USDC","network":"ARBITRUM","chainId":42161,"token":token});
    let prepared = composite.prepare("binance.deposit", &input).await.unwrap();
    assert!(prepared.summary.contains(
        "Deposit 1.25 USDC: synthetic-wallet → child-a on Arbitrum One. Max fee 0.001 ETH."
    ));
    assert!(!prepared.summary.contains("0x") && !prepared.summary.contains("42161"));
    assert_eq!(
        prepared.binding["source_asset"]["reference"],
        token.to_ascii_lowercase()
    );
    assert_eq!(
        prepared.binding["inner_preview"]["binding"]["transaction"]["sender"],
        "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf"
    );
    assert_eq!(prepared.intent.debits[0].amount.asset.decimals, 6);
    assert_eq!(prepared.intent.debits[0].amount.atoms.to_decimal(6), "1.25");
    let submission = composite.submit(&prepared, journal.clone()).await.unwrap();
    assert_eq!(submission.kind, "ethereum");
    assert_eq!(rpc.accepted.load(Ordering::SeqCst), 1);

    let record = fin_domain::OperationRecord {
        id: "synthetic-arbitrum-deposit-record".into(),
        request_id: "synthetic-arbitrum-usdc".into(),
        fingerprint: "synthetic-arbitrum-fingerprint".into(),
        fingerprint_version: 1,
        operation: "binance.deposit".into(),
        input,
        status: fin_domain::Status::Submitted,
        created_at: now(),
        updated_at: now(),
        preview: Some(prepared),
        approval: None,
        submission: Some(submission.clone()),
        dispatch_evidence: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let result = composite.reconcile(&record).await.unwrap();
    assert_eq!(
        result.status,
        fin_domain::Status::Completed,
        "{:?}",
        result.evidence
    );
    let verified = result.verified.unwrap();
    assert_eq!(verified.submission, submission);
    assert!(verified.effects.iter().any(|effect| matches!(effect,
        fin_domain::FinancialEffect::Transfer { debit, credit, to, .. }
            if debit.asset.network == "ethereum:42161"
                && debit.asset.reference == token.to_ascii_lowercase()
                && debit.asset.decimals == 6
                && debit.atoms.to_decimal(6) == "1.25"
                && credit.asset.network == "binance"
                && credit.asset.reference == "USDC"
                && credit.atoms.to_decimal(36) == "1.25"
                && to == &resource(1002)
    )));
    assert_eq!(
        venue.state.lock().unwrap().usdc_balances[EMAIL_A],
        "1.250000"
    );
    assert_eq!(
        rpc.accepted.load(Ordering::SeqCst),
        1,
        "reconciliation must not replay the original transfer"
    );

    venue.state.lock().unwrap().deposit_history[0]["network"] = json!("ETH");
    let mismatched_network = composite.reconcile(&record).await.unwrap();
    assert_eq!(mismatched_network.status, fin_domain::Status::Unknown);
    assert!(mismatched_network.verified.is_none());
}

#[tokio::test]
async fn scoped_deposit_withdraw_roundtrip_is_broker_preflighted_executed_and_finished_after_net_costs()
 {
    let venue = Arc::new(SyntheticVenue::new());
    // A small captured main balance funds the separately charged network fee;
    // any fee below the approved cap remains within that original allocation.
    venue
        .state
        .lock()
        .unwrap()
        .balances
        .insert("main".into(), "0.001".into());
    let signer = fin_evm::LocalEnvelopeSigner::from_private_key(
        "0x0000000000000000000000000000000000000000000000000000000000000001",
    )
    .unwrap();
    let address = format!("{:#x}", signer.address());
    let wallet = ResourceId(format!("ethereum:1:{address}"));
    let wallet_balance = Arc::new(Mutex::new("0.1".to_owned()));
    *venue.wallet_balance.lock().unwrap() = Some(wallet_balance.clone());
    let temp = tempfile::tempdir().unwrap();
    let store_path = temp.path().join("deposit-withdraw.sqlite");
    let transaction = Arc::new(Mutex::new(None));
    let journal = Arc::new(SyntheticSubmissionJournal::default());
    let rpc = Arc::new(DepositExecutionRpc {
        oracle_updated_at: synthetic_oracle_updated_at(),
        priority_fee: AtomicU64::new(10_000_000),
        nonce: AtomicU64::new(0),
        gas_estimate: AtomicU64::new(21_000),
        simulation_failure: AtomicBool::new(false),
        transaction: transaction.clone(),
        hash: Mutex::new(None),
        accepted: AtomicUsize::new(0),
        venue: venue.clone(),
        journal: journal.inner.clone(),
        durable_store_path: Some(store_path.clone()),
        wallet_address: address.clone(),
        wallet_balance: wallet_balance.clone(),
        payout_amount: "0.009".into(),
        lose_submit_response: AtomicBool::new(true),
    });
    let ethereum = Arc::new(
        fin_ethereum::EthereumAdapter::new(
            fin_ethereum::EthereumConfig::default(),
            vec![fin_ethereum::WalletConnection {
                name: "synthetic-wallet".into(),
                address: address.clone(),
                signing_enabled: true,
            }],
            Arc::new(fin_evm::EvmRuntime::new(
                rpc.clone(),
                1,
                Some(Arc::new(RecordingSigner {
                    signer,
                    transaction,
                })),
            )),
        )
        .unwrap(),
    );
    let payout = Arc::new(
        fin_broker::EthereumPayoutVerifier::new(BTreeMap::from([(1, ethereum.clone())])).unwrap(),
    );
    let mut binance_config = adapter_config();
    binance_config.destinations[0].address = address.clone();
    binance_config.destinations[0].resource = wallet.clone();
    let binance = Arc::new(
        BinanceAdapter::new(
            binance_config,
            venue.clone(),
            Some(Arc::new(SyntheticBinancePrice)),
            Some(payout),
        )
        .unwrap(),
    );
    let composite = Arc::new(fin_broker::BinanceEthereumDepositAdapter::new(
        binance.clone(),
        ethereum,
    ));
    let wallet_observer = Arc::new(WalletSnapshotObserver {
        resource: wallet.clone(),
        address: address.clone(),
        balance: wallet_balance.clone(),
    });
    let mut config = runtime_config();
    for connection in &mut config.connections {
        if let ConnectionConfig::Ethereum {
            resource,
            address: configured_address,
            ..
        } = connection
        {
            *resource = wallet.clone();
            *configured_address = address.clone();
        }
    }
    let approval = Arc::new(SyntheticApproval(AtomicUsize::new(0)));
    let broker = compose_runtime(
        config.clone(),
        RuntimeDependencies {
            store: Store::open(&store_path).unwrap(),
            adapters: vec![binance.clone(), composite.clone(), wallet_observer.clone()],
            authorizer: approval.clone(),
            valuator: Arc::new(SyntheticValuator),
            bridge: None,
        },
    )
    .unwrap();
    let deposit_request = OperationRequest {
        request_id: "scoped-deposit-forward".into(),
        operation: "binance.deposit".into(),
        input: json!({"requestId":"scoped-deposit-forward","account":"main","wallet":"synthetic-wallet","amount":"0.009","maxFeeEth":"0.001"}),
        run_id: None,
    };
    let lost_response = broker
        .handle(BrokerRequest::Execute {
            request: deposit_request.clone(),
        })
        .await;
    let uncertain = lost_response.result.unwrap_or_else(|| {
        panic!(
            "lost transport response should retain the durable uncertain operation: {:?}",
            lost_response.error
        )
    });
    assert_eq!(uncertain["status"], "unknown");
    assert!(uncertain["submission"].is_object());
    assert_eq!(rpc.accepted.load(Ordering::SeqCst), 1);
    assert_eq!(venue.state.lock().unwrap().deposit_history[0]["status"], 0);

    // Recreate the protected runtime over the same durable store. The same
    // request identity reconciles its persisted hash while Binance credit is
    // delayed; it does not create a new transaction or approval.
    drop(broker);
    let broker = compose_runtime(
        config.clone(),
        RuntimeDependencies {
            store: Store::open(&store_path).unwrap(),
            adapters: vec![binance.clone(), composite.clone(), wallet_observer.clone()],
            authorizer: approval.clone(),
            valuator: Arc::new(SyntheticValuator),
            bridge: None,
        },
    )
    .unwrap();
    let delayed = broker
        .handle(BrokerRequest::Execute {
            request: deposit_request.clone(),
        })
        .await;
    assert!(
        delayed.ok,
        "duplicate request should return its durable pending status: {:?}",
        delayed.error
    );
    assert_eq!(delayed.result.unwrap()["status"], "unknown");
    assert_eq!(
        rpc.accepted.load(Ordering::SeqCst),
        1,
        "restart recovery must not rebroadcast"
    );

    venue.state.lock().unwrap().deposit_history[0]["status"] = json!(1);
    let recovered = broker
        .handle(BrokerRequest::Execute {
            request: deposit_request.clone(),
        })
        .await;
    assert!(
        recovered.ok,
        "credited deposit should reconcile after restart: {:?}",
        recovered.error
    );
    assert_eq!(recovered.result.unwrap()["status"], "completed");
    assert_eq!(
        rpc.accepted.load(Ordering::SeqCst),
        1,
        "duplicate execution remains suppressed"
    );

    let withdrawal = broker.handle(BrokerRequest::Execute {
        request: OperationRequest {
            request_id: "scoped-withdraw-return".into(),
            operation: "binance.withdraw".into(),
            input: json!({"requestId":"scoped-withdraw-return","account":"main","destination":"synthetic-wallet","coin":"ETH","amount":"0.010","network":"ETH","walletType":"SPOT","maxFee":"0.001","minReceiveAmount":"0.009"}),
            run_id: None,
        },
    }).await;
    assert!(
        withdrawal.ok,
        "binance.withdraw failed: {:?}",
        withdrawal.error
    );
    assert_eq!(withdrawal.result.unwrap()["status"], "completed");
    assert_eq!(
        rpc.accepted.load(Ordering::SeqCst),
        1,
        "the Ethereum deposit was broadcast exactly once"
    );
    assert_eq!(venue.state.lock().unwrap().balances["main"], "0.000");
    assert_eq!(*wallet_balance.lock().unwrap(), "0.099958");
    assert_eq!(
        approval.0.load(Ordering::SeqCst),
        2,
        "each financial write used a fresh synthetic authorization"
    );
}

#[tokio::test]
async fn two_child_consolidation_withdrawal_and_deposit_redistribution_restores_full_allocation() {
    let venue = Arc::new(SyntheticVenue::new());
    // Child balances plus the preexisting main balance cover the gross API
    // withdrawal amount; its network fee is withheld from that amount.
    venue
        .state
        .lock()
        .unwrap()
        .balances
        .insert("main".into(), "0.001".into());
    let signer = fin_evm::LocalEnvelopeSigner::from_private_key(
        "0x0000000000000000000000000000000000000000000000000000000000000001",
    )
    .unwrap();
    let address = format!("{:#x}", signer.address());
    let wallet = ResourceId(format!("ethereum:1:{address}"));
    let wallet_balance = Arc::new(Mutex::new("0.1".to_owned()));
    *venue.wallet_balance.lock().unwrap() = Some(wallet_balance.clone());
    let temp = tempfile::tempdir().unwrap();
    let store_path = temp.path().join("two-child-funding-roundtrip.sqlite");
    let transaction = Arc::new(Mutex::new(None));
    let journal = Arc::new(SyntheticSubmissionJournal::default());
    let rpc = Arc::new(DepositExecutionRpc {
        oracle_updated_at: synthetic_oracle_updated_at(),
        priority_fee: AtomicU64::new(10_000_000),
        nonce: AtomicU64::new(0),
        gas_estimate: AtomicU64::new(21_000),
        simulation_failure: AtomicBool::new(false),
        transaction: transaction.clone(),
        hash: Mutex::new(None),
        accepted: AtomicUsize::new(0),
        venue: venue.clone(),
        journal: journal.inner.clone(),
        durable_store_path: Some(store_path.clone()),
        wallet_address: address.clone(),
        wallet_balance: wallet_balance.clone(),
        payout_amount: "0.015".into(),
        lose_submit_response: AtomicBool::new(false),
    });
    let ethereum = Arc::new(
        fin_ethereum::EthereumAdapter::new(
            fin_ethereum::EthereumConfig::default(),
            vec![fin_ethereum::WalletConnection {
                name: "synthetic-wallet".into(),
                address: address.clone(),
                signing_enabled: true,
            }],
            Arc::new(fin_evm::EvmRuntime::new(
                rpc.clone(),
                1,
                Some(Arc::new(RecordingSigner {
                    signer,
                    transaction,
                })),
            )),
        )
        .unwrap(),
    );
    let payout = Arc::new(
        fin_broker::EthereumPayoutVerifier::new(BTreeMap::from([(1, ethereum.clone())])).unwrap(),
    );
    let mut binance_config = adapter_config();
    binance_config.destinations[0].address = address.clone();
    binance_config.destinations[0].resource = wallet.clone();
    let binance = Arc::new(
        BinanceAdapter::new(
            binance_config,
            venue.clone(),
            Some(Arc::new(SyntheticBinancePrice)),
            Some(payout),
        )
        .unwrap(),
    );
    let composite = Arc::new(fin_broker::BinanceEthereumDepositAdapter::new(
        binance.clone(),
        ethereum,
    ));
    let wallet_observer = Arc::new(WalletSnapshotObserver {
        resource: wallet.clone(),
        address: address.clone(),
        balance: wallet_balance.clone(),
    });
    let mut config = runtime_config();
    for connection in &mut config.connections {
        if let ConnectionConfig::Ethereum {
            resource,
            address: configured_address,
            ..
        } = connection
        {
            *resource = wallet.clone();
            *configured_address = address.clone();
        }
    }
    let approval = Arc::new(SyntheticApproval(AtomicUsize::new(0)));
    let broker = compose_runtime(
        config,
        RuntimeDependencies {
            store: Store::open(&store_path).unwrap(),
            adapters: vec![binance, composite, wallet_observer],
            authorizer: approval.clone(),
            valuator: Arc::new(SyntheticValuator),
            bridge: None,
        },
    )
    .unwrap();

    for (id, operation, input) in [
        (
            "journey-a-to-main",
            "binance.transfer",
            transfer("journey-a-to-main", CHILD_A, MAIN, "0.005"),
        ),
        (
            "journey-b-to-main",
            "binance.transfer",
            transfer("journey-b-to-main", CHILD_B, MAIN, "0.010"),
        ),
        (
            "journey-withdraw",
            "binance.withdraw",
            json!({"requestId":"journey-withdraw","account":"main","destination":"synthetic-wallet","coin":"ETH","amount":"0.016","network":"ETH","walletType":"SPOT","maxFee":"0.001","minReceiveAmount":"0.015"}),
        ),
        (
            "scoped-deposit-forward",
            "binance.deposit",
            json!({"requestId":"scoped-deposit-forward","account":"main","wallet":"synthetic-wallet","amount":"0.015","maxFeeEth":"0.001"}),
        ),
        (
            "journey-main-to-a",
            "binance.transfer",
            transfer("journey-main-to-a", MAIN, CHILD_A, "0.005"),
        ),
        (
            "journey-main-to-b",
            "binance.transfer",
            transfer("journey-main-to-b", MAIN, CHILD_B, "0.010"),
        ),
    ] {
        let response = broker
            .handle(BrokerRequest::Execute {
                request: OperationRequest {
                    request_id: id.into(),
                    operation: operation.into(),
                    input,
                    run_id: None,
                },
            })
            .await;
        assert!(response.ok, "{operation} {id} failed: {:?}", response.error);
        assert_eq!(response.result.unwrap()["status"], "completed", "{id}");
    }
    assert_eq!(rpc.accepted.load(Ordering::SeqCst), 1);
    let venue_state = venue.state.lock().unwrap();
    assert_eq!(venue_state.balances[EMAIL_A], "0.005");
    assert_eq!(venue_state.balances[EMAIL_B], "0.010");
    assert_eq!(venue_state.balances["main"], "0.000");
    assert_eq!(venue_state.posts, 5);
    assert_eq!(*wallet_balance.lock().unwrap(), "0.099958");
    assert_eq!(approval.0.load(Ordering::SeqCst), 6);
}

#[tokio::test]
async fn partial_second_consolidation_failure_preserves_success_and_allows_explicit_followup() {
    let venue = Arc::new(SyntheticVenue::new());
    let failed_id = fin_binance::ClientIdentity::transfer("consolidate-b").unwrap();
    venue.state.lock().unwrap().reject_request_id = Some(failed_id.as_str().into());
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("partial.sqlite");
    let approval = Arc::new(SyntheticApproval(AtomicUsize::new(0)));
    let broker = runtime(&db, venue.clone(), approval.clone());
    execute(
        &broker,
        "consolidate-a",
        transfer("consolidate-a", CHILD_A, MAIN, "0.005"),
    )
    .await;
    let failed_response = broker
        .handle(BrokerRequest::Execute {
            request: OperationRequest {
                request_id: "consolidate-b".into(),
                operation: "binance.transfer".into(),
                input: transfer("consolidate-b", CHILD_B, MAIN, "0.010"),
                run_id: None,
            },
        })
        .await;
    assert!(failed_response.ok, "{:?}", failed_response.error);
    let uncertain = failed_response.result.unwrap();
    assert_eq!(uncertain["status"], "unknown");
    let reconciled = broker
        .handle(BrokerRequest::Status {
            id: uncertain["id"].as_str().unwrap().into(),
        })
        .await;
    assert!(reconciled.ok, "{:?}", reconciled.error);
    assert_eq!(reconciled.result.unwrap()["status"], "failed");

    execute(
        &broker,
        "return-a",
        transfer("return-a", MAIN, CHILD_A, "0.005"),
    )
    .await;
    let state = venue.state.lock().unwrap();
    assert_eq!(state.balances[EMAIL_A], "0.005");
    assert_eq!(state.balances[EMAIL_B], "0.010");
    assert_eq!(state.balances["main"], "0.000");
    assert_eq!(
        state.posts, 3,
        "failed accepted identity plus first/cleanup writes are not replayed"
    );
    assert_eq!(approval.0.load(Ordering::SeqCst), 3);
}
