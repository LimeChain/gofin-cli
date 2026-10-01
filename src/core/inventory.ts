import { exactUnits, fingerprint, observedUnits } from './canonical.js';
import { FinError } from './errors.js';
import type { Input, Preview, RecordOperation } from './types.js';

const WETH = '0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2';
const USDC = '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48';
const USDT = '0xdac17f958d2ee523a2206206994597c13d831ec7';
const DAI = '0x6b175474e89094c44da98b954eedeac495271d0f';
const tokenDecimals: Record<string, number> = {
  native: 18,
  [WETH]: 18,
  [USDC]: 6,
  [USDT]: 6,
  [DAI]: 18,
};

export interface InventoryScope {
  walletAddresses: Record<string, string>;
  accounts: string[];
  /** Only preflight-verified deposit addresses belong here. */
  returnDeposits: Record<string, string>;
  /** Production verified return instructions per address/asset; omitted only by isolated legacy fixtures. */
  returnMinimums?: Record<string, Record<string, string>>;
  tokens: string[];
  spenders: string[];
  confirmations?: number;
}
interface FeeReservation {
  key: string;
  maximumUnits: string;
}
interface InventoryIntent {
  operation: string;
  source: string;
  token: string;
  amountUnits: string;
  destination?: string;
  destinationAccount?: string;
  recipient?: string;
  outputToken?: string;
  minimumOutputUnits?: string;
  spender?: string;
  suppliedBeforeUnits?: string;
  debtBeforeUnits?: string;
  repayAll?: boolean;
  authorityBeforeUnits?: string;
  enabled?: boolean;
}
export interface InventoryReservation {
  operation: string;
  bindingHash: string;
  source: string;
  asset: string;
  units: string;
  debits: Record<string, string>;
  fee?: FeeReservation;
  intent: InventoryIntent;
  reason?: string;
}
export interface InventoryConversion {
  originKey: string;
  originToken: string;
  currentKey: string;
  currentToken: string;
  units: string;
}
export interface InventoryFundedCost {
  operationId: string;
  originKey: string;
  originToken: string;
  currentKey: string;
  currentToken: string;
  units: string;
  cause: 'gas' | 'venue_fee' | 'borrow_interest';
}
export interface RunInventory {
  version: 1;
  scope: InventoryScope;
  /** Spendable test holdings and underlying supplied assets, never aTokens twice. */
  available: Record<string, string>;
  initial: Record<string, string>;
  debts: Record<string, string>;
  /** Confirmed borrow proceeds minus repayments, used to prove interest on complete repayment. */
  loanCash: Record<string, string>;
  allowances: Record<string, string>;
  /** Positions introduced from a verified zero baseline can accrue within this run. */
  introducedPositions: Record<string, boolean>;
  positionOrigins: Record<string, { originKey: string; originToken: string }>;
  inFlight: Record<string, InventoryReservation>;
  settled: string[];
  /** Exact unavoidable holding debits (gas, venue fees, test borrow interest). */
  costs: Record<string, string>;
  /** Signed swap deltas, supply rounding and interest; principal relocation is excluded. */
  economicEffects: Record<string, string>;
  /** Relocated or converted principal must return to its original asset/custody, net proved costs. */
  conversions: Record<string, InventoryConversion>;
  /** Append-only proof when tagged principal finances a verified unavoidable cost. */
  fundedCosts: InventoryFundedCost[];
  cleanupOutstanding: boolean;
}
const fail = (code: string, message: string): never => {
  throw new FinError(
    code,
    'inventory',
    message,
    'Reconcile the original operation and use only the selected test inventory and verified return route.',
  );
};
function object(value: unknown): Input {
  if (!value || typeof value !== 'object' || Array.isArray(value))
    return fail('INVENTORY_EVIDENCE', 'Structured operation evidence is missing.');
  return value as Input;
}
function uint(value: unknown): bigint {
  if (typeof value !== 'string' || !/^(0|[1-9]\d*)$/.test(value))
    return fail(
      'INVENTORY_UNITS',
      'Inventory quantities must be exact nonnegative integer base-unit strings.',
    );
  return BigInt(value);
}
function signed(value: unknown): bigint {
  if (typeof value !== 'string' || !/^-?(0|[1-9]\d*)$/.test(value))
    return fail('INVENTORY_UNITS', 'Invalid signed inventory effect.');
  return BigInt(value);
}
function normalizeToken(token: string): string {
  if (token === 'native') return token;
  if (!/^0x[0-9a-fA-F]{40}$/.test(token))
    return fail('INVENTORY_ASSET', 'Use native or an exact Ethereum mainnet token contract.');
  return token.toLowerCase();
}
export function holdingKey(location: string, token: string): string {
  return `${location}/holding/${normalizeToken(token)}`;
}
export function suppliedKey(wallet: string, token: string): string {
  return `${wallet}/supplied/${normalizeToken(token)}`;
}
export function debtKey(wallet: string, token: string): string {
  return `${wallet}/debt/${normalizeToken(token)}`;
}
export function allowanceKey(wallet: string, token: string, spender: string): string {
  return `${wallet}/allowance/${normalizeToken(token)}/${normalizeToken(spender)}`;
}
export function binanceInventoryAsset(coin: string): { token: string; decimals: number } {
  const mapping: Record<string, string> = { ETH: 'native', WETH, USDC, USDT, DAI };
  const token = mapping[coin];
  if (!token)
    return fail('INVENTORY_ASSET', 'No reviewed exchange-to-mainnet inventory mapping exists.');
  return { token, decimals: tokenDecimals[token]! };
}
function scopeToken(scope: InventoryScope, value: unknown): string {
  if (typeof value !== 'string')
    return fail('INVENTORY_SCOPE', 'Operation token identity is missing.');
  const token = normalizeToken(value);
  if (!scope.tokens.includes(token) || tokenDecimals[token] === undefined)
    return fail(
      'INVENTORY_SCOPE',
      'The operation asset is outside the snapshotted live-test token scope.',
    );
  return token;
}
function location(scope: InventoryScope, value: unknown, walletOnly = false): string {
  if (
    typeof value !== 'string' ||
    (!scope.walletAddresses[value] && (walletOnly || !scope.accounts.includes(value)))
  )
    return fail(
      'INVENTORY_SCOPE',
      'The selected source or destination is outside the snapshotted test locations.',
    );
  return value;
}
function recipient(
  scope: InventoryScope,
  value: unknown,
  token: string,
): { key: string; account?: string; address: string } {
  if (typeof value !== 'string' || !/^0x[0-9a-fA-F]{40}$/.test(value))
    return fail('INVENTORY_SCOPE', 'A tracked exact recipient address is required.');
  const address = value.toLowerCase();
  const wallet = Object.entries(scope.walletAddresses).find(([, a]) => a === address)?.[0];
  if (wallet) return { key: holdingKey(wallet, token), address };
  const account = scope.returnDeposits[address];
  if (account && scope.accounts.includes(account)) {
    if (scope.returnMinimums && scope.returnMinimums[address]?.[token] === undefined)
      return fail(
        'INVENTORY_ROUTE',
        'The deposit asset has no verified return instructions at this address.',
      );
    return { key: holdingKey(account, token), account, address };
  }
  return fail(
    'INVENTORY_SCOPE',
    'Recipient was not included in the baseline or verified exchange return instructions.',
  );
}
function keyParts(key: string): { location: string; kind: string; token: string } {
  const [location = '', kind = '', token = '', extra] = key.split('/');
  if (!location || !['holding', 'supplied'].includes(kind) || extra !== undefined)
    return fail(
      'INVENTORY_KEY',
      'Allocation keys must name a scoped holding or supplied-underlying bucket.',
    );
  return { location, kind, token: normalizeToken(token) };
}
export function createInventory(
  rawScope: InventoryScope,
  allocations: Record<string, string>,
): RunInventory {
  const scope: InventoryScope = {
    walletAddresses: Object.fromEntries(
      Object.entries(rawScope.walletAddresses).map(([n, a]) => {
        if (!/^[a-zA-Z0-9][a-zA-Z0-9_-]*$/.test(n) || !/^0x[0-9a-fA-F]{40}$/.test(a))
          return fail('INVENTORY_SCOPE', 'Invalid named wallet or address in test scope.');
        return [n, a.toLowerCase()];
      }),
    ),
    accounts: [...new Set(rawScope.accounts)],
    returnDeposits: {},
    tokens: [...new Set(rawScope.tokens.map(normalizeToken))],
    spenders: [...new Set(rawScope.spenders.map(normalizeToken))],
    confirmations: rawScope.confirmations ?? 1,
  };
  if (!Number.isSafeInteger(scope.confirmations) || scope.confirmations! < 1)
    return fail('INVENTORY_SCOPE', 'Inventory needs a positive confirmation requirement.');
  if (
    new Set(Object.values(scope.walletAddresses)).size !== Object.keys(scope.walletAddresses).length
  )
    return fail(
      'INVENTORY_SCOPE',
      'Wallet aliases for the same address cannot allocate the same funds twice.',
    );
  for (const account of scope.accounts)
    if (!/^[a-zA-Z0-9][a-zA-Z0-9_-]*$/.test(account) || scope.walletAddresses[account])
      return fail('INVENTORY_SCOPE', 'Invalid or colliding exchange account name.');
  for (const [address, account] of Object.entries(rawScope.returnDeposits)) {
    if (
      !/^0x[0-9a-fA-F]{40}$/.test(address) ||
      !scope.accounts.includes(account) ||
      Object.values(scope.walletAddresses).includes(address.toLowerCase())
    )
      return fail(
        'INVENTORY_SCOPE',
        'Deposit mapping must point to a scoped exchange account and unique address.',
      );
    scope.returnDeposits[address.toLowerCase()] = account;
  }
  if (rawScope.returnMinimums !== undefined) {
    scope.returnMinimums = {};
    for (const [address, minimums] of Object.entries(rawScope.returnMinimums)) {
      if (
        !/^0x[0-9a-fA-F]{40}$/.test(address) ||
        !scope.returnDeposits[address.toLowerCase()] ||
        scope.returnMinimums[address.toLowerCase()] !== undefined ||
        !minimums ||
        typeof minimums !== 'object' ||
        Array.isArray(minimums)
      )
        return fail(
          'INVENTORY_ROUTE',
          'Return minima must reference exact verified deposit addresses.',
        );
      const normalized: Record<string, string> = {};
      for (const [rawToken, value] of Object.entries(minimums)) {
        const token = scopeToken(scope, rawToken);
        if (normalized[token] !== undefined || uint(value) === 0n)
          return fail(
            'INVENTORY_ROUTE',
            'Return minima require unique scoped assets and positive exact base units.',
          );
        normalized[token] = uint(value).toString();
      }
      if (!Object.keys(normalized).length)
        return fail(
          'INVENTORY_ROUTE',
          'A verified return address needs at least one asset minimum.',
        );
      scope.returnMinimums[address.toLowerCase()] = normalized;
    }
    if (
      Object.keys(scope.returnDeposits).some(
        (address) => scope.returnMinimums![address] === undefined,
      )
    )
      return fail(
        'INVENTORY_ROUTE',
        'Every exchange return address requires verified asset-specific minima.',
      );
  }
  const available: Record<string, string> = {};
  for (const [key, value] of Object.entries(allocations)) {
    const parts = keyParts(key);
    location(scope, parts.location, parts.kind === 'supplied');
    scopeToken(scope, parts.token);
    const normalized =
      parts.kind === 'holding'
        ? holdingKey(parts.location, parts.token)
        : suppliedKey(parts.location, parts.token);
    if (available[normalized] !== undefined)
      return fail('INVENTORY_KEY', 'Duplicate canonical allocation bucket.');
    available[normalized] = uint(value).toString();
  }
  if (!Object.values(available).some((v) => uint(v) > 0n))
    return fail(
      'INVENTORY_EMPTY',
      'Select exact test holdings, including gas and complete return costs.',
    );
  return {
    version: 1,
    scope,
    available,
    initial: { ...available },
    debts: {},
    loanCash: {},
    allowances: {},
    introducedPositions: {},
    positionOrigins: {},
    inFlight: {},
    settled: [],
    costs: {},
    economicEffects: {},
    conversions: {},
    fundedCosts: [],
    cleanupOutstanding: false,
  };
}
function add(map: Record<string, string>, key: string, value: bigint): void {
  const current = uint(map[key] ?? '0');
  if (current + value < 0n)
    fail(
      'INVENTORY_INSUFFICIENT',
      'Requested movement exceeds the selected test units at its source.',
    );
  map[key] = (current + value).toString();
}
function economic(map: Record<string, string>, key: string, value: bigint): void {
  map[key] = (signed(map[key] ?? '0') + value).toString();
}
function convertedCost(
  next: RunInventory,
  key: string,
  quantity: bigint,
  cause: InventoryFundedCost['cause'],
  operationId: string,
): void {
  // Called after actual debit/refund accounting. Reserved maximum gas must not hide available own cash.
  const cashBeforeCost = uint(next.available[key] ?? '0') + quantity;
  const tagged = Object.values(next.conversions).reduce(
    (sum, conversion) => (conversion.currentKey === key ? sum + uint(conversion.units) : sum),
    0n,
  );
  if (tagged > cashBeforeCost)
    return fail(
      'INVENTORY_CONVERSION',
      'Tracked moved principal exceeds the proved cash available before this cost.',
    );
  const ownCash = cashBeforeCost - tagged;
  let left = quantity > ownCash ? quantity - ownCash : 0n;
  for (const [id, conversion] of Object.entries(next.conversions)) {
    if (conversion.currentKey !== key || left === 0n) continue;
    const available = uint(conversion.units);
    const used = available < left ? available : left;
    left -= used;
    next.fundedCosts.push({
      operationId,
      originKey: conversion.originKey,
      originToken: conversion.originToken,
      currentKey: key,
      currentToken: conversion.currentToken,
      units: used.toString(),
      cause,
    });
    if (used === available) delete next.conversions[id];
    else conversion.units = (available - used).toString();
  }
}
function moveConversions(
  next: RunInventory,
  sourceKey: string,
  destinationKey: string,
  outputToken: string,
  input: bigint,
  output: bigint,
  operationId: string,
  newMovement = false,
): void {
  if (input <= 0n) return;
  // A transfer back to the same holding changes only its execution cost, including existing provenance.
  if (sourceKey === destinationKey && outputToken === sourceKey.split('/')[2] && input === output)
    return;
  const portions: { id: string; conversion: InventoryConversion; used: bigint }[] = [];
  let left = input;
  for (const [id, conversion] of Object.entries(next.conversions)) {
    if (conversion.currentKey !== sourceKey || left === 0n) continue;
    const used = uint(conversion.units) < left ? uint(conversion.units) : left;
    left -= used;
    portions.push({ id, conversion: { ...conversion }, used });
  }
  let credited = 0n;
  let index = 0;
  for (const portion of portions) {
    const last = ++index === portions.length && left === 0n;
    const share = last ? output - credited : (output * portion.used) / input;
    credited += share;
    if (share <= 0n)
      return fail(
        'INVENTORY_CONVERSION',
        'Converted principal output is too small to attribute exactly.',
      );
    const remaining = uint(portion.conversion.units) - portion.used;
    if (remaining === 0n) delete next.conversions[portion.id];
    else next.conversions[portion.id]!.units = remaining.toString();
    if (
      outputToken === portion.conversion.originToken &&
      destinationKey === portion.conversion.originKey
    )
      continue;
    const id = remaining === 0n ? portion.id : `${operationId}:${index}`;
    next.conversions[id] = {
      ...portion.conversion,
      currentKey: destinationKey,
      currentToken: outputToken,
      units: share.toString(),
    };
  }
  if (newMovement && left > 0n && sourceKey !== destinationKey) {
    const share = output - credited;
    if (share <= 0n)
      return fail('INVENTORY_CONVERSION', 'New converted principal output is unavailable.');
    next.conversions[`${operationId}:new`] = {
      originKey: sourceKey,
      originToken: sourceKey.split('/')[2]!,
      currentKey: destinationKey,
      currentToken: outputToken,
      units: share.toString(),
    };
  }
}
function extractIntent(
  state: RunInventory,
  preview: Preview,
): { intent: InventoryIntent; debits: Record<string, string>; fee?: FeeReservation } {
  const b = preview.binding;
  const debits: Record<string, string> = {};
  const scope = state.scope;
  if (preview.operation.startsWith('binance.')) {
    const params = object(b.params);
    const source = location(scope, b.from ?? b.account);
    if (!scope.accounts.includes(source))
      return fail('INVENTORY_SCOPE', 'Binance source must be a scoped exchange account.');
    const mapping = binanceInventoryAsset(String(params.asset ?? params.coin));
    const token = scopeToken(scope, mapping.token);
    const quantity = exactUnits(String(params.amount), mapping.decimals);
    const intent: InventoryIntent = {
      operation: preview.operation,
      source,
      token,
      amountUnits: quantity.toString(),
    };
    add(debits, holdingKey(source, token), quantity);
    if (preview.operation === 'binance.transfer') {
      location(scope, b.authority);
      const dest = location(scope, b.to);
      if (
        !scope.accounts.includes(dest) ||
        params.fromAccountType !== 'SPOT' ||
        params.toAccountType !== 'SPOT'
      )
        return fail('INVENTORY_SCOPE', 'Only scoped SPOT account allocation is supported.');
      intent.destination = holdingKey(dest, token);
      return { intent, debits };
    }
    if (preview.operation === 'binance.withdraw') {
      const dest = recipient(scope, params.address, token);
      if (dest.account)
        return fail(
          'INVENTORY_SCOPE',
          'Withdrawal inventory completion requires a tracked wallet recipient.',
        );
      const destination = location(scope, b.destination, true);
      if (
        dest.key !== holdingKey(destination, token) ||
        b.chainId !== 1 ||
        params.network !== 'ETH' ||
        params.walletType !== 0
      )
        return fail(
          'INVENTORY_SCOPE',
          'Withdrawal recipient or network differs from the tracked mainnet wallet.',
        );
      intent.destination = dest.key;
      intent.recipient = dest.address;
      const maximum = exactUnits(String(b.maximumFee), mapping.decimals);
      const fee = { key: holdingKey(source, token), maximumUnits: maximum.toString() };
      add(debits, fee.key, maximum);
      return { intent, debits, fee };
    }
    return fail(
      'INVENTORY_UNSUPPORTED',
      'This exchange write has no inventory effect implementation.',
    );
  }
  const source = location(scope, b.wallet, true);
  const token = scopeToken(scope, b.token);
  if (b.chainId !== 1 || String(b.account).toLowerCase() !== scope.walletAddresses[source])
    return fail('INVENTORY_SCOPE', 'Ethereum source identity differs from the tracked wallet.');
  if (b.decimals !== tokenDecimals[token])
    return fail('INVENTORY_ASSET', 'Operation decimals differ from the reviewed asset identity.');
  const quantity = uint(b.amountUnits);
  const e = object(b.effect);
  const intent: InventoryIntent = {
    operation: preview.operation,
    source,
    token,
    amountUnits: quantity.toString(),
  };
  const sourceHolding = holdingKey(source, token);
  if (
    ['wallet.transfer', 'uniswap.swap', 'aave.supply', 'aave.repay', 'wallet.unwrap'].includes(
      preview.operation,
    )
  )
    add(debits, sourceHolding, quantity);
  if (
    preview.operation === 'wallet.transfer' ||
    preview.operation === 'uniswap.swap' ||
    preview.operation === 'aave.withdraw'
  ) {
    const outToken = preview.operation === 'uniswap.swap' ? scopeToken(scope, e.tokenOut) : token;
    const dest = recipient(scope, e.recipient, outToken);
    if (dest.account && preview.operation !== 'wallet.transfer')
      return fail(
        'INVENTORY_SCOPE',
        'Protocol outputs to exchange addresses lack supported credit reconciliation.',
      );
    if (
      dest.account &&
      scope.returnMinimums &&
      quantity < uint(scope.returnMinimums[dest.address]?.[outToken])
    )
      return fail(
        'INVENTORY_ROUTE',
        'Return quantity is below the verified usable exchange-credit minimum.',
      );
    Object.assign(intent, {
      destination: dest.key,
      destinationAccount: dest.account,
      recipient: dest.address,
    });
    if (preview.operation === 'uniswap.swap') {
      intent.outputToken = outToken;
      intent.minimumOutputUnits = uint(e.minOutputUnits).toString();
      if (e.outputDecimals !== tokenDecimals[outToken])
        return fail('INVENTORY_ASSET', 'Output token decimals differ from reviewed identity.');
    }
  }
  if (preview.operation === 'wallet.wrap' || preview.operation === 'wallet.unwrap') {
    if (token !== WETH)
      return fail('INVENTORY_ASSET', 'Wrap/unwrap requires the reviewed WETH contract.');
    if (preview.operation === 'wallet.wrap') add(debits, holdingKey(source, 'native'), quantity);
    intent.destination = holdingKey(source, preview.operation === 'wallet.wrap' ? WETH : 'native');
    scopeToken(scope, 'native');
  }
  if (
    preview.operation === 'wallet.approve' ||
    preview.operation === 'uniswap.swap' ||
    preview.operation === 'aave.supply' ||
    preview.operation === 'aave.repay'
  ) {
    if (typeof e.spender !== 'string' || !scope.spenders.includes(e.spender.toLowerCase()))
      return fail(
        'INVENTORY_SCOPE',
        'Allowance spender is outside the snapshotted protocol scope.',
      );
    intent.spender = e.spender.toLowerCase();
  }
  if (preview.operation === 'wallet.approve') {
    if (token === 'native' || quantity > uint(state.available[sourceHolding] ?? '0'))
      return fail(
        'INVENTORY_AUTHORITY',
        'Token authority cannot exceed the test holdings currently allocated at this wallet.',
      );
    intent.authorityBeforeUnits = uint(e.previousAllowanceUnits).toString();
  }
  if (preview.operation.startsWith('aave.')) {
    intent.suppliedBeforeUnits = uint(e.suppliedBeforeUnits).toString();
    intent.debtBeforeUnits = uint(e.debtBeforeUnits).toString();
    const supplied = suppliedKey(source, token);
    const debt = debtKey(source, token);
    if (preview.operation === 'aave.withdraw') {
      // A fresh read can attribute accrued supply only when this run introduced the whole reserve position.
      const tracked = uint(state.available[supplied] ?? '0');
      const observed = uint(e.suppliedBeforeUnits);
      if (state.introducedPositions[supplied]) {
        if (observed < tracked)
          return fail(
            'INVENTORY_POSITION_CHANGED',
            'Supplied test position unexpectedly decreased; reconcile before withdrawing.',
          );
        const origin = state.positionOrigins[supplied];
        if (origin && observed > tracked)
          state.conversions[`interest:${supplied}:${observed}`] = {
            ...origin,
            currentKey: supplied,
            currentToken: token,
            units: (observed - tracked).toString(),
          };
        state.available[supplied] = observed.toString();
      }
      add(debits, supplied, quantity);
    } else if (preview.operation === 'aave.borrow') {
      if (
        !Object.entries(state.available).some(
          ([key, value]) => key.startsWith(`${source}/supplied/`) && uint(value) > 0n,
        )
      )
        return fail(
          'INVENTORY_COLLATERAL',
          'Borrowing against unrelated, unallocated collateral is not allowed in this test run.',
        );
      if (
        uint(e.debtBeforeUnits) !== uint(state.debts[debt] ?? '0') &&
        uint(state.debts[debt] ?? '0') === 0n
      )
        return fail(
          'INVENTORY_DEBT',
          'Existing unrelated debt cannot be incorporated into this test borrow.',
        );
    } else if (preview.operation === 'aave.repay') {
      if (uint(state.debts[debt] ?? '0') === 0n)
        return fail('INVENTORY_DEBT', 'Repayment must name debt introduced by this test run.');
      intent.repayAll = e.repayAll === true;
    } else if (preview.operation === 'aave.collateral') {
      if (
        uint(state.available[supplied] ?? '0') === 0n ||
        (uint(e.suppliedBeforeUnits) > uint(state.available[supplied] ?? '0') &&
          !state.introducedPositions[supplied])
      )
        return fail(
          'INVENTORY_COLLATERAL',
          'Collateral changes must affect only the scoped test position.',
        );
      intent.enabled = e.enabled === true;
    } else if (preview.operation !== 'aave.supply')
      return fail('INVENTORY_UNSUPPORTED', 'Aave inventory intent is not implemented.');
  }
  const supported = [
    'wallet.transfer',
    'wallet.wrap',
    'wallet.unwrap',
    'wallet.approve',
    'uniswap.swap',
    'aave.supply',
    'aave.borrow',
    'aave.repay',
    'aave.withdraw',
    'aave.collateral',
  ];
  if (!supported.includes(preview.operation))
    return fail('INVENTORY_UNSUPPORTED', 'This write has no inventory effect implementation.');
  const maximum = uint(b.gas) * uint(b.maxFeePerGas);
  const fee: FeeReservation = {
    key: holdingKey(source, scopeToken(scope, 'native')),
    maximumUnits: maximum.toString(),
  };
  add(debits, fee.key, maximum);
  return { intent, debits, fee };
}
function returningConversionUnits(state: RunInventory, intent: InventoryIntent): bigint {
  const inputToken = intent.operation === 'wallet.wrap' ? 'native' : intent.token;
  const outputToken =
    intent.operation === 'uniswap.swap'
      ? intent.outputToken
      : intent.operation === 'wallet.wrap'
        ? WETH
        : intent.operation === 'wallet.unwrap'
          ? 'native'
          : intent.token;
  return Object.values(state.conversions).reduce(
    (sum, conversion) =>
      conversion.currentKey === holdingKey(intent.source, inputToken) &&
      conversion.originKey === intent.destination &&
      conversion.originToken === outputToken
        ? sum + uint(conversion.units)
        : sum,
    0n,
  );
}
function increasesRisk(state: RunInventory, intent: InventoryIntent): boolean {
  if (['aave.borrow', 'aave.supply'].includes(intent.operation)) return true;
  if (intent.operation === 'wallet.wrap')
    return uint(intent.amountUnits) > returningConversionUnits(state, intent);
  if (intent.operation === 'aave.collateral') return intent.enabled === true;
  if (intent.operation === 'wallet.approve') {
    const converted = Object.values(state.conversions).reduce(
      (sum, c) =>
        c.currentKey === holdingKey(intent.source, intent.token) ? sum + uint(c.units) : sum,
      0n,
    );
    return (
      uint(intent.amountUnits) > converted &&
      uint(state.debts[debtKey(intent.source, intent.token)] ?? '0') === 0n &&
      uint(state.initial[holdingKey(intent.source, intent.token)] ?? '0') > 0n
    );
  }
  if (intent.operation === 'uniswap.swap')
    return uint(intent.amountUnits) > returningConversionUnits(state, intent);
  if (['wallet.transfer', 'binance.transfer', 'binance.withdraw'].includes(intent.operation))
    return (
      uint(intent.amountUnits) > returningConversionUnits(state, intent) &&
      (!intent.destination || uint(state.initial[intent.destination] ?? '0') === 0n)
    );
  return false;
}
/** Copy-on-success: callers persist this whole state with the operation reservation before submission. */
export function reserveInventory(
  inventory: RunInventory,
  operationId: string,
  preview: Preview,
): RunInventory {
  if (inventory.settled.includes(operationId))
    return fail(
      'INVENTORY_SETTLED',
      'This operation has already settled; reconcile its existing record.',
    );
  const existing = inventory.inFlight[operationId];
  const hash = fingerprint({ operation: preview.operation, binding: preview.binding });
  if (existing) {
    if (existing.bindingHash !== hash)
      return fail(
        'INVENTORY_IDENTITY',
        'A pending inventory reservation cannot change its bound operation.',
      );
    return structuredClone(inventory);
  }
  if (Object.keys(inventory.inFlight).length)
    return fail(
      'INVENTORY_PENDING',
      'Pending or uncertain test funds cannot finance another movement. Reconcile the original operation first.',
    );
  const next = structuredClone(inventory);
  const plan = extractIntent(next, preview);
  if (next.cleanupOutstanding && increasesRisk(next, plan.intent))
    return fail('INVENTORY_CLEANUP', 'New exposure is blocked while restoration is outstanding.');
  for (const [key, value] of Object.entries(plan.debits)) add(next.available, key, -uint(value));
  next.inFlight[operationId] = {
    operation: preview.operation,
    bindingHash: hash,
    source: plan.intent.source,
    asset: plan.intent.token,
    units: plan.intent.amountUnits,
    debits: plan.debits,
    ...(plan.fee ? { fee: plan.fee } : {}),
    intent: plan.intent,
  };
  return next;
}
function confirmations(evidence: Input, state: RunInventory): void {
  if (
    uint(evidence.confirmations) < BigInt(state.scope.confirmations ?? 1) ||
    typeof evidence.blockHash !== 'string' ||
    typeof evidence.transactionHash !== 'string'
  )
    return fail(
      'INVENTORY_EVIDENCE',
      'Canonical confirmed chain evidence is required before settling test funds.',
    );
}
function refundAndConsume(
  next: RunInventory,
  reservation: InventoryReservation,
  actual: Record<string, string>,
  fee: bigint,
  operationId: string,
): void {
  const totals = { ...actual };
  if (reservation.fee) {
    if (fee > uint(reservation.fee.maximumUnits))
      return fail('INVENTORY_FEE', 'Actual execution fee exceeds the inventory reservation.');
    add(totals, reservation.fee.key, fee);
  } else if (fee !== 0n) return fail('INVENTORY_FEE', 'No execution fee was reserved.');
  for (const [key, used] of Object.entries(totals))
    if (uint(used) > uint(reservation.debits[key] ?? '0'))
      return fail('INVENTORY_EFFECT', 'Actual debit exceeds the reserved test holdings.');
  for (const [key, reserved] of Object.entries(reservation.debits))
    add(next.available, key, uint(reserved) - uint(totals[key] ?? '0'));
  if (reservation.fee && fee > 0n) {
    add(next.costs, reservation.fee.key, fee);
    convertedCost(
      next,
      reservation.fee.key,
      fee,
      reservation.operation.startsWith('binance.') ? 'venue_fee' : 'gas',
      operationId,
    );
  }
}
function consumeAllowance(next: RunInventory, intent: InventoryIntent, amount: bigint): void {
  if (!intent.spender) return;
  const key = allowanceKey(intent.source, intent.token, intent.spender);
  const known = next.allowances[key];
  if (known !== undefined)
    next.allowances[key] = (uint(known) > amount ? uint(known) - amount : 0n).toString();
}
/** Revision-5 backing is exact floor arithmetic, never a generic position-loss tolerance. */
function supplyBackingUnits(
  next: RunInventory,
  intent: InventoryIntent,
  bindingEffect: Input,
  effects: Input,
  position: Input,
  underlying: bigint,
): bigint {
  const tracked = uint(next.available[suppliedKey(intent.source, intent.token)] ?? '0');
  const observed = uint(object(position.supplied).units);
  if (effects.supplyBacking === undefined) {
    if (observed < tracked + underlying)
      return fail(
        'INVENTORY_SUPPLY_BACKING',
        'Supply backing below the underlying debit requires exact scaled-mint proof.',
      );
    return underlying;
  }
  const proof = object(effects.supplyBacking);
  const ray = 10n ** 27n;
  const mintIndex = uint(proof.mintIndexRay);
  const incomeIndex = uint(proof.normalizedIncomeRay);
  const previousIndex = uint(proof.previousIndexRay);
  const before = uint(proof.scaledBeforeUnits);
  const minted = uint(proof.scaledMintedUnits);
  const after = uint(proof.scaledAfterUnits);
  const backed = uint(proof.backedPrincipalUnits);
  const beforeAtMint = uint(proof.suppliedBeforeAtMintUnits);
  const post = uint(proof.suppliedAfterUnits);
  const balanceIncrease = uint(proof.mintBalanceIncreaseUnits);
  const aToken = String(proof.aToken).toLowerCase();
  const maximumUint256 = 2n ** 256n - 1n;
  if (
    proof.method !== 'aave-v3-revision-5-floor-scaled-mint' ||
    uint(proof.ray) !== ray ||
    bindingEffect.aTokenRevision !== '5' ||
    !/^0x[0-9a-f]{40}$/.test(aToken) ||
    aToken !== String(bindingEffect.aToken).toLowerCase() ||
    aToken !== String(position.aToken).toLowerCase() ||
    uint(proof.underlyingUnits) !== underlying ||
    before !== uint(bindingEffect.scaledSuppliedBeforeUnits) ||
    mintIndex < ray ||
    mintIndex >= 2n ** 128n ||
    incomeIndex < mintIndex ||
    previousIndex > mintIndex ||
    (previousIndex < ray && (previousIndex !== 0n || before !== 0n)) ||
    after >= 2n ** 120n ||
    underlying * ray > maximumUint256 ||
    before * previousIndex > maximumUint256 ||
    after * mintIndex > maximumUint256 ||
    after * incomeIndex > maximumUint256 ||
    minted === 0n ||
    minted !== (underlying * ray) / mintIndex ||
    after !== before + minted ||
    beforeAtMint !== (before * mintIndex) / ray ||
    beforeAtMint < uint(intent.suppliedBeforeUnits) ||
    beforeAtMint < tracked ||
    (uint(intent.suppliedBeforeUnits) === 0n && before !== 0n) ||
    backed === 0n ||
    backed > underlying ||
    backed !== (after * mintIndex) / ray - beforeAtMint ||
    balanceIncrease !== beforeAtMint - (before * previousIndex) / ray ||
    post !== (after * incomeIndex) / ray ||
    post !== observed ||
    post < tracked + backed ||
    signed(proof.roundingDeltaUnits) !== backed - underlying ||
    uint(proof.inclusionIncomeUnits) !== post - (after * mintIndex) / ray ||
    (proof.mintValueUnits !== undefined && uint(proof.mintValueUnits) !== backed + balanceIncrease)
  )
    return fail(
      'INVENTORY_SUPPLY_BACKING',
      'Supply scaled-mint identity, floor arithmetic or observed backing does not match.',
    );
  return backed;
}
/** Pending, unknown, missing and mismatched evidence keep the original reservation; never synthesize credits. */
export function settleInventory(inventory: RunInventory, record: RecordOperation): RunInventory {
  if (inventory.settled.includes(record.id)) return structuredClone(inventory);
  const reservation = inventory.inFlight[record.id];
  if (!reservation)
    return fail(
      'INVENTORY_RESERVATION_MISSING',
      'This operation has no durable scoped inventory reservation.',
    );
  if (['submitted', 'pending', 'unknown', 'awaiting_approval'].includes(record.status))
    return structuredClone(inventory);
  const next = structuredClone(inventory);
  const r = next.inFlight[record.id]!;
  try {
    if (
      !record.preview ||
      fingerprint({ operation: record.operation, binding: record.preview.binding }) !==
        r.bindingHash
    )
      return fail('INVENTORY_IDENTITY', 'Settled record differs from the reserved operation.');
    if (!record.submission) {
      if (!['failed', 'rejected', 'cancelled', 'expired'].includes(record.status))
        return fail(
          'INVENTORY_EVIDENCE',
          'An unsubmitted record cannot prove a completed financial effect.',
        );
      refundAndConsume(next, r, {}, 0n, record.id);
    } else if (record.submission.kind === 'ethereum') {
      const evidence = object(record.evidence);
      confirmations(evidence, next);
      const fee = uint(evidence.feeWei);
      if (record.submission.reference !== evidence.transactionHash)
        return fail(
          'INVENTORY_IDENTITY',
          'Receipt transaction hash differs from the durable submission.',
        );
      if (record.status === 'failed') {
        if (evidence.receiptStatus !== 'reverted')
          return fail('INVENTORY_EVIDENCE', 'Failed submission does not prove a confirmed revert.');
        refundAndConsume(next, r, {}, fee, record.id);
      } else {
        if (record.status !== 'completed' || evidence.receiptStatus !== 'success')
          return fail('INVENTORY_EVIDENCE', 'Confirmed successful effect evidence is required.');
        const effects = object(evidence.effects);
        const intent = r.intent;
        const qty = uint(intent.amountUnits);
        const debit: Record<string, string> = {};
        let interest = 0n;
        const sourceHolding = holdingKey(intent.source, intent.token);
        if (intent.operation === 'wallet.transfer') {
          const received = uint(effects.receivedUnits);
          if (received !== qty || String(effects.recipient).toLowerCase() !== intent.recipient)
            return fail(
              'INVENTORY_EFFECT',
              'Recipient quantity differs from the reserved transfer.',
            );
          if (intent.destinationAccount) {
            const credit = object(evidence.depositCredit);
            const asset = binanceInventoryAsset(String(credit.coin));
            if (
              credit.state !== 'credited' ||
              credit.account !== intent.destinationAccount ||
              String(credit.transactionHash).toLowerCase() !==
                String(evidence.transactionHash).toLowerCase() ||
              asset.token !== intent.token ||
              uint(credit.amountUnits) !== received
            )
              return fail(
                'INVENTORY_DEPOSIT_PENDING',
                'Chain receipt is not yet correlated with usable Binance credit.',
              );
          }
          moveConversions(
            next,
            sourceHolding,
            intent.destination!,
            intent.token,
            received,
            received,
            record.id,
            true,
          );
          add(debit, sourceHolding, received);
          add(next.available, intent.destination!, received);
        } else if (intent.operation === 'wallet.wrap' || intent.operation === 'wallet.unwrap') {
          if (uint(effects.amountUnits) !== qty)
            return fail(
              'INVENTORY_EFFECT',
              'Wrap quantity differs from reserved representation change.',
            );
          const inputToken = intent.operation === 'wallet.wrap' ? 'native' : WETH;
          const outputToken = intent.operation === 'wallet.wrap' ? WETH : 'native';
          moveConversions(
            next,
            holdingKey(intent.source, inputToken),
            intent.destination!,
            outputToken,
            qty,
            qty,
            record.id,
          );
          add(debit, holdingKey(intent.source, inputToken), qty);
          add(next.available, intent.destination!, qty);
        } else if (intent.operation === 'wallet.approve') {
          if (
            uint(effects.allowanceUnits) !== qty ||
            String(effects.spender).toLowerCase() !== intent.spender
          )
            return fail('INVENTORY_EFFECT', 'Allowance differs from reserved authority.');
          next.allowances[allowanceKey(intent.source, intent.token, intent.spender!)] =
            qty.toString();
        } else if (intent.operation === 'uniswap.swap') {
          const input = uint(effects.inputUnits);
          const output = uint(effects.outputUnits);
          if (
            input !== qty ||
            output < uint(intent.minimumOutputUnits) ||
            String(effects.recipient).toLowerCase() !== intent.recipient
          )
            return fail(
              'INVENTORY_EFFECT',
              'Swap quantities or recipient differ from confirmed bounds.',
            );
          moveConversions(
            next,
            sourceHolding,
            intent.destination!,
            intent.outputToken!,
            input,
            output,
            record.id,
            true,
          );
          add(debit, sourceHolding, input);
          add(next.available, intent.destination!, output);
          economic(next.economicEffects, sourceHolding, -input);
          economic(next.economicEffects, intent.destination!, output);
          consumeAllowance(next, intent, input);
        } else if (intent.operation.startsWith('aave.')) {
          const actual = uint(effects.actualAmountUnits);
          const position = object(effects.position);
          const supplied = suppliedKey(intent.source, intent.token);
          const debt = debtKey(intent.source, intent.token);
          if (intent.operation === 'aave.supply') {
            if (actual !== qty)
              return fail('INVENTORY_EFFECT', 'Supply quantity differs from reserved underlying.');
            const backed = supplyBackingUnits(
              next,
              intent,
              object(record.preview.binding.effect),
              effects,
              position,
              actual,
            );
            const converted = Object.values(next.conversions).filter(
              (c) => c.currentKey === sourceHolding,
            );
            const tagged = converted.reduce((sum, c) => sum + uint(c.units), 0n);
            const origin = converted[0];
            if (
              origin &&
              tagged >= actual &&
              converted.every(
                (c) => c.originKey === origin.originKey && c.originToken === origin.originToken,
              ) &&
              uint(intent.suppliedBeforeUnits) === 0n
            )
              next.positionOrigins[supplied] = {
                originKey: origin.originKey,
                originToken: origin.originToken,
              };
            moveConversions(next, sourceHolding, supplied, intent.token, actual, backed, record.id);
            add(debit, sourceHolding, actual);
            add(next.available, supplied, backed);
            economic(next.economicEffects, sourceHolding, -actual);
            if (uint(intent.suppliedBeforeUnits) === 0n) next.introducedPositions[supplied] = true;
            consumeAllowance(next, intent, actual);
          } else if (intent.operation === 'aave.borrow') {
            if (actual !== qty)
              return fail(
                'INVENTORY_EFFECT',
                'Borrow quantity differs from reserved additional assets.',
              );
            add(next.available, sourceHolding, actual);
            add(next.debts, debt, actual);
            economic(next.loanCash, debt, actual);
          } else if (intent.operation === 'aave.repay') {
            if (actual <= 0n || actual > qty)
              return fail('INVENTORY_EFFECT', 'Repay debit exceeds its test holding reservation.');
            const principal = uint(next.debts[debt] ?? '0');
            if (principal === 0n)
              return fail('INVENTORY_DEBT', 'Repayment cannot discharge unrelated debt.');
            const remaining = uint(object(position.variableDebt).units);
            if (intent.repayAll && remaining !== 0n)
              return fail('INVENTORY_DEBT', 'Test debt remains after repay-all.');
            add(debit, sourceHolding, actual);
            next.debts[debt] = remaining.toString();
            economic(next.loanCash, debt, -actual);
            if (remaining === 0n) {
              const cash = signed(next.loanCash[debt] ?? '0');
              if (cash > 0n)
                return fail(
                  'INVENTORY_DEBT',
                  'Debt clearing does not reconcile the recorded test principal.',
                );
              interest = -cash;
              add(next.costs, sourceHolding, interest);
              next.loanCash[debt] = '0';
            }
            consumeAllowance(next, intent, actual);
          } else if (intent.operation === 'aave.withdraw') {
            if (actual !== qty)
              return fail(
                'INVENTORY_EFFECT',
                'Withdraw differs from the reserved supplied underlying.',
              );
            moveConversions(
              next,
              supplied,
              intent.destination!,
              intent.token,
              actual,
              actual,
              record.id,
            );
            add(debit, supplied, actual);
            add(next.available, intent.destination!, actual);
            economic(next.economicEffects, intent.destination!, actual);
            if (next.introducedPositions[supplied])
              next.available[supplied] = uint(object(position.supplied).units).toString();
          } else if (intent.operation === 'aave.collateral') {
            if (position.collateralEnabled !== intent.enabled)
              return fail(
                'INVENTORY_EFFECT',
                'Collateral state differs from the reserved setting.',
              );
          } else return fail('INVENTORY_UNSUPPORTED', 'Aave effect cannot be settled.');
        } else return fail('INVENTORY_UNSUPPORTED', 'Ethereum effect cannot be settled.');
        refundAndConsume(next, r, debit, fee, record.id);
        if (interest > 0n)
          convertedCost(next, sourceHolding, interest, 'borrow_interest', record.id);
      }
    } else if (record.submission.kind === 'binance') {
      const evidence = object(record.evidence);
      const intent = r.intent;
      const asset = binanceInventoryAsset(
        String(
          object(record.preview.binding.params).coin ?? object(record.preview.binding.params).asset,
        ),
      );
      if (record.status === 'failed') {
        if (
          (intent.operation === 'binance.withdraw'
            ? evidence.withdrawOrderId
            : evidence.clientTranId) !== record.submission.reference
        )
          return fail(
            'INVENTORY_IDENTITY',
            'Definitive venue failure must match the durable client identity.',
          );
        if (
          evidence.venueState !== 'failed' &&
          evidence.venueStatus !== 'FAILED' &&
          evidence.venueStatus !== 'FAILURE'
        )
          return fail('INVENTORY_EVIDENCE', 'Venue failure is not yet definitive.');
        refundAndConsume(next, r, {}, 0n, record.id);
      } else if (record.status === 'completed' && intent.operation === 'binance.transfer') {
        const qty = uint(intent.amountUnits);
        if (
          evidence.clientTranId !== record.submission.reference ||
          evidence.venueStatus !== 'SUCCESS' ||
          observedUnits(String(evidence.amount), asset.decimals) !== qty ||
          observedUnits(String(evidence.sourceDebitObserved), asset.decimals) < qty ||
          observedUnits(String(evidence.destinationCreditObserved), asset.decimals) < qty
        )
          return fail(
            'INVENTORY_EVIDENCE',
            'Matching internal transfer debit/credit evidence is required.',
          );
        moveConversions(
          next,
          holdingKey(intent.source, intent.token),
          intent.destination!,
          intent.token,
          qty,
          qty,
          record.id,
          true,
        );
        refundAndConsume(
          next,
          r,
          { [holdingKey(intent.source, intent.token)]: qty.toString() },
          0n,
          record.id,
        );
        add(next.available, intent.destination!, qty);
      } else if (record.status === 'completed' && intent.operation === 'binance.withdraw') {
        const received = object(evidence.recipient);
        const units = uint(received.actualUnits);
        const fee = observedUnits(String(evidence.feeAmount), asset.decimals);
        const debit = observedUnits(String(evidence.sourceDebitObserved), asset.decimals);
        if (
          evidence.withdrawOrderId !== record.submission.reference ||
          evidence.venueState !== 'completed' ||
          received.receiptStatus !== 'success' ||
          String(received.recipient).toLowerCase() !== intent.recipient ||
          units <= 0n ||
          debit !== units + fee ||
          uint(received.confirmations) < BigInt(next.scope.confirmations ?? 1)
        )
          return fail(
            'INVENTORY_EVIDENCE',
            'Withdrawal must correlate exact source debit, actual recipient units and venue fee.',
          );
        moveConversions(
          next,
          holdingKey(intent.source, intent.token),
          intent.destination!,
          intent.token,
          units,
          units,
          record.id,
          true,
        );
        refundAndConsume(
          next,
          r,
          { [holdingKey(intent.source, intent.token)]: units.toString() },
          fee,
          record.id,
        );
        add(next.available, intent.destination!, units);
      } else return fail('INVENTORY_EVIDENCE', 'Binance outcome remains unresolved.');
    } else
      return fail('INVENTORY_UNSUPPORTED', 'Submission venue has no inventory reconciliation.');
    delete next.inFlight[record.id];
    next.settled.push(record.id);
    return next;
  } catch (error) {
    // Never partly apply effects when one component is unknown. Preserve the original reservation.
    const unchanged = structuredClone(inventory);
    unchanged.inFlight[record.id]!.reason =
      error instanceof FinError ? error.code : 'INVENTORY_EVIDENCE';
    return unchanged;
  }
}
export function assertInventoryRestorable(inventory: RunInventory): void {
  if (Object.keys(inventory.inFlight).length)
    return fail(
      'INVENTORY_PENDING',
      'Restoration cannot pass while reserved or in-flight test movements remain.',
    );
  if (Object.values(inventory.debts).some((v) => uint(v) !== 0n))
    return fail('INVENTORY_DEBT', 'Restoration cannot pass while test debt remains.');
  if (Object.keys(inventory.conversions).length)
    return fail(
      'INVENTORY_CONVERSION',
      'Restoration cannot pass while moved or converted principal remains in another asset or custody location.',
    );
  if (
    Object.entries(inventory.introducedPositions).some(
      ([key, introduced]) => introduced && uint(inventory.available[key] ?? '0') !== 0n,
    )
  )
    return fail(
      'INVENTORY_POSITION',
      'Restoration cannot pass while a test-created supplied position or accrued dust remains.',
    );
}
