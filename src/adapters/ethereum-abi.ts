import { parseAbi } from 'viem';

export const WETH = '0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2' as const;
export const USDC = '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48' as const;
export const erc20Abi = parseAbi([
  'function decimals() view returns (uint8)',
  'function symbol() view returns (string)',
  'function balanceOf(address) view returns (uint256)',
  'function totalSupply() view returns (uint256)',
  'function allowance(address,address) view returns (uint256)',
  'function transfer(address,uint256) returns (bool)',
  'function approve(address,uint256) returns (bool)',
  'event Transfer(address indexed from,address indexed to,uint256 value)',
  'event Approval(address indexed owner,address indexed spender,uint256 value)',
]);
// Older ERC-20 contracts, including MKR, expose a fixed-width display label.
export const erc20Bytes32SymbolAbi = parseAbi(['function symbol() view returns (bytes32)']);
export const wethAbi = parseAbi([
  'function deposit() payable',
  'function withdraw(uint256)',
  'event Deposit(address indexed dst,uint256 wad)',
  'event Withdrawal(address indexed src,uint256 wad)',
]);
export const factoryAbi = parseAbi([
  'function getPool(address,address,uint24) view returns (address)',
]);
export const quoterAbi = parseAbi([
  'function quoteExactInputSingle((address tokenIn,address tokenOut,uint256 amountIn,uint24 fee,uint160 sqrtPriceLimitX96) params) returns (uint256 amountOut,uint160 sqrtPriceX96After,uint32 initializedTicksCrossed,uint256 gasEstimate)',
]);
export const routerAbi = parseAbi([
  'function exactInputSingle((address tokenIn,address tokenOut,uint24 fee,address recipient,uint256 deadline,uint256 amountIn,uint256 amountOutMinimum,uint160 sqrtPriceLimitX96) params) payable returns (uint256 amountOut)',
]);
export const poolAbi = parseAbi([
  'function ADDRESSES_PROVIDER() view returns (address)',
  'function getReservesList() view returns (address[])',
  'function getReserveNormalizedIncome(address) view returns (uint256)',
  'function getUserAccountData(address) view returns (uint256 totalCollateralBase,uint256 totalDebtBase,uint256 availableBorrowsBase,uint256 currentLiquidationThreshold,uint256 ltv,uint256 healthFactor)',
  'function supply(address asset,uint256 amount,address onBehalfOf,uint16 referralCode)',
  'function borrow(address asset,uint256 amount,uint256 interestRateMode,uint16 referralCode,address onBehalfOf)',
  'function repay(address asset,uint256 amount,uint256 interestRateMode,address onBehalfOf) returns (uint256)',
  'function withdraw(address asset,uint256 amount,address to) returns (uint256)',
  'function setUserUseReserveAsCollateral(address asset,bool useAsCollateral)',
  'event Supply(address indexed reserve,address user,address indexed onBehalfOf,uint256 amount,uint16 indexed referralCode)',
  'event Borrow(address indexed reserve,address user,address indexed onBehalfOf,uint256 amount,uint8 interestRateMode,uint256 borrowRate,uint16 indexed referralCode)',
  'event Repay(address indexed reserve,address indexed user,address indexed repayer,uint256 amount,bool useATokens)',
  'event Withdraw(address indexed reserve,address indexed user,address indexed to,uint256 amount)',
  'event ReserveUsedAsCollateralEnabled(address indexed reserve,address indexed user)',
  'event ReserveUsedAsCollateralDisabled(address indexed reserve,address indexed user)',
]);
export const aTokenAbi = parseAbi([
  'function ATOKEN_REVISION() view returns (uint256)',
  'function scaledBalanceOf(address) view returns (uint256)',
  'function getPreviousIndex(address) view returns (uint256)',
  'event Mint(address indexed caller,address indexed onBehalfOf,uint256 value,uint256 balanceIncrease,uint256 index)',
]);
export const addressesProviderAbi = parseAbi([
  'function getPoolDataProvider() view returns (address)',
  'function getPool() view returns (address)',
  'function getPriceOracle() view returns (address)',
]);
export const dataProviderAbi = parseAbi([
  'function getReserveConfigurationData(address) view returns (uint256 decimals,uint256 ltv,uint256 liquidationThreshold,uint256 liquidationBonus,uint256 reserveFactor,bool usageAsCollateralEnabled,bool borrowingEnabled,bool stableBorrowRateEnabled,bool isActive,bool isFrozen)',
  'function getReserveTokensAddresses(address) view returns (address aTokenAddress,address stableDebtTokenAddress,address variableDebtTokenAddress)',
  'function getReserveCaps(address) view returns (uint256 borrowCap,uint256 supplyCap)',
  'function getPaused(address) view returns (bool)',
  'function getUserReserveData(address,address) view returns (uint256 currentATokenBalance,uint256 currentStableDebt,uint256 currentVariableDebt,uint256 principalStableDebt,uint256 scaledVariableDebt,uint256 stableBorrowRate,uint256 liquidityRate,uint40 stableRateLastUpdated,bool usageAsCollateralEnabled)',
]);
export const oracleAbi = parseAbi([
  'function getAssetPrice(address) view returns (uint256)',
  'function getSourceOfAsset(address) view returns (address)',
  'function BASE_CURRENCY() view returns (address)',
  'function BASE_CURRENCY_UNIT() view returns (uint256)',
]);
export const priceFeedAbi = parseAbi([
  'function latestRoundData() view returns (uint80 roundId,int256 answer,uint256 startedAt,uint256 updatedAt,uint80 answeredInRound)',
  'function ASSET_TO_USD_AGGREGATOR() view returns (address)',
]);
