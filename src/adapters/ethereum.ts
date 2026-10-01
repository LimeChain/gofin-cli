import { z } from 'zod';
import {
  createPublicClient,
  http,
  fallback,
  getAddress,
  isAddress,
  encodeFunctionData,
  decodeFunctionResult,
  hexToString,
  decodeEventLog,
  keccak256,
  parseTransaction,
  recoverTransactionAddress,
  formatUnits,
  parseUnits,
  maxUint256,
  zeroAddress,
  type Address,
  type Hex,
  type Abi,
  type PublicClient,
  type WalletClient,
} from 'viem';
import { mainnet } from 'viem/chains';
import type {
  Adapter,
  Definition,
  FinConfig,
  Input,
  Preview,
  RecordOperation,
  Reconciliation,
  Submission,
  SubmissionHooks,
} from '../core/types.js';
import { FinError } from '../core/errors.js';
import {
  WETH,
  USDC,
  erc20Abi,
  erc20Bytes32SymbolAbi,
  wethAbi,
  factoryAbi,
  quoterAbi,
  routerAbi,
  poolAbi,
  aTokenAbi,
  addressesProviderAbi,
  dataProviderAbi,
  oracleAbi,
  priceFeedAbi,
} from './ethereum-abi.js';

const decimal = z
  .string()
  .regex(/^(0|[1-9]\d*)(\.\d+)?$/, 'Use an unsigned decimal string without exponent notation');
const address = z.string().refine((s) => isAddress(s), 'Expected a valid Ethereum address');
const positive = decimal.refine((s) => /[1-9]/.test(s), 'Amount must be positive');
const readBase = {
  wallet: z.string().min(1),
  network: z.literal('ethereum'),
  chainId: z.literal(1),
};
const writeBase = { ...readBase, requestId: z.string().min(1).max(128), maxFeeEth: positive };
const quoteFields = {
  tokenIn: address,
  tokenOut: address,
  amount: positive,
  feeTier: z.union([z.literal(100), z.literal(500), z.literal(3000), z.literal(10000)]).optional(),
};
const schemas = {
  'wallet.balances': z
    .object({ ...readBase, tokens: z.array(address).max(64).default([WETH, USDC]) })
    .strict(),
  'wallet.token': z.object({ ...readBase, token: address }).strict(),
  'wallet.allowance': z.object({ ...readBase, token: address, spender: address }).strict(),
  'wallet.receipt': z
    .object({ ...readBase, transactionHash: z.string().regex(/^0x[0-9a-fA-F]{64}$/) })
    .strict(),
  'uniswap.quote': z.object({ ...readBase, ...quoteFields }).strict(),
  'aave.reserves': z.object({ ...readBase, tokens: z.array(address).max(64).optional() }).strict(),
  'aave.positions': z.object({ ...readBase, tokens: z.array(address).max(64).optional() }).strict(),
  'wallet.transfer': z
    .object({
      ...writeBase,
      token: z.union([z.literal('native'), address]),
      amount: positive,
      recipient: address,
    })
    .strict(),
  'wallet.wrap': z.object({ ...writeBase, amount: positive }).strict(),
  'wallet.unwrap': z.object({ ...writeBase, amount: positive }).strict(),
  'wallet.approve': z
    .object({ ...writeBase, token: address, spender: address, amount: decimal })
    .strict(),
  'uniswap.swap': z
    .object({
      ...writeBase,
      ...quoteFields,
      recipient: address,
      minOutput: positive,
      slippageBps: z.number().int().min(0).max(1000),
      deadline: z.number().int().positive(),
    })
    .strict(),
  'aave.supply': z.object({ ...writeBase, token: address, amount: positive }).strict(),
  'aave.borrow': z
    .object({ ...writeBase, token: address, amount: positive, minHealthFactor: positive })
    .strict(),
  'aave.repay': z
    .object({
      ...writeBase,
      token: address,
      amount: z.union([positive, z.literal('all')]),
      maxAmount: positive.optional(),
    })
    .strict()
    .refine(
      (s) => s.amount !== 'all' || s.maxAmount !== undefined,
      'Repay all requires a finite maxAmount and finite Pool allowance',
    ),
  'aave.withdraw': z
    .object({ ...writeBase, token: address, amount: positive, recipient: address })
    .strict(),
  'aave.collateral': z.object({ ...writeBase, token: address, enabled: z.boolean() }).strict(),
};
type Operation = keyof typeof schemas;
const descriptions: Record<Operation, string> = {
  'wallet.balances':
    'Read native ETH and explicit ERC-20 holdings. Token contract identities are authoritative.',
  'wallet.token': 'Read ERC-20 metadata and verify contract code.',
  'wallet.allowance': 'Read exact spender authority.',
  'wallet.receipt': 'Read transaction receipt and confirmation status.',
  'uniswap.quote':
    'Quote a single Uniswap v3 pool using on-chain QuoterV2; select best supported fee tier if omitted.',
  'aave.reserves':
    'Discover Aave v3 reserve eligibility, caps, receipt/debt tokens, and underlying liquidity.',
  'aave.positions':
    'Read supplied underlying, collateral use, variable debt, and account metrics separately.',
  'wallet.transfer':
    'Transfer exact native ETH or a standard ERC-20 amount to an explicit recipient.',
  'wallet.wrap': 'Wrap exact ETH into WETH9; this is a separate write.',
  'wallet.unwrap': 'Unwrap exact WETH9 into native ETH; this is a separate write.',
  'wallet.approve':
    'Set an exact finite token allowance, including zero for restoration. No unlimited approvals.',
  'uniswap.swap':
    'Uniswap v3 exact-input single-pool swap with explicit minimum output, slippage, recipient and deadline.',
  'aave.supply':
    'Supply exact ERC-20 underlying into Aave v3 on behalf of the signer; separate Pool allowance required.',
  'aave.borrow':
    'Borrow variable-rate debt for the signer; read current collateral metrics and reserve constraints.',
  'aave.repay':
    'Repay variable-rate debt. all uses uint256.max constrained by finite maxAmount allowance and clears debt.',
  'aave.withdraw':
    'Withdraw an exact amount of supplied underlying to an explicit recipient; accrued supply dust may require another approved withdrawal.',
  'aave.collateral': 'Explicitly enable or disable use of one supplied reserve as collateral.',
};
const baseExample = { wallet: 'main-wallet', network: 'ethereum', chainId: 1 };
const writeExample = { ...baseExample, requestId: 'unique-logical-request', maxFeeEth: '0.001' };
const exampleExtra: Record<Operation, Input> = {
  'wallet.balances': { tokens: [WETH, USDC] },
  'wallet.token': { token: USDC },
  'wallet.allowance': { token: USDC, spender: '0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2' },
  'wallet.receipt': { transactionHash: `0x${'0'.repeat(64)}` },
  'uniswap.quote': { tokenIn: WETH, tokenOut: USDC, amount: '0.001' },
  'aave.reserves': { tokens: [WETH, USDC] },
  'aave.positions': { tokens: [WETH, USDC] },
  'wallet.transfer': {
    token: 'native',
    amount: '0.001',
    recipient: '0x0000000000000000000000000000000000000001',
  },
  'wallet.wrap': { amount: '0.001' },
  'wallet.unwrap': { amount: '0.001' },
  'wallet.approve': {
    token: USDC,
    spender: '0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2',
    amount: '1',
  },
  'uniswap.swap': {
    tokenIn: WETH,
    tokenOut: USDC,
    amount: '0.001',
    recipient: '0x0000000000000000000000000000000000000001',
    minOutput: '1',
    slippageBps: 50,
    deadline: Math.floor(Date.now() / 1000) + 120,
  },
  'aave.supply': { token: WETH, amount: '0.001' },
  'aave.borrow': { token: USDC, amount: '1', minHealthFactor: '1.5' },
  'aave.repay': { token: USDC, amount: 'all', maxAmount: '1.01' },
  'aave.withdraw': {
    token: WETH,
    amount: '0.001',
    recipient: '0x0000000000000000000000000000000000000001',
  },
  'aave.collateral': { token: WETH, enabled: true },
};
export const ethereumDefinitions: Definition[] = (Object.keys(schemas) as Operation[]).map(
  (name) => ({
    name,
    description: descriptions[name],
    kind: [
      'wallet.balances',
      'wallet.token',
      'wallet.allowance',
      'wallet.receipt',
      'uniswap.quote',
      'aave.reserves',
      'aave.positions',
    ].includes(name)
      ? 'read'
      : 'write',
    schema: schemas[name],
    examples: [
      {
        ...([
          'wallet.balances',
          'wallet.token',
          'wallet.allowance',
          'wallet.receipt',
          'uniswap.quote',
          'aave.reserves',
          'aave.positions',
        ].includes(name)
          ? baseExample
          : writeExample),
        ...exampleExtra[name],
      },
    ],
    coverage:
      'Implemented; mocked regression checks. Mainnet writes and fork acceptance require separate verification.',
    completion:
      'Reads return observed chain state. Writes require canonical successful receipt, configured confirmations, matching operation events/effects, and relevant post-state.',
  }),
);

