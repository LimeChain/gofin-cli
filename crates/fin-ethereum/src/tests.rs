use super::*;
use abi::*;
use fin_evm::{
    EvmRpc,
    alloy::{
        primitives::{
            B256, I256,
            aliases::{U40, U80},
        },
        sol_types::{SolCall, SolEvent, SolValue},
    },
    hex_bytes, hex_quantity, quantity,
};
use std::sync::{
    Mutex,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

fn addr(n: u64) -> Address {
    Address::from_word(B256::from(U256::from(n).to_be_bytes::<32>()))
}
fn ray() -> U256 {
    pow10(27).unwrap()
}
fn abi_value<T: SolValue>(value: T) -> Value {
    json!(hex_bytes(&value.abi_encode()))
}
#[derive(Clone, Default)]
struct AtBlock {
    scaled_supply: U256,
    scaled_debt: U256,
    income: U256,
    debt_index: U256,
    supply_previous: U256,
    debt_previous: U256,
    collateral: bool,
    stable_debt: U256,
}
struct Fixture {
    latest: AtomicU64,
    states: Mutex<std::collections::BTreeMap<u64, AtBlock>>,
    tx: Mutex<Option<FrozenTransaction>>,
    receipt: Mutex<Value>,
    paused: Mutex<bool>,
    frozen: Mutex<bool>,
    erc20_false: Mutex<bool>,
    allowance: Mutex<U256>,
    nonce: AtomicU64,
    chain: AtomicU64,
    calls: Mutex<Vec<String>>,
    missing_selector: Mutex<Option<[u8; 4]>>,
    other_revision_reserve: Mutex<bool>,
    other_revision_balance: Mutex<U256>,
    selected_revision: AtomicU64,
    cash_override: Mutex<Option<U256>>,
    cash_by_owner: Mutex<std::collections::BTreeMap<Address, U256>>,
    other_stable: AtomicU64,
    include_weth: Mutex<bool>,
    feed_updated_at: Mutex<Option<u64>>,
    call_overrides: Mutex<std::collections::BTreeMap<[u8; 4], Value>>,
    simulation_override: Mutex<Option<Value>>,
    simulation_failure: Mutex<bool>,
}
impl Fixture {
    fn new() -> Self {
        let mut states = std::collections::BTreeMap::new();
        states.insert(
            20,
            AtBlock {
                income: ray() + U256::from(1),
                debt_index: ray() + U256::from(1),
                ..Default::default()
            },
        );
        Self {
            latest: AtomicU64::new(20),
            states: Mutex::new(states),
            tx: Mutex::new(None),
            receipt: Mutex::new(Value::Null),
            paused: Mutex::new(false),
            frozen: Mutex::new(false),
            erc20_false: Mutex::new(false),
            allowance: Mutex::new(U256::from(10_000_000_000u64)),
            nonce: AtomicU64::new(0),
            chain: AtomicU64::new(1),
            calls: Mutex::new(Vec::new()),
            missing_selector: Mutex::new(None),
            other_revision_reserve: Mutex::new(false),
            other_revision_balance: Mutex::new(U256::ZERO),
            selected_revision: AtomicU64::new(5),
            cash_override: Mutex::new(None),
            cash_by_owner: Mutex::new(std::collections::BTreeMap::new()),
            other_stable: AtomicU64::new(777),
            include_weth: Mutex::new(false),
            // Repeated reads of the same synthetic oracle round retain its
            // publication time even when the observation crosses a second.
            feed_updated_at: Mutex::new(Some(chrono::Utc::now().timestamp() as u64)),
            call_overrides: Mutex::new(std::collections::BTreeMap::new()),
            simulation_override: Mutex::new(None),
            simulation_failure: Mutex::new(false),
        }
    }
    fn state(&self, tag: &Value) -> AtBlock {
        let block = tag
            .as_str()
            .filter(|s| *s != "latest" && *s != "finalized")
            .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            .unwrap_or(self.latest.load(Ordering::SeqCst));
        self.states
            .lock()
            .unwrap()
            .range(..=block)
            .next_back()
            .unwrap()
            .1
            .clone()
    }
    fn post(&self, height: u64, state: AtBlock) {
        self.states.lock().unwrap().insert(height, state);
        self.latest.store(height + 1, Ordering::SeqCst);
    }
    fn set_receipt(&self, binding: &Binding, height: u64, logs: Vec<Value>) {
        *self.tx.lock().unwrap() = Some(binding.transaction.clone());
        *self.receipt.lock().unwrap() = json!({"transactionHash":txhash(),"blockHash":blockhash(height),"blockNumber":format!("{height:#x}"),"status":"0x1","gasUsed":"0x10000","effectiveGasPrice":"0x989680","logs":logs});
    }
}
fn txhash() -> String {
    format!("0x{}", "ab".repeat(32))
}

mod final_submission_valuation_tests {
    use super::*;
    use fin_engine::{Engine, EngineOptions};
    use fin_evm::{EnvelopeSigner, alloy::primitives::Bytes};
    use fin_store::Store;
    use std::sync::atomic::AtomicBool;

    struct CoinGeckoStylePrices {
        unit_price: AtomicU64,
        updated_at: AtomicU64,
        stale: AtomicBool,
        changed_identity: AtomicBool,
    }

    impl CoinGeckoStylePrices {
        fn fresh() -> Self {
            Self {
                unit_price: AtomicU64::new(3_000_000_000),
                updated_at: AtomicU64::new(
                    u64::try_from(chrono::Utc::now().timestamp()).unwrap() - 10,
                ),
                stale: AtomicBool::new(false),
                changed_identity: AtomicBool::new(false),
            }
        }
    }

    #[async_trait]
    impl WalletValuation for CoinGeckoStylePrices {
        async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
            let amount = fin_evm::u256(&units)?
                .checked_mul(U256::from(self.unit_price.load(Ordering::SeqCst)))
                .ok_or_else(|| {
                    error("SYNTHETIC_OVERFLOW", "test", "Synthetic quote overflowed.")
                })?
                / pow10(asset.decimals)?;
            let updated_at =
                chrono::DateTime::from_timestamp(self.updated_at.load(Ordering::SeqCst) as i64, 0)
                    .unwrap()
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            let coin_id = if self.changed_identity.load(Ordering::SeqCst) {
                "different-coin"
            } else {
                "ethereum"
            };
            Ok((
                quantity(amount),
                format!("CoinGecko public USD quote; coin_id={coin_id}; updated:{updated_at}"),
            ))
        }

        async fn quote_observation(
            &self,
            asset: &AssetId,
            units: Quantity,
        ) -> Result<(Quantity, String, String)> {
            let (value, source) = self.quote_usd(asset, units).await?;
            let observed_at = if self.stale.load(Ordering::SeqCst) {
                (chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339()
            } else {
                now()
            };
            Ok((value, source, observed_at))
        }
    }

    type ValuationMutation = (&'static str, fn(&mut Valuation));

    #[derive(Default)]
    struct CountingSigner(AtomicUsize);
    #[async_trait]
    impl EnvelopeSigner for CountingSigner {
        async fn sign_prepared(&self, _transaction: &FrozenTransaction) -> Result<Bytes> {
            self.0.fetch_add(1, Ordering::SeqCst);
            // No key or signed bytes: reaching the isolated signing seam ends
            // the test before the fixture's prohibited dispatch seam.
            Err(FinError::new(
                "SYNTHETIC_SIGNER_REACHED",
                "signing",
                "Isolated signing seam reached.",
                "Inspect fixture counters.",
            ))
        }
    }
    #[derive(Default)]
    struct CountingJournal {
        authorized: AtomicUsize,
        persisted: AtomicUsize,
    }
    impl SubmissionJournal for CountingJournal {
        fn assert_authorized(&self) -> Result<()> {
            self.authorized.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn persist_submission(&self, _submission: &Submission) -> Result<()> {
            self.persisted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    fn counted_adapter(rpc: Arc<Fixture>, signer: Arc<CountingSigner>) -> EthereumAdapter {
        let mut a = adapter(rpc.clone());
        a.runtime = Arc::new(EvmRuntime::new(rpc, 1, Some(signer)));
        a
    }
    fn wrap_input() -> Value {
        let mut v = input();
        v["amount"] = json!("0.001");
        v
    }
    fn assert_no_dispatch(rpc: &Fixture) {
        assert!(
            !rpc.calls
                .lock()
                .unwrap()
                .iter()
                .any(|method| method == "eth_sendRawTransaction")
        );
    }
    fn coin_transfer_input() -> Value {
        let mut value = input();
        value.as_object_mut().unwrap().extend(
            json!({
                "token":"native",
                "recipient":lower_address(addr(2)),
                "amount":"0.001001"
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        value
    }

    #[tokio::test]
    async fn coin_gecko_quote_refresh_within_displayed_ceiling_keeps_approval_stable() {
        let rpc = Arc::new(Fixture::new());
        let signer = Arc::new(CountingSigner::default());
        let prices = Arc::new(CoinGeckoStylePrices::fresh());
        let a = counted_adapter(rpc.clone(), signer.clone()).with_wallet_valuation(prices.clone());
        let prepared = a
            .prepare("wallet.transfer", &coin_transfer_input())
            .await
            .unwrap();
        assert!(
            prepared
                .binding
                .get(fin_domain::value_bounds::KEY)
                .is_some()
        );
        assert!(prepared.summary.contains("USD ceilings: $3.01 movement"));
        let approved_binding = fin_domain::authorization_binding(&prepared);
        let approved_price = prepared.valuation.as_ref().unwrap().prices[0]
            .usd_micros_per_unit
            .clone();

        prices.unit_price.store(3_001_000_000, Ordering::SeqCst);
        prices.updated_at.fetch_add(1, Ordering::SeqCst);
        let refreshed = a.revalidate(&prepared).await.unwrap();
        assert_eq!(
            fin_domain::authorization_binding(&refreshed),
            approved_binding,
            "a new quote round inside the visible ceiling must retain the approval binding"
        );
        assert_eq!(refreshed.summary, prepared.summary);
        assert_ne!(
            refreshed.valuation.as_ref().unwrap().prices[0].usd_micros_per_unit,
            approved_price,
            "fresh valuation evidence remains available for actual-cost accounting"
        );

        let journal = Arc::new(CountingJournal::default());
        assert_eq!(
            a.submit(&prepared, journal.clone()).await.unwrap_err().code,
            "SYNTHETIC_SIGNER_REACHED"
        );
        assert_eq!(signer.0.load(Ordering::SeqCst), 1);
        assert_eq!(journal.authorized.load(Ordering::SeqCst), 1);
        assert_eq!(journal.persisted.load(Ordering::SeqCst), 0);
        assert_no_dispatch(&rpc);
    }

    #[tokio::test]
    async fn price_above_ceiling_changed_identity_and_stale_quote_never_reach_signing() {
        for case in ["above-ceiling", "changed-identity", "stale"] {
            let rpc = Arc::new(Fixture::new());
            let signer = Arc::new(CountingSigner::default());
            let prices = Arc::new(CoinGeckoStylePrices::fresh());
            let a =
                counted_adapter(rpc.clone(), signer.clone()).with_wallet_valuation(prices.clone());
            let prepared = a
                .prepare("wallet.transfer", &coin_transfer_input())
                .await
                .unwrap();
            match case {
                "above-ceiling" => prices.unit_price.store(3_011_000_000, Ordering::SeqCst),
                "changed-identity" => prices.changed_identity.store(true, Ordering::SeqCst),
                _ => prices.stale.store(true, Ordering::SeqCst),
            }
            prices.updated_at.fetch_add(1, Ordering::SeqCst);
            assert!(
                a.revalidate(&prepared).await.is_err(),
                "{case} must fail during the fresh adapter check"
            );
            let journal = Arc::new(CountingJournal::default());
            assert!(
                a.submit(&prepared, journal.clone()).await.is_err(),
                "{case}"
            );
            assert_eq!(signer.0.load(Ordering::SeqCst), 0, "{case}");
            assert_eq!(journal.authorized.load(Ordering::SeqCst), 0, "{case}");
            assert_eq!(journal.persisted.load(Ordering::SeqCst), 0, "{case}");
            assert_no_dispatch(&rpc);
        }
    }

    #[tokio::test]
    async fn legacy_unbounded_preview_keeps_exact_source_and_price_revalidation() {
        let rpc = Arc::new(Fixture::new());
        let signer = Arc::new(CountingSigner::default());
        let prices = Arc::new(CoinGeckoStylePrices::fresh());
        let a = counted_adapter(rpc.clone(), signer.clone()).with_wallet_valuation(prices.clone());
        let prepared = a
            .prepare_operation(
                "wallet.transfer",
                &coin_transfer_input(),
                None,
                &PreparationContext::default(),
            )
            .await
            .unwrap();
        assert!(
            prepared
                .binding
                .get(fin_domain::value_bounds::KEY)
                .is_none()
        );
        prices.updated_at.fetch_add(1, Ordering::SeqCst);
        let journal = Arc::new(CountingJournal::default());
        assert_eq!(
            a.submit(&prepared, journal.clone()).await.unwrap_err().code,
            "VALUATION_CHANGED"
        );
        assert_eq!(signer.0.load(Ordering::SeqCst), 0);
        assert_eq!(journal.authorized.load(Ordering::SeqCst), 0);
        assert_eq!(journal.persisted.load(Ordering::SeqCst), 0);
        assert_no_dispatch(&rpc);
    }

    /// Changes the oracle only after the real post-approval adapter check has
    /// completed. Thus only EthereumAdapter::submit's final revalidation sees
    /// the new price; the engine's prior authorization comparison still passes.
    struct ChangeAfterApprovedRecheck {
        inner: EthereumAdapter,
        rpc: Arc<Fixture>,
        phases: AtomicUsize,
    }
    #[async_trait]
    impl Adapter for ChangeAfterApprovedRecheck {
        fn definitions(&self) -> Vec<Definition> {
            self.inner.definitions()
        }
        async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
            self.inner.read(operation, input).await
        }
        async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
            assert_eq!(self.phases.fetch_add(1, Ordering::SeqCst), 0);
            self.inner.prepare(operation, input).await
        }
        async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
            assert_eq!(self.phases.fetch_add(1, Ordering::SeqCst), 1);
            let refreshed = self.inner.revalidate(prepared).await?;
            assert_eq!(refreshed.binding, prepared.binding);
            require_frozen_valuation(refreshed.valuation.as_ref(), prepared.valuation.as_ref())?;
            self.rpc.call_overrides.lock().unwrap().insert(
                Oracle::getAssetPriceCall::SELECTOR,
                abi_value(U256::from(310_000_000_000u64)),
            );
            Ok(refreshed)
        }
        async fn submit(
            &self,
            prepared: &PreparedOperation,
            journal: Arc<dyn SubmissionJournal>,
        ) -> Result<Submission> {
            assert_eq!(self.phases.fetch_add(1, Ordering::SeqCst), 2);
            self.inner.submit(prepared, journal).await
        }
        async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
            self.inner.reconcile(record).await
        }
    }
    #[derive(Default)]
    struct SyntheticApproval(AtomicUsize);
    #[async_trait]
    impl Authorizer for SyntheticApproval {
        async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ApprovalAudit {
                mechanism: "synthetic-final-valuation-regression".into(),
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: now(),
                expires_at: challenge.expires_at.clone(),
            })
        }
    }

    #[tokio::test]
    async fn engine_final_oracle_change_stops_before_signing_or_identity_persistence() {
        let rpc = Arc::new(Fixture::new());
        let signer = Arc::new(CountingSigner::default());
        let a = Arc::new(ChangeAfterApprovedRecheck {
            inner: counted_adapter(rpc.clone(), signer.clone()),
            rpc: rpc.clone(),
            phases: AtomicUsize::new(0),
        });
        let approval = Arc::new(SyntheticApproval::default());
        let directory = tempfile::tempdir().unwrap();
        let engine = Engine::new(
            Store::open(directory.path().join("synthetic.sqlite")).unwrap(),
            vec![a.clone()],
            approval.clone(),
            EngineOptions::default(),
        )
        .unwrap();
        let request = OperationRequest {
            request_id: "synthetic-final-oracle-change".into(),
            operation: "wallet.wrap".into(),
            input: wrap_input(),
            run_id: None,
        };
        let record = engine.execute(request.clone()).await.unwrap();
        assert_eq!(record.error.as_ref().unwrap().code, "VALUE_BOUND_EXCEEDED");
        assert_eq!(record.status, Status::Failed);
        assert!(record.approval.is_some());
        assert!(record.submission.is_none());
        assert!(record.effects.is_none());
        assert_eq!(a.phases.load(Ordering::SeqCst), 3);
        assert_eq!(approval.0.load(Ordering::SeqCst), 1);
        assert_eq!(signer.0.load(Ordering::SeqCst), 0);
        assert_no_dispatch(&rpc);
        // The same logical identity keeps its durable failure; no new quote,
        // approval or submission is attempted on a duplicate request.
        let duplicate = engine.execute(request).await.unwrap();
        assert_eq!(duplicate.id, record.id);
        assert_eq!(duplicate.error, record.error);
        assert_eq!(a.phases.load(Ordering::SeqCst), 3);
        assert_eq!(approval.0.load(Ordering::SeqCst), 1);
        assert_eq!(signer.0.load(Ordering::SeqCst), 0);
        assert_no_dispatch(&rpc);
    }

    #[tokio::test]
    async fn unchanged_quotes_allow_new_timestamps_and_other_network_origin_rows() {
        let rpc = Arc::new(Fixture::new());
        let signer = Arc::new(CountingSigner::default());
        let a = counted_adapter(rpc.clone(), signer.clone());
        let mut p = a.prepare("wallet.wrap", &wrap_input()).await.unwrap();
        let v = p.valuation.as_mut().unwrap();
        let prior_observation = (chrono::Utc::now() - chrono::Duration::seconds(5))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        v.observed_at = prior_observation.clone();
        for row in &mut v.prices {
            row.observed_at = prior_observation.clone();
        }
        // Same reference/decimals but a different exact network must not be
        // mistaken for a duplicate or for the Ethereum adapter-owned row.
        let mut origin = v.prices[0].clone();
        origin.asset.network = "ethereum:11155111".into();
        origin.usd_micros_per_unit = Quantity::from_u64(7);
        origin.source = "synthetic-engine-origin".into();
        v.prices.push(origin);
        v.prices.reverse();
        let fresh = a.revalidate(&p).await.unwrap();
        assert_ne!(
            fresh.valuation.as_ref().unwrap().observed_at,
            prior_observation
        );
        assert_eq!(fresh.binding, p.binding);
        let journal = Arc::new(CountingJournal::default());
        assert_eq!(
            a.submit(&p, journal.clone()).await.unwrap_err().code,
            "SYNTHETIC_SIGNER_REACHED"
        );
        assert_eq!(signer.0.load(Ordering::SeqCst), 1);
        assert_eq!(journal.authorized.load(Ordering::SeqCst), 1);
        assert_eq!(journal.persisted.load(Ordering::SeqCst), 0);
        assert_no_dispatch(&rpc);
    }

    #[tokio::test]
    async fn changed_oracle_round_source_with_same_price_stops_before_signing() {
        let rpc = Arc::new(Fixture::new());
        let published = u64::try_from(chrono::Utc::now().timestamp()).unwrap() - 30;
        *rpc.feed_updated_at.lock().unwrap() = Some(published);
        let signer = Arc::new(CountingSigner::default());
        let a = counted_adapter(rpc.clone(), signer.clone());
        let prepared = a.prepare("wallet.wrap", &wrap_input()).await.unwrap();
        // Publish a new synthetic round with the same answer/asset price, but
        // a different publication timestamp. This is a changed exact source
        // observation, unlike changing only Valuation.observed_at.
        rpc.call_overrides.lock().unwrap().insert(
            PriceFeed::latestRoundDataCall::SELECTOR,
            abi_value((
                U80::from(2),
                I256::from_raw(U256::from(100_000_000)),
                U256::from(published + 1),
                U256::from(published + 1),
                U80::from(2),
            )),
        );
        assert_eq!(
            a.revalidate(&prepared).await.unwrap_err().code,
            "VALUE_BOUND_EXCEEDED",
            "Aave oracle publication timestamps remain part of the exact source identity"
        );
        let journal = Arc::new(CountingJournal::default());
        assert_eq!(
            a.submit(&prepared, journal.clone()).await.unwrap_err().code,
            "VALUE_BOUND_EXCEEDED"
        );
        assert_eq!(signer.0.load(Ordering::SeqCst), 0);
        assert_eq!(journal.authorized.load(Ordering::SeqCst), 0);
        assert_eq!(journal.persisted.load(Ordering::SeqCst), 0);
        assert_no_dispatch(&rpc);
    }

    #[tokio::test]
    async fn missing_duplicate_or_materially_changed_approved_valuation_never_signs() {
        let rpc = Arc::new(Fixture::new());
        let signer = Arc::new(CountingSigner::default());
        let a = counted_adapter(rpc.clone(), signer.clone());
        let prepared = a
            .prepare_operation(
                "wallet.wrap",
                &wrap_input(),
                None,
                &PreparationContext::default(),
            )
            .await
            .unwrap();
        let changes: &[ValuationMutation] = &[
            ("movement", |v| v.movement_usd_micros = Quantity::zero()),
            ("fee", |v| v.maximum_fee_usd_micros = Quantity::zero()),
            ("allocation", |v| {
                v.additional_allocation_usd_micros = Quantity::from_u64(1)
            }),
            ("valuation source", |v| v.source = "changed-source".into()),
            ("missing row", |v| {
                v.prices.remove(0);
            }),
            ("missing all rows", |v| v.prices.clear()),
            ("duplicate row", |v| v.prices.push(v.prices[0].clone())),
            ("changed USD", |v| {
                v.prices[0].usd_micros_per_unit = Quantity::from_u64(1)
            }),
            ("changed unit", |v| {
                v.prices[0].unit_atoms = Quantity::from_u64(1)
            }),
            ("changed price source", |v| {
                v.prices[0].source = "changed-source".into()
            }),
            ("blank price source", |v| v.prices[0].source = "  ".into()),
            ("zero price", |v| {
                v.prices[0].usd_micros_per_unit = Quantity::zero()
            }),
            ("zero unit", |v| v.prices[0].unit_atoms = Quantity::zero()),
            ("wrong network", |v| {
                v.prices[0].asset.network = "ethereum:11155111".into()
            }),
            ("wrong precision", |v| v.prices[0].asset.decimals = 17),
            ("wrong reference", |v| {
                v.prices[0].asset.reference = lower_address(addr(199))
            }),
        ];
        for (case, change) in changes {
            let mut p = prepared.clone();
            change(p.valuation.as_mut().unwrap());
            let journal = Arc::new(CountingJournal::default());
            let failure = a.submit(&p, journal.clone()).await.unwrap_err();
            assert_eq!(failure.code, "VALUATION_CHANGED", "{case}");
            assert_eq!(signer.0.load(Ordering::SeqCst), 0, "{case}");
            assert_eq!(journal.authorized.load(Ordering::SeqCst), 0, "{case}");
            assert_eq!(journal.persisted.load(Ordering::SeqCst), 0, "{case}");
            assert_no_dispatch(&rpc);
        }
        let mut missing = prepared;
        missing.valuation = None;
        let journal = Arc::new(CountingJournal::default());
        assert_eq!(
            a.submit(&missing, journal.clone()).await.unwrap_err().code,
            "VALUATION_CHANGED"
        );
        assert_eq!(signer.0.load(Ordering::SeqCst), 0);
        assert_eq!(journal.authorized.load(Ordering::SeqCst), 0);
        assert_eq!(journal.persisted.load(Ordering::SeqCst), 0);
        assert_no_dispatch(&rpc);
    }

    #[tokio::test]
    async fn malformed_or_duplicate_fresh_prices_fail_closed_even_when_frozen_identically() {
        let rpc = Arc::new(Fixture::new());
        let prepared = adapter(rpc)
            .prepare("wallet.wrap", &wrap_input())
            .await
            .unwrap();
        let valuation = prepared.valuation.unwrap();
        let invalid: &[ValuationMutation] = &[
            ("missing prices", |v| v.prices.clear()),
            ("duplicate prices", |v| v.prices.push(v.prices[0].clone())),
            ("zero USD", |v| {
                v.prices[0].usd_micros_per_unit = Quantity::zero()
            }),
            ("zero unit", |v| v.prices[0].unit_atoms = Quantity::zero()),
            ("blank price source", |v| v.prices[0].source = " ".into()),
            ("blank valuation source", |v| v.source = " ".into()),
            ("blank network", |v| v.prices[0].asset.network.clear()),
            ("blank reference", |v| v.prices[0].asset.reference.clear()),
        ];
        for (case, invalidate) in invalid {
            let mut malformed = valuation.clone();
            invalidate(&mut malformed);
            assert_eq!(
                require_frozen_valuation(Some(&malformed), Some(&malformed))
                    .unwrap_err()
                    .code,
                "VALUATION_CHANGED",
                "{case}"
            );
        }
        assert_eq!(
            require_frozen_valuation(None, Some(&valuation))
                .unwrap_err()
                .code,
            "VALUATION_CHANGED"
        );
        assert_eq!(
            require_frozen_valuation(None, None).unwrap_err().code,
            "VALUATION_CHANGED"
        );
    }
}

#[tokio::test]
async fn external_funding_proves_net_credit_and_checks_finality_before_revert() {
    let rpc = Arc::new(Fixture::new());
    let a = adapter(rpc.clone());
    let mut v = input();
    v["token"] = json!(lower_address(addr(101)));
    v["amount"] = json!("1");
    v["recipient"] = json!(lower_address(addr(2)));
    let p = a.prepare("wallet.transfer", &v).await.unwrap();
    let b: Binding = serde_json::from_value(p.binding).unwrap();
    let asset = a.asset(&lower_address(addr(101)), 6);
    let target = lower_address(addr(1));
    let q = U256::from(1_000_000);
    rpc.latest.store(22, Ordering::SeqCst);
    rpc.set_receipt(
        &b,
        21,
        vec![event(
            addr(101),
            Erc20::Transfer {
                from: addr(2),
                to: addr(1),
                value: q,
            },
            21,
        )],
    );
    let good = a
        .verify_external_credit(&txhash(), &target, &asset, "1", "1", 2)
        .await
        .unwrap();
    assert_eq!(good.status, Status::Completed);
    assert_eq!(good.amount_atoms, Some(quantity(q)));
    rpc.set_receipt(
        &b,
        21,
        vec![event(
            addr(101),
            Erc20::Transfer {
                from: addr(1),
                to: addr(1),
                value: q,
            },
            21,
        )],
    );
    assert_eq!(
        a.verify_external_credit(&txhash(), &target, &asset, "1", "1", 2)
            .await
            .unwrap_err()
            .code,
        "FUNDING_AMOUNT_MISMATCH"
    );
    rpc.receipt.lock().unwrap()["status"] = json!("0x0");
    rpc.latest.store(21, Ordering::SeqCst);
    assert_eq!(
        a.verify_external_credit(&txhash(), &target, &asset, "1", "1", 2)
            .await
            .unwrap()
            .status,
        Status::Pending
    );
    rpc.latest.store(22, Ordering::SeqCst);
    assert_eq!(
        a.verify_external_credit(&txhash(), &target, &asset, "1", "1", 2)
            .await
            .unwrap()
            .status,
        Status::Failed
    );
    assert_eq!(
        a.verify_external_credit(&txhash(), &target, &asset, "1", "1", 1)
            .await
            .unwrap_err()
            .code,
        "FINALITY_LIMIT"
    );
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .contains(&"eth_sendRawTransaction".into())
    );
}

struct ExternalPayoutFinalityRpc {
    inner: Arc<Fixture>,
    finalized_calls: AtomicUsize,
    finalized_height: Option<u64>,
}

#[async_trait]
impl EvmRpc for ExternalPayoutFinalityRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "eth_getBlockByNumber" && params[0] == json!("finalized") {
            self.finalized_calls.fetch_add(1, Ordering::SeqCst);
            return match self.finalized_height {
                Some(height) => {
                    Ok(json!({"number":format!("{height:#x}"),"hash":blockhash(height)}))
                }
                None => Err(FinError::new(
                    "SYNTHETIC_FINALITY_UNSUPPORTED",
                    "rpc",
                    "Synthetic provider does not implement the finalized tag.",
                    "Fixture only.",
                )),
            };
        }
        self.inner.request(method, params).await
    }
}

