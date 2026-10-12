//! Complete capture at one block, with bounded ordered work and capture-local
//! single-flight reads. Neither pending state nor the final reorg check is cached.
use super::*;
use abi::*;
use fin_evm::{EvmRpc, block_tag, quantity, rpc_u64};
use std::collections::BTreeMap;
use tokio::{
    sync::{Mutex, OnceCell, Semaphore},
    task::JoinSet,
};

pub(crate) const SNAPSHOT_CONCURRENCY: usize = 8;
type ReadCell = Arc<OnceCell<Result<Value>>>;

struct SnapshotRpc {
    inner: Arc<dyn EvmRpc>,
    tag: String,
    permits: Semaphore,
    reads: Mutex<BTreeMap<String, ReadCell>>,
}
impl SnapshotRpc {
    fn new(inner: Arc<dyn EvmRpc>, block: u64, limit: usize) -> Self {
        Self {
            inner,
            tag: block_tag(Some(block)),
            permits: Semaphore::new(limit),
            reads: Mutex::new(BTreeMap::new()),
        }
    }
    fn pinned(&self, method: &str, params: &Value) -> bool {
        let index = match method {
            "eth_call" | "eth_getCode" | "eth_getBalance" | "eth_getTransactionCount" => 1,
            "eth_getBlockByNumber" => 0,
            _ => return false,
        };
        params[index].as_str() == Some(self.tag.as_str())
    }
    async fn fetch(&self, method: &str, params: Value) -> Result<Value> {
        let _permit = self.permits.acquire().await.map_err(|_| {
            error(
                "SNAPSHOT_UNAVAILABLE",
                "read",
                "Snapshot read capacity closed.",
            )
        })?;
        self.inner.request(method, params).await
    }
}
#[async_trait]
impl EvmRpc for SnapshotRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        require(
            matches!(
                method,
                "eth_call"
                    | "eth_getCode"
                    | "eth_getBalance"
                    | "eth_getTransactionCount"
                    | "eth_getBlockByNumber"
                    | "eth_chainId"
                    | "eth_blockNumber"
            ),
            "SNAPSHOT_READ_ONLY",
            "A snapshot can only call reviewed read methods.",
        )?;
        if !self.pinned(method, &params) {
            return self.fetch(method, params).await;
        }
        // Exact request identity includes calldata, sender (if present), target,
        // and numeric block tag. Never share this cache with another capture.
        let key = format!("{method}:{}", params);
        let cell = self.reads.lock().await.entry(key).or_default().clone();
        cell.get_or_init(|| self.fetch(method, params))
            .await
            .clone()
    }
}

