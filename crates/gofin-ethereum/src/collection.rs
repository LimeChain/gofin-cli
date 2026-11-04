//! Explicit, pinned, sender-independent EVM reads. Callers own ABI decoding.
//! Only use this for reviewed calls whose result does not depend on msg.sender.
use super::*;
use abi::Multicall3;
use gofin_evm::{
    RpcReadRequirements,
    alloy::{primitives::keccak256, sol_types::SolCall},
    rpc_bytes,
};
use tokio::task::JoinSet;

pub(crate) const MULTICALL: &str = "0xca11bde05977b3631167028862be2a173976ca11";
pub(crate) const BATCH_SIZE: usize = 32;
pub(crate) const CONCURRENCY: usize = 4;
pub(crate) const DIRECT_CONCURRENCY: usize = 8;

/// Compiled deployment trust for the reviewed Aave networks. This registry is
/// not configurable from operation input or provider responses. Unknown chains
/// retain direct reads; an unavailable aggregate falls back through the normal
/// direct-call path. Other consumers still verify runtime bytecode in `new`.
const AAVE_MULTICALL_DEPLOYMENTS: &[(u64, &str)] =
    &[(1, MULTICALL), (8453, MULTICALL), (42161, MULTICALL)];

#[derive(Clone)]
pub(crate) struct ReadCall {
    pub(crate) target: Address,
    pub(crate) data: Vec<u8>,
}
impl ReadCall {
    pub(crate) fn new<C: SolCall>(target: Address, call: &C) -> Self {
        Self {
            target,
            data: call.abi_encode(),
        }
    }
}

/// Counts collector-level work, not transport retries or physical endpoint attempts.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CollectionStats {
    pub(crate) logical_calls: usize,
    pub(crate) aggregate_requests: usize,
    pub(crate) direct_requests: usize,
    pub(crate) fallback_chunks: usize,
}

/// One capture's trusted batch endpoint and ordered read phases.
/// Invalid/unavailable Multicall code (for runtime-verified consumers) and failed or malformed aggregate envelopes
/// fall back to direct calls. A decoded envelope with a wrong result count fails
/// explicitly. A target revert is returned as an individual error;
/// it is never replaced by a zero or retried as if the aggregate were unavailable.
pub(crate) struct ReadCollector {
    adapter: EthereumAdapter,
    block: u64,
    multicall: Option<Address>,
    reads: BTreeMap<(Address, Vec<u8>), Result<Vec<u8>>>,
    pub(crate) stats: CollectionStats,
}
impl ReadCollector {
    pub(crate) async fn new(adapter: &EthereumAdapter, block: u64) -> Self {
        let multicall = Self::verified_multicall(adapter, block).await;
        Self::with_multicall(adapter, block, multicall)
    }

    /// Aave uses explicit local deployment trust, without a runtime code probe.
    pub(crate) fn for_aave(adapter: &EthereumAdapter, block: u64) -> Self {
        let multicall = AAVE_MULTICALL_DEPLOYMENTS
            .iter()
            .find(|(chain, _)| {
                *chain == adapter.config.chain_id && *chain == adapter.runtime.chain_id
            })
            .and_then(|(_, deployment)| address(deployment).ok());
        Self::with_multicall(adapter, block, multicall)
    }