struct MissingHistoricalTokenStateRpc {
    inner: Arc<dyn EvmRpc>,
}

#[async_trait]
impl EvmRpc for MissingHistoricalTokenStateRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "eth_call"
            && params[1].as_str().is_some_and(|tag| tag != "latest")
            && params[0]["data"].as_str().is_some_and(|data| {
                data.starts_with(&format!(
                    "0x{}",
                    hex::encode(Erc20::balanceOfCall::SELECTOR)
                ))
            })
        {
            return Err(FinError::new(
                "SYNTHETIC_HISTORICAL_STATE_UNAVAILABLE",
                "rpc",
                "Historical token state is unavailable in the isolated fixture.",
                "Fixture only.",
            ));
        }
        self.inner.request(method, params).await
    }
}

struct ReorgAfterHistoricalTokenReadRpc {
    inner: Arc<dyn EvmRpc>,
    historical_token_read: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl EvmRpc for ReorgAfterHistoricalTokenReadRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "eth_call"
            && params[1].as_str().is_some_and(|tag| tag != "latest")
            && params[0]["data"].as_str().is_some_and(|data| {
                data.starts_with(&format!(
                    "0x{}",
                    hex::encode(Erc20::balanceOfCall::SELECTOR)
                ))
            })
        {
            self.historical_token_read.store(true, Ordering::SeqCst);
        }
        if method == "eth_getBlockByNumber"
            && params[0] == json!("0x15")
            && self.historical_token_read.load(Ordering::SeqCst)
        {
            return Ok(json!({"number":"0x15","hash":blockhash(99)}));
        }
        self.inner.request(method, params).await
    }
}

fn payout_proof_adapter(chain: u64, rpc: Arc<dyn EvmRpc>) -> EthereumAdapter {
    EthereumAdapter::new(
        EthereumConfig::wallet_only(chain, 2, vec![]),
        vec![WalletConnection {
            name: "synthetic-payout-target".into(),
            address: lower_address(addr(2)),
            signing_enabled: false,
        }],
        Arc::new(EvmRuntime::new(rpc, chain, None)),
    )
    .unwrap()
}

fn set_external_native_receipt(rpc: &Fixture, chain: u64) {
    let block = 21u64;
    rpc.chain.store(chain, Ordering::SeqCst);
    *rpc.tx.lock().unwrap() = Some(FrozenTransaction {
        rollup_fee: None,
        chain_id: chain,
        sender: lower_address(addr(1)),
        to: lower_address(addr(2)),
        value: Quantity::from_decimal("0.001", 18).unwrap(),
        calldata: "0x".into(),
        nonce: 0,
        gas: 21_000,
        max_fee_per_gas: Quantity::from_u64(1),
        max_priority_fee_per_gas: Quantity::from_u64(1),
    });
    *rpc.receipt.lock().unwrap() = json!({
        "transactionHash":txhash(),
        "blockHash":blockhash(block),
        "blockNumber":format!("{block:#x}"),
        "status":"0x1",
        "gasUsed":"0x5208",
        "effectiveGasPrice":"0x1",
        "logs":[]
    });
}

fn set_external_erc20_receipt(rpc: &Fixture, chain: u64, logs: Vec<Value>) {
    let block = 21u64;
    rpc.chain.store(chain, Ordering::SeqCst);
    *rpc.tx.lock().unwrap() = Some(FrozenTransaction {
        rollup_fee: None,
        chain_id: chain,
        sender: lower_address(addr(1)),
        to: lower_address(addr(101)),
        value: Quantity::zero(),
        calldata: "0xa9059cbb".into(),
        nonce: 0,
        gas: 65_000,
        max_fee_per_gas: Quantity::from_u64(1),
        max_priority_fee_per_gas: Quantity::from_u64(1),
    });
    *rpc.receipt.lock().unwrap() = json!({
        "transactionHash":txhash(),
        "blockHash":blockhash(block),
        "blockNumber":format!("{block:#x}"),
        "status":"0x1",
        "gasUsed":"0x10000",
        "effectiveGasPrice":"0x1",
        "logs":logs
    });
}

struct FailedPayoutReceiptRpc(Arc<Fixture>);
#[async_trait]
impl EvmRpc for FailedPayoutReceiptRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "eth_getTransactionReceipt" {
            return Err(FinError::new(
                "RPC_UNAVAILABLE",
                "rpc",
                "Synthetic isolated receipt RPC failure.",
                "Retry reconciliation of the same withdrawal identity.",
            ));
        }
        self.0.request(method, params).await
    }
}

#[tokio::test]
async fn payout_rpc_failures_report_only_the_reviewed_method_name() {
    let fixture = Arc::new(Fixture::new());
    fixture.latest.store(21, Ordering::SeqCst);
    set_external_native_receipt(&fixture, 1);
    let adapter = payout_proof_adapter(1, Arc::new(FailedPayoutReceiptRpc(fixture)));

    let error = adapter
        .verify_external_credit(
            &txhash(),
            &lower_address(addr(2)),
            &AssetId {
                network: "ethereum:1".into(),
                reference: "native".into(),
                decimals: 18,
            },
            "0.001",
            "0.001",
            2,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "RPC_UNAVAILABLE");
    assert_eq!(error.stage, "external-credit:eth_getTransactionReceipt");
    assert!(!error.stage.contains("http"));
}

