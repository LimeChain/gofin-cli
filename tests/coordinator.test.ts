import test from 'node:test';
import assert from 'node:assert/strict';
import { z } from 'zod';
import { RunCoordinator } from '../src/core/run-coordinator.js';
import { OperationStore } from '../src/core/store.js';
import { BudgetGuard } from '../src/core/budget.js';
import { MAINNET } from '../src/config.js';
import type { Adapter, FinConfig, Preview } from '../src/core/types.js';
const a = '0x0000000000000000000000000000000000000001',
  b = '0x0000000000000000000000000000000000000002';
const WETH = '0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2',
  USDC = '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48';
function fixture() {
  const config: FinConfig = {
    version: 1,
    connections: [
      { name: 'a', kind: 'ethereum', address: a, credential: 'synthetic-only' },
      { name: 'b', kind: 'ethereum', address: b, credential: 'synthetic-only' },
    ],
    ethereum: MAINNET,
  };
  const store = new OperationStore(':memory:');
  let price = 1n;
  let unrelatedSupply = '0',
    supplied = '0',
    allowance = '0';
  const balances: Record<string, string> = {
    a: '100000000000000000000',
    b: '100000000000000000000',
  };
  const wethHoldings: Record<string, string> = { a: '0', b: '0' };
  const adapter: Adapter & { quoteUsd: Function } = {
    definitions: ['wallet.balances', 'aave.positions', 'wallet.allowance'].map((name) => ({
      name,
      description: 'isolated synthetic',
      kind: 'read',
      schema: z.object({}),
      examples: [],
      coverage: 'fixture',
      completion: 'read',
    })),
    read: async (op, input) => {
      if (op === 'wallet.balances')
        return {
          data: {
            holdings: [
              { address: 'native', units: balances[String(input.wallet)] },
              { address: WETH, units: wethHoldings[String(input.wallet)] },
              { address: USDC, units: '0' },
            ],
          },
        };
      if (op === 'aave.positions')
        return {
          data: {
            positions: [
              WETH,
              USDC,
              ...(input.tokens ? [] : ['0x0000000000000000000000000000000000000010']),
            ].map((address) => ({
              token: { address },
              supplied: {
                units:
                  address === WETH
                    ? supplied
                    : address === '0x0000000000000000000000000000000000000010'
                      ? unrelatedSupply
                      : '0',
              },
              variableDebt: { units: '0' },
              stableDebt: { units: '0' },
              collateralEnabled: false,
            })),
            account: { totalCollateralBase: '0', totalDebtBase: '0' },
          },
        };
      return { data: { units: allowance } };
    },
    quoteUsd: async (_token: string, units: bigint) => ({
      valueUsdMicros: ((units / 1000000000000n) * price).toString(),
      observedAt: new Date().toISOString(),
      source: 'synthetic $1 ETH oracle',
    }),
    preview: async () => {
      throw Error('unused');
    },
    submit: async () => {
      throw Error('unused');
    },
    reconcile: async () => {
      throw Error('unused');
    },
  };
  return {
    store,
    wethHoldings,
    config,
    adapter,
    coordinator: new RunCoordinator(config, [adapter], store),
    setPrice: (v: bigint) => {
      price = v;
    },
    setUnrelatedSupply: (v: string) => {
      unrelatedSupply = v;
    },
    setSupply: (v: string) => {
      supplied = v;
    },
    setAllowance: (v: string) => {
      allowance = v;
    },
    balances,
  };
}
function plan() {
  return {
    id: 'synthetic-plan',
    allocations: { 'a/holding/native': '70000000000000000000' },
    borrowReserveUsdMicros: '0',
    costReserveUsdMicros: '20000000',
    wallets: ['a', 'b'],
    accounts: [],
    returnRoutes: [],
    unwind: ['Return principal to selected source and reconcile gas.'],
  };
}
function transfer(wallet = 'a', recipient = b): Preview {
  return {
    operation: 'wallet.transfer',
    request: {},
    binding: {
      wallet,
      account: wallet === 'a' ? a : b,
      chainId: 1,
      token: 'native',
      decimals: 18,
      amountUnits: '1000000000000000000',
      effect: { recipient },
      gas: '21000',
      maxFeePerGas: '1',
    },
    summary: 'synthetic',
    checks: [],
    warnings: [],
    observedAt: new Date().toISOString(),
    expiresAt: new Date(Date.now() + 60000).toISOString(),
    valuation: {
      movementUsdMicros: '1000000',
      maximumFeeUsdMicros: '1',
      observedAt: new Date().toISOString(),
      source: 'synthetic',
    },
  };
}
test('production coordinator values exact selected units independently; two source allocations above $100 cannot hide in claimed amounts', async () => {
  const f = fixture();
  await assert.rejects(
    () =>
      f.coordinator.begin({
        ...plan(),
        allocations: {
          'a/holding/native': '70000000000000000000',
          'b/holding/native': '70000000000000000000',
        },
      }),
    { code: 'ALLOCATION_LIMIT' },
  );
  await assert.rejects(
    () =>
      f.coordinator.begin({
        ...plan(),
        allocations: { 'a/holding/native': '101000000000000000000' },
      }),
    { code: 'ALLOCATION_BALANCE' },
  );
});
test('live scope excludes unrelated sources, recipients, assets and spenders', async () => {
  const f = fixture();
  const run = await f.coordinator.begin(plan());
  const budget = new BudgetGuard(f.store);
  assert.throws(() => budget.check(transfer('b', a), run.id, 'unallocated'), {
    code: 'INVENTORY_INSUFFICIENT',
  });
  assert.throws(
    () =>
      budget.check(
        transfer('a', '0x0000000000000000000000000000000000000003'),
        run.id,
        'recipient',
      ),
    { code: 'INVENTORY_SCOPE' },
  );
  const token = transfer();
  token.binding.token = '0x0000000000000000000000000000000000000004';
  assert.throws(() => budget.check(token, run.id, 'token'), { code: 'INVENTORY_SCOPE' });
});
test('aggregate allocation is revalued before each write and blocks a price-driven cap breach', async () => {
  const f = fixture();
  await f.coordinator.begin(plan());
  await f.coordinator.revalueAllocation(plan().id);
  f.setPrice(2n);
  await assert.rejects(() => f.coordinator.revalueAllocation(plan().id), {
    code: 'ALLOCATION_REVALUED_LIMIT',
  });
});
test('known principal displacement cannot be submitted as an explanation for restored state', async () => {
  const f = fixture();
  await f.coordinator.begin(plan());
  f.balances.a = '99000000000000000000';
  f.balances.b = '101000000000000000000';
  const result = await f.coordinator.finish(plan().id);
  assert.equal(result.status, 'cleanup_outstanding');
  assert.ok(result.differences?.['a/holding/native']);
  assert.ok(result.differences?.['b/holding/native']);
});

