import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { z } from 'zod';
import { MAINNET } from '../src/config.js';
import { OperationEngine, type Authorizer } from '../src/core/engine.js';
import { OperationStore } from '../src/core/store.js';
import { RunCoordinator } from '../src/core/run-coordinator.js';
import type { TestRun } from '../src/core/budget.js';
import { holdingKey } from '../src/core/inventory.js';
import type { Adapter, FinConfig, Input, Preview, RecordOperation } from '../src/core/types.js';

const WALLET = `0x${'11'.repeat(20)}`;
const DEPOSIT = `0x${'22'.repeat(20)}`;
const HASH = `0x${'33'.repeat(32)}`;
const BLOCK = `0x${'44'.repeat(32)}`;
const WETH = '0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2';
const USDC = '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48';
const request = { requestId: 'synthetic-recovery-deposit' };

/** Synthetic venue/chain observations, with the actual coordinator, engine and inventory. */
async function fixture(path = ':memory:', transientReads = 0) {
  const config: FinConfig = {
    version: 1,
    connections: [
      { name: 'wallet', kind: 'ethereum', address: WALLET, credential: 'isolated-unused-wallet' },
      { name: 'main', kind: 'binance', role: 'main', credential: 'isolated-unused-main' },
      {
        name: 'sub',
        kind: 'binance',
        role: 'sub',
        parent: 'main',
        email: 'synthetic-child@example.invalid',
        credential: 'isolated-unused-child',
      },
    ],
    ethereum: { ...MAINNET, confirmations: 2 },
  };
  const state = {
    approvals: 0,
    submissions: 0,
    reconciliations: 0,
    depositReads: 0,
    transientReads,
    proofRecipient: DEPOSIT,
    proofFee: '2',
    native: '1000',
    walletUsdc: '2000000',
  };
  const authorizer: Authorizer = {
    approve: async (_preview, bindingHash) => {
      state.approvals++;
      return {
        mechanism: 'isolated-test-only',
        bindingHash,
        approvedAt: new Date().toISOString(),
        expiresAt: new Date(Date.now() + 60000).toISOString(),
      };
    },
  };
  const definitions = ['wallet.transfer', 'binance.transfer', 'binance.withdraw'].map((name) => ({
    name,
    kind: 'write' as const,
    description: 'Synthetic bounded proof fixture; no signer or transport.',
    schema: z.object({ requestId: z.string() }).strict(),
    examples: [],
    coverage: 'isolated synthetic only',
    completion: 'Validated scoped settlement.',
  }));
  const preview = async (operation: string, input: Input): Promise<Preview> => ({
    operation,
    request: input,
    binding:
      operation === 'wallet.transfer'
        ? {
            wallet: 'wallet',
            account: WALLET,
            chainId: 1,
            token: USDC,
            decimals: 6,
            amountUnits: '1000000',
            gas: '10',
            maxFeePerGas: '2',
            effect: { recipient: DEPOSIT },
          }
        : operation === 'binance.transfer'
          ? {
              authority: 'main',
              from: 'sub',
              to: 'main',
              params: {
                asset: 'USDC',
                amount: '1',
                fromAccountType: 'SPOT',
                toAccountType: 'SPOT',
                clientTranId: `synthetic-${input.requestId}`,
              },
            }
          : { account: 'main', chainId: 1, preparationOnly: true },
    summary: 'Synthetic exact one USDC movement; no live financial action.',
    observedAt: new Date().toISOString(),
    expiresAt: new Date(Date.now() + 60000).toISOString(),
    checks: [],
    warnings: [],
    valuation: {
      movementUsdMicros: '1000000',
      maximumFeeUsdMicros: operation === 'wallet.transfer' ? '20' : '0',
      observedAt: new Date().toISOString(),
      source: 'isolated synthetic valuation',
    },
  });
  const chainEvidence = () => ({
    transactionHash: HASH,
    blockHash: BLOCK,
    confirmations: '2',
    feeWei: state.proofFee,
    receiptStatus: 'success',
    effects: { recipient: state.proofRecipient, receivedUnits: '1000000' },
  });
  const financial: Adapter & {
    preflightFundedWithdrawal: (input: Input) => Promise<Preview>;
  } = {
    definitions,
    read: async () => {
      throw Error('This synthetic write adapter has no reads.');
    },
    preview,
    preflightFundedWithdrawal: (input) => preview('binance.withdraw', input),
    submit: async (p, hooks) => {
      state.submissions++;
      const submission = {
        kind: p.operation === 'wallet.transfer' ? ('ethereum' as const) : ('binance' as const),
        reference:
          p.operation === 'wallet.transfer'
            ? HASH
            : String((p.binding.params as Input).clientTranId),
        data: {},
      };
      hooks.persistSubmission(submission);
      if (p.operation === 'wallet.transfer') {
        state.native = '998';
        state.walletUsdc = '1000000';
      }
      return submission;
    },
    reconcile: async (record) => {
      state.reconciliations++;
      return record.operation === 'wallet.transfer'
        ? { status: 'completed', evidence: chainEvidence() }
        : {
            status: 'failed',
            evidence: { clientTranId: record.submission!.reference, venueStatus: 'FAILED' },
          };
    },
  };
  const reads: Adapter & {
    quoteUsd: (token: string, units?: bigint) => Promise<Input>;
  } = {
    definitions: [
      'wallet.balances',
      'wallet.allowance',
      'aave.positions',
      'binance.balances',
      'binance.withdrawal-history',
      'binance.deposit-history',
      'binance.open-orders',
      'binance.networks',
      'binance.deposit-address',
      'binance.permissions',
      'binance.hierarchy',
    ].map((name) => ({
      name,
      kind: 'read' as const,
      description: 'Synthetic complete observation; no network.',
      schema: z.object({}),
      examples: [],
      coverage: 'isolated synthetic only',
      completion: 'Observed exact fixture state.',
    })),
    read: async (operation, input) => {
      if (operation === 'wallet.balances')
        return {
          data: {
            holdings: [
              { address: 'native', units: state.native },
              { address: WETH, units: '0' },
              { address: USDC, units: state.walletUsdc },
            ],
          },
        };
      if (operation === 'wallet.allowance') return { data: { units: '0' } };
      if (operation === 'aave.positions')
        return {
          data: {
            positions: [WETH, USDC].map((address) => ({
              token: { address },
              supplied: { units: '0' },
              variableDebt: { units: '0' },
              stableDebt: { units: '0' },
              collateralEnabled: false,
            })),
            account: { totalCollateralBase: '0', totalDebtBase: '0' },
          },
        };
      if (operation === 'binance.balances')
        return {
          balances: [{ asset: 'USDC', free: '2', locked: '0', freeze: '0', withdrawing: '0' }],
        };
      if (operation === 'binance.deposit-history' && input.txHash) {
        state.depositReads++;
        if (state.transientReads-- > 0)
          throw new Error('Synthetic signed-request text that must remain suppressed.');
        return {
          complete: true,
          records: [
            {
              txId: HASH,
              coin: 'USDC',
              network: 'ETH',
              creditState: 'credited',
              amount: '1.000000000000000000',
            },
          ],
        };
      }
      if (operation.endsWith('-history')) return { complete: true, records: [] };
      if (operation === 'binance.open-orders') return { orders: [] };
      if (operation === 'binance.permissions')
        return { permissions: { enableInternalTransfer: true } };
      if (operation === 'binance.hierarchy')
        return { accounts: [{ name: 'sub', ownership: 'observed' }] };
      if (operation === 'binance.deposit-address') return { address: DEPOSIT, memo: '' };
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
                  depositDust: '0.001',
                  withdrawFee: '0.1',
                },
              ],
            },
          ],
        };
      throw Error('Missing synthetic required read.');
    },
    quoteUsd: async (_token, units = 0n) => ({
      valueUsdMicros: units.toString(),
      observedAt: new Date().toISOString(),
      source: 'isolated synthetic atomic-unit valuation',
    }),
    preview: async () => {
      throw Error('Read fixture cannot preview a write.');
    },
    submit: async () => {
      throw Error('Read fixture cannot submit.');
    },
    reconcile: async () => {
      throw Error('Read fixture cannot reconcile a write.');
    },
  };
  const adapters = [financial, reads];
  const open = (store: OperationStore) => {
    const coordinator = new RunCoordinator(config, adapters, store);
    return {
      store,
      coordinator,
      engine: new OperationEngine(store, adapters, authorizer, {
        liveTestOnly: true,
        revalueAllocation: (id) => coordinator.revalueAllocation(id),
        enrichReconciliation: (record) => coordinator.enrichReconciliation(record),
      }),
    };
  };
  const session = open(new OperationStore(path));
  const run = await session.coordinator.begin({
    id: 'synthetic-scoped-recovery',
    allocations: {
      [holdingKey('wallet', 'native')]: '1000',
      [holdingKey('wallet', USDC)]: '2000000',
      [holdingKey('main', USDC)]: '2000000',
      [holdingKey('sub', USDC)]: '2000000',
    },
    borrowReserveUsdMicros: '0',
    costReserveUsdMicros: '100000',
    wallets: ['wallet'],
    accounts: ['main', 'sub'],
    returnRoutes: [
      {
        account: 'main',
        coin: 'USDC',
        network: 'ETH',
        wallet: 'wallet',
        withdrawAmount: '1',
        maxWithdrawalFee: '0.1',
        minimumReceipt: '0.9',
        minimumReturnAmount: '0.9',
      },
    ],
    unwind: [
      'Synthetic usable credit returns to original source; actual funds are never submitted.',
    ],
  });
  return { ...session, state, run, open, chainEvidence, authorizer };
}

