//! Chain-verified V2 identities and holdings, with separately labelled indexer metadata.
use super::{api, profile};
use crate::*;
use gofin_evm::alloy::sol_types::SolValue;
use gofin_evm::alloy::{primitives::keccak256, sol};
use gofin_evm::{block_tag, hash, rpc_bytes, rpc_u64};

sol! {
    struct MarketParams { address loanToken; address collateralToken; address oracle; address irm; uint256 lltv; }
    interface VaultView {
        function asset() external view returns (address);
        function totalAssets() external view returns (uint256);
        function totalSupply() external view returns (uint256);
        function virtualShares() external view returns (uint256);
        function previewRedeem(uint256 shares) external view returns (uint256);
        function liquidityAdapter() external view returns (address);
        function sendAssetsGate() external view returns (address);
        function receiveAssetsGate() external view returns (address);
        function sendSharesGate() external view returns (address);
        function receiveSharesGate() external view returns (address);
        function adaptersLength() external view returns (uint256);
        function adapters(uint256 index) external view returns (address);
        function adapterRegistry() external view returns (address);
        function liquidityData() external view returns (bytes);
        function absoluteCap(bytes32 id) external view returns (uint256);
        function relativeCap(bytes32 id) external view returns (uint256);
        function allocation(bytes32 id) external view returns (uint256);
        function performanceFee() external view returns (uint96);
        function managementFee() external view returns (uint96);
        function canReceiveShares(address account) external view returns (bool);
        function canSendShares(address account) external view returns (bool);
        function canReceiveAssets(address account) external view returns (bool);
        function canSendAssets(address account) external view returns (bool);
    }
    interface FactoryView { function isVaultV2(address account) external view returns (bool); }
    interface AdapterFactoryView {
        function isMorphoMarketV1AdapterV2(address account) external view returns (bool);
        function isMorphoVaultV1Adapter(address account) external view returns (bool);
    }
    interface AdapterView {
        function parentVault() external view returns (address);
        function morphoVaultV1() external view returns (address);
        function morpho() external view returns (address);
        function expectedSupplyAssets(bytes32 marketId) external view returns (uint256);
    }
    interface MarketAdapterView { function ids(MarketParams marketParams) external view returns (bytes32[]); }
    interface NestedAdapterView { function ids() external view returns (bytes32[]); }
    interface NestedVaultView {
        function maxDeposit(address owner) external view returns (uint256);
        function maxWithdraw(address owner) external view returns (uint256);
    }
    interface BlueView { function market(bytes32 id) external view returns (uint128 totalSupplyAssets, uint128 totalSupplyShares, uint128 totalBorrowAssets, uint128 totalBorrowShares, uint128 lastUpdate, uint128 fee); }
}

fn network() -> String {
    "ethereum".into()
}
fn default_limit() -> u32 {
    20
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MorphoReadInput {
    #[serde(default = "network")]
    pub network: String,
    pub chain_id: u64,
    #[serde(default)]
    pub wallet: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub vault: Option<String>,
    #[serde(default)]
    pub asset: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default)]
    pub cursor: Option<String>,
    /// Historical on-chain observation only; indexer metadata is always labelled separately.
    #[serde(default)]
    pub block_number: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MorphoVaultState {
    pub vault: String,
    pub asset: AssetId,
    pub shares: AssetId,
    pub block: u64,
    pub block_hash: String,
    pub block_timestamp: u64,
    pub total_assets: Quantity,
    pub total_supply: Quantity,
    pub virtual_shares: Quantity,
    pub owner_shares: Quantity,
    pub owner_assets: Quantity,
    pub idle_assets: Quantity,
    pub liquidity_adapter: String,
    pub code_identity: String,
    pub profile_identity: String,
    pub gates: Value,
    pub adapters: Vec<Value>,
    pub write_compatibility: Value,
    pub capacity: Value,
    pub fees: Value,
}