test('all-reserve baseline rejects unrelated non-collateral supply outside representative tokens', async () => {
  const f = fixture();
  f.setUnrelatedSupply('1');
  await assert.rejects(() => f.coordinator.begin(plan()), { code: 'ISOLATED_POSITION_REQUIRED' });
});

test('aggregate allocation values observed accrued supplied units before every write', async () => {
  const f = fixture();
  const run = await f.coordinator.begin(plan());
  run.inventory!.available['a/holding/native'] = '0';
  run.inventory!.available[`a/supplied/${WETH}`] = '70000000000000000000';
  run.inventory!.introducedPositions[`a/supplied/${WETH}`] = true;
  f.store.saveRun(run.id, run);
  f.setSupply('101000000000000000000');
  await assert.rejects(() => f.coordinator.revalueAllocation(run.id), {
    code: 'ALLOCATION_REVALUED_LIMIT',
  });
  f.setSupply('69000000000000000000');
  await assert.rejects(() => f.coordinator.revalueAllocation(run.id), {
    code: 'INVENTORY_POSITION_CHANGED',
  });
});

test('preexisting spender authority is rejected before a test can make restoration impossible', async () => {
  const f = fixture();
  f.setAllowance('1000000000000000000000');
  await assert.rejects(() => f.coordinator.begin(plan()), { code: 'ISOLATED_ALLOWANCE_REQUIRED' });
});