#[tokio::test]
async fn external_payouts_use_l2_sequencer_inclusion_without_weakening_mainnet_finality() {
    for chain in [42161, 4663, 8453] {
        for case in [
            "success",
            "wrong-recipient",
            "revert",
            "wrong-hash",
            "noncanonical",
        ] {
            let rpc = Arc::new(Fixture::new());
            let finality_rpc = Arc::new(ExternalPayoutFinalityRpc {
                inner: rpc.clone(),
                finalized_calls: AtomicUsize::new(0),
                finalized_height: None,
            });
            let adapter = payout_proof_adapter(chain, finality_rpc.clone());
            rpc.latest.store(21, Ordering::SeqCst); // One sequencer-included L2 block.
            set_external_native_receipt(&rpc, chain);
            match case {
                "revert" => rpc.receipt.lock().unwrap()["status"] = json!("0x0"),
                "wrong-hash" => {
                    rpc.receipt.lock().unwrap()["transactionHash"] =
                        json!(format!("0x{}", "ef".repeat(32)))
                }
                "noncanonical" => rpc.receipt.lock().unwrap()["blockHash"] = json!(blockhash(99)),
                _ => {}
            }
            let recipient = if case == "wrong-recipient" {
                lower_address(addr(3))
            } else {
                lower_address(addr(2))
            };
            let result = adapter
                .verify_external_credit(
                    &txhash(),
                    &recipient,
                    &AssetId {
                        network: format!("ethereum:{chain}"),
                        reference: "native".into(),
                        decimals: 18,
                    },
                    "0.001",
                    "0.001",
                    1,
                )
                .await;
            match case {
                "success" => {
                    let proof = result.unwrap();
                    assert_eq!(proof.status, Status::Completed, "{chain}: {case}");
                    assert_eq!(proof.evidence["finality"], json!("sequencer-inclusion"));
                    assert_eq!(proof.evidence["confirmations"], json!(1));
                }
                "revert" => assert_eq!(result.unwrap().status, Status::Failed, "{chain}: {case}"),
                "noncanonical" => {
                    assert_eq!(result.unwrap().status, Status::Pending, "{chain}: {case}")
                }
                "wrong-recipient" => assert_eq!(
                    result.unwrap_err().code,
                    "FUNDING_RECIPIENT_MISMATCH",
                    "{chain}: {case}"
                ),
                "wrong-hash" => assert_eq!(
                    result.unwrap_err().code,
                    "FUNDING_IDENTITY_MISMATCH",
                    "{chain}: {case}"
                ),
                _ => unreachable!(),
            }
            assert_eq!(
                finality_rpc.finalized_calls.load(Ordering::SeqCst),
                0,
                "{chain}: {case}"
            );
        }

        let amount_atoms = U256::from(1_250_000u64);
        let asset = AssetId {
            network: format!("ethereum:{chain}"),
            reference: lower_address(addr(101)),
            decimals: 6,
        };
        let rpc = Arc::new(Fixture::new());
        let finality_rpc = Arc::new(ExternalPayoutFinalityRpc {
            inner: rpc.clone(),
            finalized_calls: AtomicUsize::new(0),
            finalized_height: None,
        });
        let adapter = payout_proof_adapter(chain, finality_rpc.clone());
        rpc.latest.store(21, Ordering::SeqCst);
        set_external_erc20_receipt(
            &rpc,
            chain,
            vec![event(
                addr(101),
                Erc20::Transfer {
                    from: addr(3),
                    to: addr(2),
                    value: amount_atoms,
                },
                21,
            )],
        );
        let proof = adapter
            .verify_external_credit(
                &txhash(),
                &lower_address(addr(2)),
                &asset,
                "1.25",
                "1.25",
                1,
            )
            .await
            .unwrap();
        assert_eq!(proof.status, Status::Completed, "{chain}: ERC20 payout");
        assert_eq!(proof.amount_atoms, Some(quantity(amount_atoms)));
        assert_eq!(proof.evidence["finality"], json!("sequencer-inclusion"));
        assert_eq!(proof.evidence["recipient"], json!(lower_address(addr(2))));
        assert_eq!(finality_rpc.finalized_calls.load(Ordering::SeqCst), 0);

        // The exact inclusion is checked again after historical state reads;
        // if it changed during those reads, the credit remains unresolved.
        let rpc = Arc::new(Fixture::new());
        rpc.latest.store(21, Ordering::SeqCst);
        set_external_erc20_receipt(
            &rpc,
            chain,
            vec![event(
                addr(101),
                Erc20::Transfer {
                    from: addr(3),
                    to: addr(2),
                    value: amount_atoms,
                },
                21,
            )],
        );
        let finality_rpc = Arc::new(ExternalPayoutFinalityRpc {
            inner: rpc.clone(),
            finalized_calls: AtomicUsize::new(0),
            finalized_height: None,
        });
        let reorg_rpc = Arc::new(ReorgAfterHistoricalTokenReadRpc {
            inner: finality_rpc.clone(),
            historical_token_read: std::sync::atomic::AtomicBool::new(false),
        });
        let adapter = payout_proof_adapter(chain, reorg_rpc.clone());
        let proof = adapter
            .verify_external_credit(
                &txhash(),
                &lower_address(addr(2)),
                &asset,
                "1.25",
                "1.25",
                1,
            )
            .await
            .unwrap();
        assert_eq!(
            proof.status,
            Status::Pending,
            "{chain}: reorg after historical read"
        );
        assert_eq!(proof.evidence["canonical_confirmed"], json!(false));
        assert_eq!(finality_rpc.finalized_calls.load(Ordering::SeqCst), 0);

        // Inclusion policy does not bypass historical holding checks. If the
        // configured RPC cannot serve the exact historical token state, proof
        // remains an error for the caller to preserve as unresolved.
        let rpc = Arc::new(Fixture::new());
        rpc.latest.store(21, Ordering::SeqCst);
        set_external_erc20_receipt(
            &rpc,
            chain,
            vec![event(
                addr(101),
                Erc20::Transfer {
                    from: addr(3),
                    to: addr(2),
                    value: amount_atoms,
                },
                21,
            )],
        );
        let finality_rpc = Arc::new(ExternalPayoutFinalityRpc {
            inner: rpc.clone(),
            finalized_calls: AtomicUsize::new(0),
            finalized_height: None,
        });
        let historical_rpc = Arc::new(MissingHistoricalTokenStateRpc {
            inner: finality_rpc.clone(),
        });
        let adapter = payout_proof_adapter(chain, historical_rpc);
        assert_eq!(
            adapter
                .verify_external_credit(
                    &txhash(),
                    &lower_address(addr(2)),
                    &asset,
                    "1.25",
                    "1.25",
                    1,
                )
                .await
                .unwrap_err()
                .code,
            "SYNTHETIC_HISTORICAL_STATE_UNAVAILABLE"
        );
        assert_eq!(finality_rpc.finalized_calls.load(Ordering::SeqCst), 0);
    }

    // Ethereum mainnet still requires its configured two confirmations and
    // the provider's finalized marker when that policy is enabled.
    let rpc = Arc::new(Fixture::new());
    let finality_rpc = Arc::new(ExternalPayoutFinalityRpc {
        inner: rpc.clone(),
        finalized_calls: AtomicUsize::new(0),
        finalized_height: Some(21),
    });
    let adapter = payout_proof_adapter(1, finality_rpc.clone());
    rpc.latest.store(22, Ordering::SeqCst);
    set_external_native_receipt(&rpc, 1);
    let proof = adapter
        .verify_external_credit(
            &txhash(),
            &lower_address(addr(2)),
            &AssetId {
                network: "ethereum:1".into(),
                reference: "native".into(),
                decimals: 18,
            },
            "0.001",
            "0.001",
            2,
        )
        .await
        .unwrap();
    assert_eq!(proof.status, Status::Completed);
    assert_eq!(proof.evidence["finality"], json!("rpc-finalized"));
    assert_eq!(finality_rpc.finalized_calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn collateral_requires_matching_event_and_inclusion_state() {
    let rpc = Arc::new(Fixture::new());
    rpc.states.lock().unwrap().insert(
        20,
        AtBlock {
            scaled_supply: U256::from(1_000_000),
            income: ray(),
            debt_index: ray(),
            supply_previous: ray(),
            collateral: true,
            ..Default::default()
        },
    );
    let a = adapter(rpc.clone());
    let p = a
        .prepare("aave.collateral", &aave_input("aave.collateral"))
        .await
        .unwrap();
    let b: Binding = serde_json::from_value(p.binding.clone()).unwrap();
    rpc.post(
        21,
        AtBlock {
            scaled_supply: U256::from(1_000_000),
            income: ray(),
            debt_index: ray(),
            supply_previous: ray(),
            collateral: false,
            ..Default::default()
        },
    );
    rpc.set_receipt(&b, 21, vec![]);
    let r = record(p);
    assert_eq!(
        a.reconcile(&r).await.unwrap_err().code,
        "COLLATERAL_PROOF_MISMATCH"
    );
    rpc.set_receipt(
        &b,
        21,
        vec![event(
            addr(300),
            Pool::ReserveUsedAsCollateralDisabled {
                reserve: addr(101),
                user: addr(1),
            },
            21,
        )],
    );
    let done = a.reconcile(&r).await.unwrap();
    assert_eq!(done.status, Status::Completed);
    assert!(
        done.verified
            .unwrap()
            .effects
            .iter()
            .any(|e| matches!(e,FinancialEffect::AuthorityChange{after,..} if after=="false"))
    );
}

#[tokio::test]
async fn wallet_transfer_wrap_unwrap_and_approval_require_exact_original_effects() {
    for operation in [
        "wallet.transfer",
        "wallet.wrap",
        "wallet.unwrap",
        "wallet.approve",
    ] {
        let rpc = Arc::new(Fixture::new());
        let a = adapter(rpc.clone());
        let mut v = input();
        v["amount"] = json!("0.001");
        if matches!(operation, "wallet.transfer" | "wallet.approve") {
            v["token"] = json!(lower_address(addr(101)));
            v["amount"] = json!("1");
        }
        if operation == "wallet.transfer" {
            v["recipient"] = json!(lower_address(addr(2)))
        }
        if operation == "wallet.approve" {
            v["spender"] = json!(lower_address(addr(300)))
        }
        let p = a.prepare(operation, &v).await.unwrap();
        let b: Binding = serde_json::from_value(p.binding.clone()).unwrap();
        let q = fin_evm::u256(&b.units).unwrap();
        let logs = match operation {
            "wallet.transfer" => vec![event(
                addr(101),
                Erc20::Transfer {
                    from: addr(1),
                    to: addr(2),
                    value: q,
                },
                21,
            )],
            "wallet.wrap" => vec![event(
                addr(100),
                Weth::Deposit {
                    dst: addr(1),
                    wad: q,
                },
                21,
            )],
            "wallet.unwrap" => vec![event(
                addr(100),
                Weth::Withdrawal {
                    src: addr(1),
                    wad: q,
                },
                21,
            )],
            _ => {
                *rpc.allowance.lock().unwrap() = q;
                vec![event(
                    addr(101),
                    Erc20::Approval {
                        owner: addr(1),
                        spender: addr(300),
                        value: q,
                    },
                    21,
                )]
            }
        };
        rpc.latest.store(22, Ordering::SeqCst);
        rpc.set_receipt(&b, 21, Vec::new());
        let r = record(p);
        assert!(a.reconcile(&r).await.is_err());
        rpc.set_receipt(&b, 21, logs);
        let done = a.reconcile(&r).await.unwrap();
        assert_eq!(done.status, Status::Completed);
        assert!(done.verified.unwrap().effects.len() >= 2);
    }
    let rpc = Arc::new(Fixture::new());
    let a = adapter(rpc.clone());
    let mut v = input();
    v["token"] = json!("native");
    v["amount"] = json!("0.001");
    v["recipient"] = json!(lower_address(addr(2)));
    let p = a.prepare("wallet.transfer", &v).await.unwrap();
    let b: Binding = serde_json::from_value(p.binding.clone()).unwrap();
    rpc.latest.store(22, Ordering::SeqCst);
    rpc.set_receipt(&b, 21, vec![]);
    assert_eq!(
        a.reconcile(&record(p)).await.unwrap().status,
        Status::Completed
    );
}
#[tokio::test]
async fn uniswap_requires_bound_pool_cash_legs_minimum_output_and_finite_allowance() {
    let rpc = Arc::new(Fixture::new());
    let a = adapter(rpc.clone());
    let mut v = input();
    v["token_in"] = json!(lower_address(addr(100)));
    v["token_out"] = json!(lower_address(addr(101)));
    v["amount"] = json!("0.001");
    v["recipient"] = json!(lower_address(addr(1)));
    v["min_output"] = json!("2.97");
    v["slippage_bps"] = json!(100);
    v["deadline"] = json!(chrono::Utc::now().timestamp() as u64 + 120);
    *rpc.allowance.lock().unwrap() = U256::MAX;
    assert_eq!(
        a.prepare("uniswap.swap", &v).await.unwrap_err().code,
        "ALLOWANCE_INSUFFICIENT"
    );
    *rpc.allowance.lock().unwrap() = U256::from(10_000_000_000_000_000u64);
    let p = a.prepare("uniswap.swap", &v).await.unwrap();
    assert_eq!(
        p.valuation.as_ref().unwrap().movement_usd_micros,
        Quantity::from_u64(3_000_000)
    );
    let b: Binding = serde_json::from_value(p.binding.clone()).unwrap();
    assert_eq!(b.fee_tier, Some(500));
    let q = fin_evm::u256(&b.units).unwrap();
    let logs = |out| {
        vec![
            event(
                addr(500),
                UniswapPool::Swap {
                    sender: addr(470),
                    recipient: addr(1),
                    amount0: I256::from_raw(q),
                    amount1: -I256::from_raw(out),
                    sqrtPriceX96: Default::default(),
                    liquidity: 1,
                    tick: Default::default(),
                },
                21,
            ),
            event(
                addr(100),
                Erc20::Transfer {
                    from: addr(1),
                    to: addr(500),
                    value: q,
                },
                21,
            ),
            event(
                addr(101),
                Erc20::Transfer {
                    from: addr(500),
                    to: addr(1),
                    value: out,
                },
                21,
            ),
        ]
    };
    rpc.latest.store(22, Ordering::SeqCst);
    rpc.set_receipt(&b, 21, logs(U256::from(2_000_000)));
    let r = record(p);
    assert_eq!(
        a.reconcile(&r).await.unwrap_err().code,
        "SWAP_PROOF_MISMATCH"
    );
    rpc.set_receipt(&b, 21, logs(U256::from(3_000_000)));
    assert_eq!(a.reconcile(&r).await.unwrap().status, Status::Completed);
}
fn blockhash(n: u64) -> String {
    format!("{:#x}", B256::from(U256::from(n).to_be_bytes::<32>()))
}
fn event<E: SolEvent>(emitter: Address, e: E, height: u64) -> Value {
    let log = e.encode_log_data();
    json!({"address":lower_address(emitter),"topics":log.topics().iter().map(|t|format!("{t:#x}")).collect::<Vec<_>>(),"data":hex_bytes(&log.data),"removed":false,"transactionHash":txhash(),"blockHash":blockhash(height),"blockNumber":format!("{height:#x}")})
}
#[async_trait]
impl EvmRpc for Fixture {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.calls.lock().unwrap().push(method.into());
        let latest = self.latest.load(Ordering::SeqCst);
        Ok(match method {
            "eth_chainId" => json!(format!("{:#x}", self.chain.load(Ordering::SeqCst))),
            "eth_blockNumber" => json!(format!("{latest:#x}")),
            "eth_getBlockByNumber" => {
                let n = params[0]
                    .as_str()
                    .filter(|s| *s != "latest" && *s != "finalized")
                    .map(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap())
                    .unwrap_or(latest);
                json!({"hash":blockhash(n),"number":format!("{n:#x}"),"baseFeePerGas":"0x1"})
            }
            "eth_getCode" => json!(if [addr(1), addr(2)]
                .contains(&address(params[0].as_str().unwrap())?)
            {
                "0x"
            } else {
                "0x6000"
            }),
            "eth_getBalance" => json!("0xde0b6b3a7640000"),
            "eth_getTransactionCount" => json!(format!("{:#x}", self.nonce.load(Ordering::SeqCst))),
            "eth_estimateGas" => json!("0x186a0"),
            "eth_maxPriorityFeePerGas" => json!("0x989680"),
            "eth_getTransactionReceipt" => self.receipt.lock().unwrap().clone(),
            "eth_getTransactionByHash" => {
                let locked = self.tx.lock().unwrap();
                let t = locked.as_ref().expect("test sets original transaction");
                let r = self.receipt.lock().unwrap();
                json!({"hash":txhash(),"from":t.sender,"to":t.to,"nonce":format!("{:#x}",t.nonce),"chainId":format!("{:#x}",t.chain_id),"type":"0x2","gas":format!("{:#x}",t.gas),"value":hex_quantity(fin_evm::u256(&t.value)?),"input":t.calldata,"maxFeePerGas":hex_quantity(fin_evm::u256(&t.max_fee_per_gas)?),"maxPriorityFeePerGas":hex_quantity(fin_evm::u256(&t.max_priority_fee_per_gas)?),"accessList":[],"blockHash":r["blockHash"],"blockNumber":r["blockNumber"]})
            }
            "eth_call" => {
                if params[0].get("from").is_some() {
                    if *self.simulation_failure.lock().unwrap() {
                        return Err(FinError::new(
                            "SYNTHETIC_SIMULATION_FAILED",
                            "simulation",
                            "Isolated executable call simulation failed.",
                            "Inspect fixture.",
                        ));
                    }
                    if let Some(value) = self.simulation_override.lock().unwrap().clone() {
                        return Ok(value);
                    }
                }
                let data = fin_evm::rpc_bytes(&params[0]["data"])?;
                if data.is_empty() {
                    return Ok(json!("0x"));
                }
                let selector: [u8; 4] = data[..4].try_into().unwrap();
                if *self.missing_selector.lock().unwrap() == Some(selector) {
                    return Err(FinError::new(
                        "SYNTHETIC_REQUIRED_READ_FAILED",
                        "read",
                        "Isolated required read failed.",
                        "Inspect fixture.",
                    ));
                }
                if let Some(value) = self.call_overrides.lock().unwrap().get(&selector).cloned() {
                    return Ok(value);
                }
                let to = address(params[0]["to"].as_str().unwrap())?;
                let s = self.state(&params[1]);
                let token = if to == addr(100) { WETH } else { USDC };
                let supplied = anchors::backing(s.scaled_supply, s.income, &PositionKind::Supply)?;
                let debt =
                    anchors::backing(s.scaled_debt, s.debt_index, &PositionKind::VariableDebt)?;
                match selector {
                    Erc20::decimalsCall::SELECTOR => {
                        abi_value(U256::from(if token == WETH { 18u8 } else { 6u8 }))
                    }
                    Erc20::symbolCall::SELECTOR => {
                        abi_value(if token == WETH { "WETH" } else { "USDC" }.to_string())
                    }
                    Erc20::balanceOfCall::SELECTOR => {
                        abi_value(if to == addr(111) || to == addr(121) {
                            *self.other_revision_balance.lock().unwrap()
                        } else if to == addr(110) {
                            supplied
                        } else if to == addr(120) {
                            debt
                        } else if Erc20::balanceOfCall::abi_decode_validate(&data)
                            .unwrap()
                            .owner
                            == addr(110)
                        {
                            pow10(18)?
                        } else {
                            self.cash_by_owner
                                .lock()
                                .unwrap()
                                .get(
                                    &Erc20::balanceOfCall::abi_decode_validate(&data)
                                        .unwrap()
                                        .owner,
                                )
                                .copied()
                                .unwrap_or(self.cash_override.lock().unwrap().unwrap_or(pow10(18)?))
                        })
                    }
                    Erc20::totalSupplyCall::SELECTOR => abi_value(U256::from(1_000_000_000u64)),
                    Erc20::allowanceCall::SELECTOR => abi_value(*self.allowance.lock().unwrap()),
                    Erc20::transferCall::SELECTOR | Erc20::approveCall::SELECTOR => {
                        abi_value(!*self.erc20_false.lock().unwrap())
                    }
                    Pool::ADDRESSES_PROVIDERCall::SELECTOR => abi_value(addr(200)),
                    AddressesProvider::getPoolCall::SELECTOR => abi_value(addr(300)),
                    AddressesProvider::getPriceOracleCall::SELECTOR => abi_value(addr(400)),
                    AddressesProvider::getPoolDataProviderCall::SELECTOR => abi_value(addr(210)),
                    Pool::getReservesListCall::SELECTOR => {
                        abi_value(if *self.include_weth.lock().unwrap() {
                            vec![addr(100), addr(101), addr(102)]
                        } else {
                            vec![addr(101), addr(102)]
                        })
                    }
                    DataProvider::getReserveConfigurationDataCall::SELECTOR => abi_value((
                        U256::from(
                            if DataProvider::getReserveConfigurationDataCall::abi_decode_validate(
                                &data,
                            )
                            .unwrap()
                            .asset
                                == addr(100)
                            {
                                18
                            } else {
                                6
                            },
                        ),
                        U256::from(8000),
                        U256::from(8500),
                        U256::from(10500),
                        U256::from(1000),
                        true,
                        true,
                        false,
                        true,
                        *self.frozen.lock().unwrap(),
                    )),
                    DataProvider::getReserveTokensAddressesCall::SELECTOR => {
                        let call =
                            DataProvider::getReserveTokensAddressesCall::abi_decode_validate(&data)
                                .unwrap();
                        if call.asset == addr(102) && *self.other_revision_reserve.lock().unwrap() {
                            abi_value((addr(111), addr(115), addr(121)))
                        } else {
                            abi_value((addr(110), addr(115), addr(120)))
                        }
                    }
                    DataProvider::getReserveCapsCall::SELECTOR => {
                        abi_value((U256::ZERO, U256::ZERO))
                    }
                    DataProvider::getPausedCall::SELECTOR => {
                        abi_value(*self.paused.lock().unwrap())
                    }
                    DataProvider::getUserReserveDataCall::SELECTOR => {
                        let call = DataProvider::getUserReserveDataCall::abi_decode_validate(&data)
                            .unwrap();
                        let other =
                            call.asset == addr(102) && *self.other_revision_reserve.lock().unwrap();
                        let other_units = *self.other_revision_balance.lock().unwrap();
                        abi_value((
                            if other { other_units } else { supplied },
                            if call.asset == addr(102) {
                                U256::from(self.other_stable.load(Ordering::SeqCst))
                            } else {
                                s.stable_debt
                            },
                            if other { other_units } else { debt },
                            U256::ZERO,
                            if other { other_units } else { s.scaled_debt },
                            U256::ZERO,
                            U256::ZERO,
                            U40::ZERO,
                            s.collateral,
                        ))
                    }
                    Pool::getUserAccountDataCall::SELECTOR => abi_value((
                        U256::from(100_000_000_000u64),
                        U256::ZERO,
                        U256::from(80_000_000_000u64),
                        U256::from(8500),
                        U256::from(8000),
                        U256::MAX,
                    )),
                    AToken::ATOKEN_REVISIONCall::SELECTOR
                    | AToken::DEBT_TOKEN_REVISIONCall::SELECTOR => {
                        abi_value(U256::from(if to == addr(111) || to == addr(121) {
                            4
                        } else {
                            self.selected_revision.load(Ordering::SeqCst)
                        }))
                    }
                    AToken::scaledBalanceOfCall::SELECTOR => {
                        abi_value(if to == addr(111) || to == addr(121) {
                            *self.other_revision_balance.lock().unwrap()
                        } else if to == addr(110) {
                            s.scaled_supply
                        } else {
                            s.scaled_debt
                        })
                    }
                    AToken::getPreviousIndexCall::SELECTOR => abi_value(if to == addr(110) {
                        s.supply_previous
                    } else {
                        s.debt_previous
                    }),
                    Pool::getReserveNormalizedIncomeCall::SELECTOR => abi_value(s.income),
                    Pool::getReserveNormalizedVariableDebtCall::SELECTOR => abi_value(s.debt_index),
                    Oracle::BASE_CURRENCYCall::SELECTOR => abi_value(Address::ZERO),
                    Oracle::BASE_CURRENCY_UNITCall::SELECTOR => abi_value(U256::from(100_000_000)),
                    Oracle::getAssetPriceCall::SELECTOR => {
                        let call = Oracle::getAssetPriceCall::abi_decode_validate(&data).unwrap();
                        abi_value(U256::from(if call.asset == addr(100) {
                            300_000_000_000u64
                        } else {
                            100_000_000u64
                        }))
                    }
                    Oracle::getSourceOfAssetCall::SELECTOR => abi_value(addr(410)),
                    PriceFeed::latestRoundDataCall::SELECTOR => abi_value((
                        U80::from(1),
                        I256::from_raw(U256::from(100_000_000)),
                        U256::ZERO,
                        U256::from(
                            self.feed_updated_at
                                .lock()
                                .unwrap()
                                .unwrap_or(chrono::Utc::now().timestamp() as u64),
                        ),
                        U80::from(1),
                    )),
                    Factory::getPoolCall::SELECTOR => {
                        let call = Factory::getPoolCall::abi_decode_validate(&data).unwrap();
                        abi_value(if call.fee.to::<u32>() == 500 {
                            addr(500)
                        } else {
                            Address::ZERO
                        })
                    }
                    Quoter::quoteExactInputSingleCall::SELECTOR => abi_value((
                        U256::from(3_000_000),
                        fin_evm::alloy::primitives::aliases::U160::ZERO,
                        0u32,
                        U256::from(100000),
                    )),
                    Weth::depositCall::SELECTOR
                    | Weth::withdrawCall::SELECTOR
                    | Pool::supplyCall::SELECTOR
                    | Pool::borrowCall::SELECTOR
                    | Pool::setUserUseReserveAsCollateralCall::SELECTOR => json!("0x"),
                    Router::exactInputSingleCall::SELECTOR => abi_value(U256::from(3_000_000)),
                    Pool::repayCall::SELECTOR => abi_value(U256::from(1_000_000)),
                    Pool::withdrawCall::SELECTOR => {
                        let q = Pool::withdrawCall::abi_decode_validate(&data)
                            .unwrap()
                            .amount;
                        abi_value(if q == U256::MAX { supplied } else { q })
                    }
                    _ => {
                        return Err(FinError::validation(format!(
                            "Unimplemented isolated selector {}",
                            hex::encode(selector)
                        )));
                    }
                }
            }
            "eth_sendRawTransaction" => {
                return Err(FinError::validation(
                    "Protocol fixtures must never dispatch.",
                ));
            }
            _ => return Err(FinError::validation("Unimplemented isolated RPC method.")),
        })
    }
}
fn adapter(rpc: Arc<Fixture>) -> EthereumAdapter {
    let config = EthereumConfig {
        weth: Some(lower_address(addr(100))),
        snapshot_tokens: vec![lower_address(addr(101))],
        uniswap: Some(UniswapConfig {
            factory: lower_address(addr(450)),
            quoter: lower_address(addr(460)),
            router: lower_address(addr(470)),
        }),
        aave: Some(AaveConfig {
            pool: lower_address(addr(300)),
            oracle: lower_address(addr(400)),
        }),
        ..Default::default()
    };
    EthereumAdapter::new(
        config,
        vec![WalletConnection {
            name: "synthetic".into(),
            address: lower_address(addr(1)),
            signing_enabled: true,
        }],
        Arc::new(EvmRuntime::new(rpc, 1, None)),
    )
    .unwrap()
}
fn input() -> Value {
    json!({"wallet":"synthetic","network":"ethereum","chain_id":1,"request_id":"isolated-request","max_fee_eth":"0.001"})
}
fn swap_input() -> Value {
    let mut value = input();
    value.as_object_mut().unwrap().extend(
        json!({
            "token_in":lower_address(addr(100)), "token_out":lower_address(addr(101)),
            "amount":"0.001", "recipient":lower_address(addr(1)),
            "min_output":"2.97", "slippage_bps":100,
            "deadline":chrono::Utc::now().timestamp() as u64+120,
            "max_fee_eth":"0.0001",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    value
}
fn synthetic_route_context(
    baseline: StateSnapshot,
    allocations: Vec<ResourceAmount>,
) -> ReturnRouteContext {
    ReturnRouteContext {
        projected: ProjectedState {
            cash: baseline
                .state
                .holdings
                .iter()
                .map(|holding| ProjectedCash {
                    resource: holding.resource.clone(),
                    asset: holding.amount.asset.clone(),
                    amount: QuantityInterval {
                        minimum: holding.amount.atoms.clone(),
                        maximum: holding.amount.atoms.clone(),
                    },
                })
                .collect(),
            positions: baseline
                .state
                .positions
                .iter()
                .map(|position| ProjectedPosition {
                    resource: position.resource.clone(),
                    position: position.position.clone(),
                    asset: position.amount.asset.clone(),
                    position_kind: position.kind.clone(),
                    amount: QuantityInterval {
                        minimum: position.amount.atoms.clone(),
                        maximum: position.amount.atoms.clone(),
                    },
                })
                .collect(),
            authorities: baseline.state.authorities.clone(),
        },
        baseline,
        allocations,
        step_index: 0,
        prior_proofs: vec![],
        remaining_cost_budget_usd_micros: Quantity::from_u64(10_000_000),
        asset_prices: vec![],
    }
}

#[tokio::test]
async fn failed_zero_malformed_quotes_and_foreign_swap_recipient_fail_before_simulation() {
    for case in 0..4 {
        let rpc = Arc::new(Fixture::new());
        *rpc.allowance.lock().unwrap() = exact("0.01", 18).unwrap();
        let mut a = adapter(rpc.clone());
        a.config.snapshot_tokens.push(lower_address(addr(100)));
        let baseline = a.read_snapshot().await.unwrap();
        let context = synthetic_route_context(
            baseline,
            vec![
                ResourceAmount {
                    resource: resource(1, addr(1)),
                    amount: amount(&a.native_asset(), exact("0.001", 18).unwrap()),
                },
                ResourceAmount {
                    resource: resource(1, addr(1)),
                    amount: amount(
                        &a.asset(&lower_address(addr(100)), 18),
                        exact("0.001", 18).unwrap(),
                    ),
                },
            ],
        );
        let mut request = swap_input();
        let expected = if case == 3 {
            "SWAP_RECIPIENT_UNSUPPORTED"
        } else {
            "QUOTE_UNAVAILABLE"
        };
        match case {
            0 => {
                *rpc.missing_selector.lock().unwrap() =
                    Some(Quoter::quoteExactInputSingleCall::SELECTOR)
            }
            1 => {
                rpc.call_overrides.lock().unwrap().insert(
                    Quoter::quoteExactInputSingleCall::SELECTOR,
                    abi_value((
                        U256::ZERO,
                        fin_evm::alloy::primitives::aliases::U160::ZERO,
                        0u32,
                        U256::from(100000),
                    )),
                );
            }
            2 => {
                rpc.call_overrides
                    .lock()
                    .unwrap()
                    .insert(Quoter::quoteExactInputSingleCall::SELECTOR, json!("0x"));
            }
            3 => request["recipient"] = json!(lower_address(addr(2))),
            _ => unreachable!(),
        }
        assert_eq!(
            a.prepare("uniswap.swap", &request).await.unwrap_err().code,
            expected,
            "ordinary case {case}"
        );
        assert_eq!(
            a.preflight_return_route("uniswap.swap", &request, &context)
                .await
                .unwrap_err()
                .code,
            expected,
            "route case {case}"
        );
        assert!(
            !rpc.calls
                .lock()
                .unwrap()
                .iter()
                .any(|method| method == "eth_estimateGas" || method == "eth_sendRawTransaction")
        );
    }
}

#[tokio::test]
async fn failed_malformed_and_below_minimum_simulations_never_become_swap_previews() {
    for (case, expected) in [
        (0, "SYNTHETIC_SIMULATION_FAILED"),
        (1, "SIMULATION_RETURN_INVALID"),
        (2, "SWAP_SIMULATION_LIMIT"),
    ] {
        let rpc = Arc::new(Fixture::new());
        *rpc.allowance.lock().unwrap() = exact("0.01", 18).unwrap();
        match case {
            0 => *rpc.simulation_failure.lock().unwrap() = true,
            1 => *rpc.simulation_override.lock().unwrap() = Some(json!("0x")),
            2 => *rpc.simulation_override.lock().unwrap() = Some(abi_value(U256::from(2_000_000))),
            _ => unreachable!(),
        }
        assert_eq!(
            adapter(rpc.clone())
                .prepare("uniswap.swap", &swap_input())
                .await
                .unwrap_err()
                .code,
            expected
        );
        assert!(
            !rpc.calls
                .lock()
                .unwrap()
                .contains(&"eth_sendRawTransaction".into())
        );
        if case == 0 {
            assert!(
                !rpc.calls
                    .lock()
                    .unwrap()
                    .contains(&"eth_estimateGas".into())
            );
        }
    }
}

#[tokio::test]
async fn changed_quote_simulation_nonce_chain_and_frozen_calldata_invalidate_old_preview() {
    for (case, expected) in [
        (0, "QUOTE_UNAVAILABLE"),
        (1, "SLIPPAGE_LIMIT"),
        (2, "SWAP_SIMULATION_LIMIT"),
        (3, "NONCE_CHANGED"),
        (4, "WRONG_CHAIN"),
        (5, "BINDING_CHANGED"),
        (6, "WRONG_CHAIN"),
        (7, "ORACLE_STALE"),
    ] {
        let rpc = Arc::new(Fixture::new());
        *rpc.allowance.lock().unwrap() = exact("0.01", 18).unwrap();
        let a = adapter(rpc.clone());
        let preview = a.prepare("uniswap.swap", &swap_input()).await.unwrap();
        let original = preview.binding.clone();
        let mut candidate = preview.clone();
        match case {
            0 => {
                *rpc.missing_selector.lock().unwrap() =
                    Some(Quoter::quoteExactInputSingleCall::SELECTOR);
            }
            1 => {
                rpc.call_overrides.lock().unwrap().insert(
                    Quoter::quoteExactInputSingleCall::SELECTOR,
                    abi_value((
                        U256::from(2_500_000),
                        fin_evm::alloy::primitives::aliases::U160::ZERO,
                        0u32,
                        U256::from(100000),
                    )),
                );
            }
            2 => {
                *rpc.simulation_override.lock().unwrap() = Some(abi_value(U256::from(2_000_000)));
            }
            3 => rpc.nonce.store(1, Ordering::SeqCst),
            4 => rpc.chain.store(2, Ordering::SeqCst),
            5 => candidate.binding["transaction"]["calldata"] = json!("0x1234"),
            6 => candidate.binding["transaction"]["chain_id"] = json!(2),
            7 => {
                *rpc.feed_updated_at.lock().unwrap() =
                    Some(chrono::Utc::now().timestamp() as u64 + 60);
            }
            _ => unreachable!(),
        }
        assert_eq!(
            a.revalidate(&candidate).await.unwrap_err().code,
            expected,
            "case {case}"
        );
        assert_eq!(preview.binding, original);
        assert!(
            !rpc.calls
                .lock()
                .unwrap()
                .contains(&"eth_sendRawTransaction".into())
        );
    }
}

#[tokio::test]
async fn production_oracle_rejects_future_stale_incomplete_negative_and_zero_feeds() {
    let timestamp = chrono::Utc::now().timestamp() as u64;
    for case in 0..7 {
        let rpc = Arc::new(Fixture::new());
        let (answer, updated, answered) = match case {
            0 => (
                I256::from_raw(U256::from(100_000_000)),
                timestamp + 60,
                U80::from(1),
            ),
            1 => (
                I256::from_raw(U256::from(100_000_000)),
                timestamp - 86_401,
                U80::from(1),
            ),
            2 => (
                I256::from_raw(U256::from(100_000_000)),
                timestamp,
                U80::ZERO,
            ),
            3 => (I256::ZERO, timestamp, U80::from(1)),
            4 => (-I256::from_raw(U256::from(1)), timestamp, U80::from(1)),
            5 => (I256::from_raw(U256::from(100_000_000)), 0, U80::from(1)),
            _ => (
                I256::from_raw(U256::from(100_000_000)),
                timestamp,
                U80::from(1),
            ),
        };
        rpc.call_overrides.lock().unwrap().insert(
            PriceFeed::latestRoundDataCall::SELECTOR,
            abi_value((
                U80::from(1),
                answer,
                U256::ZERO,
                U256::from(updated),
                answered,
            )),
        );
        if case == 6 {
            rpc.call_overrides
                .lock()
                .unwrap()
                .insert(Oracle::getAssetPriceCall::SELECTOR, abi_value(U256::ZERO));
        }
        let error = adapter(rpc.clone())
            .quote_usd(&lower_address(addr(101)), Quantity::from_u64(1_000_000))
            .await
            .unwrap_err();
        assert_eq!(
            error.code,
            if case == 6 {
                "ORACLE_PRICE_INVALID"
            } else {
                "ORACLE_STALE"
            },
            "case {case}"
        );
        assert!(
            !rpc.calls
                .lock()
                .unwrap()
                .iter()
                .any(|method| method == "eth_estimateGas" || method == "eth_sendRawTransaction")
        );
    }
}
fn aave_input(op: &str) -> Value {
    let mut v = input();
    v["token"] = json!(lower_address(addr(101)));
    if op == "aave.collateral" {
        v["enabled"] = json!(false)
    } else {
        v["amount"] = json!("1")
    }
    if op == "aave.borrow" {
        v["min_health_factor"] = json!("1.5")
    }
    if op == "aave.withdraw" {
        v["recipient"] = json!(lower_address(addr(1)))
    }
    v
}
fn record(preview: PreparedOperation) -> OperationRecord {
    let b: Binding = serde_json::from_value(preview.binding.clone()).unwrap();
    let submission = Submission {
        kind: "ethereum".into(),
        reference: txhash(),
        data: json!({"transaction":b.transaction,"metadata":{"operation":preview.operation,"resource":b.resource}}),
    };
    OperationRecord {
        id: "isolated-op".into(),
        request_id: "isolated-request".into(),
        fingerprint: "isolated".into(),
        fingerprint_version: 1,
        operation: preview.operation.clone(),
        input: preview.input.clone(),
        status: Status::Submitted,
        created_at: now(),
        updated_at: now(),
        preview: Some(preview),
        approval: None,
        submission: Some(submission),
        dispatch_evidence: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    }
}

#[test]
fn schemas_are_operation_specific_and_unknown_or_imprecise_requests_fail() {
    let defs = operation_definitions();
    assert_eq!(defs.len(), 17);
    let transfer = defs.iter().find(|d| d.name == "wallet.transfer").unwrap();
    assert_eq!(transfer.input_schema["additionalProperties"], json!(false));
    assert!(transfer.input_schema["properties"].get("enabled").is_none());
    let mut bad = input();
    bad["amount"] = json!("1");
    bad["enabled"] = json!(true);
    assert!(request::parse("wallet.wrap", &bad).is_err());
    for bad in ["1e-6", "-1", "00.1", "0.0000001", "1."] {
        assert!(exact(bad, 6).is_err())
    }
}
#[test]
fn revision_five_rounding_is_proved_and_not_a_tolerance() {
    let q = Quantity::from_u64(1_000_000);
    let index = quantity(ray() + U256::from(1));
    let p = proof::revision_five_backing(
        q.clone(),
        Quantity::zero(),
        Quantity::from_u64(999999),
        Quantity::zero(),
        index.clone(),
        index,
    )
    .unwrap();
    assert_eq!(p.rounding_loss, Quantity::from_u64(1));
    assert_eq!(p.backed_principal, Quantity::from_u64(999999));
    assert!(
        proof::revision_five_backing(
            q,
            Quantity::zero(),
            Quantity::from_u64(1000000),
            Quantity::zero(),
            quantity(ray() + U256::from(1)),
            quantity(ray() + U256::from(1))
        )
        .is_err()
    );
    assert!(
        proof::revision_five_backing(
            Quantity::new(U256::MAX.to_string()).unwrap(),
            Quantity::zero(),
            Quantity::from_u64(1),
            Quantity::zero(),
            quantity(ray()),
            quantity(ray())
        )
        .is_err()
    );
}
#[test]
fn partial_repayments_carry_unpaid_interest_and_rounding_without_double_cost() {
    let (first_cost, interest, rounding) = proof::settle_debt_costs(
        U256::from(900),
        U256::ZERO,
        U256::from(1),
        U256::from(100),
        U256::from(99),
    )
    .unwrap();
    assert_eq!(first_cost, U256::from(100));
    assert_eq!(interest, U256::from(801));
    assert_eq!(rounding, U256::from(1));
    let (second_cost, interest, rounding) = proof::settle_debt_costs(
        U256::ZERO,
        interest,
        rounding,
        U256::from(1802),
        U256::from(1802),
    )
    .unwrap();
    assert_eq!(first_cost + second_cost, U256::from(902));
    assert_eq!(interest, U256::ZERO);
    assert_eq!(rounding, U256::ZERO);
    assert!(
        proof::settle_debt_costs(
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U256::from(1),
            U256::from(2)
        )
        .is_err()
    );
}
#[tokio::test]
async fn complete_snapshot_includes_unrequested_reserve_stable_debt_and_fails_closed_on_missing_read()
 {
    let rpc = Arc::new(Fixture::new());
    let a = adapter(rpc.clone());
    let s = a.read_snapshot().await.unwrap();
    assert!(s.complete);
    assert_eq!(s.state.positions.len(), 6);
    assert!(s.coverage.contains(&resource(1, addr(1)).0));
    assert!(
        s.state.positions.iter().any(
            |p| p.kind == PositionKind::StableDebt && p.amount.atoms == Quantity::from_u64(777)
        )
    );
    assert_eq!(s.state.holdings.len(), 2);
    assert_eq!(s.state.authorities.len(), 4);
    *rpc.missing_selector.lock().unwrap() = Some(DataProvider::getUserReserveDataCall::SELECTOR);
    assert_eq!(
        a.read_snapshot().await.unwrap_err().code,
        "SYNTHETIC_REQUIRED_READ_FAILED"
    );
}

/// Synthetic transport instrumentation only: no sockets, keys or external RPC.
struct SnapshotProbeRpc {
    inner: Arc<Fixture>,
    active: Arc<AtomicUsize>,
    peak: AtomicUsize,
    requests: Mutex<Vec<(String, Value)>>,
    headers: AtomicUsize,
    header_failure: AtomicU64,
    pending_nonce: AtomicU64,
}
struct SnapshotReadGuard(Arc<AtomicUsize>);
impl Drop for SnapshotReadGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl SnapshotProbeRpc {
    fn new(inner: Arc<Fixture>) -> Self {
        Self {
            inner,
            active: Arc::new(AtomicUsize::new(0)),
            peak: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
            headers: AtomicUsize::new(0),
            header_failure: AtomicU64::new(0),
            pending_nonce: AtomicU64::new(u64::MAX),
        }
    }
}
#[async_trait]
impl EvmRpc for SnapshotProbeRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let _guard = SnapshotReadGuard(self.active.clone());
        self.peak.fetch_max(active, Ordering::SeqCst);
        self.requests
            .lock()
            .unwrap()
            .push((method.into(), params.clone()));
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        if method == "eth_getBlockByNumber" {
            let nth = self.headers.fetch_add(1, Ordering::SeqCst) + 1;
            // Initial header, one shared anchor header, then the uncached final
            // recheck. Reorg/pruning appears only at the final transport fetch.
            if nth >= 3 {
                match self.header_failure.load(Ordering::SeqCst) {
                    1 => return Ok(json!({"hash":blockhash(999),"number":"0x14"})),
                    2 => return Ok(Value::Null),
                    _ => {}
                }
            }
        }
        if method == "eth_getTransactionCount" && params[1] == "pending" {
            let nonce = self.pending_nonce.load(Ordering::SeqCst);
            if nonce != u64::MAX {
                return Ok(json!(format!("{nonce:#x}")));
            }
        }
        self.inner.request(method, params).await
    }
}
fn probe_adapter(probe: Arc<SnapshotProbeRpc>, wallet_count: u64) -> EthereumAdapter {
    let mut a = adapter(probe.inner.clone());
    for n in 1..wallet_count {
        a.wallets.push(WalletConnection {
            name: format!("synthetic-{n}"),
            address: lower_address(addr(n + 1)),
            signing_enabled: false,
        });
    }
    a.runtime = Arc::new(EvmRuntime::new(probe, 1, None));
    a
}
fn snapshot_financial_output(mut snapshot: StateSnapshot) -> Value {
    snapshot.observed_at.clear();
    for position in &mut snapshot.state.positions {
        if let Some(anchor) = &mut position.anchor {
            anchor.observed_at.clear();
        }
    }
    serde_json::to_value(snapshot).unwrap()
}
#[tokio::test]
async fn concurrent_snapshot_preserves_all_ordered_financial_and_backing_evidence_with_bounded_reads()
 {
    let rpc = Arc::new(Fixture::new());
    {
        let mut states = rpc.states.lock().unwrap();
        let s = states.get_mut(&20).unwrap();
        s.scaled_supply = U256::from(1000);
        s.scaled_debt = U256::from(2000);
    }
    let serial = Arc::new(SnapshotProbeRpc::new(rpc.clone()));
    let concurrent = Arc::new(SnapshotProbeRpc::new(rpc));
    let expected = probe_adapter(serial, 8).capture_snapshot(1).await.unwrap();
    let actual = probe_adapter(concurrent.clone(), 8)
        .read_snapshot()
        .await
        .unwrap();
    assert!(actual.complete);
    assert_eq!(actual.state.holdings.len(), 16);
    assert_eq!(actual.state.positions.len(), 48);
    assert_eq!(actual.state.authorities.len(), 32);
    assert_eq!(actual.coverage.len(), 8);
    assert_eq!(
        snapshot_financial_output(expected),
        snapshot_financial_output(actual)
    );
    assert!(concurrent.peak.load(Ordering::SeqCst) > 1);
    assert!(concurrent.peak.load(Ordering::SeqCst) <= snapshot::SNAPSHOT_CONCURRENCY);
    assert_eq!(concurrent.active.load(Ordering::SeqCst), 0);
    let requests = concurrent.requests.lock().unwrap();
    let mut pinned = std::collections::BTreeSet::new();
    for (method, params) in requests.iter() {
        if matches!(
            method.as_str(),
            "eth_call" | "eth_getCode" | "eth_getBalance" | "eth_getTransactionCount"
        ) && params[1] == "0x14"
        {
            assert!(
                pinned.insert(format!("{method}:{params}")),
                "duplicate pinned transport read: {method}"
            );
        }
        if method == "eth_getCode" {
            assert_eq!(params[1], "0x14");
        }
        assert_ne!(method, "eth_sendRawTransaction");
        assert_ne!(method, "eth_estimateGas");
    }
    // Header cache must not swallow the start/final canonical transport checks.
    assert_eq!(concurrent.headers.load(Ordering::SeqCst), 3);
}
#[tokio::test]
async fn concurrent_snapshot_rejects_final_reorg_or_missing_header_after_cached_anchor_reads() {
    for failure in [1, 2] {
        let probe = Arc::new(SnapshotProbeRpc::new(Arc::new(Fixture::new())));
        probe.header_failure.store(failure, Ordering::SeqCst);
        assert_eq!(
            probe_adapter(probe.clone(), 4)
                .read_snapshot()
                .await
                .unwrap_err()
                .code,
            "SNAPSHOT_CHANGED"
        );
        assert_eq!(probe.headers.load(Ordering::SeqCst), 3);
        assert_eq!(probe.active.load(Ordering::SeqCst), 0);
    }
}
#[tokio::test]
async fn failed_concurrent_snapshot_drains_readers_and_new_capture_retries_required_evidence() {
    let rpc = Arc::new(Fixture::new());
    *rpc.missing_selector.lock().unwrap() = Some(DataProvider::getUserReserveDataCall::SELECTOR);
    let probe = Arc::new(SnapshotProbeRpc::new(rpc.clone()));
    let a = probe_adapter(probe.clone(), 8);
    assert_eq!(
        a.read_snapshot().await.unwrap_err().code,
        "SYNTHETIC_REQUIRED_READ_FAILED"
    );
    assert_eq!(probe.active.load(Ordering::SeqCst), 0);
    *rpc.missing_selector.lock().unwrap() = None;
    let restored = a.read_snapshot().await.unwrap();
    assert!(restored.complete);
    assert_eq!(restored.state.positions.len(), 48);
    assert_eq!(probe.active.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn concurrent_snapshot_retains_pending_nonce_classification_and_does_not_cache_across_captures()
 {
    let probe = Arc::new(SnapshotProbeRpc::new(Arc::new(Fixture::new())));
    let a = probe_adapter(probe.clone(), 2);
    let initial = a.read_snapshot().await.unwrap();
    assert!(initial.complete);
    probe.pending_nonce.store(1, Ordering::SeqCst);
    let pending = a.read_snapshot().await.unwrap();
    assert!(!pending.complete);
    assert_eq!(pending.pending.len(), 2);
    assert_eq!(pending.pending, pending.state.in_flight);
    assert_eq!(pending.state.holdings, initial.state.holdings);
    assert_eq!(pending.state.authorities, initial.state.authorities);
    assert_eq!(pending.state.positions.len(), initial.state.positions.len());
    let requests = probe.requests.lock().unwrap();
    for wallet in &a.wallets {
        for tag in ["0x14", "pending"] {
            assert_eq!(
                requests
                    .iter()
                    .filter(|(method, p)| method == "eth_getTransactionCount"
                        && p[0] == wallet.address
                        && p[1] == tag)
                    .count(),
                2
            );
        }
    }
}
#[tokio::test]
async fn snapshot_coverage_is_stable_across_blocks_while_anchor_evidence_remains_fresh() {
    let rpc = Arc::new(Fixture::new());
    let a = adapter(rpc.clone());
    let initial = a.read_snapshot().await.unwrap();
    rpc.latest.store(23, Ordering::SeqCst);
    let final_state = a.read_snapshot().await.unwrap();
    assert_eq!(initial.coverage, vec![resource(1, addr(1)).0]);
    assert_eq!(initial.coverage, final_state.coverage);
    assert_eq!(initial.state.holdings, final_state.state.holdings);
    assert_eq!(initial.state.authorities, final_state.state.authorities);
    assert_eq!(
        initial.state.positions.len(),
        final_state.state.positions.len()
    );
    for (before, after) in initial
        .state
        .positions
        .iter()
        .zip(&final_state.state.positions)
    {
        assert_eq!(
            (
                &before.resource,
                &before.position,
                &before.kind,
                &before.amount,
                &before.scaled_receipt
            ),
            (
                &after.resource,
                &after.position,
                &after.kind,
                &after.amount,
                &after.scaled_receipt
            )
        );
        if let (Some(previous), Some(current)) = (&before.anchor, &after.anchor) {
            assert_eq!(previous.evidence["block"], json!(20));
            assert_eq!(current.evidence["block"], json!(23));
            assert_eq!(previous.evidence["block_hash"], json!(blockhash(20)));
            assert_eq!(current.evidence["block_hash"], json!(blockhash(23)));
        }
    }
    assert!(initial.complete && final_state.complete);
}
#[tokio::test]
async fn zero_other_revision_is_observed_without_authorizing_its_math_or_hiding_nonzero_claims() {
    let rpc = Arc::new(Fixture::new());
    *rpc.other_revision_reserve.lock().unwrap() = true;
    let a = adapter(rpc.clone());
    let snapshot = a.read_snapshot().await.unwrap();
    assert!(snapshot.complete);
    let other = snapshot
        .state
        .positions
        .iter()
        .filter(|p| {
            p.amount.asset.reference == lower_address(addr(102))
                && p.kind != PositionKind::StableDebt
        })
        .collect::<Vec<_>>();
    assert_eq!(other.len(), 2);
    for p in other {
        let anchor = p.anchor.as_ref().unwrap();
        assert!(anchor.principal.atoms.is_zero());
        assert_eq!(
            anchor.evidence["method"],
            json!("aave-zero-observed-backing")
        );
        let token = a.token(&lower_address(addr(102)), Some(20)).await.unwrap();
        let receipt = if p.kind == PositionKind::Supply {
            addr(111)
        } else {
            addr(121)
        };
        a.validate_anchor(anchor, &token, addr(1), receipt)
            .await
            .unwrap();
        let mut forged = anchor.clone();
        forged.principal.atoms = Quantity::from_u64(1);
        assert!(
            a.validate_anchor(&forged, &token, addr(1), receipt)
                .await
                .is_err()
        );
    }
    // Observation is not a reviewed-write claim, even when the source is empty.
    rpc.selected_revision.store(4, Ordering::SeqCst);
    assert_eq!(
        a.prepare("aave.supply", &aave_input("aave.supply"))
            .await
            .unwrap_err()
            .code,
        "ATOKEN_REVISION_UNSUPPORTED"
    );
    assert_eq!(
        a.prepare("aave.borrow", &aave_input("aave.borrow"))
            .await
            .unwrap_err()
            .code,
        "DEBT_REVISION_UNSUPPORTED"
    );
    rpc.selected_revision.store(5, Ordering::SeqCst);
    *rpc.other_revision_balance.lock().unwrap() = U256::from(1);
    assert_eq!(
        a.read_snapshot().await.unwrap_err().code,
        "TOKEN_REVISION_UNSUPPORTED"
    );
}
#[tokio::test]
async fn protocol_prerequisites_reject_paused_frozen_false_erc20_and_nonce_changes() {
    let rpc = Arc::new(Fixture::new());
    let a = adapter(rpc.clone());
    *rpc.paused.lock().unwrap() = true;
    assert_eq!(
        a.prepare("aave.supply", &aave_input("aave.supply"))
            .await
            .unwrap_err()
            .code,
        "RESERVE_UNAVAILABLE"
    );
    *rpc.paused.lock().unwrap() = false;
    *rpc.frozen.lock().unwrap() = true;
    assert_eq!(
        a.prepare("aave.supply", &aave_input("aave.supply"))
            .await
            .unwrap_err()
            .code,
        "RESERVE_FROZEN"
    );
    *rpc.frozen.lock().unwrap() = false;
    let mut v = input();
    v["token"] = json!(lower_address(addr(101)));
    v["amount"] = json!("1");
    v["recipient"] = json!(lower_address(addr(2)));
    *rpc.erc20_false.lock().unwrap() = true;
    assert_eq!(
        a.prepare("wallet.transfer", &v).await.unwrap_err().code,
        "ERC20_RETURN_FALSE"
    );
    *rpc.erc20_false.lock().unwrap() = false;
    let p = a.prepare("wallet.transfer", &v).await.unwrap();
    rpc.nonce.store(1, Ordering::SeqCst);
    assert_eq!(a.revalidate(&p).await.unwrap_err().code, "NONCE_CHANGED");
}
#[tokio::test]
async fn supply_proves_exact_mint_loss_and_rejects_forged_mint_then_recovers_same_hash() {
    let rpc = Arc::new(Fixture::new());
    let a = adapter(rpc.clone());
    let p = a
        .prepare("aave.supply", &aave_input("aave.supply"))
        .await
        .unwrap();
    let b: Binding = serde_json::from_value(p.binding.clone()).unwrap();
    let q = U256::from(1_000_000);
    let scaled = q - U256::from(1);
    let index = ray() + U256::from(1);
    rpc.post(
        21,
        AtBlock {
            scaled_supply: scaled,
            income: index,
            debt_index: index,
            supply_previous: index,
            collateral: true,
            ..Default::default()
        },
    );
    let logs = |mint_value| {
        vec![
            event(
                addr(101),
                Erc20::Transfer {
                    from: addr(1),
                    to: addr(110),
                    value: q,
                },
                21,
            ),
            event(
                addr(300),
                Pool::Supply {
                    reserve: addr(101),
                    user: addr(1),
                    onBehalfOf: addr(1),
                    amount: q,
                    referralCode: 0,
                },
                21,
            ),
            event(
                addr(110),
                AToken::Mint {
                    caller: addr(1),
                    onBehalfOf: addr(1),
                    value: mint_value,
                    balanceIncrease: U256::ZERO,
                    index,
                },
                21,
            ),
            event(
                addr(110),
                Erc20::Transfer {
                    from: Address::ZERO,
                    to: addr(1),
                    value: scaled,
                },
                21,
            ),
        ]
    };
    rpc.set_receipt(&b, 21, logs(q));
    let record = record(p);
    assert_eq!(
        a.reconcile(&record).await.unwrap_err().code,
        "SUPPLY_PROOF_MISMATCH"
    );
    rpc.set_receipt(&b, 21, logs(scaled));
    let done = a.reconcile(&record).await.unwrap();
    assert_eq!(done.status, Status::Completed);
    let proof = done.verified.unwrap();
    assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::Cost{amount,treatment:CostTreatment::IncludedInDebit,..} if amount.atoms==Quantity::from_u64(1))));
    assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::PositionChange{anchor:Some(anchor),..} if anchor.principal.atoms==Quantity::from_u64(999999))));
    assert_eq!(proof.submission.reference, txhash());
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .contains(&"eth_sendRawTransaction".into())
    );
}

