use fin_evm::alloy::sol;
sol! {
    interface Erc20 {
        function decimals() external view returns (uint8 value);
        function symbol() external view returns (string value);
        function balanceOf(address owner) external view returns (uint256 value);
        function totalSupply() external view returns (uint256 value);
        function allowance(address owner,address spender) external view returns (uint256 value);
        function transfer(address to,uint256 amount) external returns (bool value);
        function approve(address spender,uint256 amount) external returns (bool value);
        event Transfer(address indexed from,address indexed to,uint256 value);
        event Approval(address indexed owner,address indexed spender,uint256 value);
    }
    interface Bytes32Symbol { function symbol() external view returns (bytes32 value); }
    interface Weth {
        function deposit() external payable;
        function withdraw(uint256 amount) external;
        event Deposit(address indexed dst,uint256 wad);
        event Withdrawal(address indexed src,uint256 wad);
    }
    interface Factory { function getPool(address tokenA,address tokenB,uint24 fee) external view returns (address value); }
    interface Quoter {
        struct QuoteParams { address tokenIn; address tokenOut; uint256 amountIn; uint24 fee; uint160 sqrtPriceLimitX96; }
        function quoteExactInputSingle(QuoteParams params) external returns (uint256 amountOut,uint160 sqrtPriceX96After,uint32 initializedTicksCrossed,uint256 gasEstimate);
    }
    interface Router {
        struct SwapParams { address tokenIn; address tokenOut; uint24 fee; address recipient; uint256 deadline; uint256 amountIn; uint256 amountOutMinimum; uint160 sqrtPriceLimitX96; }
        function exactInputSingle(SwapParams params) external payable returns (uint256 amountOut);
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
    interface UniswapPool {
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
