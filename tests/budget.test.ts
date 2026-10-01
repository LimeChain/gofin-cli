import test from 'node:test';
import assert from 'node:assert/strict';
import { BudgetGuard, type FinancialSnapshot, type TestRun } from '../src/core/budget.js';
import { createInventory, holdingKey } from '../src/core/inventory.js';
import { OperationStore } from '../src/core/store.js';
import type { Input, Preview, RecordOperation } from '../src/core/types.js';

const USDC = '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48';
const WALLET = `0x${'11'.repeat(20)}`;
type VenueOperation = 'binance.transfer' | 'binance.withdraw';

/** Isolated exact-unit ledger fixtures. No clients, credentials, signers or transport. */
function fixture(operation: VenueOperation, withInventory = true) {
  const store = new OperationStore(':memory:');
  const budget = new BudgetGuard(store);
  const source = holdingKey('main', USDC);
  const child = holdingKey('sub', USDC);
  const residual = `main/residual/${USDC}`;
  const initial = { [source]: '2000000', [child]: '2000000' };
  const baseline: FinancialSnapshot = {
    observedAt: new Date().toISOString(),
    quantities: { ...initial, [residual]: '100000000000' },
    pending: [],
    metadata: {},
  };
  const run = budget.begin({
    id: 'synthetic-failed-venue-run',
    initialAllocationUsdMicros: '3000000',
    borrowReserveUsdMicros: '0',
    costReserveUsdMicros: '1000000',
    unwind: ['Return all selected account allocation; a rejected venue movement has no debit.'],
    returnRoutesVerified: true,
    baseline,
    ...(withInventory
      ? {
          inventory: createInventory(
            {
              walletAddresses: { wallet: WALLET },
              accounts: ['main', 'sub'],
              returnDeposits: {},
              tokens: ['native', USDC],
              spenders: [],
            },
            initial,
          ),
        }
      : {}),
  });
  const reference = `synthetic-${operation}-identity`;
  const request = { requestId: reference };
  const preview: Preview = {
    operation,
    request,
    binding:
      operation === 'binance.transfer'
        ? {
            authority: 'main',
            from: 'sub',
            to: 'main',
            params: {
              asset: 'USDC',
              amount: '1',
              fromAccountType: 'SPOT',
              toAccountType: 'SPOT',
              clientTranId: reference,
            },
          }
        : {
            account: 'main',
            destination: 'wallet',
            chainId: 1,
            maximumFee: '0.1',
            params: {
              coin: 'USDC',
              amount: '1',
              network: 'ETH',
              walletType: 0,
              address: WALLET,
              withdrawOrderId: reference,
            },
          },
    summary: 'Synthetic definitive Binance failure; no venue request is made.',
    observedAt: baseline.observedAt,
    expiresAt: new Date(Date.now() + 60000).toISOString(),
    checks: [],
    warnings: [],
    valuation: {
      movementUsdMicros: '1000000',
      maximumFeeUsdMicros: operation === 'binance.transfer' ? '0' : '100000',
      observedAt: baseline.observedAt,
      source: 'synthetic-exact-USD-fixture',
    },
  };
  const { record } = store.create(operation, request, reference, run.id);
  record.preview = preview;
  budget.check(preview, run.id, record.id, true);
  record.submission = { kind: 'binance', reference, data: {} };
  record.status = 'failed';
  record.evidence =
    operation === 'binance.transfer'
      ? { clientTranId: reference, venueStatus: 'FAILED' }
      : { withdrawOrderId: reference, venueState: 'failed', status: 3 };
  store.save(record);
  const current = () => store.getRun<TestRun>(run.id)!;
  const finish = (final = baseline, costs: Record<string, string> = {}) =>
    budget.finish(run.id, final, costs, store.list());
  return { store, budget, record, current, finish, baseline, residual };
}

for (const operation of ['binance.transfer', 'binance.withdraw'] as const)
  test(`definitive failed ${operation} settles its exact reservation once and permits independently restored snapshots`, () => {
    const f = fixture(operation);
    try {
      assert.ok(f.current().inventory!.inFlight[f.record.id]);
      f.budget.settle(f.record);
      const settled = f.current();
      assert.deepEqual(settled.inventory!.available, settled.inventory!.initial);
      assert.deepEqual(settled.inventory!.settled, [f.record.id]);
      assert.equal(Object.keys(settled.inventory!.inFlight).length, 0);
      assert.equal(settled.feeReservations[f.record.id], '0');
      assert.deepEqual(settled.inventory!.costs, {});
      f.budget.settle(f.record);
      assert.deepEqual(f.current().inventory, settled.inventory);
      assert.equal(f.finish().status, 'restored');
    } finally {
      f.store.close();
    }
  });

