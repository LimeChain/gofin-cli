//! Synthetic Binance transport feeding the real Base withdrawal receipt observer.
use super::*;
use fin_binance::*;
#[derive(Default)]
struct State {
    transferred: bool,
    withdrawn: bool,
    history: Vec<Value>,
    withdrawal: Vec<Value>,
    posts: usize,
}
pub(crate) struct BinanceFixture {
    state: Mutex<State>,
    source: Arc<Mutex<ChainState>>,
    owner: String,
}
impl BinanceFixture {
    pub(super) fn new(source: Arc<Mutex<ChainState>>, owner: String) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            source,
            owner,
        })
    }
    pub(crate) fn adapter(self: &Arc<Self>, wallet: Arc<EthereumAdapter>) -> Arc<BinanceAdapter> {
        Arc::new(
            BinanceAdapter::new(
                BinanceConfig {
                    accounts: vec![
                        BinanceAccount {
                            name: "model-main".into(),
                            resource: ResourceId("binance:uid:1001:SPOT".into()),
                            credential: CredentialHandle::new("test-only:main").unwrap(),
                            role: AccountRole::Main,
                            parent: None,
                            email: None,
                            wallet_type: "SPOT".into(),
                        },
                        BinanceAccount {
                            name: "model-child".into(),
                            resource: ResourceId("binance:uid:1002:SPOT".into()),
                            credential: CredentialHandle::new("test-only:child").unwrap(),
                            role: AccountRole::Sub,
                            parent: Some("model-main".into()),
                            email: Some("child@example.invalid".into()),
                            wallet_type: "SPOT".into(),
                        },
                    ],
                    destinations: vec![EthereumDestination {
                        name: "model-wallet".into(),
                        resource: ResourceId(format!("ethereum:8453:{}", self.owner)),
                        address: self.owner.clone(),
                        chain_id: 8453,
                        assets: BTreeMap::from([(
                            "USDC".into(),
                            usdc_asset(8453).unwrap().asset_id(),
                        )]),
                        confirmations: 1,
                    }],
                },
                self.clone(),
                Some(self.clone()),
                Some(Arc::new(Recipient(wallet))),
            )
            .unwrap(),
        )
    }
}
#[async_trait]
impl BinancePriceProvider for BinanceFixture {
    async fn quote_usd(&self, coin: &str) -> Result<PriceQuote> {
        assert_eq!(coin, "USDC");
        Ok(PriceQuote {
            usd_per_unit: "1".into(),
            observed_at: now(),
            source: "synthetic USDC".into(),
        })
    }
}
#[async_trait]
impl BinanceTransport for BinanceFixture {
    async fn request(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, TransportError> {
        let mut s = self.state.lock().unwrap();
        let p: BTreeMap<_, _> = request
            .params
            .iter()
            .map(|p| (p.key.as_str(), p.value.as_str()))
            .collect();
        use BinanceEndpoint as E;
        Ok(match request.endpoint {
            E::Account => {
                let main = credential.as_str() == "test-only:main";
                json!({"uid":if main{1001}else{1002},"accountType":"SPOT","balances":[{"asset":"USDC","free":if (main&&s.transferred&&!s.withdrawn)||(!main&&!s.transferred){"10"}else{"0"},"locked":"0"}]})
            }
            E::SubAccountTransferHistory => json!(if p.get("type") == Some(&"2") {
                s.history.iter().map(|r|json!({"counterParty":"master","email":"master@example.invalid","type":2,"asset":"USDC","qty":"10","fromAccountType":"SPOT","toAccountType":"SPOT","status":"SUCCESS","tranId":r["tranId"],"time":r["createTimeStamp"]})).collect::<Vec<_>>()
            } else {
                vec![]
            }),
            E::SubAccountSpotAssets => {
                json!({"balances":[{"asset":"USDC","free":if s.transferred{"0"}else{"10"},"locked":"0"}]})
            }
            E::ApiRestrictions => {
                json!({"enableInternalTransfer":true,"permitsUniversalTransfer":true,"enableWithdrawals":true})
            }
            E::SubAccountList => {
                json!({"subAccounts":[{"email":"child@example.invalid","isFreeze":false,"subAccountId":1002}]})
            }
            E::CapitalConfig => {
                json!([{"coin":"USDC","depositAllEnable":true,"withdrawAllEnable":true,"networkList":[{"network":"BASE","coin":"USDC","withdrawIntegerMultiple":"0.000001","withdrawFee":"0.2","withdrawMin":"1","withdrawMax":"1000","withdrawEnable":true,"depositEnable":true,"contractAddress":usdc_asset(8453).unwrap().token,"minConfirm":1,"unLockConfirm":1}]}])
            }
            E::WithdrawAddressList => json!([]),
            E::TravelRuleRequirements => json!({"questionnaireCountryCode":"NIL"}),
            E::UniversalTransferHistory => json!({"totalCount":s.history.len(),"result":s.history}),
            E::UniversalTransfer => {
                assert!(!s.transferred);
                assert_eq!(p["asset"], "USDC");
                assert_eq!(p["amount"], "10");
                s.transferred = true;
                s.posts += 1;
                let id = p["clientTranId"];
                s.history.push(json!({"tranId":"1","clientTranId":id,"asset":"USDC","amount":"10","fromAccountType":"SPOT","toAccountType":"SPOT","fromEmail":"child@example.invalid","toEmail":"master@example.invalid","status":"SUCCESS","createTimeStamp":chrono::Utc::now().timestamp_millis()}));
                json!({"tranId":"1","clientTranId":id})
            }
            E::WithdrawalHistory => json!(s.withdrawal),
            E::StandardWithdrawal => {
                assert!(s.transferred && !s.withdrawn);
                assert_eq!(p["coin"], "USDC");
                assert_eq!(p["network"], "BASE");
                assert_eq!(p["amount"], "10");
                assert_eq!(p["address"], self.owner);
                s.withdrawn = true;
                s.posts += 1;
                let hash = format!("0x{}", "ee".repeat(32));
                let token = address(&usdc_asset(8453).unwrap().token.unwrap()).unwrap();
                let sender = Address::repeat_byte(0xab);
                let recipient = address(&self.owner).unwrap();
                let event = Transfer {
                    from: sender,
                    to: recipient,
                    value: U256::from(9_800_000),
                };
                let log = json!({"address":lower_address(token),"topics":event.encode_topics().iter().map(|v|format!("{:#x}",v.0)).collect::<Vec<_>>(),"data":hex_bytes(&event.encode_data()),"logIndex":"0x0","transactionHash":hash,"blockHash":format!("{:#x}",block_hash(8453)),"blockNumber":"0x64","removed":false});
                let tx = json!({"hash":hash,"from":lower_address(sender),"to":lower_address(token),"value":"0x0","chainId":"0x2105","input":"0x","blockNumber":"0x64","blockHash":format!("{:#x}",block_hash(8453))});
                let receipt = json!({"transactionHash":hash,"blockNumber":"0x64","blockHash":format!("{:#x}",block_hash(8453)),"status":"0x1","logs":[log]});
                let mut chain = self.source.lock().unwrap();
                chain.source_balance = U256::from(9_800_000);
                chain
                    .transactions
                    .insert((8453, hash.clone()), (tx, receipt));
                s.withdrawal.push(json!({"id":"withdraw-1","withdrawOrderId":p["withdrawOrderId"],"coin":"USDC","network":"BASE","address":self.owner,"amount":"9.8","transactionFee":"0.2","status":6,"txId":hash,"transferType":0,"walletType":0,"applyTime":chrono::Utc::now().to_rfc3339()}));
                json!({"id":"withdraw-1"})
            }
            endpoint => panic!("Unexpected synthetic Binance endpoint {endpoint:?}"),
        })
    }
}
struct Recipient(Arc<EthereumAdapter>);
#[async_trait]
impl BinanceRecipientVerifier for Recipient {
    async fn verify(&self, r: PayoutRequest) -> PayoutResult {
        match self
            .0
            .verify_external_credit(
                &r.tx_hash,
                &r.recipient,
                &r.asset,
                &r.minimum_amount,
                &r.maximum_amount,
                r.confirmations,
            )
            .await
        {
            Ok(p) => PayoutResult {
                status: p.status,
                amount_atoms: p.amount_atoms,
                evidence: p.evidence,
            },
            Err(e) => PayoutResult {
                status: Status::Unknown,
                amount_atoms: None,
                evidence: json!({"error":e}),
            },
        }
    }
}