#[derive(Clone)]
enum CaptureUnit {
    Wallet(WalletConnection),
    Holding(WalletConnection, String),
    Reserve(WalletConnection, String),
}
#[derive(Default)]
struct CapturePart {
    state: FinancialState,
    pending: Vec<String>,
}
impl EthereumAdapter {
    pub async fn read_snapshot(&self) -> Result<StateSnapshot> {
        self.capture_snapshot(SNAPSHOT_CONCURRENCY).await
    }
    pub async fn read_snapshot_scoped(
        &self,
        scope: &fin_domain::SnapshotScope,
    ) -> Result<StateSnapshot> {
        if scope.is_full() {
            return self.read_snapshot().await;
        }
        let network = format!("ethereum:{}", self.config.chain_id);
        let mut wallets = Vec::new();
        for wallet in &self.wallets {
            if scope.matches_network(&network)
                && scope
                    .matches_resource(&resource(self.config.chain_id, address(&wallet.address)?))
            {
                wallets.push(wallet.clone());
            }
        }
        if wallets.is_empty() {
            return Ok(StateSnapshot {
                state: FinancialState::default(),
                observed_at: now(),
                complete: false,
                coverage: Vec::new(),
                pending: vec![format!(
                    "{network}: no wallet matches the requested snapshot scope"
                )],
            });
        }
        let mut config = self.config.clone();
        if !scope.assets.is_empty() {
            config.snapshot_tokens = scope
                .assets
                .iter()
                .filter(|asset| asset.network == network && asset.reference != "native")
                .map(|asset| asset.reference.clone())
                .collect();
            config.snapshot_tokens.sort();
            config.snapshot_tokens.dedup();
        }
        let scoped = Self {
            config,
            wallets,
            runtime: self.runtime.clone(),
            oracle_clock: self.oracle_clock.clone(),
            clock: self.clock.clone(),
            wallet_valuation: self.wallet_valuation.clone(),
            token_discovery: self.token_discovery.clone(),
        };
        let mut snapshot = scoped.read_snapshot().await?;
        // Keep every Aave reserve for the selected wallet: collateral and debt
        // outside the requested token can determine the safety of an action.
        for wallet in &scoped.wallets {
            snapshot.coverage.push(format!(
                "scoped-assets:{}:native{};related-aave-exposure={}",
                resource(self.config.chain_id, address(&wallet.address)?).0,
                scoped
                    .config
                    .snapshot_tokens
                    .iter()
                    .map(|token| format!(",{token}"))
                    .collect::<String>(),
                scoped.config.aave.is_some()
            ));
        }
        for asset in &scope.assets {
            if asset.network == network
                && !snapshot
                    .state
                    .holdings
                    .iter()
                    .any(|holding| holding.amount.asset == *asset)
            {
                snapshot.complete = false;
                snapshot.pending.push(format!(
                    "{network}: requested asset identity was not observed: {} ({} decimals)",
                    asset.reference, asset.decimals
                ));
            }
        }
        Ok(snapshot)
    }
    // Limit is private and fixed in production; one-worker tests compare the
    // exact same complete capture with the concurrent implementation.
    pub(crate) async fn capture_snapshot(&self, limit: usize) -> Result<StateSnapshot> {
        require(
            limit > 0 && limit <= SNAPSHOT_CONCURRENCY,
            "SNAPSHOT_SCOPE_INVALID",
            "Snapshot worker limit is invalid.",
        )?;
        self.runtime.assert_chain().await?;
        let block = rpc_u64(
            &self
                .runtime
                .rpc
                .request("eth_blockNumber", json!([]))
                .await?,
        )?;
        let params = json!([block_tag(Some(block)), false]);
        let block_identity = self
            .runtime
            .rpc
            .request("eth_getBlockByNumber", params.clone())
            .await?;
        fin_evm::hash(block_identity["hash"].as_str().unwrap_or(""))?;
        require(
            rpc_u64(&block_identity["number"])? == block,
            "SNAPSHOT_CHANGED",
            "Snapshot block identity differs from the requested height.",
        )?;
        // No signer is carried into the local capture runtime.
        let capture = Arc::new(Self {
            config: self.config.clone(),
            wallets: self.wallets.clone(),
            runtime: Arc::new(EvmRuntime::new(
                Arc::new(SnapshotRpc::new(self.runtime.rpc.clone(), block, limit)),
                self.config.chain_id,
                None,
            )),
            oracle_clock: self.oracle_clock.clone(),
            clock: self.clock.clone(),
            wallet_valuation: self.wallet_valuation.clone(),
            token_discovery: self.token_discovery.clone(),
        });
        let all = if self.config.aave.is_some() {
            capture.all_tokens(None, Some(block)).await?
        } else {
            Vec::new()
        };
        let mut units = Vec::new();
        for wallet in &self.wallets {
            units.push(CaptureUnit::Wallet(wallet.clone()));
            // Financial baselines declare a finite configured holding scope.
            // Indexer completeness is not evidence of exhaustive wallet state.
            let scope = &self.config.snapshot_tokens;
            for token in scope {
                units.push(CaptureUnit::Holding(wallet.clone(), token.clone()));
            }
            for token in &all {
                units.push(CaptureUnit::Reserve(wallet.clone(), token.clone()));
            }
        }
        let mut tasks = JoinSet::new();
        let mut results: Vec<Option<CapturePart>> = (0..units.len()).map(|_| None).collect();
        let mut next = units.into_iter().enumerate();
        let mut state = FinancialState::default();
        let mut pending = Vec::new();
        loop {
            while tasks.len() < limit {
                let Some((index, unit)) = next.next() else {
                    break;
                };
                let a = capture.clone();
                tasks.spawn(async move { (index, a.capture_unit(unit, block).await) });
            }
            let Some(result) = tasks.join_next().await else {
                break;
            };
            let result = result.map_err(|_| {
                error(
                    "SNAPSHOT_TASK_FAILED",
                    "read",
                    "A required snapshot task did not complete.",
                )
            });
            match result {
                Ok((index, Ok(part))) => results[index] = Some(part),
                failure => {
                    // Cancel and drain every outstanding read before reporting
                    // failure. No detached task can keep a capture alive.
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    return Err(match failure {
                        Ok((_, Err(e))) | Err(e) => e,
                        _ => unreachable!("successful task handled above"),
                    });
                }
            }
        }
        for part in results {
            let mut part = part.ok_or_else(|| {
                error(
                    "SNAPSHOT_INCOMPLETE",
                    "read",
                    "A required snapshot part is missing.",
                )
            })?;
            state.holdings.append(&mut part.state.holdings);
            state.positions.append(&mut part.state.positions);
            state.authorities.append(&mut part.state.authorities);
            pending.append(&mut part.pending);
        }
        // Explicitly bypass the capture cache, including its block-header cell.
        let after = self
            .runtime
            .rpc
            .request("eth_getBlockByNumber", params)
            .await?;
        require(
            block_identity["hash"] == after["hash"] && rpc_u64(&after["number"])? == block,
            "SNAPSHOT_CHANGED",
            "Snapshot block became noncanonical during capture.",
        )?;
        state.in_flight = pending.clone();
        let mut coverage = self
            .wallets
            .iter()
            .map(|w| Ok(resource(self.config.chain_id, address(&w.address)?).0))
            .collect::<Result<Vec<_>>>()?;
        if self.config.aave.is_none() && self.config.uniswap.is_none() {
            for wallet in &self.wallets {
                coverage.push(format!(
                    "explicit-token-scope:{}:native{}",
                    resource(self.config.chain_id, address(&wallet.address)?).0,
                    self.config
                        .snapshot_tokens
                        .iter()
                        .map(|token| format!(",{token}"))
                        .collect::<String>()
                ));
            }
        }
        Ok(StateSnapshot {
            state,
            observed_at: now(),
            complete: pending.is_empty(),
            coverage,
            pending,
        })
    }
    async fn capture_unit(&self, unit: CaptureUnit, block: u64) -> Result<CapturePart> {
        let w = match &unit {
            CaptureUnit::Wallet(w) | CaptureUnit::Holding(w, _) | CaptureUnit::Reserve(w, _) => w,
        };
        let owner = address(&w.address)?;
        let r = resource(self.config.chain_id, owner);
        let mut part = CapturePart::default();
        match unit {
            CaptureUnit::Wallet(w) => {
                let confirmed = rpc_u64(
                    &self
                        .runtime
                        .rpc
                        .request(
                            "eth_getTransactionCount",
                            json!([w.address, block_tag(Some(block))]),
                        )
                        .await?,
                )?;
                let live = self.runtime.pending_nonce(owner).await?;
                if live != confirmed {
                    part.pending.push(format!("{}:nonce-unsettled", r.0));
                }
                part.state.holdings.push(ResourceAmount {
                    resource: r,
                    amount: amount(
                        &self.native_asset(),
                        self.runtime.balance(owner, Some(block)).await?,
                    ),
                });
            }
            CaptureUnit::Holding(_, t) => {
                let token = self.token(&t, Some(block)).await?;
                part.state.holdings.push(ResourceAmount {
                    resource: r.clone(),
                    amount: amount(
                        &token.asset(self),
                        self.balance_of(&token, owner, Some(block)).await?,
                    ),
                });
                let spenders = self
                    .config
                    .aave
                    .as_ref()
                    .map(|a| &a.pool)
                    .into_iter()
                    .chain(self.config.uniswap.as_ref().map(|u| &u.router));
                for spender in spenders {
                    part.state.authorities.push(AuthorityState {
                        resource: r.clone(),
                        authority: authority(&token.reference, spender),
                        value: self
                            .allowance(&token, owner, address(spender)?, Some(block))
                            .await?
                            .to_string(),
                    });
                }
            }
            CaptureUnit::Reserve(_, t) => {
                let reserve = self.reserve(&t, Some(block)).await?;
                let user = self
                    .user_reserve(&reserve.token, owner, Some(block))
                    .await?;
                let scaled = self
                    .runtime
                    .call(
                        address(&reserve.a_token)?,
                        &AToken::scaledBalanceOfCall { user: owner },
                        Some(block),
                    )
                    .await?;
                for (kind, label, units, receipt) in [
                    (
                        PositionKind::Supply,
                        "supply",
                        user.supplied.clone(),
                        Some(quantity(scaled)),
                    ),
                    (
                        PositionKind::StableDebt,
                        "stable-debt",
                        user.stable_debt.clone(),
                        None,
                    ),
                    (
                        PositionKind::VariableDebt,
                        "variable-debt",
                        user.variable_debt.clone(),
                        Some(user.scaled_variable_debt.clone()),
                    ),
                ] {
                    let anchor = if kind == PositionKind::StableDebt {
                        None
                    } else {
                        let anchor = self
                            .snapshot_anchor(
                                &reserve.token,
                                owner,
                                address(if kind == PositionKind::Supply {
                                    &reserve.a_token
                                } else {
                                    &reserve.variable_debt_token
                                })?,
                                kind.clone(),
                                block,
                            )
                            .await?;
                        require(
                            anchor.principal.atoms == units && anchor.scaled_receipt == receipt,
                            "SNAPSHOT_POSITION_MISMATCH",
                            "Reserve position and independently proved receipt backing differ.",
                        )?;
                        Some(anchor)
                    };
                    part.state.positions.push(PositionState {
                        resource: r.clone(),
                        position: position(&self.aave()?.pool, &t, label),
                        kind,
                        amount: Amount {
                            asset: reserve.token.asset(self),
                            atoms: units,
                        },
                        scaled_receipt: receipt,
                        anchor,
                    });
                }
                part.state.authorities.push(AuthorityState {
                    resource: r,
                    authority: collateral(&self.aave()?.pool, &t),
                    value: user.collateral_enabled.to_string(),
                });
            }
        }
        Ok(part)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct ReadProbe(AtomicUsize);
    #[async_trait]
    impl EvmRpc for ReadProbe {
        async fn request(&self, _: &str, _: Value) -> Result<Value> {
            self.0.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            Ok(json!("0x0"))
        }
    }
    #[tokio::test]
    async fn snapshot_cache_coalesces_only_exact_pinned_reads_and_rejects_writes_before_transport()
    {
        let probe = Arc::new(ReadProbe::default());
        let rpc = SnapshotRpc::new(probe.clone(), 20, 2);
        let pinned = json!(["0x0000000000000000000000000000000000000001", "0x14"]);
        let (a, b) = tokio::join!(
            rpc.request("eth_getTransactionCount", pinned.clone()),
            rpc.request("eth_getTransactionCount", pinned.clone())
        );
        assert_eq!(a.unwrap(), b.unwrap());
        assert_eq!(probe.0.load(Ordering::SeqCst), 1);
        // Another target/tag/method cannot borrow the first request's response.
        rpc.request(
            "eth_getTransactionCount",
            json!(["0x0000000000000000000000000000000000000002", "0x14"]),
        )
        .await
        .unwrap();
        rpc.request("eth_getBalance", pinned).await.unwrap();
        for tag in ["pending", "latest", "0x13"] {
            for _ in 0..2 {
                rpc.request(
                    "eth_getTransactionCount",
                    json!(["0x0000000000000000000000000000000000000001", tag]),
                )
                .await
                .unwrap();
            }
        }
        assert_eq!(probe.0.load(Ordering::SeqCst), 9);
        for method in [
            "eth_sendRawTransaction",
            "eth_signTransaction",
            "eth_estimateGas",
            "anvil_setBalance",
        ] {
            assert_eq!(
                rpc.request(method, json!([])).await.unwrap_err().code,
                "SNAPSHOT_READ_ONLY"
            );
        }
        assert_eq!(probe.0.load(Ordering::SeqCst), 9);
    }
}