#[tokio::test]
async fn bounded_all_requires_attached_exact_cap_and_freezes_complete_cash() {
    for op in [
        "wallet.unwrap",
        "uniswap.swap",
        "aave.withdraw",
        "aave.repay",
    ] {
        let d = operation_definitions()
            .into_iter()
            .find(|d| d.name == op)
            .unwrap();
        assert!(d.input_schema["properties"].get("max_amount").is_some());
    }
    let rpc = Arc::new(Fixture::new());
    let a = adapter(rpc.clone());
    let mut v = input();
    v["amount"] = json!("all");
    assert!(a.prepare("wallet.unwrap", &v).await.is_err());
    v["max_amount"] = json!("0.5");
    assert_eq!(
        a.prepare("wallet.unwrap", &v).await.unwrap_err().code,
        "SWEEP_MAXIMUM"
    );
    v["max_amount"] = json!("2");
    let p = a.prepare("wallet.unwrap", &v).await.unwrap();
    let b: Binding = serde_json::from_value(p.binding.clone()).unwrap();
    assert!(b.all_amount);
    assert_eq!(b.units, Quantity::from_decimal("1", 18).unwrap());
    *rpc.cash_override.lock().unwrap() = Some(pow10(18).unwrap() + U256::from(1));
    assert_eq!(a.revalidate(&p).await.unwrap_err().code, "BINDING_CHANGED");
    v["amount"] = json!("1");
    assert!(request::parse("wallet.unwrap", &v).is_err());
    v["amount"] = json!("all");
    v.as_object_mut().unwrap().remove("max_amount");
    assert!(request::parse("wallet.wrap", &v).is_err());
    v["max_amount"] = json!(U256::MAX.to_string());
    assert!(a.prepare("wallet.unwrap", &v).await.is_err());
    *rpc.cash_override.lock().unwrap() = Some(U256::from(3_000_000));
    let mut swap = input();
    swap["amount"] = json!("all");
    swap["max_amount"] = json!("10");
    swap["token_in"] = json!(lower_address(addr(101)));
    swap["token_out"] = json!(lower_address(addr(100)));
    swap["recipient"] = json!(lower_address(addr(1)));
    swap["min_output"] = json!("0.000000000003");
    swap["slippage_bps"] = json!(0);
    swap["deadline"] = json!(chrono::Utc::now().timestamp() as u64 + 120);
    let p = a.prepare("uniswap.swap", &swap).await.unwrap();
    let b: Binding = serde_json::from_value(p.binding.clone()).unwrap();
    assert_eq!(b.units, Quantity::from_u64(3_000_000));
    *rpc.cash_override.lock().unwrap() = Some(U256::from(3_000_001));
    assert_eq!(a.revalidate(&p).await.unwrap_err().code, "BINDING_CHANGED");
}
#[tokio::test]
async fn bounded_withdraw_all_requires_exact_complete_scaled_close_and_finite_credit() {
    let rpc = Arc::new(Fixture::new());
    let a = adapter(rpc.clone());
    let q = U256::from(999999);
    rpc.states
        .lock()
        .unwrap()
        .get_mut(&20)
        .unwrap()
        .scaled_supply = q;
    let mut v = aave_input("aave.withdraw");
    v["amount"] = json!("all");
    v["max_amount"] = json!("1.02");
    let p = a.prepare("aave.withdraw", &v).await.unwrap();
    let b: Binding = serde_json::from_value(p.binding.clone()).unwrap();
    let calldata = fin_evm::rpc_bytes(&json!(b.transaction.calldata)).unwrap();
    assert_eq!(
        Pool::withdrawCall::abi_decode_validate(&calldata)
            .unwrap()
            .amount,
        U256::MAX
    );
    let logs = vec![
        event(
            addr(300),
            Pool::Withdraw {
                reserve: addr(101),
                user: addr(1),
                to: addr(1),
                amount: q,
            },
            21,
        ),
        event(
            addr(101),
            Erc20::Transfer {
                from: addr(110),
                to: addr(1),
                value: q,
            },
            21,
        ),
    ];
    rpc.post(
        21,
        AtBlock {
            income: ray() + U256::from(1),
            debt_index: ray() + U256::from(1),
            scaled_supply: U256::from(1),
            ..Default::default()
        },
    );
    rpc.set_receipt(&b, 21, logs.clone());
    let record = record(p);
    assert!(a.reconcile(&record).await.is_err());
    rpc.states
        .lock()
        .unwrap()
        .get_mut(&21)
        .unwrap()
        .scaled_supply = U256::ZERO;
    let proof = a.reconcile(&record).await.unwrap().verified.unwrap();
    assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::PositionChange{anchor:Some(a),..} if a.principal.atoms.is_zero()&&a.scaled_receipt.as_ref().unwrap().is_zero())));
    rpc.receipt.lock().unwrap()["logs"] = json!([
        event(
            addr(300),
            Pool::Withdraw {
                reserve: addr(101),
                user: addr(1),
                to: addr(1),
                amount: U256::from(1_020_001)
            },
            21
        ),
        event(
            addr(101),
            Erc20::Transfer {
                from: addr(110),
                to: addr(1),
                value: U256::from(1_020_001)
            },
            21
        ),
    ]);
    assert_eq!(
        a.reconcile(&record).await.unwrap_err().code,
        "WITHDRAW_MAXIMUM"
    );
}
#[tokio::test]
async fn withdrawal_credits_exact_proved_yield_and_closes_scaled_claim() {
    let rpc = Arc::new(Fixture::new());
    let a = adapter(rpc.clone());
    let old = ray() + U256::from(1);
    let scaled = U256::from(999999);
    rpc.states.lock().unwrap().insert(
        20,
        AtBlock {
            scaled_supply: scaled,
            income: old,
            debt_index: old,
            supply_previous: old,
            collateral: true,
            ..Default::default()
        },
    );
    let token = a.token(&lower_address(addr(101)), Some(20)).await.unwrap();
    let anchor = a
        .make_anchor(
            &token,
            addr(1),
            addr(110),
            PositionKind::Supply,
            20,
            U256::ZERO,
        )
        .await
        .unwrap();
    let index = ray() + ray() / U256::from(1000);
    rpc.states.lock().unwrap().insert(
        22,
        AtBlock {
            scaled_supply: scaled,
            income: index,
            debt_index: old,
            supply_previous: old,
            collateral: true,
            ..Default::default()
        },
    );
    rpc.latest.store(22, Ordering::SeqCst);
    let mut v = aave_input("aave.withdraw");
    v["amount"] = json!("1.000998");
    let p = a
        .prepare_with_context(
            "aave.withdraw",
            &v,
            &PreparationContext {
                positions: vec![anchor],
            },
        )
        .await
        .unwrap();
    let b: Binding = serde_json::from_value(p.binding.clone()).unwrap();
    let q = U256::from(1_000_998);
    rpc.post(
        23,
        AtBlock {
            income: index,
            debt_index: old,
            supply_previous: index,
            ..Default::default()
        },
    );
    rpc.set_receipt(
        &b,
        23,
        vec![
            event(
                addr(300),
                Pool::Withdraw {
                    reserve: addr(101),
                    user: addr(1),
                    to: addr(1),
                    amount: q,
                },
                23,
            ),
            event(
                addr(101),
                Erc20::Transfer {
                    from: addr(110),
                    to: addr(1),
                    value: q,
                },
                23,
            ),
        ],
    );
    let proof = a.reconcile(&record(p)).await.unwrap().verified.unwrap();
    assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::Accrual{position_kind:PositionKind::Supply,delta,..} if delta.as_str()=="999")));
    assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::PositionChange{anchor:Some(anchor),..} if anchor.principal.atoms.is_zero()&&anchor.scaled_receipt.as_ref().unwrap().is_zero())));
    assert!(proof.effects.iter().any(
        |e| matches!(e,FinancialEffect::UsableCredit{amount,..} if amount.atoms==quantity(q))
    ));
}
#[tokio::test]
async fn borrow_ceil_rounding_is_deferred_and_repay_proves_interest_once() {
    let rpc = Arc::new(Fixture::new());
    let a = adapter(rpc.clone());
    let p = a
        .prepare("aave.borrow", &aave_input("aave.borrow"))
        .await
        .unwrap();
    let b: Binding = serde_json::from_value(p.binding.clone()).unwrap();
    let q = U256::from(1_000_000);
    let index = ray() + U256::from(1);
    rpc.post(
        21,
        AtBlock {
            scaled_debt: q,
            income: index,
            debt_index: index,
            debt_previous: index,
            ..Default::default()
        },
    );
    rpc.set_receipt(
        &b,
        21,
        vec![
            event(
                addr(300),
                Pool::Borrow {
                    reserve: addr(101),
                    user: addr(1),
                    onBehalfOf: addr(1),
                    amount: q,
                    interestRateMode: 2,
                    borrowRate: U256::from(1),
                    referralCode: 0,
                },
                21,
            ),
            event(
                addr(101),
                Erc20::Transfer {
                    from: addr(110),
                    to: addr(1),
                    value: q,
                },
                21,
            ),
            event(
                addr(120),
                AToken::Mint {
                    caller: addr(1),
                    onBehalfOf: addr(1),
                    value: q + U256::from(1),
                    balanceIncrease: U256::ZERO,
                    index,
                },
                21,
            ),
            event(
                addr(120),
                Erc20::Transfer {
                    from: Address::ZERO,
                    to: addr(1),
                    value: q + U256::from(1),
                },
                21,
            ),
        ],
    );
    let proof = a.reconcile(&record(p)).await.unwrap().verified.unwrap();
    assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::Accrual{position_kind:PositionKind::VariableDebt,delta,..} if delta.is_zero())));
    assert!(
        proof.effects.iter().any(
            |e| matches!(e,FinancialEffect::DebtChange{delta,..} if delta.as_str()=="1000001")
        )
    );
    assert!(!proof.effects.iter().any(|e| matches!(
        e,
        FinancialEffect::Cost {
            treatment: CostTreatment::IncludedInDebit,
            ..
        }
    )));
    let anchor = proof
        .effects
        .into_iter()
        .find_map(|e| {
            if let FinancialEffect::DebtChange { anchor, .. } = e {
                anchor
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(anchor.principal.atoms, quantity(q + U256::from(1)));
    let new_index = ray() + ray() / U256::from(1000);
    rpc.states.lock().unwrap().insert(
        23,
        AtBlock {
            scaled_debt: q,
            income: index,
            debt_index: new_index,
            debt_previous: index,
            ..Default::default()
        },
    );
    rpc.latest.store(23, Ordering::SeqCst);
    *rpc.allowance.lock().unwrap() = U256::from(1_010_000);
    let mut v = aave_input("aave.repay");
    v["amount"] = json!("all");
    v["max_amount"] = json!("1.01");
    let p = a
        .prepare_with_context(
            "aave.repay",
            &v,
            &PreparationContext {
                positions: vec![anchor],
            },
        )
        .await
        .unwrap();
    assert!(p.intent.expected.iter().any(|expected| matches!(
        expected,
        EffectConstraint::Accrual {
            position_kind: PositionKind::VariableDebt,
            maximum_change,
            ..
        } if maximum_change == &Quantity::from_u64(1_010_000)
    )));
    let b: Binding = serde_json::from_value(p.binding.clone()).unwrap();
    let paid = U256::from(1_001_000);
    rpc.post(
        24,
        AtBlock {
            income: index,
            debt_index: new_index,
            debt_previous: new_index,
            ..Default::default()
        },
    );
    rpc.set_receipt(
        &b,
        24,
        vec![
            event(
                addr(300),
                Pool::Repay {
                    reserve: addr(101),
                    user: addr(1),
                    repayer: addr(1),
                    amount: paid,
                    useATokens: false,
                },
                24,
            ),
            event(
                addr(101),
                Erc20::Transfer {
                    from: addr(1),
                    to: addr(110),
                    value: paid,
                },
                24,
            ),
        ],
    );
    let proof = a.reconcile(&record(p)).await.unwrap().verified.unwrap();
    assert!(
        proof
            .effects
            .iter()
            .any(|e| matches!(e,FinancialEffect::Accrual{delta,..} if delta.as_str()=="999"))
    );
    assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::Cost{amount,treatment:CostTreatment::IncludedInDebit,..} if amount.atoms==Quantity::from_u64(1000))));
    assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::DebtChange{anchor:Some(anchor),delta,..} if anchor.principal.atoms.is_zero()&&delta.as_str()=="-1001000")));
}
#[tokio::test]
async fn wrong_anchor_resource_or_untracked_scaled_change_is_rejected_before_submission() {
    let rpc = Arc::new(Fixture::new());
    let a = adapter(rpc.clone());
    let token = a.token(&lower_address(addr(101)), Some(20)).await.unwrap();
    let mut anchor = a
        .make_anchor(
            &token,
            addr(1),
            addr(110),
            PositionKind::Supply,
            20,
            U256::ZERO,
        )
        .await
        .unwrap();
    anchor.principal.atoms = Quantity::from_u64(1);
    assert_eq!(
        a.prepare_with_context(
            "aave.supply",
            &aave_input("aave.supply"),
            &PreparationContext {
                positions: vec![anchor]
            }
        )
        .await
        .unwrap_err()
        .code,
        "ANCHOR_PROOF_MISMATCH"
    );
    let anchor = a
        .make_anchor(
            &token,
            addr(1),
            addr(110),
            PositionKind::Supply,
            20,
            U256::ZERO,
        )
        .await
        .unwrap();
    rpc.states.lock().unwrap().insert(
        21,
        AtBlock {
            scaled_supply: U256::from(1),
            income: ray(),
            debt_index: ray(),
            supply_previous: ray(),
            ..Default::default()
        },
    );
    rpc.latest.store(21, Ordering::SeqCst);
    assert_eq!(
        a.prepare_with_context(
            "aave.supply",
            &aave_input("aave.supply"),
            &PreparationContext {
                positions: vec![anchor]
            }
        )
        .await
        .unwrap_err()
        .code,
        "POSITION_PRINCIPAL_CHANGED"
    );
}