impl EthereumAdapter {
    /// Every read is pinned to one canonical block. Current observations retain
    /// the same 60-second freshness requirement as financial preparation.
    pub(crate) async fn morpho_vault_state(
        &self,
        vault: &str,
        owner: Option<Address>,
        block: Option<u64>,
    ) -> Result<MorphoVaultState> {
        let profile = profile::profile(self.config.chain_id)?;
        self.runtime.assert_chain().await?;
        let vault = address(&api::normalized_address(vault)?)?;
        let header = self
            .runtime
            .rpc
            .request("eth_getBlockByNumber", json!([block_tag(block), false]))
            .await?;
        let number = rpc_u64(&header["number"])?;
        let block_hash = header["hash"]
            .as_str()
            .ok_or_else(|| read_error("Block hash is missing."))?
            .to_lowercase();
        hash(&block_hash)?;
        let timestamp = rpc_u64(&header["timestamp"])?;
        if let Some(expected) = block {
            require(
                number == expected,
                "MORPHO_READ_CHANGED",
                "Historical Morpho observation returned another block.",
            )?;
        } else {
            let now = self.clock.now().timestamp();
            require(
                now >= 0
                    && timestamp <= now as u64 + 30
                    && timestamp.saturating_add(60) >= now as u64,
                "MORPHO_STATE_STALE",
                "Morpho observation is not a fresh canonical block.",
            )?;
        }
        let block = Some(number);
        let factory = address(profile.vault_factory)?;
        let bundles = address(profile.vault_bundles)?;
        let (factory_hash, bundles_hash, vault_hash) = tokio::try_join!(
            async { self.morpho_code_hash(factory, number).await },
            async { self.morpho_code_hash(bundles, number).await },
            async { self.morpho_code_hash(vault, number).await }
        )?;
        require(
            factory_hash == profile.factory_code_hash && bundles_hash == profile.bundles_code_hash,
            "MORPHO_DEPLOYMENT_CHANGED",
            "Morpho factory or vault bundle code differs from the reviewed deployment.",
        )?;
        require(
            self.runtime
                .call(
                    factory,
                    &FactoryView::isVaultV2Call { account: vault },
                    block,
                )
                .await?,
            "MORPHO_VAULT_UNSUPPORTED",
            "This address was not deployed by the reviewed Vault V2 factory; V1 and fee wrappers are unsupported.",
        )?;
        let (asset, total_assets, total_supply, virtual_shares, liquidity_adapter) = tokio::try_join!(
            async {
                self.runtime
                    .call(vault, &VaultView::assetCall {}, block)
                    .await
            },
            async {
                self.runtime
                    .call(vault, &VaultView::totalAssetsCall {}, block)
                    .await
            },
            async {
                self.runtime
                    .call(vault, &VaultView::totalSupplyCall {}, block)
                    .await
            },
            async {
                self.runtime
                    .call(vault, &VaultView::virtualSharesCall {}, block)
                    .await
            },
            async {
                self.runtime
                    .call(vault, &VaultView::liquidityAdapterCall {}, block)
                    .await
            }
        )?;
        let asset_address = lower_address(asset);
        let vault_address = lower_address(vault);
        let (asset_token, share_token) =
            tokio::try_join!(async { self.token(&asset_address, block).await }, async {
                self.token(&vault_address, block).await
            })?;
        let idle_assets = self.balance_of(&asset_token, vault, block).await?;
        let owner_shares = if let Some(owner) = owner {
            self.balance_of(&share_token, owner, block).await?
        } else {
            U256::ZERO
        };
        let owner_assets = self
            .runtime
            .call(
                vault,
                &VaultView::previewRedeemCall {
                    shares: owner_shares,
                },
                block,
            )
            .await?;
        let (send_assets, receive_assets, send_shares, receive_shares) = tokio::try_join!(
            async {
                self.runtime
                    .call(vault, &VaultView::sendAssetsGateCall {}, block)
                    .await
            },
            async {
                self.runtime
                    .call(vault, &VaultView::receiveAssetsGateCall {}, block)
                    .await
            },
            async {
                self.runtime
                    .call(vault, &VaultView::sendSharesGateCall {}, block)
                    .await
            },
            async {
                self.runtime
                    .call(vault, &VaultView::receiveSharesGateCall {}, block)
                    .await
            }
        )?;
        let (adapters, route_identity, compatibility, capacity) = self
            .morpho_route_state(
                vault,
                owner,
                bundles,
                asset,
                number,
                total_assets,
                idle_assets,
                owner_assets,
                liquidity_adapter,
                [send_assets, receive_assets, send_shares, receive_shares],
            )
            .await?;
        let (performance_fee, management_fee) = tokio::try_join!(
            async {
                self.runtime
                    .call(vault, &VaultView::performanceFeeCall {}, block)
                    .await
            },
            async {
                self.runtime
                    .call(vault, &VaultView::managementFeeCall {}, block)
                    .await
            }
        )?;
        let final_header = self
            .runtime
            .rpc
            .request("eth_getBlockByNumber", json!([block_tag(block), false]))
            .await?;
        require(
            final_header["hash"]
                .as_str()
                .is_some_and(|h| h.eq_ignore_ascii_case(&block_hash)),
            "MORPHO_READ_CHANGED",
            "Canonical block changed during the Morpho observation.",
        )?;
        self.morpho_observed_vaults
            .lock()
            .map_err(|_| read_error("Observed vault registry is unavailable."))?
            .insert(lower_address(vault));
        Ok(MorphoVaultState {
            vault: lower_address(vault),
            asset: asset_token.asset(self),
            shares: share_token.asset(self),
            block: number,
            block_hash,
            block_timestamp: timestamp,
            total_assets: quantity(total_assets),
            total_supply: quantity(total_supply),
            virtual_shares: quantity(virtual_shares),
            owner_shares: quantity(owner_shares),
            owner_assets: quantity(owner_assets),
            idle_assets: quantity(idle_assets),
            liquidity_adapter: lower_address(liquidity_adapter),
            code_identity: keccak256(format!(
                "{vault_hash}:{factory_hash}:{bundles_hash}:{route_identity}"
            ))
            .to_string(),
            profile_identity: keccak256(format!(
                "{}:{}:{}:{}:{}:{}",
                profile.chain_id,
                profile.vault_factory,
                profile.vault_bundles,
                factory_hash,
                bundles_hash,
                route_identity
            ))
            .to_string(),
            gates: json!({"send_assets":lower_address(send_assets),"receive_assets":lower_address(receive_assets),"send_shares":lower_address(send_shares),"receive_shares":lower_address(receive_shares)}),
            adapters,
            write_compatibility: compatibility,
            capacity,
            fees: json!({"source":"canonical_rpc","performance_fee_wad":performance_fee.to_string(),"management_fee_per_second_wad":management_fee.to_string(),"fee_scale":"1000000000000000000"}),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn morpho_route_state(
        &self,
        vault: Address,
        owner: Option<Address>,
        bundles: Address,
        asset: Address,
        block: u64,
        total_assets: U256,
        idle_assets: U256,
        owner_assets: U256,
        liquidity_adapter: Address,
        gates: [Address; 4],
    ) -> Result<(Vec<Value>, String, Value, Value)> {
        let pin = Some(block);
        let p = profile::profile(self.config.chain_id)?;
        let (length, registry, liquidity_data) = tokio::try_join!(
            async {
                self.runtime
                    .call(vault, &VaultView::adaptersLengthCall {}, pin)
                    .await
            },
            async {
                self.runtime
                    .call(vault, &VaultView::adapterRegistryCall {}, pin)
                    .await
            },
            async {
                self.runtime
                    .call(vault, &VaultView::liquidityDataCall {}, pin)
                    .await
            }
        )?;
        require(
            length <= U256::from(64),
            "MORPHO_VAULT_UNSUPPORTED",
            "Vault adapter count exceeds the bounded reviewed observation scope.",
        )?;
        let mut identities = Vec::new();
        let mut reasons = Vec::new();
        for (label, account) in [
            "send_assets",
            "receive_assets",
            "send_shares",
            "receive_shares",
        ]
        .into_iter()
        .zip(gates)
        .chain(std::iter::once(("registry", registry)))
        {
            identities.push(json!({"role":label,"address":lower_address(account),"code_hash":if account.is_zero(){None}else{Some(self.morpho_code_hash(account,block).await?)}}));
        }
        let mut adapters = Vec::new();
        let mut liquidity_kind = None;
        if length > U256::ZERO {
            let market_factory = address(p.market_adapter_factory)?;
            let nested_factory = address(p.vault_adapter_factory)?;
            let (market_hash, nested_hash) = tokio::try_join!(
                async { self.morpho_code_hash(market_factory, block).await },
                async { self.morpho_code_hash(nested_factory, block).await }
            )?;
            require(
                market_hash == p.market_adapter_factory_code_hash
                    && nested_hash == p.vault_adapter_factory_code_hash,
                "MORPHO_DEPLOYMENT_CHANGED",
                "Morpho adapter factory code differs from its reviewed immutable deployment.",
            )?;
            identities.push(json!({"market_factory":p.market_adapter_factory,"market_factory_code_hash":market_hash,"nested_factory":p.vault_adapter_factory,"nested_factory_code_hash":nested_hash}));
            for i in 0..length.to::<u64>() {
                let adapter = self
                    .runtime
                    .call(
                        vault,
                        &VaultView::adaptersCall {
                            index: U256::from(i),
                        },
                        pin,
                    )
                    .await?;
                let (market, nested, code_hash) = tokio::try_join!(
                    async {
                        self.runtime
                            .call(
                                market_factory,
                                &AdapterFactoryView::isMorphoMarketV1AdapterV2Call {
                                    account: adapter,
                                },
                                pin,
                            )
                            .await
                    },
                    async {
                        self.runtime
                            .call(
                                nested_factory,
                                &AdapterFactoryView::isMorphoVaultV1AdapterCall {
                                    account: adapter,
                                },
                                pin,
                            )
                            .await
                    },
                    async { self.morpho_code_hash(adapter, block).await }
                )?;
                let kind = if market {
                    Some("MorphoMarketV1AdapterV2")
                } else if nested {
                    Some("MorphoVaultV1Adapter")
                } else {
                    None
                };
                if let Some(kind) = kind {
                    require(
                        self.runtime
                            .call(adapter, &AdapterView::parentVaultCall {}, pin)
                            .await?
                            == vault,
                        "MORPHO_ADAPTER_MISMATCH",
                        "Reviewed adapter belongs to another parent vault.",
                    )?;
                    if adapter == liquidity_adapter {
                        liquidity_kind = Some(kind);
                    }
                } else {
                    reasons.push(format!("Adapter {} is not a member of either reviewed bundle-compatible adapter factory.",lower_address(adapter)));
                }
                adapters.push(json!({"address":lower_address(adapter),"kind":kind,"code_hash":code_hash,"supported":kind.is_some()}));
            }
        }
        if !liquidity_adapter.is_zero() && liquidity_kind.is_none() {
            reasons.push(
                "Liquidity adapter is not a reviewed member of the active adapter list.".into(),
            );
        }
        // Gates may inspect VaultBundlesV1's transient initiator. An ordinary
        // view call cannot recreate that execution context, so it is advisory.
        let gate_owner = owner.unwrap_or(Address::ZERO);
        let (send_assets, receive_assets, send_shares, receive_shares) = tokio::join!(
            async {
                self.runtime
                    .call(
                        vault,
                        &VaultView::canSendAssetsCall { account: bundles },
                        pin,
                    )
                    .await
                    .ok()
            },
            async {
                self.runtime
                    .call(
                        vault,
                        &VaultView::canReceiveAssetsCall { account: bundles },
                        pin,
                    )
                    .await
                    .ok()
            },
            async {
                self.runtime
                    .call(
                        vault,
                        &VaultView::canSendSharesCall {
                            account: gate_owner,
                        },
                        pin,
                    )
                    .await
                    .ok()
            },
            async {
                self.runtime
                    .call(
                        vault,
                        &VaultView::canReceiveSharesCall {
                            account: gate_owner,
                        },
                        pin,
                    )
                    .await
                    .ok()
            }
        );
        let deposit_allowed = (gates[0].is_zero() && gates[3].is_zero()).then_some(true);
        let withdrawal_allowed = (gates[1].is_zero() && gates[2].is_zero()).then_some(true);
        let mut deposit_capacity = Some(U256::from(u128::MAX).saturating_sub(total_assets));
        let mut withdraw_capacity = idle_assets;
        let mut complete = true;
        let mut notes: Vec<String> = vec![];
        let mut cap_ids = vec![];
        if let Some(kind) = liquidity_kind {
            if kind == "MorphoVaultV1Adapter" {
                if !liquidity_data.is_empty() {
                    reasons.push(
                        "Nested vault liquidity adapter requires empty liquidity data.".into(),
                    );
                }
                let nested = self
                    .runtime
                    .call(liquidity_adapter, &AdapterView::morphoVaultV1Call {}, pin)
                    .await?;
                let (deposit, withdraw, ids) = tokio::try_join!(
                    async {
                        self.runtime
                            .call(
                                nested,
                                &NestedVaultView::maxDepositCall {
                                    owner: liquidity_adapter,
                                },
                                pin,
                            )
                            .await
                    },
                    async {
                        self.runtime
                            .call(
                                nested,
                                &NestedVaultView::maxWithdrawCall {
                                    owner: liquidity_adapter,
                                },
                                pin,
                            )
                            .await
                    },
                    async {
                        self.runtime
                            .call(liquidity_adapter, &NestedAdapterView::idsCall {}, pin)
                            .await
                    }
                )?;
                deposit_capacity = deposit_capacity.map(|n| n.min(deposit));
                withdraw_capacity = withdraw_capacity.saturating_add(withdraw);
                cap_ids = ids;
                identities.push(json!({"nested_vault":lower_address(nested),"nested_vault_code_hash":self.morpho_code_hash(nested,block).await?}));
                complete = false;
                notes.push("Nested vault capacity can change during accrual; exact bundle simulation remains required.".into());
            } else {
                let params = MarketParams::abi_decode(&liquidity_data)
                    .map_err(|_| read_error("Market liquidity adapter data is malformed."))?;
                require(
                    params.loanToken == asset,
                    "MORPHO_ADAPTER_MISMATCH",
                    "Liquidity market loan token differs from the vault asset.",
                )?;
                let market_id = keccak256(params.abi_encode());
                let morpho = self
                    .runtime
                    .call(liquidity_adapter, &AdapterView::morphoCall {}, pin)
                    .await?;
                let (supply, market, ids) = tokio::try_join!(
                    async {
                        self.runtime
                            .call(
                                liquidity_adapter,
                                &AdapterView::expectedSupplyAssetsCall {
                                    marketId: market_id,
                                },
                                pin,
                            )
                            .await
                    },
                    async {
                        self.runtime
                            .call(morpho, &BlueView::marketCall { id: market_id }, pin)
                            .await
                    },
                    async {
                        self.runtime
                            .call(
                                liquidity_adapter,
                                &MarketAdapterView::idsCall {
                                    marketParams: params,
                                },
                                pin,
                            )
                            .await
                    }
                )?;
                let liquid = U256::from(
                    market
                        .totalSupplyAssets
                        .saturating_sub(market.totalBorrowAssets),
                );
                withdraw_capacity = withdraw_capacity.saturating_add(supply.min(liquid));
                cap_ids = ids;
                complete = false;
                notes.push("Market liquidity uses stored supply less borrow, with current adapter supply assets; interest, rounding and same-transaction allocation can change the exact amount.".into());
            }
        }
        let mut caps = vec![];
        for id in cap_ids {
            let (absolute, relative, allocation) = tokio::try_join!(
                async {
                    self.runtime
                        .call(vault, &VaultView::absoluteCapCall { id }, pin)
                        .await
                },
                async {
                    self.runtime
                        .call(vault, &VaultView::relativeCapCall { id }, pin)
                        .await
                },
                async {
                    self.runtime
                        .call(vault, &VaultView::allocationCall { id }, pin)
                        .await
                }
            )?;
            // VaultV2.allocateInternal uses transient firstTotalAssets from
            // the first pre-deposit accrual (source lines 45–54, 554, 610–613),
            // not total assets after the new deposit.
            let relative_limit = if relative == U256::from(1_000_000_000_000_000_000u64) {
                U256::MAX
            } else {
                total_assets
                    .checked_mul(relative)
                    .ok_or_else(|| read_error("Relative capacity overflow."))?
                    / U256::from(1_000_000_000_000_000_000u64)
            };
            let remaining = absolute.min(relative_limit).saturating_sub(allocation);
            deposit_capacity = deposit_capacity.map(|n| n.min(remaining));
            caps.push(json!({"id":id.to_string(),"absolute":quantity(absolute),"relative_wad":quantity(relative),"allocation":quantity(allocation),"remaining":quantity(remaining)}));
        }
        // Donations can make idle cash exceed the vault's accounted assets.
        // Standard exits remain bounded by the accounting value, and accrueInterest
        // must fit that value into VaultV2's uint128 storage before any write.
        withdraw_capacity = withdraw_capacity.min(total_assets);
        if total_assets > U256::from(u128::MAX) {
            reasons.push(
                "Current totalAssets exceeds VaultV2's executable uint128 accounting range.".into(),
            );
        }
        let supported = reasons.is_empty();
        if !supported {
            deposit_capacity = None;
            complete = false;
        }
        if deposit_allowed.is_none() || withdrawal_allowed.is_none() {
            complete = false;
            notes.push("Nonzero gates require the exact bundle execution context; standalone gate observations are advisory and cannot establish capacity.".into());
        }
        let owner_withdrawable = owner
            .filter(|_| supported && withdrawal_allowed == Some(true))
            .map(|_| withdraw_capacity.min(owner_assets));
        let capacity = json!({"source":"canonical_rpc","block":block,"complete":complete,"deposit_assets":deposit_capacity.filter(|_|deposit_allowed == Some(true)).map(quantity),"vault_withdrawable_assets":if supported && withdrawal_allowed == Some(true){Some(quantity(withdraw_capacity))}else{None},"owner_withdrawable_assets":owner_withdrawable.map(quantity),"idle_assets":quantity(idle_assets),"deposit_gate_allowed":deposit_allowed,"withdrawal_gate_allowed":withdrawal_allowed,"advisory_gate_observation":{"send_assets":send_assets,"receive_assets":receive_assets,"send_shares":send_shares,"receive_shares":receive_shares,"owner_observed":owner.is_some()},"caps":caps,"notes":notes,"exact_bundle_simulation_required":true});
        let identity = keccak256(serde_json::to_vec(&json!({"identities":identities,"adapters":adapters,"liquidity_adapter":lower_address(liquidity_adapter),"liquidity_data":gofin_evm::hex_bytes(&liquidity_data)})).map_err(|_|read_error("Route identity encoding failed."))?).to_string();
        Ok((
            adapters,
            identity,
            json!({"supported":supported,"reasons":reasons,"deposit_gate_allowed":deposit_allowed,"withdrawal_gate_allowed":withdrawal_allowed}),
            capacity,
        ))
    }

    async fn morpho_code_hash(&self, contract: Address, block: u64) -> Result<String> {
        let bytes = rpc_bytes(
            &self
                .runtime
                .rpc
                .request_checked(
                    "eth_getCode",
                    json!([lower_address(contract), block_tag(Some(block))]),
                    gofin_evm::RpcReadRequirements {
                        pin_block: Some(block),
                        ..Default::default()
                    },
                )
                .await?,
        )?;
        require(
            !bytes.is_empty(),
            "MORPHO_CONTRACT_UNAVAILABLE",
            "Morpho deployment has no code at the observation block.",
        )?;
        Ok(keccak256(bytes).to_string())
    }

    fn morpho_read_owner(&self, input: &MorphoReadInput) -> Result<Option<Address>> {
        let explicit = input
            .owner
            .as_deref()
            .map(api::normalized_address)
            .transpose()?;
        let selected = if let Some(name) = &input.wallet {
            Some(
                self.wallets
                    .iter()
                    .find(|w| w.name == *name)
                    .ok_or_else(|| read_error("The selected wallet is not enrolled."))?,
            )
        } else {
            None
        };
        if let Some(wallet) = selected {
            if let Some(explicit) = &explicit {
                require(
                    explicit.eq_ignore_ascii_case(&wallet.address),
                    "MORPHO_OWNER_MISMATCH",
                    "Explicit owner differs from the selected wallet.",
                )?;
            }
            Ok(Some(address(&wallet.address)?))
        } else {
            explicit.as_deref().map(address).transpose()
        }
    }

    pub(crate) async fn morpho_read(&self, operation: &str, input: &Value) -> Result<Value> {
        let input: MorphoReadInput = serde_json::from_value(input.clone())
            .map_err(|_| read_error("Invalid Morpho read input."))?;
        require(
            matches!(input.network.as_str(), "ethereum" | "evm")
                && input.chain_id == self.config.chain_id,
            "MORPHO_NETWORK_MISMATCH",
            "The selected adapter and Morpho read network differ.",
        )?;
        profile::profile(input.chain_id)?;
        require(
            (1..=100).contains(&input.limit),
            "MORPHO_INPUT_INVALID",
            "Page limit must be between 1 and 100.",
        )?;
        let owner = self.morpho_read_owner(&input)?;
        match operation {
            "morpho.vaults" => {
                let page = self
                    .morpho_api
                    .vaults(
                        input.chain_id,
                        input.limit,
                        input.cursor.as_deref(),
                        input.asset.as_deref(),
                    )
                    .await?;
                Ok(
                    json!({"protocol":"morpho","version":2,"chain_id":input.chain_id,"observed_at":self.clock.now().to_rfc3339(),"source":"https://api.morpho.org/v0/vaults-v2","verification":"indexed_metadata_only","vaults":page.data,"cursor":page.cursor,"coverage":{"complete":false,"page_complete":page.cursor.is_none(),"scope":"listed V2 vaults only; explicit vault inspection also supports unlisted factory deployments"},"next_action":"Inspect morpho.vault for independent on-chain identity and current state before preparing a write."}),
                )
            }
            "morpho.vault" => {
                let vault = input
                    .vault
                    .as_deref()
                    .ok_or_else(|| read_error("A vault address is required."))?;
                let state = self
                    .morpho_vault_state(vault, owner, input.block_number)
                    .await;
                let mut warnings = Vec::new();
                let details = optional_metadata(
                    self.morpho_api.vault(input.chain_id, vault).await,
                    &mut warnings,
                    "vault metadata",
                );
                let indexed = optional_metadata(
                    self.morpho_api.state(input.chain_id, vault).await,
                    &mut warnings,
                    "indexed capacity",
                );
                let analytics = optional_metadata(
                    self.morpho_api.analytics(input.chain_id, vault).await,
                    &mut warnings,
                    "yield and rewards",
                );
                let allocations = optional_metadata(
                    self.morpho_api.allocations(input.chain_id, vault).await,
                    &mut warnings,
                    "indexed allocations",
                );
                let yields = yield_metadata(analytics.as_ref());
                let (state, capacity, write_availability, fees) = match state {
                    Ok(state) => (
                        Some(state.clone()),
                        state.capacity,
                        state.write_compatibility,
                        state.fees,
                    ),
                    Err(error) => (
                        None,
                        json!({"complete":false}),
                        json!({"supported":false,"reason":error}),
                        Value::Null,
                    ),
                };
                let allocations =
                    allocation_metadata(allocations, state.as_ref().map(|state| state.block));
                Ok(
                    json!({"protocol":"morpho","version":2,"chain_id":input.chain_id,"observed_at":self.clock.now().to_rfc3339(),"state":state,"verification":if state.is_some(){"canonical_rpc_factory_membership_and_reviewed_deployments"}else{"unverified"},"write_availability":write_availability,"yield":yields,"fees":fees,"allocations":allocations,"indexed":{"provenance":"unverified_morpho_api","details":details,"state":indexed,"analytics":analytics},"capacity":capacity,"warnings":warnings}),
                )
            }
            "morpho.positions" => {
                let owner = owner.ok_or_else(|| {
                    read_error("Select an enrolled wallet or an explicit public owner address.")
                })?;
                let (candidates, cursor, scope) = if let Some(vault) = &input.vault {
                    (
                        vec![api::normalized_address(vault)?],
                        None,
                        "explicit V2 vault",
                    )
                } else {
                    let page = self
                        .morpho_api
                        .positions(
                            input.chain_id,
                            &lower_address(owner),
                            input.limit,
                            input.cursor.as_deref(),
                        )
                        .await?;
                    (
                        page.data.into_iter().map(|p| p.vault_address).collect(),
                        page.cursor,
                        "indexed active V2 positions; fee wrappers excluded",
                    )
                };
                let mut positions = Vec::new();
                let mut warnings = Vec::new();
                let mut seen = BTreeSet::new();
                for vault in candidates {
                    let vault = api::normalized_address(&vault)?;
                    require(
                        seen.insert(vault.clone()),
                        "MORPHO_API_INVALID",
                        "Indexer returned duplicate vault positions.",
                    )?;
                    let state = match self
                        .morpho_vault_state(&vault, Some(owner), input.block_number)
                        .await
                    {
                        Ok(state) => state,
                        Err(error) => {
                            positions.push(json!({"vault":vault,"state":null,"write_availability":{"supported":false,"reason":error},"cost_basis":null,"observation_complete":false}));
                            continue;
                        }
                    };
                    let earnings = optional_metadata(
                        self.morpho_api
                            .performance(input.chain_id, &vault, &lower_address(owner))
                            .await,
                        &mut warnings,
                        "indexed earnings",
                    );
                    let indexed = optional_metadata(
                        self.morpho_api.state(input.chain_id, &vault).await,
                        &mut warnings,
                        "indexed capacity",
                    );
                    let estimate = indexed
                        .as_ref()
                        .and_then(|v| v.get("withdrawable_assets"))
                        .cloned();
                    positions.push(json!({"vault":vault,"shares":{"asset":state.shares,"atoms":state.owner_shares},"underlying":{"asset":state.asset,"atoms":state.owner_assets},"position_value_basis":"previewRedeem at the canonical observation block; shares and underlying describe one holding, not two assets","block":state.block,"block_hash":state.block_hash,"withdrawable_assets":state.capacity["owner_withdrawable_assets"],"indexed_vault_withdrawable_assets":estimate,"withdrawal_capacity_complete":state.capacity["complete"],"withdrawal_capacity_note":"Fresh RPC capacity is a point-in-time observation; exact standard-exit bundle simulation is required.","cost_basis":null,"cost_basis_status":"Imported holdings have no verified acquisition basis in this read. Indexed average-cost earnings are estimates, separate from verified journal accounting.","earnings":{"source":"unverified_morpho_api_average_cost","verified":false,"data":earnings},"write_availability":state.write_compatibility,"state":state}));
                }
                Ok(
                    json!({"protocol":"morpho","version":2,"chain_id":input.chain_id,"owner":lower_address(owner),"observed_at":self.clock.now().to_rfc3339(),"positions":positions,"cursor":cursor,"coverage":{"complete":false,"page_complete":cursor.is_none(),"scope":scope,"reason":"Indexer enumeration can lag the chain; use explicit vault inspection for omitted holdings."},"warnings":warnings}),
                )
            }
            _ => Err(read_error("Unsupported Morpho vault read operation.")),
        }
    }
}
fn allocation_metadata(data: Option<Value>, canonical_block: Option<u64>) -> Value {
    let indexed_block = data
        .as_ref()
        .and_then(|data| data["last_indexed_block"].as_str())
        .and_then(|block| block.parse::<u64>().ok());
    let relation = match (indexed_block, canonical_block) {
        (Some(indexed), Some(canonical)) if indexed == canonical => "same_block",
        (Some(indexed), Some(canonical)) if indexed < canonical => "behind_observation",
        (Some(_), Some(_)) => "ahead_of_observation",
        _ if data.is_none() => "unavailable",
        _ => "unknown",
    };
    json!({
        "source":"unverified_morpho_rest_allocations",
        "verified":false,
        "coverage":{"complete":false,"scope":"API-reported adapter, market and shared cap allocations; not independently enumerated on chain"},
        "freshness":{"indexed_block":indexed_block,"canonical_observation_block":canonical_block,"block_relation":relation,"lag_blocks":indexed_block.zip(canonical_block).and_then(|(indexed,canonical)|canonical.checked_sub(indexed)),"indexed_timestamp":null},
        "accounting_note":"Adapter, market and unscoped cap rows overlap. Cap IDs identify allocation constraints, not additive holdings; do not sum these rows. Use the separately reported vault totalAssets for total backing.",
        "data":data,
    })
}
fn yield_metadata(value: Option<&Value>) -> Value {
    let vault = value
        .and_then(|v| v.get("vaultV2ByAddress"))
        .filter(|v| v.is_object());
    let complete = vault.is_some_and(|v| {
        ["apy", "netApy", "netApyExcludingRewards"]
            .iter()
            .all(|field| v[*field].is_number())
            && v["rewards"].is_array()
    });
    json!({"source":"unverified_morpho_graphql","base_gross_apy":vault.and_then(|v|v.get("apy")),"base_net_apy":vault.and_then(|v|v.get("netApyExcludingRewards")),"total_net_apy_including_incentives":vault.and_then(|v|v.get("netApy")),"incentives":vault.and_then(|v|v.get("rewards")),"incentive_status":"APR estimates are not claimable entitlement; inspect morpho.rewards for cumulative Merkl claims.","complete":complete})
}
fn optional_metadata<T: Serialize>(
    result: Result<T>,
    warnings: &mut Vec<Value>,
    label: &str,
) -> Option<Value> {
    match result {
        Ok(value) => match serde_json::to_value(value) {
            Ok(value) => Some(value),
            Err(_) => {
                warnings.push(json!({"observation":label,"status":"unavailable","reason":"Response could not be encoded."}));
                None
            }
        },
        Err(error) => {
            warnings.push(json!({"observation":label,"status":"unavailable","error":error}));
            None
        }
    }
}
fn read_error(message: &str) -> GoFinError {
    error("MORPHO_READ_INVALID", "read", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn machine_inputs_are_exact_and_do_not_accept_runtime_routes() {
        assert!(
            serde_json::from_value::<MorphoReadInput>(
                json!({"chain_id":1,"rpc_url":"https://example.org"})
            )
            .is_err()
        );
        let input: MorphoReadInput = serde_json::from_value(json!({"chain_id":8453})).unwrap();
        assert_eq!(input.network, "ethereum");
        assert_eq!(input.limit, 20);
    }
    #[test]
    fn missing_observation_remains_unknown_with_error() {
        let mut warnings = vec![];
        let value =
            optional_metadata::<Value>(Err(read_error("unavailable")), &mut warnings, "earnings");
        assert!(value.is_none());
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0]["status"], "unavailable");
    }
    #[test]
    fn allocation_metadata_labels_freshness_and_never_sums_overlapping_scopes() {
        let data = json!({"last_indexed_block":"100","allocations":[{"caps":[{"cap_id":"adapter","allocated_assets":"7"},{"cap_id":"market","allocated_assets":"7"}]}],"unscoped_caps":[{"cap_id":"collateral","allocated_assets":"7"}]});
        let metadata = allocation_metadata(Some(data.clone()), Some(105));
        assert_eq!(metadata["data"], data);
        assert_eq!(metadata["freshness"]["lag_blocks"], 5);
        assert_eq!(
            metadata["freshness"]["block_relation"],
            "behind_observation"
        );
        assert_eq!(metadata["coverage"]["complete"], false);
        assert_eq!(metadata["verified"], false);
        assert!(metadata.get("total_assets").is_none());
        let historical = allocation_metadata(Some(data.clone()), Some(90));
        assert_eq!(
            historical["freshness"]["block_relation"],
            "ahead_of_observation"
        );
        assert_eq!(historical["freshness"]["lag_blocks"], Value::Null);
        let same = allocation_metadata(Some(data), Some(100));
        assert_eq!(same["freshness"]["block_relation"], "same_block");
    }
    #[test]
    fn unavailable_allocation_metadata_stays_unknown_with_warning() {
        let mut warnings = vec![];
        let data = optional_metadata::<api::VaultAllocations>(
            Err(read_error("Synthetic allocation indexer outage.")),
            &mut warnings,
            "indexed allocations",
        );
        let metadata = allocation_metadata(data, Some(100));
        assert_eq!(metadata["data"], Value::Null);
        assert_eq!(metadata["freshness"]["block_relation"], "unavailable");
        assert_eq!(metadata["coverage"]["complete"], false);
        assert_eq!(warnings[0]["observation"], "indexed allocations");
        assert_eq!(warnings[0]["status"], "unavailable");
    }
    #[test]
    fn missing_or_null_yields_are_incomplete_unknown_not_zero() {
        for value in [
            None,
            Some(json!({"vaultV2ByAddress":null})),
            Some(json!({"vaultV2ByAddress":{"apy":null,"rewards":null}})),
        ] {
            let summary = yield_metadata(value.as_ref());
            assert_eq!(summary["complete"], false);
            assert_eq!(summary["base_gross_apy"], Value::Null);
            assert_eq!(summary["incentives"], Value::Null);
        }
    }
}
