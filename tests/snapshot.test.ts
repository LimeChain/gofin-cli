import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { z } from 'zod';
// The imported module's CLI is guarded: these tests read no credential/config files and create no network clients.
import {
  assertReadOnlyRpcPayload,
  assertReadOnlyVenueRequest,
  captureSnapshot,
} from '../scripts/live-snapshot.js';
import { FinError } from '../src/core/errors.js';
import type { Adapter, FinConfig, Input, RecordOperation, Status } from '../src/core/types.js';

const address = (pair: string) => `0x${pair.repeat(20)}`;
const WETH = '0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2';
const USDC = '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48';
const OTHER_RESERVE = address('44');
function fixture() {
  const config: FinConfig = {
    version: 1,
    connections: [
      { name: 'synthetic-wallet', kind: 'ethereum', address: address('11') },
      { name: 'synthetic-main', kind: 'binance', role: 'main' },
      {
        name: 'synthetic-child',
        kind: 'binance',
        role: 'sub',
        parent: 'synthetic-main',
        email: 'synthetic-child@example.invalid',
      },
    ],
    ethereum: {
      chainId: 1,
      confirmations: 2,
      rpcUrls: ['http://isolated-fixture.invalid'],
      uniswapRouter: address('33'),
      uniswapFactory: address('33'),
      uniswapQuoter: address('33'),
      aavePool: address('22'),
      aaveOracle: address('22'),
    },
  };
  const state = {
    failRead: '',
    incompleteHistory: false,
    venueFree: '10',
    venueLocked: '0',
    venueFreeze: '0.25',
    venueWithdrawing: '0.5',
  };
  const calls: { operation: string; input: Input }[] = [];
  const reads = [
    'wallet.balances',
    'aave.positions',
    'wallet.allowance',
    'binance.balances',
    'binance.withdrawal-history',
    'binance.deposit-history',
    'binance.open-orders',
    'binance.permissions',
    'binance.hierarchy',
    'binance.networks',
  ];
  const adapter: Adapter = {
    definitions: reads.map((name) => ({
      name,
      kind: 'read',
      schema: z.object({}).passthrough(),
      description: 'Isolated snapshot fixture',
      examples: [],
      coverage: 'No credentials, signer or network transport',
      completion: 'Synthetic read observation',
    })),
    read: async (operation, input) => {
      calls.push({ operation, input });
      if (operation === state.failRead)
        throw new Error('SYNTHETIC_SIGNED_REQUEST_SECRET_MUST_NOT_ESCAPE');
      if (operation === 'wallet.balances')
        return {
          data: {
            holdings: [
              { address: 'native', units: '100' },
              { address: WETH, units: '0' },
              { address: USDC, units: '0' },
            ],
          },
        };
      if (operation === 'aave.positions')
        return {
          data: {
            positions: [
              {
                token: { address: OTHER_RESERVE },
                supplied: { units: '7' },
                variableDebt: { units: '2' },
                stableDebt: { units: '0' },
                collateralEnabled: true,
              },
            ],
            account: { totalCollateralBase: '7', totalDebtBase: '2' },
          },
        };
      if (operation === 'wallet.allowance') return { data: { units: '0' } };
      if (operation === 'binance.balances')
        return {
          balances: [
            {
              asset: 'USDC',
              free: state.venueFree,
              locked: state.venueLocked,
              ...(input.account === 'synthetic-child'
                ? { freeze: state.venueFreeze, withdrawing: state.venueWithdrawing }
                : {}),
            },
          ],
        };
      if (operation.endsWith('-history'))
        return { complete: !state.incompleteHistory, records: [] };
      if (operation === 'binance.open-orders') return { orders: [] };
      if (operation === 'binance.permissions') return { permissions: { enableReading: true } };
      if (operation === 'binance.hierarchy')
        return { accounts: [{ name: 'synthetic-child', ownership: 'observed' }] };
      if (operation === 'binance.networks') return { assets: [] };
      assert.fail('Unexpected synthetic snapshot read.');
    },
    preview: async () => assert.fail('Read-only snapshot must never preview a write.'),
    submit: async () => assert.fail('Read-only snapshot must never submit a write.'),
    reconcile: async () =>
      assert.fail('Read-only snapshot must never invoke execution reconciliation.'),
  };
  return { config, adapter, state, calls };
}
function record(status: Status): RecordOperation {
  return {
    id: 'synthetic-operation',
    requestId: 'synthetic-request',
    fingerprint: 'synthetic-fingerprint',
    operation: 'binance.transfer',
    input: {},
    status,
    createdAt: '2026-10-01T10:00:00.000Z',
    updatedAt: '2026-10-01T10:00:00.000Z',
  };
}
const code = (expected: string) => (error: unknown) =>
  error instanceof FinError && error.code === expected;

