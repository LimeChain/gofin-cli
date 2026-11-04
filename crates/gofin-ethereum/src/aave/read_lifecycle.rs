//! Native Aave read facts and pure presentation, exposed through typed phases.
use super::super::*;
#[path = "read_batch.rs"]
mod batch;
use crate::{
    abi::Pool,
    reads::{Reserve, UserReserve},
};
use gofin_domain::operation_lifecycle::{
    CandidateResult, LifecycleSession, OperationLifecycle, TypedSession,
};
use gofin_evm::{block_tag, quantity, rpc_u64};

pub(crate) struct Markets;
pub(crate) struct Reserves;
pub(crate) struct Positions;
#[derive(Clone)]
pub(crate) struct ReadInput {
    raw: Value,
    request: request::Request,
}
pub(crate) struct MarketsInput(ReadInput);
pub(crate) struct ReservesInput(ReadInput);
pub(crate) struct PositionsInput(ReadInput);
#[derive(Clone)]
pub(crate) struct Capture {
    operation: &'static str,
    chain_id: u64,
    owner: Address,
    block: u64,
    hash: String,
    observed_at: String,
    historical: bool,
}
#[derive(Clone, Serialize)]
pub(crate) struct ReadOutput<T> {
    operation: &'static str,
    resource: ResourceId,
    chain_id: u64,
    observed_at: String,
    block: u64,
    data: T,
    discovery: Option<Value>,
    block_hash: String,
    summary: String,
}
fn output<T>(capture: &Capture, data: T, summary: String) -> ReadOutput<T> {
    ReadOutput {
        operation: capture.operation,
        resource: resource(capture.chain_id, capture.owner),
        chain_id: capture.chain_id,
        observed_at: capture.observed_at.clone(),
        block: capture.block,
        data,
        discovery: None,
        block_hash: capture.hash.clone(),
        summary: if capture.historical {
            format!("State at block {}: {summary}", capture.block)
        } else {
            summary
        },
    }
}
pub(crate) struct MarketObservation {
    config: AaveConfig,
    profile: Option<aave::MarketProfile>,
    horizon: bool,
    provider: Result<Address>,
    revision: Result<U256>,
}
#[derive(Clone)]
pub(crate) struct HeaderFact {
    block: u64,
    hash: String,
}
pub(crate) struct MarketsEvidence {
    capture: Capture,
    default_pool: Option<String>,
    markets: Vec<MarketObservation>,
}
pub(crate) struct ReservesEvidence {
    capture: Capture,
    facts: batch::RowsFacts,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct PositionRow {
    #[serde(flatten)]
    user: UserReserve,
    scaled_supply: Quantity,
    supply_previous_index: Quantity,
    debt_previous_index: Quantity,
    reserve: Reserve,
    wallet_balance: Quantity,
    pool_allowance: Quantity,
    historical_principal: Option<Quantity>,
    historical_interest: Option<Quantity>,
    interest_attribution: String,
}
pub(crate) struct PositionsEvidence {
    capture: Capture,
    facts: batch::RowsFacts,
    all_reserves: bool,
}
#[derive(Clone, Serialize)]
pub(crate) struct MarketRow {
    pool: String,
    oracle: String,
    profile: Option<aave::MarketProfile>,
    backing_source_commit: Option<&'static str>,
    pool_revision: Option<Quantity>,
    risk_revision_compatible: bool,
    provider_verified: bool,
    error: Option<GoFinError>,
    supported_operations: [&'static str; 7],
    write_compatibility: &'static str,
}
#[derive(Clone, Serialize)]
pub(crate) struct MarketsData {
    version: &'static str,
    markets: Vec<MarketRow>,
    available: bool,
    unavailable_reason: Option<&'static str>,
    address_book_commit: &'static str,
    unsupported: [&'static str; 6],
}
#[derive(Clone, Serialize)]
pub(crate) struct AccountData {
    collateral_base: Quantity,
    debt_base: Quantity,
    available_borrow_base: Quantity,
    liquidation_threshold_bps: Quantity,
    health_factor: Quantity,
}
#[derive(Clone, Serialize)]
pub(crate) struct PositionsData {
    positions: Vec<PositionRow>,
    emode_category: Quantity,
    account: AccountData,
    all_reserves: bool,
}

impl EthereumAdapter {
    async fn native_read_capture(
        &self,
        operation: &'static str,
        input: &ReadInput,
    ) -> Result<(Self, Capture)> {
        let selected = self
            .selected_market(&input.raw)?
            .unwrap_or_else(|| self.clone());
        selected.require_operation(operation)?;
        let wallet = selected.resolve(&input.request, false)?;
        let owner = address(&wallet.address)?;
        selected.runtime.assert_chain().await?;
        let block = if let Some(block) = input.request.block_number {
            block
        } else {
            rpc_u64(
                &selected
                    .runtime
                    .rpc
                    .request("eth_blockNumber", json!([]))
                    .await?,
            )?
        };
        let header = selected
            .runtime
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([block_tag(Some(block)), false]),
            )
            .await?;
        let hash = header["hash"].as_str().unwrap_or("").to_owned();
        gofin_evm::hash(&hash)?;
        require(
            rpc_u64(&header["number"])? == block,
            "AAVE_READ_CHANGED",
            "Aave read block identity differs.",
        )?;
        let capture = Capture {
            operation,
            chain_id: selected.config.chain_id,
            owner,
            block,
            hash,
            observed_at: selected.clock.now().to_rfc3339(),
            historical: input.request.block_number.is_some(),
        };
        // Share only this capture's pinned observations across collection rounds.
        let mut reader = selected.clone();
        reader.runtime = Arc::new(EvmRuntime::new(
            Arc::new(snapshot::SnapshotRpc::new(
                selected.runtime.rpc.clone(),
                block,
                snapshot::SNAPSHOT_CONCURRENCY,
            )),
            selected.config.chain_id,
            None,
        ));
        Ok((reader, capture))
    }
    async fn collect_markets(&self, input: &MarketsInput) -> Result<MarketsEvidence> {
        let (adapter, capture) = self.native_read_capture("aave.markets", &input.0).await?;
        use crate::{
            abi::AddressesProvider,
            collection::{ReadCall, ReadCollector},
        };
        let configs = adapter.market_configs();
        let mut collector = ReadCollector::new(&adapter, capture.block).await;
        let mut calls = Vec::new();
        for config in &configs {
            let pool = address(&config.pool)?;
            calls.push(ReadCall::new(pool, &Pool::ADDRESSES_PROVIDERCall {}));
            calls.push(ReadCall::new(pool, &Pool::POOL_REVISIONCall {}));
        }
        let mut values = collector.collect(calls).await.into_iter();
        let mut markets = Vec::new();
        for config in configs {
            let mut raw = input.0.raw.clone();
            raw["market"] = json!(config.pool);
            let selected = adapter
                .selected_market(&raw)?
                .unwrap_or_else(|| adapter.clone());
            let profile = aave::reviewed_markets(adapter.config.chain_id)
                .into_iter()
                .find(|p| p.pool == config.pool);
            let mut provider = batch::next::<Pool::ADDRESSES_PROVIDERCall>(&mut values);
            let revision = batch::next::<Pool::POOL_REVISIONCall>(&mut values);
            let pool = address(&config.pool)?;
            let oracle = address(&config.oracle)?;
            if let Err(e) = selected.runtime.code_at(pool, Some(capture.block)).await {
                provider = Err(e);
            } else if let Err(e) = selected.runtime.code_at(oracle, Some(capture.block)).await {
                provider = Err(e);
            } else if let Ok(value) = &provider {
                if let Err(e) = selected.runtime.code_at(*value, Some(capture.block)).await {
                    provider = Err(e);
                } else if let Some(reviewed) = &profile
                    && *value != address(&reviewed.addresses_provider)?
                {
                    provider = Err(error(
                        "DEPLOYMENT_IDENTITY_MISMATCH",
                        "read",
                        "Pool provider differs from the reviewed market identity.",
                    ));
                }
            }
            markets.push(MarketObservation {
                config,
                profile,
                horizon: selected.horizon_math(),
                provider,
                revision,
            });
        }
        let mut calls = Vec::new();
        for market in &markets {
            if let Ok(provider) = market.provider {
                calls.extend([
                    ReadCall::new(provider, &AddressesProvider::getPoolCall {}),
                    ReadCall::new(provider, &AddressesProvider::getPriceOracleCall {}),
                    ReadCall::new(provider, &AddressesProvider::getPoolDataProviderCall {}),
                ]);
            }
        }
        let mut values = collector.collect(calls).await.into_iter();
        for market in &mut markets {
            if market.provider.is_err() {
                continue;
            }
            let pool = batch::next::<AddressesProvider::getPoolCall>(&mut values);
            let oracle = batch::next::<AddressesProvider::getPriceOracleCall>(&mut values);
            let data = batch::next::<AddressesProvider::getPoolDataProviderCall>(&mut values);
            market.provider = match (pool, oracle, data) {
                (Ok(pool), Ok(oracle), data)
                    if pool == address(&market.config.pool)?
                        && oracle == address(&market.config.oracle)? =>
                {
                    match data {
                        Ok(data) => adapter
                            .runtime
                            .code_at(data, Some(capture.block))
                            .await
                            .map(|_| data),
                        Err(e) => Err(e),
                    }
                }
                (Err(e), _, _) | (_, Err(e), _) => Err(e),
                _ => Err(error(
                    "DEPLOYMENT_IDENTITY_MISMATCH",
                    "read",
                    "Aave provider disagrees with configured pool or oracle.",
                )),
            };
        }
        Ok(MarketsEvidence {
            default_pool: adapter.config.aave.as_ref().map(|a| a.pool.clone()),
            capture,
            markets,
        })
    }
    async fn collect_reserves(&self, input: &ReservesInput) -> Result<ReservesEvidence> {
        let (adapter, capture) = self.native_read_capture("aave.reserves", &input.0).await?;
        let facts =
            batch::collect(&adapter, &capture, input.0.request.tokens.as_ref(), false).await?;
        Ok(ReservesEvidence { capture, facts })
    }
    async fn collect_positions(&self, input: &PositionsInput) -> Result<PositionsEvidence> {
        let (adapter, capture) = self.native_read_capture("aave.positions", &input.0).await?;
        let facts =
            batch::collect(&adapter, &capture, input.0.request.tokens.as_ref(), true).await?;
        Ok(PositionsEvidence {
            capture,
            facts,
            all_reserves: input.0.request.tokens.is_none(),
        })
    }

