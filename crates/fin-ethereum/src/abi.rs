// This module consists solely of deployed ABI declarations. Alloy-generated
// methods must preserve the protocols' fixed positional function signatures.
#![allow(clippy::too_many_arguments)]

use fin_evm::alloy::sol;
sol! {
    interface Erc20 {
        function decimals() external view returns (uint8 value);
        function symbol() external view returns (string value);
        function name() external view returns (string value);
        function version() external view returns (string value);
        function balanceOf(address owner) external view returns (uint256 value);
        function totalSupply() external view returns (uint256 value);
        function allowance(address owner,address spender) external view returns (uint256 value);
        function nonces(address owner) external view returns (uint256 value);
        function DOMAIN_SEPARATOR() external view returns (bytes32 value);
        function transfer(address to,uint256 amount) external returns (bool value);
        function approve(address spender,uint256 amount) external returns (bool value);
        event Transfer(address indexed from,address indexed to,uint256 value);
        event Approval(address indexed owner,address indexed spender,uint256 value);
    }
    interface Permit2 {
        function allowance(address owner,address token,address spender) external view returns (uint160 amount,uint48 expiration,uint48 nonce);
        event Permit(address indexed owner,address indexed token,address indexed spender,uint160 amount,uint48 expiration,uint48 nonce);
    }
    interface Bytes32Symbol { function symbol() external view returns (bytes32 value); }
    interface Weth {
        function deposit() external payable;
        function withdraw(uint256 amount) external;
        event Deposit(address indexed dst,uint256 wad);
        event Withdrawal(address indexed src,uint256 wad);
    }
    interface Factory { function getPool(address tokenA,address tokenB,uint24 fee) external view returns (address value); }
    interface RamsesFactory {
        function getPool(address tokenA,address tokenB,int24 tickSpacing) external view returns (address value);
        function ramsesV3PoolDeployer() external view returns (address value);
    }
    interface Quoter {
        struct QuoteParams { address tokenIn; address tokenOut; uint256 amountIn; uint24 fee; uint160 sqrtPriceLimitX96; }
        function quoteExactInputSingle(QuoteParams params) external returns (uint256 amountOut,uint160 sqrtPriceX96After,uint32 initializedTicksCrossed,uint256 gasEstimate);
    }
    interface RamsesQuoter {
        struct QuoteParams { address tokenIn; address tokenOut; uint256 amountIn; int24 tickSpacing; uint160 sqrtPriceLimitX96; }
        function quoteExactInputSingle(QuoteParams params) external returns (uint256 amountOut,uint160 sqrtPriceX96After,uint32 initializedTicksCrossed,uint256 gasEstimate);
        function deployer() external view returns (address value);
    }
    interface Router {
        struct SwapParams { address tokenIn; address tokenOut; uint24 fee; address recipient; uint256 deadline; uint256 amountIn; uint256 amountOutMinimum; uint160 sqrtPriceLimitX96; }
        function exactInputSingle(SwapParams params) external payable returns (uint256 amountOut);
        function selfPermit(address token,uint256 value,uint256 deadline,uint8 v,bytes32 r,bytes32 s) external;
    }
    interface SwapRouter02 {
        struct SwapParams { address tokenIn; address tokenOut; uint24 fee; address recipient; uint256 amountIn; uint256 amountOutMinimum; uint160 sqrtPriceLimitX96; }
        function exactInputSingle(SwapParams params) external payable returns (uint256 amountOut);
        function selfPermit(address token,uint256 value,uint256 deadline,uint8 v,bytes32 r,bytes32 s) external;
        function multicall(uint256 deadline, bytes[] data) external payable returns (bytes[] results);
        function unwrapWETH9(uint256 amountMinimum,address recipient) external payable;
    }
    interface LegacyRouterPayments {
        function multicall(bytes[] data) external payable returns (bytes[] results);
        function unwrapWETH9(uint256 amountMinimum,address recipient) external payable;
    }
    interface V3RouterInfo { function WETH9() external view returns (address value); }
    interface RamsesRouter {
        struct SwapParams { address tokenIn; address tokenOut; int24 tickSpacing; address recipient; uint256 deadline; uint256 amountIn; uint256 amountOutMinimum; uint160 sqrtPriceLimitX96; }
        function exactInputSingle(SwapParams params) external payable returns (uint256 amountOut);
        function selfPermit(address token,uint256 value,uint256 deadline,uint8 v,bytes32 r,bytes32 s) external;
        function multicall(bytes[] data) external payable returns (bytes[] results);
        function unwrapWETH9(uint256 amountMinimum,address recipient) external payable;
        function deployer() external view returns (address value);
    }
    interface RamsesPool {
        function token0() external view returns (address value);
        function token1() external view returns (address value);
        function fee() external view returns (uint24 value);
        function tickSpacing() external view returns (int24 value);
        function slot0() external view returns (uint160 sqrtPriceX96,int24 tick,uint16 observationIndex,uint16 observationCardinality,uint16 observationCardinalityNext,uint8 feeProtocol,bool unlocked);
        event Swap(address indexed sender,address indexed recipient,int256 amount0,int256 amount1,uint160 sqrtPriceX96,uint128 liquidity,int24 tick);
    }
    interface V4Quoter {
        struct PoolKey { address currency0; address currency1; uint24 fee; int24 tickSpacing; address hooks; }
        struct QuoteExactSingleParams { PoolKey poolKey; bool zeroForOne; uint128 exactAmount; bytes hookData; }
        function quoteExactInputSingle(QuoteExactSingleParams params) external returns (uint256 amountOut,uint256 gasEstimate);
    }
    interface UniversalRouter {
        struct V4ExactInputSingle { V4Quoter.PoolKey poolKey; bool zeroForOne; uint128 amountIn; uint128 amountOutMinimum; uint256 minHopPriceX36; bytes hookData; }
        struct V4ExactInputSingleLegacy { V4Quoter.PoolKey poolKey; bool zeroForOne; uint128 amountIn; uint128 amountOutMinimum; bytes hookData; }
        function execute(bytes commands,bytes[] inputs,uint256 deadline) external payable;
        function encodeV4Swap(bytes actions,bytes[] params) external pure;
        function encodeV4Single(V4Quoter.PoolKey poolKey,bool zeroForOne,uint128 amountIn,uint128 amountOutMinimum,uint256 minHopPriceX36,bytes hookData) external pure;
        function encodeV4SingleLegacy(V4Quoter.PoolKey poolKey,bool zeroForOne,uint128 amountIn,uint128 amountOutMinimum,bytes hookData) external pure;
        function encodeSettleAll(address currency,uint256 maximumAmount) external pure;
        function encodeTakeAll(address currency,uint256 minimumAmount) external pure;
    }
    interface V4PoolManager {
        event Swap(bytes32 indexed id,address indexed sender,int128 amount0,int128 amount1,uint160 sqrtPriceX96,uint128 liquidity,int24 tick,uint24 fee);
    }
    interface V4StateView {
        function getSlot0(bytes32 poolId) external view returns (uint160 sqrtPriceX96,int24 tick,uint24 protocolFee,uint24 lpFee);
    }
    interface Pool {
        function ADDRESSES_PROVIDER() external view returns (address value);
        function getReservesList() external view returns (address[] value);
        function getReserveNormalizedIncome(address asset) external view returns (uint256 value);
        function getReserveNormalizedVariableDebt(address asset) external view returns (uint256 value);
        function getUserAccountData(address user) external view returns (uint256 totalCollateralBase,uint256 totalDebtBase,uint256 availableBorrowsBase,uint256 currentLiquidationThreshold,uint256 ltv,uint256 healthFactor);
        function supply(address asset,uint256 amount,address onBehalfOf,uint16 referralCode) external;
        function borrow(address asset,uint256 amount,uint256 interestRateMode,uint16 referralCode,address onBehalfOf) external;
        function repay(address asset,uint256 amount,uint256 interestRateMode,address onBehalfOf) external returns (uint256 value);
        function withdraw(address asset,uint256 amount,address to) external returns (uint256 value);
        function setUserUseReserveAsCollateral(address asset,bool useAsCollateral) external;
        event Supply(address indexed reserve,address user,address indexed onBehalfOf,uint256 amount,uint16 indexed referralCode);
        event Borrow(address indexed reserve,address user,address indexed onBehalfOf,uint256 amount,uint8 interestRateMode,uint256 borrowRate,uint16 indexed referralCode);
        event Repay(address indexed reserve,address indexed user,address indexed repayer,uint256 amount,bool useATokens);
        event Withdraw(address indexed reserve,address indexed user,address indexed to,uint256 amount);
        event ReserveUsedAsCollateralEnabled(address indexed reserve,address indexed user);
        event ReserveUsedAsCollateralDisabled(address indexed reserve,address indexed user);
    }
    interface AToken {
        function ATOKEN_REVISION() external view returns (uint256 value);
        function DEBT_TOKEN_REVISION() external view returns (uint256 value);
        function scaledBalanceOf(address user) external view returns (uint256 value);
        function getPreviousIndex(address user) external view returns (uint256 value);
        event Mint(address indexed caller,address indexed onBehalfOf,uint256 value,uint256 balanceIncrease,uint256 index);
    }
    interface PancakePool {
        function slot0() external view returns (uint160 sqrtPriceX96,int24 tick,uint16 observationIndex,uint16 observationCardinality,uint16 observationCardinalityNext,uint32 feeProtocol,bool unlocked);
        event Swap(address indexed sender,address indexed recipient,int256 amount0,int256 amount1,uint160 sqrtPriceX96,uint128 liquidity,int24 tick,uint128 protocolFeesToken0,uint128 protocolFeesToken1);
    }
    interface V3Refund { function refundETH() external payable; }
    interface V3DeploymentInfo {
        function factory() external view returns (address value);
        function deployer() external view returns (address value);
        function WETH9() external view returns (address value);
    }
    interface UniswapPool {
        function factory() external view returns (address value);
        function fee() external view returns (uint24 value);
        function token0() external view returns (address value);
        function token1() external view returns (address value);
        function slot0() external view returns (uint160 sqrtPriceX96,int24 tick,uint16 observationIndex,uint16 observationCardinality,uint16 observationCardinalityNext,uint8 feeProtocol,bool unlocked);
        event Swap(address indexed sender,address indexed recipient,int256 amount0,int256 amount1,uint160 sqrtPriceX96,uint128 liquidity,int24 tick);
    }
    interface AddressesProvider {
        function getPoolDataProvider() external view returns (address value);
        function getPool() external view returns (address value);
        function getPriceOracle() external view returns (address value);
    }
    interface DataProvider {
        function getReserveConfigurationData(address asset) external view returns (uint256 decimals,uint256 ltv,uint256 liquidationThreshold,uint256 liquidationBonus,uint256 reserveFactor,bool usageAsCollateralEnabled,bool borrowingEnabled,bool stableBorrowRateEnabled,bool isActive,bool isFrozen);
        function getReserveTokensAddresses(address asset) external view returns (address aTokenAddress,address stableDebtTokenAddress,address variableDebtTokenAddress);
        function getReserveCaps(address asset) external view returns (uint256 borrowCap,uint256 supplyCap);
        function getPaused(address asset) external view returns (bool value);
        function getUserReserveData(address asset,address user) external view returns (uint256 currentATokenBalance,uint256 currentStableDebt,uint256 currentVariableDebt,uint256 principalStableDebt,uint256 scaledVariableDebt,uint256 stableBorrowRate,uint256 liquidityRate,uint40 stableRateLastUpdated,bool usageAsCollateralEnabled);
    }
    interface Oracle {
        function getAssetPrice(address asset) external view returns (uint256 value);
        function getSourceOfAsset(address asset) external view returns (address value);
        function BASE_CURRENCY() external view returns (address value);
        function BASE_CURRENCY_UNIT() external view returns (uint256 value);
    }
    interface PriceFeed {
        function latestRoundData() external view returns (uint80 roundId,int256 answer,uint256 startedAt,uint256 updatedAt,uint80 answeredInRound);
        function ASSET_TO_USD_AGGREGATOR() external view returns (address value);
    }
}
