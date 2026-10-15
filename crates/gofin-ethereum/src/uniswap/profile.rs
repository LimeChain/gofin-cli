//! Closed, ABI-specific AMM deployment descriptions.
//!
//! Profiles are constructed only from reviewed deployment configuration. The
//! legacy config and operation bindings remain unchanged; this internal layer
//! keeps a DEX's factory, quote, fee, and router assumptions together instead
//! of spreading them through quote and preparation code.

use crate::{
    RamsesConfig, RouterKind, UniswapConfig, V3DeploymentConfig, V3Dex, V3PoolAbi,
    V3RouterMetadataAbi,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FactoryLookup {
    /// `getPool(address,address,uint24)`.
    FeeTierU24,
    /// Ramses CL factory `getPool(address,address,int24 tickSpacing)`.
    TickSpacingI24,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuoterAbi {
    /// QuoterV2 `quoteExactInputSingle(QuoteParams)` with uint24 fee.
    UniswapV3QuoterV2,
    /// Ramses QuoterV2 uses `int24 tickSpacing` instead of `uint24 fee`.
    RamsesV3QuoterV2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PoolFeeSemantics {
    /// The selected factory fee tier is the pool's fixed swap fee.
    FixedFeeTier,
    /// Ramses pools expose a dynamic fee at the pinned observation block.
    DynamicPoolFee,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct V3DeploymentProfile<'a> {
    pub(crate) dex_id: &'static str,
    pub(crate) factory: &'a str,
    pub(crate) quoter: &'a str,
    pub(crate) router: &'a str,
    pub(crate) factory_lookup: FactoryLookup,
    pub(crate) quoter_abi: QuoterAbi,
    pub(crate) pool_fee_semantics: PoolFeeSemantics,
    pub(crate) router_kind: RouterKind,
    pub(crate) pool_deployer: Option<&'a str>,
    pub(crate) pool_abi: V3PoolAbi,
    pub(crate) router_metadata_abi: V3RouterMetadataAbi,
    pub(crate) fee_tiers: &'a [u32],
}

impl<'a> V3DeploymentProfile<'a> {
    /// Derive the current Uniswap v3 profile without changing legacy config
    /// serialization or its router-kind behavior.
    pub(crate) fn uniswap(config: &'a UniswapConfig) -> Self {
        Self {
            dex_id: "uniswap",
            factory: &config.factory,
            quoter: &config.quoter,
            router: &config.router,
            factory_lookup: FactoryLookup::FeeTierU24,
            quoter_abi: QuoterAbi::UniswapV3QuoterV2,
            pool_fee_semantics: PoolFeeSemantics::FixedFeeTier,
            router_kind: config.router_kind,
            pool_deployer: None,
            pool_abi: V3PoolAbi::UniswapV3,
            router_metadata_abi: V3RouterMetadataAbi::UniswapV3,
            fee_tiers: &[100, 500, 3000, 10000],
        }
    }

    pub(crate) fn additional(dex: V3Dex, config: &'a V3DeploymentConfig) -> Self {
        Self {
            dex_id: dex.id(),
            factory: &config.factory,
            quoter: &config.quoter,
            router: &config.router,
            factory_lookup: FactoryLookup::FeeTierU24,
            quoter_abi: QuoterAbi::UniswapV3QuoterV2,
            pool_fee_semantics: PoolFeeSemantics::FixedFeeTier,
            router_kind: config.router_kind,
            pool_deployer: config.pool_deployer.as_deref(),
            pool_abi: config.pool_abi,
            router_metadata_abi: config.router_metadata_abi,
            fee_tiers: &config.allowed_fee_tiers,
        }
    }
    pub(crate) fn ramses(config: &'a RamsesConfig) -> Self {
        Self {
            dex_id: "ramses",
            factory: &config.factory,
            quoter: &config.quoter,
            router: &config.router,
            factory_lookup: FactoryLookup::TickSpacingI24,
            quoter_abi: QuoterAbi::RamsesV3QuoterV2,
            pool_fee_semantics: PoolFeeSemantics::DynamicPoolFee,
            // Ramses supports multicall(bytes[]) and EIP-2612 selfPermit.
            // LegacyV3 is used only as the shared permit wrapper kind; exact
            // swap calldata is encoded by its ABI-specific helper.
            router_kind: RouterKind::LegacyV3,
            pool_deployer: Some(&config.pool_deployer),
            pool_abi: V3PoolAbi::UniswapV3,
            router_metadata_abi: V3RouterMetadataAbi::UniswapV3,
            fee_tiers: &[],
        }
    }
}

/// Resolve operation names once; consumers never select a fallback venue.
pub(crate) fn operation_dex(operation: &str) -> crate::Result<&'static str> {
    match operation {
        "uniswap.quote" | "uniswap.swap" => Ok("uniswap"),
        "ramses.quote" | "ramses.swap" => Ok("ramses"),
        "sushi.quote" | "sushi.swap" => Ok("sushi"),
        "pancakeswap.quote" | "pancakeswap.swap" => Ok("pancakeswap"),
        _ => Err(crate::GoFinError::validation("Unsupported AMM operation.")),
    }
}
pub(crate) fn additional_dex(value: &str) -> Option<crate::V3Dex> {
    match value {
        "sushi" | "sushi.quote" | "sushi.swap" => Some(crate::V3Dex::Sushi),
        "pancakeswap" | "pancakeswap.quote" | "pancakeswap.swap" => Some(crate::V3Dex::PancakeSwap),
        _ => None,
    }
}