fn apply_test_plan(context: &mut ReturnRouteContext, mut proof: ReturnRouteProof) {
    // Mirror the broker's one-time cost projection: merge principal cash legs,
    // then widen their lower bound by AdditionalDebit fees. IncludedInDebit
    // costs already belong to those principal legs and never debit cash twice.
    let mut deltas = std::collections::BTreeMap::new();
    for effect in &proof.effects {
        if let ProjectedEffect::CashDelta {
            resource,
            asset,
            minimum_delta,
            maximum_delta,
        } = effect
        {
            let entry = deltas
                .entry((resource.clone(), asset.clone()))
                .or_insert_with(|| {
                    (
                        SignedQuantity::new("0").unwrap().integer(),
                        SignedQuantity::new("0").unwrap().integer(),
                    )
                });
            entry.0 += minimum_delta.integer();
            entry.1 += maximum_delta.integer();
        }
    }
    for cost in &proof.intent.maximum_costs {
        if cost.treatment != CostTreatment::AdditionalDebit {
            continue;
        }
        let AccountingLocation::Cash { resource } = &cost.location else {
            panic!("AdditionalDebit must have an exact cash source");
        };
        let entry = deltas
            .entry((resource.clone(), cost.amount.asset.clone()))
            .or_insert_with(|| {
                (
                    SignedQuantity::new("0").unwrap().integer(),
                    SignedQuantity::new("0").unwrap().integer(),
                )
            });
        entry.0 += SignedQuantity::negative(&cost.amount.atoms).integer();
    }
    proof
        .effects
        .retain(|effect| !matches!(effect, ProjectedEffect::CashDelta { .. }));
    proof.effects.extend(
        deltas
            .into_iter()
            .map(
                |((resource, asset), (minimum, maximum))| ProjectedEffect::CashDelta {
                    resource,
                    asset,
                    minimum_delta: SignedQuantity::new(minimum.to_string()).unwrap(),
                    maximum_delta: SignedQuantity::new(maximum.to_string()).unwrap(),
                },
            ),
    );
    for effect in &proof.effects {
        match effect {
            ProjectedEffect::CashDelta {
                resource,
                asset,
                minimum_delta,
                maximum_delta,
            } => {
                let row_index = context
                    .projected
                    .cash
                    .iter()
                    .position(|p| p.resource == *resource && p.asset == *asset)
                    .unwrap_or_else(|| {
                        context.projected.cash.push(ProjectedCash {
                            resource: resource.clone(),
                            asset: asset.clone(),
                            amount: QuantityInterval {
                                minimum: Quantity::zero(),
                                maximum: Quantity::zero(),
                            },
                        });
                        context.projected.cash.len() - 1
                    });
                let row = &mut context.projected.cash[row_index];
                let apply = |q: &Quantity, d: &SignedQuantity| {
                    if d.is_negative() {
                        q.checked_sub(&d.magnitude()).unwrap()
                    } else {
                        q.checked_add(&d.magnitude()).unwrap()
                    }
                };
                row.amount.minimum = apply(&row.amount.minimum, minimum_delta);
                row.amount.maximum = apply(&row.amount.maximum, maximum_delta);
            }
            ProjectedEffect::PositionDelta {
                resource,
                position,
                asset,
                position_kind,
                minimum_delta,
                maximum_delta,
            } => {
                let row = context
                    .projected
                    .positions
                    .iter_mut()
                    .find(|p| {
                        p.resource == *resource
                            && p.position == *position
                            && p.asset == *asset
                            && p.position_kind == *position_kind
                    })
                    .unwrap();
                let apply = |q: &Quantity, d: &SignedQuantity| {
                    if d.is_negative() {
                        q.checked_sub(&d.magnitude()).unwrap()
                    } else {
                        q.checked_add(&d.magnitude()).unwrap()
                    }
                };
                row.amount.minimum = apply(&row.amount.minimum, minimum_delta);
                row.amount.maximum = apply(&row.amount.maximum, maximum_delta);
            }
            ProjectedEffect::AuthorityValue {
                resource,
                authority,
                value,
            } => {
                context
                    .projected
                    .authorities
                    .iter_mut()
                    .find(|a| a.resource == *resource && a.authority == *authority)
                    .unwrap()
                    .value = value.clone();
            }
            ProjectedEffect::CashSweep {
                resource, asset, ..
            } => {
                context
                    .projected
                    .cash
                    .iter_mut()
                    .find(|p| p.resource == *resource && p.asset == *asset)
                    .unwrap()
                    .amount = QuantityInterval {
                    minimum: Quantity::zero(),
                    maximum: Quantity::zero(),
                };
            }
            ProjectedEffect::PositionClose {
                resource,
                position,
                position_kind,
                asset,
                ..
            } => {
                context
                    .projected
                    .positions
                    .iter_mut()
                    .find(|p| {
                        p.resource == *resource
                            && p.position == *position
                            && p.position_kind == *position_kind
                            && p.asset == *asset
                    })
                    .unwrap()
                    .amount = QuantityInterval {
                    minimum: Quantity::zero(),
                    maximum: Quantity::zero(),
                };
            }
        }
    }
    context.prior_proofs.push(proof);
    context.step_index += 1;
}
#[tokio::test]
async fn conditional_routes_prove_dependencies_without_simulation_or_unselected_cash() {
    let rpc = Arc::new(Fixture::new());
    *rpc.cash_override.lock().unwrap() = Some(U256::ZERO);
    rpc.other_stable.store(0, Ordering::SeqCst);
    *rpc.include_weth.lock().unwrap() = true;
    let mut a = adapter(rpc.clone());
    a.config.snapshot_tokens.push(lower_address(addr(100)));
    let baseline = a.read_snapshot().await.unwrap();
    let mut context = ReturnRouteContext {
        projected: ProjectedState {
            cash: baseline
                .state
                .holdings
                .iter()
                .map(|p| ProjectedCash {
                    resource: p.resource.clone(),
                    asset: p.amount.asset.clone(),
                    amount: QuantityInterval {
                        minimum: p.amount.atoms.clone(),
                        maximum: p.amount.atoms.clone(),
                    },
                })
                .collect(),
            positions: baseline
                .state
                .positions
                .iter()
                .map(|p| ProjectedPosition {
                    resource: p.resource.clone(),
                    position: p.position.clone(),
                    asset: p.amount.asset.clone(),
                    position_kind: p.kind.clone(),
                    amount: QuantityInterval {
                        minimum: p.amount.atoms.clone(),
                        maximum: p.amount.atoms.clone(),
                    },
                })
                .collect(),
            authorities: baseline.state.authorities.clone(),
        },
        allocations: vec![ResourceAmount {
            resource: resource(1, addr(1)),
            amount: amount(&a.native_asset(), exact("0.02", 18).unwrap()),
        }],
        baseline,
        step_index: 0,
        prior_proofs: vec![],
        remaining_cost_budget_usd_micros: Quantity::from_u64(30_000_000),
        asset_prices: vec![],
    };
    let mk = |extra: Value| {
        let mut v = input();
        v["max_fee_eth"] = json!("0.0001");
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        v
    };
    let weth = lower_address(addr(100));
    let usdc = lower_address(addr(101));
    let wrap = mk(json!({"amount":"0.01"}));
    let mut insufficient_fee = wrap.clone();
    insufficient_fee["max_fee_eth"] = json!("0.00000000000001");
    assert_eq!(
        a.preflight_return_route("wallet.wrap", &insufficient_fee, &context)
            .await
            .unwrap_err()
            .code,
        "PLAN_FEE_TOO_LOW"
    );
    let mut unselected = context.clone();
    unselected.allocations[0].amount.atoms = Quantity::from_decimal("0.001", 18).unwrap();
    assert!(
        a.preflight_return_route("wallet.wrap", &wrap, &unselected)
            .await
            .is_err()
    );
    let proof = a
        .preflight_return_route("wallet.wrap", &wrap, &context)
        .await
        .unwrap();
    assert_eq!(proof.input_hash, binding_hash(&wrap).unwrap());
    assert_eq!(
        proof.evidence["method"],
        json!("conditional-ethereum-route-v1")
    );
    assert!(
        proof.evidence["unmet_execution_dependencies"]
            .as_str()
            .unwrap()
            .contains("not current holdings")
    );
    assert!(proof.effects.iter().any(|effect| matches!(effect,
        ProjectedEffect::CashDelta {asset,minimum_delta,maximum_delta,..}
            if asset.reference == "native"
                && minimum_delta.as_str() == "-10000000000000000"
                && minimum_delta == maximum_delta
    )));
    apply_test_plan(&mut context, proof);
    let native_projection = context
        .projected
        .cash
        .iter()
        .find(|row| row.asset.reference == "native")
        .unwrap();
    let initial_native = context
        .baseline
        .state
        .holdings
        .iter()
        .find(|row| row.amount.asset.reference == "native")
        .unwrap();
    assert_eq!(
        initial_native
            .amount
            .atoms
            .checked_sub(&native_projection.amount.minimum)
            .unwrap(),
        Quantity::from_decimal("0.0101", 18).unwrap()
    );
    assert_eq!(
        initial_native
            .amount
            .atoms
            .checked_sub(&native_projection.amount.maximum)
            .unwrap(),
        Quantity::from_decimal("0.01", 18).unwrap()
    );
    let q = "0.003333333333333333";
    let swap = mk(
        json!({"token_in":weth,"token_out":usdc,"amount":q,"min_output":"2.985","slippage_bps":50,"deadline":chrono::Utc::now().timestamp() as u64+120,"recipient":lower_address(addr(1))}),
    );
    assert_eq!(
        a.preflight_return_route("uniswap.swap", &swap, &context)
            .await
            .unwrap_err()
            .code,
        "PLAN_ALLOWANCE_UNFUNDED"
    );
    let approve =
        mk(json!({"token":weth,"spender":a.config.uniswap.as_ref().unwrap().router,"amount":q}));
    let proof = a
        .preflight_return_route("wallet.approve", &approve, &context)
        .await
        .unwrap();
    apply_test_plan(&mut context, proof);
    let proof = a
        .preflight_return_route("uniswap.swap", &swap, &context)
        .await
        .unwrap();
    assert!(
        proof.evidence["output_cap_semantics"]
            .as_str()
            .unwrap()
            .contains("NOT an enforced")
    );
    apply_test_plan(&mut context, proof);
    let supply_q = "0.006666666666666667";
    let approve =
        mk(json!({"token":weth,"spender":a.config.aave.as_ref().unwrap().pool,"amount":supply_q}));
    let proof = a
        .preflight_return_route("wallet.approve", &approve, &context)
        .await
        .unwrap();
    apply_test_plan(&mut context, proof);
    let supply = mk(json!({"token":weth,"amount":supply_q}));
    rpc.selected_revision.store(4, Ordering::SeqCst);
    assert_eq!(
        a.preflight_return_route("aave.supply", &supply, &context)
            .await
            .unwrap_err()
            .code,
        "TOKEN_REVISION_UNSUPPORTED"
    );
    rpc.selected_revision.store(5, Ordering::SeqCst);
    let proof = a
        .preflight_return_route("aave.supply", &supply, &context)
        .await
        .unwrap();
    apply_test_plan(&mut context, proof);
    let borrow = mk(json!({"token":usdc,"amount":"1","min_health_factor":"1.5"}));
    let proof = a
        .preflight_return_route("aave.borrow", &borrow, &context)
        .await
        .unwrap();
    apply_test_plan(&mut context, proof);
    let withdraw = mk(
        json!({"token":weth,"amount":"all","max_amount":"0.0074","recipient":lower_address(addr(1))}),
    );
    assert_eq!(
        a.preflight_return_route("aave.withdraw", &withdraw, &context)
            .await
            .unwrap_err()
            .code,
        "PLAN_DEBT_UNCLOSED"
    );
    let approve_repay =
        mk(json!({"token":usdc,"spender":a.config.aave.as_ref().unwrap().pool,"amount":"1.02"}));
    let proof = a
        .preflight_return_route("wallet.approve", &approve_repay, &context)
        .await
        .unwrap();
    apply_test_plan(&mut context, proof);
    let too_small_repay = mk(json!({"token":usdc,"amount":"all","max_amount":"1"}));
    assert_eq!(
        a.preflight_return_route("aave.repay", &too_small_repay, &context)
            .await
            .unwrap_err()
            .code,
        "PLAN_CLOSE_UNBOUNDED"
    );
    let repay = mk(json!({"token":usdc,"amount":"all","max_amount":"1.02"}));
    let proof = a
        .preflight_return_route("aave.repay", &repay, &context)
        .await
        .unwrap();
    let finite_repay_cap = Quantity::from_u64(1_020_000);
    assert!(proof.intent.expected.iter().any(|expected| matches!(
        expected,
        EffectConstraint::Accrual {
            position_kind: PositionKind::VariableDebt,
            maximum_change,
            ..
        } if maximum_change == &finite_repay_cap
    )));
    assert!(proof.intent.maximum_costs.iter().any(|cost| {
        cost.amount.asset.reference == usdc
            && cost.amount.atoms == finite_repay_cap
            && cost.treatment == CostTreatment::IncludedInDebit
    }));

    // Fresh execution preview after index accrual has a larger physical debt
    // than run.begin observed, but the repay-all accrual envelope remains the
    // exact finite close cap shared by the stored route proof.
    let original_state = rpc.states.lock().unwrap().get(&20).cloned().unwrap();
    let original_allowance = *rpc.allowance.lock().unwrap();
    let original_cash = *rpc.cash_override.lock().unwrap();
    *rpc.allowance.lock().unwrap() = U256::from(1_020_000);
    *rpc.cash_override.lock().unwrap() = Some(U256::from(1_020_000));
    rpc.states.lock().unwrap().insert(
        20,
        AtBlock {
            scaled_debt: U256::from(1_000_000),
            income: ray(),
            debt_index: ray() + ray() / U256::from(100),
            debt_previous: ray(),
            ..Default::default()
        },
    );
    let accrued_snapshot = a.read_snapshot().await.unwrap();
    let accrued_context = PreparationContext {
        positions: accrued_snapshot
            .state
            .positions
            .into_iter()
            .filter_map(|position| position.anchor)
            .collect(),
    };
    let accrued_preview = a
        .prepare_with_context("aave.repay", &repay, &accrued_context)
        .await
        .unwrap();
    assert!(
        accrued_preview
            .intent
            .expected
            .iter()
            .any(|expected| matches!(
                expected,
                EffectConstraint::Accrual {
                    position_kind: PositionKind::VariableDebt,
                    maximum_change,
                    ..
                } if maximum_change == &finite_repay_cap
            ))
    );
    assert!(accrued_preview.intent.maximum_costs.iter().any(|cost| {
        cost.amount.asset.reference == usdc
            && cost.amount.atoms == finite_repay_cap
            && cost.treatment == CostTreatment::IncludedInDebit
    }));

    // Partial repayment retains its distinct envelope: the installment may
    // be smaller than the accrued debt left outstanding after it.
    let partial_repay = mk(json!({"token":usdc,"amount":"0.5"}));
    *rpc.simulation_override.lock().unwrap() = Some(abi_value(U256::from(500_000)));
    let partial_preview = a
        .prepare_with_context("aave.repay", &partial_repay, &accrued_context)
        .await
        .unwrap();
    assert!(
        partial_preview
            .intent
            .expected
            .iter()
            .any(|expected| matches!(
                expected,
                EffectConstraint::Accrual {
                    position_kind: PositionKind::VariableDebt,
                    maximum_change,
                    ..
                } if maximum_change == &Quantity::from_u64(1_510_000)
            ))
    );
    *rpc.simulation_override.lock().unwrap() = None;

    // The ordinary simulation remains capped. Debt above the cap cannot have
    // a sufficient finite allowance that is itself within the same cap, so
    // the fresh preview rejects it at the allowance check before authorization.
    *rpc.simulation_override.lock().unwrap() = Some(abi_value(U256::from(1_021_000)));
    assert_eq!(
        a.prepare_with_context("aave.repay", &repay, &accrued_context)
            .await
            .unwrap_err()
            .code,
        "REPAY_SIMULATION_LIMIT"
    );
    *rpc.simulation_override.lock().unwrap() = None;
    rpc.states.lock().unwrap().insert(
        20,
        AtBlock {
            scaled_debt: U256::from(1_000_000),
            income: ray(),
            debt_index: ray() + ray() / U256::from(10),
            debt_previous: ray(),
            ..Default::default()
        },
    );
    let over_cap_snapshot = a.read_snapshot().await.unwrap();
    let over_cap_context = PreparationContext {
        positions: over_cap_snapshot
            .state
            .positions
            .into_iter()
            .filter_map(|position| position.anchor)
            .collect(),
    };
    assert_eq!(
        a.prepare_with_context("aave.repay", &repay, &over_cap_context)
            .await
            .unwrap_err()
            .code,
        "ALLOWANCE_INSUFFICIENT"
    );
    rpc.states.lock().unwrap().insert(20, original_state);
    *rpc.allowance.lock().unwrap() = original_allowance;
    *rpc.cash_override.lock().unwrap() = original_cash;
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .contains(&"eth_sendRawTransaction".into())
    );

    assert!(proof.effects.iter().any(|effect| matches!(effect, ProjectedEffect::PositionClose{position_kind:PositionKind::VariableDebt,maximum_amount,source_step,..} if maximum_amount == &Quantity::from_u64(1_020_000) && *source_step < proof.step_index)));
    // The latest one-USDC borrowing credit alone is smaller than repayment.
    // Its guaranteed source chain also contains earlier swap proceeds.
    assert!(
        proof
            .requires
            .iter()
            .any(|requirement| matches!(requirement,
                ProjectedRequirement::CashAtLeast {asset,amount,source_step:Some(5),..}
                    if asset.reference == usdc && amount == &Quantity::from_u64(1_020_000)
            ))
    );
    apply_test_plan(&mut context, proof);
    assert!(
        context
            .projected
            .positions
            .iter()
            .filter(|p| p.position_kind != PositionKind::Supply)
            .all(|p| p.amount.maximum.is_zero())
    );
    let mut too_small_withdraw = withdraw.clone();
    too_small_withdraw["max_amount"] = json!("0.001");
    assert_eq!(
        a.preflight_return_route("aave.withdraw", &too_small_withdraw, &context)
            .await
            .unwrap_err()
            .code,
        "PLAN_CLOSE_UNBOUNDED"
    );
    let proof = a
        .preflight_return_route("aave.withdraw", &withdraw, &context)
        .await
        .unwrap();
    let withdraw_cap = quantity(exact("0.0074", 18).unwrap());
    assert!(proof.intent.expected.iter().any(|expected| matches!(
        expected,
        EffectConstraint::Accrual {
            position_kind: PositionKind::Supply,
            maximum_change,
            ..
        } if maximum_change == &withdraw_cap
    )));

    // A fresh full-close preview sees the physical supply after index yield;
    // its Accrual envelope remains the finite max_amount from the stored route.
    let original_state = rpc.states.lock().unwrap().get(&20).cloned().unwrap();
    rpc.states.lock().unwrap().insert(
        20,
        AtBlock {
            scaled_supply: exact("0.006666666666666667", 18).unwrap(),
            income: ray() + ray() / U256::from(100),
            debt_index: ray() + U256::from(1),
            supply_previous: ray(),
            debt_previous: ray(),
            ..Default::default()
        },
    );
    let yielded_snapshot = a.read_snapshot().await.unwrap();
    let yielded_context = PreparationContext {
        positions: yielded_snapshot
            .state
            .positions
            .into_iter()
            .filter_map(|position| position.anchor)
            .collect(),
    };
    let yielded_preview = a
        .prepare_with_context("aave.withdraw", &withdraw, &yielded_context)
        .await
        .unwrap();
    let fresh_withdraw_accrual = yielded_preview.intent.expected.iter().find_map(|expected| {
        if let EffectConstraint::Accrual {
            position_kind: PositionKind::Supply,
            maximum_change,
            ..
        } = expected
        {
            Some(maximum_change)
        } else {
            None
        }
    });
    assert_eq!(fresh_withdraw_accrual, Some(&withdraw_cap));

    // Partial withdrawals retain the observed-claim envelope because they do
    // not close the entire position under the finite max_amount.
    let partial_withdraw = mk(json!({
        "token":weth,
        "amount":"0.001",
        "recipient":lower_address(addr(1))
    }));
    let partial_preview = a
        .prepare_with_context("aave.withdraw", &partial_withdraw, &yielded_context)
        .await
        .unwrap();
    let yielded_binding: Binding = serde_json::from_value(yielded_preview.binding.clone()).unwrap();
    let expected_partial_accrual = quantity(
        exact("0.001", 18).unwrap()
            + fin_evm::u256(yielded_binding.supplied_before.as_ref().unwrap()).unwrap(),
    );
    assert!(
        partial_preview
            .intent
            .expected
            .iter()
            .any(|expected| matches!(
                expected,
                EffectConstraint::Accrual {
                    position_kind: PositionKind::Supply,
                    maximum_change,
                    ..
                } if maximum_change == &expected_partial_accrual
            ))
    );

    // A physical claim above the same finite cap is rejected before a write;
    // growth never expands the route or its receipt bounds.
    rpc.states.lock().unwrap().insert(
        20,
        AtBlock {
            scaled_supply: exact("0.006666666666666667", 18).unwrap(),
            income: ray() + ray() / U256::from(2),
            debt_index: ray() + U256::from(1),
            supply_previous: ray(),
            debt_previous: ray(),
            ..Default::default()
        },
    );
    let over_cap_snapshot = a.read_snapshot().await.unwrap();
    let over_cap_context = PreparationContext {
        positions: over_cap_snapshot
            .state
            .positions
            .into_iter()
            .filter_map(|position| position.anchor)
            .collect(),
    };
    assert_eq!(
        a.prepare_with_context("aave.withdraw", &withdraw, &over_cap_context)
            .await
            .unwrap_err()
            .code,
        "WITHDRAW_LIMIT"
    );
    rpc.states.lock().unwrap().insert(20, original_state);
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .contains(&"eth_sendRawTransaction".into())
    );
    assert!(proof.effects.iter().any(|effect| matches!(effect, ProjectedEffect::PositionClose{position_kind:PositionKind::Supply,source_step,..} if *source_step < proof.step_index)));
    apply_test_plan(&mut context, proof);
    assert!(
        context
            .projected
            .positions
            .iter()
            .all(|p| p.amount.maximum.is_zero())
    );
    let approve_reverse =
        mk(json!({"token":usdc,"spender":a.config.uniswap.as_ref().unwrap().router,"amount":"11"}));
    let proof = a
        .preflight_return_route("wallet.approve", &approve_reverse, &context)
        .await
        .unwrap();
    apply_test_plan(&mut context, proof);
    let reverse = mk(
        json!({"token_in":usdc,"token_out":weth,"amount":"all","max_amount":"11","min_output":"0.000000000002985","slippage_bps":50,"deadline":chrono::Utc::now().timestamp() as u64+120,"recipient":lower_address(addr(1))}),
    );
    let proof = a
        .preflight_return_route("uniswap.swap", &reverse, &context)
        .await
        .unwrap();
    let projected_usdc = context
        .projected
        .cash
        .iter()
        .find(|row| row.asset.reference == usdc)
        .unwrap();
    assert!(proof.effects.iter().any(
        |effect| matches!(effect,ProjectedEffect::CashSweep{asset,maximum_amount,..}
            if asset.reference == usdc && maximum_amount == &projected_usdc.amount.maximum)
    ));
    apply_test_plan(&mut context, proof);
    assert!(
        context
            .projected
            .cash
            .iter()
            .find(|p| p.asset.reference == usdc)
            .unwrap()
            .amount
            .maximum
            .is_zero()
    );
    let unwrap = mk(json!({"amount":"all","max_amount":"0.02"}));
    let proof = a
        .preflight_return_route("wallet.unwrap", &unwrap, &context)
        .await
        .unwrap();
    let projected_weth = context
        .projected
        .cash
        .iter()
        .find(|row| row.asset.reference == weth)
        .unwrap();
    assert!(proof.effects.iter().any(|effect| matches!(effect,
        ProjectedEffect::CashSweep {asset,maximum_amount,..}
            if asset.reference == weth && maximum_amount == &projected_weth.amount.maximum
    )));
    apply_test_plan(&mut context, proof);
    assert!(
        context
            .projected
            .cash
            .iter()
            .find(|p| p.asset.reference == weth)
            .unwrap()
            .amount
            .maximum
            .is_zero()
    );
    // No signed transaction is broadcast by route proofs or ordinary previews.
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "eth_sendRawTransaction")
    );
}