    fn with_multicall(adapter: &EthereumAdapter, block: u64, multicall: Option<Address>) -> Self {
        Self {
            adapter: adapter.clone(),
            block,
            multicall,
            reads: BTreeMap::new(),
            stats: CollectionStats::default(),
        }
    }
    pub(crate) fn block(&self) -> u64 {
        self.block
    }
    async fn verified_multicall(adapter: &EthereumAdapter, block: u64) -> Option<Address> {
        if !matches!(adapter.config.chain_id, 1 | 8453 | 42161 | 4663) {
            return None;
        }
        let contract = address(MULTICALL).ok()?;
        let raw = adapter
            .runtime
            .rpc
            .request_checked(
                "eth_getCode",
                json!([MULTICALL, gofin_evm::block_tag(Some(block))]),
                RpcReadRequirements {
                    pin_block: Some(block),
                    ..Default::default()
                },
            )
            .await
            .ok()?;
        let code = rpc_bytes(&raw).ok()?;
        let canonical =
            hex::decode(include_str!("multicall/multicall3-runtime.hex").trim()).ok()?;
        (keccak256(&code) == keccak256(&canonical)).then_some(contract)
    }
    async fn direct_call(adapter: &EthereumAdapter, call: ReadCall, block: u64) -> Result<Vec<u8>> {
        let raw = adapter.runtime.rpc.request_checked("eth_call", json!([{"to":lower_address(call.target),"data":gofin_evm::hex_bytes(&call.data)},gofin_evm::block_tag(Some(block))]), RpcReadRequirements { pin_block: Some(block), ..Default::default() }).await?;
        rpc_bytes(&raw).map(|bytes| bytes.to_vec())
    }
    async fn direct_calls(&mut self, calls: Vec<ReadCall>) -> Vec<Result<Vec<u8>>> {
        self.stats.direct_requests += calls.len();
        let mut output = Vec::with_capacity(calls.len());
        for chunk in calls.chunks(DIRECT_CONCURRENCY) {
            let mut jobs = JoinSet::new();
            for (i, call) in chunk.iter().cloned().enumerate() {
                let adapter = self.adapter.clone();
                let block = self.block;
                jobs.spawn(async move { (i, Self::direct_call(&adapter, call, block).await) });
            }
            let mut part: Vec<_> = (0..chunk.len())
                .map(|_| {
                    Err(error(
                        "HOLDINGS_READ_FAILED",
                        "read",
                        "A required read did not complete.",
                    ))
                })
                .collect();
            while let Some(result) = jobs.join_next().await {
                if let Ok((i, value)) = result {
                    part[i] = value;
                }
            }
            output.extend(part);
        }
        output
    }
    async fn aggregate_chunk(
        adapter: &EthereumAdapter,
        calls: Vec<ReadCall>,
        block: u64,
        multicall: Address,
    ) -> Option<Vec<Result<Vec<u8>>>> {
        let request = Multicall3::aggregate3Call {
            calls: calls
                .iter()
                .map(|call| Multicall3::Call3 {
                    target: call.target,
                    allowFailure: true,
                    callData: call.data.clone().into(),
                })
                .collect(),
        };
        let result = adapter
            .runtime
            .call(multicall, &request, Some(block))
            .await
            .ok()?;
        if result.len() != calls.len() {
            return Some(
                (0..calls.len())
                    .map(|_| {
                        Err(error(
                            "MULTICALL_INCOMPLETE",
                            "read",
                            "Multicall returned an unexpected result count.",
                        ))
                    })
                    .collect(),
            );
        }
        Some(
            result
                .into_iter()
                .map(|value| {
                    if value.success {
                        Ok(value.returnData.to_vec())
                    } else {
                        Err(error(
                            "TOKEN_CALL_FAILED",
                            "read",
                            "A target read reverted in Multicall.",
                        ))
                    }
                })
                .collect(),
        )
    }
    pub(crate) async fn collect(&mut self, calls: Vec<ReadCall>) -> Vec<Result<Vec<u8>>> {
        self.stats.logical_calls += calls.len();
        let keys: Vec<_> = calls
            .iter()
            .map(|call| (call.target, call.data.clone()))
            .collect();
        let mut unseen = BTreeSet::new();
        let unique: Vec<_> = calls
            .into_iter()
            .filter(|call| {
                let key = (call.target, call.data.clone());
                !self.reads.contains_key(&key) && unseen.insert(key)
            })
            .collect();
        if !unique.is_empty() {
            let results = self.collect_uncached(unique.clone()).await;
            for (call, result) in unique.into_iter().zip(results) {
                self.reads.insert((call.target, call.data), result);
            }
        }
        keys.into_iter()
            .map(|key| self.reads.get(&key).expect("collected read").clone())
            .collect()
    }
    /// Read one reviewed sender-independent fact through the same pass-local memo.
    pub(crate) async fn call<C: SolCall>(
        &mut self,
        target: Address,
        call: &C,
    ) -> Result<C::Return> {
        let bytes = self
            .collect(vec![ReadCall::new(target, call)])
            .await
            .into_iter()
            .next()
            .expect("one call")?;
        C::abi_decode_returns(&bytes).map_err(|_| {
            error(
                "CONTRACT_DATA_INVALID",
                "read",
                "Contract return does not match the reviewed ABI.",
            )
        })
    }
    async fn collect_uncached(&mut self, calls: Vec<ReadCall>) -> Vec<Result<Vec<u8>>> {
        let Some(multicall) = self.multicall else {
            return self.direct_calls(calls).await;
        };
        let chunks: Vec<_> = calls
            .chunks(BATCH_SIZE)
            .map(|chunk| chunk.to_vec())
            .collect();
        let mut output = Vec::with_capacity(calls.len());
        for group in chunks.chunks(CONCURRENCY) {
            self.stats.aggregate_requests += group.len();
            let mut jobs = JoinSet::new();
            for (i, chunk) in group.iter().cloned().enumerate() {
                let adapter = self.adapter.clone();
                let block = self.block;
                jobs.spawn(async move {
                    (
                        i,
                        Self::aggregate_chunk(&adapter, chunk, block, multicall).await,
                    )
                });
            }
            let mut parts: Vec<Option<Vec<Result<Vec<u8>>>>> =
                (0..group.len()).map(|_| None).collect();
            while let Some(result) = jobs.join_next().await {
                if let Ok((i, value)) = result {
                    parts[i] = value;
                }
            }
            for (chunk, part) in group.iter().zip(parts) {
                match part {
                    Some(part) => output.extend(part),
                    None => {
                        self.stats.fallback_chunks += 1;
                        output.extend(self.direct_calls(chunk.clone()).await);
                    }
                }
            }
        }
        output
    }
}