    pub(crate) fn aave_read_lifecycle<'a>(
        &'a self,
        operation: &str,
        input: &Value,
    ) -> Result<Option<Box<dyn LifecycleSession + 'a>>> {
        let request = match operation {
            "aave.markets" | "aave.reserves" | "aave.positions" => {
                self.require_operation(operation)?;
                request::parse(operation, input)?
            }
            _ => return Ok(None),
        };
        let input = ReadInput {
            raw: input.clone(),
            request,
        };
        Ok(Some(match operation {
            "aave.markets" => Box::new(TypedSession::<_, Markets>::new(self, MarketsInput(input))),
            "aave.reserves" => {
                Box::new(TypedSession::<_, Reserves>::new(self, ReservesInput(input)))
            }
            _ => Box::new(TypedSession::<_, Positions>::new(
                self,
                PositionsInput(input),
            )),
        }))
    }
}
pub(crate) use batch::RowsFacts as WriteReserveFacts;
pub(crate) async fn collect_write_reserve(
    adapter: &EthereumAdapter,
    block: u64,
    owner: Address,
    token: &str,
    collector: &mut crate::collection::ReadCollector,
) -> Result<WriteReserveFacts> {
    let capture = Capture {
        operation: "aave.reserves",
        chain_id: adapter.config.chain_id,
        owner,
        block,
        hash: String::new(),
        observed_at: String::new(),
        historical: false,
    };
    batch::collect_with_collector(
        adapter,
        &capture,
        Some(&vec![token.to_owned()]),
        false,
        collector,
    )
    .await
}
pub(crate) fn evaluate_write_reserve(facts: &WriteReserveFacts) -> Result<Reserve> {
    let (mut reserves, _) = batch::evaluate(facts)?;
    require(
        reserves.len() == 1,
        "AAVE_READ_INCOMPLETE",
        "Expected one reserve observation.",
    )?;
    Ok(reserves.remove(0))
}
fn evaluate_markets(e: &MarketsEvidence) -> Result<ReadOutput<MarketsData>> {
    let markets=e.markets.iter().map(|m|MarketRow{pool:m.config.pool.clone(),oracle:m.config.oracle.clone(),profile:m.profile.clone(),backing_source_commit:if m.horizon{Some(aave::math::HORIZON_SOURCE_COMMIT)}else{None},pool_revision:m.revision.as_ref().ok().copied().map(quantity),risk_revision_compatible:m.revision.as_ref().is_ok_and(|r|if m.horizon {*r==U256::from(7)}else{[U256::from(10),U256::from(11)].contains(r)}),provider_verified:m.provider.is_ok(),error:m.provider.as_ref().err().cloned(),supported_operations:["aave.reserves","aave.positions","aave.supply","aave.borrow","aave.repay","aave.withdraw","aave.collateral"],write_compatibility:"Selected reserve token revisions, access restrictions and current protocol state are checked before dispatch."}).collect::<Vec<_>>();
    let available = !markets.is_empty();
    let summary = if available {
        format!(
            "{} enabled Aave V3 markets; default {}. Next: inspect aave.reserves for current eligibility and caps.",
            markets.len(),
            e.default_pool.as_deref().unwrap_or("unavailable")
        )
    } else {
        "No reviewed compatible Aave V3 market on this network.".into()
    };
    Ok(output(
        &e.capture,
        MarketsData {
            version: "v3",
            markets,
            available,
            unavailable_reason: if available {
                None
            } else {
                Some("No reviewed compatible Aave V3 deployment on this network.")
            },
            address_book_commit: aave::ADDRESS_BOOK_COMMIT,
            unsupported: [
                "v2",
                "v4",
                "stable-rate writes",
                "eMode changes",
                "delegation",
                "gateway flows",
            ],
        },
        summary,
    ))
}
fn evaluate_reserves(e: &ReservesEvidence) -> Result<ReadOutput<Vec<Reserve>>> {
    let (reserves, _) = batch::evaluate(&e.facts)?;
    let blocked = reserves
        .iter()
        .filter(|r| r.paused || r.frozen || !r.active)
        .count();
    let summary = format!(
        "{} exact Aave reserves checked; {blocked} paused, frozen or inactive. Caps, liquidity and permissions are checked again before each write. Next: use --json for token identities and current restrictions.",
        reserves.len()
    );
    Ok(output(&e.capture, reserves, summary))
}
fn evaluate_positions(e: &PositionsEvidence) -> Result<ReadOutput<PositionsData>> {
    let (_, rows) = batch::evaluate(&e.facts)?;
    let account = e.facts.account.as_ref().expect("positions account");
    let rows_json = rows
        .iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| GoFinError::validation("Position presentation failed."))?;
    let summary = aave::reads::position_summary(
        &rows_json,
        &quantity(account.totalDebtBase),
        &quantity(account.healthFactor),
    );
    Ok(output(
        &e.capture,
        PositionsData {
            positions: rows,
            emode_category: quantity(e.facts.mode.expect("positions mode")),
            account: AccountData {
                collateral_base: quantity(account.totalCollateralBase),
                debt_base: quantity(account.totalDebtBase),
                available_borrow_base: quantity(account.availableBorrowsBase),
                liquidation_threshold_bps: quantity(account.currentLiquidationThreshold),
                health_factor: quantity(account.healthFactor),
            },
            all_reserves: e.all_reserves,
        },
        summary,
    ))
}

