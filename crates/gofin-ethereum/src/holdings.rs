//! Balance-only metadata caching. Financial preparation continues to use fresh
//! `token()` reads and never trusts this display cache for transaction authority.
use super::*;
use abi::{Erc20, Multicall3};
use gofin_evm::{
    RpcReadRequirements,
    alloy::{
        primitives::keccak256,
        sol_types::{SolCall, SolValue},
    },
    rpc_bytes,
};
use tokio::task::JoinSet;

const MULTICALL: &str = "0xca11bde05977b3631167028862be2a173976ca11";
const BATCH_SIZE: usize = 32;
const CONCURRENCY: usize = 4;
const DIRECT_CONCURRENCY: usize = 8;
#[derive(Clone)]
struct Call {
    target: Address,
    data: Vec<u8>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Metadata {
    token: reads::Token,
    name: Option<String>,
}
fn label_valid(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}
fn symbol(raw: &[u8]) -> Result<String> {
    let value = String::abi_decode(raw)
        .or_else(|_| {
            gofin_evm::alloy::primitives::B256::abi_decode(raw).and_then(|value| {
                // Preserve the existing bytes32 symbol compatibility.
                String::from_utf8(value.to_vec())
                    .map(|s| s.trim_end_matches('\0').to_owned())
                    .map_err(|_| gofin_evm::alloy::sol_types::Error::custom("invalid UTF-8"))
            })
        })
        .map_err(|_| {
            error(
                "TOKEN_METADATA_INVALID",
                "read",
                "Token symbol encoding is invalid.",
            )
        })?;
    require(
        label_valid(&value),
        "TOKEN_METADATA_INVALID",
        "Token symbol is not a valid bounded display label.",
    )?;
    Ok(value)
}
fn decode_balance(raw: &[u8]) -> Result<U256> {
    require(
        raw.len() == 32,
        "CONTRACT_DATA_INVALID",
        "Balance return must be exactly one ABI word.",
    )?;
    Ok(U256::from_be_slice(raw))
}
impl EthereumAdapter {
    fn metadata_key(&self, reference: &str) -> String {
        format!("evm-metadata-v1:{}:{reference}", self.config.chain_id)
    }
    pub(crate) fn learned_key(&self, owner: &str) -> String {
        format!(
            "evm-holdings-v1:{}:{}",
            self.config.chain_id,
            owner.to_ascii_lowercase()
        )
    }
    pub(crate) fn remembered_tokens(&self, owner: &str) -> Vec<String> {
        self.metadata_cache
            .get::<Vec<String>>(&self.learned_key(owner), -1, self.clock.now().timestamp())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|value| address(&value).ok())
            .filter(|value| *value != Address::ZERO)
            .map(lower_address)
            .filter(|reference| !reads::is_arc_native_usdc_alias(self.config.chain_id, reference))
            .take(512)
            .collect()
    }
    fn metadata_ttl(&self, reference: &str) -> i64 {
        let ttl = self.token_discovery.as_ref().map_or(86400, |provider| {
            provider.metadata_expiry_seconds(self.config.chain_id, reference)
        });
        if ttl == -1 || ttl > 0 { ttl } else { 86400 }
    }
    async fn verified_multicall(&self, block: u64) -> Option<Address> {
        if !matches!(self.config.chain_id, 1 | 8453 | 42161 | 4663) {
            return None;
        }
        let contract = address(MULTICALL).ok()?;
        let raw = self
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
    async fn direct_call(&self, call: Call, block: u64) -> Result<Vec<u8>> {
        let raw = self
            .runtime
            .rpc
            .request_checked(
                "eth_call",
                json!([{
                "to": lower_address(call.target),
                "data": gofin_evm::hex_bytes(&call.data)
            }, gofin_evm::block_tag(Some(block))]),
                RpcReadRequirements {
                    pin_block: Some(block),
                    ..Default::default()
                },
            )
            .await?;
        rpc_bytes(&raw).map(|bytes| bytes.to_vec())
    }
    async fn direct_calls(&self, calls: Vec<Call>, block: u64) -> Vec<Result<Vec<u8>>> {
        let mut output = Vec::with_capacity(calls.len());
        // At most eight direct calls per reader, including when aggregation fails.
        for chunk in calls.chunks(DIRECT_CONCURRENCY) {
            let mut jobs = JoinSet::new();
            for (i, call) in chunk.iter().cloned().enumerate() {
                let adapter = self.clone();
                jobs.spawn(async move { (i, adapter.direct_call(call, block).await) });
            }
            let mut part: Vec<_> = (0..chunk.len())
                .map(|_| {
                    Err(error(
                        "HOLDINGS_READ_FAILED",
                        "read",
                        "A holdings read did not complete.",
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
        &self,
        calls: Vec<Call>,
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
        let result = self
            .runtime
            .call(multicall, &request, Some(block))
            .await
            .ok()?;
        if result.len() != calls.len() {
            return None;
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
                            "Token read reverted in Multicall.",
                        ))
                    }
                })
                .collect(),
        )
    }
    async fn batch_calls(
        &self,
        calls: Vec<Call>,
        block: u64,
        multicall: Option<Address>,
    ) -> Vec<Result<Vec<u8>>> {
        let Some(multicall) = multicall else {
            return self.direct_calls(calls, block).await;
        };
        let chunks: Vec<_> = calls
            .chunks(BATCH_SIZE)
            .map(|chunk| chunk.to_vec())
            .collect();
        let mut output = Vec::with_capacity(calls.len());
        for group in chunks.chunks(CONCURRENCY) {
            let mut jobs = JoinSet::new();
            for (i, chunk) in group.iter().cloned().enumerate() {
                let adapter = self.clone();
                jobs.spawn(
                    async move { (i, adapter.aggregate_chunk(chunk, block, multicall).await) },
                );
            }
            let mut parts: Vec<Option<Vec<Result<Vec<u8>>>>> =
                (0..group.len()).map(|_| None).collect();
            while let Some(result) = jobs.join_next().await {
                if let Ok((i, value)) = result {
                    parts[i] = value;
                }
            }
            // Fallback is outside concurrent aggregation tasks so the eight-call
            // direct limit remains global to this balance read.
            for (chunk, part) in group.iter().zip(parts) {
                match part {
                    Some(part) => output.extend(part),
                    None => output.extend(self.direct_calls(chunk.clone(), block).await),
                }
            }
        }
        output
    }
    pub(crate) async fn read_holdings(
        &self,
        owner_string: &str,
        owner: Address,
        block: u64,
        explicit: Option<&Vec<String>>,
        refresh: bool,
    ) -> Result<(Vec<ResourceAmount>, TokenDiscoveryResult)> {
        let mut scope = self.wallet_tokens(owner_string, explicit).await?;
        let candidates = scope.candidates.clone();
        let (native, multicall) = tokio::join!(
            self.runtime.balance(owner, Some(block)),
            self.verified_multicall(block)
        );
        let mut holdings = vec![ResourceAmount {
            resource: resource(self.config.chain_id, owner),
            amount: amount(&self.native_asset(), native?),
        }];
        let calls = candidates
            .iter()
            .map(|reference| {
                Ok(Call {
                    target: address(reference)?,
                    data: Erc20::balanceOfCall { owner }.abi_encode(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let balances = self.batch_calls(calls, block, multicall).await;
        let now = self.clock.now().timestamp();
        let mut pending = Vec::new();
        let mut learned: BTreeSet<_> = self.remembered_tokens(owner_string).into_iter().collect();
        let configured: BTreeSet<_> = self
            .config
            .snapshot_tokens
            .iter()
            .map(|s| s.to_ascii_lowercase())
            .collect();
        for (reference, balance) in candidates.into_iter().zip(balances) {
            let balance = balance.and_then(|raw| decode_balance(&raw));
            let balance = match balance {
                Ok(value) => value,
                Err(failure) => {
                    Self::holding_failure(&mut scope, explicit.is_some(), &reference, failure)?;
                    continue;
                }
            };
            let key = self.metadata_key(&reference);
            if refresh {
                self.metadata_cache.remove(&key);
            }
            // Configured/explicit zeros retain their longstanding output; catalog
            // zeros are candidates only, and do not incur metadata requests.
            if balance == U256::ZERO && explicit.is_none() && !configured.contains(&reference) {
                continue;
            }
            let cached = self
                .metadata_cache
                .get::<Metadata>(&key, self.metadata_ttl(&reference), now)
                .filter(|m| {
                    m.token.reference == reference
                        && pow10(m.token.decimals).is_ok()
                        && label_valid(&m.token.symbol)
                        && m.name.as_ref().is_none_or(|s| label_valid(s))
                });
            pending.push((reference, balance, cached));
        }
        let mut calls = Vec::new();
        for (reference, _, metadata) in &pending {
            if metadata.is_none() {
                let target = address(reference)?;
                calls.extend([
                    Call {
                        target,
                        data: Erc20::decimalsCall {}.abi_encode(),
                    },
                    Call {
                        target,
                        data: Erc20::symbolCall {}.abi_encode(),
                    },
                    Call {
                        target,
                        data: Erc20::nameCall {}.abi_encode(),
                    },
                ]);
            }
        }
        let mut fresh = self.batch_calls(calls, block, multicall).await.into_iter();
        for (reference, balance, cached) in pending {
            let was_cached = cached.is_some();
            let metadata = if let Some(cached) = cached {
                Ok(cached)
            } else {
                let decimals = fresh.next().expect("three metadata results per token");
                let symbol_result = fresh.next().expect("three metadata results per token");
                let name = fresh
                    .next()
                    .expect("three metadata results per token")
                    .ok()
                    .and_then(|raw| symbol(&raw).ok());
                (|| {
                    let raw = decimals?;
                    let decimals = Erc20::decimalsCall::abi_decode_returns(&raw).map_err(|_| {
                        error("TOKEN_METADATA_INVALID", "read", "Invalid token decimals.")
                    })?;
                    pow10(decimals)?;
                    Ok(Metadata {
                        token: reads::Token {
                            reference: reference.clone(),
                            decimals,
                            symbol: symbol(&symbol_result?)?,
                        },
                        name,
                    })
                })()
            };
            match metadata {
                Ok(metadata) => {
                    if balance > U256::ZERO {
                        learned.insert(reference.clone());
                    }
                    if !was_cached {
                        self.metadata_cache.put(
                            &self.metadata_key(&reference),
                            &metadata,
                            self.metadata_ttl(&reference),
                            now,
                        );
                    }
                    holdings.push(ResourceAmount {
                        resource: resource(self.config.chain_id, owner),
                        amount: amount(&metadata.token.asset(self), balance),
                    });
                }
                Err(failure) => {
                    Self::holding_failure(&mut scope, explicit.is_some(), &reference, failure)?
                }
            }
        }
        let learned: Vec<_> = learned.into_iter().take(512).collect();
        self.metadata_cache
            .put(&self.learned_key(owner_string), &learned, -1, now);
        Ok((holdings, scope))
    }
    fn holding_failure(
        scope: &mut TokenDiscoveryResult,
        strict: bool,
        reference: &str,
        failure: GoFinError,
    ) -> Result<()> {
        if strict {
            return Err(failure);
        }
        scope.complete = false;
        scope.warnings.push(format!(
            "Token {reference} could not be verified ({}); its holding is omitted.",
            failure.code
        ));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gofin_evm::EvmRpc;
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering},
    };
    fn token(n: u64) -> String {
        lower_address(super::super::tests::addr(n))
    }
    struct Clock(AtomicI64);
    impl gofin_domain::clock::Clock for Clock {
        fn now(&self) -> chrono::DateTime<chrono::Utc> {
            chrono::DateTime::from_timestamp(self.0.load(Ordering::SeqCst), 0).unwrap()
        }
    }
    struct Catalog {
        tokens: Vec<String>,
        ttl: i64,
    }
    #[async_trait]
    impl TokenDiscovery for Catalog {
        async fn tokens(&self, _: u64, _: &str) -> Result<TokenDiscoveryResult> {
            Ok(TokenDiscoveryResult {
                candidates: self.tokens.clone(),
                complete: false,
                source: "local-fixture".into(),
                warnings: vec![],
            })
        }
        fn metadata_expiry_seconds(&self, _: u64, _: &str) -> i64 {
            self.ttl
        }
    }
    struct Rpc {
        canonical: AtomicBool,
        aggregation_fails: AtomicBool,
        corrupt_aggregate: AtomicBool,
        bad_token: AtomicBool,
        balance: AtomicU64,
        metadata_reads: AtomicUsize,
        aggregates: AtomicUsize,
        direct: AtomicUsize,
        active: AtomicUsize,
        max_active: AtomicUsize,
        requests: Mutex<Vec<(String, Value)>>,
    }
    impl Rpc {
        fn new(canonical: bool) -> Self {
            Self {
                canonical: AtomicBool::new(canonical),
                aggregation_fails: AtomicBool::new(false),
                corrupt_aggregate: AtomicBool::new(false),
                bad_token: AtomicBool::new(false),
                balance: AtomicU64::new(123),
                metadata_reads: AtomicUsize::new(0),
                aggregates: AtomicUsize::new(0),
                direct: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                requests: Mutex::new(vec![]),
            }
        }
        fn call(&self, target: Address, data: &[u8]) -> Result<Vec<u8>> {
            if self.bad_token.load(Ordering::SeqCst) && target == super::super::tests::addr(102) {
                return Err(error("SYNTHETIC_REVERT", "read", "Fixture revert."));
            }
            match data.get(..4) {
                Some(selector) if selector == Erc20::balanceOfCall::SELECTOR => {
                    Ok(U256::from(self.balance.load(Ordering::SeqCst)).abi_encode())
                }
                Some(selector) if selector == Erc20::decimalsCall::SELECTOR => {
                    self.metadata_reads.fetch_add(1, Ordering::SeqCst);
                    Ok(U256::from(6).abi_encode())
                }
                Some(selector) if selector == Erc20::symbolCall::SELECTOR => {
                    self.metadata_reads.fetch_add(1, Ordering::SeqCst);
                    Ok("USDC".to_string().abi_encode())
                }
                Some(selector) if selector == Erc20::nameCall::SELECTOR => {
                    self.metadata_reads.fetch_add(1, Ordering::SeqCst);
                    Ok("USD Coin".to_string().abi_encode())
                }
                _ => Err(error("SYNTHETIC_UNKNOWN", "read", "Unexpected call.")),
            }
        }
    }
    #[async_trait]
    impl EvmRpc for Rpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            self.requests
                .lock()
                .unwrap()
                .push((method.into(), params.clone()));
            match method {
                "eth_getCode" => Ok(json!(if self.canonical.load(Ordering::SeqCst) {
                    format!(
                        "0x{}",
                        include_str!("multicall/multicall3-runtime.hex").trim()
                    )
                } else {
                    "0x6000".into()
                })),
                "eth_getBalance" => Ok(json!("0x123")),
                "eth_call" => {
                    let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                    self.max_active.fetch_max(active, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    self.active.fetch_sub(1, Ordering::SeqCst);
                    let target = address(params[0]["to"].as_str().unwrap())?;
                    let data =
                        hex::decode(params[0]["data"].as_str().unwrap().trim_start_matches("0x"))
                            .unwrap();
                    if target == address(MULTICALL)? {
                        self.aggregates.fetch_add(1, Ordering::SeqCst);
                        if self.aggregation_fails.load(Ordering::SeqCst) {
                            return Err(error(
                                "SYNTHETIC_BATCH_LIMIT",
                                "read",
                                "Provider rejected batch.",
                            ));
                        }
                        if self.corrupt_aggregate.load(Ordering::SeqCst) {
                            return Ok(json!("0x1234"));
                        }
                        let calls = Multicall3::aggregate3Call::abi_decode(&data).unwrap().calls;
                        assert!(calls.len() <= BATCH_SIZE);
                        let results: Vec<_> = calls
                            .into_iter()
                            .map(|call| match self.call(call.target, &call.callData) {
                                Ok(data) => Multicall3::Result3 {
                                    success: true,
                                    returnData: data.into(),
                                },
                                Err(_) => Multicall3::Result3 {
                                    success: false,
                                    returnData: vec![].into(),
                                },
                            })
                            .collect();
                        Ok(json!(gofin_evm::hex_bytes(&results.abi_encode())))
                    } else {
                        self.direct.fetch_add(1, Ordering::SeqCst);
                        Ok(json!(gofin_evm::hex_bytes(&self.call(target, &data)?)))
                    }
                }
                _ => Err(error("SYNTHETIC_UNKNOWN", "read", "Unexpected RPC method.")),
            }
        }
    }
    fn adapter(rpc: Arc<Rpc>, count: u64, ttl: i64) -> EthereumAdapter {
        EthereumAdapter::new(
            EthereumConfig::wallet_only(1, 2, vec![]),
            vec![WalletConnection {
                name: "test".into(),
                address: token(1),
                signing_enabled: false,
            }],
            Arc::new(EvmRuntime::new(rpc, 1, None)),
        )
        .unwrap()
        .with_token_discovery(Arc::new(Catalog {
            tokens: (100..100 + count).map(token).collect(),
            ttl,
        }))
    }
    async fn read(
        adapter: &EthereumAdapter,
        explicit: Option<&Vec<String>>,
        refresh: bool,
    ) -> Result<(Vec<ResourceAmount>, TokenDiscoveryResult)> {
        adapter
            .read_holdings(
                &token(1),
                address(&token(1)).unwrap(),
                1234,
                explicit,
                refresh,
            )
            .await
    }
    #[tokio::test]
    async fn multicall_matches_direct_pinned_reads_and_bounded_batches() {
        let batched = Arc::new(Rpc::new(true));
        let direct = Arc::new(Rpc::new(false));
        let a = adapter(batched.clone(), 128, 86400);
        let b = adapter(direct.clone(), 128, 86400);
        let first = read(&a, None, false).await.unwrap();
        let baseline = read(&b, None, false).await.unwrap();
        assert_eq!(
            serde_json::to_value(&first.0).unwrap(),
            serde_json::to_value(&baseline.0).unwrap()
        );
        assert_eq!(first.0.len(), 129);
        assert_eq!(batched.aggregates.load(Ordering::SeqCst), 16); // 4 balance + 12 metadata
        assert_eq!(batched.direct.load(Ordering::SeqCst), 0);
        assert_eq!(direct.direct.load(Ordering::SeqCst), 512);
        assert!(batched.max_active.load(Ordering::SeqCst) <= CONCURRENCY);
        assert!(direct.max_active.load(Ordering::SeqCst) <= DIRECT_CONCURRENCY);
        read(&a, None, false).await.unwrap();
        assert_eq!(batched.aggregates.load(Ordering::SeqCst), 20); // warm: four balances only
        assert_eq!(batched.metadata_reads.load(Ordering::SeqCst), 384);
        for rpc in [batched, direct] {
            for (_, params) in rpc.requests.lock().unwrap().iter() {
                assert_eq!(params[1], json!("0x4d2"));
            }
        }
    }
    #[tokio::test]
    async fn metadata_expiration_does_not_slide_and_refresh_is_explicit() {
        let rpc = Arc::new(Rpc::new(true));
        let clock = Arc::new(Clock(AtomicI64::new(1000)));
        let a = adapter(rpc.clone(), 1, 100).with_clock(clock.clone());
        read(&a, None, false).await.unwrap();
        clock.0.store(1099, Ordering::SeqCst);
        rpc.balance.store(456, Ordering::SeqCst);
        let warm = read(&a, None, false).await.unwrap();
        assert_eq!(warm.0[1].amount.atoms, Quantity::from_u64(456));
        assert_eq!(rpc.metadata_reads.load(Ordering::SeqCst), 3);
        clock.0.store(1100, Ordering::SeqCst);
        read(&a, None, false).await.unwrap();
        assert_eq!(rpc.metadata_reads.load(Ordering::SeqCst), 6);
        read(&a, None, true).await.unwrap();
        assert_eq!(rpc.metadata_reads.load(Ordering::SeqCst), 9);
    }
    #[tokio::test]
    async fn forever_cache_survives_restart_and_policy_change_invalidates() {
        let directory = tempfile::tempdir().unwrap();
        let rpc = Arc::new(Rpc::new(true));
        let clock = Arc::new(Clock(AtomicI64::new(1000)));
        let a = adapter(rpc.clone(), 1, -1)
            .with_clock(clock.clone())
            .with_metadata_cache(Arc::new(gofin_token_cache::TokenCache::new(
                directory.path(),
            )));
        read(&a, None, false).await.unwrap();
        drop(a);
        clock.0.store(10000000, Ordering::SeqCst);
        let a = adapter(rpc.clone(), 1, -1)
            .with_clock(clock.clone())
            .with_metadata_cache(Arc::new(gofin_token_cache::TokenCache::new(
                directory.path(),
            )));
        read(&a, None, false).await.unwrap();
        assert_eq!(rpc.metadata_reads.load(Ordering::SeqCst), 3);
        let a = a.with_token_discovery(Arc::new(Catalog {
            tokens: vec![token(100)],
            ttl: 86400,
        }));
        read(&a, None, false).await.unwrap();
        assert_eq!(rpc.metadata_reads.load(Ordering::SeqCst), 6);
    }
    #[tokio::test]
    async fn zeros_skip_metadata_but_new_balances_and_explicit_zeros_are_observed() {
        let rpc = Arc::new(Rpc::new(true));
        rpc.balance.store(0, Ordering::SeqCst);
        let a = adapter(rpc.clone(), 1, 86400);
        assert_eq!(read(&a, None, false).await.unwrap().0.len(), 1);
        assert_eq!(rpc.metadata_reads.load(Ordering::SeqCst), 0);
        rpc.balance.store(12, Ordering::SeqCst);
        assert_eq!(
            read(&a, None, false).await.unwrap().0[1].amount.atoms,
            Quantity::from_u64(12)
        );
        rpc.balance.store(0, Ordering::SeqCst);
        assert_eq!(
            read(&a, Some(&vec![token(199)]), false)
                .await
                .unwrap()
                .0
                .len(),
            2
        );
        let a = a.with_token_discovery(Arc::new(Catalog {
            tokens: vec![],
            ttl: 86400,
        }));
        rpc.balance.store(14, Ordering::SeqCst);
        assert_eq!(read(&a, None, false).await.unwrap().0.len(), 2); // remembered positive
    }
    #[tokio::test]
    async fn failed_or_malformed_aggregate_falls_back_with_bounded_direct_calls() {
        for malformed in [false, true] {
            let rpc = Arc::new(Rpc::new(true));
            rpc.aggregation_fails.store(!malformed, Ordering::SeqCst);
            rpc.corrupt_aggregate.store(malformed, Ordering::SeqCst);
            let a = adapter(rpc.clone(), 40, 86400);
            assert_eq!(read(&a, None, false).await.unwrap().0.len(), 41);
            assert_eq!(rpc.direct.load(Ordering::SeqCst), 160);
            assert!(rpc.max_active.load(Ordering::SeqCst) <= DIRECT_CONCURRENCY);
        }
    }
    #[tokio::test]
    async fn per_token_failure_preserves_automatic_success_and_explicit_is_strict() {
        let rpc = Arc::new(Rpc::new(true));
        rpc.bad_token.store(true, Ordering::SeqCst);
        let a = adapter(rpc, 3, 86400);
        let result = read(&a, None, false).await.unwrap();
        assert_eq!(result.0.len(), 3);
        assert_eq!(result.1.warnings.len(), 1);
        assert!(!result.1.complete);
        assert!(read(&a, Some(&vec![token(102)]), false).await.is_err());
    }
    #[tokio::test]
    async fn corrupt_or_mismatched_cache_is_refetched_and_financial_token_reads_stay_fresh() {
        let directory = tempfile::tempdir().unwrap();
        let rpc = Arc::new(Rpc::new(true));
        let cache = Arc::new(gofin_token_cache::TokenCache::new(directory.path()));
        let a = adapter(rpc.clone(), 1, 86400).with_metadata_cache(cache.clone());
        read(&a, None, false).await.unwrap();
        for entry in std::fs::read_dir(directory.path()).unwrap() {
            std::fs::write(entry.unwrap().path(), b"{corrupt").unwrap();
        }
        read(&a, None, false).await.unwrap();
        assert_eq!(rpc.metadata_reads.load(Ordering::SeqCst), 6);
        cache.put(
            &a.metadata_key(&token(100)),
            &Metadata {
                token: reads::Token {
                    reference: token(101),
                    decimals: 6,
                    symbol: "WRONG".into(),
                },
                name: None,
            },
            86400,
            a.clock.now().timestamp(),
        );
        read(&a, None, false).await.unwrap();
        assert_eq!(rpc.metadata_reads.load(Ordering::SeqCst), 9);
        a.token(&token(100), Some(1234)).await.unwrap();
        assert_eq!(rpc.metadata_reads.load(Ordering::SeqCst), 11); // financial path still reads decimals + symbol
        let other = adapter(rpc.clone(), 1, 86400).with_metadata_cache(cache);
        let mut other = other;
        other.config.chain_id = 42161;
        read(&other, None, false).await.unwrap();
        assert_eq!(rpc.metadata_reads.load(Ordering::SeqCst), 14);
    }
    #[tokio::test]
    async fn remembered_arc_native_alias_cannot_reenter_erc20_scope() {
        let rpc = Arc::new(Rpc::new(false));
        let mut a = adapter(rpc, 0, 86400);
        a.config.chain_id = 5042;
        a.metadata_cache.put(
            &a.learned_key(&token(1)),
            &vec![reads::ARC_NATIVE_USDC_ERC20_ALIAS.to_owned(), token(100)],
            -1,
            a.clock.now().timestamp(),
        );
        assert_eq!(
            a.wallet_tokens(&token(1), None).await.unwrap().candidates,
            vec![token(100)]
        );
    }
}