#[cfg(test)]
mod memo_tests {
    use super::*;
    use gofin_evm::EvmRpc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Probe {
        calls: AtomicUsize,
        fail: bool,
    }
    #[async_trait]
    impl EvmRpc for Probe {
        async fn request(&self, _: &str, _: Value) -> Result<Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(GoFinError::validation("synthetic read failure"))
            } else {
                Ok(json!(gofin_evm::hex_bytes(
                    &U256::from(7).to_be_bytes::<32>()
                )))
            }
        }
    }
    fn collector(probe: Arc<Probe>, block: u64) -> ReadCollector {
        let mut adapter = crate::tests::adapter(Arc::new(crate::tests::Fixture::new()));
        adapter.runtime = Arc::new(EvmRuntime::new(probe, 1, None));
        ReadCollector {
            adapter,
            block,
            multicall: None,
            reads: BTreeMap::new(),
            stats: CollectionStats::default(),
        }
    }
    #[tokio::test]
    async fn exact_facts_are_shared_within_and_across_rounds_but_not_across_passes() {
        let probe = Arc::new(Probe {
            calls: AtomicUsize::new(0),
            fail: false,
        });
        let mut first = collector(probe.clone(), 20);
        let target = Address::from_word(U256::from(3).into());
        let owner = Address::from_word(U256::from(1).into());
        let other_owner = Address::from_word(U256::from(2).into());
        let a = ReadCall::new(target, &abi::Erc20::balanceOfCall { owner });
        let b = ReadCall::new(target, &abi::Erc20::balanceOfCall { owner: other_owner });
        let c = ReadCall::new(owner, &abi::Erc20::balanceOfCall { owner });
        let results = first
            .collect(vec![a.clone(), a.clone(), b.clone(), c])
            .await;
        assert_eq!(results.len(), 4);
        assert_eq!(results[0], results[1]);
        assert!(results.iter().all(Result::is_ok));
        assert_eq!(probe.calls.load(Ordering::SeqCst), 3);
        assert!(
            first
                .collect(vec![b, a.clone()])
                .await
                .iter()
                .all(Result::is_ok)
        );
        assert_eq!(probe.calls.load(Ordering::SeqCst), 3);
        let mut fresh = collector(probe.clone(), 20);
        assert!(fresh.collect(vec![a.clone()]).await[0].is_ok());
        let mut historical = collector(probe.clone(), 19);
        assert!(historical.collect(vec![a]).await[0].is_ok());
        assert_eq!(probe.calls.load(Ordering::SeqCst), 5);
    }
    #[tokio::test]
    async fn repeated_target_failure_is_not_replaced_with_success() {
        let probe = Arc::new(Probe {
            calls: AtomicUsize::new(0),
            fail: true,
        });
        let mut reader = collector(probe.clone(), 20);
        let call = ReadCall::new(Address::ZERO, &abi::Erc20::totalSupplyCall {});
        let first = reader.collect(vec![call.clone(), call.clone()]).await;
        assert!(first.iter().all(Result::is_err));
        assert!(reader.collect(vec![call]).await[0].is_err());
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod trusted_deployment_tests {
    use super::*;
    use gofin_evm::{EvmRpc, alloy::sol_types::SolValue};
    use std::sync::Mutex;

    #[derive(Clone, Copy)]
    enum AggregateReply {
        Valid,
        Unavailable,
        Incomplete,
        RevertedMember,
    }
    struct RegistryProbe {
        requests: Mutex<Vec<(String, Value)>>,
        reply: AggregateReply,
    }
    #[async_trait]
    impl EvmRpc for RegistryProbe {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            self.requests
                .lock()
                .unwrap()
                .push((method.into(), params.clone()));
            if method == "eth_getCode" {
                // Legacy consumers still reject unverified runtime code.
                return Ok(json!("0x"));
            }
            if method != "eth_call" {
                return Err(GoFinError::validation("Unexpected registry fixture RPC."));
            }
            if params[0]["to"] == MULTICALL {
                if matches!(self.reply, AggregateReply::Unavailable) {
                    // A registered address may not exist at an old block. The
                    // malformed/empty aggregate response triggers direct reads.
                    return Ok(json!("0x"));
                }
                let request = Multicall3::aggregate3Call::abi_decode_validate(&rpc_bytes(
                    &params[0]["data"],
                )?)
                .unwrap();
                let mut rows: Vec<_> = request
                    .calls
                    .into_iter()
                    .map(|_| Multicall3::Result3 {
                        success: !matches!(self.reply, AggregateReply::RevertedMember),
                        returnData: U256::from(7).abi_encode().into(),
                    })
                    .collect();
                if matches!(self.reply, AggregateReply::Incomplete) {
                    rows.pop();
                }
                return Ok(json!(gofin_evm::hex_bytes(&rows.abi_encode())));
            }
            Ok(json!(gofin_evm::hex_bytes(&U256::from(7).abi_encode())))
        }
    }
    fn fixture(
        chain: u64,
        runtime_chain: u64,
        reply: AggregateReply,
    ) -> (EthereumAdapter, Arc<RegistryProbe>) {
        let probe = Arc::new(RegistryProbe {
            requests: Mutex::new(vec![]),
            reply,
        });
        let mut adapter = crate::tests::adapter(Arc::new(crate::tests::Fixture::new()));
        adapter.config.chain_id = chain;
        adapter.runtime = Arc::new(EvmRuntime::new(probe.clone(), runtime_chain, None));
        (adapter, probe)
    }
    fn call() -> ReadCall {
        ReadCall::new(crate::tests::addr(101), &abi::Erc20::totalSupplyCall {})
    }
    #[tokio::test]
    async fn aave_registry_is_exact_chain_scoped_and_never_probes_runtime_code() {
        for (chain, runtime_chain, registered) in [
            (1, 1, true),
            (8453, 8453, true),
            (42161, 42161, true),
            (4663, 4663, false),
            (999, 999, false),
            (1, 8453, false),
        ] {
            let (adapter, probe) = fixture(chain, runtime_chain, AggregateReply::Valid);
            let mut collector = ReadCollector::for_aave(&adapter, 20);
            assert!(
                probe.requests.lock().unwrap().is_empty(),
                "construction must be local"
            );
            assert_eq!(
                collector.multicall,
                registered.then(|| address(MULTICALL).unwrap())
            );
            let values = collector.collect(vec![call()]).await;
            assert_eq!(values[0].as_ref().unwrap(), &U256::from(7).abi_encode());
            let requests = probe.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].0, "eth_call");
            assert_eq!(requests[0].1[1], "0x14");
            assert_eq!(requests[0].1[0]["to"] == MULTICALL, registered);
        }
    }
    #[tokio::test]
    async fn unavailable_registered_aggregate_falls_back_but_target_and_shape_failures_do_not() {
        for reply in [
            AggregateReply::Unavailable,
            AggregateReply::Incomplete,
            AggregateReply::RevertedMember,
        ] {
            let (adapter, probe) = fixture(1, 1, reply);
            let mut collector = ReadCollector::for_aave(&adapter, 20);
            let values = collector.collect(vec![call()]).await;
            match reply {
                AggregateReply::Unavailable => {
                    assert!(values[0].is_ok());
                    assert_eq!(collector.stats.fallback_chunks, 1);
                    assert_eq!(collector.stats.direct_requests, 1);
                    assert_eq!(probe.requests.lock().unwrap().len(), 2);
                }
                AggregateReply::Incomplete | AggregateReply::RevertedMember => {
                    assert_eq!(
                        values[0].as_ref().unwrap_err().code,
                        if matches!(reply, AggregateReply::Incomplete) {
                            "MULTICALL_INCOMPLETE"
                        } else {
                            "TOKEN_CALL_FAILED"
                        }
                    );
                    assert_eq!(collector.stats.fallback_chunks, 0);
                    assert_eq!(collector.stats.direct_requests, 0);
                    assert_eq!(probe.requests.lock().unwrap().len(), 1);
                }
                AggregateReply::Valid => unreachable!(),
            }
            assert!(
                probe
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(method, _)| method == "eth_call")
            );
        }
    }
    #[tokio::test]
    async fn non_aave_collector_keeps_its_runtime_verification_policy() {
        let (adapter, probe) = fixture(1, 1, AggregateReply::Valid);
        let mut collector = ReadCollector::new(&adapter, 20).await;
        assert!(collector.multicall.is_none());
        assert!(collector.collect(vec![call()]).await[0].is_ok());
        let requests = probe.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0, "eth_getCode");
        assert_eq!(requests[0].1[0], MULTICALL);
        assert_eq!(requests[1].0, "eth_call");
        assert_ne!(requests[1].1[0]["to"], MULTICALL);
    }
}