fn read_only<T>() -> Result<T> {
    Err(GoFinError::validation(
        "A native read has no authorization, execution or submission observation.",
    ))
}
macro_rules! native_read {
    ($op:ty,$input:ty,$evidence:ty,$data:ty,$collect:ident,$evaluate:ident) => {
        #[async_trait]
        impl OperationLifecycle<$op> for EthereumAdapter {
            type Input = $input;
            type Evidence = $evidence;
            type Candidate = ReadOutput<$data>;
            type Prepared = HeaderFact;
            type Simulated = ReadOutput<$data>;
            type ExecutionRef = Submission;
            type Observation = ();
            type Output = ReadOutput<$data>;
            async fn collect(&self, input: &Self::Input) -> Result<Self::Evidence> {
                self.$collect(input).await
            }
            fn evaluate(
                _input: &Self::Input,
                evidence: &Self::Evidence,
            ) -> Result<Self::Candidate> {
                $evaluate(evidence)
            }
            async fn prepare(
                &self,
                _input: &Self::Input,
                candidate: &Self::Candidate,
            ) -> Result<HeaderFact> {
                let header = self
                    .runtime
                    .rpc
                    .request(
                        "eth_getBlockByNumber",
                        json!([block_tag(Some(candidate.block)), false]),
                    )
                    .await?;
                let hash = header["hash"].as_str().unwrap_or("").to_owned();
                gofin_evm::hash(&hash)?;
                Ok(HeaderFact {
                    block: rpc_u64(&header["number"])?,
                    hash,
                })
            }
            async fn simulate(
                &self,
                _input: &Self::Input,
                candidate: &Self::Candidate,
                header: &HeaderFact,
            ) -> Result<Self::Simulated> {
                require(
                    header.block == candidate.block && header.hash == candidate.block_hash,
                    "AAVE_READ_CHANGED",
                    "Aave read block identity changed.",
                )?;
                Ok(candidate.clone())
            }
            fn candidate_result(
                candidate: &Self::Simulated,
            ) -> Result<CandidateResult<Self::Output>> {
                Ok(CandidateResult::Read(candidate.clone()))
            }
            fn output_value(output: Self::Output) -> Result<Value> {
                serde_json::to_value(output)
                    .map_err(|_| GoFinError::validation("Native read presentation failed."))
            }
            async fn execute(
                &self,
                _: &Self::Input,
                _: &gofin_domain::operation_lifecycle::AuthorizedOperation,
                _: Arc<dyn SubmissionJournal>,
            ) -> Result<Submission> {
                read_only()
            }
            fn submission(_: Submission) -> Result<Submission> {
                read_only()
            }
            async fn observe(&self, _: &Self::Input, _: &OperationRecord) -> Result<()> {
                read_only()
            }
            fn evaluate_observation(
                _: &Self::Input,
                _: &OperationRecord,
                _: &(),
            ) -> Result<Reconciliation> {
                read_only()
            }
        }
    };
}
native_read!(
    Markets,
    MarketsInput,
    MarketsEvidence,
    MarketsData,
    collect_markets,
    evaluate_markets
);
native_read!(
    Reserves,
    ReservesInput,
    ReservesEvidence,
    Vec<Reserve>,
    collect_reserves,
    evaluate_reserves
);
native_read!(
    Positions,
    PositionsInput,
    PositionsEvidence,
    PositionsData,
    collect_positions,
    evaluate_positions
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::Multicall3;
    use gofin_evm::{
        EvmRpc,
        alloy::sol_types::{SolCall, SolValue},
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct BatchRpc {
        inner: Arc<crate::tests::Fixture>,
        aggregates: AtomicUsize,
        canonical: AtomicBool,
        revert: AtomicBool,
        incomplete: AtomicBool,
    }
    #[async_trait]
    impl EvmRpc for BatchRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            let multicall = address(crate::collection::MULTICALL)?;
            if method == "eth_getCode" && params[0] == json!(lower_address(multicall)) {
                return Ok(json!(if self.canonical.load(Ordering::SeqCst) {
                    format!(
                        "0x{}",
                        include_str!("../multicall/multicall3-runtime.hex").trim()
                    )
                } else {
                    "0x6000".into()
                }));
            }
            if method == "eth_call" && params[0]["to"] == json!(lower_address(multicall)) {
                self.aggregates.fetch_add(1, Ordering::SeqCst);
                let data = gofin_evm::rpc_bytes(&params[0]["data"])?;
                let calls = Multicall3::aggregate3Call::abi_decode_validate(&data)
                    .map_err(|_| GoFinError::validation("invalid aggregate fixture"))?
                    .calls;
                let mut values = Vec::new();
                for (index, call) in calls.into_iter().enumerate() {
                    if index == 0 && self.revert.load(Ordering::SeqCst) {
                        values.push(Multicall3::Result3 {
                            success: false,
                            returnData: Vec::new().into(),
                        });
                        continue;
                    }
                    let result=self.inner.request("eth_call",json!([{"to":lower_address(call.target),"data":gofin_evm::hex_bytes(&call.callData)},params[1]])).await;
                    values.push(match result {
                        Ok(raw) => Multicall3::Result3 {
                            success: true,
                            returnData: gofin_evm::rpc_bytes(&raw)?,
                        },
                        Err(_) => Multicall3::Result3 {
                            success: false,
                            returnData: Vec::new().into(),
                        },
                    });
                }
                if self.incomplete.load(Ordering::SeqCst) {
                    values.pop();
                }
                return Ok(json!(gofin_evm::hex_bytes(&values.abi_encode())));
            }
            self.inner.request(method, params).await
        }
    }
    fn adapter(rpc: Arc<BatchRpc>) -> EthereumAdapter {
        let mut adapter = crate::tests::adapter(rpc.inner.clone());
        adapter.runtime = Arc::new(EvmRuntime::new(rpc, 1, None));
        adapter
    }
    fn fixture(canonical: bool) -> Arc<BatchRpc> {
        Arc::new(BatchRpc {
            inner: Arc::new(crate::tests::Fixture::new()),
            aggregates: AtomicUsize::new(0),
            canonical: AtomicBool::new(canonical),
            revert: AtomicBool::new(false),
            incomplete: AtomicBool::new(false),
        })
    }
    #[tokio::test]
    async fn native_reads_match_legacy_results_with_verified_batch_and_direct_fallback() {
        for operation in ["aave.markets", "aave.reserves", "aave.positions"] {
            for scoped in [false, true] {
                if operation == "aave.markets" && scoped {
                    continue;
                }
                let rpc = fixture(true);
                let adapter = adapter(rpc.clone());
                let mut input = json!({"wallet":"synthetic","network":"ethereum","chain_id":1});
                if scoped {
                    input["tokens"] = json!([lower_address(crate::tests::addr(101))]);
                }
                let expected = adapter.read_operation(operation, &input).await.unwrap();
                let actual = adapter.read(operation, &input).await.unwrap();
                assert_eq!(
                    actual["data"], expected["data"],
                    "{operation} scoped={scoped}"
                );
                assert_eq!(actual["summary"], expected["summary"]);
                assert_eq!(actual["block_hash"], expected["block_hash"]);
                assert!(rpc.aggregates.load(Ordering::SeqCst) > 0);
                rpc.canonical.store(false, Ordering::SeqCst);
                let fallback = adapter.read(operation, &input).await.unwrap();
                assert_eq!(fallback["data"], expected["data"]);
            }
        }
    }
    #[tokio::test]
    async fn native_reads_fail_closed_on_reverted_or_missing_batch_results() {
        for incomplete in [false, true] {
            let rpc = fixture(true);
            rpc.revert.store(!incomplete, Ordering::SeqCst);
            rpc.incomplete.store(incomplete, Ordering::SeqCst);
            let a = adapter(rpc);
            let error=a.read("aave.reserves",&json!({"wallet":"synthetic","network":"ethereum","chain_id":1,"tokens":[lower_address(crate::tests::addr(101))]})).await.unwrap_err();
            assert_eq!(
                error.code,
                if incomplete {
                    "MULTICALL_INCOMPLETE"
                } else {
                    "TOKEN_CALL_FAILED"
                }
            );
        }
    }
}