function assertSettled(store: OperationStore, runId: string, recordId: string) {
  const inventory = store.getRun<TestRun>(runId)!.inventory!;
  assert.deepEqual(inventory.settled, [recordId]);
  assert.equal(Object.keys(inventory.inFlight).length, 0);
  assert.equal(inventory.available[holdingKey('wallet', 'native')], '998');
  assert.equal(inventory.available[holdingKey('wallet', USDC)], '1000000');
  assert.equal(inventory.available[holdingKey('main', USDC)], '3000000');
  assert.equal(inventory.costs[holdingKey('wallet', 'native')], '2');
}

test('confirmed deposit transfer survives a transient venue read and restart without terminal poisoning or another approval/submission', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'fin-isolated-enrichment-recovery-'));
  let store: OperationStore | undefined;
  try {
    const path = join(dir, 'operations.sqlite');
    const f = await fixture(path, 1);
    store = f.store;
    const first = await f.engine.execute('wallet.transfer', request, f.run.id);
    assert.equal(first.status, 'pending');
    assert.equal(first.evidence, undefined);
    assert.ok(store.getRun<TestRun>(f.run.id)!.inventory!.inFlight[first.id]);
    assert.equal(JSON.stringify(first).includes('signed-request'), false);
    store.close();
    store = new OperationStore(path);
    const resumed = f.open(store);
    resumed.engine.recoverInterrupted();
    const final = await resumed.engine.status(first.id, true);
    assert.equal(final.status, 'completed');
    assert.equal(final.submission!.reference, HASH);
    assert.equal((final.evidence!.depositCredit as Input).state, 'credited');
    assertSettled(store, f.run.id, final.id);
    assert.equal((await resumed.engine.execute('wallet.transfer', request, f.run.id)).id, final.id);
    assert.equal(f.state.submissions, 1);
    assert.equal(f.state.approvals, 1);
    assert.equal(f.state.depositReads, 2);
  } finally {
    store?.close();
    rmSync(dir, { recursive: true, force: true });
  }
});

