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

/// One capture's verified batch endpoint and ordered read phases.
/// Invalid/unavailable Multicall code and failed or malformed aggregate envelopes
/// fall back to direct calls. A decoded envelope with a wrong result count fails
/// explicitly. A target revert is returned as an individual error;
/// it is never replaced by a zero or retried as if the aggregate were unavailable.
pub(crate) struct ReadCollector {
    adapter: EthereumAdapter,
    block: u64,
    multicall: Option<Address>,
    pub(crate) stats: CollectionStats,
}
impl ReadCollector {
    pub(crate) async fn new(adapter: &EthereumAdapter, block: u64) -> Self {
        let multicall = Self::verified_multicall(adapter, block).await;
        Self {
            adapter: adapter.clone(),
            block,
            multicall,
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
