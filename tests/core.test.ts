import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { z } from 'zod';
import { OperationStore } from '../src/core/store.js';
import { OperationEngine, type Authorizer } from '../src/core/engine.js';
import { BudgetGuard, TEST_LIMIT, type RunPlan, type TestRun } from '../src/core/budget.js';
import { canonical, exactUnits, fingerprint, observedUnits } from '../src/core/canonical.js';
import { FinError, publicError } from '../src/core/errors.js';
import { createInventory, holdingKey } from '../src/core/inventory.js';
import type { Adapter, Input, Preview } from '../src/core/types.js';
const syntheticSchema = z.object({ requestId: z.string().min(1), amount: z.string() }).strict();
function fixture(
  options: { denied?: boolean; lost?: boolean; changed?: boolean; alteredApproval?: boolean } = {},
) {
  const store = new OperationStore(':memory:');
  let submits = 0,
    approvals = 0,
    previews = 0;
  const preview = async (_op: string, input: any): Promise<Preview> => ({
    operation: 'fixture.move',
    request: input,
    binding: { ...input, recipient: options.changed && ++previews > 1 ? 'altered' : 'synthetic' },
    summary: 'Synthetic local move',
    checks: [
      { name: 'fixture', status: 'passed', detail: 'Synthetic isolated adapter; no network' },
    ],
    warnings: [],
    observedAt: new Date().toISOString(),
    expiresAt: new Date(Date.now() + 60000).toISOString(),
  });
  const adapter: Adapter = {
    definitions: [
      {
        name: 'fixture.move',
        description: 'Synthetic',
        kind: 'write',
        schema: syntheticSchema,
        examples: [],
        coverage: 'fixture only',
        completion: 'Synthetic receipt',
      },
    ],
    read: async () => {},
    preview,
    submit: async (_p, h) => {
      submits++;
      h.persistSubmission({ kind: 'ethereum', reference: 'synthetic-hash', data: { nonce: 0 } });
      if (options.lost) throw new Error('credential-bearing external error');
      return { kind: 'ethereum', reference: 'synthetic-hash', data: { nonce: 0 } };
    },
    reconcile: async () => ({ status: 'completed', evidence: { synthetic: true } }),
  };
  const auth: Authorizer = {
    approve: async (_p, hash) => {
      approvals++;
      if (options.denied)
        throw new FinError('APPROVAL_DECLINED', 'approval', 'Synthetic denial.', 'Inspect.');
      return {
        mechanism: 'isolated-test-only',
        bindingHash: options.alteredApproval ? 'invalid' : hash,
        approvedAt: new Date().toISOString(),
        expiresAt: new Date(Date.now() + 60000).toISOString(),
      };
    },
  };
  return {
    store,
    adapter,
    auth,
    engine: new OperationEngine(store, [adapter], auth),
    counts: () => ({ submits, approvals }),
  };
}
test('exact financial units reject float notation, signs, precision overflow and silent rounding', () => {
  assert.equal(exactUnits('0.000001', 6), 1n);
  assert.equal(exactUnits('100.1', 6), 100100000n);
  for (const s of ['1e2', '1.0000001', '-1', '+1', '01', 'NaN'])
    assert.throws(() => exactUnits(s, 6));
  assert.equal(canonical({ b: '0.1', a: 1 }), canonical({ a: 1, b: '0.1' }));
  assert.throws(() => canonical({ amount: 0.1 }));
});
test('observed units remove only insignificant fractional zeros while requested units remain strict', () => {
  assert.equal(observedUnits('0.000001000000000000', 6), 1n);
  assert.equal(observedUnits('1000.000000000000000000', 6), 1_000_000_000n);
  assert.equal(observedUnits('1.123456000000000000', 6), 1_123_456n);
  assert.equal(observedUnits('0.000000000000000000', 6), 0n);
  assert.equal(observedUnits('1.000000000000000000', 0), 1n);
  assert.equal(observedUnits('1000', 0), 1000n);
  for (const value of ['1.123456700000000000', '0.000000100000000000'])
    assert.throws(() => observedUnits(value, 6), { code: 'INVALID_PRECISION' });
  for (const value of [
    '1e0',
    '+1.0000',
    '-0.0000',
    '01.0000',
    '1.00.000',
    '.0000',
    '1.',
    ' 1.0000',
  ])
    assert.throws(() => observedUnits(value, 6), { code: 'INVALID_AMOUNT' });
  assert.throws(() => observedUnits(1.5 as unknown as string, 6), { code: 'INVALID_AMOUNT' });
  for (const decimals of [-1, 256, 1.5])
    assert.throws(() => observedUnits('1.0000', decimals), { code: 'INVALID_AMOUNT' });
  assert.throws(() => exactUnits('1.0000000', 6), { code: 'INVALID_PRECISION' });
  assert.throws(() => exactUnits('1.0000', 0), { code: 'INVALID_PRECISION' });
});
test('preview has no financial submission or approval', async () => {
  const f = fixture();
  await f.engine.preview('fixture.move', { requestId: 'preview', amount: '1' });
  assert.deepEqual(f.counts(), { submits: 0, approvals: 0 });
});
test('duplicate concurrent request submits and authorizes once; new identity requires fresh approval', async () => {
  const f = fixture();
  const input = { requestId: 'same', amount: '1' };
  const [a, b] = await Promise.all([
    f.engine.execute('fixture.move', input),
    f.engine.execute('fixture.move', input),
  ]);
  assert.equal(a.id, b.id);
  assert.equal(a.status, 'completed');
  assert.deepEqual(f.counts(), { submits: 1, approvals: 1 });
  await f.engine.execute('fixture.move', { ...input, requestId: 'deliberate-new' });
  assert.deepEqual(f.counts(), { submits: 2, approvals: 2 });
  await assert.rejects(() => f.engine.execute('fixture.move', { ...input, amount: '2' }), {
    code: 'REQUEST_ID_CONFLICT',
  });
});
test('decline, altered approval, and changed actual request prevent submission', async () => {
  for (const options of [{ denied: true }, { alteredApproval: true }, { changed: true }]) {
    const f = fixture(options);
    const record = await f.engine.execute('fixture.move', { requestId: 'denied', amount: '1' });
    assert.notEqual(record.status, 'completed');
    assert.equal(f.counts().submits, 0);
  }
});
test('accepted response loss is unknown then reconciles same identity, without replay', async () => {
  const f = fixture({ lost: true });
  const input = { requestId: 'lost', amount: '1' };
  const record = await f.engine.execute('fixture.move', input);
  assert.equal(record.status, 'unknown');
  assert.equal(record.submission?.reference, 'synthetic-hash');
  const retry = await f.engine.execute('fixture.move', input);
  assert.equal(retry.status, 'completed');
  assert.equal(retry.id, record.id);
  assert.equal(f.counts().submits, 1);
  assert.equal(JSON.stringify(record).includes('credential-bearing'), false);
});
test('durable operation persists before submission and survives crash/restart', () => {
  const dir = mkdtempSync(join(tmpdir(), 'fin-synthetic-'));
  try {
    let store = new OperationStore(join(dir, 'ops.sqlite'));
    const { record } = store.create(
      'fixture.move',
      { requestId: 'restart', amount: '1' },
      'restart',
    );
    record.status = 'submitted';
    record.submission = { kind: 'ethereum', reference: 'synthetic', data: { nonce: 1 } };
    store.save(record);
    store.close();
    store = new OperationStore(join(dir, 'ops.sqlite'));
    store.recoverInterrupted();
    assert.equal(store.get(record.id)?.status, 'unknown');
    assert.equal(store.byRequest('restart')?.submission?.reference, 'synthetic');
    assert.equal(
      store.create('fixture.move', { requestId: 'restart', amount: '1' }, 'restart').created,
      false,
    );
    store.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
test('cross-process store uniqueness rejects repeated identity with new payload', () => {
  const dir = mkdtempSync(join(tmpdir(), 'fin-synthetic-'));
  try {
    const a = new OperationStore(join(dir, 'ops.sqlite')),
      b = new OperationStore(join(dir, 'ops.sqlite'));
    const record = a.create('move', { amount: '1' }, 'same');
    assert.equal(b.create('move', { amount: '1' }, 'same').record.id, record.record.id);
    assert.throws(() => b.create('move', { amount: '2' }, 'same'), { code: 'REQUEST_ID_CONFLICT' });
    a.close();
    b.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
function plan(): RunPlan {
  return {
    id: 'synthetic-run',
    initialAllocationUsdMicros: '50000000',
    borrowReserveUsdMicros: '20000000',
    costReserveUsdMicros: '30000000',
    unwind: ['Repay all debt, withdraw collateral, return assets and restore approvals.'],
    returnRoutesVerified: true,
    baseline: {
      observedAt: new Date().toISOString(),
      quantities: { 'wallet/ETH': '100', 'aave/debt/USDC': '0', 'token/allowance/Aave': '0' },
      pending: [],
      metadata: { environment: 'synthetic' },
    },
  };
}
function valued(movement: string, cost: string, borrow = '0'): Preview {
  return {
    operation: 'synthetic',
    request: {},
    binding: {},
    summary: 'synthetic',
    checks: [],
    warnings: [],
    observedAt: new Date().toISOString(),
    expiresAt: new Date(Date.now() + 60000).toISOString(),
    valuation: {
      movementUsdMicros: movement,
      maximumFeeUsdMicros: cost,
      additionalAllocationUsdMicros: borrow,
      observedAt: new Date().toISOString(),
      source: 'synthetic oracle',
    },
  };
}
test('run allocation includes borrowed proceeds, costs, gas and unwind; over $100 is refused', () => {
  const store = new OperationStore(':memory:'),
    budget = new BudgetGuard(store);
  assert.equal(TEST_LIMIT, 100000000n);
  assert.throws(() => budget.begin({ ...plan(), initialAllocationUsdMicros: '50000001' }), {
    code: 'ALLOCATION_LIMIT',
  });
  budget.begin(plan());
  assert.throws(() => budget.check(valued('100000000', '1'), plan().id, 'over'), {
    code: 'MOVEMENT_LIMIT',
  });
  assert.throws(() => budget.check(valued('70000001', '0'), plan().id, 'unallocated'), {
    code: 'UNALLOCATED_FUNDS',
  });
  assert.throws(() => budget.check(valued('10000000', '30000001'), plan().id, 'fees'), {
    code: 'UNWIND_RESERVE',
  });
  assert.throws(() => budget.check(valued('10000000', '0', '20000001'), plan().id, 'borrow'), {
    code: 'BORROW_ALLOCATION',
  });
  budget.check(valued('10000000', '1000000'), plan().id, 'first', true);
  budget.check(valued('10000000', '1000000'), plan().id, 'first', true);
  assert.equal(store.getRun<any>(plan().id).feeReservations.first, '1000000');
});
test('unknown and stale valuation fail closed, even for cleanup', () => {
  const store = new OperationStore(':memory:'),
    budget = new BudgetGuard(store);
  budget.begin(plan());
  const p = valued('1', '1');
  delete p.valuation;
  assert.throws(() => budget.check(p, plan().id, 'x'), { code: 'VALUATION_UNAVAILABLE' });
  p.valuation = valued('1', '1').valuation!;
  p.valuation.observedAt = '2020-01-01T00:00:00Z';
  assert.throws(() => budget.check(p, plan().id, 'x'), { code: 'VALUATION_UNAVAILABLE' });
});
test('restoration cannot pass with debt, allowance, pending outcome or unexplained dust', () => {
  for (const quantities of [
    { 'wallet/ETH': '99', 'aave/debt/USDC': '1', 'token/allowance/Aave': '0' },
    { 'wallet/ETH': '99', 'aave/debt/USDC': '0', 'token/allowance/Aave': '1' },
    { 'wallet/ETH': '98', 'aave/debt/USDC': '0', 'token/allowance/Aave': '0' },
  ]) {
    const store = new OperationStore(':memory:'),
      budget = new BudgetGuard(store);
    budget.begin(plan());
    const run = budget.finish(
      plan().id,
      { ...plan().baseline, quantities },
      { 'wallet/ETH': '1' },
      [],
    );
    assert.equal(run.status, 'cleanup_outstanding');
    assert.throws(() => budget.begin({ ...plan(), id: 'extra' }), { code: 'CLEANUP_OUTSTANDING' });
  }
});
test('fully reconciled asset costs with baseline debt and allowances permits restoration', () => {
  const store = new OperationStore(':memory:'),
    budget = new BudgetGuard(store);
  budget.begin(plan());
  const run = budget.finish(
    plan().id,
    { ...plan().baseline, quantities: { ...plan().baseline.quantities, 'wallet/ETH': '99' } },
    { 'wallet/ETH': '1' },
    [],
  );
  assert.equal(run.status, 'restored');
});
test('arbitrary external exceptions never leak signed request or credentials', () => {
  const e = publicError(new Error('API_KEY=synthetic-secret'));
  assert.equal(JSON.stringify(e).includes('synthetic-secret'), false);
});

test('reconciliation serializes with another write before its approval and submission', async () => {
  const f = fixture({ lost: true });
  const first = await f.engine.execute('fixture.move', { requestId: 'first-pending', amount: '1' });
  let release!: () => void, entered!: () => void;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const started = new Promise<void>((resolve) => {
    entered = resolve;
  });
  f.adapter.reconcile = async () => {
    entered();
    await gate;
    return { status: 'completed', evidence: { synthetic: true } };
  };
  const reconciliation = f.engine.status(first.id, true);
  await started;
  const second = f.engine.execute('fixture.move', {
    requestId: 'second-after-reconcile',
    amount: '1',
  });
  await new Promise<void>((resolve) => setImmediate(resolve));
  assert.deepEqual(f.counts(), { submits: 1, approvals: 1 });
  release();
  assert.equal((await reconciliation).status, 'completed');
  await second;
  assert.deepEqual(f.counts(), { submits: 2, approvals: 2 });
});

test('approval expiry during adapter preflight prevents financial dispatch', async () => {
  const f = fixture();
  let clock = Date.now(),
    dispatches = 0;
  f.adapter.submit = async (_p, h) => {
    clock += 120000;
    h.persistSubmission({ kind: 'ethereum', reference: 'never-dispatched', data: {} });
    dispatches++;
    return { kind: 'ethereum', reference: 'never-dispatched', data: {} };
  };
  const engine = new OperationEngine(f.store, [f.adapter], f.auth, { now: () => clock });
  const outcome = await engine.execute('fixture.move', {
    requestId: 'expiry-before-dispatch',
    amount: '1',
  });
  assert.equal(outcome.status, 'expired');
  assert.equal(outcome.submission, undefined);
  assert.equal(dispatches, 0);
});

test('invalid approval timestamps cannot authorize a write', async () => {
  const f = fixture();
  const invalid: Authorizer = {
    approve: async (_p, hash) => ({
      mechanism: 'isolated-test-only',
      bindingHash: hash,
      approvedAt: 'invalid',
      expiresAt: 'invalid',
    }),
  };
  const outcome = await new OperationEngine(f.store, [f.adapter], invalid).execute('fixture.move', {
    requestId: 'invalid-time',
    amount: '1',
  });
  assert.equal(outcome.status, 'failed');
  assert.equal(outcome.error?.code, 'INVALID_APPROVAL');
  assert.equal(f.counts().submits, 0);
});

test('restart refunds reserved but unsubmitted approval; uncertain dispatched identity stays reserved', () => {
  for (const submitted of [false, true]) {
    const dir = mkdtempSync(join(tmpdir(), 'fin-synthetic-recovery-'));
    try {
      const path = join(dir, 'operations.sqlite');
      let store = new OperationStore(path);
      const A = `0x${'11'.repeat(20)}`,
        B = `0x${'22'.repeat(20)}`;
      const inventory = createInventory(
        {
          walletAddresses: { a: A, b: B },
          accounts: [],
          returnDeposits: {},
          tokens: ['native'],
          spenders: [],
        },
        { [holdingKey('a', 'native')]: '1000', [holdingKey('b', 'native')]: '20' },
      );
      const run = new BudgetGuard(store).begin({ ...plan(), inventory });
      const p = {
        ...valued('100', '20'),
        operation: 'wallet.transfer',
        request: { requestId: 'interrupted-reservation' },
        binding: {
          wallet: 'a',
          account: A,
          chainId: 1,
          token: 'native',
          decimals: 18,
          amountUnits: '100',
          gas: '10',
          maxFeePerGas: '2',
          effect: { recipient: B },
        },
      };
      const { record } = store.create(p.operation, p.request, String(p.request.requestId), run.id);
      record.preview = p;
      store.save(record);
      new BudgetGuard(store).check(p, run.id, record.id, true);
      if (submitted) {
        record.status = 'submitted';
        record.submission = {
          kind: 'ethereum',
          reference: 'synthetic-uncertain-identity',
          data: {},
        };
        store.save(record);
      }
      store.close();
      store = new OperationStore(path);
      const engine = new OperationEngine(store, [], {
        approve: async () => {
          throw new Error('No test approval is permitted.');
        },
      });
      engine.recoverInterrupted();
      const recovered = store.getRun<any>(run.id);
      assert.equal(store.get(record.id)?.status, submitted ? 'unknown' : 'cancelled');
      assert.equal(
        recovered.inventory.available[holdingKey('a', 'native')],
        submitted ? '880' : '1000',
      );
      assert.equal(Object.keys(recovered.inventory.inFlight).length, submitted ? 1 : 0);
      assert.equal(Object.keys(recovered.feeReservations).length, submitted ? 1 : 0);
      assert.equal(
        store.create(p.operation, p.request, String(p.request.requestId), run.id).created,
        false,
      );
      store.close();
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  }
});

test('restart settles a durable completed receipt when crash preceded the inventory save', () => {
  const store = new OperationStore(':memory:');
  const A = `0x${'11'.repeat(20)}`,
    B = `0x${'22'.repeat(20)}`,
    hash = `0x${'33'.repeat(32)}`,
    block = `0x${'44'.repeat(32)}`;
  const inventory = createInventory(
    {
      walletAddresses: { a: A, b: B },
      accounts: [],
      returnDeposits: {},
      tokens: ['native'],
      spenders: [],
      confirmations: 3,
    },
    { [holdingKey('a', 'native')]: '1000', [holdingKey('b', 'native')]: '20' },
  );
  const run = new BudgetGuard(store).begin({ ...plan(), inventory });
  const p = {
    ...valued('100', '20'),
    operation: 'wallet.transfer',
    request: { requestId: 'completed-crash-window' },
    binding: {
      wallet: 'a',
      account: A,
      chainId: 1,
      token: 'native',
      decimals: 18,
      amountUnits: '100',
      gas: '10',
      maxFeePerGas: '2',
      effect: { recipient: B },
    },
  };
  const { record } = store.create(p.operation, p.request, String(p.request.requestId), run.id);
  record.preview = p;
  store.save(record);
  new BudgetGuard(store).check(p, run.id, record.id, true);
  record.status = 'completed';
  record.submission = { kind: 'ethereum', reference: hash, data: {} };
  record.evidence = {
    receiptStatus: 'success',
    transactionHash: hash,
    blockHash: block,
    confirmations: '3',
    feeWei: '2',
    effects: { recipient: B, receivedUnits: '100' },
  };
  store.save(record);
  const engine = new OperationEngine(store, [], {
    approve: async () => {
      throw new Error('No new authorization allowed.');
    },
  });
  engine.recoverInterrupted();
  const recovered = store.getRun<any>(run.id);
  assert.equal(Object.keys(recovered.inventory.inFlight).length, 0);
  assert.equal(recovered.inventory.available[holdingKey('a', 'native')], '898');
  assert.equal(recovered.inventory.available[holdingKey('b', 'native')], '120');
  assert.deepEqual(recovered.inventory.settled, [record.id]);
  assert.equal(recovered.feeReservations[record.id], '2');
  engine.recoverInterrupted();
  assert.deepEqual(store.getRun<any>(run.id), recovered);
  store.close();
});

function feeSettlementFixture(path = ':memory:', feeUsd = '30000000', maximumFeeUnits = '200') {
  const store = new OperationStore(path),
    budget = new BudgetGuard(store);
  const A = `0x${'11'.repeat(20)}`,
    B = `0x${'22'.repeat(20)}`,
    hash = `0x${'33'.repeat(32)}`;
  const inventory = createInventory(
    {
      walletAddresses: { a: A, b: B },
      accounts: [],
      returnDeposits: {},
      tokens: ['native'],
      spenders: [],
      confirmations: 3,
    },
    { [holdingKey('a', 'native')]: '1000', [holdingKey('b', 'native')]: '20' },
  );
  const run = budget.begin({ ...plan(), inventory });
  const preview: Preview = {
    ...valued('100', feeUsd),
    operation: 'wallet.transfer',
    request: { requestId: 'synthetic-fee-settlement' },
    binding: {
      wallet: 'a',
      account: A,
      chainId: 1,
      token: 'native',
      decimals: 18,
      amountUnits: '100',
      gas: maximumFeeUnits,
      maxFeePerGas: '1',
      effect: { recipient: B },
    },
  };
  const { record } = store.create(
    preview.operation,
    preview.request,
    String(preview.request.requestId),
    run.id,
  );
  record.preview = preview;
  store.save(record);
  budget.check(preview, run.id, record.id, true);
  record.status = 'completed';
  record.submission = { kind: 'ethereum', reference: hash, data: {} };
  record.evidence = {
    receiptStatus: 'success',
    transactionHash: hash,
    blockHash: `0x${'44'.repeat(32)}`,
    confirmations: '3',
    feeWei: '2',
    effects: { recipient: B, receivedUnits: '100' },
  };
  return { store, budget, run, preview, record };
}

test('confirmed low gas keeps consumed USD costs and frees only unused fee capacity for cleanup', () => {
  const f = feeSettlementFixture();
  try {
    f.budget.settle(f.record);
    const settled = f.store.getRun<TestRun>(f.run.id)!;
    assert.equal(settled.inventory!.available[holdingKey('a', 'native')], '898');
    assert.equal(settled.inventory!.costs[holdingKey('a', 'native')], '2');
    assert.equal(settled.feeReservations[f.record.id], '300000');
    const cleanup = { ...f.preview, valuation: valued('100', '29700000').valuation };
    assert.doesNotThrow(() => f.budget.check(cleanup, f.run.id, 'synthetic-cleanup'));
    cleanup.valuation!.maximumFeeUsdMicros = '29700001';
    assert.throws(() => f.budget.check(cleanup, f.run.id, 'synthetic-cleanup'), {
      code: 'UNWIND_RESERVE',
    });
  } finally {
    f.store.close();
  }
});

test('pending, submitted and unknown outcomes cannot release maximum fee reservations', () => {
  for (const status of ['submitted', 'pending', 'unknown'] as const) {
    const f = feeSettlementFixture();
    try {
      f.record.status = status;
      f.record.evidence!.feeWei = '0'; // A raw low fee is not settlement proof.
      f.budget.settle(f.record);
      const unresolved = f.store.getRun<TestRun>(f.run.id)!;
      assert.equal(unresolved.feeReservations[f.record.id], '30000000');
      assert.equal(unresolved.inventory!.available[holdingKey('a', 'native')], '700');
      assert.ok(unresolved.inventory!.inFlight[f.record.id]);
      assert.deepEqual(unresolved.inventory!.costs, {});
    } finally {
      f.store.close();
    }
  }
});

test('confirmed revert retains actual spent gas while refunding principal and unused fees', () => {
  const f = feeSettlementFixture();
  try {
    f.record.status = 'failed';
    f.record.evidence!.receiptStatus = 'reverted';
    f.record.evidence!.feeWei = '3';
    delete f.record.evidence!.effects;
    f.budget.settle(f.record);
    const reverted = f.store.getRun<TestRun>(f.run.id)!;
    assert.equal(reverted.inventory!.available[holdingKey('a', 'native')], '997');
    assert.equal(reverted.inventory!.available[holdingKey('b', 'native')], '20');
    assert.equal(reverted.inventory!.costs[holdingKey('a', 'native')], '3');
    assert.equal(reverted.feeReservations[f.record.id], '450000');
    f.budget.settle(f.record);
    assert.deepEqual(f.store.getRun<TestRun>(f.run.id), reverted);
  } finally {
    f.store.close();
  }
});

test('mismatched effect, identity or fee evidence retains original maximum reservations', () => {
  for (const change of ['effect', 'hash', 'over-fee', 'unsubmitted-binding']) {
    const f = feeSettlementFixture();
    try {
      if (change === 'effect') (f.record.evidence!.effects as Input).receivedUnits = '99';
      if (change === 'hash') f.record.evidence!.transactionHash = `0x${'99'.repeat(32)}`;
      if (change === 'over-fee') f.record.evidence!.feeWei = '201';
      if (change === 'unsubmitted-binding') {
        f.record.status = 'failed';
        delete f.record.submission;
        f.record.preview!.binding.amountUnits = '99';
      }
      f.budget.settle(f.record);
      const unresolved = f.store.getRun<TestRun>(f.run.id)!;
      assert.equal(unresolved.feeReservations[f.record.id], '30000000');
      assert.equal(unresolved.inventory!.available[holdingKey('a', 'native')], '700');
      assert.ok(unresolved.inventory!.inFlight[f.record.id]?.reason);
      assert.deepEqual(unresolved.inventory!.costs, {});
    } finally {
      f.store.close();
    }
  }
});

test('settled USD costs round upwards and zero-fee proofs do not divide by zero', () => {
  for (const [maximum, consumed, reservedUsd, expectedUsd] of [
    ['200', '3', '101', '2'],
    ['0', '0', '0', '0'],
  ]) {
    const f = feeSettlementFixture(':memory:', reservedUsd, maximum);
    try {
      f.record.evidence!.feeWei = consumed;
      f.budget.settle(f.record);
      const settled = f.store.getRun<TestRun>(f.run.id)!;
      assert.equal(settled.feeReservations[f.record.id], expectedUsd);
      assert.deepEqual(settled.inventory!.settled, [f.record.id]);
    } finally {
      f.store.close();
    }
  }
});

test('venue settlement uses proven withdrawal fees and zero fees for internal account transfers', () => {
  for (const operation of ['binance.withdraw', 'binance.transfer']) {
    const store = new OperationStore(':memory:'),
      budget = new BudgetGuard(store);
    try {
      const recipient = `0x${'22'.repeat(20)}`;
      const inventory = createInventory(
        {
          walletAddresses: { b: recipient },
          accounts: ['main', 'sub'],
          returnDeposits: {},
          tokens: ['native'],
          spenders: [],
          confirmations: 3,
        },
        {
          [holdingKey('main', 'native')]: '1000',
          [holdingKey('sub', 'native')]: '20',
          [holdingKey('b', 'native')]: '20',
        },
      );
      const run = budget.begin({ ...plan(), inventory });
      const preview: Preview = {
        ...valued('100', '30000000'),
        operation,
        request: { requestId: `synthetic-fee-${operation}` },
        binding:
          operation === 'binance.withdraw'
            ? {
                account: 'main',
                destination: 'b',
                chainId: 1,
                maximumFee: '0.00000000000000002',
                params: {
                  coin: 'ETH',
                  network: 'ETH',
                  walletType: 0,
                  address: recipient,
                  amount: '0.0000000000000001',
                },
              }
            : {
                from: 'main',
                to: 'sub',
                authority: 'main',
                params: {
                  asset: 'ETH',
                  amount: '0.0000000000000001',
                  fromAccountType: 'SPOT',
                  toAccountType: 'SPOT',
                },
              },
      };
      const { record } = store.create(
        operation,
        preview.request,
        String(preview.request.requestId),
        run.id,
      );
      record.preview = preview;
      store.save(record);
      budget.check(preview, run.id, record.id, true);
      record.status = 'completed';
      record.submission = { kind: 'binance', reference: 'synthetic-venue-identity', data: {} };
      record.evidence =
        operation === 'binance.withdraw'
          ? {
              withdrawOrderId: 'synthetic-venue-identity',
              venueState: 'completed',
              feeAmount: '0.0000000000000000100000',
              sourceDebitObserved: '0.0000000000000001000000',
              recipient: {
                actualUnits: '90',
                recipient,
                receiptStatus: 'success',
                confirmations: '3',
              },
            }
          : {
              clientTranId: 'synthetic-venue-identity',
              venueStatus: 'SUCCESS',
              amount: '0.0000000000000001000000',
              sourceDebitObserved: '0.0000000000000001000000',
              destinationCreditObserved: '0.0000000000000001000000',
            };
      budget.settle(record);
      const settled = store.getRun<TestRun>(run.id)!;
      assert.deepEqual(settled.inventory!.settled, [record.id]);
      assert.equal(
        settled.feeReservations[record.id],
        operation === 'binance.withdraw' ? '15000000' : '0',
      );
      assert.equal(
        settled.inventory!.costs[holdingKey('main', 'native')] ?? '0',
        operation === 'binance.withdraw' ? '10' : '0',
      );
      assert.equal(settled.inventory!.available[holdingKey('main', 'native')], '900');
    } finally {
      store.close();
    }
  }
});

test('completed receipt recovery atomically persists inventory and consumed USD fees across restarts', () => {
  const dir = mkdtempSync(join(tmpdir(), 'fin-synthetic-fee-recovery-'));
  let store: OperationStore | undefined;
  try {
    const path = join(dir, 'operations.sqlite');
    const f = feeSettlementFixture(path);
    store = f.store;
    store.save(f.record); // Crash after completed receipt, before BudgetGuard.settle.
    store.close();
    store = undefined;
    const noApproval: Authorizer = {
      approve: async () => {
        throw new Error('Recovery cannot authorize another write.');
      },
    };
    store = new OperationStore(path);
    new OperationEngine(store, [], noApproval).recoverInterrupted();
    const recovered = store.getRun<TestRun>(f.run.id)!;
    assert.equal(recovered.inventory!.available[holdingKey('a', 'native')], '898');
    assert.equal(recovered.feeReservations[f.record.id], '300000');
    assert.deepEqual(recovered.inventory!.settled, [f.record.id]);
    store.close();
    store = new OperationStore(path);
    const restarted = new OperationEngine(store, [], noApproval);
    restarted.recoverInterrupted();
    restarted.recoverInterrupted();
    assert.deepEqual(store.getRun<TestRun>(f.run.id), recovered);
  } finally {
    store?.close();
    rmSync(dir, { recursive: true, force: true });
  }
});