test('wrong venue identity, absent definitive proof and uncertain statuses retain reservations and cannot pass restoration', () => {
  for (const operation of ['binance.transfer', 'binance.withdraw'] as const)
    for (const mode of ['wrong-identity', 'missing-proof', 'pending', 'unknown', 'submitted']) {
      const f = fixture(operation);
      try {
        if (mode === 'wrong-identity') {
          const key = operation === 'binance.transfer' ? 'clientTranId' : 'withdrawOrderId';
          f.record.evidence![key] = 'synthetic-other-identity';
        } else if (mode === 'missing-proof') {
          delete f.record.evidence![
            operation === 'binance.transfer' ? 'venueStatus' : 'venueState'
          ];
        } else f.record.status = mode as RecordOperation['status'];
        f.store.save(f.record);
        f.budget.settle(f.record);
        assert.ok(f.current().inventory!.inFlight[f.record.id], `${operation}: ${mode}`);
        assert.equal(f.current().inventory!.settled.includes(f.record.id), false);
        const final = f.finish();
        assert.equal(final.status, 'cleanup_outstanding', `${operation}: ${mode}`);
        assert.equal(final.differences!.inventory, 'INVENTORY_PENDING');
      } finally {
        f.store.close();
      }
    }
});

test('settled reservation still requires the same submitted and bound venue identity at finish', () => {
  for (const operation of ['binance.transfer', 'binance.withdraw'] as const)
    for (const changed of ['evidence', 'bound-identity', 'submission']) {
      const f = fixture(operation);
      try {
        f.budget.settle(f.record);
        assert.ok(f.current().inventory!.settled.includes(f.record.id));
        const key = operation === 'binance.transfer' ? 'clientTranId' : 'withdrawOrderId';
        if (changed === 'evidence') f.record.evidence![key] = 'synthetic-changed';
        else if (changed === 'bound-identity')
          (f.record.preview!.binding.params as Input)[key] = 'synthetic-changed';
        else f.record.submission!.reference = 'synthetic-changed';
        f.store.save(f.record);
        assert.equal(f.finish().status, 'cleanup_outstanding', `${operation}: ${changed}`);
      } finally {
        f.store.close();
      }
    }
});

test('a submitted venue failure cannot pass with no inventory proof or a synthetic chain revert field', () => {
  for (const operation of ['binance.transfer', 'binance.withdraw'] as const) {
    const f = fixture(operation, false);
    try {
      f.budget.settle(f.record);
      assert.equal(f.finish().status, 'cleanup_outstanding');
      f.record.evidence!.receiptStatus = 'reverted';
      f.store.save(f.record);
      assert.equal(f.finish().status, 'cleanup_outstanding');
    } finally {
      f.store.close();
    }
  }
});

test('definitive venue failure does not excuse changed financial balances or pending external movement', () => {
  for (const operation of ['binance.transfer', 'binance.withdraw'] as const) {
    const f = fixture(operation);
    try {
      f.budget.settle(f.record);
      const key = holdingKey('sub', USDC);
      const changed = {
        ...f.baseline,
        quantities: { ...f.baseline.quantities, [key]: '1999999' },
      };
      const incomplete = f.finish(changed);
      assert.equal(incomplete.status, 'cleanup_outstanding');
      assert.equal(incomplete.differences![key], '-1');
      assert.equal(
        f.finish({ ...f.baseline, pending: ['synthetic-external-pending'] }).status,
        'cleanup_outstanding',
      );
    } finally {
      f.store.close();
    }
  }
});

test('changed subatomic venue residual is immutable even when an explicit cost entry equals its debit', () => {
  const f = fixture('binance.transfer');
  try {
    f.budget.settle(f.record);
    const changed = {
      ...f.baseline,
      quantities: { ...f.baseline.quantities, [f.residual]: '99999999999' },
    };
    const incomplete = f.finish(changed, { [f.residual]: '1' });
    assert.equal(incomplete.status, 'cleanup_outstanding');
    assert.equal(incomplete.differences![f.residual], '-1');
    assert.equal(f.finish().status, 'restored');
  } finally {
    f.store.close();
  }
});
