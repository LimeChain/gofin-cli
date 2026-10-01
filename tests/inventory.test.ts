import test from 'node:test';
import assert from 'node:assert/strict';
import {
  createInventory,
  reserveInventory,
  settleInventory,
  assertInventoryRestorable,
  holdingKey,
  suppliedKey,
  debtKey,
  allowanceKey,
  binanceInventoryAsset,
  type InventoryScope,
  type RunInventory,
} from '../src/core/inventory.js';
import type { Input, Preview, RecordOperation } from '../src/core/types.js';

const A = `0x${'11'.repeat(20)}`,
  B = `0x${'22'.repeat(20)}`,
  DEPOSIT = `0x${'33'.repeat(20)}`;
const HASH = `0x${'44'.repeat(32)}`,
  BLOCK = `0x${'55'.repeat(32)}`;
const POOL = `0x${'66'.repeat(20)}`,
  ROUTER = `0x${'77'.repeat(20)}`,
  ATOKEN = `0x${'88'.repeat(20)}`;
const WETH = '0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2',
  USDC = '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48';
const scope: InventoryScope = {
  walletAddresses: { a: A, b: B },
  accounts: ['main', 'sub'],
  returnDeposits: { [DEPOSIT]: 'main' },
  tokens: ['native', WETH, USDC],
  spenders: [POOL, ROUTER],
  confirmations: 2,
};
function inventory(extra: Record<string, string> = {}): RunInventory {
  return createInventory(scope, {
    [holdingKey('a', 'native')]: '1000',
    [holdingKey('a', WETH)]: '100',
    [holdingKey('a', USDC)]: '10',
    [holdingKey('b', 'native')]: '20',
    [holdingKey('main', 'native')]: '1000',
    [holdingKey('sub', 'native')]: '500',
    ...extra,
  });
}
function preview(
  operation = 'wallet.transfer',
  token = 'native',
  qty = '100',
  effect: Input = { recipient: B },
  extra: Input = {},
): Preview {
  return {
    operation,
    request: { requestId: 'isolated-inventory-request' },
    binding: {
      wallet: 'a',
      account: A,
      chainId: 1,
      token,
      decimals: token === USDC ? 6 : 18,
      amountUnits: qty,
      gas: '10',
      maxFeePerGas: '2',
      effect,
      ...extra,
    },
    summary: 'Synthetic isolated inventory fixture; no signer or transport.',
    checks: [],
    warnings: [],
    observedAt: new Date().toISOString(),
    expiresAt: new Date(Date.now() + 60_000).toISOString(),
  };
}
function record(
  id: string,
  p: Preview,
  effects: Input,
  status: RecordOperation['status'] = 'completed',
  evidence: Input = {},
): RecordOperation {
  return {
    id,
    operation: p.operation,
    requestId: String(p.request.requestId),
    fingerprint: 'synthetic-only',
    input: p.request,
    preview: p,
    status,
    createdAt: new Date().toISOString(),
    updatedAt: new Date().toISOString(),
    submission: {
      kind: p.operation.startsWith('binance.') ? 'binance' : 'ethereum',
      reference: p.operation.startsWith('binance.') ? 'synthetic-binance-id' : HASH,
      data: {},
    },
    evidence: {
      transactionHash: HASH,
      blockHash: BLOCK,
      confirmations: '2',
      feeWei: '2',
      receiptStatus: status === 'failed' ? 'reverted' : 'success',
      effects,
      ...evidence,
    },
  };
}
function settle(
  state: RunInventory,
  id: string,
  p: Preview,
  effects: Input,
  evidence: Input = {},
): RunInventory {
  return settleInventory(
    reserveInventory(state, id, p),
    record(id, p, effects, 'completed', evidence),
  );
}

