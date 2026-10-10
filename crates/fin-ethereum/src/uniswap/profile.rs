//! Closed, ABI-specific AMM deployment descriptions.
//!
//! Profiles are constructed only from reviewed deployment configuration. The
//! public config and operation bindings remain unchanged; this internal layer
//! keeps a DEX's factory, quote, fee, and router assumptions together instead
//! of spreading them through quote and preparation code.

use crate::{RamsesConfig, RouterKind, UniswapConfig};

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
        }
    }
}