test('completed but mismatched inventory proof remains reconcilable until corrected proof settles the same identity', async () => {
  const f = await fixture();
  try {
    f.state.proofRecipient = WALLET;
    const first = await f.engine.execute('wallet.transfer', request, f.run.id);
    assert.equal(first.status, 'unknown');
    assert.equal(first.error!.code, 'RECONCILIATION_PROOF_PENDING');
    const pending = f.store.getRun<TestRun>(f.run.id)!.inventory!;
    assert.equal(pending.inFlight[first.id]!.reason, 'INVENTORY_EFFECT');
    assert.deepEqual(pending.settled, []);
    assert.deepEqual(pending.costs, {});
    f.state.proofRecipient = DEPOSIT;
    const final = await f.engine.execute('wallet.transfer', request, f.run.id);
    assert.equal(final.id, first.id);
    assert.equal(final.status, 'completed');
    assert.equal(final.submission!.reference, first.submission!.reference);
    assertSettled(f.store, f.run.id, final.id);
    assert.equal(f.state.approvals, 1);
    assert.equal(f.state.submissions, 1);
  } finally {
    f.store.close();
  }
});

test('legacy completed and failed submitted records with unresolved reservations become unknown on startup and recover their original proof', async () => {
  for (const status of ['completed', 'failed'] as const) {
    const f = await fixture(':memory:', 1);
    try {
      const original = await f.engine.execute('wallet.transfer', request, f.run.id);
      const poisoned = structuredClone(original);
      poisoned.status = status;
      poisoned.evidence = f.chainEvidence();
      f.store.save(poisoned);
      f.engine.recoverInterrupted();
      assert.equal(f.store.get(original.id)!.status, 'unknown');
      assert.ok(f.store.getRun<TestRun>(f.run.id)!.inventory!.inFlight[original.id]);
      const final = await f.engine.status(original.id, true);
      assert.equal(final.status, 'completed');
      assertSettled(f.store, f.run.id, final.id);
      assert.equal(f.state.approvals, 1);
      assert.equal(f.state.submissions, 1);
    } finally {
      f.store.close();
    }
  }
});