test('allocation scope uses exact actual token units, rejects aliased wallets and malformed units', () => {
  assert.deepEqual(binanceInventoryAsset('USDC'), { token: USDC, decimals: 6 });
  assert.throws(
    () =>
      createInventory(
        { ...scope, walletAddresses: { a: A, alias: A.toUpperCase().replace('0X', '0x') } },
        { [holdingKey('a', 'native')]: '100' },
      ),
    { code: 'INVENTORY_SCOPE' },
  );
  for (const bad of ['1.1', '-1', '1e18', '01'])
    assert.throws(() => inventory({ [holdingKey('a', USDC)]: bad }), { code: 'INVENTORY_UNITS' });
  assert.throws(() => inventory({ [holdingKey('untracked', 'native')]: '100' }), {
    code: 'INVENTORY_SCOPE',
  });
});
test('two individually small movements cannot select more units than source allocation', () => {
  const initial = inventory({ [holdingKey('a', 'native')]: '150' });
  const p = preview('wallet.transfer', 'native', '100');
  const first = settle(initial, 'first', p, { recipient: B, receivedUnits: '100' });
  assert.equal(first.available[holdingKey('a', 'native')], '48');
  assert.equal(first.available[holdingKey('b', 'native')], '120');
  assert.throws(() => reserveInventory(first, 'second', p), { code: 'INVENTORY_INSUFFICIENT' });
  assert.equal(initial.available[holdingKey('a', 'native')], '150');
});
test('pending reservations hold exact principal and gas and deny even a fresh ID until original outcome reconciles', () => {
  const initial = inventory();
  const p = preview();
  const pending = reserveInventory(initial, 'pending', p);
  assert.equal(pending.available[holdingKey('a', 'native')], '880');
  assert.deepEqual(reserveInventory(pending, 'pending', p), pending);
  assert.throws(() => reserveInventory(pending, 'fresh', p), { code: 'INVENTORY_PENDING' });
  assert.throws(
    () => reserveInventory(pending, 'pending', preview('wallet.transfer', 'native', '101')),
    { code: 'INVENTORY_IDENTITY' },
  );
  assert.deepEqual(settleInventory(pending, record('pending', p, {}, 'unknown')), pending);
  assert.throws(() => assertInventoryRestorable(pending), { code: 'INVENTORY_PENDING' });
});
test('same confirmed settlement applies exactly once and funds can return and be reused', () => {
  const p = preview();
  const pending = reserveInventory(inventory(), 'first', p);
  const r = record('first', p, { recipient: B, receivedUnits: '100' });
  const first = settleInventory(pending, r);
  assert.deepEqual(settleInventory(first, r), first);
  assert.equal(first.costs[holdingKey('a', 'native')], '2');
  const back = preview(
    'wallet.transfer',
    'native',
    '100',
    { recipient: A },
    { wallet: 'b', account: B },
  );
  const returned = settle(first, 'return', back, { recipient: A, receivedUnits: '100' });
  assert.equal(returned.available[holdingKey('a', 'native')], '998');
  assert.equal(returned.available[holdingKey('b', 'native')], '18');
  assert.ok(reserveInventory(returned, 'reuse', p).inFlight.reuse);
  assert.equal(
    Object.keys(returned.economicEffects).length,
    0,
    'Principal custody changes are never costs or economic loss.',
  );
});
test('untracked source, recipient, asset, allowance spender and network are denied before mutation', () => {
  const initial = inventory();
  const cases = [
    preview('wallet.transfer', 'native', '10', { recipient: B }, { wallet: 'untracked' }),
    preview('wallet.transfer', 'native', '10', { recipient: `0x${'99'.repeat(20)}` }),
    preview('wallet.transfer', `0x${'aa'.repeat(20)}`, '10'),
    preview('wallet.approve', WETH, '10', {
      spender: `0x${'bb'.repeat(20)}`,
      previousAllowanceUnits: '0',
    }),
    preview('wallet.transfer', 'native', '10', { recipient: B }, { chainId: 10 }),
  ];
  for (const p of cases)
    assert.throws(() => reserveInventory(initial, 'bad', p), { code: 'INVENTORY_SCOPE' });
  assert.equal(Object.keys(initial.inFlight).length, 0);
});
test('finite allowance is bounded to the source test cash and has no principal movement', () => {
  const initial = inventory();
  const tooMuch = preview('wallet.approve', WETH, '101', {
    spender: ROUTER,
    previousAllowanceUnits: '0',
  });
  assert.throws(() => reserveInventory(initial, 'bad-allowance', tooMuch), {
    code: 'INVENTORY_AUTHORITY',
  });
  const p = preview('wallet.approve', WETH, '100', {
    spender: ROUTER,
    previousAllowanceUnits: '0',
  });
  const next = settle(initial, 'approve', p, { spender: ROUTER, allowanceUnits: '100' });
  assert.equal(next.available[holdingKey('a', WETH)], '100');
  assert.equal(next.allowances[allowanceKey('a', WETH, ROUTER)], '100');
  assert.equal(next.available[holdingKey('a', 'native')], '998');
});
test('wrap and unwrap move one economic holding across representations without doubling allocation', () => {
  const initial = inventory({ [holdingKey('a', WETH)]: '0' });
  const wrap = preview('wallet.wrap', WETH, '100', {});
  const wrapped = settle(initial, 'wrap', wrap, { amountUnits: '100', representation: 'WETH' });
  assert.equal(wrapped.available[holdingKey('a', 'native')], '898');
  assert.equal(wrapped.available[holdingKey('a', WETH)], '100');
  const unwrap = preview('wallet.unwrap', WETH, '100', {});
  const returned = settle(wrapped, 'unwrap', unwrap, {
    amountUnits: '100',
    representation: 'native ETH',
  });
  assert.equal(returned.available[holdingKey('a', 'native')], '996');
  assert.equal(returned.available[holdingKey('a', WETH)], '0');
  assert.equal(returned.economicEffects[holdingKey('a', 'native')], undefined);
});
test('swap tracks actual output and allows transformed proceeds for the inverse swap; only proven net trade difference enters economics', () => {
  const initial = inventory({ [holdingKey('a', USDC)]: '0' });
  const forward = preview('uniswap.swap', WETH, '100', {
    tokenOut: USDC,
    outputDecimals: 6,
    minOutputUnits: '9',
    spender: ROUTER,
    recipient: A,
  });
  const out = settle(initial, 'swap-out', forward, {
    inputUnits: '100',
    outputUnits: '10',
    recipient: A,
  });
  assert.equal(out.available[holdingKey('a', WETH)], '0');
  assert.equal(out.available[holdingKey('a', USDC)], '10');
  const back = preview('uniswap.swap', USDC, '10', {
    tokenOut: WETH,
    outputDecimals: 18,
    minOutputUnits: '89',
    spender: ROUTER,
    recipient: A,
  });
  const returned = settle(out, 'swap-back', back, {
    inputUnits: '10',
    outputUnits: '90',
    recipient: A,
  });
  assert.equal(returned.available[holdingKey('a', WETH)], '90');
  assert.equal(returned.available[holdingKey('a', USDC)], '0');
  assert.equal(returned.economicEffects[holdingKey('a', WETH)], '-10');
  assert.equal(returned.economicEffects[holdingKey('a', USDC)], '0');
});
test('unknown or mismatched evidence never partially creates outputs or releases reservations', () => {
  const p = preview('uniswap.swap', WETH, '100', {
    tokenOut: USDC,
    outputDecimals: 6,
    minOutputUnits: '9',
    spender: ROUTER,
    recipient: A,
  });
  const pending = reserveInventory(inventory(), 'swap', p);
  const bad = settleInventory(
    pending,
    record('swap', p, { inputUnits: '100', outputUnits: '8', recipient: A }),
  );
  assert.deepEqual(bad.available, pending.available);
  assert.ok(bad.inFlight.swap);
  assert.equal(bad.inFlight.swap!.reason, 'INVENTORY_EFFECT');
  assert.equal(bad.settled.length, 0);
  const feeOver = settleInventory(
    pending,
    record('swap', p, { inputUnits: '100', outputUnits: '10', recipient: A }, 'completed', {
      feeWei: '21',
    }),
  );
  assert.deepEqual(feeOver.available, pending.available);
  assert.equal(feeOver.inFlight.swap!.reason, 'INVENTORY_FEE');
});
test('confirmed revert spends only actual gas; unconfirmed revert keeps funds in flight', () => {
  const p = preview();
  const pending = reserveInventory(inventory(), 'revert', p);
  const r = record('revert', p, {}, 'failed');
  const reverted = settleInventory(pending, r);
  assert.equal(reverted.available[holdingKey('a', 'native')], '998');
  assert.equal(reverted.available[holdingKey('b', 'native')], '20');
  assert.equal(reverted.costs[holdingKey('a', 'native')], '2');
  assert.equal(Object.keys(reverted.inFlight).length, 0);
  const unconfirmed = settleInventory(pending, {
    ...r,
    evidence: { ...r.evidence, confirmations: '1' },
  });
  assert.ok(unconfirmed.inFlight.revert);
  assert.equal(unconfirmed.inFlight.revert!.reason, 'INVENTORY_EVIDENCE');
});
test('pre-submission rejection refunds reservation without consuming test money', () => {
  const p = preview();
  const initial = inventory();
  const pending = reserveInventory(initial, 'declined', p);
  const rejected = { ...record('declined', p, {}, 'rejected'), submission: undefined };
  const returned = settleInventory(pending, rejected);
  assert.deepEqual(returned.available, initial.available);
  assert.equal(Object.keys(returned.costs).length, 0);
});
test('Aave supply/withdraw uses the underlying position bucket once and reconciles accrued supply interest', () => {
  const supply = preview('aave.supply', WETH, '100', {
    spender: POOL,
    suppliedBeforeUnits: '0',
    debtBeforeUnits: '0',
  });
  const supplied = settle(inventory(), 'supply', supply, {
    actualAmountUnits: '100',
    position: { supplied: { units: '100' }, variableDebt: { units: '0' }, collateralEnabled: true },
  });
  assert.equal(supplied.available[holdingKey('a', WETH)], '0');
  assert.equal(supplied.available[suppliedKey('a', WETH)], '100');
  assert.throws(() => assertInventoryRestorable(supplied), { code: 'INVENTORY_POSITION' });
  const withdraw = preview('aave.withdraw', WETH, '102', {
    recipient: A,
    suppliedBeforeUnits: '102',
    debtBeforeUnits: '0',
  });
  const returned = settle(supplied, 'withdraw', withdraw, {
    actualAmountUnits: '102',
    position: { supplied: { units: '0' }, variableDebt: { units: '0' }, collateralEnabled: false },
  });
  assert.equal(returned.available[holdingKey('a', WETH)], '102');
  assert.equal(returned.available[suppliedKey('a', WETH)], '0');
  assert.equal(returned.economicEffects[holdingKey('a', WETH)], '2');
  assert.doesNotThrow(() => assertInventoryRestorable(returned));
});
function floorSupplyFixture(): { p: Preview; effects: Input } {
  return {
    p: preview('aave.supply', WETH, '100', {
      spender: POOL,
      aToken: ATOKEN,
      aTokenRevision: '5',
      scaledSuppliedBeforeUnits: '0',
      suppliedBeforeUnits: '0',
      debtBeforeUnits: '0',
    }),
    effects: {
      actualAmountUnits: '100',
      position: {
        aToken: ATOKEN,
        supplied: { units: '99' },
        variableDebt: { units: '0' },
        collateralEnabled: true,
      },
      supplyBacking: {
        method: 'aave-v3-revision-5-floor-scaled-mint',
        ray: '1000000000000000000000000000',
        aToken: ATOKEN,
        underlyingUnits: '100',
        mintIndexRay: '1100000000000000000000000000',
        normalizedIncomeRay: '1100000000000000000000000000',
        previousIndexRay: '0',
        scaledBeforeUnits: '0',
        scaledMintedUnits: '90',
        scaledAfterUnits: '90',
        suppliedBeforeAtMintUnits: '0',
        backedPrincipalUnits: '99',
        suppliedAfterUnits: '99',
        mintBalanceIncreaseUnits: '0',
        mintValueUnits: '99',
        roundingDeltaUnits: '-1',
        inclusionIncomeUnits: '0',
      },
    },
  };
}
test('proved Aave floor backing charges supply rounding exactly once on the native return path', () => {
  let state = inventory({ [holdingKey('a', WETH)]: '0' });
  state = settle(state, 'wrap-floor', preview('wallet.wrap', WETH, '100', {}), {
    amountUnits: '100',
  });
  const { p, effects } = floorSupplyFixture();
  state = settle(state, 'supply-floor', p, effects);
  assert.equal(state.available[suppliedKey('a', WETH)], '99');
  assert.equal(state.available[holdingKey('a', WETH)], '0');
  assert.equal(state.economicEffects[holdingKey('a', WETH)], '-100');
  assert.equal(state.costs[holdingKey('a', WETH)], undefined);
  assert.deepEqual(settleInventory(state, record('supply-floor', p, effects)), state);
  assert.throws(
    () =>
      reserveInventory(
        state,
        'unexplained-decrease',
        preview('aave.withdraw', WETH, '98', {
          recipient: A,
          suppliedBeforeUnits: '98',
          debtBeforeUnits: '0',
        }),
      ),
    { code: 'INVENTORY_POSITION_CHANGED' },
  );
  state = settle(
    state,
    'withdraw-floor',
    preview('aave.withdraw', WETH, '99', {
      recipient: A,
      suppliedBeforeUnits: '99',
      debtBeforeUnits: '0',
    }),
    {
      actualAmountUnits: '99',
      position: { supplied: { units: '0' }, variableDebt: { units: '0' } },
    },
  );
  state = settle(state, 'unwrap-floor', preview('wallet.unwrap', WETH, '99', {}), {
    amountUnits: '99',
  });
  assert.doesNotThrow(() => assertInventoryRestorable(state));
  assert.equal(Object.keys(state.conversions).length, 0);
  assert.equal(state.costs[holdingKey('a', 'native')], '8');
  assert.equal(state.economicEffects[holdingKey('a', WETH)], '-1');
  assert.equal(state.available[holdingKey('a', 'native')], '991');
  assert.equal(
    BigInt(state.initial[holdingKey('a', 'native')]!) -
      BigInt(state.costs[holdingKey('a', 'native')]!) +
      BigInt(state.economicEffects[holdingKey('a', WETH)]!),
    991n,
    'Rounding debit appears only in the closed underlying supply/withdraw net.',
  );
});
test('proved supply backing preserves converted principal origin through withdrawal and exact inverse swap', () => {
  let state = inventory({ [holdingKey('a', WETH)]: '0' });
  state = settle(
    state,
    'converted-floor-input',
    preview('uniswap.swap', USDC, '10', {
      tokenOut: WETH,
      outputDecimals: 18,
      minOutputUnits: '100',
      spender: ROUTER,
      recipient: A,
    }),
    { inputUnits: '10', outputUnits: '100', recipient: A },
  );
  const { p, effects } = floorSupplyFixture();
  state = settle(state, 'converted-floor-supply', p, effects);
  assert.deepEqual(Object.values(state.conversions), [
    {
      originKey: holdingKey('a', USDC),
      originToken: USDC,
      currentKey: suppliedKey('a', WETH),
      currentToken: WETH,
      units: '99',
    },
  ]);
  assert.deepEqual(state.positionOrigins[suppliedKey('a', WETH)], {
    originKey: holdingKey('a', USDC),
    originToken: USDC,
  });
  state = settle(
    state,
    'converted-floor-withdraw',
    preview('aave.withdraw', WETH, '99', {
      recipient: A,
      suppliedBeforeUnits: '99',
      debtBeforeUnits: '0',
    }),
    {
      actualAmountUnits: '99',
      position: { supplied: { units: '0' }, variableDebt: { units: '0' } },
    },
  );
  assert.equal(Object.values(state.conversions)[0]?.units, '99');
  state.cleanupOutstanding = true;
  state = settle(
    state,
    'converted-floor-return',
    preview('uniswap.swap', WETH, '99', {
      tokenOut: USDC,
      outputDecimals: 6,
      minOutputUnits: '9',
      spender: ROUTER,
      recipient: A,
    }),
    { inputUnits: '99', outputUnits: '9', recipient: A },
  );
  assert.doesNotThrow(() => assertInventoryRestorable(state));
  assert.equal(Object.keys(state.conversions).length, 0);
  assert.equal(state.economicEffects[holdingKey('a', WETH)], '0');
  assert.equal(state.economicEffects[holdingKey('a', USDC)], '-1');
  assert.equal(state.available[holdingKey('a', WETH)], '0');
  assert.equal(state.available[holdingKey('a', USDC)], '9');
});
test('Aave short backing without exact proof cannot credit a position or release its original reservation', () => {
  const { p, effects } = floorSupplyFixture();
  delete effects.supplyBacking;
  const reserved = reserveInventory(inventory(), 'missing-floor-proof', p);
  const unresolved = settleInventory(reserved, record('missing-floor-proof', p, effects));
  assert.equal(unresolved.inFlight['missing-floor-proof']?.reason, 'INVENTORY_SUPPLY_BACKING');
  assert.deepEqual(unresolved.available, reserved.available);
  assert.deepEqual(unresolved.costs, reserved.costs);
  assert.deepEqual(unresolved.conversions, reserved.conversions);
  assert.deepEqual(unresolved.settled, []);
});
test('mismatched Aave mint identity, binding, floor math and backing stay unresolved even with a confirmed receipt', () => {
  const mutations: Array<[string, (p: Preview, effects: Input) => void]> = [
    ['wrong-version', (_, e) => ((e.supplyBacking as Input).method = 'aave-v3-half-up')],
    ['wrong-ray', (_, e) => ((e.supplyBacking as Input).ray = '1000000000000000000000000001')],
    ['wrong-atoken', (_, e) => ((e.supplyBacking as Input).aToken = POOL)],
    ['wrong-position-atoken', (_, e) => ((e.position as Input).aToken = POOL)],
    ['unbound-revision', (p) => ((p.binding.effect as Input).aTokenRevision = '4')],
    ['unbound-scaled-before', (p) => ((p.binding.effect as Input).scaledSuppliedBeforeUnits = '1')],
    ['wrong-underlying', (_, e) => ((e.supplyBacking as Input).underlyingUnits = '99')],
    ['zero-index', (_, e) => ((e.supplyBacking as Input).mintIndexRay = '0')],
    [
      'overflow-mint-index',
      (_, e) => ((e.supplyBacking as Input).mintIndexRay = (2n ** 128n).toString()),
    ],
    [
      'invalid-zero-position-previous-index',
      (_, e) => ((e.supplyBacking as Input).previousIndexRay = '1'),
    ],
    [
      'overflow-scaled-balance',
      (_, e) => ((e.supplyBacking as Input).scaledAfterUnits = (2n ** 120n).toString()),
    ],
    [
      'overflow-backing-product',
      (_, e) => ((e.supplyBacking as Input).normalizedIncomeRay = (2n ** 256n).toString()),
    ],
    [
      'backwards-income-index',
      (_, e) => ((e.supplyBacking as Input).normalizedIncomeRay = '1000000000000000000000000000'),
    ],
    [
      'backwards-previous-index',
      (_, e) => ((e.supplyBacking as Input).previousIndexRay = '1200000000000000000000000000'),
    ],
    ['rounded-up-mint', (_, e) => ((e.supplyBacking as Input).scaledMintedUnits = '91')],
    ['missing-scaled-mint', (_, e) => ((e.supplyBacking as Input).scaledMintedUnits = '0')],
    ['wrong-scaled-total', (_, e) => ((e.supplyBacking as Input).scaledAfterUnits = '91')],
    [
      'wrong-before-backing',
      (_, e) => ((e.supplyBacking as Input).suppliedBeforeAtMintUnits = '1'),
    ],
    ['fabricated-principal', (_, e) => ((e.supplyBacking as Input).backedPrincipalUnits = '100')],
    ['zero-principal', (_, e) => ((e.supplyBacking as Input).backedPrincipalUnits = '0')],
    [
      'wrong-balance-increase',
      (_, e) => ((e.supplyBacking as Input).mintBalanceIncreaseUnits = '1'),
    ],
    ['wrong-mint-value', (_, e) => ((e.supplyBacking as Input).mintValueUnits = '100')],
    ['wrong-post', (_, e) => ((e.supplyBacking as Input).suppliedAfterUnits = '100')],
    ['wrong-observed-post', (_, e) => (((e.position as Input).supplied as Input).units = '100')],
    ['arbitrary-loss', (_, e) => ((e.supplyBacking as Input).roundingDeltaUnits = '-2')],
    ['invented-income', (_, e) => ((e.supplyBacking as Input).inclusionIncomeUnits = '1')],
    ['noncanonical-units', (_, e) => ((e.supplyBacking as Input).scaledMintedUnits = '090')],
  ];
  for (const [name, mutate] of mutations) {
    const { p, effects } = floorSupplyFixture();
    mutate(p, effects);
    const reserved = reserveInventory(inventory(), name, p);
    const unresolved = settleInventory(reserved, record(name, p, effects));
    assert.ok(unresolved.inFlight[name]?.reason, name);
    assert.deepEqual(unresolved.available, reserved.available, name);
    assert.deepEqual(unresolved.costs, reserved.costs, name);
    assert.deepEqual(unresolved.conversions, reserved.conversions, name);
    assert.deepEqual(unresolved.settled, [], name);
  }
});
test('Aave existing scaled balance uses total floor difference, separates inclusion income and blocks unexplained prior loss', () => {
  const { p, effects } = floorSupplyFixture();
  p.binding.amountUnits = '101';
  Object.assign(p.binding.effect as Input, {
    suppliedBeforeUnits: '9',
    scaledSuppliedBeforeUnits: '9',
  });
  effects.actualAmountUnits = '101';
  Object.assign(effects.supplyBacking as Input, {
    underlyingUnits: '101',
    previousIndexRay: '1050000000000000000000000000',
    normalizedIncomeRay: '1200000000000000000000000000',
    scaledBeforeUnits: '9',
    scaledMintedUnits: '91',
    scaledAfterUnits: '100',
    suppliedBeforeAtMintUnits: '9',
    backedPrincipalUnits: '101',
    suppliedAfterUnits: '120',
    mintValueUnits: '101',
    roundingDeltaUnits: '0',
    inclusionIncomeUnits: '10',
  });
  ((effects.position as Input).supplied as Input).units = '120';
  const initial = inventory({
    [holdingKey('a', WETH)]: '101',
    [suppliedKey('a', WETH)]: '9',
  });
  const state = settle(initial, 'existing-floor', p, effects);
  assert.equal(state.available[suppliedKey('a', WETH)], '110');
  assert.equal(state.available[holdingKey('a', WETH)], '0');
  assert.equal(state.economicEffects[holdingKey('a', WETH)], '-101');
  const reserved = reserveInventory(
    inventory({ [holdingKey('a', WETH)]: '101', [suppliedKey('a', WETH)]: '10' }),
    'prior-loss',
    p,
  );
  const unresolved = settleInventory(reserved, record('prior-loss', p, effects));
  assert.equal(unresolved.inFlight['prior-loss']?.reason, 'INVENTORY_SUPPLY_BACKING');
});
test('Aave borrowing creates separately counted cash and debt; partial and full repayment prove interest without netting debt', () => {
  const supplied = settle(
    inventory(),
    'supply',
    preview('aave.supply', WETH, '100', {
      spender: POOL,
      suppliedBeforeUnits: '0',
      debtBeforeUnits: '0',
    }),
    {
      actualAmountUnits: '100',
      position: { supplied: { units: '100' }, variableDebt: { units: '0' } },
    },
  );
  const borrow = preview('aave.borrow', USDC, '20', {
    suppliedBeforeUnits: '0',
    debtBeforeUnits: '0',
  });
  const borrowed = settle(supplied, 'borrow', borrow, {
    actualAmountUnits: '20',
    position: { supplied: { units: '0' }, variableDebt: { units: '20' } },
  });
  assert.equal(borrowed.available[holdingKey('a', USDC)], '30');
  assert.equal(borrowed.debts[debtKey('a', USDC)], '20');
  assert.throws(() => assertInventoryRestorable(borrowed), { code: 'INVENTORY_DEBT' });
  const partial = preview('aave.repay', USDC, '10', {
    spender: POOL,
    suppliedBeforeUnits: '0',
    debtBeforeUnits: '22',
  });
  const repaidSome = settle(borrowed, 'repay-some', partial, {
    actualAmountUnits: '10',
    position: { supplied: { units: '0' }, variableDebt: { units: '12' } },
  });
  const all = preview('aave.repay', USDC, '13', {
    spender: POOL,
    suppliedBeforeUnits: '0',
    debtBeforeUnits: '12',
    repayAll: true,
  });
  const repaid = settle(repaidSome, 'repay-all', all, {
    actualAmountUnits: '12',
    position: { supplied: { units: '0' }, variableDebt: { units: '0' } },
  });
  assert.equal(repaid.available[holdingKey('a', USDC)], '8');
  assert.equal(repaid.debts[debtKey('a', USDC)], '0');
  assert.equal(repaid.costs[holdingKey('a', USDC)], '2');
});
test('unrelated collateral and debt cannot fund a live-test borrow or repayment', () => {
  const initial = inventory();
  assert.throws(
    () =>
      reserveInventory(
        initial,
        'borrow',
        preview('aave.borrow', USDC, '20', { suppliedBeforeUnits: '1000', debtBeforeUnits: '0' }),
      ),
    { code: 'INVENTORY_COLLATERAL' },
  );
  assert.throws(
    () =>
      reserveInventory(
        initial,
        'repay',
        preview('aave.repay', USDC, '10', {
          spender: POOL,
          suppliedBeforeUnits: '0',
          debtBeforeUnits: '1000',
        }),
      ),
    { code: 'INVENTORY_DEBT' },
  );
});
test('chain transfer to exchange remains in flight until exact usable Binance credit is correlated', () => {
  const p = preview('wallet.transfer', USDC, '10', { recipient: DEPOSIT });
  const pending = reserveInventory(inventory(), 'deposit', p);
  const r = record('deposit', p, { recipient: DEPOSIT, receivedUnits: '10' });
  const chainOnly = settleInventory(pending, r);
  assert.ok(chainOnly.inFlight.deposit);
  assert.equal(chainOnly.available[holdingKey('main', USDC)], undefined);
  const credited = settleInventory(chainOnly, {
    ...r,
    evidence: {
      ...r.evidence,
      depositCredit: {
        account: 'main',
        coin: 'USDC',
        transactionHash: HASH,
        amountUnits: '10',
        state: 'credited',
      },
    },
  });
  assert.equal(credited.available[holdingKey('main', USDC)], '10');
  assert.equal(Object.keys(credited.inFlight).length, 0);
});
test('Binance transfer consumes exact source token units and credits configured account without economic loss', () => {
  const p = preview(
    'binance.transfer',
    'native',
    '100',
    {},
    {
      authority: 'main',
      from: 'sub',
      to: 'main',
      params: {
        asset: 'ETH',
        amount: '0.0000000000000001',
        fromAccountType: 'SPOT',
        toAccountType: 'SPOT',
      },
    },
  );
  const out = settle(
    inventory(),
    'internal',
    p,
    {},
    {
      clientTranId: 'synthetic-binance-id',
      venueStatus: 'SUCCESS',
      amount: '0.0000000000000001',
      sourceDebitObserved: '0.0000000000000001',
      destinationCreditObserved: '0.0000000000000001',
    },
  );
  assert.equal(out.available[holdingKey('sub', 'native')], '400');
  assert.equal(out.available[holdingKey('main', 'native')], '1100');
  assert.equal(Object.keys(out.economicEffects).length, 0);
});
test('Binance withdrawal reconciles actual net receipt and fee for both advertised amount fee conventions', () => {
  for (const [actual, debit] of [
    ['90', '100'],
    ['100', '110'],
  ]) {
    const p = preview(
      'binance.withdraw',
      'native',
      '100',
      {},
      {
        account: 'main',
        destination: 'a',
        chainId: 1,
        maximumFee: '0.00000000000000002',
        params: {
          coin: 'ETH',
          amount: '0.0000000000000001',
          network: 'ETH',
          walletType: 0,
          address: A,
        },
      },
    );
    const out = settle(
      inventory(),
      'withdraw',
      p,
      {},
      {
        withdrawOrderId: 'synthetic-binance-id',
        venueState: 'completed',
        feeAmount: '0.00000000000000001',
        sourceDebitObserved: `0.000000000000000${debit}`,
        recipient: {
          receiptStatus: 'success',
          recipient: A,
          actualUnits: actual,
          confirmations: '2',
        },
      },
    );
    assert.equal(Object.keys(out.inFlight).length, 0);
    assert.equal(out.available[holdingKey('main', 'native')], (1000n - BigInt(debit!)).toString());
    assert.equal(out.available[holdingKey('a', 'native')], (1000n + BigInt(actual!)).toString());
    assert.equal(out.costs[holdingKey('main', 'native')], '10');
  }
});
function usdcVenuePreview(operation: 'binance.transfer' | 'binance.withdraw'): Preview {
  return preview(
    operation,
    USDC,
    '1000000',
    {},
    operation === 'binance.transfer'
      ? {
          authority: 'main',
          from: 'sub',
          to: 'main',
          params: { asset: 'USDC', amount: '1', fromAccountType: 'SPOT', toAccountType: 'SPOT' },
        }
      : {
          account: 'main',
          destination: 'a',
          chainId: 1,
          maximumFee: '0.000002',
          params: { coin: 'USDC', amount: '1', network: 'ETH', walletType: 0, address: A },
        },
  );
}
function usdcVenueInventory(): RunInventory {
  return inventory({ [holdingKey('main', USDC)]: '2000000', [holdingKey('sub', USDC)]: '2000000' });
}
function paddedTransferEvidence(): Input {
  return {
    clientTranId: 'synthetic-binance-id',
    venueStatus: 'SUCCESS',
    amount: '1.000000000000000000',
    sourceDebitObserved: '1.000000000000000000',
    destinationCreditObserved: '1.000000000000000000',
  };
}
function paddedWithdrawalEvidence(): Input {
  return {
    withdrawOrderId: 'synthetic-binance-id',
    venueState: 'completed',
    feeAmount: '0.000001000000000000',
    sourceDebitObserved: '1.000001000000000000',
    recipient: {
      receiptStatus: 'success',
      recipient: A,
      actualUnits: '1000000',
      confirmations: '2',
    },
  };
}
test('Binance padded observations settle exact atomic USDC movements and fees without loosening request precision', () => {
  const transfer = usdcVenuePreview('binance.transfer');
  const transferred = settle(
    usdcVenueInventory(),
    'padded-transfer',
    transfer,
    {},
    paddedTransferEvidence(),
  );
  assert.equal(transferred.available[holdingKey('sub', USDC)], '1000000');
  assert.equal(transferred.available[holdingKey('main', USDC)], '3000000');
  assert.equal(Object.keys(transferred.inFlight).length, 0);
  const withdrawal = usdcVenuePreview('binance.withdraw');
  const withdrawn = settle(
    usdcVenueInventory(),
    'padded-withdrawal',
    withdrawal,
    {},
    paddedWithdrawalEvidence(),
  );
  assert.equal(withdrawn.available[holdingKey('main', USDC)], '999999');
  assert.equal(withdrawn.available[holdingKey('a', USDC)], '1000010');
  assert.equal(withdrawn.costs[holdingKey('main', USDC)], '1');
  assert.equal(Object.keys(withdrawn.inFlight).length, 0);
  for (const amount of ['1.0000000', '1.0000001']) {
    const p = usdcVenuePreview('binance.transfer');
    (p.binding.params as Input).amount = amount;
    assert.throws(() => reserveInventory(usdcVenueInventory(), 'strict-request', p), {
      code: 'INVALID_PRECISION',
    });
  }
  withdrawal.binding.maximumFee = '0.0000020';
  assert.throws(() => reserveInventory(usdcVenueInventory(), 'strict-fee-cap', withdrawal), {
    code: 'INVALID_PRECISION',
  });
});
test('meaningful subatomic Binance transfer observations remain in flight instead of being rounded', () => {
  for (const field of ['amount', 'sourceDebitObserved', 'destinationCreditObserved']) {
    const p = usdcVenuePreview('binance.transfer');
    const reserved = reserveInventory(usdcVenueInventory(), 'subatomic-transfer', p);
    const evidence = paddedTransferEvidence();
    evidence[field] = '1.000000100000000000';
    const unresolved = settleInventory(
      reserved,
      record('subatomic-transfer', p, {}, 'completed', evidence),
    );
    assert.equal(unresolved.inFlight['subatomic-transfer']?.reason, 'INVALID_PRECISION');
    assert.deepEqual(unresolved.available, reserved.available);
    assert.deepEqual(unresolved.costs, reserved.costs);
    assert.deepEqual(unresolved.settled, []);
  }
});
test('subatomic actual venue fees/debits and padded fee overruns cannot release or increase the reserved cap', () => {
  for (const field of ['feeAmount', 'sourceDebitObserved', 'over-fee']) {
    const p = usdcVenuePreview('binance.withdraw');
    const reserved = reserveInventory(usdcVenueInventory(), 'subatomic-fee', p);
    const evidence = paddedWithdrawalEvidence();
    if (field === 'feeAmount') evidence.feeAmount = '0.000001100000000000';
    if (field === 'sourceDebitObserved') evidence.sourceDebitObserved = '1.000001100000000000';
    if (field === 'over-fee') {
      evidence.feeAmount = '0.000003000000000000';
      evidence.sourceDebitObserved = '1.000003000000000000';
    }
    const unresolved = settleInventory(
      reserved,
      record('subatomic-fee', p, {}, 'completed', evidence),
    );
    assert.equal(
      unresolved.inFlight['subatomic-fee']?.reason,
      field === 'over-fee' ? 'INVENTORY_FEE' : 'INVALID_PRECISION',
    );
    assert.equal(unresolved.inFlight['subatomic-fee']?.fee?.maximumUnits, '2');
    assert.deepEqual(unresolved.available, reserved.available);
    assert.deepEqual(unresolved.costs, reserved.costs);
    assert.deepEqual(unresolved.settled, []);
  }
});
test('cleanup mode denies new supply/borrow/external custody and permits scoped reverse operations', () => {
  const state = inventory();
  state.cleanupOutstanding = true;
  assert.throws(
    () =>
      reserveInventory(
        state,
        'supply',
        preview('aave.supply', WETH, '100', {
          spender: POOL,
          suppliedBeforeUnits: '0',
          debtBeforeUnits: '0',
        }),
      ),
    { code: 'INVENTORY_CLEANUP' },
  );
  const allowed = preview('wallet.transfer', 'native', '10', { recipient: B });
  assert.ok(reserveInventory(state, 'return', allowed).inFlight.return);
  const zero = preview('wallet.approve', WETH, '0', {
    spender: ROUTER,
    previousAllowanceUnits: '100',
  });
  assert.ok(reserveInventory(state, 'revoke', zero).inFlight.revoke);
});
test('closed swap and supply round trip exposes exact ETH/WETH execution loss plus supply income with zero intermediate exposure', () => {
  let state = inventory({ [holdingKey('a', WETH)]: '0', [holdingKey('a', USDC)]: '0' });
  state = settle(state, 'wrap', preview('wallet.wrap', WETH, '100', {}), {
    amountUnits: '100',
    representation: 'WETH',
  });
  state = settle(
    state,
    'swap-out',
    preview('uniswap.swap', WETH, '100', {
      tokenOut: USDC,
      outputDecimals: 6,
      minOutputUnits: '9',
      spender: ROUTER,
      recipient: A,
    }),
    { inputUnits: '100', outputUnits: '10', recipient: A },
  );
  state = settle(
    state,
    'swap-back',
    preview('uniswap.swap', USDC, '10', {
      tokenOut: WETH,
      outputDecimals: 18,
      minOutputUnits: '89',
      spender: ROUTER,
      recipient: A,
    }),
    { inputUnits: '10', outputUnits: '90', recipient: A },
  );
  state = settle(
    state,
    'supply',
    preview('aave.supply', WETH, '90', {
      spender: POOL,
      suppliedBeforeUnits: '0',
      debtBeforeUnits: '0',
    }),
    {
      actualAmountUnits: '90',
      position: { supplied: { units: '90' }, variableDebt: { units: '0' } },
    },
  );
  state = settle(
    state,
    'withdraw',
    preview('aave.withdraw', WETH, '92', {
      recipient: A,
      suppliedBeforeUnits: '92',
      debtBeforeUnits: '0',
    }),
    {
      actualAmountUnits: '92',
      position: { supplied: { units: '0' }, variableDebt: { units: '0' } },
    },
  );
  state = settle(state, 'unwrap', preview('wallet.unwrap', WETH, '92', {}), {
    amountUnits: '92',
    representation: 'native ETH',
  });
  assert.doesNotThrow(() => assertInventoryRestorable(state));
  assert.equal(state.available[holdingKey('a', WETH)], '0');
  assert.equal(state.available[holdingKey('a', USDC)], '0');
  assert.equal(state.costs[holdingKey('a', 'native')], '12');
  assert.equal(state.economicEffects[holdingKey('a', WETH)], '-8');
  assert.equal(state.available[holdingKey('a', 'native')], '980');
  assert.equal(
    BigInt(state.initial[holdingKey('a', 'native')]!) -
      BigInt(state.costs[holdingKey('a', 'native')]!) +
      BigInt(state.economicEffects[holdingKey('a', WETH)]!),
    980n,
  );
});
test('one-way swap cannot claim restoration even when both input and output existed in baseline', () => {
  const p = preview('uniswap.swap', WETH, '100', {
    tokenOut: USDC,
    outputDecimals: 6,
    minOutputUnits: '9',
    spender: ROUTER,
    recipient: A,
  });
  const state = settle(inventory(), 'one-way', p, {
    inputUnits: '100',
    outputUnits: '10',
    recipient: A,
  });
  assert.throws(() => assertInventoryRestorable(state), { code: 'INVENTORY_CONVERSION' });
  const small = preview('uniswap.swap', USDC, '1', {
    tokenOut: WETH,
    outputDecimals: 18,
    minOutputUnits: '8',
    spender: ROUTER,
    recipient: A,
  });
  const partiallyReturned = settle(state, 'tiny-reverse', small, {
    inputUnits: '1',
    outputUnits: '9',
    recipient: A,
  });
  assert.throws(() => assertInventoryRestorable(partiallyReturned), {
    code: 'INVENTORY_CONVERSION',
  });
  assert.equal(Object.values(partiallyReturned.conversions)[0]!.units, '9');
});
test('partial inverse swaps close only after all tagged proceeds return, and unrelated output balance is preserved', () => {
  let state = settle(
    inventory(),
    'out',
    preview('uniswap.swap', WETH, '100', {
      tokenOut: USDC,
      outputDecimals: 6,
      minOutputUnits: '9',
      spender: ROUTER,
      recipient: A,
    }),
    { inputUnits: '100', outputUnits: '10', recipient: A },
  );
  for (const [id, qty, output] of [
    ['part-one', '4', '36'],
    ['part-two', '6', '54'],
  ]) {
    state = settle(
      state,
      id!,
      preview('uniswap.swap', USDC, qty!, {
        tokenOut: WETH,
        outputDecimals: 18,
        minOutputUnits: (BigInt(output!) - 1n).toString(),
        spender: ROUTER,
        recipient: A,
      }),
      { inputUnits: qty, outputUnits: output, recipient: A },
    );
  }
  assert.equal(Object.keys(state.conversions).length, 0);
  assert.equal(state.available[holdingKey('a', USDC)], '10');
  assert.equal(state.available[holdingKey('a', WETH)], '90');
  assert.doesNotThrow(() => assertInventoryRestorable(state));
});
test('converted borrow-interest reserve can pay proved interest and remaining exact proceeds reverse without open conversion', () => {
  let state = inventory({ [holdingKey('a', WETH)]: '200', [holdingKey('a', USDC)]: '0' });
  state = settle(
    state,
    'supply',
    preview('aave.supply', WETH, '100', {
      spender: POOL,
      suppliedBeforeUnits: '0',
      debtBeforeUnits: '0',
    }),
    {
      actualAmountUnits: '100',
      position: { supplied: { units: '100' }, variableDebt: { units: '0' } },
    },
  );
  state = settle(
    state,
    'interest-reserve',
    preview('uniswap.swap', WETH, '100', {
      tokenOut: USDC,
      outputDecimals: 6,
      minOutputUnits: '9',
      spender: ROUTER,
      recipient: A,
    }),
    { inputUnits: '100', outputUnits: '10', recipient: A },
  );
  state = settle(
    state,
    'borrow',
    preview('aave.borrow', USDC, '20', { suppliedBeforeUnits: '0', debtBeforeUnits: '0' }),
    {
      actualAmountUnits: '20',
      position: { supplied: { units: '0' }, variableDebt: { units: '20' } },
    },
  );
  state = settle(
    state,
    'repay',
    preview('aave.repay', USDC, '22', {
      spender: POOL,
      suppliedBeforeUnits: '0',
      debtBeforeUnits: '22',
      repayAll: true,
    }),
    {
      actualAmountUnits: '22',
      position: { supplied: { units: '0' }, variableDebt: { units: '0' } },
    },
  );
  assert.equal(Object.values(state.conversions)[0]!.units, '8');
  assert.equal(state.costs[holdingKey('a', USDC)], '2');
  assert.deepEqual(state.fundedCosts, [
    {
      operationId: 'repay',
      originKey: holdingKey('a', WETH),
      originToken: WETH,
      currentKey: holdingKey('a', USDC),
      currentToken: USDC,
      units: '2',
      cause: 'borrow_interest',
    },
  ]);
  state = settle(
    state,
    'reverse',
    preview('uniswap.swap', USDC, '8', {
      tokenOut: WETH,
      outputDecimals: 18,
      minOutputUnits: '79',
      spender: ROUTER,
      recipient: A,
    }),
    { inputUnits: '8', outputUnits: '80', recipient: A },
  );
  assert.equal(Object.keys(state.conversions).length, 0);
  assert.equal(state.economicEffects[holdingKey('a', USDC)], '2');
  assert.equal(state.available[holdingKey('a', USDC)], '0');
});
test('supplying converted proceeds preserves their return identity including proved supply interest', () => {
  let state = settle(
    inventory({ [holdingKey('a', USDC)]: '0' }),
    'swap',
    preview('uniswap.swap', WETH, '100', {
      tokenOut: USDC,
      outputDecimals: 6,
      minOutputUnits: '9',
      spender: ROUTER,
      recipient: A,
    }),
    { inputUnits: '100', outputUnits: '10', recipient: A },
  );
  state = settle(
    state,
    'supply',
    preview('aave.supply', USDC, '10', {
      spender: POOL,
      suppliedBeforeUnits: '0',
      debtBeforeUnits: '0',
    }),
    {
      actualAmountUnits: '10',
      position: { supplied: { units: '10' }, variableDebt: { units: '0' } },
    },
  );
  state = settle(
    state,
    'withdraw',
    preview('aave.withdraw', USDC, '11', {
      recipient: A,
      suppliedBeforeUnits: '11',
      debtBeforeUnits: '0',
    }),
    {
      actualAmountUnits: '11',
      position: { supplied: { units: '0' }, variableDebt: { units: '0' } },
    },
  );
  assert.equal(
    Object.values(state.conversions).reduce((sum, c) => sum + BigInt(c.units), 0n),
    11n,
  );
  state = settle(
    state,
    'reverse',
    preview('uniswap.swap', USDC, '11', {
      tokenOut: WETH,
      outputDecimals: 18,
      minOutputUnits: '109',
      spender: ROUTER,
      recipient: A,
    }),
    { inputUnits: '11', outputUnits: '110', recipient: A },
  );
  assert.equal(Object.keys(state.conversions).length, 0);
  assert.equal(state.economicEffects[holdingKey('a', USDC)], '0');
  assert.equal(state.economicEffects[holdingKey('a', WETH)], '10');
  assert.doesNotThrow(() => assertInventoryRestorable(state));
});
test('cleanup permits the exact converted inverse through a zero-baseline WETH representation and blocks extra swap input', () => {
  let state = inventory({ [holdingKey('a', WETH)]: '0', [holdingKey('a', USDC)]: '10' });
  state = settle(state, 'cleanup-wrap', preview('wallet.wrap', WETH, '100', {}), {
    amountUnits: '100',
    representation: 'WETH',
  });
  state = settle(
    state,
    'cleanup-out',
    preview('uniswap.swap', WETH, '100', {
      tokenOut: USDC,
      outputDecimals: 6,
      minOutputUnits: '9',
      spender: ROUTER,
      recipient: A,
    }),
    { inputUnits: '100', outputUnits: '10', recipient: A },
  );
  state.cleanupOutstanding = true;
  const approve = preview('wallet.approve', USDC, '10', {
    spender: ROUTER,
    previousAllowanceUnits: '0',
  });
  state = settle(state, 'cleanup-approve', approve, { spender: ROUTER, allowanceUnits: '10' });
  const inverse = (quantity: string) =>
    preview('uniswap.swap', USDC, quantity, {
      tokenOut: WETH,
      outputDecimals: 18,
      minOutputUnits: '89',
      spender: ROUTER,
      recipient: A,
    });
  assert.throws(() => reserveInventory(state, 'enlarge-inverse', inverse('11')), {
    code: 'INVENTORY_CLEANUP',
  });
  state = settle(state, 'cleanup-inverse', inverse('10'), {
    inputUnits: '10',
    outputUnits: '90',
    recipient: A,
  });
  assert.equal(Object.keys(state.conversions).length, 0);
  state = settle(state, 'cleanup-unwrap', preview('wallet.unwrap', WETH, '90', {}), {
    amountUnits: '90',
    representation: 'native ETH',
  });
  assert.equal(state.available[holdingKey('a', WETH)], '0');
  assert.equal(state.available[holdingKey('a', USDC)], '10');
  assert.doesNotThrow(() => assertInventoryRestorable(state));
});
test('cleanup cannot start a new swap merely because its output already existed in baseline', () => {
  const state = inventory();
  state.cleanupOutstanding = true;
  const p = preview('uniswap.swap', USDC, '10', {
    tokenOut: WETH,
    outputDecimals: 18,
    minOutputUnits: '89',
    spender: ROUTER,
    recipient: A,
  });
  assert.throws(() => reserveInventory(state, 'new-cleanup-exposure', p), {
    code: 'INVENTORY_CLEANUP',
  });
  assert.equal(Object.keys(state.conversions).length, 0);
  assert.equal(Object.keys(state.inFlight).length, 0);
});
test('moved principal remains attributable when a recipient with its own gas allocation spends gas', () => {
  const out = preview('wallet.transfer', 'native', '100', { recipient: B });
  let state = settle(inventory({ [holdingKey('b', 'native')]: '100' }), 'custody-out', out, {
    recipient: B,
    receivedUnits: '100',
  });
  const reverted = preview(
    'wallet.transfer',
    'native',
    '1',
    { recipient: A },
    { wallet: 'b', account: B },
  );
  state = settleInventory(
    reserveInventory(state, 'recipient-own-gas', reverted),
    record('recipient-own-gas', reverted, {}, 'failed'),
  );
  assert.equal(Object.values(state.conversions)[0]!.units, '100');
  assert.deepEqual(state.fundedCosts, []);
  assert.throws(() => assertInventoryRestorable(state), { code: 'INVENTORY_CONVERSION' });
  const back = preview(
    'wallet.transfer',
    'native',
    '100',
    { recipient: A },
    { wallet: 'b', account: B },
  );
  state = settle(state, 'custody-return', back, { recipient: A, receivedUnits: '100' });
  assert.equal(Object.keys(state.conversions).length, 0);
  assert.equal(state.available[holdingKey('a', 'native')], '998');
  assert.equal(state.available[holdingKey('b', 'native')], '96');
  assert.doesNotThrow(() => assertInventoryRestorable(state));
});
test('a tiny custody transfer cannot be erased by larger gas paid from recipient own cash', () => {
  const p = preview('wallet.transfer', 'native', '1', { recipient: B });
  let state = settle(
    inventory({ [holdingKey('b', 'native')]: '100' }),
    'tiny-custody-out',
    p,
    { recipient: B, receivedUnits: '1' },
    { feeWei: '0' },
  );
  const failed = preview(
    'wallet.transfer',
    'native',
    '1',
    { recipient: A },
    { wallet: 'b', account: B },
  );
  state = settleInventory(
    reserveInventory(state, 'own-cash-gas', failed),
    record('own-cash-gas', failed, {}, 'failed'),
  );
  assert.equal(state.available[holdingKey('a', 'native')], '999');
  assert.equal(state.available[holdingKey('b', 'native')], '99');
  assert.equal(Object.values(state.conversions)[0]!.units, '1');
  assert.deepEqual(state.fundedCosts, []);
  assert.throws(() => assertInventoryRestorable(state), { code: 'INVENTORY_CONVERSION' });
});
test('verified recipient gas may consume transferred funding only after own cash is exhausted, with exact origin proof', () => {
  const initial = inventory({ [holdingKey('b', 'native')]: '0' });
  let state = settle(
    initial,
    'fund-recipient',
    preview('wallet.transfer', 'native', '100', { recipient: B }),
    { recipient: B, receivedUnits: '100' },
  );
  const failed = preview(
    'wallet.transfer',
    'native',
    '1',
    { recipient: A },
    { wallet: 'b', account: B },
  );
  state = settleInventory(
    reserveInventory(state, 'funded-revert', failed),
    record('funded-revert', failed, {}, 'failed'),
  );
  assert.equal(Object.values(state.conversions)[0]!.units, '98');
  assert.deepEqual(state.fundedCosts, [
    {
      operationId: 'funded-revert',
      originKey: holdingKey('a', 'native'),
      originToken: 'native',
      currentKey: holdingKey('b', 'native'),
      currentToken: 'native',
      units: '2',
      cause: 'gas',
    },
  ]);
  const back = preview(
    'wallet.transfer',
    'native',
    '96',
    { recipient: A },
    { wallet: 'b', account: B, gas: '1', maxFeePerGas: '2' },
  );
  const r = record('return-net-funding', back, { recipient: A, receivedUnits: '96' });
  state = settleInventory(reserveInventory(state, r.id, back), r);
  assert.equal(state.available[holdingKey('b', 'native')], '0');
  assert.equal(state.available[holdingKey('a', 'native')], '994');
  assert.equal(Object.keys(state.conversions).length, 0);
  assert.equal(state.fundedCosts.length, 2);
  assert.equal(state.fundedCosts[1]!.cause, 'gas');
  assert.equal(state.fundedCosts[1]!.units, '2');
  assert.deepEqual(settleInventory(state, r).fundedCosts, state.fundedCosts);
  assert.doesNotThrow(() => assertInventoryRestorable(state));
});
test('Binance account transfers and withdrawal preserve the original wallet custody through the full return route', () => {
  let state = inventory({
    [holdingKey('main', 'native')]: '0',
    [holdingKey('sub', 'native')]: '0',
  });
  const deposit = preview('wallet.transfer', 'native', '110', { recipient: DEPOSIT });
  state = settle(
    state,
    'custody-deposit',
    deposit,
    { recipient: DEPOSIT, receivedUnits: '110' },
    {
      depositCredit: {
        account: 'main',
        coin: 'ETH',
        transactionHash: HASH,
        amountUnits: '110',
        state: 'credited',
      },
    },
  );
  const transfer = (from: string, to: string) =>
    preview(
      'binance.transfer',
      'native',
      '110',
      {},
      {
        authority: 'main',
        from,
        to,
        params: {
          asset: 'ETH',
          amount: '0.00000000000000011',
          fromAccountType: 'SPOT',
          toAccountType: 'SPOT',
        },
      },
    );
  for (const [id, from, to] of [
    ['to-child', 'main', 'sub'],
    ['to-main', 'sub', 'main'],
  ]) {
    const p = transfer(from!, to!);
    state = settle(
      state,
      id!,
      p,
      {},
      {
        clientTranId: 'synthetic-binance-id',
        venueStatus: 'SUCCESS',
        amount: '0.00000000000000011',
        sourceDebitObserved: '0.00000000000000011',
        destinationCreditObserved: '0.00000000000000011',
      },
    );
    assert.equal(Object.values(state.conversions)[0]!.originKey, holdingKey('a', 'native'));
    assert.equal(Object.values(state.conversions)[0]!.currentKey, holdingKey(to!, 'native'));
  }
  assert.throws(() => assertInventoryRestorable(state), { code: 'INVENTORY_CONVERSION' });
  const withdraw = preview(
    'binance.withdraw',
    'native',
    '100',
    {},
    {
      account: 'main',
      destination: 'a',
      chainId: 1,
      maximumFee: '0.00000000000000001',
      params: {
        coin: 'ETH',
        amount: '0.0000000000000001',
        network: 'ETH',
        walletType: 0,
        address: A,
      },
    },
  );
  // Added fee: 100 units reach the original wallet and the final 10 prove an actual venue cost.
  state = settle(
    state,
    'custody-withdraw',
    withdraw,
    {},
    {
      withdrawOrderId: 'synthetic-binance-id',
      venueState: 'completed',
      feeAmount: '0.00000000000000001',
      sourceDebitObserved: '0.00000000000000011',
      recipient: { receiptStatus: 'success', recipient: A, actualUnits: '100', confirmations: '2' },
    },
  );
  assert.equal(Object.keys(state.inFlight).length, 0);
  assert.equal(Object.keys(state.conversions).length, 0);
  assert.equal(state.available[holdingKey('a', 'native')], '988');
  assert.deepEqual(state.fundedCosts, [
    {
      operationId: 'custody-withdraw',
      originKey: holdingKey('a', 'native'),
      originToken: 'native',
      currentKey: holdingKey('main', 'native'),
      currentToken: 'native',
      units: '10',
      cause: 'venue_fee',
    },
  ]);
  assert.doesNotThrow(() => assertInventoryRestorable(state));
});
test('verified exchange return routes bind the deposit asset and exact usable-credit minimum', () => {
  const state = createInventory(
    { ...scope, returnMinimums: { [DEPOSIT]: { native: '10' } } },
    { [holdingKey('a', 'native')]: '1000', [holdingKey('a', USDC)]: '100' },
  );
  assert.throws(
    () =>
      reserveInventory(
        state,
        'wrong-deposit-asset',
        preview('wallet.transfer', USDC, '10', { recipient: DEPOSIT }),
      ),
    { code: 'INVENTORY_ROUTE' },
  );
  assert.throws(
    () =>
      reserveInventory(
        state,
        'below-deposit-minimum',
        preview('wallet.transfer', 'native', '9', { recipient: DEPOSIT }),
      ),
    { code: 'INVENTORY_ROUTE' },
  );
  assert.ok(
    reserveInventory(
      state,
      'exact-deposit-minimum',
      preview('wallet.transfer', 'native', '10', { recipient: DEPOSIT }),
    ).inFlight['exact-deposit-minimum'],
  );
  const normalized = createInventory(
    { ...scope, returnMinimums: { [DEPOSIT]: { [WETH.toUpperCase().replace('0X', '0x')]: '1' } } },
    { [holdingKey('a', 'native')]: '100' },
  );
  assert.equal(normalized.scope.returnMinimums![DEPOSIT]![WETH], '1');
  for (const minimums of [
    {},
    { [DEPOSIT]: {} },
    { [DEPOSIT]: { native: '0' } },
    { [A]: { native: '1' } },
  ]) {
    assert.throws(
      () =>
        createInventory(
          { ...scope, returnMinimums: minimums },
          { [holdingKey('a', 'native')]: '100' },
        ),
      { code: 'INVENTORY_ROUTE' },
    );
  }
  assert.throws(
    () =>
      createInventory(
        { ...scope, returnMinimums: { [DEPOSIT]: { native: '1e18' } } },
        { [holdingKey('a', 'native')]: '100' },
      ),
    { code: 'INVENTORY_UNITS' },
  );
});
test('native and ERC20 self-transfers preserve principal and provenance and consume only actual gas', () => {
  let state = inventory();
  for (const token of ['native', WETH]) {
    const p = preview('wallet.transfer', token, '100', { recipient: A });
    state = settle(state, `self-${token}`, p, { recipient: A, receivedUnits: '100' });
    assert.equal(Object.keys(state.inFlight).length, 0);
    assert.equal(Object.keys(state.conversions).length, 0);
  }
  assert.equal(state.available[holdingKey('a', 'native')], '996');
  assert.equal(state.available[holdingKey('a', WETH)], '100');
  assert.deepEqual(state.economicEffects, {});
  assert.doesNotThrow(() => assertInventoryRestorable(state));
  state = settle(
    state,
    'tagged-funding',
    preview('wallet.transfer', 'native', '100', { recipient: B }),
    { recipient: B, receivedUnits: '100' },
  );
  const originalTags = structuredClone(state.conversions);
  const taggedSelf = preview(
    'wallet.transfer',
    'native',
    '50',
    { recipient: B },
    { wallet: 'b', account: B },
  );
  state = settle(state, 'tagged-self', taggedSelf, { recipient: B, receivedUnits: '50' });
  assert.deepEqual(
    state.conversions,
    originalTags,
    'A self-transfer cannot erase or invent custody provenance.',
  );
});