test('snapshot helper imports from an empty input directory with no credential environment and disabled network', () => {
  const dir = mkdtempSync(join(tmpdir(), 'fin-snapshot-import-isolated-'));
  try {
    const script = new URL('../scripts/live-snapshot.ts', import.meta.url).href;
    const loader = new URL('../node_modules/tsx/dist/loader.mjs', import.meta.url).href;
    const child = spawnSync(
      process.execPath,
      [
        '--import',
        loader,
        '--input-type=module',
        '--eval',
        `
      import http from 'node:http';
      import https from 'node:https';
      import { syncBuiltinESMExports } from 'node:module';
      const forbidden = () => { throw new Error('Synthetic isolated import forbids network access.'); };
      http.request = http.get = forbidden;
      https.request = https.get = forbidden;
      globalThis.fetch = forbidden;
      syncBuiltinESMExports();
      await import(${JSON.stringify(script)});
      process.stdout.write('isolated-import-complete');
    `,
      ],
      {
        cwd: dir,
        env: { PATH: process.env.PATH, NODE_NO_WARNINGS: '1' },
        encoding: 'utf8',
        timeout: 15000,
      },
    );
    assert.equal(
      child.status,
      0,
      'Import must not require configuration, credentials or network access.',
    );
    assert.ok(
      child.stdout === 'isolated-import-complete',
      'Imported helper must emit no incidental output.',
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
test('snapshot module imports without credentials and injected reads capture all reserves and four allowances', async () => {
  const h = fixture();
  const evidence = await captureSnapshot(h.config, [h.adapter], []);
  assert.equal(evidence.status, 'complete');
  assert.equal(evidence.snapshotComplete, true);
  assert.equal(evidence.usableBaseline, true);
  assert.equal(evidence.authorizesLiveExecution, false);
  assert.equal(evidence.financialWrites, 0);
  assert.equal(h.calls.find((c) => c.operation === 'aave.positions')!.input.tokens, undefined);
  assert.equal(h.calls.filter((c) => c.operation === 'wallet.allowance').length, 4);
  assert.equal(evidence.snapshot!.quantities[`synthetic-wallet/supplied/${OTHER_RESERVE}`], '7');
  assert.equal(evidence.snapshot!.quantities[`synthetic-wallet/debt/${OTHER_RESERVE}`], '2');
  assert.equal(
    evidence.snapshot!.quantities[`synthetic-child/order/freeze/${USDC}`],
    '250000000000000000',
  );
  assert.equal(
    evidence.snapshot!.quantities[`synthetic-child/order/withdrawing/${USDC}`],
    '500000000000000000',
  );
});
test('read-only evidence preserves meaningful venue precision with immutable residual and exact encumbered units', async () => {
  const h = fixture();
  Object.assign(h.state, {
    venueFree: '10.0000001',
    venueLocked: '0.0000002',
    venueFreeze: '0.2500003',
    venueWithdrawing: '0.5000004',
  });
  const evidence = await captureSnapshot(h.config, [h.adapter], []);
  assert.equal(evidence.usableBaseline, true);
  assert.equal(evidence.snapshot!.quantities[`synthetic-main/holding/${USDC}`], '10000000');
  assert.equal(evidence.snapshot!.quantities[`synthetic-main/residual/${USDC}`], '100000000000');
  assert.equal(
    evidence.snapshot!.quantities[`synthetic-main/order/locked/${USDC}`],
    '200000000000',
  );
  assert.equal(
    evidence.snapshot!.quantities[`synthetic-child/order/freeze/${USDC}`],
    '250000300000000000',
  );
  assert.equal(
    evidence.snapshot!.quantities[`synthetic-child/order/withdrawing/${USDC}`],
    '500000400000000000',
  );
});
test('read-only RPC guard rejects a mixed financial batch before dispatching any member', () => {
  let dispatched = 0;
  const dispatch = (payload: unknown) => {
    assertReadOnlyRpcPayload(payload);
    dispatched++;
  };
  assert.throws(
    () =>
      dispatch([
        { method: 'eth_chainId' },
        { method: 'eth_sendRawTransaction', params: ['synthetic-only'] },
      ]),
    code('READ_ONLY_RPC'),
  );
  assert.equal(dispatched, 0);
  for (const payload of [[], null, { method: 'eth_sign' }, { method: 'personal_sendTransaction' }])
    assert.throws(() => dispatch(payload), code('READ_ONLY_RPC'));
  dispatch([{ method: 'eth_chainId' }, { method: 'eth_call', params: [] }]);
  assert.equal(dispatched, 1);
});
test('Binance read guard permits reviewed v4 and read endpoints and denies POST or unreviewed routes', () => {
  for (const [path, api] of [
    ['sub-account/assets', 'sapiV4'],
    ['account', 'private'],
    ['openOrders', 'private'],
    ['capital/withdraw/history', 'sapi'],
    ['capital/deposit/hisrec', 'sapi'],
  ])
    assertReadOnlyVenueRequest(path, api, 'GET');
  for (const [path, api, method] of [
    ['capital/withdraw/apply', 'sapi', 'POST'],
    ['capital/withdraw/apply', 'sapi', 'GET'],
    ['order', 'private', 'POST'],
    ['sub-account/assets', 'sapiV3', 'GET'],
  ])
    assert.throws(() => assertReadOnlyVenueRequest(path, api, method), code('READ_ONLY_VENUE'));
});
test('pending or awaiting operations cannot produce a usable read-only baseline', async () => {
  for (const status of ['submitted', 'pending', 'unknown', 'awaiting_approval'] as const) {
    const h = fixture();
    const evidence = await captureSnapshot(h.config, [h.adapter], [record(status)]);
    assert.equal(evidence.snapshotComplete, true);
    assert.equal(evidence.usableBaseline, false);
    assert.equal(evidence.status, 'blocked');
    assert.equal(evidence.failure!.code, 'PENDING_BASELINE');
    assert.ok(evidence.snapshot!.pending.includes('fin:synthetic-operation'));
  }
});
test('operation-state changes during observation cannot produce a usable baseline even with terminal records', async () => {
  const h = fixture();
  let reads = 0;
  const evidence = await captureSnapshot(h.config, [h.adapter], () =>
    ++reads >= 3 ? [record('completed')] : [],
  );
  assert.equal(evidence.status, 'blocked');
  assert.equal(evidence.usableBaseline, false);
  assert.equal(evidence.snapshotComplete, false);
  assert.equal(evidence.failure!.code, 'SNAPSHOT_STATE_CHANGED');
});
test('missing required reads or incomplete funding histories remain blocked and suppress external error details', async () => {
  for (const requiredRead of [
    'aave.positions',
    'wallet.allowance',
    'binance.open-orders',
    'binance.permissions',
  ]) {
    const h = fixture();
    h.state.failRead = requiredRead;
    const evidence = await captureSnapshot(h.config, [h.adapter], []);
    assert.equal(evidence.status, 'blocked');
    assert.equal(evidence.snapshotComplete, false);
    assert.equal(evidence.usableBaseline, false);
    assert.equal(evidence.snapshot, undefined);
    assert.ok(!JSON.stringify(evidence).includes('SYNTHETIC_SIGNED_REQUEST_SECRET'));
  }
  const h = fixture();
  h.state.incompleteHistory = true;
  const evidence = await captureSnapshot(h.config, [h.adapter], []);
  assert.equal(evidence.usableBaseline, false);
  assert.equal(evidence.failure!.code, 'SNAPSHOT_INCOMPLETE');
});