test('unwind net return must fit the exchange deposit minimum before any outward action', async () => {
  const f = fixture();
  f.config.connections.push({ name: 'main', kind: 'binance', role: 'main' });
  for (const name of ['binance.networks', 'binance.deposit-address'])
    f.adapter.definitions.push({
      name,
      description: 'synthetic route fixture',
      kind: 'read',
      schema: z.object({}),
      examples: [],
      coverage: 'fixture',
      completion: 'read',
    });
  const originalRead = f.adapter.read;
  f.adapter.read = async (op, input) =>
    op === 'binance.networks'
      ? {
          assets: [
            {
              networks: [
                {
                  network: 'ETH',
                  depositEnable: true,
                  withdrawEnable: true,
                  busy: false,
                  depositDust: '0.1',
                },
              ],
            },
          ],
        }
      : op === 'binance.deposit-address'
        ? { address: '0x0000000000000000000000000000000000000003' }
        : originalRead(op, input);
  const route = {
    account: 'main',
    coin: 'ETH',
    network: 'ETH',
    wallet: 'a',
    withdrawAmount: '0.2',
    maxWithdrawalFee: '0.01',
    minimumReceipt: '0.19',
    minimumReturnAmount: '0.01',
  };
  await assert.rejects(
    () => f.coordinator.begin({ ...plan(), accounts: ['main'], returnRoutes: [route] }),
    { code: 'RETURN_MINIMUM' },
  );
  await assert.rejects(
    () =>
      f.coordinator.begin({
        ...plan(),
        accounts: ['main'],
        returnRoutes: [{ ...route, minimumReturnAmount: '0.2' }],
      }),
    { code: 'RETURN_MINIMUM' },
  );
});

test('closed native-funded WETH roundtrip preserves unrelated baseline WETH while reconciling its exact costs', async () => {
  const f = fixture();
  f.wethHoldings.a = '5000000000000000000';
  const run = await f.coordinator.begin(plan());
  run.inventory!.costs['a/holding/native'] = '1000000000000000000';
  run.inventory!.economicEffects[`a/holding/${WETH}`] = '-1000000000000000000';
  f.store.saveRun(run.id, run);
  f.balances.a = '98000000000000000000';
  const result = await f.coordinator.finish(run.id);
  assert.equal(result.status, 'restored');
  assert.deepEqual(result.differences, {});
  // A WETH starting allocation remains distinct and cannot have its loss charged to native gas.
  const g = fixture();
  g.wethHoldings.a = '5000000000000000000';
  const second = await g.coordinator.begin({
    ...plan(),
    allocations: { ...plan().allocations, [`a/holding/${WETH}`]: '1000000000000000000' },
  });
  second.inventory!.costs['a/holding/native'] = '1000000000000000000';
  second.inventory!.economicEffects[`a/holding/${WETH}`] = '-1000000000000000000';
  g.store.saveRun(second.id, second);
  g.balances.a = '98000000000000000000';
  assert.equal((await g.coordinator.finish(second.id)).status, 'cleanup_outstanding');
});

