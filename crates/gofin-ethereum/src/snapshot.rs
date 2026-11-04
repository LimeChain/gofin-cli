//! Complete capture at one block, with bounded ordered work and capture-local
//! single-flight reads. Neither pending state nor the final reorg check is cached.
use super::*;
use abi::*;
use gofin_evm::{EvmRpc, block_tag, quantity, rpc_u64};
use std::collections::BTreeMap;
use tokio::{
    sync::{Mutex, OnceCell, Semaphore},
    task::JoinSet,
};

pub(crate) const SNAPSHOT_CONCURRENCY: usize = 8;
type ReadCell = Arc<OnceCell<Result<Value>>>;

pub(crate) struct SnapshotRpc {
    inner: Arc<dyn EvmRpc>,
    tag: String,
    permits: Semaphore,
    reads: Mutex<BTreeMap<String, ReadCell>>,
    pin_latest: bool,
    block: u64,
}
impl SnapshotRpc {
    pub(crate) fn new(inner: Arc<dyn EvmRpc>, block: u64, limit: usize) -> Self {
        Self {
            inner,
            tag: block_tag(Some(block)),
            permits: Semaphore::new(limit),
            reads: Mutex::new(BTreeMap::new()),
            pin_latest: false,
            block,
        }
    }
    pub(crate) fn preparation(inner: Arc<dyn EvmRpc>, block: u64) -> Self {
        let mut rpc = Self::new(inner, block, SNAPSHOT_CONCURRENCY);
        rpc.pin_latest = true;
        rpc
    }
    fn pinned(&self, method: &str, params: &Value) -> bool {
        let index = match method {
            "eth_call" | "eth_getCode" | "eth_getBalance" | "eth_getTransactionCount" => 1,
            "eth_getBlockByNumber" => 0,
            "eth_getStorageAt" => 2,
            _ => return false,
        };
        params[index].as_str() == Some(self.tag.as_str())
    }
    async fn fetch(
        &self,
        method: &str,
        params: Value,
        requirements: gofin_evm::RpcReadRequirements,
    ) -> Result<Value> {
        let _permit = self.permits.acquire().await.map_err(|_| {
            error(
                "SNAPSHOT_UNAVAILABLE",
                "read",
                "Snapshot read capacity closed.",
            )
        })?;
        self.inner
            .request_checked(method, params, requirements)
            .await
    }
}
#[async_trait]
impl EvmRpc for SnapshotRpc {
    fn enforces_chain_identity(&self, expected_chain_id: u64) -> bool {
        self.inner.enforces_chain_identity(expected_chain_id)
    }
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_checked(method, params, Default::default())
            .await
    }
    async fn request_checked(
        &self,
        method: &str,
        params: Value,
        requirements: gofin_evm::RpcReadRequirements,
    ) -> Result<Value> {
        require(
            matches!(
                method,
                "eth_call"
                    | "eth_getCode"
                    | "eth_getStorageAt"
                    | "eth_getBalance"
                    | "eth_getTransactionCount"
                    | "eth_getBlockByNumber"
                    | "eth_chainId"
                    | "eth_blockNumber"
            ),
            "SNAPSHOT_READ_ONLY",
            "A snapshot can only call reviewed read methods.",
        )?;
        let mut params = params;
        if self.pin_latest {
            if method == "eth_blockNumber" {
                require(
                    requirements
                        .minimum_block
                        .is_none_or(|minimum| self.block >= minimum)
                        && requirements.pin_block.is_none(),
                    "RPC_DATA_INVALID",
                    "Captured head does not satisfy the requested read requirements.",
                )?;
                return Ok(json!(format!("{:#x}", self.block)));
            }
            if matches!(method, "eth_call" | "eth_getCode" | "eth_getBalance")
                && params[1] == "latest"
            {
                params[1] = json!(self.tag);
            }
        }
        if !self.pinned(method, &params) {
            return self.fetch(method, params, requirements).await;
        }
        // Every cacheable state fact is fetched with canonical pin requirements.
        // Thus a later checked caller never reuses weaker, unchecked evidence.
        // Conflicting requirements remain distinct and are validated by transport.
        let requirements = if matches!(
            method,
            "eth_call"
                | "eth_getCode"
                | "eth_getBalance"
                | "eth_getTransactionCount"
                | "eth_getStorageAt"
        ) && requirements.minimum_block.is_none()
            && requirements
                .pin_block
                .is_none_or(|height| height == self.block)
        {
            gofin_evm::RpcReadRequirements {
                pin_block: Some(self.block),
                ..requirements
            }
        } else {
            requirements
        };
        // Exact request identity includes calldata, sender (if present), target,
        // and numeric block tag. Never share this cache with another capture.
        // A weaker read cannot satisfy a caller's stronger endpoint/hash or
        // freshness constraints. Preserve them through the real transport.
        let key = format!(
            "{method}:{params}:{:?}:{:?}",
            requirements.minimum_block, requirements.pin_block
        );
        let cell = self.reads.lock().await.entry(key).or_default().clone();
        cell.get_or_init(|| self.fetch(method, params, requirements))
            .await
            .clone()
    }
}