export class EthereumError extends FinError {
  constructor(code: string, message: string) {
    super(
      code,
      'ethereum',
      message,
      'Inspect the exact request and current chain state. For an existing submission, reconcile its original transaction identity without resubmitting.',
    );
    this.name = 'EthereumError';
  }
}
function requireCondition(ok: unknown, code: string, message: string): asserts ok {
  if (!ok) throw new EthereumError(code, message);
}
export function exactUnits(amount: string, decimals: number): bigint {
  requireCondition(
    /^(0|[1-9]\d*)(\.\d+)?$/.test(amount),
    'INVALID_AMOUNT',
    'Use an unsigned decimal string',
  );
  requireCondition(
    (amount.split('.')[1]?.length ?? 0) <= decimals,
    'EXCESS_PRECISION',
    `Amount has more than ${decimals} decimal places`,
  );
  return parseUnits(amount, decimals);
}
function safe(value: unknown): unknown {
  if (typeof value === 'bigint') return value.toString();
  if (Array.isArray(value)) return value.map(safe);
  if (value && typeof value === 'object')
    return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, safe(v)]));
  return value;
}
function quantity(units: bigint, decimals: number) {
  return { units: units.toString(), amount: formatUnits(units, decimals), decimals };
}
function asAddress(value: unknown): Address {
  return getAddress(String(value));
}
function asBigInt(value: unknown): bigint {
  return BigInt(String(value));
}
function rpcUnsigned(value: unknown, field: string, positive = false): bigint {
  requireCondition(
    typeof value === 'bigint' ||
      (typeof value === 'number' && Number.isSafeInteger(value)) ||
      (typeof value === 'string' && /^(0|[1-9]\d*)$/.test(value)),
    'RECEIPT_INVALID',
    `RPC ${field} must be an exact unsigned quantity`,
  );
  const quantity = BigInt(value);
  requireCondition(
    quantity >= (positive ? 1n : 0n) && quantity <= maxUint256,
    'RECEIPT_INVALID',
    `RPC ${field} is outside the permitted unsigned range`,
  );
  return quantity;
}
function equalHash(value: unknown, expected: unknown): boolean {
  return (
    typeof value === 'string' &&
    typeof expected === 'string' &&
    /^0x[0-9a-fA-F]{64}$/.test(value) &&
    /^0x[0-9a-fA-F]{64}$/.test(expected) &&
    value.toLowerCase() === expected.toLowerCase()
  );
}
function tuple(value: unknown): unknown[] {
  requireCondition(Array.isArray(value), 'RPC_DATA_INVALID', 'Unexpected contract return shape');
  return value;
}

/** No secret material is accepted by this module. Only an injected signing boundary can sign. */
export interface EthereumTransaction {
  account: Address;
  chainId: 1;
  to: Address;
  data: Hex;
  value: string;
  gas: string;
  maxFeePerGas: string;
  maxPriorityFeePerGas: string;
  nonce: number;
  type: 'eip1559';
}
export interface EthereumOptions {
  publicClient?: PublicClient | unknown;
  walletClient?: WalletClient | unknown;
  signTransaction?: (transaction: EthereumTransaction, wallet: string) => Promise<Hex>;
  now?: () => Date;
}
/** Narrow RPC port permits deterministic mocks while runtime defaults use viem. */
interface RpcPort {
  getChainId(): Promise<number>;
  getBytecode(args: { address: Address }): Promise<Hex | undefined>;
  readContract(args: Record<string, unknown>): Promise<unknown>;
  simulateContract(args: Record<string, unknown>): Promise<{ result: unknown }>;
  getBalance(args: { address: Address; blockNumber?: bigint }): Promise<bigint>;
  getBlockNumber(): Promise<bigint>;
  estimateGas(args: Record<string, unknown>): Promise<bigint>;
  estimateFeesPerGas(): Promise<{ maxFeePerGas?: bigint; maxPriorityFeePerGas?: bigint }>;
  call(args: Record<string, unknown>): Promise<{ data?: Hex }>;
  getTransactionCount(args: { address: Address; blockTag: 'pending' | 'latest' }): Promise<number>;
  sendRawTransaction(args: { serializedTransaction: Hex }): Promise<Hex>;
  getTransactionReceipt(args: { hash: Hex }): Promise<Record<string, unknown>>;
  getTransaction(args: { hash: Hex }): Promise<Record<string, unknown>>;
  getBlock(args: { blockNumber: bigint }): Promise<{ hash: Hex | null; number: bigint | null }>;
}
interface Token {
  address: Address | 'native';
  symbol: string;
  decimals: number;
}
interface Reserve {
  token: Token;
  aToken: Address;
  variableDebtToken: Address;
  active: boolean;
  frozen: boolean;
  paused: boolean;
  borrowing: boolean;
  collateral: boolean;
  ltvBps: string;
  liquidationThresholdBps: string;
  borrowCapUnits: string;
  supplyCapUnits: string;
  totalSupplyUnits: string;
  totalDebtUnits: string;
  liquidityUnits: string;
}
type PositionReserve = Pick<Reserve, 'token' | 'aToken' | 'variableDebtToken'>;