test('consolidation preflight uses only selected child funds and independently snapshots frozen and withdrawing amounts', async () => {
  const f = fixture();
  f.config.connections.push(
    { name: 'main', kind: 'binance', role: 'main' },
    {
      name: 'child',
      kind: 'binance',
      role: 'sub',
      parent: 'main',
      email: 'synthetic@example.invalid',
    },
    {
      name: 'unselected',
      kind: 'binance',
      role: 'sub',
      parent: 'main',
      email: 'other@example.invalid',
    },
  );
  const reads = [
    'binance.networks',
    'binance.deposit-address',
    'binance.permissions',
    'binance.hierarchy',
    'binance.balances',
    'binance.withdrawal-history',
    'binance.deposit-history',
    'binance.open-orders',
  ];
  for (const name of [...reads, 'binance.withdraw'])
    f.adapter.definitions.push({
      name,
      description: 'synthetic funding',
      kind: name === 'binance.withdraw' ? 'write' : 'read',
      schema: z.object({}),
      examples: [],
      coverage: 'fixture',
      completion: 'read',
    });
  const originalRead = f.adapter.read;
  f.adapter.read = async (op, input) => {
    if (op === 'binance.networks')
      return {
        assets: [
          {
            networks: [
              {
                network: 'ETH',
                depositEnable: true,
                withdrawEnable: true,
                busy: false,
                depositDust: '0.001',
              },
            ],
          },
        ],
      };
    if (op === 'binance.deposit-address')
      return { address: '0x0000000000000000000000000000000000000003' };
    if (op === 'binance.permissions') return { permissions: { enableInternalTransfer: true } };
    if (op === 'binance.hierarchy') return { accounts: [{ name: 'child', ownership: 'observed' }] };
    if (op === 'binance.balances')
      return {
        balances: [
          {
            asset: 'USDC',
            free: input.account === 'child' ? '20' : '0',
            locked: '0',
            ...(input.account === 'child' ? { freeze: '0.5', withdrawing: '0.25' } : {}),
          },
        ],
      };
    if (op.endsWith('history')) return { records: [], complete: true };
    if (op === 'binance.open-orders') return { orders: [] };
    return originalRead(op, input);
  };
  const originalPrice = f.adapter.quoteUsd;
  f.adapter.quoteUsd = async (token: string, units: bigint) =>
    token === USDC
      ? {
          valueUsdMicros: units.toString(),
          observedAt: new Date().toISOString(),
          source: 'synthetic USDC oracle',
        }
      : originalPrice(token, units);
  let receivedSources: unknown;
  const funded = f.adapter as typeof f.adapter & { preflightFundedWithdrawal?: Function };
  funded.preflightFundedWithdrawal = async (_input: unknown, sources: unknown, cap: string) => {
    receivedSources = sources;
    assert.equal(cap, '100000000');
    return {
      ...transfer(),
      operation: 'binance.withdraw',
      binding: { preparationOnly: true },
      valuation: {
        movementUsdMicros: '9200000',
        maximumFeeUsdMicros: '800000',
        observedAt: new Date().toISOString(),
        source: 'synthetic oracle',
      },
    };
  };
  f.adapter.preview = async () => {
    throw Object.assign(Error('Actual main balance unavailable'), { code: 'INSUFFICIENT_BALANCE' });
  };
  const request = {
    ...plan(),
    wallets: ['a'],
    accounts: ['main', 'child'],
    allocations: {
      'a/holding/native': '3000000000000000000',
      [`child/holding/${USDC}`]: '10000000',
    },
    costReserveUsdMicros: '2000000',
    returnRoutes: [
      {
        account: 'main',
        coin: 'USDC',
        network: 'ETH',
        wallet: 'a',
        withdrawAmount: '9.2',
        maxWithdrawalFee: '0.8',
        minimumReceipt: '8.4',
        minimumReturnAmount: '8.4',
      },
    ],
  };
  await assert.rejects(
    () =>
      f.coordinator.begin({
        ...request,
        id: 'unselected-main-cannot-fund',
        allocations: { ...request.allocations, [`child/holding/${USDC}`]: '2000000' },
      }),
    { code: 'RETURN_UNFUNDED' },
  );
  f.config.connections.push({
    name: 'alias',
    kind: 'binance',
    role: 'sub',
    parent: 'main',
    email: 'SYNTHETIC@example.invalid',
  });
  await assert.rejects(
    () =>
      f.coordinator.begin({
        ...request,
        id: 'aliased-child',
        accounts: ['main', 'child', 'alias'],
        allocations: {
          ...request.allocations,
          [`child/holding/${USDC}`]: '5000000',
          [`alias/holding/${USDC}`]: '5000000',
        },
      }),
    { code: 'DUPLICATE_SCOPE' },
  );
  const begun = await f.coordinator.begin(request);
  assert.deepEqual(receivedSources, [
    { authority: 'main', from: 'child', coin: 'USDC', amount: '10' },
  ]);
  assert.equal(begun.baseline.quantities[`main/holding/${USDC}`], '0');
  assert.equal(begun.baseline.quantities[`child/order/freeze/${USDC}`], '500000000000000000');
  assert.equal(begun.baseline.quantities[`child/order/withdrawing/${USDC}`], '250000000000000000');
  await assert.rejects(
    () =>
      f.coordinator.begin({
        ...request,
        id: 'excess-child',
        allocations: { ...request.allocations, [`child/holding/${USDC}`]: '21000000' },
      }),
    { code: 'ALLOCATION_BALANCE' },
  );
  delete funded.preflightFundedWithdrawal;
  await assert.rejects(() => f.coordinator.begin({ ...request, id: 'missing-funded-preflight' }), {
    code: 'FUNDED_PREFLIGHT_UNAVAILABLE',
  });
});