#[derive(Clone)]
enum CaptureUnit {
    Wallet(WalletConnection),
    Holding(WalletConnection, String),
    Reserve(WalletConnection, String, Option<String>),
    Morpho(WalletConnection, String),
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
        scope: &gofin_domain::SnapshotScope,
    ) -> Result<StateSnapshot> {
        if scope.is_full() {
            return self.read_snapshot().await;
        }
        let network = format!("ethereum:{}", self.config.chain_id);
        for asset in &scope.assets {
            if asset.network == network
                && super::reads::is_arc_native_usdc_alias(self.config.chain_id, &asset.reference)
            {
                require(
                    asset.decimals == 18,
                    "SNAPSHOT_SCOPE_INVALID",
                    "Arc native USDC alias must use its 18-decimal native asset identity.",
                )?;
            }
        }
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
        let mut requested_arc_native_alias = false;
        if !scope.assets.is_empty() {
            config.snapshot_tokens = scope
                .assets
                .iter()
                .filter(|asset| asset.network == network && asset.reference != "native")
                .filter_map(|asset| {
                    if super::reads::is_arc_native_usdc_alias(
                        self.config.chain_id,
                        &asset.reference,
                    ) {
                        requested_arc_native_alias = true;
                        None
                    } else {
                        Some(asset.reference.clone())
                    }
                })
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
            metadata_cache: self.metadata_cache.clone(),
            aave_mcp: self.aave_mcp.clone(),
            morpho_api: self.morpho_api.clone(),
            uniswap_api: self.uniswap_api.clone(),
            morpho_observed_vaults: self.morpho_observed_vaults.clone(),
            execution_state: self.execution_state.clone(),
        };
        let mut snapshot = scoped.read_snapshot().await?;
        if requested_arc_native_alias {
            snapshot.coverage.push(format!(
                "arc-native-usdc-alias:{}:represented-by-native-balance",
                super::reads::ARC_NATIVE_USDC_ERC20_ALIAS
            ));
        }
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
            if asset.network != network {
                continue;
            }
            let expected =
                if super::reads::is_arc_native_usdc_alias(self.config.chain_id, &asset.reference) {
                    require(
                        asset.decimals == 18,
                        "SNAPSHOT_SCOPE_INVALID",
                        "Arc native USDC alias must use its 18-decimal native asset identity.",
                    )?;
                    self.native_asset()
                } else {
                    asset.clone()
                };
            if !snapshot
                .state
                .holdings
                .iter()
                .any(|holding| holding.amount.asset == expected)
                && !snapshot
                    .state
                    .positions
                    .iter()
                    .any(|position| morpho::snapshot::represented_share(position, &expected))
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
        gofin_evm::hash(block_identity["hash"].as_str().unwrap_or(""))?;
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
            metadata_cache: self.metadata_cache.clone(),
            aave_mcp: self.aave_mcp.clone(),
            morpho_api: self.morpho_api.clone(),
            uniswap_api: self.uniswap_api.clone(),
            morpho_observed_vaults: self.morpho_observed_vaults.clone(),
            execution_state: self.execution_state.clone(),
        });
        let mut all = Vec::new();
        for market in self.market_configs() {
            let selected = capture.selected_market(&json!({"market":market.pool}))?;
            let adapter = selected.as_ref().unwrap_or(&capture);
            for token in adapter.all_tokens(None, Some(block)).await? {
                all.push((market.pool.clone(), token));
            }
        }
        let morpho_vaults = self.morpho_snapshot_vaults()?;
        let mut units = Vec::new();
        let configured_arc_native_alias = super::reads::is_arc_native_usdc_alias(
            self.config.chain_id,
            super::reads::ARC_NATIVE_USDC_ERC20_ALIAS,
        ) && self
            .config
            .snapshot_tokens
            .iter()
            .any(|token| super::reads::is_arc_native_usdc_alias(self.config.chain_id, token));
        for wallet in &self.wallets {
            units.push(CaptureUnit::Wallet(wallet.clone()));
            // Financial baselines declare a finite configured holding scope.
            // Indexer completeness is not evidence of exhaustive wallet state.
            let scope = self
                .config
                .snapshot_tokens
                .iter()
                .filter(|token| {
                    !morpho_vaults.contains(&token.to_ascii_lowercase())
                        && !super::reads::is_arc_native_usdc_alias(self.config.chain_id, token)
                })
                .cloned()
                .collect::<Vec<_>>();
            for token in scope {
                units.push(CaptureUnit::Holding(wallet.clone(), token.clone()));
            }
            for vault in &morpho_vaults {
                units.push(CaptureUnit::Morpho(wallet.clone(), vault.clone()));
            }
            for (pool, token) in &all {
                units.push(CaptureUnit::Reserve(
                    wallet.clone(),
                    token.clone(),
                    Some(pool.clone()),
                ));
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
        for vault in &morpho_vaults {
            coverage.push(format!(
                "selected-morpho-v2:{vault}:shares-and-backing-counted-once"
            ));
        }
        if configured_arc_native_alias {
            coverage.push(format!(
                "arc-native-usdc-alias:{}:represented-by-native-balance",
                super::reads::ARC_NATIVE_USDC_ERC20_ALIAS
            ));
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
        if let CaptureUnit::Reserve(w, t, Some(pool)) = &unit
            && let Some(a) = self.selected_market(&json!({"market":pool}))?
        {
            return Box::pin(
                a.capture_unit(CaptureUnit::Reserve(w.clone(), t.clone(), None), block),
            )
            .await;
        }
        let w = match &unit {
            CaptureUnit::Wallet(w)
            | CaptureUnit::Holding(w, _)
            | CaptureUnit::Reserve(w, _, _)
            | CaptureUnit::Morpho(w, _) => w,
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
                let mut spenders: BTreeSet<String> =
                    self.market_configs().into_iter().map(|a| a.pool).collect();
                spenders.extend(self.config.uniswap.as_ref().map(|u| u.router.clone()));
                if self.supports_operation("uniswap.swap") {
                    spenders.insert("0x000000000022d473030f116ddee9f6b43ac78ba3".into());
                }
                if let Ok(profile) = morpho::profile::profile(self.config.chain_id)
                    && !self.morpho_snapshot_vaults()?.is_empty()
                {
                    spenders.insert(profile.vault_bundles.into());
                }
                for spender in &spenders {
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
            CaptureUnit::Morpho(_, vault) => {
                part.state
                    .positions
                    .push(self.morpho_snapshot_position(&vault, owner, block).await?);
                let shares = self.token(&vault, Some(block)).await?;
                let spender = morpho::profile::profile(self.config.chain_id)?.vault_bundles;
                part.state.authorities.push(AuthorityState {
                    resource: r,
                    authority: authority(&vault, spender),
                    value: self
                        .allowance(&shares, owner, address(spender)?, Some(block))
                        .await?
                        .to_string(),
                });
            }
            CaptureUnit::Reserve(_, t, _) => {
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

    #[test]
    fn snapshot_forwards_only_its_inner_transport_chain_guarantee() {
        let guarded = Arc::new(
            gofin_evm::HttpRpc::isolated_localhost("http://127.0.0.1:1234".into(), 1).unwrap(),
        );
        let snapshot = SnapshotRpc::preparation(guarded, 20);
        assert!(snapshot.enforces_chain_identity(1));
        assert!(!snapshot.enforces_chain_identity(2));
        let alternate = SnapshotRpc::preparation(Arc::new(ReadProbe::default()), 20);
        assert!(!alternate.enforces_chain_identity(1));
    }

    #[derive(Default)]
    struct CheckedProbe(Mutex<Vec<gofin_evm::RpcReadRequirements>>);
    #[async_trait]
    impl EvmRpc for CheckedProbe {
        async fn request(&self, _: &str, _: Value) -> Result<Value> {
            panic!("snapshot must preserve checked transport requirements")
        }
        async fn request_checked(
            &self,
            _: &str,
            _: Value,
            requirements: gofin_evm::RpcReadRequirements,
        ) -> Result<Value> {
            self.0.lock().await.push(requirements);
            Ok(json!(if requirements.pin_block.is_some() {
                "0x2"
            } else {
                "0x1"
            }))
        }
    }
    #[tokio::test]
    async fn checked_snapshot_upgrades_pinned_facts_before_coalescing() {
        let probe = Arc::new(CheckedProbe::default());
        let rpc = SnapshotRpc::preparation(probe.clone(), 20);
        let params = json!(["0x0000000000000000000000000000000000000001", "0x14"]);
        assert_eq!(
            rpc.request("eth_getBalance", params.clone()).await.unwrap(),
            json!("0x2")
        );
        for _ in 0..2 {
            assert_eq!(
                rpc.request_checked(
                    "eth_getBalance",
                    params.clone(),
                    gofin_evm::RpcReadRequirements {
                        pin_block: Some(20),
                        ..Default::default()
                    }
                )
                .await
                .unwrap(),
                json!("0x2")
            );
        }
        let calls = probe.0.lock().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].pin_block, Some(20));
        drop(calls);
        for _ in 0..2 {
            rpc.request_checked(
                "eth_getBalance",
                json!(["0x0000000000000000000000000000000000000001", "0x13"]),
                gofin_evm::RpcReadRequirements {
                    pin_block: Some(19),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        assert_eq!(
            probe.0.lock().await.len(),
            3,
            "historical constraints must reach transport each time"
        );
        assert!(
            rpc.request_checked(
                "eth_blockNumber",
                json!([]),
                gofin_evm::RpcReadRequirements {
                    minimum_block: Some(21),
                    ..Default::default()
                }
            )
            .await
            .is_err()
        );
        assert_eq!(
            probe.0.lock().await.len(),
            3,
            "inadequate captured head must fail before transport"
        );
    }
    #[async_trait]
    impl EvmRpc for ReadProbe {
        async fn request(&self, _: &str, _: Value) -> Result<Value> {
            self.0.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            Ok(json!("0x0"))
        }
    }
    #[tokio::test]
    async fn storage_cache_only_shares_exact_pinned_block_requests() {
        let probe = Arc::new(ReadProbe::default());
        let rpc = SnapshotRpc::preparation(probe.clone(), 20);
        for tag in [
            "0x14", "0x14", "latest", "latest", "pending", "pending", "0x13", "0x13",
        ] {
            rpc.request(
                "eth_getStorageAt",
                json!(["0x0000000000000000000000000000000000000001", "0x00", tag]),
            )
            .await
            .unwrap();
        }
        assert_eq!(probe.0.load(Ordering::SeqCst), 7);
    }
    #[tokio::test]
    async fn preparation_cache_pins_latest_but_never_aliases_historical_or_pending_reads() {
        let probe = Arc::new(ReadProbe::default());
        let first = SnapshotRpc::preparation(probe.clone(), 20);
        for tag in ["latest", "0x14", "0x13", "0x13", "pending", "pending"] {
            first
                .request(
                    "eth_getBalance",
                    json!(["0x0000000000000000000000000000000000000001", tag]),
                )
                .await
                .unwrap();
        }
        // One current-block request, two historical requests, two pending requests.
        assert_eq!(probe.0.load(Ordering::SeqCst), 5);
        let next = SnapshotRpc::preparation(probe.clone(), 20);
        next.request(
            "eth_getBalance",
            json!(["0x0000000000000000000000000000000000000001", "latest"]),
        )
        .await
        .unwrap();
        assert_eq!(probe.0.load(Ordering::SeqCst), 6);
    }
    #[tokio::test]
    async fn conflicting_requirements_never_reuse_a_cached_pinned_fact() {
        let probe = Arc::new(ReadProbe::default());
        let rpc = SnapshotRpc::preparation(probe.clone(), 20);
        let params = json!(["0x0000000000000000000000000000000000000001", "0x14"]);
        rpc.request("eth_getBalance", params.clone()).await.unwrap();
        assert!(
            rpc.request_checked(
                "eth_getBalance",
                params.clone(),
                gofin_evm::RpcReadRequirements {
                    pin_block: Some(19),
                    ..Default::default()
                }
            )
            .await
            .is_err()
        );
        assert!(
            rpc.request_checked(
                "eth_getBalance",
                params,
                gofin_evm::RpcReadRequirements {
                    minimum_block: Some(20),
                    ..Default::default()
                }
            )
            .await
            .is_err()
        );
        assert_eq!(probe.0.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn pinned_calls_never_share_results_across_senders() {
        let probe = Arc::new(ReadProbe::default());
        let rpc = SnapshotRpc::preparation(probe.clone(), 20);
        for sender in [
            "0x0000000000000000000000000000000000000001",
            "0x0000000000000000000000000000000000000002",
            "0x0000000000000000000000000000000000000001",
        ] {
            rpc.request("eth_call", json!([{"to":"0x0000000000000000000000000000000000000003", "data":"0x12345678", "from":sender}, "0x14"])).await.unwrap();
        }
        assert_eq!(probe.0.load(Ordering::SeqCst), 2);
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
