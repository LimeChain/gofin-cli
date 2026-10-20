//! Real Kraken adapter driven by a stateful synthetic venue and independent EVM proof.
use async_trait::async_trait;
use gofin_domain::*;
use gofin_evm::alloy::primitives::U256;
use gofin_exchange::{ExchangeAdapter, kraken::DispatchGuard};
use gofin_kraken::funding::{
    KrakenDestination, KrakenPayoutRequest, KrakenPayoutResult, KrakenRecipientVerifier,
};
use gofin_kraken::{
    KrakenAccount, KrakenEndpoint as E, KrakenProduct, KrakenProfile, KrakenResponse,
    KrakenTransport,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
pub const ACCOUNT: &str = "kraken:synthetic-plan";
pub const METHOD: &str = "00000000-0000-0000-0000-000000000001";
pub const ADDRESS_ID: &str = "ABabcde-abcde-abcdef";
pub struct Venue {
    pub wallet: String,
    pub source: Arc<super::full_plan_evm::WorkflowEvm>,
    pub recipient: Arc<gofin_ethereum::EthereumAdapter>,
    calls: Mutex<Vec<(E, Value)>>,
    pub sold: AtomicBool,
    pub lose_response: AtomicBool,
    pub hide_history: AtomicBool,
    pub historical_unknown: AtomicBool,
    pub funding_v1: AtomicBool,
    pub funding_history_ready: AtomicBool,
    withdrawn: AtomicBool,

    pub writes: AtomicUsize,
}
impl Venue {
    pub fn new(
        wallet: String,
        source: Arc<super::full_plan_evm::WorkflowEvm>,
        recipient: Arc<gofin_ethereum::EthereumAdapter>,
    ) -> Self {
        Self {
            wallet,
            source,
            recipient,
            calls: Mutex::new(vec![]),
            sold: AtomicBool::new(false),
            lose_response: AtomicBool::new(false),
            hide_history: AtomicBool::new(false),
            historical_unknown: AtomicBool::new(false),
            funding_v1: AtomicBool::new(false),
            funding_history_ready: AtomicBool::new(false),
            withdrawn: AtomicBool::new(false),
            writes: AtomicUsize::new(0),
        }
    }
    fn eth(&self) -> &'static str {
        if self.withdrawn.load(Ordering::SeqCst) {
            if self.funding_v1.load(Ordering::SeqCst) {
                "0.00005"
            } else {
                "0.00004"
            }
        } else if self.sold.load(Ordering::SeqCst) {
            "0.01494"
        } else {
            "0"
        }
    }
    pub fn funding_withdrawal_dispatches(&self) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(endpoint, _)| *endpoint == E::KrakenFundingPostFundingV1Withdrawals)
            .count()
    }
    fn sol(&self) -> &'static str {
        if self.sold.load(Ordering::SeqCst) {
            "0.0012"
        } else {
            "0.3012"
        }
    }
    fn order(&self) -> Value {
        let calls = self.calls.lock().unwrap();
        let input = &calls
            .iter()
            .find(|(e, input)| *e == E::KrakenPrivatePostAddOrder && input["volume"] == "0.3")
            .unwrap()
            .1;
        json!({"cl_ord_id":input["cl_ord_id"],"opentm":format!("{}.{:09}", chrono::Utc::now().timestamp(), chrono::Utc::now().timestamp_subsec_nanos()),"descr":{"pair":"SOLETH","type":"sell"},"status":"closed","vol":"0.3","vol_exec":"0.3","cost":"0.015","fee":"0.00006","trades":["SYNTHETIC-TRADE"]})
    }
}
#[async_trait]
impl KrakenTransport for Venue {
    async fn request(
        &self,
        e: E,
        _: Option<&KrakenAccount>,
        p: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        self.calls.lock().unwrap().push((e, p.clone()));
        if e.mutating() {
            assert!(guard.expect("write guard")());
            self.writes.fetch_add(1, Ordering::SeqCst);
        } else {
            assert!(guard.is_none());
        }
        let result = match e {
            E::KrakenPrivatePostGetApiKeyInfo => {
                json!({"permissions":["query-funds","query-open-trades","query-closed-trades","query-ledger","modify-trades","close-trades","withdraw-funds"],"valid_until":"0","iban":"SYNTHETIC"})
            }
            E::KrakenPublicGetAssets => json!({"XETH":{"decimals":18},"SOL":{"decimals":9}}),
            E::KrakenPublicGetAssetPairs => {
                json!({"SOLETH":{"altname":"SOLETH","base":"SOL","quote":"XETH","status":"online","lot_decimals":8,"pair_decimals":6,"cost_decimals":6,"ordermin":"0.0001"}})
            }
            E::KrakenPublicGetTicker => {
                json!({"SOLETH":{"b":["0.05","1","1"],"a":["0.0501","1","1"]}})
            }
            E::KrakenPrivatePostTradeVolume => {
                json!({"fees":{"SOLETH":{"fee":"0.4"}},"fees_maker":{"SOLETH":{"fee":"0.25"}}})
            }
            E::KrakenPrivatePostBalanceEx => {
                json!({"XETH":{"balance":self.eth(),"hold_trade":"0"},"SOL":{"balance":self.sol(),"hold_trade":"0"}})
            }
            E::KrakenPrivatePostBalance => json!({"XETH":self.eth(),"SOL":self.sol()}),
            E::KrakenPrivatePostOpenPositions | E::KrakenPrivatePostCreditLines => json!({}),
            E::KrakenPrivatePostEarnAllocations => json!({"items":[]}),
            E::KrakenPrivatePostAddOrder => {
                if self.historical_unknown.load(Ordering::SeqCst) && p["volume"] == "0.01" {
                    return Err(GoFinError::new(
                        "KRAKEN_UNAVAILABLE",
                        "kraken",
                        "Synthetic historical response loss with no outcome evidence.",
                        "Preserve the original identity.",
                    ));
                }
                assert_eq!(p["volume"], "0.3");
                assert_eq!(p["price"], "0.05");
                assert!(!self.sold.swap(true, Ordering::SeqCst));
                if self.lose_response.swap(false, Ordering::SeqCst) {
                    return Err(GoFinError::new(
                        "KRAKEN_UNAVAILABLE",
                        "kraken",
                        "Controlled response loss after accepted sell",
                        "Reconcile the original client order identity",
                    ));
                }
                json!({"txid":["SYNTHETIC-ORDER"]})
            }
            E::KrakenPrivatePostQueryOrders => json!({"SYNTHETIC-ORDER":self.order()}),
            E::KrakenPrivatePostOpenOrders => json!({"open":{}}),
            E::KrakenPrivatePostClosedOrders => {
                if self.hide_history.load(Ordering::SeqCst) || !self.sold.load(Ordering::SeqCst) {
                    json!({"closed":{}})
                } else {
                    json!({"closed":{"SYNTHETIC-ORDER":self.order()}})
                }
            }
            E::KrakenPrivatePostQueryTrades => {
                json!({"SYNTHETIC-TRADE":{"ordertxid":"SYNTHETIC-ORDER","pair":"SOLETH","type":"sell","vol":"0.3","cost":"0.015","fee":"0.00006","margin":"0","leverage":"0","ledgers":["SELL-BASE","SELL-QUOTE"]}})
            }
            E::KrakenPrivatePostQueryLedgers => {
                json!({"SELL-BASE":{"refid":"SYNTHETIC-TRADE","type":"trade","asset":"SOL","amount":"-0.3","fee":"0"},"SELL-QUOTE":{"refid":"SYNTHETIC-TRADE","type":"trade","asset":"XETH","amount":"0.015","fee":"0.00006"}})
            }
            E::KrakenPrivatePostWithdrawInfo => {
                json!({"method":"Ether","amount":"0.0148","fee":"0.0001","limit":self.eth()})
            }
            E::KrakenPrivatePostWithdrawAddresses => {
                json!([{"key":"synthetic-ethereum","method":"Ether","verified":true,"address":self.wallet}])
            }
            E::KrakenPrivatePostWithdrawMethods => {
                json!([{"method":"Ether","network":"Ethereum","minimum":"0.0001"}])
            }
            E::KrakenPrivatePostWithdraw => {
                assert!(self.sold.load(Ordering::SeqCst));
                assert_eq!(p["amount"], "0.0149");
                assert!(!self.withdrawn.swap(true, Ordering::SeqCst));
                self.source
                    .credit_withdrawal(U256::from(14_800_000_000_000_000u64));
                json!({"refid":"SYNTHETIC-WITHDRAWAL"})
            }
            E::KrakenPrivatePostWithdrawStatus => {
                json!([{"refid":"SYNTHETIC-WITHDRAWAL","status":"Success","asset":"XETH","method":"Ether","key":"synthetic-ethereum","info":self.wallet,"amount":"0.0148","fee":"0.0001","txid":super::full_plan_evm::withdrawal_hash()}])
            }
            E::KrakenPrivatePostLedgers => {
                json!({"ledger":{"WITHDRAW-LEDGER":{"refid":"SYNTHETIC-WITHDRAWAL","type":"withdrawal","asset":"XETH","amount":"-0.0148","fee":if self.funding_v1.load(Ordering::SeqCst) {"0.00009"} else {"0.0001"},"balance":self.eth()}}})
            }
            E::KrakenFundingGetFundingV1MethodsDirection => {
                json!({"methods":[{"method_id":METHOD,"method_name":"Ether","asset":{"class":"currency","name":"XETH"},"network":{"network_id":"00000000-0000-0000-0000-000000000002","network_name":"Ethereum"},"minimum_amount":"0.0001","maximum_amount":"2","fees":{"base":{"asset":{"class":"currency","name":"XETH"},"amount":"0.0001"},"included":false}}]})
            }
            E::KrakenFundingGetFundingV1Addresses => {
                json!({"addresses":[{"address_id":ADDRESS_ID,"scope":{"method_id":METHOD},"verified":true,"address_details":{"crypto":{"address":self.wallet}}}]})
            }
            E::KrakenFundingGetFundingV1FeesMethodId => {
                json!({"fee":{"asset":{"class":"currency","name":"XETH"},"amount":"0.0001"},"gross_amount":{"asset":{"class":"currency","name":"XETH"},"amount":"0.0149"},"net_amount":{"asset":{"class":"currency","name":"XETH"},"amount":"0.0148"}})
            }
            E::KrakenFundingGetFundingV1LimitsWithdrawalAssetClassAsset => {
                json!({"withdrawal_limits":[{"method_id":METHOD,"maximum_amount":{"asset":{"class":"currency","name":"XETH"},"amount":"1"}}]})
            }
            E::KrakenFundingPostFundingV1Withdrawals => {
                assert!(self.funding_v1.load(Ordering::SeqCst));
                assert!(self.sold.load(Ordering::SeqCst));
                assert_eq!(p["amount"]["asset_amount"]["amount"], "0.0148");
                assert_eq!(p["fee"]["fee_included"], false);
                assert_eq!(
                    p["fee"]["current_fee"]["max_fee"]["asset_amount"]["amount"],
                    "0.0001"
                );
                assert!(!self.withdrawn.swap(true, Ordering::SeqCst));
                self.source
                    .credit_withdrawal(U256::from(14_800_000_000_000_000u64));
                json!({"withdrawal_id":"SYNTHETIC-WITHDRAWAL","gross_amount":{"asset_amount":{"asset":{"class":"currency","name":"XETH"},"amount":"0.01489"}},"net_amount":{"asset_amount":{"asset":{"class":"currency","name":"XETH"},"amount":"0.0148"}},"fee":{"asset_amount":{"asset":{"class":"currency","name":"XETH"},"amount":"0.00009"}}})
            }
            E::KrakenFundingGetFundingV1Withdrawals => {
                json!({"withdrawals":[{"withdrawal_id":"SYNTHETIC-WITHDRAWAL","status":if self.funding_history_ready.load(Ordering::SeqCst) {"success"} else {"pending"},"address_id":ADDRESS_ID,"method_id":METHOD,"amount":{"asset":{"class":"currency","name":"XETH"},"amount":"0.0148"},"fee":{"asset":{"class":"currency","name":"XETH"},"amount":"0.00009"},"onchain_transaction":super::full_plan_evm::withdrawal_hash()}]})
            }
            _ => panic!("Unexpected synthetic endpoint {e:?}: {p}"),
        };
        Ok(KrakenResponse::Json(json!({"error":[],"result":result})))
    }
}
#[async_trait]
impl KrakenRecipientVerifier for Venue {
    async fn resolve(
        &self,
        network: &str,
        coin: &str,
        contract: Option<&str>,
        address: &str,
        selector: Option<&str>,
    ) -> Result<Option<KrakenDestination>> {
        assert_eq!(
            (network, coin, address),
            ("Ethereum", "XETH", self.wallet.as_str())
        );
        assert!(contract.is_none());
        assert_eq!(
            selector,
            Some(format!("ethereum:1:{}", self.wallet).as_str())
        );
        Ok(Some(KrakenDestination {
            enrolled: true,
            resource: ResourceId(format!("ethereum:1:{}", self.wallet)),
            address: self.wallet.clone(),
            chain_id: 1,
            network: None,
            cluster: None,
            mint: None,
            asset: AssetId {
                network: "ethereum:1".into(),
                reference: "native".into(),
                decimals: 18,
            },
            confirmations: 2,
        }))
    }
    async fn verify(&self, r: KrakenPayoutRequest) -> Result<KrakenPayoutResult> {
        assert_eq!(r.tx_hash, super::full_plan_evm::withdrawal_hash());
        assert_eq!(
            (r.minimum.as_str(), r.maximum.as_str()),
            ("0.0148", "0.0148")
        );
        assert_eq!(
            r.destination.resource.0,
            format!("ethereum:1:{}", self.wallet)
        );
        let proof = self
            .recipient
            .verify_external_credit(
                &r.tx_hash,
                &r.destination.address,
                &r.destination.asset,
                &r.minimum,
                &r.maximum,
                r.destination.confirmations,
            )
            .await?;
        Ok(KrakenPayoutResult {
            status: proof.status,
            amount: proof.amount_atoms,
            evidence: proof.evidence,
        })
    }
}
pub fn adapter(
    venue: Arc<Venue>,
    prices: Arc<dyn gofin_pricing::UsdPriceSource>,
) -> Arc<ExchangeAdapter<KrakenProfile>> {
    Arc::new(ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Spot,
            vec![KrakenAccount {
                name: "synthetic-spot".into(),
                resource: ResourceId(ACCOUNT.into()),
                credential_ref: "synthetic-never-a-real-key".into(),
                physical_identity: "synthetic-identity".into(),
                wallet_account_id: None,
                default_wallet_account_id: None,
                identity_verified: true,
                product: KrakenProduct::Spot,
            }],
            venue.clone(),
        )
        .unwrap()
        .with_prices(prices)
        .with_recipient_verifier(venue),
    ))
}