test('raw coordinator scope cannot count two names for one physical wallet', async () => {
  const f = fixture();
  f.config.connections[1]!.address = f.config.connections[0]!.address;
  await assert.rejects(() => f.coordinator.begin(plan()), { code: 'DUPLICATE_SCOPE' });
});

function venuePrecisionFixture() {
  const f = fixture();
  f.config.connections.push({ name: 'main', kind: 'binance', role: 'main' });
  const state = {
    free: '20.0000001',
    locked: '0.0000002',
    freeze: '0.0000003',
    withdrawing: '0.0000004',
    depositDust: '0.00100000',
    withdrawFee: '0.80000000',
  };
  for (const name of [
    'binance.balances',
    'binance.withdrawal-history',
    'binance.deposit-history',
    'binance.open-orders',
    'binance.networks',
    'binance.deposit-address',
    'binance.withdraw',
  ])
    f.adapter.definitions.push({
      name,
      description: 'Isolated venue precision fixture',
      kind: name === 'binance.withdraw' ? 'write' : 'read',
      schema: z.object({}),
      examples: [],
      coverage: 'Synthetic no transport',
      completion: 'Synthetic observation',
    });
  const originalRead = f.adapter.read;
  f.adapter.read = async (operation, input) => {
    if (operation === 'binance.balances')
      return {
        balances: [
          {
            asset: 'USDC',
            free: state.free,
            locked: state.locked,
            freeze: state.freeze,
            withdrawing: state.withdrawing,
          },
        ],
      };
    if (operation.endsWith('-history')) return { complete: true, records: [] };
    if (operation === 'binance.open-orders') return { orders: [] };
    if (operation === 'binance.deposit-address')
      return { address: '0x0000000000000000000000000000000000000003' };
    if (operation === 'binance.networks')
      return {
        assets: [
          {
            networks: [
              {
                network: 'ETH',
                depositEnable: true,
                withdrawEnable: true,
                busy: false,
                depositDust: state.depositDust,
                withdrawFee: state.withdrawFee,
              },
            ],
          },
        ],
      };
    return originalRead(operation, input);
  };
  const originalPrice = f.adapter.quoteUsd;
  f.adapter.quoteUsd = async (token: string, units: bigint) =>
    token === USDC
      ? {
          valueUsdMicros: units.toString(),
          observedAt: new Date().toISOString(),
          source: 'Isolated synthetic USDC oracle',
        }
      : originalPrice(token, units);
  f.adapter.preview = async (_operation, request) => ({
    ...transfer(),
    operation: 'binance.withdraw',
    request,
    binding: {},
    valuation: {
      movementUsdMicros: '9200000',
      maximumFeeUsdMicros: '800000',
      source: 'Isolated synthetic USDC oracle',
      observedAt: new Date().toISOString(),
    },
  });
  const request = {
    ...plan(),
    wallets: ['a'],
    accounts: ['main'],
    allocations: {
      'a/holding/native': '3000000000000000000',
      [`main/holding/${USDC}`]: '10000000',
    },
    costReserveUsdMicros: '2000000',
    returnRoutes: [
      {
        account: 'main',
        coin: 'USDC',
        network: 'ETH',
        wallet: 'a',
        withdrawAmount: '9.2',
        maxWithdrawalFee: '0.8',
        minimumReceipt: '8.4',
        minimumReturnAmount: '8.4',
      },
    ],
  };
  return { ...f, state, request };
}
test('venue snapshot floors spendable USDC atomic units and preserves exact free residual and encumbered18-decimal fields', async () => {
  const f = venuePrecisionFixture();
  try {
    const snapshot = await f.coordinator.snapshot(['a'], ['main']);
    assert.equal(snapshot.quantities[`main/holding/${USDC}`], '20000000');
    assert.equal(snapshot.quantities[`main/residual/${USDC}`], '100000000000');
    assert.equal(snapshot.quantities[`main/order/locked/${USDC}`], '200000000000');
    assert.equal(snapshot.quantities[`main/order/freeze/${USDC}`], '300000000000');
    assert.equal(snapshot.quantities[`main/order/withdrawing/${USDC}`], '400000000000');
    assert.equal(
      BigInt(snapshot.quantities[`main/holding/${USDC}`]!) * 10n ** 12n +
        BigInt(snapshot.quantities[`main/residual/${USDC}`]!),
      20_000_000_100_000_000_000n,
    );
    assert.equal(
      (snapshot.metadata['main:quantityUnits'] as { orderDecimals: number }).orderDecimals,
      18,
    );
    await assert.rejects(
      f.coordinator.begin({
        ...f.request,
        allocations: { ...f.request.allocations, [`main/holding/${USDC}`]: '20000001' },
      }),
      { code: 'ALLOCATION_BALANCE' },
    );
    await assert.rejects(
      f.coordinator.begin({
        ...f.request,
        allocations: { ...f.request.allocations, [`main/residual/${USDC}`]: '1' },
      }),
      { code: 'INVENTORY_KEY' },
    );
    const run = await f.coordinator.begin(f.request);
    assert.equal(run.inventory!.initial[`main/holding/${USDC}`], '10000000');
    assert.equal(run.inventory!.initial[`main/residual/${USDC}`], undefined);
  } finally {
    f.store.close();
  }
});
test('meaningful Binance USDC residual changes cannot pass restoration when projected holding is unchanged', async () => {
  for (const free of ['20.0000000', '20.0000002']) {
    const f = venuePrecisionFixture();
    try {
      const run = await f.coordinator.begin(f.request);
      f.state.free = free;
      const final = await f.coordinator.finish(run.id);
      assert.equal(
        final.final!.quantities[`main/holding/${USDC}`],
        run.baseline.quantities[`main/holding/${USDC}`],
      );
      assert.equal(final.status, 'cleanup_outstanding');
      assert.ok(final.differences![`main/residual/${USDC}`]);
    } finally {
      f.store.close();
    }
  }
});
test('proved atomic USDC venue fee reconciles exactly while the initial subatomic residual stays fixed', async () => {
  const f = venuePrecisionFixture();
  try {
    const run = await f.coordinator.begin(f.request);
    run.inventory!.costs[`main/holding/${USDC}`] = '800000';
    run.inventory!.available[`main/holding/${USDC}`] = '9200000';
    f.store.saveRun(run.id, run);
    f.state.free = '19.2000001';
    const restored = await f.coordinator.finish(run.id);
    assert.equal(restored.status, 'restored');
    assert.deepEqual(restored.differences, {});
    assert.equal(restored.final!.quantities[`main/residual/${USDC}`], '100000000000');
    f.state.free = '19.2000002';
    const changedResidual = await f.coordinator.finish(run.id);
    assert.equal(changedResidual.status, 'cleanup_outstanding');
    assert.equal(changedResidual.differences![`main/residual/${USDC}`], '100000000000');
  } finally {
    f.store.close();
  }
});
test('venue route metadata trims zero padding, ceilings deposit dust only, and rejects meaningful subatomic fees or requested quantities', async () => {
  const f = venuePrecisionFixture();
  try {
    f.state.depositDust = '0.0000001';
    const run = await f.coordinator.begin({
      ...f.request,
      returnRoutes: [{ ...f.request.returnRoutes[0]!, minimumReturnAmount: '0.000001' }],
    });
    assert.equal(
      run.inventory!.scope.returnMinimums!['0x0000000000000000000000000000000000000003']![USDC],
      '1',
    );
    assert.equal((await f.coordinator.finish(run.id)).status, 'restored');
    await assert.rejects(
      f.coordinator.begin({
        ...f.request,
        id: 'nonrepresentable-request',
        returnRoutes: [{ ...f.request.returnRoutes[0]!, withdrawAmount: '9.2000001' }],
      }),
      { code: 'INVALID_PRECISION' },
    );
    f.state.withdrawFee = '0.8000001';
    await assert.rejects(f.coordinator.begin({ ...f.request, id: 'nonrepresentable-fee' }), {
      code: 'RETURN_FEE_UNREPRESENTABLE',
    });
  } finally {
    f.store.close();
  }
});