test('a known definitive venue failure stays terminal after validated settlement and startup recovery', async () => {
  const f = await fixture();
  try {
    const input = { requestId: 'synthetic-definitive-transfer-failure' };
    const outcome = await f.engine.execute('binance.transfer', input, f.run.id);
    assert.equal(outcome.status, 'failed');
    const before = f.store.getRun<TestRun>(f.run.id)!;
    assert.deepEqual(before.inventory!.settled, [outcome.id]);
    assert.equal(Object.keys(before.inventory!.inFlight).length, 0);
    assert.deepEqual(before.inventory!.available, before.inventory!.initial);
    f.engine.recoverInterrupted();
    assert.equal(f.store.get(outcome.id)!.status, 'failed');
    assert.deepEqual(f.store.getRun<TestRun>(f.run.id), before);
    assert.equal((await f.engine.execute('binance.transfer', input, f.run.id)).status, 'failed');
    assert.equal(f.state.approvals, 1);
    assert.equal(f.state.submissions, 1);
    assert.equal(f.state.reconciliations, 1);
    assert.equal((await f.coordinator.finish(f.run.id)).status, 'restored');
  } finally {
    f.store.close();
  }
});

test('missing scoped inventory, reservation or run cannot fabricate terminal settlement from a completed chain receipt', async () => {
  for (const missing of ['inventory', 'reservation', 'run']) {
    const f = await fixture(':memory:', 1);
    try {
      const first = await f.engine.execute('wallet.transfer', request, f.run.id);
      const run = f.store.getRun<TestRun>(f.run.id)!;
      if (missing === 'inventory') delete run.inventory;
      else if (missing === 'reservation') delete run.inventory!.inFlight[first.id];
      if (missing === 'run') f.store.db.prepare('DELETE FROM runs WHERE id=?').run(run.id);
      else f.store.saveRun(run.id, run);
      const result = await f.engine.status(first.id, true);
      assert.equal(result.status, 'unknown', missing);
      assert.equal(result.error!.code, 'RECONCILIATION_PROOF_PENDING');
      assert.equal(result.submission!.reference, first.submission!.reference);
      assert.equal(f.state.submissions, 1);
      assert.equal(f.state.approvals, 1);
    } finally {
      f.store.close();
    }
  }
});

test('candidate save failure rolls back inventory and fee settlement with the operation record, then recovery applies it once', async () => {
  const f = await fixture(':memory:', 1);
  try {
    const first = await f.engine.execute('wallet.transfer', request, f.run.id);
    const before = f.store.getRun<TestRun>(f.run.id)!;
    const originalSave = f.store.save.bind(f.store);
    let rejected = false;
    f.store.save = (record: RecordOperation) => {
      if (!rejected && record.status === 'completed') {
        rejected = true;
        throw Error('Synthetic database fault after ledger settlement, before record save.');
      }
      return originalSave(record);
    };
    const failedCommit = await f.engine.status(first.id, true);
    assert.equal(failedCommit.status, 'pending');
    assert.deepEqual(f.store.getRun<TestRun>(f.run.id), before);
    assert.equal(f.store.get(first.id)!.status, 'pending');
    f.store.save = originalSave;
    const final = await f.engine.status(first.id, true);
    assert.equal(final.status, 'completed');
    assertSettled(f.store, f.run.id, final.id);
    assert.equal(f.state.approvals, 1);
    assert.equal(f.state.submissions, 1);
  } finally {
    f.store.close();
  }
});