struct FixedOracleClock(u64);
#[async_trait]
impl OracleObservationClock for FixedOracleClock {
    async fn unix_seconds(&self) -> Result<u64> {
        Ok(self.0)
    }
}
#[tokio::test]
async fn oracle_observation_clock_keeps_feed_age_strict_and_cannot_extend_write_freshness() {
    let rpc = Arc::new(Fixture::new());
    let historical = chrono::Utc::now().timestamp() as u64 - 172_800;
    *rpc.feed_updated_at.lock().unwrap() = Some(historical);
    let units = Quantity::from_u64(1_000_000);
    // Production composition uses system UTC: historical data remains stale.
    assert_eq!(
        adapter(rpc.clone())
            .quote_usd(&lower_address(addr(101)), units.clone())
            .await
            .unwrap_err()
            .code,
        "ORACLE_STALE"
    );
    let future =
        adapter(rpc.clone()).with_isolated_oracle_clock(Arc::new(FixedOracleClock(historical - 1)));
    assert_eq!(
        future
            .quote_usd(&lower_address(addr(101)), units.clone())
            .await
            .unwrap_err()
            .code,
        "ORACLE_STALE"
    );
    let stale = adapter(rpc.clone())
        .with_isolated_oracle_clock(Arc::new(FixedOracleClock(historical + 7_201)));
    assert_eq!(
        stale
            .quote_usd("native", Quantity::from_u64(1))
            .await
            .unwrap_err()
            .code,
        "ORACLE_STALE"
    );
    let boundary = adapter(rpc.clone())
        .with_isolated_oracle_clock(Arc::new(FixedOracleClock(historical + 7_200)));
    assert!(
        boundary
            .quote_usd("native", Quantity::from_u64(1))
            .await
            .is_ok()
    );
    let a = adapter(rpc.clone()).with_isolated_oracle_clock(Arc::new(FixedOracleClock(historical)));
    assert_eq!(
        a.quote_usd(&lower_address(addr(101)), units.clone())
            .await
            .unwrap()
            .0,
        Quantity::from_u64(1_000_000)
    );
    let before = chrono::Utc::now();
    let mut wrap = input();
    wrap["amount"] = json!("0.001");
    let prepared = a.prepare("wallet.wrap", &wrap).await.unwrap();
    let observed = chrono::DateTime::parse_from_rfc3339(&prepared.observed_at).unwrap();
    assert!(observed.timestamp() >= before.timestamp());
    assert!(
        chrono::DateTime::parse_from_rfc3339(&prepared.expires_at)
            .unwrap()
            .timestamp()
            > before.timestamp()
    );
    let mut swap = input();
    swap.as_object_mut().unwrap().extend(json!({"token_in":lower_address(addr(100)),"token_out":lower_address(addr(101)),"amount":"0.000001","min_output":"2.99","slippage_bps":100,"deadline":historical+120,"recipient":lower_address(addr(1))}).as_object().unwrap().clone());
    // Deadline validation continues to use system UTC, not the historical oracle clock.
    assert_eq!(
        a.prepare("uniswap.swap", &swap).await.unwrap_err().code,
        "DEADLINE_INVALID"
    );
}

#[tokio::test]
async fn tracked_wallet_transfer_routes_restore_both_sources_and_require_original_return_gas() {
    let rpc = Arc::new(Fixture::new());
    rpc.other_stable.store(0, Ordering::SeqCst);
    *rpc.cash_override.lock().unwrap() = Some(U256::from(1_000_000));
    rpc.cash_by_owner
        .lock()
        .unwrap()
        .insert(addr(2), U256::ZERO);
    let mut a = adapter(rpc.clone());
    a.wallets.push(WalletConnection {
        name: "return-wallet".into(),
        address: lower_address(addr(2)),
        signing_enabled: true,
    });
    let baseline = a.read_snapshot().await.unwrap();
    let owner = resource(1, addr(1));
    let destination = resource(1, addr(2));
    let native = a.native_asset();
    let usdc = a.asset(&lower_address(addr(101)), 6);
    let mut context = ReturnRouteContext {
        projected: ProjectedState {
            cash: baseline
                .state
                .holdings
                .iter()
                .map(|h| ProjectedCash {
                    resource: h.resource.clone(),
                    asset: h.amount.asset.clone(),
                    amount: QuantityInterval {
                        minimum: h.amount.atoms.clone(),
                        maximum: h.amount.atoms.clone(),
                    },
                })
                .collect(),
            positions: baseline
                .state
                .positions
                .iter()
                .map(|p| ProjectedPosition {
                    resource: p.resource.clone(),
                    position: p.position.clone(),
                    asset: p.amount.asset.clone(),
                    position_kind: p.kind.clone(),
                    amount: QuantityInterval {
                        minimum: p.amount.atoms.clone(),
                        maximum: p.amount.atoms.clone(),
                    },
                })
                .collect(),
            authorities: baseline.state.authorities.clone(),
        },
        baseline: baseline.clone(),
        allocations: vec![
            ResourceAmount {
                resource: owner.clone(),
                amount: Amount {
                    asset: native.clone(),
                    atoms: Quantity::from_decimal("0.01", 18).unwrap(),
                },
            },
            ResourceAmount {
                resource: destination.clone(),
                amount: Amount {
                    asset: native.clone(),
                    atoms: Quantity::from_decimal("0.0002", 18).unwrap(),
                },
            },
            ResourceAmount {
                resource: owner.clone(),
                amount: Amount {
                    asset: usdc.clone(),
                    atoms: Quantity::from_u64(500_000),
                },
            },
        ],
        step_index: 0,
        prior_proofs: vec![],
        remaining_cost_budget_usd_micros: Quantity::from_u64(10_000_000),
        asset_prices: vec![],
    };
    // Engine projection begins with selected allocations only. The complete
    // zero baseline proves this recipient asset despite its omitted cash row.
    context
        .projected
        .cash
        .retain(|row| row.resource != destination || row.asset != usdc);
    let mk = |wallet: &str, token: &str, amount: &str, recipient: Address| {
        let mut value = input();
        value.as_object_mut().unwrap().extend(json!({"wallet":wallet,"token":token,"amount":amount,"recipient":lower_address(recipient),"max_fee_eth":"0.0001"}).as_object().unwrap().clone());
        value
    };
    let primary = a.wallets[0].name.clone();
    // A principal credit smaller than one return fee cannot serve as its
    // funding proof. Both return fees are selected at the destination upfront.
    let forward = mk(&primary, "native", "0.00001", addr(2));
    let mut missing_gas = context.clone();
    missing_gas
        .allocations
        .retain(|h| h.resource != destination);
    assert_eq!(
        a.preflight_return_route("wallet.transfer", &forward, &missing_gas)
            .await
            .unwrap_err()
            .code,
        "PLAN_RETURN_GAS_UNFUNDED"
    );
    let mut missing_destination_coverage = context.clone();
    missing_destination_coverage
        .baseline
        .coverage
        .retain(|covered| covered != &destination.0);
    assert_eq!(
        a.preflight_return_route("wallet.transfer", &forward, &missing_destination_coverage,)
            .await
            .unwrap_err()
            .code,
        "PLAN_BASELINE_INCOMPLETE"
    );
    let mut self_transfer = forward.clone();
    self_transfer["recipient"] = json!(lower_address(addr(1)));
    assert_eq!(
        a.preflight_return_route("wallet.transfer", &self_transfer, &context)
            .await
            .unwrap_err()
            .code,
        "RECIPIENT_INVALID"
    );
    let mut arbitrary = forward.clone();
    arbitrary["recipient"] = json!(lower_address(addr(3)));
    assert_eq!(
        a.preflight_return_route("wallet.transfer", &arbitrary, &context)
            .await
            .unwrap_err()
            .code,
        "PLAN_RECIPIENT_UNSUPPORTED"
    );
    a.wallets[1].signing_enabled = false;
    assert_eq!(
        a.preflight_return_route("wallet.transfer", &forward, &context)
            .await
            .unwrap_err()
            .code,
        "PLAN_RECIPIENT_UNSUPPORTED"
    );
    a.wallets[1].signing_enabled = true;
    let proof = a
        .preflight_return_route("wallet.transfer", &forward, &context)
        .await
        .unwrap();
    apply_test_plan(&mut context, proof);
    let back = mk("return-wallet", "native", "0.00001", addr(1));
    let proof = a
        .preflight_return_route("wallet.transfer", &back, &context)
        .await
        .unwrap();
    assert!(proof.requires.iter().any(|r| matches!(r,
        ProjectedRequirement::CashAtLeast {resource,asset,amount,source_step:None}
            if resource == &destination && asset == &native
                && amount == &Quantity::from_decimal("0.0001", 18).unwrap()
    )));
    assert!(proof.requires.iter().any(|r| matches!(r,
        ProjectedRequirement::CashAtLeast {resource,asset,amount,source_step:Some(0)}
            if resource == &destination && asset == &native
                && amount == &Quantity::from_decimal("0.00001", 18).unwrap()
    )));
    apply_test_plan(&mut context, proof);
    let mut wrap = input();
    wrap["amount"] = json!("0.002");
    wrap["max_fee_eth"] = json!("0.0001");
    let wrap_proof = a
        .preflight_return_route("wallet.wrap", &wrap, &context)
        .await
        .unwrap();
    // The latest returned credit anchors the source chain, whose original
    // selected cash funds the larger subsequent wrap.
    assert!(
        wrap_proof
            .requires
            .iter()
            .any(|requirement| matches!(requirement,
                ProjectedRequirement::CashAtLeast {resource,asset,amount,source_step:Some(1)}
                    if resource == &owner && asset == &native
                        && amount == &Quantity::from_decimal("0.002",18).unwrap()
            ))
    );
    let forward = mk(&primary, &usdc.reference, "0.25", addr(2));
    for case in 0..4 {
        let mut invalid = context.clone();
        match case {
            0 => {
                invalid.baseline.state.holdings.retain(|holding| {
                    holding.resource != destination || holding.amount.asset != usdc
                })
            }
            1 => {
                let duplicate = invalid
                    .baseline
                    .state
                    .holdings
                    .iter()
                    .find(|holding| holding.resource == destination && holding.amount.asset == usdc)
                    .unwrap()
                    .clone();
                invalid.baseline.state.holdings.push(duplicate);
            }
            2 => {
                invalid
                    .baseline
                    .state
                    .holdings
                    .iter_mut()
                    .find(|holding| holding.resource == destination && holding.amount.asset == usdc)
                    .unwrap()
                    .amount
                    .atoms = Quantity::from_u64(1);
            }
            3 => {
                // Missing a nonzero derived row cannot be interpreted as zero.
                invalid
                    .prior_proofs
                    .last_mut()
                    .unwrap()
                    .effects
                    .push(ProjectedEffect::CashDelta {
                        resource: destination.clone(),
                        asset: usdc.clone(),
                        minimum_delta: SignedQuantity::positive(&Quantity::from_u64(1)),
                        maximum_delta: SignedQuantity::positive(&Quantity::from_u64(1)),
                    });
            }
            _ => unreachable!(),
        }
        assert_eq!(
            a.preflight_return_route("wallet.transfer", &forward, &invalid)
                .await
                .unwrap_err()
                .code,
            "PLAN_CASH_UNPROVED",
            "zero destination case {case}"
        );
    }
    let proof = a
        .preflight_return_route("wallet.transfer", &forward, &context)
        .await
        .unwrap();
    apply_test_plan(&mut context, proof);
    let back = mk("return-wallet", &usdc.reference, "0.25", addr(1));
    let proof = a
        .preflight_return_route("wallet.transfer", &back, &context)
        .await
        .unwrap();
    apply_test_plan(&mut context, proof);
    for wallet in [&owner, &destination] {
        let original = baseline
            .state
            .holdings
            .iter()
            .find(|h| &h.resource == wallet && h.amount.asset == usdc)
            .unwrap();
        let final_cash = context
            .projected
            .cash
            .iter()
            .find(|h| &h.resource == wallet && h.asset == usdc)
            .unwrap();
        assert_eq!(final_cash.amount.minimum, original.amount.atoms);
        assert_eq!(final_cash.amount.maximum, original.amount.atoms);
        let original_native = baseline
            .state
            .holdings
            .iter()
            .find(|h| &h.resource == wallet && h.amount.asset == native)
            .unwrap();
        let final_native = context
            .projected
            .cash
            .iter()
            .find(|h| &h.resource == wallet && h.asset == native)
            .unwrap();
        assert_eq!(
            original_native
                .amount
                .atoms
                .checked_sub(&final_native.amount.minimum)
                .unwrap(),
            Quantity::from_decimal("0.0002", 18).unwrap()
        );
        assert_eq!(original_native.amount.atoms, final_native.amount.maximum);
    }
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "eth_estimateGas" || m == "eth_sendRawTransaction")
    );
}

#[tokio::test]
async fn combined_native_credits_fund_only_their_guaranteed_selected_history() {
    let rpc = Arc::new(Fixture::new());
    let mut a = adapter(rpc.clone());
    a.wallets.push(WalletConnection {
        name: "return-wallet".into(),
        address: lower_address(addr(2)),
        signing_enabled: true,
    });
    let owner = resource(1, addr(1));
    let donor = resource(1, addr(2));
    let native = a.native_asset();
    let baseline = a.read_snapshot().await.unwrap();
    let mut context = synthetic_route_context(
        baseline,
        vec![
            ResourceAmount {
                resource: owner.clone(),
                amount: amount(&native, exact("0.0002", 18).unwrap()),
            },
            ResourceAmount {
                resource: donor.clone(),
                amount: amount(&native, exact("0.003", 18).unwrap()),
            },
        ],
    );
    for index in 0..2 {
        let mut request = input();
        request.as_object_mut().unwrap().extend(
            json!({
                "wallet":"return-wallet", "token":"native", "amount":"0.0003",
                "recipient":lower_address(addr(1)), "max_fee_eth":"0.0001",
                "request_id":format!("synthetic-combined-credit-{index}"),
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let proof = a
            .preflight_return_route("wallet.transfer", &request, &context)
            .await
            .unwrap();
        apply_test_plan(&mut context, proof);
    }
    let mut wrap = input();
    wrap["amount"] = json!("0.0006");
    wrap["max_fee_eth"] = json!("0.0001");
    let proof = a
        .preflight_return_route("wallet.wrap", &wrap, &context)
        .await
        .unwrap();
    // Each credit (0.0003) and original allocation (0.0002) is smaller than
    // principal (0.0006); the guaranteed combined source funds it and gas.
    assert!(
        proof
            .requires
            .iter()
            .any(|requirement| matches!(requirement,
                ProjectedRequirement::CashAtLeast {resource,asset,amount,source_step:Some(1)}
                    if resource == &owner && asset == &native
                        && amount == &Quantity::from_decimal("0.0006",18).unwrap()
            ))
    );
    for case in 0..3 {
        let mut invalid = context.clone();
        let first_credit = invalid.prior_proofs[0].effects.iter_mut().find(|effect| matches!(effect,
            ProjectedEffect::CashDelta {resource,asset,..} if resource == &owner && asset == &native
        )).unwrap();
        match case {
            0 => {
                if let ProjectedEffect::CashDelta { asset, .. } = first_credit {
                    asset.network = "ethereum:2".into();
                }
            }
            1 => {
                if let ProjectedEffect::CashDelta { resource, .. } = first_credit {
                    *resource = super::resource(1, addr(3));
                }
            }
            2 => {
                invalid.prior_proofs[1]
                    .effects
                    .push(ProjectedEffect::CashDelta {
                        resource: owner.clone(),
                        asset: native.clone(),
                        minimum_delta: SignedQuantity::negative(
                            &Quantity::from_decimal("0.0002", 18).unwrap(),
                        ),
                        maximum_delta: SignedQuantity::negative(
                            &Quantity::from_decimal("0.0002", 18).unwrap(),
                        ),
                    });
            }
            _ => unreachable!(),
        }
        assert_eq!(
            a.preflight_return_route("wallet.wrap", &wrap, &invalid)
                .await
                .unwrap_err()
                .code,
            if case == 2 {
                "PLAN_GAS_UNFUNDED"
            } else {
                "PLAN_PRINCIPAL_UNFUNDED"
            },
            "case {case}"
        );
    }
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "eth_estimateGas" || method == "eth_sendRawTransaction")
    );
}

#[tokio::test]
async fn broker_verified_native_deposit_is_exact_scoped_fresh_and_decimal_safe() {
    let rpc = Arc::new(Fixture::new());
    let a = adapter(rpc.clone());
    let owner = resource(1, addr(1));
    let venue = ResourceId("synthetic-binance-observed-main".into());
    let venue_asset = AssetId {
        network: "binance".into(),
        reference: "ETH".into(),
        decimals: 36,
    };
    let mut baseline = a.read_snapshot().await.unwrap();
    baseline.coverage.push(venue.0.clone());
    baseline.state.holdings.push(ResourceAmount {
        resource: venue.clone(),
        amount: Amount {
            asset: venue_asset.clone(),
            atoms: Quantity::zero(),
        },
    });
    let context = ReturnRouteContext {
        projected: ProjectedState {
            cash: baseline
                .state
                .holdings
                .iter()
                .map(|h| ProjectedCash {
                    resource: h.resource.clone(),
                    asset: h.amount.asset.clone(),
                    amount: QuantityInterval {
                        minimum: h.amount.atoms.clone(),
                        maximum: h.amount.atoms.clone(),
                    },
                })
                .collect(),
            positions: vec![],
            authorities: baseline.state.authorities.clone(),
        },
        baseline,
        allocations: vec![ResourceAmount {
            resource: owner.clone(),
            amount: Amount {
                asset: a.native_asset(),
                atoms: Quantity::from_decimal("0.01", 18).unwrap(),
            },
        }],
        step_index: 0,
        prior_proofs: vec![],
        remaining_cost_budget_usd_micros: Quantity::from_u64(1_000_000),
        asset_prices: vec![],
    };
    let mut inner = input();
    inner.as_object_mut().unwrap().extend(json!({"token":"native","amount":"0.001","recipient":lower_address(addr(2)),"max_fee_eth":"0.0001"}).as_object().unwrap().clone());
    let target = BrokerVerifiedNativeDepositTarget {
        chain_id: 1,
        account_resource: venue.clone(),
        address: lower_address(addr(2)),
        asset: venue_asset.clone(),
        minimum_deposit: Quantity::from_decimal("0.00001", 36).unwrap(),
        network: "ETH".into(),
        observed_at: now(),
        expires_at: (chrono::Utc::now() + chrono::Duration::seconds(90))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        evidence_digest: binding_hash(&json!({"synthetic_authenticated_observation":true}))
            .unwrap(),
    };
    // The broker seam does not widen the public wallet-transfer route profile.
    assert_eq!(
        a.preflight_return_route("wallet.transfer", &inner, &context)
            .await
            .unwrap_err()
            .code,
        "PLAN_RECIPIENT_UNSUPPORTED"
    );
    let mut wrong = target.clone();
    wrong.chain_id = 2;
    assert_eq!(
        a.preflight_native_deposit_transfer(&inner, &context, &wrong)
            .await
            .unwrap_err()
            .code,
        "DEPOSIT_TARGET_INVALID"
    );
    wrong = target.clone();
    wrong.asset.decimals = 18;
    assert_eq!(
        a.preflight_native_deposit_transfer(&inner, &context, &wrong)
            .await
            .unwrap_err()
            .code,
        "DEPOSIT_TARGET_INVALID"
    );
    wrong = target.clone();
    wrong.address = lower_address(addr(3));
    assert_eq!(
        a.preflight_native_deposit_transfer(&inner, &context, &wrong)
            .await
            .unwrap_err()
            .code,
        "DEPOSIT_TARGET_MISMATCH"
    );
    wrong = target.clone();
    wrong.observed_at = (chrono::Utc::now() - chrono::Duration::seconds(91))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    assert_eq!(
        a.preflight_native_deposit_transfer(&inner, &context, &wrong)
            .await
            .unwrap_err()
            .code,
        "DEPOSIT_TARGET_STALE"
    );
    wrong = target.clone();
    wrong.minimum_deposit =
        Quantity::from_decimal("0.001000000000000000000000000000000001", 36).unwrap();
    assert_eq!(
        a.preflight_native_deposit_transfer(&inner, &context, &wrong)
            .await
            .unwrap_err()
            .code,
        "DEPOSIT_MINIMUM"
    );
    let mut missing = context.clone();
    missing.baseline.coverage.retain(|r| r != &venue.0);
    assert_eq!(
        a.preflight_native_deposit_transfer(&inner, &missing, &target)
            .await
            .unwrap_err()
            .code,
        "PLAN_BASELINE_INCOMPLETE"
    );
    let mut unselected = context.clone();
    unselected.allocations[0].amount.atoms = Quantity::from_decimal("0.001", 18).unwrap();
    assert_eq!(
        a.preflight_native_deposit_transfer(&inner, &unselected, &target)
            .await
            .unwrap_err()
            .code,
        "PLAN_PRINCIPAL_UNFUNDED"
    );
    let mut wrong_token = inner.clone();
    wrong_token["token"] = json!(lower_address(addr(101)));
    assert_eq!(
        a.preflight_native_deposit_transfer(&wrong_token, &context, &target)
            .await
            .unwrap_err()
            .code,
        "DEPOSIT_TARGET_MISMATCH"
    );
    let mut all = inner.clone();
    all["amount"] = json!("all");
    all["max_amount"] = json!("0.01");
    assert!(
        a.preflight_native_deposit_transfer(&all, &context, &target)
            .await
            .is_err()
    );
    let proof = a
        .preflight_native_deposit_transfer(&inner, &context, &target)
        .await
        .unwrap();
    let debit = Quantity::from_decimal("0.001", 18).unwrap();
    let credit = Quantity::from_decimal("0.001", 36).unwrap();
    assert!(proof.intent.expected.iter().any(|e|matches!(e,EffectConstraint::Transfer {from,to,debit_asset,credit_asset,maximum_debit,minimum_credit} if from == &owner && to == &venue && debit_asset == &a.native_asset() && credit_asset == &venue_asset && maximum_debit == &debit && minimum_credit == &credit)));
    assert!(proof.effects.iter().any(|e|matches!(e,ProjectedEffect::CashDelta {resource,asset,minimum_delta,maximum_delta} if resource == &venue && asset == &venue_asset && minimum_delta == &SignedQuantity::positive(&credit) && maximum_delta == &SignedQuantity::positive(&credit))));
    assert_eq!(
        proof.evidence["broker_verified_deposit_target"]["evidence_digest"],
        json!(target.evidence_digest)
    );
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "eth_estimateGas" || m == "eth_sendRawTransaction")
    );
    // The same fixed recipient/amount produces an ordinary frozen preview;
    // no signer, alternate envelope or special dispatch API is introduced.
    let prepared = a.prepare("wallet.transfer", &inner).await.unwrap();
    let binding: Binding = serde_json::from_value(prepared.binding).unwrap();
    assert_eq!(binding.transaction.to, target.address);
    assert_eq!(binding.transaction.value, debit);

    // The generalized internal seam preserves exact ERC20 units instead of
    // assuming every source asset has native ETH's 18 decimal places.
    let token_asset = a.asset(&lower_address(addr(101)), 6);
    let mut token_context = context.clone();
    token_context.allocations.push(ResourceAmount {
        resource: owner.clone(),
        amount: Amount {
            asset: token_asset.clone(),
            atoms: Quantity::from_decimal("2", 6).unwrap(),
        },
    });
    let mut token_target = target.clone();
    token_target.asset.reference = "USDC".into();
    token_context.baseline.state.holdings.push(ResourceAmount {
        resource: venue.clone(),
        amount: Amount {
            asset: token_target.asset.clone(),
            atoms: Quantity::zero(),
        },
    });
    token_context.projected.cash.push(ProjectedCash {
        resource: venue.clone(),
        asset: token_target.asset.clone(),
        amount: QuantityInterval {
            minimum: Quantity::zero(),
            maximum: Quantity::zero(),
        },
    });
    let mut token_input = inner.clone();
    token_input["token"] = json!(token_asset.reference);
    token_input["amount"] = json!("1.000001");
    let proof = a
        .preflight_deposit_transfer(&token_input, &token_context, &token_target, &token_asset)
        .await
        .unwrap();
    assert!(proof.intent.expected.iter().any(|effect| matches!(effect,
        EffectConstraint::Transfer { debit_asset, maximum_debit, minimum_credit, .. }
        if debit_asset == &token_asset
        && maximum_debit == &Quantity::from_decimal("1.000001", 6).unwrap()
        && minimum_credit == &Quantity::from_decimal("1.000001", 36).unwrap()
    )));
    let mut wrong_asset = token_asset.clone();
    wrong_asset.decimals = 18;
    assert_eq!(
        a.preflight_deposit_transfer(&token_input, &token_context, &token_target, &wrong_asset)
            .await
            .unwrap_err()
            .code,
        "DEPOSIT_TARGET_MISMATCH"
    );
    wrong_asset = token_asset.clone();
    wrong_asset.network = "ethereum:42161".into();
    assert_eq!(
        a.preflight_deposit_transfer(&token_input, &token_context, &token_target, &wrong_asset)
            .await
            .unwrap_err()
            .code,
        "DEPOSIT_TARGET_MISMATCH"
    );
}