export class EthereumAdapter implements Adapter {
  readonly definitions = ethereumDefinitions;
  readonly client: RpcPort;
  private readonly now: () => Date;
  constructor(
    readonly config: FinConfig,
    private readonly options: EthereumOptions = {},
  ) {
    this.client = (options.publicClient ??
      createPublicClient({
        chain: mainnet,
        transport: fallback(
          config.ethereum.rpcUrls.map((url) => http(url, { timeout: 15000, retryCount: 1 })),
        ),
      })) as RpcPort;
    this.now = options.now ?? (() => new Date());
  }
  private schema(operation: string, input: Input): Input {
    requireCondition(
      operation in schemas,
      'UNSUPPORTED_OPERATION',
      `Unsupported Ethereum operation ${operation}`,
    );
    return schemas[operation as Operation].parse(input) as Input;
  }
  private wallet(input: Input): Address {
    const c = this.config.connections.find((c) => c.name === input.wallet && c.kind === 'ethereum');
    requireCondition(
      c?.address,
      'UNKNOWN_WALLET',
      'Configure a named Ethereum connection and its address',
    );
    return asAddress(c.address);
  }
  private async chain() {
    requireCondition(
      this.config.ethereum.chainId === 1 && (await this.client.getChainId()) === 1,
      'WRONG_CHAIN',
      'This adapter requires Ethereum mainnet chain ID 1',
    );
  }
  private async code(address: Address) {
    const code = await this.client.getBytecode({ address });
    requireCondition(code && code !== '0x', 'MISSING_CONTRACT', `No contract code at ${address}`);
  }
  private async contract(
    address: Address,
    abi: Abi,
    functionName: string,
    args: unknown[] = [],
    blockNumber?: bigint,
  ): Promise<unknown> {
    return this.client.readContract({
      address,
      abi,
      functionName,
      args,
      ...(blockNumber === undefined ? {} : { blockNumber }),
    });
  }
  private async tokenSymbol(address: Address): Promise<string> {
    let value: unknown;
    try {
      value = await this.contract(address, erc20Abi, 'symbol');
    } catch {
      try {
        const legacy = await this.contract(address, erc20Bytes32SymbolAbi, 'symbol');
        if (typeof legacy === 'string' && /^0x[0-9a-fA-F]{64}$/.test(legacy)) {
          value = hexToString(legacy as Hex, { size: 32 });
        }
      } catch {
        // A symbol is an optional display label, never the financial asset identity.
      }
    }
    if (typeof value !== 'string') return address;
    const label = value
      .replace(/[\p{Cc}\p{Cf}]/gu, '')
      .trim()
      .slice(0, 64);
    return label || address;
  }
  async token(token: string): Promise<Token> {
    if (token === 'native') return { address: 'native', symbol: 'ETH', decimals: 18 };
    const a = asAddress(token);
    await this.code(a);
    const [d, s] = await Promise.all([this.contract(a, erc20Abi, 'decimals'), this.tokenSymbol(a)]);
    const decimals = Number(d);
    requireCondition(
      (typeof d === 'number' || typeof d === 'bigint') &&
        Number.isInteger(decimals) &&
        decimals >= 0 &&
        decimals <= 255,
      'TOKEN_METADATA_INVALID',
      'Token decimals are invalid',
    );
    return { address: a, symbol: s, decimals };
  }
  async quoteUsd(
    token: string,
    units?: bigint,
  ): Promise<{
    priceUsdMicros: string;
    valueUsdMicros?: string;
    source: string;
    observedAt: string;
    feedUpdatedAt: string;
    maximumPriceAgeSeconds: number;
  }> {
    await this.chain();
    const t = await this.token(token);
    const oracle = asAddress(this.config.ethereum.aaveOracle);
    await this.code(oracle);
    const [currency, unit, price] = await Promise.all([
      this.contract(oracle, oracleAbi, 'BASE_CURRENCY'),
      this.contract(oracle, oracleAbi, 'BASE_CURRENCY_UNIT'),
      this.contract(oracle, oracleAbi, 'getAssetPrice', [
        t.address === 'native' ? WETH : t.address,
      ]),
    ]);
    requireCondition(
      String(currency).toLowerCase() === zeroAddress && asBigInt(unit) === 100_000_000n,
      'UNSUPPORTED_ORACLE_UNIT',
      'Ethereum Aave oracle must use USD base currency with 1e8 units',
    );
    const u = asBigInt(unit),
      p = asBigInt(price);
    requireCondition(p > 0n, 'PRICE_UNAVAILABLE', 'No positive USD oracle price for this asset');
    const asset = t.address === 'native' ? WETH : t.address;
    const source = asAddress(await this.contract(oracle, oracleAbi, 'getSourceOfAsset', [asset]));
    await this.code(source);
    let feed = source,
      round: unknown[];
    try {
      round = tuple(await this.contract(feed, priceFeedAbi, 'latestRoundData'));
    } catch {
      try {
        feed = asAddress(await this.contract(source, priceFeedAbi, 'ASSET_TO_USD_AGGREGATOR'));
        await this.code(feed);
        round = tuple(await this.contract(feed, priceFeedAbi, 'latestRoundData'));
      } catch {
        throw new EthereumError(
          'PRICE_FRESHNESS_UNAVAILABLE',
          'Oracle source has no supported Chainlink freshness evidence; configure a reviewed source before bounded live verification',
        );
      }
    }
    const updated = asBigInt(round[3]),
      now = BigInt(Math.floor(this.now().getTime() / 1000)),
      maximumPriceAgeSeconds = asset.toLowerCase() === WETH.toLowerCase() ? 7200 : 86400;
    requireCondition(
      asBigInt(round[1]) > 0n &&
        updated > 0n &&
        updated <= now &&
        now - updated <= BigInt(maximumPriceAgeSeconds) &&
        asBigInt(round[4]) >= asBigInt(round[0]),
      'STALE_PRICE',
      'Chainlink price is stale, invalid, or from an incomplete round',
    );
    const feedUpdatedAt = new Date(Number(updated) * 1000).toISOString();
    return {
      priceUsdMicros: ((p * 1_000_000n + u - 1n) / u).toString(),
      ...(units === undefined
        ? {}
        : {
            valueUsdMicros: (
              (units * p * 1_000_000n + u * 10n ** BigInt(t.decimals) - 1n) /
              (u * 10n ** BigInt(t.decimals))
            ).toString(),
          }),
      source: `Aave v3 USD oracle ${oracle}; source ${source}; Chainlink feed ${feed}, updated ${feedUpdatedAt}, maximum age ${maximumPriceAgeSeconds}s`,
      observedAt: this.now().toISOString(),
      feedUpdatedAt,
      maximumPriceAgeSeconds,
    };
  }
  private async balance(t: Token, wallet: Address, blockNumber?: bigint): Promise<bigint> {
    return t.address === 'native'
      ? this.client.getBalance({
          address: wallet,
          ...(blockNumber === undefined ? {} : { blockNumber }),
        })
      : asBigInt(await this.contract(t.address, erc20Abi, 'balanceOf', [wallet], blockNumber));
  }
  private async allowance(token: Address, wallet: Address, spender: Address) {
    return asBigInt(await this.contract(token, erc20Abi, 'allowance', [wallet, spender]));
  }
  private async provider() {
    const pool = asAddress(this.config.ethereum.aavePool);
    await this.code(pool);
    const addresses = asAddress(await this.contract(pool, poolAbi, 'ADDRESSES_PROVIDER'));
    await this.code(addresses);
    const [p, o, d] = await Promise.all([
      this.contract(addresses, addressesProviderAbi, 'getPool'),
      this.contract(addresses, addressesProviderAbi, 'getPriceOracle'),
      this.contract(addresses, addressesProviderAbi, 'getPoolDataProvider'),
    ]);
    requireCondition(
      String(p).toLowerCase() === pool.toLowerCase() &&
        String(o).toLowerCase() === this.config.ethereum.aaveOracle.toLowerCase(),
      'MARKET_IDENTITY_CHANGED',
      'Aave provider disagrees with configured Pool or Oracle',
    );
    const data = asAddress(d);
    await this.code(data);
    return data;
  }
  private async reserve(token: string, resolvedProvider?: Address): Promise<Reserve> {
    const t = await this.token(token);
    requireCondition(
      t.address !== 'native',
      'ERC20_REQUIRED',
      'Aave ERC-20 flow requires an explicit token contract; wrap ETH first',
    );
    const d = resolvedProvider ?? (await this.provider());
    const [cfg, tokens, caps, paused] = await Promise.all([
      this.contract(d, dataProviderAbi, 'getReserveConfigurationData', [t.address]),
      this.contract(d, dataProviderAbi, 'getReserveTokensAddresses', [t.address]),
      this.contract(d, dataProviderAbi, 'getReserveCaps', [t.address]),
      this.contract(d, dataProviderAbi, 'getPaused', [t.address]),
    ]);
    const c = tuple(cfg),
      a = tuple(tokens),
      cap = tuple(caps);
    const aToken = asAddress(a[0]),
      variableDebtToken = asAddress(a[2]);
    requireCondition(
      aToken !== zeroAddress && variableDebtToken !== zeroAddress,
      'UNAVAILABLE_RESERVE',
      'Token is not an Aave reserve',
    );
    await Promise.all([this.code(aToken), this.code(variableDebtToken)]);
    requireCondition(
      Number(c[0]) === t.decimals,
      'TOKEN_METADATA_CHANGED',
      'Aave reserve and ERC-20 decimals disagree',
    );
    const [s, b, l] = await Promise.all([
      this.contract(aToken, erc20Abi, 'totalSupply'),
      this.contract(variableDebtToken, erc20Abi, 'totalSupply'),
      this.contract(t.address, erc20Abi, 'balanceOf', [aToken]),
    ]);
    return {
      token: t,
      aToken,
      variableDebtToken,
      active: Boolean(c[8]),
      frozen: Boolean(c[9]),
      paused: Boolean(paused),
      borrowing: Boolean(c[6]),
      collateral: Boolean(c[5]),
      ltvBps: String(c[1]),
      liquidationThresholdBps: String(c[2]),
      borrowCapUnits: (asBigInt(cap[0]) * 10n ** BigInt(t.decimals)).toString(),
      supplyCapUnits: (asBigInt(cap[1]) * 10n ** BigInt(t.decimals)).toString(),
      totalSupplyUnits: String(s),
      totalDebtUnits: String(b),
      liquidityUnits: String(l),
    };
  }
  private async positionReserve(
    token: string,
    resolvedProvider: Address,
  ): Promise<PositionReserve> {
    const t = await this.token(token);
    requireCondition(
      t.address !== 'native',
      'ERC20_REQUIRED',
      'Aave reserve identity requires an ERC-20 contract',
    );
    const [configuration, addresses] = await Promise.all([
      this.contract(resolvedProvider, dataProviderAbi, 'getReserveConfigurationData', [t.address]),
      this.contract(resolvedProvider, dataProviderAbi, 'getReserveTokensAddresses', [t.address]),
    ]);
    requireCondition(
      Number(tuple(configuration)[0]) === t.decimals,
      'TOKEN_METADATA_CHANGED',
      'Aave reserve and ERC-20 decimals disagree',
    );
    const tokens = tuple(addresses);
    const aToken = asAddress(tokens[0]);
    const variableDebtToken = asAddress(tokens[2]);
    requireCondition(
      aToken !== zeroAddress && variableDebtToken !== zeroAddress,
      'UNAVAILABLE_RESERVE',
      'Token is not an Aave reserve',
    );
    await Promise.all([this.code(aToken), this.code(variableDebtToken)]);
    return { token: t, aToken, variableDebtToken };
  }
  private async positionsForTokens(tokens: string[], wallet: Address, resolvedProvider: Address) {
    const positions: Awaited<ReturnType<EthereumAdapter['position']>>[] = [];
    let next = 0;
    let failed = false;
    // Limit complete reserve tasks, including metadata and contract-code checks.
    // Preserve every discovered reserve and its input order without a stale cache.
    const workers = Array.from({ length: Math.min(4, tokens.length) }, async () => {
      while (!failed && next < tokens.length) {
        const index = next++;
        try {
          const reserve = await this.positionReserve(tokens[index]!, resolvedProvider);
          positions[index] = await this.position(reserve, wallet, undefined, resolvedProvider);
        } catch (error) {
          failed = true;
          throw error;
        }
      }
    });
    const outcomes = await Promise.allSettled(workers);
    const failure = outcomes.find(
      (outcome): outcome is PromiseRejectedResult => outcome.status === 'rejected',
    );
    if (failure) throw failure.reason;
    return positions;
  }
  private async position(
    reserve: PositionReserve,
    wallet: Address,
    blockNumber?: bigint,
    resolvedProvider?: Address,
  ) {
    const d = resolvedProvider ?? (await this.provider());
    const r = tuple(
      await this.contract(
        d,
        dataProviderAbi,
        'getUserReserveData',
        [reserve.token.address, wallet],
        blockNumber,
      ),
    );
    return {
      token: reserve.token,
      supplied: quantity(asBigInt(r[0]), reserve.token.decimals),
      stableDebt: quantity(asBigInt(r[1]), reserve.token.decimals),
      variableDebt: quantity(asBigInt(r[2]), reserve.token.decimals),
      scaledVariableDebtUnits: String(r[4]),
      collateralEnabled: Boolean(r[8]),
      aToken: reserve.aToken,
      variableDebtToken: reserve.variableDebtToken,
      interpretation:
        'Supplied underlying is the claim represented by aTokens; do not count both as independent holdings. Stable debt is observed for full exposure discovery; writes support variable debt only.',
    };
  }
  private async account(wallet: Address, blockNumber?: bigint) {
    const a = tuple(
      await this.contract(
        asAddress(this.config.ethereum.aavePool),
        poolAbi,
        'getUserAccountData',
        [wallet],
        blockNumber,
      ),
    );
    return {
      totalCollateralBase: String(a[0]),
      totalDebtBase: String(a[1]),
      availableBorrowsBase: String(a[2]),
      liquidationThresholdBps: String(a[3]),
      ltvBps: String(a[4]),
      healthFactorWad: String(a[5]),
      baseUnit: 'USD 1e8 (verified through configured USD oracle for writes)',
      healthFactorDecimals: 18,
    };
  }
  private async quote(input: Input) {
    const [tokenIn, tokenOut] = await Promise.all([
      this.token(String(input.tokenIn)),
      this.token(String(input.tokenOut)),
    ]);
    requireCondition(
      tokenIn.address !== 'native' &&
        tokenOut.address !== 'native' &&
        tokenIn.address !== tokenOut.address,
      'INVALID_ROUTE',
      'Choose two distinct ERC-20 tokens',
    );
    const amount = exactUnits(String(input.amount), tokenIn.decimals);
    requireCondition(amount > 0n, 'INVALID_AMOUNT', 'Input must be positive');
    const factory = asAddress(this.config.ethereum.uniswapFactory),
      quoter = asAddress(this.config.ethereum.uniswapQuoter);
    await Promise.all([this.code(factory), this.code(quoter)]);
    const tiers = input.feeTier === undefined ? [100, 500, 3000, 10000] : [Number(input.feeTier)];
    const outcomes = await Promise.allSettled(
      tiers.map(async (fee) => {
        const pool = asAddress(
          await this.contract(factory, factoryAbi, 'getPool', [
            tokenIn.address,
            tokenOut.address,
            fee,
          ]),
        );
        requireCondition(pool !== zeroAddress, 'NO_POOL', 'No pool at fee tier');
        await this.code(pool);
        const result = tuple(
          (
            await this.client.simulateContract({
              address: quoter,
              abi: quoterAbi,
              functionName: 'quoteExactInputSingle',
              args: [
                {
                  tokenIn: tokenIn.address,
                  tokenOut: tokenOut.address,
                  amountIn: amount,
                  fee,
                  sqrtPriceLimitX96: 0n,
                },
              ],
            })
          ).result,
        );
        return { fee, pool, amountOut: asBigInt(result[0]), quoteGas: String(result[3]) };
      }),
    );
    const successful = outcomes
      .filter(
        (
          o,
        ): o is PromiseFulfilledResult<{
          fee: number;
          pool: Address;
          amountOut: bigint;
          quoteGas: string;
        }> => o.status === 'fulfilled',
      )
      .map((o) => o.value)
      .filter((o) => o.amountOut > 0n)
      .sort((a, b) => (a.amountOut > b.amountOut ? -1 : 1));
    requireCondition(
      successful[0],
      'QUOTE_UNAVAILABLE',
      'No executable single-pool route for the selected fee tiers',
    );
    const selected = successful[0];
    return {
      protocol: 'Uniswap v3',
      version: 'exactInputSingle / original SwapRouter',
      tokenIn,
      tokenOut,
      input: quantity(amount, tokenIn.decimals),
      output: quantity(selected.amountOut, tokenOut.decimals),
      feeTier: selected.fee,
      pool: selected.pool,
      quoter,
      router: asAddress(this.config.ethereum.uniswapRouter),
      quoteGas: selected.quoteGas,
      unavailableFeeTiers: tiers.filter((f) => !successful.some((s) => s.fee === f)),
      observedAt: this.now().toISOString(),
    };
  }
  async read(operation: string, raw: Input): Promise<unknown> {
    const input = this.schema(operation, raw);
    await this.chain();
    const wallet = this.wallet(input);
    let data: unknown;
    if (operation === 'wallet.balances') {
      const tokens = await Promise.all(
        ['native', ...(input.tokens as string[])].map((t) => this.token(t)),
      );
      data = {
        holdings: await Promise.all(
          tokens.map(async (t) => ({
            ...t,
            ...quantity(await this.balance(t, wallet), t.decimals),
          })),
        ),
      };
    } else if (operation === 'wallet.token') data = await this.token(String(input.token));
    else if (operation === 'wallet.allowance') {
      const t = await this.token(String(input.token));
      data = {
        token: t,
        spender: asAddress(input.spender),
        ...quantity(
          await this.allowance(asAddress(t.address), wallet, asAddress(input.spender)),
          t.decimals,
        ),
      };
    } else if (operation === 'wallet.receipt')
      data = await this.receipt(String(input.transactionHash) as Hex);
    else if (operation === 'uniswap.quote') data = await this.quote(input);
    else if (operation === 'aave.reserves' || operation === 'aave.positions') {
      // Resolve and validate the market once within this read. Writes and receipt
      // reconciliation still resolve fresh; no cross-operation cache is retained.
      const resolvedProvider = await this.provider();
      const tokens = (input.tokens ??
        (await this.contract(
          asAddress(this.config.ethereum.aavePool),
          poolAbi,
          'getReservesList',
        ))) as string[];
      if (operation === 'aave.reserves') {
        data = {
          market: 'Aave v3 Ethereum',
          reserves: await Promise.all(tokens.map((token) => this.reserve(token, resolvedProvider))),
        };
      } else {
        const [positions, account] = await Promise.all([
          this.positionsForTokens(tokens, wallet, resolvedProvider),
          this.account(wallet),
        ]);
        data = { market: 'Aave v3 Ethereum', positions, account };
      }
    } else throw new EthereumError('NOT_A_READ', 'Use preview/execute for writes');
    return safe({
      operation,
      wallet: String(input.wallet),
      address: wallet,
      network: 'ethereum',
      chainId: 1,
      observedAt: this.now().toISOString(),
      data,
    });
  }
  async preview(operation: string, raw: Input): Promise<Preview> {
    return this.preparePreview(operation, raw);
  }
  private async preparePreview(
    operation: string,
    raw: Input,
    approvedCaps?: { gas: bigint; maxFeePerGas: bigint; maxPriorityFeePerGas: bigint },
  ): Promise<Preview> {
    const input = this.schema(operation, raw);
    requireCondition(
      this.definitions.find((d) => d.name === operation)?.kind === 'write',
      'NOT_A_WRITE',
      'This operation is read-only',
    );
    await this.chain();
    const wallet = this.wallet(input),
      pool = asAddress(this.config.ethereum.aavePool),
      router = asAddress(this.config.ethereum.uniswapRouter);
    let token: Token,
      amount = 0n,
      to: Address = wallet,
      data: Hex = '0x',
      value = 0n;
    const effect: Input = {};
    const warnings: string[] = [];
    const checks: Preview['checks'] = [
      {
        name: 'chain',
        status: 'passed',
        detail: 'RPC and configuration chain ID are 1 (Ethereum mainnet)',
      },
    ];
    if (operation === 'wallet.wrap' || operation === 'wallet.unwrap') {
      token = await this.token(WETH);
      amount = exactUnits(String(input.amount), 18);
      to = WETH;
      if (operation === 'wallet.wrap') {
        value = amount;
        data = encodeFunctionData({ abi: wethAbi, functionName: 'deposit' });
      } else {
        data = encodeFunctionData({ abi: wethAbi, functionName: 'withdraw', args: [amount] });
        requireCondition(
          (await this.balance(token, wallet)) >= amount,
          'INSUFFICIENT_BALANCE',
          'Insufficient WETH to unwrap',
        );
      }
    } else if (operation === 'uniswap.swap') {
      requireCondition(
        asAddress(input.recipient) !== zeroAddress,
        'ZERO_RECIPIENT',
        'Zero address recipient is not permitted',
      );
      const q = await this.quote(input);
      token = q.tokenIn;
      amount = asBigInt(q.input.units);
      const minimum = exactUnits(String(input.minOutput), q.tokenOut.decimals);
      const slippageMinimum =
        (asBigInt(q.output.units) * (10000n - BigInt(Number(input.slippageBps)))) / 10000n;
      requireCondition(
        minimum >= slippageMinimum && minimum <= asBigInt(q.output.units),
        'SLIPPAGE_BOUND',
        'minOutput must satisfy the selected quote and requested slippage',
      );
      const deadline = Number(input.deadline);
      requireCondition(
        deadline > Math.floor(this.now().getTime() / 1000) &&
          deadline <= Math.floor(this.now().getTime() / 1000) + 600,
        'INVALID_DEADLINE',
        'Deadline must be in the next ten minutes',
      );
      await this.code(router);
      to = router;
      data = encodeFunctionData({
        abi: routerAbi,
        functionName: 'exactInputSingle',
        args: [
          {
            tokenIn: asAddress(token.address),
            tokenOut: asAddress(q.tokenOut.address),
            fee: q.feeTier,
            recipient: asAddress(input.recipient),
            deadline: BigInt(deadline),
            amountIn: amount,
            amountOutMinimum: minimum,
            sqrtPriceLimitX96: 0n,
          },
        ],
      });
      requireCondition(
        (await this.allowance(asAddress(token.address), wallet, router)) >= amount,
        'ALLOWANCE_REQUIRED',
        'A separate exact token approval to the Uniswap router is required',
      );
      Object.assign(effect, {
        tokenOut: q.tokenOut.address,
        outputDecimals: q.tokenOut.decimals,
        minOutputUnits: minimum.toString(),
        quoteOutputUnits: q.output.units,
        feeTier: q.feeTier,
        pool: q.pool,
        recipient: asAddress(input.recipient),
        deadline,
        spender: router,
      });
    } else {
      token = await this.token(String(input.token));
      const amountText = String(input.amount ?? '0');
      amount = amountText === 'all' ? 0n : exactUnits(amountText, token.decimals);
      if (operation === 'wallet.transfer') {
        to = token.address === 'native' ? asAddress(input.recipient) : asAddress(token.address);
        requireCondition(
          asAddress(input.recipient) !== wallet,
          'SAME_RECIPIENT',
          'Choose a different recipient for transfers',
        );
        requireCondition(
          asAddress(input.recipient) !== zeroAddress,
          'ZERO_RECIPIENT',
          'Zero address recipient is not permitted',
        );
        if (token.address === 'native') {
          value = amount;
          const recipientCode = await this.client.getBytecode({ address: to });
          requireCondition(
            !recipientCode || recipientCode === '0x',
            'CONTRACT_RECIPIENT_UNSUPPORTED',
            'Native transfer reconciliation currently requires an externally owned recipient',
          );
        } else
          data = encodeFunctionData({
            abi: erc20Abi,
            functionName: 'transfer',
            args: [asAddress(input.recipient), amount],
          });
        Object.assign(effect, { recipient: asAddress(input.recipient) });
      } else if (operation === 'wallet.approve') {
        requireCondition(
          token.address !== 'native',
          'ERC20_REQUIRED',
          'Native ETH has no ERC-20 allowance',
        );
        requireCondition(
          amount < maxUint256,
          'UNLIMITED_APPROVAL',
          'Unlimited allowances are not supported',
        );
        const spender = asAddress(input.spender);
        requireCondition(spender !== zeroAddress, 'ZERO_SPENDER', 'Zero spender is not permitted');
        await this.code(spender);
        const baseline = await this.allowance(token.address, wallet, spender);
        to = token.address;
        data = encodeFunctionData({
          abi: erc20Abi,
          functionName: 'approve',
          args: [spender, amount],
        });
        Object.assign(effect, {
          spender,
          previousAllowanceUnits: baseline.toString(),
          authorityUnits: amount.toString(),
        });
        warnings.push(
          'ERC-20 allowance has no expiry; restore the intended baseline as a separate approved write.',
        );
      } else if (operation.startsWith('aave.')) {
        const reserve = await this.reserve(String(input.token));
        const position = await this.position(reserve, wallet);
        requireCondition(
          reserve.active && !reserve.paused,
          'UNAVAILABLE_RESERVE',
          'Reserve is inactive or paused',
        );
        to = pool;
        Object.assign(effect, {
          market: 'Aave v3 Ethereum',
          aToken: reserve.aToken,
          variableDebtToken: reserve.variableDebtToken,
          suppliedBeforeUnits: position.supplied.units,
          debtBeforeUnits: position.variableDebt.units,
          collateralBefore: position.collateralEnabled,
          interestRateMode: 2,
        });
        if (operation === 'aave.supply') {
          const [revision, scaledBefore] = await Promise.all([
            this.contract(asAddress(reserve.aToken), aTokenAbi, 'ATOKEN_REVISION'),
            this.contract(asAddress(reserve.aToken), aTokenAbi, 'scaledBalanceOf', [wallet]),
          ]);
          requireCondition(
            rpcUnsigned(revision, 'aToken revision') === 5n,
            'AAVE_TOKEN_VERSION_UNSUPPORTED',
            'Supply backing verification requires the reviewed Aave aToken revision 5',
          );
          Object.assign(effect, {
            aTokenRevision: '5',
            scaledSuppliedBeforeUnits: rpcUnsigned(
              scaledBefore,
              'scaled supplied balance',
            ).toString(),
          });
          requireCondition(!reserve.frozen, 'FROZEN_RESERVE', 'Reserve is frozen');
          const cap = asBigInt(reserve.supplyCapUnits);
          requireCondition(
            cap === 0n || asBigInt(reserve.totalSupplyUnits) + amount <= cap,
            'SUPPLY_CAP',
            'Requested supply exceeds reserve cap',
          );
          requireCondition(
            (await this.allowance(asAddress(token.address), wallet, pool)) >= amount,
            'ALLOWANCE_REQUIRED',
            'Separate exact Pool approval required',
          );
          data = encodeFunctionData({
            abi: poolAbi,
            functionName: 'supply',
            args: [asAddress(token.address), amount, wallet, 0],
          });
          Object.assign(effect, { spender: pool, recipient: wallet });
          warnings.push(
            'Aave can enable supplied reserve as collateral automatically; inspect collateral state after supply.',
          );
        } else if (operation === 'aave.borrow') {
          requireCondition(
            !reserve.frozen && reserve.borrowing,
            'BORROWING_DISABLED',
            'Reserve does not currently permit borrowing',
          );
          const cap = asBigInt(reserve.borrowCapUnits);
          requireCondition(
            cap === 0n || asBigInt(reserve.totalDebtUnits) + amount <= cap,
            'BORROW_CAP',
            'Requested borrow exceeds reserve cap',
          );
          requireCondition(
            asBigInt(reserve.liquidityUnits) >= amount,
            'INSUFFICIENT_LIQUIDITY',
            'Insufficient reserve underlying liquidity',
          );
          const account = await this.account(wallet);
          const price = await this.quoteUsd(String(token.address), amount);
          const addedBase = asBigInt(price.valueUsdMicros) * 100n;
          const debt = asBigInt(account.totalDebtBase) + addedBase;
          const health =
            debt === 0n
              ? maxUint256
              : (asBigInt(account.totalCollateralBase) *
                  asBigInt(account.liquidationThresholdBps) *
                  10n ** 18n) /
                (10000n * debt);
          const min = exactUnits(String(input.minHealthFactor), 18);
          requireCondition(
            min > 10n ** 18n && health >= min,
            'HEALTH_FACTOR_BOUND',
            'Projected health factor is below the explicit bound',
          );
          requireCondition(
            addedBase <= asBigInt(account.availableBorrowsBase),
            'INSUFFICIENT_COLLATERAL',
            'Requested debt exceeds currently available borrow amount',
          );
          data = encodeFunctionData({
            abi: poolAbi,
            functionName: 'borrow',
            args: [asAddress(token.address), amount, 2n, 0, wallet],
          });
          Object.assign(effect, {
            recipient: wallet,
            projectedHealthFactorWad: health.toString(),
            minimumHealthFactorWad: min.toString(),
            accountBefore: account,
          });
          warnings.push(
            'Variable interest and oracle prices can change health factor; the Pool validates its own constraints. Fin minimumHealthFactor is a current-state preflight condition, not an additional on-chain covenant.',
          );
        } else if (operation === 'aave.repay') {
          requireCondition(
            asBigInt(position.variableDebt.units) > 0n,
            'NO_DEBT',
            'There is no variable debt to repay',
          );
          let protocolAmount = amount;
          if (input.amount === 'all') {
            requireCondition(
              input.maxAmount,
              'MAX_AMOUNT_REQUIRED',
              'Repay all requires maxAmount',
            );
            amount = exactUnits(String(input.maxAmount), token.decimals);
            protocolAmount = maxUint256;
            requireCondition(
              asBigInt(position.variableDebt.units) <= amount,
              'DEBT_EXCEEDS_BOUND',
              'Current accrued debt exceeds maxAmount',
            );
            const authority = await this.allowance(asAddress(token.address), wallet, pool);
            requireCondition(
              authority >= asBigInt(position.variableDebt.units) && authority <= amount,
              'BOUNDED_ALLOWANCE_REQUIRED',
              'Repay all requires sufficient finite Pool allowance no greater than maxAmount',
            );
            Object.assign(effect, { repayAll: true, maxAmountUnits: amount.toString() });
          } else
            requireCondition(
              (await this.allowance(asAddress(token.address), wallet, pool)) >= amount,
              'ALLOWANCE_REQUIRED',
              'Separate Pool approval required',
            );
          data = encodeFunctionData({
            abi: poolAbi,
            functionName: 'repay',
            args: [asAddress(token.address), protocolAmount, 2n, wallet],
          });
          Object.assign(effect, { spender: pool, recipient: wallet });
          warnings.push(
            'Debt accrues until inclusion. A repay-all transaction reverts if accrued debt exceeds its bounded allowance or available token balance.',
          );
        } else if (operation === 'aave.withdraw') {
          requireCondition(
            amount <= asBigInt(position.supplied.units),
            'INSUFFICIENT_SUPPLY',
            'Requested withdrawal exceeds supplied underlying',
          );
          requireCondition(
            amount <= asBigInt(reserve.liquidityUnits),
            'INSUFFICIENT_LIQUIDITY',
            'Insufficient underlying to withdraw',
          );
          const recipient = asAddress(input.recipient);
          requireCondition(
            recipient !== zeroAddress,
            'ZERO_RECIPIENT',
            'Zero address recipient is not permitted',
          );
          data = encodeFunctionData({
            abi: poolAbi,
            functionName: 'withdraw',
            args: [asAddress(token.address), amount, recipient],
          });
          Object.assign(effect, { recipient });
          warnings.push(
            'Exact withdrawal may leave accrued supply dust; complete restoration requires a refreshed position check and any necessary separately approved cleanup.',
          );
        } else if (operation === 'aave.collateral') {
          requireCondition(
            asBigInt(position.supplied.units) > 0n,
            'NO_SUPPLY',
            'No supplied reserve to configure',
          );
          if (input.enabled)
            requireCondition(
              reserve.collateral && asBigInt(reserve.ltvBps) > 0n,
              'COLLATERAL_DISABLED',
              'Reserve is not eligible as borrowing collateral',
            );
          data = encodeFunctionData({
            abi: poolAbi,
            functionName: 'setUserUseReserveAsCollateral',
            args: [asAddress(token.address), Boolean(input.enabled)],
          });
          Object.assign(effect, { enabled: Boolean(input.enabled) });
          amount = 0n;
        }
      } else throw new EthereumError('UNSUPPORTED_OPERATION', 'Unsupported Ethereum write');
    }
    requireCondition(
      operation === 'wallet.approve' || operation === 'aave.collateral' || amount > 0n,
      'INVALID_AMOUNT',
      'Amount must be positive',
    );
    if (
      ![
        'wallet.approve',
        'aave.borrow',
        'aave.withdraw',
        'aave.collateral',
        'wallet.wrap',
      ].includes(operation)
    )
      requireCondition(
        (await this.balance(token, wallet)) >= amount,
        'INSUFFICIENT_BALANCE',
        'Insufficient wallet token balance',
      );
    const [estimated, fees, native, nonce] = await Promise.all([
      this.client.estimateGas({ account: wallet, to, data, value }),
      this.client.estimateFeesPerGas(),
      this.client.getBalance({ address: wallet }),
      this.client.getTransactionCount({ address: wallet, blockTag: 'pending' }),
    ]);
    requireCondition(
      Number.isSafeInteger(nonce) && nonce >= 0,
      'INVALID_NONCE',
      'RPC did not return a valid pending transaction nonce',
    );
    requireCondition(
      fees.maxFeePerGas !== undefined &&
        fees.maxPriorityFeePerGas !== undefined &&
        fees.maxFeePerGas >= 0n &&
        fees.maxPriorityFeePerGas >= 0n &&
        fees.maxPriorityFeePerGas <= fees.maxFeePerGas,
      'FEE_UNAVAILABLE',
      'RPC did not supply valid EIP-1559 fee data',
    );
    requireCondition(
      estimated > 0n,
      'GAS_UNAVAILABLE',
      'RPC did not supply a positive gas estimate',
    );
    const estimatedGasCap = (estimated * 120n + 99n) / 100n;
    if (approvedCaps)
      requireCondition(
        estimated <= approvedCaps.gas,
        'GAS_BOUND_CHANGED',
        'Current gas estimate exceeds approved gas cap',
      );
    // The preview applies gas headroom once. A fresh raw estimate may consume
    // that approved margin, while exact eth_call still uses the original cap.
    const gas = approvedCaps?.gas ?? estimatedGasCap;
    const minimumPriorityFeePerGas = 10_000_000n; // Disclosed 0.01 Gwei policy for NEW previews only.
    const maxPriorityFeePerGas =
      approvedCaps?.maxPriorityFeePerGas ??
      (fees.maxPriorityFeePerGas < minimumPriorityFeePerGas
        ? minimumPriorityFeePerGas
        : fees.maxPriorityFeePerGas);
    // Preserve the RPC's base-fee allowance when raising a new preview's tip.
    // Existing authorizations retain their exact priority fee, including zero.
    const requiredFeePerGas = approvedCaps
      ? fees.maxFeePerGas
      : fees.maxFeePerGas + maxPriorityFeePerGas - fees.maxPriorityFeePerGas;
    const callerMaximumFee = exactUnits(String(input.maxFeeEth), 18);
    requireCondition(
      gas * requiredFeePerGas <= callerMaximumFee,
      'FEE_LIMIT',
      'Estimated maximum gas cost exceeds maxFeeEth',
    );
    const headroomFee = (requiredFeePerGas * 3n + 1n) / 2n;
    const callerPerGasCap = callerMaximumFee / gas;
    const candidateFee = headroomFee < callerPerGasCap ? headroomFee : callerPerGasCap;
    if (approvedCaps) {
      requireCondition(
        fees.maxFeePerGas <= approvedCaps.maxFeePerGas,
        'FEE_BOUND_CHANGED',
        'Current fee conditions exceed the approved fee cap',
      );
    }
    // Headroom is inside the caller's explicit total fee limit. Refreshes use the
    // original approved caps for simulation, funding, display, and valuation.
    const maxFeePerGas = approvedCaps?.maxFeePerGas ?? candidateFee;
    const maximumFee = gas * maxFeePerGas;
    requireCondition(
      maxFeePerGas >= requiredFeePerGas &&
        maxPriorityFeePerGas >= 0n &&
        maxPriorityFeePerGas <= maxFeePerGas &&
        maximumFee <= callerMaximumFee,
      'FEE_LIMIT',
      'Transaction fee bounds exceed maxFeeEth or cannot cover current fee conditions',
    );
    requireCondition(
      native >= value + maximumFee,
      'INSUFFICIENT_GAS',
      'Native ETH balance cannot fund the movement and maximum gas fee',
    );
    const simulation = await this.client.call({
      account: wallet,
      to,
      data,
      value,
      gas,
      maxFeePerGas,
      maxPriorityFeePerGas,
    });
    if (
      operation === 'wallet.approve' ||
      (operation === 'wallet.transfer' && token.address !== 'native')
    )
      requireCondition(
        simulation.data &&
          decodeFunctionResult({
            abi: erc20Abi,
            functionName: operation === 'wallet.approve' ? 'approve' : 'transfer',
            data: simulation.data,
          }) === true,
        'TOKEN_OPERATION_FALSE',
        'ERC-20 simulation did not return true; unsupported or rejected token operation',
      );
    checks.push(
      {
        name: 'simulation',
        status: 'passed',
        detail:
          'Exact transaction eth_call and gas estimation succeeded; state may change before inclusion.',
      },
      {
        name: 'fees',
        status: 'passed',
        detail: `Gas ${gas} × maxFeePerGas ${maxFeePerGas} wei = ${formatUnits(maximumFee, 18)} ETH, within maxFeeEth ${input.maxFeeEth}; priority fee cap ${maxPriorityFeePerGas} wei. New previews use a minimum 0.01 Gwei tip; includes bounded fee headroom.`,
      },
    );
    const valuationToken = operation === 'wallet.wrap' ? 'native' : String(token.address);
    const valuationAmount =
      operation === 'aave.collateral' ? asBigInt(effect.suppliedBeforeUnits) : amount;
    const [movement, feeValue] = await Promise.all([
      this.quoteUsd(valuationToken, valuationAmount),
      this.quoteUsd('native', maximumFee),
    ]);
    const expiresAt = new Date(
      Math.min(
        this.now().getTime() + 120000,
        typeof effect.deadline === 'number' ? effect.deadline * 1000 : Infinity,
      ),
    ).toISOString();
    const binding: Input = {
      wallet: String(input.wallet),
      account: wallet,
      chainId: 1,
      nonce,
      to,
      data,
      value: value.toString(),
      gas: gas.toString(),
      maxFeePerGas: maxFeePerGas.toString(),
      maxPriorityFeePerGas: maxPriorityFeePerGas.toString(),
      token: token.address,
      decimals: token.decimals,
      amountUnits: amount.toString(),
      effect,
    };
    return {
      operation,
      request: input,
      binding,
      summary: `${descriptions[operation as Operation]} Wallet ${input.wallet}; ${operation === 'aave.collateral' ? 'affected supplied exposure' : 'amount'} ${formatUnits(valuationAmount, token.decimals)} ${token.symbol}; maximum gas ${formatUnits(maximumFee, 18)} ETH.`,
      checks,
      warnings,
      observedAt: this.now().toISOString(),
      expiresAt,
      valuation: {
        movementUsdMicros: String(movement.valueUsdMicros),
        maximumFeeUsdMicros: String(feeValue.valueUsdMicros),
        observedAt: movement.observedAt,
        source: movement.source,
        ...(operation === 'aave.borrow'
          ? { additionalAllocationUsdMicros: String(movement.valueUsdMicros) }
          : {}),
      },
    };
  }
  async revalidate(preview: Preview): Promise<Preview> {
    requireCondition(
      this.now().getTime() < Date.parse(preview.expiresAt),
      'PREVIEW_EXPIRED',
      'Refresh preview and obtain new approval',
    );
    requireCondition(
      preview.checks.every((c) => c.status === 'passed'),
      'PREVIEW_FAILED',
      'Failed or unchecked conditions prevent submission',
    );
    await this.chain();
    // Rebuild from the authorized request and require exact stable operation data. Fees and estimate may fluctuate; original bounds remain enforced.
    const current = await this.preparePreview(preview.operation, preview.request, {
      gas: asBigInt(preview.binding.gas),
      maxFeePerGas: asBigInt(preview.binding.maxFeePerGas),
      maxPriorityFeePerGas: asBigInt(preview.binding.maxPriorityFeePerGas),
    });
    for (const field of [
      'wallet',
      'account',
      'chainId',
      'to',
      'data',
      'value',
      'token',
      'decimals',
      'amountUnits',
    ])
      requireCondition(
        current.binding[field] === preview.binding[field],
        'APPROVED_REQUEST_CHANGED',
        `Approved ${field} changed; obtain a fresh approval`,
      );
    const b = preview.binding;
    if (preview.operation === 'aave.supply') {
      const before = (b.effect ?? {}) as Input;
      const refreshed = (current.binding.effect ?? {}) as Input;
      requireCondition(
        refreshed.aToken === before.aToken &&
          refreshed.aTokenRevision === before.aTokenRevision &&
          refreshed.scaledSuppliedBeforeUnits === before.scaledSuppliedBeforeUnits,
        'APPROVED_STATE_CHANGED',
        'Aave scaled supply identity or balance changed; obtain a fresh approval',
      );
    }
    requireCondition(
      current.binding.nonce === b.nonce,
      'NONCE_CHANGED',
      'Pending wallet nonce differs from the approved transaction; obtain a fresh approval',
    );
    requireCondition(
      asBigInt(current.binding.gas) <= asBigInt(b.gas),
      'GAS_BOUND_CHANGED',
      'Current gas estimate exceeds approved gas cap',
    );
    const fees = await this.client.estimateFeesPerGas();
    requireCondition(
      fees.maxFeePerGas !== undefined && fees.maxFeePerGas <= asBigInt(b.maxFeePerGas),
      'FEE_BOUND_CHANGED',
      'Current fee conditions exceed the approved fee cap',
    );
    return {
      ...current,
      binding: preview.binding,
      expiresAt: preview.expiresAt,
    };
  }
  async submit(preview: Preview, hooks: SubmissionHooks): Promise<Submission> {
    await this.revalidate(preview);
    const b = preview.binding;
    const account = asAddress(b.account);
    const nonce = Number(b.nonce);
    requireCondition(
      Number.isSafeInteger(nonce) && nonce >= 0,
      'INVALID_NONCE',
      'Approved transaction lacks a valid bound nonce',
    );
    const pendingNonce = await this.client.getTransactionCount({
      address: account,
      blockTag: 'pending',
    });
    requireCondition(
      pendingNonce === nonce,
      'NONCE_CHANGED',
      'Pending wallet nonce changed before signing; obtain a fresh approval',
    );
    const tx: EthereumTransaction = {
      account,
      chainId: 1,
      to: asAddress(b.to),
      data: String(b.data) as Hex,
      value: String(b.value),
      gas: String(b.gas),
      maxFeePerGas: String(b.maxFeePerGas),
      maxPriorityFeePerGas: String(b.maxPriorityFeePerGas),
      nonce,
      type: 'eip1559',
    };
    let serialized: Hex;
    if (this.options.signTransaction) {
      await hooks.assertAuthorized?.();
      serialized = await this.options.signTransaction(tx, String(b.wallet));
    } else if (this.options.walletClient) {
      const signer = this.options.walletClient as {
        account?: { address: Address };
        signTransaction: (args: Record<string, unknown>) => Promise<Hex>;
      };
      requireCondition(
        signer.account?.address.toLowerCase() === account.toLowerCase(),
        'SIGNER_MISMATCH',
        'Injected signer does not match the named wallet',
      );
      await hooks.assertAuthorized?.();
      serialized = await signer.signTransaction({
        ...tx,
        account: signer.account,
        chain: mainnet,
        value: asBigInt(tx.value),
        gas: asBigInt(tx.gas),
        maxFeePerGas: asBigInt(tx.maxFeePerGas),
        maxPriorityFeePerGas: asBigInt(tx.maxPriorityFeePerGas),
      });
    } else
      throw new EthereumError(
        'PROTECTED_SIGNER_REQUIRED',
        'Install the protected per-write approval/signing component; no environment-key signing bypass is provided',
      );
    requireCondition(
      serialized.startsWith('0x02'),
      'SIGNED_TRANSACTION_MISMATCH',
      'Expected an EIP-1559 signed transaction',
    );
    const signed = parseTransaction(serialized),
      from = await recoverTransactionAddress({
        serializedTransaction: serialized as `0x02${string}`,
      });
    requireCondition(
      from.toLowerCase() === account.toLowerCase() &&
        signed.type === 'eip1559' &&
        signed.chainId === 1 &&
        signed.nonce === nonce &&
        signed.to?.toLowerCase() === tx.to.toLowerCase() &&
        (signed.data ?? '0x').toLowerCase() === tx.data.toLowerCase() &&
        (signed.value ?? 0n) === asBigInt(tx.value) &&
        signed.gas === asBigInt(tx.gas) &&
        // viem omits canonical RLP zero quantities; only zero is equivalent.
        (signed.maxFeePerGas ?? 0n) === asBigInt(tx.maxFeePerGas) &&
        (signed.maxPriorityFeePerGas ?? 0n) === asBigInt(tx.maxPriorityFeePerGas) &&
        (signed.accessList?.length ?? 0) === 0,
      'SIGNED_TRANSACTION_MISMATCH',
      'Signing component returned a transaction outside the authorized request',
    );
    const hash = keccak256(serialized);
    const submission: Submission = {
      kind: 'ethereum',
      reference: hash,
      data: {
        wallet: String(b.wallet),
        account,
        nonce,
        chainId: 1,
        to: tx.to,
        value: tx.value,
        gas: tx.gas,
        maxFeePerGas: tx.maxFeePerGas,
        maxPriorityFeePerGas: tx.maxPriorityFeePerGas,
      },
    };
    hooks.persistSubmission(submission);
    const returned = await this.client.sendRawTransaction({ serializedTransaction: serialized });
    requireCondition(
      returned.toLowerCase() === hash.toLowerCase(),
      'RPC_HASH_MISMATCH',
      'RPC returned a different hash; reconcile the persisted signed-transaction hash',
    );
    return submission;
  }
  private async receipt(hash: Hex, binding?: Input) {
    requireCondition(
      equalHash(hash, hash),
      'RECEIPT_INVALID',
      'Original transaction hash is invalid',
    );
    const r = await this.client.getTransactionReceipt({ hash });
    requireCondition(
      equalHash(r.transactionHash, hash),
      'RECEIPT_HASH_MISMATCH',
      'RPC returned a receipt for another transaction',
    );
    requireCondition(
      r.status === 'success' || r.status === 'reverted',
      'RECEIPT_INVALID',
      'Receipt must report success or reverted',
    );
    requireCondition(Array.isArray(r.logs), 'RECEIPT_INVALID', 'Receipt logs must be an array');
    const block = rpcUnsigned(r.blockNumber, 'receipt block number');
    const transactionIndex = rpcUnsigned(r.transactionIndex, 'receipt transaction index');
    const gasUsed = rpcUnsigned(r.gasUsed, 'receipt gas used', true);
    const effectiveGasPrice = rpcUnsigned(r.effectiveGasPrice, 'receipt effective gas price', true);
    const [observedHead, canonical, transaction] = await Promise.all([
      this.client.getBlockNumber(),
      this.client.getBlock({ blockNumber: block }),
      this.client.getTransaction({ hash }),
    ]);
    const head = rpcUnsigned(observedHead, 'current block number');
    requireCondition(
      equalHash(r.blockHash, canonical.hash) &&
        rpcUnsigned(canonical.number, 'canonical block number') === block,
      'NON_CANONICAL_RECEIPT',
      'Receipt block is no longer canonical',
    );
    requireCondition(
      equalHash(transaction.hash, hash) &&
        equalHash(transaction.blockHash, r.blockHash) &&
        rpcUnsigned(transaction.blockNumber, 'transaction block number') === block &&
        rpcUnsigned(transaction.transactionIndex, 'transaction index') === transactionIndex &&
        typeof transaction.from === 'string' &&
        typeof transaction.to === 'string' &&
        typeof r.from === 'string' &&
        typeof r.to === 'string' &&
        transaction.from.toLowerCase() === r.from.toLowerCase() &&
        transaction.to.toLowerCase() === r.to.toLowerCase(),
      'TRANSACTION_MISMATCH',
      'Mined transaction and receipt identities do not match',
    );
    const transactionGas = rpcUnsigned(transaction.gas, 'transaction gas limit', true);
    requireCondition(
      gasUsed <= transactionGas,
      'RECEIPT_FEE_BOUND',
      'Receipt gas exceeds the mined transaction gas limit',
    );
    if (transaction.gasPrice !== undefined)
      requireCondition(
        effectiveGasPrice ===
          rpcUnsigned(transaction.gasPrice, 'mined transaction gas price', true),
        'RECEIPT_FEE_MISMATCH',
        'Receipt gas price differs from the mined transaction gas price',
      );
    if (binding) {
      requireCondition(
        transaction.type === 'eip1559' &&
          rpcUnsigned(transaction.chainId, 'transaction chain ID') === 1n &&
          transaction.from.toLowerCase() === String(binding.account).toLowerCase() &&
          transaction.to.toLowerCase() === String(binding.to).toLowerCase() &&
          String(transaction.input ?? transaction.data).toLowerCase() ===
            String(binding.data).toLowerCase() &&
          rpcUnsigned(transaction.value, 'transaction value') ===
            rpcUnsigned(binding.value, 'approved value') &&
          rpcUnsigned(transaction.nonce, 'transaction nonce') ===
            rpcUnsigned(binding.nonce, 'approved nonce') &&
          transactionGas === rpcUnsigned(binding.gas, 'approved gas limit', true) &&
          rpcUnsigned(transaction.maxFeePerGas, 'transaction maximum fee') ===
            rpcUnsigned(binding.maxFeePerGas, 'approved maximum fee') &&
          rpcUnsigned(transaction.maxPriorityFeePerGas, 'transaction priority fee') ===
            rpcUnsigned(binding.maxPriorityFeePerGas, 'approved priority fee') &&
          (transaction.accessList === undefined ||
            (Array.isArray(transaction.accessList) && transaction.accessList.length === 0)),
        'TRANSACTION_MISMATCH',
        'Mined transaction does not match the authorized request',
      );
      requireCondition(
        effectiveGasPrice <= rpcUnsigned(binding.maxFeePerGas, 'approved maximum fee') &&
          gasUsed * effectiveGasPrice <=
            rpcUnsigned(binding.gas, 'approved gas limit', true) *
              rpcUnsigned(binding.maxFeePerGas, 'approved maximum fee'),
        'RECEIPT_FEE_BOUND',
        'Receipt fee exceeds the authorized transaction bounds',
      );
    }
    const confirmations = head >= block ? head - block + 1n : 0n;
    return {
      receipt: {
        ...r,
        blockNumber: block,
        blockHash: r.blockHash,
        status: r.status,
        logs: r.logs,
        gasUsed,
        effectiveGasPrice,
      },
      transaction,
      confirmations: confirmations.toString(),
      requiredConfirmations: this.config.ethereum.confirmations,
      confirmed: confirmations >= BigInt(this.config.ethereum.confirmations),
    };
  }
  async reconcile(record: RecordOperation): Promise<Reconciliation> {
    if (!record.submission)
      return {
        status: 'unknown',
        evidence: {
          reason: 'No persisted transaction identity; do not submit a new financial action',
        },
      };
    await this.chain();
    const hash = record.submission.reference as Hex;
    let checked: Awaited<ReturnType<EthereumAdapter['receipt']>>;
    try {
      requireCondition(
        record.preview,
        'MISSING_PREVIEW',
        'Cannot verify a receipt without the original preview',
      );
      requireCondition(
        rpcUnsigned(record.submission.data.nonce, 'persisted nonce') ===
          rpcUnsigned(record.preview.binding.nonce, 'approved nonce'),
        'TRANSACTION_MISMATCH',
        'Persisted transaction nonce differs from the authorized request',
      );
      checked = await this.receipt(hash, record.preview.binding);
    } catch (error) {
      const name = (error as Error).name;
      const code = (error as { code?: string }).code;
      if (name === 'TransactionReceiptNotFoundError')
        return {
          status: 'pending',
          evidence: {
            transactionHash: hash,
            nonce: record.submission.data.nonce,
            reason:
              'No receipt yet. Original identity remains authoritative; do not submit a replacement.',
          },
        };
      return {
        status: 'unknown',
        evidence: {
          transactionHash: hash,
          reason:
            code === 'NON_CANONICAL_RECEIPT'
              ? 'Receipt was reorganized; continue reconciliation'
              : error instanceof EthereumError
                ? error.code
                : 'Receipt lookup unavailable; continue reconciliation',
        },
      };
    }
    const r = checked.receipt;
    const evidence: Input = {
      transactionHash: hash,
      blockNumber: String(r.blockNumber),
      blockHash: String(r.blockHash),
      confirmations: checked.confirmations,
      gasUsed: String(r.gasUsed),
      effectiveGasPrice: String(r.effectiveGasPrice),
      feeWei: (asBigInt(r.gasUsed) * asBigInt(r.effectiveGasPrice)).toString(),
    };
    if (!checked.confirmed)
      return { status: 'pending', evidence: { ...evidence, receiptStatus: r.status } };
    if (r.status !== 'success')
      return { status: 'failed', evidence: { ...evidence, receiptStatus: r.status } };
    const preview = record.preview;
    requireCondition(preview, 'MISSING_PREVIEW', 'Cannot verify effects without original preview');
    const b = preview.binding,
      e = (b.effect ?? {}) as Input;
    const account = asAddress(b.account),
      amount = asBigInt(b.amountUnits),
      token = String(b.token);
    let effects: Input = {};
    try {
      const tx = checked.transaction;
      const logs = (r.logs ?? []) as Array<{ address: Address; data: Hex; topics: readonly Hex[] }>;
      const decoded = (abi: Abi, emitter: string) =>
        logs
          .filter((l) => l.address.toLowerCase() === emitter.toLowerCase())
          .flatMap((l) => {
            try {
              return [
                decodeEventLog({
                  abi,
                  data: l.data,
                  topics: l.topics as [Hex, ...Hex[]],
                }) as unknown as { eventName: string; args: Record<string, unknown> },
              ];
            } catch {
              return [];
            }
          });
      const transfers = (t: string, from: string, to: string) =>
        decoded(erc20Abi, t)
          .filter(
            (l) =>
              l.eventName === 'Transfer' &&
              String(l.args.from).toLowerCase() === from.toLowerCase() &&
              String(l.args.to).toLowerCase() === to.toLowerCase(),
          )
          .reduce((s, l) => s + asBigInt(l.args.value), 0n);
      const event = (name: string) =>
        decoded(poolAbi, String(b.to)).find(
          (l) =>
            l.eventName === name &&
            String(l.args.reserve).toLowerCase() === token.toLowerCase() &&
            String(l.args.user ?? l.args.onBehalfOf).toLowerCase() === account.toLowerCase(),
        );
      if (record.operation === 'wallet.transfer') {
        const actual =
          token === 'native' ? asBigInt(tx.value) : transfers(token, account, String(e.recipient));
        requireCondition(
          actual === amount,
          'TRANSFER_EFFECT_MISMATCH',
          'Exact recipient transfer amount was not observed',
        );
        effects = { recipient: e.recipient, receivedUnits: actual.toString() };
      } else if (record.operation === 'wallet.wrap' || record.operation === 'wallet.unwrap') {
        const name = record.operation === 'wallet.wrap' ? 'Deposit' : 'Withdrawal';
        const l = decoded(wethAbi, WETH).find(
          (l) =>
            l.eventName === name &&
            String(l.args.dst ?? l.args.src).toLowerCase() === account.toLowerCase(),
        );
        requireCondition(
          l && asBigInt(l.args.wad) === amount,
          'WRAP_EFFECT_MISMATCH',
          'Exact WETH deposit/withdrawal effect was not observed',
        );
        effects = {
          amountUnits: amount.toString(),
          representation: record.operation === 'wallet.wrap' ? 'WETH' : 'native ETH',
        };
      } else if (record.operation === 'wallet.approve') {
        const authority = asBigInt(
          await this.contract(
            asAddress(token),
            erc20Abi,
            'allowance',
            [account, asAddress(e.spender)],
            asBigInt(r.blockNumber),
          ),
        );
        requireCondition(
          authority === amount,
          'ALLOWANCE_EFFECT_MISMATCH',
          'Allowance at inclusion block differs from authorized amount',
        );
        effects = { spender: e.spender, allowanceUnits: authority.toString() };
      } else if (record.operation === 'uniswap.swap') {
        const outgoing = decoded(erc20Abi, token)
          .filter(
            (l) =>
              l.eventName === 'Transfer' &&
              String(l.args.from).toLowerCase() === account.toLowerCase(),
          )
          .reduce((s, l) => s + asBigInt(l.args.value), 0n);
        const incoming = decoded(erc20Abi, String(e.tokenOut))
          .filter(
            (l) =>
              l.eventName === 'Transfer' &&
              String(l.args.to).toLowerCase() === String(e.recipient).toLowerCase(),
          )
          .reduce((s, l) => s + asBigInt(l.args.value), 0n);
        requireCondition(
          outgoing === amount && incoming >= asBigInt(e.minOutputUnits),
          'SWAP_EFFECT_MISMATCH',
          'Swap actual token input/output differs from approved bounds',
        );
        effects = {
          inputUnits: outgoing.toString(),
          outputUnits: incoming.toString(),
          pool: e.pool,
          feeTier: e.feeTier,
          recipient: e.recipient,
        };
      } else if (record.operation.startsWith('aave.')) {
        const reserve = await this.reserve(token),
          position = await this.position(reserve, account, asBigInt(r.blockNumber));
        const names: Record<string, string> = {
          'aave.supply': 'Supply',
          'aave.borrow': 'Borrow',
          'aave.repay': 'Repay',
          'aave.withdraw': 'Withdraw',
          'aave.collateral': e.enabled
            ? 'ReserveUsedAsCollateralEnabled'
            : 'ReserveUsedAsCollateralDisabled',
        };
        const l = event(names[record.operation]!);
        requireCondition(
          l,
          'AAVE_EFFECT_MISMATCH',
          'Expected Aave event for signer and reserve was not observed',
        );
        const actual = l.args.amount === undefined ? 0n : asBigInt(l.args.amount);
        if (record.operation === 'aave.repay') {
          requireCondition(
            actual > 0n && actual <= amount,
            'REPAY_EFFECT_MISMATCH',
            'Repayment differs from authorized maximum',
          );
          if (e.repayAll)
            requireCondition(
              asBigInt(position.variableDebt.units) === 0n,
              'DEBT_REMAINS',
              'Repay-all receipt did not clear variable debt',
            );
        } else if (record.operation === 'aave.collateral')
          requireCondition(
            position.collateralEnabled === e.enabled,
            'COLLATERAL_EFFECT_MISMATCH',
            'Collateral setting was not applied',
          );
        else
          requireCondition(
            actual === amount,
            'AAVE_AMOUNT_MISMATCH',
            'Actual Aave operation amount differs from the approved amount',
          );
        if (record.operation === 'aave.borrow')
          requireCondition(
            transfers(token, String(reserve.aToken), account) === amount &&
              asBigInt(position.variableDebt.units) >= asBigInt(e.debtBeforeUnits) + amount,
            'BORROW_EFFECT_MISMATCH',
            'Borrow must create debt and credit underlying to the signer',
          );
        if (record.operation === 'aave.withdraw')
          requireCondition(
            transfers(token, String(reserve.aToken), String(e.recipient)) === amount,
            'WITHDRAW_EFFECT_MISMATCH',
            'Withdraw must credit underlying to the approved recipient',
          );
        let supplyBacking: Input | undefined;
        if (record.operation === 'aave.supply') {
          requireCondition(
            String(reserve.aToken).toLowerCase() === String(e.aToken).toLowerCase() &&
              e.aTokenRevision === '5' &&
              transfers(token, account, String(reserve.aToken)) === actual,
            'AAVE_SUPPLY_BACKING_MISMATCH',
            'Supply must debit the exact underlying to the approved aToken',
          );
          const mints = decoded(aTokenAbi, String(reserve.aToken)).filter(
            (mint) =>
              mint.eventName === 'Mint' &&
              String(mint.args.caller).toLowerCase() === account.toLowerCase() &&
              String(mint.args.onBehalfOf).toLowerCase() === account.toLowerCase(),
          );
          requireCondition(
            mints.length === 1,
            'AAVE_SUPPLY_BACKING_MISMATCH',
            'Exactly one aToken mint for the supplier is required',
          );
          const mint = mints[0]!.args;
          const block = asBigInt(r.blockNumber);
          requireCondition(
            block > 0n,
            'AAVE_SUPPLY_BACKING_MISMATCH',
            'Supply backing needs an inclusion and preceding block',
          );
          const aToken = asAddress(reserve.aToken);
          const [
            revisionRaw,
            scaledBeforeRaw,
            scaledAfterRaw,
            previousIndexRaw,
            postIndexRaw,
            incomeRaw,
          ] = await Promise.all([
            this.contract(aToken, aTokenAbi, 'ATOKEN_REVISION', [], block),
            this.contract(aToken, aTokenAbi, 'scaledBalanceOf', [account], block - 1n),
            this.contract(aToken, aTokenAbi, 'scaledBalanceOf', [account], block),
            this.contract(aToken, aTokenAbi, 'getPreviousIndex', [account], block - 1n),
            this.contract(aToken, aTokenAbi, 'getPreviousIndex', [account], block),
            this.contract(
              asAddress(b.to),
              poolAbi,
              'getReserveNormalizedIncome',
              [asAddress(token)],
              block,
            ),
          ]);
          requireCondition(
            rpcUnsigned(revisionRaw, 'aToken inclusion revision') === 5n,
            'AAVE_TOKEN_VERSION_UNSUPPORTED',
            'Inclusion aToken revision differs from the reviewed revision 5',
          );
          const ray = 10n ** 27n;
          const scaledBefore = rpcUnsigned(scaledBeforeRaw, 'scaled balance before supply');
          const scaledAfter = rpcUnsigned(scaledAfterRaw, 'scaled balance after supply');
          const previousIndex = rpcUnsigned(previousIndexRaw, 'previous aToken index');
          const mintIndex = rpcUnsigned(mint.index, 'aToken mint index', true);
          const normalizedIncome = rpcUnsigned(incomeRaw, 'reserve normalized income', true);
          requireCondition(
            mintIndex >= ray &&
              mintIndex < 2n ** 128n &&
              normalizedIncome >= mintIndex &&
              previousIndex <= mintIndex &&
              (previousIndex >= ray || (previousIndex === 0n && scaledBefore === 0n)) &&
              scaledAfter < 2n ** 120n &&
              rpcUnsigned(postIndexRaw, 'post-supply aToken index') === mintIndex &&
              scaledBefore === rpcUnsigned(e.scaledSuppliedBeforeUnits, 'approved scaled balance'),
            'AAVE_SUPPLY_BACKING_MISMATCH',
            'Supply backing indices or scaled baseline do not match the authorized mint',
          );
          const multiplyFloor = (left: bigint, right: bigint, divisor: bigint) => {
            requireCondition(
              left * right <= maxUint256,
              'AAVE_SUPPLY_BACKING_MISMATCH',
              'Supply backing arithmetic exceeds the contract uint256 range',
            );
            return (left * right) / divisor;
          };
          // Exact revision-5 TokenMath and ScaledBalanceTokenBase semantics:
          // mint scaled=floor(underlying*RAY/index), backing=floor(scaled*index/RAY).
          // Verified aWETH implementation source (TokenMath.sol and token base):
          // https://etherscan.io/address/0xadc45df3cf1584624c97338bef33363bf5b97ada#code
          const scaledMinted = multiplyFloor(actual, ray, mintIndex);
          requireCondition(
            scaledMinted > 0n && scaledAfter === scaledBefore + scaledMinted,
            'AAVE_SUPPLY_BACKING_MISMATCH',
            'Observed scaled mint differs from revision-5 floor arithmetic',
          );
          const suppliedBeforeAtMint = multiplyFloor(scaledBefore, mintIndex, ray);
          const previousBalance = multiplyFloor(scaledBefore, previousIndex, ray);
          const totalAtMint = multiplyFloor(scaledAfter, mintIndex, ray);
          const suppliedAfter = multiplyFloor(scaledAfter, normalizedIncome, ray);
          const backedPrincipal = totalAtMint - suppliedBeforeAtMint;
          const mintBalanceIncrease = suppliedBeforeAtMint - previousBalance;
          const mintValue = totalAtMint - previousBalance;
          requireCondition(
            suppliedBeforeAtMint >=
              rpcUnsigned(e.suppliedBeforeUnits, 'approved supplied underlying') &&
              rpcUnsigned(mint.balanceIncrease, 'aToken mint balance increase') ===
                mintBalanceIncrease &&
              rpcUnsigned(mint.value, 'aToken mint value') === mintValue &&
              transfers(String(reserve.aToken), zeroAddress, account) === mintValue &&
              asBigInt(position.supplied.units) === suppliedAfter,
            'AAVE_SUPPLY_BACKING_MISMATCH',
            'Supply event, mint, and historical underlying backing do not agree',
          );
          supplyBacking = {
            method: 'aave-v3-revision-5-floor-scaled-mint',
            ray: ray.toString(),
            aToken: reserve.aToken,
            underlyingUnits: actual.toString(),
            mintIndexRay: mintIndex.toString(),
            normalizedIncomeRay: normalizedIncome.toString(),
            previousIndexRay: previousIndex.toString(),
            scaledBeforeUnits: scaledBefore.toString(),
            scaledMintedUnits: scaledMinted.toString(),
            scaledAfterUnits: scaledAfter.toString(),
            suppliedBeforeAtMintUnits: suppliedBeforeAtMint.toString(),
            backedPrincipalUnits: backedPrincipal.toString(),
            suppliedAfterUnits: suppliedAfter.toString(),
            roundingDeltaUnits: (backedPrincipal - actual).toString(),
            mintBalanceIncreaseUnits: mintBalanceIncrease.toString(),
            mintValueUnits: mintValue.toString(),
            inclusionIncomeUnits: (suppliedAfter - totalAtMint).toString(),
          };
        }
        effects = {
          actualAmountUnits: actual.toString(),
          position,
          ...(supplyBacking ? { supplyBacking } : {}),
          account: await this.account(account, asBigInt(r.blockNumber)),
        };
      }
    } catch (error) {
      return {
        status: 'unknown',
        evidence: {
          ...evidence,
          effectVerification: 'unresolved',
          reason:
            error instanceof EthereumError
              ? error.code
              : 'Effect data unavailable; continue reconciliation',
        },
      };
    }
    return {
      status: 'completed',
      evidence: safe({ ...evidence, receiptStatus: 'success', effects }) as Input,
    };
  }
}