mod wallet_only_tests {
    use super::*;
    struct Prices(AtomicU64);
    #[async_trait]
    impl WalletValuation for Prices {
        async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
            Ok((
                quantity(
                    mul(
                        fin_evm::u256(&units)?,
                        U256::from(self.0.load(Ordering::SeqCst)),
                    )? / pow10(asset.decimals)?,
                ),
                "synthetic-wallet-usd".into(),
            ))
        }
    }
    struct Discovery {
        complete: bool,
        failure: bool,
    }
    #[async_trait]
    impl TokenDiscovery for Discovery {
        async fn tokens(&self, chain: u64, owner: &str) -> Result<TokenDiscoveryResult> {
            assert!([42161, 4663].contains(&chain));
            assert_eq!(owner, lower_address(addr(1)));
            if self.failure {
                return Err(error(
                    "SYNTHETIC_DISCOVERY_FAILED",
                    "read",
                    "Isolated discovery failure.",
                ));
            }
            Ok(TokenDiscoveryResult {
                candidates: vec![lower_address(addr(101))],
                complete: self.complete,
                source: "synthetic-indexer".into(),
                warnings: if self.complete {
                    vec![]
                } else {
                    vec!["Synthetic incomplete index.".into()]
                },
            })
        }
    }
    fn wallet_adapter(chain: u64, rpc: Arc<Fixture>) -> EthereumAdapter {
        rpc.chain.store(chain, Ordering::SeqCst);
        let rpc: Arc<dyn EvmRpc> = if chain == 8453 {
            // Explicit synthetic Base oracle/header/receipt evidence; this
            // fixture never establishes deployed fee or biometric acceptance.
            Arc::new(super::base_aave_tests::BaseAaveRpc { inner: rpc })
        } else {
            rpc
        };
        EthereumAdapter::new(
            EthereumConfig::wallet_only(chain, 2, vec![]),
            vec![WalletConnection {
                name: "synthetic".into(),
                address: lower_address(addr(1)),
                signing_enabled: true,
            }],
            Arc::new(EvmRuntime::new(rpc, chain, None)),
        )
        .unwrap()
    }
    fn transfer(chain: u64, token: &str) -> Value {
        json!({"wallet":"synthetic","network":"evm","chain_id":chain,"request_id":"synthetic-wallet-request","max_fee_eth":"0.001","token":token,"amount":if token=="native" {"0.001"}else{"1"},"recipient":lower_address(addr(2))})
    }
    #[test]
    fn optional_deployments_serialize_without_dummy_addresses_and_capabilities_are_wallet_only() {
        for chain in [42161, 4663] {
            let a = wallet_adapter(chain, Arc::new(Fixture::new()));
            let config = serde_json::to_value(&a.config).unwrap();
            assert_eq!(config["weth"], Value::Null);
            assert_eq!(config["confirmations"], json!(1));
            assert_eq!(config["require_finalized"], json!(false));
            let parsed: EthereumConfig = serde_json::from_value(
                json!({"chain_id":chain,"confirmations":2,"snapshot_tokens":[]}),
            )
            .unwrap();
            assert!(parsed.aave.is_none() && parsed.uniswap.is_none());
            assert_eq!(
                a.definitions()
                    .iter()
                    .map(|d| d.name.as_str())
                    .collect::<BTreeSet<_>>(),
                [
                    "wallet.transfer",
                    "wallet.balances",
                    "wallet.token",
                    "wallet.allowance",
                    "wallet.receipt"
                ]
                .into_iter()
                .collect()
            );
            assert_eq!(a.connections()[0].provider, "evm");
            assert!(
                a.connections()[0]
                    .resource
                    .0
                    .starts_with(&format!("ethereum:{chain}:"))
            );
        }
    }
    #[tokio::test]
    async fn discovered_balances_and_snapshots_are_verified_without_protocol_dependencies() {
        for chain in [42161, 4663] {
            let rpc = Arc::new(Fixture::new());
            let a = wallet_adapter(chain, rpc.clone()).with_token_discovery(Arc::new(Discovery {
                complete: true,
                failure: false,
            }));
            let input = json!({"wallet":"synthetic","network":"evm","chain_id":chain});
            let result = a.read("wallet.balances", &input).await.unwrap();
            assert_eq!(result["data"].as_array().unwrap().len(), 2);
            assert_eq!(result["discovery"]["complete"], json!(true));
            assert_eq!(result["block"], json!(20));
            let snapshot = a.snapshot().await.unwrap();
            assert!(snapshot.complete && snapshot.pending.is_empty());
            assert_eq!(snapshot.state.holdings.len(), 1);
            assert!(snapshot.state.positions.is_empty() && snapshot.state.authorities.is_empty());
            let explicit = json!({"wallet":"synthetic","network":"ethereum","chain_id":chain,"tokens":[lower_address(addr(101))]});
            assert_eq!(
                a.read("wallet.balances", &explicit).await.unwrap()["discovery"]["source"],
                json!("explicit-token-scope")
            );
        }
    }
    #[tokio::test]
    async fn partial_missing_or_failed_discovery_keeps_balances_honest_and_baselines_scoped() {
        for chain in [42161, 4663] {
            let mut a = wallet_adapter(chain, Arc::new(Fixture::new()));
            a.config.snapshot_tokens = vec![lower_address(addr(100))];
            let snapshot = a.snapshot().await.unwrap();
            assert!(snapshot.complete);
            assert_eq!(snapshot.state.holdings.len(), 2);
            assert!(
                snapshot
                    .coverage
                    .iter()
                    .any(|s| s.starts_with("explicit-token-scope:")
                        && s.ends_with(&lower_address(addr(100))))
            );
            let input = json!({"wallet":"synthetic","network":"evm","chain_id":chain});
            assert_eq!(
                a.read("wallet.balances", &input).await.unwrap()["discovery"]["complete"],
                json!(false)
            );
            let a = a.with_token_discovery(Arc::new(Discovery {
                complete: false,
                failure: false,
            }));
            let result = a.read("wallet.balances", &input).await.unwrap();
            assert_eq!(result["discovery"]["complete"], json!(false));
            assert_eq!(result["data"].as_array().unwrap().len(), 3);
            assert_eq!(a.snapshot().await.unwrap().state.holdings.len(), 2);
            let a = a.with_token_discovery(Arc::new(Discovery {
                complete: true,
                failure: true,
            }));
            assert_eq!(
                a.read("wallet.balances", &input).await.unwrap_err().code,
                "SYNTHETIC_DISCOVERY_FAILED"
            );
            assert!(a.snapshot().await.unwrap().complete);
        }
    }
    struct CandidateDiscovery(Vec<String>);
    #[async_trait]
    impl TokenDiscovery for CandidateDiscovery {
        async fn tokens(&self, _: u64, _: &str) -> Result<TokenDiscoveryResult> {
            Ok(TokenDiscoveryResult {
                candidates: self.0.clone(),
                complete: true,
                source: "synthetic-candidate-index".into(),
                warnings: vec![],
            })
        }
    }
    #[tokio::test]
    async fn full_indexer_page_retains_configured_holdings_and_bounds_additional_candidates() {
        for chain in [42161, 4663] {
            let mut a = wallet_adapter(chain, Arc::new(Fixture::new()));
            let configured = vec![lower_address(addr(100)), lower_address(addr(101))];
            a.config.snapshot_tokens = configured.clone();
            let a = a.with_token_discovery(Arc::new(CandidateDiscovery(
                (1000..1064).map(|n| lower_address(addr(n))).collect(),
            )));
            let result = a
                .read(
                    "wallet.balances",
                    &json!({"wallet":"synthetic","network":"evm","chain_id":chain}),
                )
                .await
                .unwrap();
            let candidates = result["discovery"]["candidates"].as_array().unwrap();
            assert_eq!(candidates.len(), 64);
            assert_eq!(candidates[0], json!(configured[0]));
            assert_eq!(candidates[1], json!(configured[1]));
            assert_eq!(result["data"].as_array().unwrap().len(), 65);
            assert_eq!(result["discovery"]["complete"], json!(false));
            assert!(
                result["discovery"]["warnings"][0]
                    .as_str()
                    .unwrap()
                    .contains("additional candidates omitted")
            );
            let explicit = json!({"wallet":"synthetic","network":"evm","chain_id":chain,"tokens":[lower_address(addr(1000))]});
            assert_eq!(
                a.read("wallet.balances", &explicit).await.unwrap()["data"]
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
            let duplicate = json!({"wallet":"synthetic","network":"evm","chain_id":chain,"tokens":[lower_address(addr(1000)),lower_address(addr(1000))]});
            assert_eq!(
                a.read("wallet.balances", &duplicate)
                    .await
                    .unwrap_err()
                    .code,
                "DUPLICATE_TOKEN"
            );
        }
    }
    struct BadMetadataRpc(Arc<Fixture>);
    #[async_trait]
    impl EvmRpc for BadMetadataRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            if method == "eth_call" && params[0]["to"] == json!(lower_address(addr(900))) {
                return Ok(json!("0x")); // deployed non-ERC20 contract: no metadata ABI
            }
            self.0.request(method, params).await
        }
    }
    #[tokio::test]
    async fn invalid_discovered_contracts_report_partial_balances_but_explicit_reads_fail_strictly()
    {
        for chain in [42161, 4663] {
            let rpc = Arc::new(Fixture::new());
            let mut a = wallet_adapter(chain, rpc.clone());
            a.runtime = Arc::new(EvmRuntime::new(Arc::new(BadMetadataRpc(rpc)), chain, None));
            let a = a.with_token_discovery(Arc::new(CandidateDiscovery(vec![
                lower_address(addr(1)),
                lower_address(addr(900)),
                lower_address(addr(101)),
            ])));
            let result = a
                .read(
                    "wallet.balances",
                    &json!({"wallet":"synthetic","network":"evm","chain_id":chain}),
                )
                .await
                .unwrap();
            assert_eq!(result["data"].as_array().unwrap().len(), 2);
            assert_eq!(
                result["data"][1]["amount"]["asset"]["reference"],
                json!(lower_address(addr(101)))
            );
            assert_eq!(result["discovery"]["complete"], json!(false));
            assert_eq!(result["discovery"]["warnings"].as_array().unwrap().len(), 2);
            for bad in [addr(1), addr(900)] {
                let explicit = json!({"wallet":"synthetic","network":"evm","chain_id":chain,"tokens":[lower_address(bad)]});
                assert!(a.read("wallet.balances", &explicit).await.is_err());
            }
        }
    }
    #[tokio::test]
    async fn wallet_only_refuses_protocol_writes_and_missing_valuation() {
        for chain in [42161, 4663] {
            let a = wallet_adapter(chain, Arc::new(Fixture::new()));
            for operation in [
                "wallet.wrap",
                "wallet.unwrap",
                "wallet.approve",
                "uniswap.swap",
                "aave.supply",
            ] {
                assert_eq!(
                    a.prepare(operation, &json!({})).await.unwrap_err().code,
                    "OPERATION_UNAVAILABLE"
                );
            }
            for operation in ["uniswap.quote", "aave.positions", "aave.reserves"] {
                assert_eq!(
                    a.read(operation, &json!({})).await.unwrap_err().code,
                    "OPERATION_UNAVAILABLE"
                );
            }
            assert_eq!(
                a.prepare("wallet.transfer", &transfer(chain, "native"))
                    .await
                    .unwrap_err()
                    .code,
                "VALUATION_UNAVAILABLE"
            );
        }
    }
    #[tokio::test]
    async fn both_networks_freeze_transfers_fees_nonce_destination_and_price() {
        for chain in [42161, 4663] {
            for token in ["native".into(), lower_address(addr(101))] {
                let rpc = Arc::new(Fixture::new());
                let prices = Arc::new(Prices(AtomicU64::new(3_000_000_000)));
                let a = wallet_adapter(chain, rpc.clone()).with_wallet_valuation(prices.clone());
                let input = transfer(chain, &token);
                let prepared = a.prepare("wallet.transfer", &input).await.unwrap();
                let binding: Binding = serde_json::from_value(prepared.binding.clone()).unwrap();
                assert_eq!(binding.transaction.chain_id, chain);
                assert!(binding.transaction.maximum_cost().unwrap() <= exact("0.001", 18).unwrap());
                assert_eq!(
                    a.revalidate(&prepared).await.unwrap().binding,
                    prepared.binding
                );
                rpc.nonce.store(1, Ordering::SeqCst);
                assert!(a.revalidate(&prepared).await.is_err());
                rpc.nonce.store(0, Ordering::SeqCst);
                let mut changed = prepared.clone();
                changed.input["recipient"] = json!(lower_address(addr(3)));
                assert!(a.revalidate(&changed).await.is_err());
                prices.0.store(3_100_000_000, Ordering::SeqCst);
                assert_eq!(
                    a.revalidate(&prepared).await.unwrap_err().code,
                    "VALUE_BOUND_EXCEEDED"
                );
                let mut overfee = input.clone();
                overfee["max_fee_eth"] = json!("0.00000000000001");
                assert!(a.prepare("wallet.transfer", &overfee).await.is_err());
            }
        }
    }
    struct ObservedPrices {
        observed_at: String,
        fail: bool,
        calls: AtomicUsize,
    }
    #[async_trait]
    impl WalletValuation for ObservedPrices {
        async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
            EnginePrices::default().quote_usd(asset, units).await
        }
        async fn quote_observation(
            &self,
            asset: &AssetId,
            units: Quantity,
        ) -> Result<(Quantity, String, String)> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(error(
                    "SYNTHETIC_SHARED_PRICE_UNAVAILABLE",
                    "valuation",
                    "Injected source unavailable.",
                ));
            }
            let (value, _) = self.quote_usd(asset, units).await?;
            Ok((
                value,
                format!("synthetic-shared-price:{}", self.observed_at),
                self.observed_at.clone(),
            ))
        }
    }
    #[tokio::test]
    async fn injected_shared_prices_win_on_all_networks_and_keep_actual_observation_times() {
        for chain in [1, 42161, 4663, 8453] {
            let rpc = Arc::new(Fixture::new());
            let mut a = if chain == 1 {
                adapter(rpc.clone())
            } else {
                wallet_adapter(chain, rpc.clone())
            };
            *rpc.missing_selector.lock().unwrap() = Some(Oracle::getAssetPriceCall::SELECTOR);
            let observed_at = (chrono::Utc::now() - chrono::Duration::seconds(45)).to_rfc3339();
            let provider = Arc::new(ObservedPrices {
                observed_at: observed_at.clone(),
                fail: false,
                calls: AtomicUsize::new(0),
            });
            a = a.with_wallet_valuation(provider.clone());
            let prepared = a
                .prepare("wallet.transfer", &transfer(chain, "native"))
                .await
                .unwrap();
            let valuation = prepared.valuation.as_ref().unwrap();
            assert_eq!(valuation.observed_at, observed_at);
            assert!(
                valuation.prices.iter().all(|p| p.observed_at == observed_at
                    && p.source.starts_with("synthetic-shared-price:"))
            );
            assert!(valuation.source.starts_with("synthetic-shared-price:"));
            assert_eq!(valuation.movement_usd_micros, Quantity::from_u64(3_000_000));
            assert!(provider.calls.load(Ordering::SeqCst) > 0);
            assert!(
                chrono::DateTime::parse_from_rfc3339(&prepared.observed_at).unwrap()
                    > chrono::DateTime::parse_from_rfc3339(&observed_at).unwrap()
            );
            // Mainnet retains deployment and risk checks despite shared policy pricing.
            if chain == 1 {
                *rpc.paused.lock().unwrap() = true;
                assert_eq!(
                    a.prepare("aave.supply", &aave_input("aave.supply"))
                        .await
                        .unwrap_err()
                        .code,
                    "RESERVE_UNAVAILABLE"
                );
                *rpc.paused.lock().unwrap() = false;
                assert!(
                    a.quote_protocol_usd(&lower_address(addr(101)), Quantity::from_u64(1_000_000))
                        .await
                        .is_err()
                );
            }
        }
    }
    struct DistinctObservationPrices {
        principal: String,
        native: String,
    }
    #[async_trait]
    impl WalletValuation for DistinctObservationPrices {
        async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
            EnginePrices::default().quote_usd(asset, units).await
        }
        async fn quote_observation(
            &self,
            asset: &AssetId,
            units: Quantity,
        ) -> Result<(Quantity, String, String)> {
            let (amount, _) = self.quote_usd(asset, units).await?;
            let observed = if asset.reference == "native" {
                self.native.clone()
            } else {
                self.principal.clone()
            };
            Ok((
                amount,
                format!("synthetic-distinct-observation:{observed}"),
                observed,
            ))
        }
    }
    #[tokio::test]
    async fn principal_and_fee_prices_keep_distinct_observations_and_valuation_uses_oldest() {
        for chain in [1, 42161, 4663, 8453] {
            let rpc = Arc::new(Fixture::new());
            let a = if chain == 1 {
                adapter(rpc)
            } else {
                wallet_adapter(chain, rpc)
            };
            let principal = (chrono::Utc::now() - chrono::Duration::seconds(45)).to_rfc3339();
            let native = (chrono::Utc::now() - chrono::Duration::seconds(90)).to_rfc3339();
            let a = a.with_wallet_valuation(Arc::new(DistinctObservationPrices {
                principal: principal.clone(),
                native: native.clone(),
            }));
            let prepared = a
                .prepare(
                    "wallet.transfer",
                    &transfer(chain, &lower_address(addr(101))),
                )
                .await
                .unwrap();
            let valuation = prepared.valuation.unwrap();
            assert_eq!(valuation.observed_at, native);
            assert_eq!(
                valuation
                    .prices
                    .iter()
                    .find(|p| p.asset.reference == "native")
                    .unwrap()
                    .observed_at,
                native
            );
            assert_eq!(
                valuation
                    .prices
                    .iter()
                    .find(|p| p.asset.reference == lower_address(addr(101)))
                    .unwrap()
                    .observed_at,
                principal
            );
        }
    }
    #[tokio::test]
    async fn injected_unavailable_stale_future_or_malformed_prices_never_fall_back_to_aave() {
        for chain in [1, 42161, 4663, 8453] {
            for (fail, observed_at, expected) in [
                (true, now(), "SYNTHETIC_SHARED_PRICE_UNAVAILABLE"),
                (
                    false,
                    (chrono::Utc::now() - chrono::Duration::seconds(180)).to_rfc3339(),
                    "VALUATION_UNAVAILABLE",
                ),
                (
                    false,
                    (chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339(),
                    "VALUATION_UNAVAILABLE",
                ),
                (false, "invalid-timestamp".into(), "VALUATION_UNAVAILABLE"),
            ] {
                let rpc = Arc::new(Fixture::new());
                let a = if chain == 1 {
                    adapter(rpc.clone())
                } else {
                    wallet_adapter(chain, rpc.clone())
                };
                let provider = Arc::new(ObservedPrices {
                    observed_at,
                    fail,
                    calls: AtomicUsize::new(0),
                });
                let a = a.with_wallet_valuation(provider.clone());
                assert_eq!(
                    a.prepare("wallet.transfer", &transfer(chain, "native"))
                        .await
                        .unwrap_err()
                        .code,
                    expected
                );
                assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
                assert!(
                    !rpc.calls
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|method| method == "eth_sendRawTransaction")
                );
            }
        }
    }
    #[derive(Default)]
    struct IsolatedSigningSeam(AtomicUsize);
    #[async_trait]
    impl fin_evm::EnvelopeSigner for IsolatedSigningSeam {
        async fn sign_prepared(
            &self,
            _: &FrozenTransaction,
        ) -> Result<fin_evm::alloy::primitives::Bytes> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(error(
                "SYNTHETIC_WALLET_SIGNER_REACHED",
                "signing",
                "Isolated test ends before producing signed bytes.",
            ))
        }
    }
    struct IsolatedAuthorization {
        calls: AtomicUsize,
        decline: bool,
    }
    #[async_trait]
    impl Authorizer for IsolatedAuthorization {
        async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(challenge.purpose, "financial_write");
            if self.decline {
                return Err(error(
                    "SYNTHETIC_APPROVAL_DECLINED",
                    "approval",
                    "Synthetic human declines.",
                ));
            }
            Ok(ApprovalAudit {
                mechanism: "synthetic-wallet-engine-regression".into(),
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: now(),
                expires_at: challenge.expires_at.clone(),
            })
        }
    }
    #[derive(Default)]
    struct IsolatedPlanAuthorization(AtomicUsize);
    #[async_trait]
    impl Authorizer for IsolatedPlanAuthorization {
        async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
            assert_eq!(challenge.purpose, "financial_plan");
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ApprovalAudit {
                mechanism: "synthetic-wallet-plan-approval".into(),
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: now(),
                expires_at: challenge.expires_at.clone(),
            })
        }
    }
    struct SyntheticPlanSigner {
        calls: AtomicUsize,
        signer: fin_evm::LocalEnvelopeSigner,
    }
    impl SyntheticPlanSigner {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                // Deterministic test-only key. This fixture never reaches a
                // public network and the key is not a real credential.
                signer: fin_evm::LocalEnvelopeSigner::from_private_key(
                    "0x0000000000000000000000000000000000000000000000000000000000000001",
                )
                .unwrap(),
            }
        }
        fn address(&self) -> String {
            format!("{:#x}", self.signer.address())
        }
    }
    #[async_trait]
    impl fin_evm::EnvelopeSigner for SyntheticPlanSigner {
        async fn sign_prepared(
            &self,
            transaction: &FrozenTransaction,
        ) -> Result<fin_evm::alloy::primitives::Bytes> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.signer.sign_prepared(transaction).await
        }
    }
    struct SyntheticPlanDispatch {
        inner: Arc<Fixture>,
        calls: AtomicUsize,
    }
    #[async_trait]
    impl EvmRpc for SyntheticPlanDispatch {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            if method == "eth_sendRawTransaction" {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let encoded = params[0]
                    .as_str()
                    .and_then(|value| value.strip_prefix("0x"))
                    .expect("signed transaction is hex encoded");
                let raw = hex::decode(encoded).expect("signed transaction hex is valid");
                let hash = fin_evm::alloy::primitives::keccak256(raw);
                let receipt_hash = format!("{hash:#x}");
                if let Some(receipt) = self.inner.receipt.lock().unwrap().as_object_mut() {
                    receipt.insert("transactionHash".into(), json!(receipt_hash));
                    if let Some(logs) = receipt.get_mut("logs").and_then(Value::as_array_mut) {
                        for log in logs {
                            log["transactionHash"] = json!(receipt_hash);
                        }
                    }
                }
                self.inner.latest.store(22, Ordering::SeqCst);
                return Ok(json!(receipt_hash));
            }
            let mut result = self.inner.request(method, params).await?;
            if method == "eth_getTransactionByHash" {
                result["hash"] = self.inner.receipt.lock().unwrap()["transactionHash"].clone();
            }
            Ok(result)
        }
    }
    struct EnginePrices(AtomicU64);
    impl Default for EnginePrices {
        fn default() -> Self {
            Self(AtomicU64::new(1))
        }
    }
    #[async_trait]
    impl WalletValuation for EnginePrices {
        async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
            let rate = if asset.reference == "native" {
                3_000_000_000u64
            } else {
                1_000_000
            };
            Ok((
                quantity(
                    mul(
                        fin_evm::u256(&units)?,
                        U256::from(rate * self.0.load(Ordering::SeqCst)),
                    )? / pow10(asset.decimals)?,
                ),
                "synthetic-engine-wallet-prices".into(),
            ))
        }
    }
    #[tokio::test]
    async fn durable_engine_on_both_chains_enforces_declines_and_one_signing_attempt_across_duplicates_and_restart()
     {
        use fin_engine::{Engine, EngineOptions};
        use fin_store::Store;
        for chain in [42161, 4663] {
            for token in ["native".into(), lower_address(addr(101))] {
                for decline in [true, false] {
                    let rpc = Arc::new(Fixture::new());
                    let signer = Arc::new(IsolatedSigningSeam::default());
                    let mut adapter = wallet_adapter(chain, rpc.clone())
                        .with_wallet_valuation(Arc::new(EnginePrices::default()));
                    adapter.runtime =
                        Arc::new(EvmRuntime::new(rpc.clone(), chain, Some(signer.clone())));
                    let adapter = Arc::new(adapter);
                    let authorization = Arc::new(IsolatedAuthorization {
                        calls: AtomicUsize::new(0),
                        decline,
                    });
                    let directory = tempfile::tempdir().unwrap();
                    let database = directory.path().join("synthetic-wallet-engine.sqlite");
                    let engine = Engine::new(
                        Store::open(&database).unwrap(),
                        vec![adapter.clone()],
                        authorization.clone(),
                        EngineOptions::default(),
                    )
                    .unwrap();
                    let request = OperationRequest {
                        request_id: "synthetic-wallet-engine-identity".into(),
                        operation: "wallet.transfer".into(),
                        input: transfer(chain, &token),
                        run_id: None,
                    };
                    let (first, concurrent) = tokio::join!(
                        engine.execute(request.clone()),
                        engine.execute(request.clone())
                    );
                    let first = first.unwrap();
                    let concurrent = concurrent.unwrap();
                    assert_eq!(first.id, concurrent.id);
                    assert_eq!(
                        first.status,
                        if decline {
                            Status::Rejected
                        } else {
                            Status::Failed
                        }
                    );
                    assert_eq!(
                        first.error.as_ref().unwrap().code,
                        if decline {
                            "SYNTHETIC_APPROVAL_DECLINED"
                        } else {
                            "SYNTHETIC_WALLET_SIGNER_REACHED"
                        }
                    );
                    assert_eq!(authorization.calls.load(Ordering::SeqCst), 1);
                    assert_eq!(signer.0.load(Ordering::SeqCst), usize::from(!decline));
                    assert!(first.submission.is_none() && first.effects.is_none());
                    let duplicate = engine.execute(request.clone()).await.unwrap();
                    assert_eq!(duplicate.id, first.id);
                    drop(engine);
                    let restarted = Engine::new(
                        Store::open(&database).unwrap(),
                        vec![adapter],
                        authorization.clone(),
                        EngineOptions::default(),
                    )
                    .unwrap();
                    let duplicate = restarted.execute(request.clone()).await.unwrap();
                    assert_eq!(duplicate.id, first.id);
                    assert_eq!(duplicate.error, first.error);
                    let mut changed = request;
                    changed.input["chain_id"] = json!(if chain == 42161 { 4663 } else { 42161 });
                    assert_eq!(
                        restarted.execute(changed).await.unwrap_err().code,
                        "REQUEST_ID_CONFLICT"
                    );
                    assert_eq!(authorization.calls.load(Ordering::SeqCst), 1);
                    assert_eq!(signer.0.load(Ordering::SeqCst), usize::from(!decline));
                    assert!(
                        !rpc.calls
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|method| method == "eth_sendRawTransaction")
                    );
                }
            }
        }
    }
    #[tokio::test]
    async fn generic_exact_plan_keeps_broker_metadata_out_of_strict_ethereum_bindings() {
        use fin_domain::financial_plans::{
            FinancialPlanAuthorizationRequest, FinancialPlanBounds, FinancialPlanSpec,
            FinancialPlanStepSpec, FinancialStepCompletion,
        };
        use fin_engine::{Engine, EngineOptions};
        use fin_store::Store;

        for operation in ["wallet.transfer", "aave.collateral"] {
            let rpc = Arc::new(Fixture::new());
            if operation == "aave.collateral" {
                rpc.states.lock().unwrap().insert(
                    20,
                    AtBlock {
                        scaled_supply: U256::from(1_000_000),
                        income: ray(),
                        debt_index: ray(),
                        supply_previous: ray(),
                        collateral: true,
                        ..Default::default()
                    },
                );
            }
            let signer = Arc::new(IsolatedSigningSeam::default());
            let mut adapter =
                adapter(rpc.clone()).with_wallet_valuation(Arc::new(EnginePrices::default()));
            adapter.runtime = Arc::new(EvmRuntime::new(rpc.clone(), 1, Some(signer.clone())));
            let adapter = Arc::new(adapter);
            let input = if operation == "wallet.transfer" {
                transfer(1, "native")
            } else {
                aave_input(operation)
            };
            let candidate = adapter.prepare(operation, &input).await.unwrap();
            let variations = json!({
                "mode":"exact",
                "maximum_debits":candidate.intent.debits,
                "maximum_costs":candidate.intent.maximum_costs,
                "possible_credits":[]
            });
            let step_id = if operation == "wallet.transfer" {
                "transfer"
            } else {
                "collateral"
            };
            let plan = FinancialPlanSpec {
                version: 1,
                name: format!("Synthetic {operation} plan"),
                steps: vec![FinancialPlanStepSpec {
                    step_id: step_id.into(),
                    operation: operation.into(),
                    input: input.clone(),
                    permitted_variations: variations,
                    completion: FinancialStepCompletion {
                        operation_succeeded: true,
                        principal_spend: None,
                        minimum_net_receipt: None,
                    },
                    funding_dependencies: vec![],
                }],
                aggregate_bounds: FinancialPlanBounds {
                    maximum_debits: candidate.intent.debits.clone(),
                    maximum_costs: candidate.intent.maximum_costs.clone(),
                },
            };
            let approval = Arc::new(IsolatedPlanAuthorization::default());
            let engine = Engine::new(
                Store::in_memory().unwrap(),
                vec![adapter],
                approval.clone(),
                EngineOptions::default(),
            )
            .unwrap();
            let mut authorization_request = FinancialPlanAuthorizationRequest {
                request_id: format!("synthetic-plan-{operation}"),
                plan,
                valid_for_seconds: 600,
                replaces_revision_id: None,
                expected_review_hash: None,
            };
            let review = engine
                .preview_financial_plan(authorization_request.clone())
                .await
                .unwrap();
            authorization_request.expected_review_hash = Some(review.review_hash);
            let status = engine
                .authorize_financial_plan(authorization_request)
                .await
                .unwrap();
            let attempt = engine
                .execute_financial_plan_step(
                    &status.plan_id,
                    step_id,
                    OperationRequest {
                        request_id: format!("synthetic-attempt-{operation}"),
                        operation: operation.into(),
                        input,
                        run_id: None,
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                attempt.error.as_ref().map(|error| error.code.as_str()),
                Some("SYNTHETIC_WALLET_SIGNER_REACHED"),
                "{operation} must pass strict adapter binding decode and reach only the isolated signer"
            );
            assert_eq!(approval.0.load(Ordering::SeqCst), 1);
            assert_eq!(signer.0.load(Ordering::SeqCst), 1);
            assert!(attempt.submission.is_none() && attempt.effects.is_none());
            assert!(
                !rpc.calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|method| method == "eth_sendRawTransaction")
            );
        }
    }
    #[tokio::test]
    async fn generic_exact_wallet_wrap_plan_reconciles_verified_receipt_under_one_approval() {
        use fin_domain::financial_plans::{
            FinancialPlanAuthorizationRequest, FinancialPlanBounds, FinancialPlanSpec,
            FinancialPlanStepSpec, FinancialStepCompletion,
        };
        use fin_engine::{Engine, EngineOptions};
        use fin_store::Store;

        let rpc = Arc::new(Fixture::new());
        let signer = Arc::new(SyntheticPlanSigner::new());
        let dispatch = Arc::new(SyntheticPlanDispatch {
            inner: rpc.clone(),
            calls: AtomicUsize::new(0),
        });
        let mut adapter =
            adapter(rpc.clone()).with_wallet_valuation(Arc::new(EnginePrices::default()));
        adapter.wallets[0].address = signer.address();
        adapter.runtime = Arc::new(EvmRuntime::new(dispatch.clone(), 1, Some(signer.clone())));
        let mut input = input();
        input["amount"] = json!("0.001");
        let candidate = adapter.prepare("wallet.wrap", &input).await.unwrap();
        let binding: Binding = serde_json::from_value(candidate.binding.clone()).unwrap();
        let amount = candidate.intent.debits[0].amount.clone();
        let native = AssetId {
            network: "ethereum:1".into(),
            reference: "native".into(),
            decimals: 18,
        };
        let weth = AssetId {
            network: "ethereum:1".into(),
            reference: lower_address(addr(100)),
            decimals: 18,
        };
        let variations = json!({
            "mode":"exact",
            "maximum_debits":candidate.intent.debits,
            "maximum_costs":candidate.intent.maximum_costs,
            "possible_credits":[{"resource":candidate.intent.debits[0].resource,"asset":weth}]
        });
        rpc.set_receipt(
            &binding,
            21,
            vec![event(
                addr(100),
                Weth::Deposit {
                    dst: signer.signer.address(),
                    wad: fin_evm::u256(&amount.atoms).unwrap(),
                },
                21,
            )],
        );
        let plan = FinancialPlanSpec {
            version: 1,
            name: "Synthetic wrap settlement".into(),
            steps: vec![FinancialPlanStepSpec {
                step_id: "wrap".into(),
                operation: "wallet.wrap".into(),
                input: input.clone(),
                permitted_variations: variations,
                completion: FinancialStepCompletion {
                    operation_succeeded: true,
                    principal_spend: Some(fin_domain::ResourceAmount {
                        resource: candidate.intent.debits[0].resource.clone(),
                        amount: amount.clone(),
                    }),
                    minimum_net_receipt: None,
                },
                funding_dependencies: vec![],
            }],
            aggregate_bounds: FinancialPlanBounds {
                maximum_debits: candidate.intent.debits.clone(),
                maximum_costs: candidate.intent.maximum_costs.clone(),
            },
        };
        // Keep the receipt's principal identity explicit and exercise exact
        // asset matching in completion/accounting.
        assert_eq!(amount.asset, native);
        let authorization = Arc::new(IsolatedPlanAuthorization::default());
        let engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![Arc::new(adapter)],
            authorization.clone(),
            EngineOptions::default(),
        )
        .unwrap();
        let mut request = FinancialPlanAuthorizationRequest {
            request_id: "synthetic-wrap-plan-approval".into(),
            plan,
            valid_for_seconds: 600,
            replaces_revision_id: None,
            expected_review_hash: None,
        };
        let review = engine
            .preview_financial_plan(request.clone())
            .await
            .unwrap();
        request.expected_review_hash = Some(review.review_hash);
        let approved = engine.authorize_financial_plan(request).await.unwrap();
        let record = engine
            .execute_financial_plan_step(
                &approved.plan_id,
                "wrap",
                OperationRequest {
                    request_id: "synthetic-wrap-plan-attempt".into(),
                    operation: "wallet.wrap".into(),
                    input,
                    run_id: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(record.status, Status::Completed, "{:?}", record.error);
        assert!(record.effects.is_some());
        assert_eq!(
            record.preview.as_ref().unwrap().binding
                [fin_domain::financial_plans::PLAN_METADATA_KEY]["plan_id"],
            approved.plan_id
        );
        let status = engine
            .financial_plan_status(&approved.plan_id)
            .await
            .unwrap();
        assert_eq!(
            status.steps[0].state,
            fin_domain::financial_plans::FinancialPlanStepProgressState::Completed
        );
        assert!(
            status
                .usage
                .verified_proceeds
                .iter()
                .any(|proceeds| { proceeds.asset == weth && proceeds.available == amount.atoms })
        );
        assert_eq!(authorization.0.load(Ordering::SeqCst), 1);
        assert_eq!(signer.calls.load(Ordering::SeqCst), 1);
        assert_eq!(dispatch.calls.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn ordinary_new_chain_transfers_enforce_personal_allocation_and_movement_before_approval()
    {
        use fin_engine::{Engine, EngineOptions};
        use fin_store::Store;
        for chain in [42161, 4663] {
            for token in ["native".into(), lower_address(addr(101))] {
                for excessive_movement in [false, true] {
                    let rpc = Arc::new(Fixture::new());
                    let signer = Arc::new(IsolatedSigningSeam::default());
                    let prices = Arc::new(EnginePrices::default());
                    let mut adapter =
                        wallet_adapter(chain, rpc.clone()).with_wallet_valuation(prices.clone());
                    adapter.runtime =
                        Arc::new(EvmRuntime::new(rpc.clone(), chain, Some(signer.clone())));
                    let authorization = Arc::new(IsolatedAuthorization {
                        calls: AtomicUsize::new(0),
                        decline: false,
                    });
                    let engine = Engine::new(
                        Store::in_memory().unwrap(),
                        vec![Arc::new(adapter)],
                        authorization.clone(),
                        EngineOptions {
                            max_movement_and_cost_usd_micros: "100000000".into(),
                            allow_unpriced_exchange_assets: false,
                            max_run_allocation_usd_micros: if excessive_movement {
                                "5000000000"
                            } else {
                                "1000000"
                            }
                            .into(),
                        },
                    )
                    .unwrap();
                    if excessive_movement {
                        prices.0.store(110, Ordering::SeqCst);
                    }
                    let rejected = engine
                        .execute(OperationRequest {
                            request_id: "synthetic-personal-budget".into(),
                            operation: "wallet.transfer".into(),
                            input: transfer(chain, &token),
                            run_id: None,
                        })
                        .await
                        .unwrap();
                    assert_eq!(
                        rejected.error.as_ref().unwrap().code,
                        if excessive_movement {
                            "MOVEMENT_LIMIT"
                        } else {
                            "ALLOCATION_LIMIT"
                        }
                    );
                    assert!(rejected.approval.is_none() && rejected.submission.is_none());
                    assert_eq!(authorization.calls.load(Ordering::SeqCst), 0);
                    assert_eq!(signer.0.load(Ordering::SeqCst), 0);
                    assert!(
                        !rpc.calls
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|method| method == "eth_sendRawTransaction")
                    );
                }
            }
        }
    }
    struct FinalityRpc {
        inner: Arc<Fixture>,
        mode: AtomicU64,
        finalized_seen: AtomicU64,
    }
    #[async_trait]
    impl EvmRpc for FinalityRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            if method == "eth_getBlockByNumber" && params[0] == json!("finalized") {
                self.finalized_seen.store(1, Ordering::SeqCst);
                return match self.mode.load(Ordering::SeqCst) {
                    0 => Ok(Value::Null),
                    1 => Err(error(
                        "SYNTHETIC_FINALITY_UNSUPPORTED",
                        "read",
                        "No finalized tag.",
                    )),
                    2 => Ok(json!({"number":"0x14","hash":blockhash(20)})),
                    3 => Ok(json!({"number":"0x16","hash":"invalid"})),
                    _ => Ok(json!({"number":"0x16","hash":blockhash(22)})),
                };
            }
            if self.mode.load(Ordering::SeqCst) == 5
                && self.finalized_seen.load(Ordering::SeqCst) == 1
                && method == "eth_getBlockByNumber"
                && params[0] == json!("0x15")
            {
                return Ok(json!({"number":"0x15","hash":blockhash(99)}));
            }
            self.inner.request(method, params).await
        }
    }
    #[tokio::test]
    async fn finalized_evidence_is_required_and_never_settles_missing_lagging_or_changed_inclusion()
    {
        for chain in [42161, 4663] {
            let rpc = Arc::new(Fixture::new());
            let mut a = wallet_adapter(chain, rpc.clone())
                .with_wallet_valuation(Arc::new(Prices(AtomicU64::new(3_000_000_000))));
            // Direct callers may retain an explicit stricter policy. The
            // reviewed broker wallet defaults no longer enable this path.
            a.config.confirmations = 2;
            a.config.require_finalized = true;
            let finality = Arc::new(FinalityRpc {
                inner: rpc.clone(),
                mode: AtomicU64::new(0),
                finalized_seen: AtomicU64::new(0),
            });
            a.runtime = Arc::new(EvmRuntime::new(finality.clone(), chain, None));
            let prepared = a
                .prepare("wallet.transfer", &transfer(chain, "native"))
                .await
                .unwrap();
            let binding: Binding = serde_json::from_value(prepared.binding.clone()).unwrap();
            rpc.latest.store(22, Ordering::SeqCst);
            rpc.set_receipt(&binding, 21, vec![]);
            let record = record(prepared);
            for mode in 0..4 {
                finality.mode.store(mode, Ordering::SeqCst);
                let result = a.reconcile(&record).await.unwrap();
                assert_eq!(result.status, Status::Pending);
                assert!(result.verified.is_none());
            }
            finality.mode.store(5, Ordering::SeqCst);
            assert_eq!(a.reconcile(&record).await.unwrap().status, Status::Pending);
            finality.mode.store(4, Ordering::SeqCst);
            let result = a.reconcile(&record).await.unwrap();
            assert_eq!(result.status, Status::Completed);
            assert_eq!(result.evidence["finality"], json!("rpc-finalized"));
        }
    }

    #[tokio::test]
    async fn reviewed_l2_wallet_default_settles_sequenced_receipt_without_finalized_rpc() {
        for chain in [42161, 4663] {
            let rpc = Arc::new(Fixture::new());
            let mut a = wallet_adapter(chain, rpc.clone())
                .with_wallet_valuation(Arc::new(Prices(AtomicU64::new(3_000_000_000))));
            let finality = Arc::new(FinalityRpc {
                inner: rpc.clone(),
                mode: AtomicU64::new(1), // finalized tag unavailable
                finalized_seen: AtomicU64::new(0),
            });
            a.runtime = Arc::new(EvmRuntime::new(finality.clone(), chain, None));
            let prepared = a
                .prepare("wallet.transfer", &transfer(chain, "native"))
                .await
                .unwrap();
            let binding: Binding = serde_json::from_value(prepared.binding.clone()).unwrap();
            rpc.latest.store(21, Ordering::SeqCst); // Exactly one sequencer block.
            rpc.set_receipt(&binding, 21, vec![]);
            let result = a.reconcile(&record(prepared)).await.unwrap();
            assert_eq!(result.status, Status::Completed, "{chain}");
            assert_eq!(result.evidence["finality"], json!("sequencer-inclusion"));
            assert_eq!(result.evidence["confirmations"], json!(1));
            assert_eq!(finality.finalized_seen.load(Ordering::SeqCst), 0);
        }
    }
    #[tokio::test]
    async fn both_networks_reconcile_original_native_and_erc20_receipts_and_reject_reorg() {
        for chain in [42161, 4663] {
            for token in ["native".into(), lower_address(addr(101))] {
                let rpc = Arc::new(Fixture::new());
                let a = wallet_adapter(chain, rpc.clone())
                    .with_wallet_valuation(Arc::new(Prices(AtomicU64::new(3_000_000_000))));
                let prepared = a
                    .prepare("wallet.transfer", &transfer(chain, &token))
                    .await
                    .unwrap();
                let binding: Binding = serde_json::from_value(prepared.binding.clone()).unwrap();
                let logs = if token == "native" {
                    vec![]
                } else {
                    vec![event(
                        addr(101),
                        Erc20::Transfer {
                            from: addr(1),
                            to: addr(2),
                            value: fin_evm::u256(&binding.units).unwrap(),
                        },
                        21,
                    )]
                };
                rpc.latest.store(22, Ordering::SeqCst);
                rpc.set_receipt(&binding, 21, logs);
                let record = record(prepared);
                assert_eq!(
                    a.reconcile(&record).await.unwrap().status,
                    Status::Completed
                );
                rpc.receipt.lock().unwrap()["blockHash"] = json!(blockhash(99));
                assert_ne!(
                    a.reconcile(&record).await.unwrap().status,
                    Status::Completed
                );
            }
        }
    }
}

#[path = "base_aave_tests.rs"]
mod base_aave_tests;

#[path = "base_fee_tests.rs"]
mod base_fee_tests;
