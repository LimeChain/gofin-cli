import assert from 'node:assert/strict';
import test from 'node:test';
import ccxt from 'ccxt';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  BinanceAdapter,
  BinanceAdapterError,
  binanceDefinitions,
  type BinanceClient,
  type BinanceOptions,
} from '../src/adapters/binance.js';
import { OperationEngine, type Authorizer } from '../src/core/engine.js';
import { OperationStore } from '../src/core/store.js';
import { FinError } from '../src/core/errors.js';
import { BudgetGuard, type RunPlan } from '../src/core/budget.js';
import type { FinConfig, Input, Preview, RecordOperation, Submission } from '../src/core/types.js';

// These fixtures have no signing key, no usable account, and no network transport.
const FIXTURE_TIME = new Date('2026-10-01T10:00:00.000Z');
const FIXTURE_ADDRESS = `0x${'11'.repeat(20)}`;
const FIXTURE_TX = `0x${'22'.repeat(32)}`;
function config(): FinConfig {
  return {
    version: 1,
    connections: [
      { name: 'binance-main', kind: 'binance', role: 'main', walletType: 'SPOT' },
      {
        name: 'binance-sub-a',
        kind: 'binance',
        role: 'sub',
        parent: 'binance-main',
        email: 'synthetic-sub-a@example.invalid',
        walletType: 'SPOT',
      },
      {
        name: 'binance-sub-b',
        kind: 'binance',
        role: 'sub',
        parent: 'binance-main',
        email: 'synthetic-sub-b@example.invalid',
        walletType: 'SPOT',
      },
      { name: 'other-main', kind: 'binance', role: 'main', walletType: 'SPOT' },
      { name: 'hot-wallet', kind: 'ethereum', address: FIXTURE_ADDRESS },
    ],
    ethereum: {
      chainId: 1,
      confirmations: 2,
      rpcUrls: ['http://isolated-fixture.invalid'],
      uniswapRouter: FIXTURE_ADDRESS,
      uniswapQuoter: FIXTURE_ADDRESS,
      uniswapFactory: FIXTURE_ADDRESS,
      aavePool: FIXTURE_ADDRESS,
      aaveOracle: FIXTURE_ADDRESS,
    },
  };
}
const TRANSFER: Input = {
  requestId: 'isolated-transfer-001',
  authority: 'binance-main',
  from: 'binance-sub-a',
  to: 'binance-main',
  coin: 'ETH',
  amount: '0.001',
  fromWallet: 'SPOT',
  toWallet: 'SPOT',
};
const WITHDRAW: Input = {
  requestId: 'isolated-withdraw-001',
  account: 'binance-main',
  destination: 'hot-wallet',
  coin: 'ETH',
  amount: '0.01',
  network: 'ETH',
  walletType: 'SPOT',
  maxFee: '0.0002',
  minReceiveAmount: '0.0098',
};
function operation(preview: Preview, submission: Submission): RecordOperation {
  return {
    id: 'synthetic-operation',
    requestId: String(preview.request.requestId),
    fingerprint: 'synthetic-fingerprint',
    operation: preview.operation,
    input: preview.request,
    status: 'submitted',
    createdAt: FIXTURE_TIME.toISOString(),
    updatedAt: FIXTURE_TIME.toISOString(),
    preview,
    submission,
  };
}
function harness(options: BinanceOptions = {}) {
  const state = {
    mainFree: '1',
    subFree: '0.1',
    subBFree: '0.1',
    locked: '0',
    permissions: {
      enableReading: true,
      enableWithdrawals: true,
      enableInternalTransfer: true,
      permitsUniversalTransfer: false,
    },
    network: {
      network: 'ETH',
      coin: 'ETH',
      name: 'Ethereum (ERC20)',
      withdrawEnable: true,
      depositEnable: true,
      withdrawFee: '0.0001',
      withdrawMin: '0.0001',
      withdrawMax: '100',
      withdrawIntegerMultiple: '0.00000001',
      withdrawTag: false,
      minConfirm: 2,
      unLockConfirm: 2,
      depositDust: '0.000001',
      contractAddress: '',
      denomination: 1,
      busy: false,
    },
    currency: { coin: 'ETH', isLegalMoney: false, withdrawAllEnable: true, depositAllEnable: true },
    travelRule: { questionnaireCountryCode: 'NIL' },
    frozen: false,
    transferHistory: [] as Input[],
    withdrawalHistory: [] as Input[],
    depositHistory: [] as Input[],
    writes: 0,
    failTransferResponse: false,
    failWithdrawResponse: false,
    permissionsFailure: false,
    recipientStatus: 'completed' as 'pending' | 'completed' | 'unknown' | 'failed',
  };
  const calls: { method: string; params: Input }[] = [];
  const receiptCalls: Input[] = [];
  const accountConfig = config();
  const methods: Record<string, (params: Input) => Promise<unknown>> = {
    privateGetAccount: async () => ({
      balances: [{ asset: 'ETH', free: state.mainFree, locked: state.locked }],
      apiKey: 'DO_NOT_RETURN_SYNTHETIC_SECRET',
    }),
    sapiV4GetSubAccountAssets: async (p) => ({
      balances: [
        {
          asset: 'ETH',
          free: p.email === 'synthetic-sub-b@example.invalid' ? state.subBFree : state.subFree,
          locked: '0',
          freeze: '0',
          withdrawing: '0',
        },
      ],
    }),
    sapiGetAccountApiRestrictions: async () => {
      if (state.permissionsFailure)
        throw Object.assign(new Error('signed_url=SYNTHETIC_SECRET'), {
          name: 'AuthenticationError',
        });
      return { ...state.permissions, apiKey: 'DO_NOT_RETURN_SYNTHETIC_SECRET' };
    },
    sapiGetSubAccountList: async (p) => ({
      subAccounts: [{ email: p.email, isFreeze: state.frozen }],
    }),
    sapiGetCapitalConfigGetall: async () => [{ ...state.currency, networkList: [state.network] }],
    sapiGetLocalentityQuestionnaireRequirements: async () => state.travelRule,
    sapiGetCapitalDepositAddress: async () => ({
      address: FIXTURE_ADDRESS,
      coin: 'ETH',
      tag: '',
      url: 'do-not-return',
    }),
    sapiGetCapitalDepositHisrec: async () => state.depositHistory,
    sapiGetCapitalWithdrawHistory: async () => state.withdrawalHistory,
    sapiGetSubAccountUniversalTransfer: async () => ({
      result: state.transferHistory,
      totalCount: state.transferHistory.length,
    }),
    sapiPostSubAccountUniversalTransfer: async (p) => {
      state.writes++;
      if (p.fromEmail) {
        state.subFree = '0.099';
        state.mainFree = '1.001';
      } else {
        state.mainFree = '0.999';
        state.subFree = '0.101';
      }
      state.transferHistory = [
        {
          ...p,
          fromEmail: p.fromEmail ?? 'synthetic-main@example.invalid',
          toEmail: p.toEmail ?? 'synthetic-main@example.invalid',
          tranId: '10001',
          status: 'SUCCESS',
          createTimeStamp: FIXTURE_TIME.getTime(),
        },
      ];
      if (state.failTransferResponse)
        throw Object.assign(new Error('SYNTHETIC_SIGNED_REQUEST_NEVER_EXPOSE'), {
          name: 'RequestTimeout',
        });
      return { tranId: '10001', clientTranId: p.clientTranId };
    },
    sapiPostCapitalWithdrawApply: async (p) => {
      state.writes++;
      state.mainFree = '0.99';
      state.withdrawalHistory = [
        {
          id: 'fixture-withdraw-1',
          withdrawOrderId: p.withdrawOrderId,
          coin: 'ETH',
          amount: '0.0099',
          transactionFee: '0.0001',
          network: 'ETH',
          address: p.address,
          transferType: 0,
          status: 6,
          txId: FIXTURE_TX,
          walletType: 0,
          txKey: 'NEVER_RETURN_SYNTHETIC_SECRET',
          info: 'NEVER_RETURN_PRIVATE_RAW_MESSAGE',
        },
      ];
      if (state.failWithdrawResponse)
        throw Object.assign(new Error('SYNTHETIC_SIGNED_REQUEST_NEVER_EXPOSE'), {
          name: 'RequestTimeout',
        });
      return { id: 'fixture-withdraw-1' };
    },
  };
  const client: BinanceClient = {};
  for (const [method, handler] of Object.entries(methods))
    client[method] = async (params: Input = {}) => {
      calls.push({ method, params: structuredClone(params) });
      return handler(params);
    };
  const adapter = new BinanceAdapter(
    accountConfig,
    { 'binance-main': client },
    {
      now: () => FIXTURE_TIME,
      priceProvider: {
        quote: async () => ({
          usdPerUnit: '2500.123456',
          observedAt: FIXTURE_TIME.toISOString(),
          source: 'isolated synthetic USD fixture',
        }),
      },
      recipientVerifier: {
        verify: async (p) => {
          receiptCalls.push(p as unknown as Input);
          return {
            status: state.recipientStatus,
            evidence: { chainId: 1, recipient: p.recipient, amount: '0.0099', confirmations: 2 },
          };
        },
      },
      ...options,
    },
  );
  return { adapter, state, calls, client, accountConfig, receiptCalls };
}
async function submitted(
  h: ReturnType<typeof harness>,
  kind = 'binance.withdraw',
  input = WITHDRAW,
) {
  const preview = await h.adapter.preview(kind, input);
  const persisted: Submission[] = [];
  const submission = await h.adapter.submit(preview, {
    persistSubmission: (s) => persisted.push(structuredClone(s)),
  });
  return { preview, submission, persisted, record: operation(preview, submission) };
}
function rejectCode(code: string) {
  return (error: unknown) => error instanceof BinanceAdapterError && error.code === code;
}
function isolatedAuthorizer(onApprove?: () => void): Authorizer {
  return {
    approve: async (_preview, bindingHash) => {
      onApprove?.();
      return {
        mechanism: 'isolated-test-only',
        bindingHash,
        approvedAt: FIXTURE_TIME.toISOString(),
        expiresAt: new Date(FIXTURE_TIME.getTime() + 60_000).toISOString(),
      };
    },
  };
}

test('strict discoverable schemas classify reads/writes and require explicit units/network/request identity', () => {
  assert.equal(binanceDefinitions.filter((d) => d.kind === 'write').length, 2);
  for (const definition of binanceDefinitions)
    for (const example of definition.examples)
      assert.equal(definition.schema.safeParse(example).success, true);
  const schema = binanceDefinitions.find((d) => d.name === 'binance.withdraw')!.schema;
  assert.equal(schema.safeParse({ ...WITHDRAW, network: 'BSC' }).success, false);
  assert.equal(schema.safeParse({ ...WITHDRAW, amount: 0.01 }).success, false);
  assert.equal(schema.safeParse({ ...WITHDRAW, amount: '1e-2' }).success, false);
  assert.equal(
    schema.safeParse({ ...WITHDRAW, unauthorizedMethod: 'arbitrary signed write' }).success,
    false,
  );
  const { requestId: _, ...withoutIdentity } = WITHDRAW;
  assert.equal(schema.safeParse(withoutIdentity).success, false);
});
test('balances and permissions preserve venue meanings and never return raw secrets', async () => {
  const h = harness();
  h.state.locked = '0.02';
  const balances = await h.adapter.read('binance.balances', { account: 'binance-main' });
  assert.deepEqual((balances as Input).balances, [{ asset: 'ETH', free: '1', locked: '0.02' }]);
  const permissions = await h.adapter.read('binance.permissions', { account: 'binance-main' });
  assert.equal(((permissions as Input).permissions as Input).ipRestrict, 'unknown');
  assert.equal(JSON.stringify([balances, permissions]).includes('SYNTHETIC_SECRET'), false);
  assert.equal(h.state.writes, 0);
});
test('open orders preserve exact identities, financial quantities and partial fill state without raw payloads', async () => {
  const h = harness();
  const requests: Input[] = [];
  h.client.privateGetOpenOrders = async (p) => {
    requests.push(p as Input);
    return [
      {
        symbol: 'ETHUSDC',
        orderId: 10001,
        clientOrderId: 'isolated-client-order',
        side: 'BUY',
        type: 'LIMIT',
        status: 'PARTIALLY_FILLED',
        price: '2500.12345678',
        origQty: '0.01',
        executedQty: '0.001',
        cummulativeQuoteQty: '2.50012345',
        timeInForce: 'GTC',
        time: FIXTURE_TIME.getTime(),
        apiKey: 'DO_NOT_RETURN_SYNTHETIC_SECRET',
      },
    ];
  };
  const result = (await h.adapter.read('binance.open-orders', {
    account: 'binance-main',
    symbol: 'ETHUSDC',
  })) as Input;
  assert.deepEqual(requests, [{ symbol: 'ETHUSDC' }]);
  const order = (result.orders as Input[])[0]!;
  assert.equal(order.orderId, '10001');
  assert.equal(order.price, '2500.12345678');
  assert.equal(order.executedQty, '0.001');
  assert.equal(order.status, 'PARTIALLY_FILLED');
  assert.equal(JSON.stringify(result).includes('SYNTHETIC_SECRET'), false);
  await assert.rejects(
    h.adapter.read('binance.open-orders', { account: 'binance-sub-a' }),
    rejectCode('MISSING_ACCESS'),
  );
  assert.equal(h.state.writes, 0);
  h.client.privateGetOpenOrders = async () => [
    {
      orderId: Number.MAX_SAFE_INTEGER + 1,
      symbol: 'ETHUSDC',
      status: 'NEW',
      origQty: '1',
      executedQty: '0',
    },
  ];
  await assert.rejects(
    h.adapter.read('binance.open-orders', { account: 'binance-main' }),
    rejectCode('VENUE_RESPONSE_INVALID'),
  );
});
test('hierarchy verifies named children and rejects credential substitution', async () => {
  const h = harness();
  const hierarchy = (await h.adapter.read('binance.hierarchy', {
    authority: 'binance-main',
  })) as Input;
  assert.equal((hierarchy.accounts as Input[]).length, 2);
  await assert.rejects(
    h.adapter.preview('binance.transfer', { ...TRANSFER, authority: 'binance-sub-a' }),
    rejectCode('WRONG_AUTHORITY'),
  );
  await assert.rejects(
    h.adapter.preview('binance.transfer', { ...TRANSFER, to: 'other-main' }),
    rejectCode('WRONG_HIERARCHY'),
  );
  h.state.frozen = true;
  await assert.rejects(
    h.adapter.preview('binance.transfer', TRANSFER),
    rejectCode('ACCOUNT_FROZEN_OR_UNKNOWN'),
  );
  assert.equal(h.state.writes, 0);
});
test('subaccount balance read uses explicitly selected main authority and exact child mapping', async () => {
  const h = harness();
  const result = (await h.adapter.read('binance.balances', {
    account: 'binance-sub-a',
    authority: 'binance-main',
  })) as Input;
  assert.equal((result.balances as Input[])[0]!.free, '0.1');
  assert.deepEqual(h.calls.at(-1), {
    method: 'sapiV4GetSubAccountAssets',
    params: { email: 'synthetic-sub-a@example.invalid' },
  });
  await assert.rejects(
    h.adapter.read('binance.balances', { account: 'binance-sub-a' }),
    rejectCode('MISSING_ACCESS'),
  );
});
test('v4 subaccount balances preserve raw decimal strings and never fall back to lossy v3 JSON numbers', async () => {
  const h = harness();
  h.client.sapiV3GetSubAccountAssets = async () =>
    assert.fail('Lossy v3 response must not be requested.');
  const precise = {
    asset: 'ETH',
    free: '0.100000000000000001',
    locked: '0.000000000000000003',
    freeze: '0.000000000000000004',
    withdrawing: '0.000000000000000005',
  };
  h.client.sapiV4GetSubAccountAssets = async () => ({ balances: [precise] });
  const result = (await h.adapter.read('binance.balances', {
    account: 'binance-sub-a',
    authority: 'binance-main',
  })) as Input;
  assert.deepEqual(result.balances, [precise]);
  for (const field of ['free', 'locked', 'freeze', 'withdrawing']) {
    h.client.sapiV4GetSubAccountAssets = async () => ({ balances: [{ ...precise, [field]: 0 }] });
    await assert.rejects(
      h.adapter.read('binance.balances', { account: 'binance-sub-a', authority: 'binance-main' }),
      rejectCode('VENUE_RESPONSE_INVALID'),
    );
  }
  h.client.sapiV4GetSubAccountAssets = undefined;
  await assert.rejects(
    h.adapter.read('binance.balances', { account: 'binance-sub-a', authority: 'binance-main' }),
    rejectCode('MISSING_ACCESS'),
  );
  assert.equal(h.state.writes, 0);
});
test('conditional funded withdrawal observes child funds but cannot replace actual balance checks or authorize a write', async () => {
  const h = harness();
  h.state.mainFree = '0';
  const sources = [
    { authority: 'binance-main', from: 'binance-sub-a', coin: 'ETH', amount: '0.011' },
  ];
  await assert.rejects(
    h.adapter.preview('binance.withdraw', WITHDRAW),
    rejectCode('INSUFFICIENT_BALANCE'),
  );
  const conditional = await h.adapter.preflightFundedWithdrawal(WITHDRAW, sources, '30000000');
  assert.equal(conditional.binding.preparationOnly, true);
  assert.equal(conditional.binding.observedMainFree, '0');
  assert.equal(conditional.binding.projectedMainFree, '0.011');
  assert.equal((conditional.binding.fundingDependencies as Input[])[0]!.from, 'binance-sub-a');
  assert.ok(
    conditional.checks.some((c) => c.name === 'funding_dependency' && c.status === 'unchecked'),
  );
  assert.equal(h.state.writes, 0);
  await assert.rejects(h.adapter.revalidate(conditional), rejectCode('PREPARATION_ONLY'));
  await assert.rejects(
    h.adapter.submit(conditional, {
      persistSubmission: () => assert.fail('Preparation must not persist any submission.'),
    }),
    rejectCode('PREPARATION_ONLY'),
  );
  await assert.rejects(
    h.adapter.preview('binance.withdraw', WITHDRAW),
    rejectCode('INSUFFICIENT_BALANCE'),
  );
  h.state.mainFree = '0.011';
  assert.equal(
    (await h.adapter.preview('binance.withdraw', WITHDRAW)).binding.preparationOnly,
    undefined,
  );
  await assert.rejects(
    h.adapter.submit(conditional, {
      persistSubmission: () =>
        assert.fail('Preparation remains non-executable even after funding.'),
    }),
    rejectCode('PREPARATION_ONLY'),
  );
  assert.equal(h.state.writes, 0);
});
test('conditional funding rejects unproved hierarchy, authority, permissions, funds, precision, asset and cap', async () => {
  const source = { authority: 'binance-main', from: 'binance-sub-a', coin: 'ETH', amount: '0.011' };
  for (const [mutate, sources, cap, expected] of [
    [
      (h: ReturnType<typeof harness>) => {
        h.state.frozen = true;
      },
      [source],
      '30000000',
      'ACCOUNT_FROZEN_OR_UNKNOWN',
    ],
    [
      (h: ReturnType<typeof harness>) => {
        h.state.permissions.enableInternalTransfer = false;
      },
      [source],
      '30000000',
      'MISSING_PERMISSION',
    ],
    [() => {}, [{ ...source, from: 'other-main' }], '30000000', 'WRONG_HIERARCHY'],
    [() => {}, [{ ...source, authority: 'other-main' }], '30000000', 'FUNDING_SOURCE_MISMATCH'],
    [() => {}, [{ ...source, amount: '0.2' }], '100000000', 'INSUFFICIENT_BALANCE'],
    [() => {}, [{ ...source, amount: '0.011000001' }], '30000000', 'INVALID_PRECISION'],
    [() => {}, [{ ...source, coin: 'USDC' }], '30000000', 'FUNDING_SOURCE_MISMATCH'],
    [() => {}, [source, source], '100000000', 'FUNDING_SOURCE_MISMATCH'],
    [() => {}, [source], '1000000', 'FUNDING_CAP'],
    [() => {}, [source], '100000001', 'FUNDING_CAP'],
  ] as const) {
    const h = harness();
    h.state.mainFree = '0';
    mutate(h);
    await assert.rejects(
      h.adapter.preflightFundedWithdrawal(WITHDRAW, [...sources], cap),
      rejectCode(expected),
    );
    assert.equal(h.state.writes, 0);
  }
  const h = harness();
  h.state.mainFree = '0';
  h.state.currency.coin = 'USDC';
  h.state.network.coin = 'USDC';
  h.state.network.contractAddress = '0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48';
  h.state.network.withdrawIntegerMultiple = '0.00000001';
  await assert.rejects(
    h.adapter.preflightFundedWithdrawal(
      { ...WITHDRAW, coin: 'USDC' },
      [{ ...source, coin: 'USDC', amount: '0.00000001' }],
      '30000000',
    ),
    rejectCode('INVALID_PRECISION'),
  );
  assert.equal(h.state.writes, 0);
});
test('conditional funding rejects two configured aliases for the same real child', async () => {
  for (const aliasEmail of [
    'synthetic-sub-a@example.invalid',
    '  SYNTHETIC-SUB-A@EXAMPLE.INVALID  ',
  ]) {
    const h = harness();
    h.state.mainFree = '0';
    h.accountConfig.connections.find((c) => c.name === 'binance-sub-b')!.email = aliasEmail;
    await assert.rejects(
      h.adapter.preflightFundedWithdrawal(
        WITHDRAW,
        ['binance-sub-a', 'binance-sub-b'].map((from) => ({
          authority: 'binance-main',
          from,
          coin: 'ETH',
          amount: '0.011',
        })),
        '100000000',
      ),
      rejectCode('FUNDING_SOURCE_MISMATCH'),
    );
    assert.equal(h.state.writes, 0);
    assert.equal(h.calls.filter((c) => c.method === 'sapiV4GetSubAccountAssets').length, 1);
  }
});
test('transfer preview does not submit, values exact decimal USD and checks internal transfer permission', async () => {
  const h = harness();
  const preview = await h.adapter.preview('binance.transfer', TRANSFER);
  assert.equal(h.state.writes, 0);
  assert.equal(preview.valuation!.movementUsdMicros, '2500124');
  assert.equal(preview.valuation!.maximumFeeUsdMicros, '0');
  assert.equal((preview.binding.params as Input).amount, '0.001');
  h.state.permissions.enableInternalTransfer = false;
  await assert.rejects(
    h.adapter.preview('binance.transfer', TRANSFER),
    rejectCode('MISSING_PERMISSION'),
  );
});
test('sub-to-main transfer persists client identity and baseline before send, then reconciles history and both credits', async () => {
  const h = harness();
  const preview = await h.adapter.preview('binance.transfer', TRANSFER);
  const persisted: Submission[] = [];
  const original = h.client.sapiPostSubAccountUniversalTransfer as (p: Input) => Promise<unknown>;
  h.client.sapiPostSubAccountUniversalTransfer = async (p: Input) => {
    assert.equal(persisted.length, 1);
    assert.equal(persisted[0]!.reference, p.clientTranId);
    assert.equal(persisted[0]!.data.sourceFreeBefore, '0.1');
    return original(p);
  };
  const submission = await h.adapter.submit(preview, {
    persistSubmission: (s) => persisted.push(structuredClone(s)),
  });
  assert.equal(h.state.writes, 1);
  assert.equal(persisted.length, 2);
  const result = await h.adapter.reconcile(operation(preview, submission));
  assert.equal(result.status, 'completed');
  assert.equal(result.evidence.sourceDebitObserved, '0.001');
  assert.equal(result.evidence.destinationCreditObserved, '0.001');
  const query = h.calls.find((c) => c.method === 'sapiGetSubAccountUniversalTransfer')!;
  assert.equal(query.params.fromEmail, 'synthetic-sub-a@example.invalid');
  assert.equal(query.params.toEmail, undefined);
});
test('main-to-sub transfer constructs the reverse route without changing authority', async () => {
  const h = harness();
  const r = await submitted(h, 'binance.transfer', {
    ...TRANSFER,
    from: 'binance-main',
    to: 'binance-sub-a',
  });
  const post = h.calls.find((c) => c.method === 'sapiPostSubAccountUniversalTransfer')!;
  assert.equal(post.params.fromEmail, undefined);
  assert.equal(post.params.toEmail, 'synthetic-sub-a@example.invalid');
  assert.equal((await h.adapter.reconcile(r.record)).status, 'completed');
});
test('accepted transfer followed by timeout recovers from persisted identity without another POST', async () => {
  const h = harness();
  h.state.failTransferResponse = true;
  const preview = await h.adapter.preview('binance.transfer', TRANSFER);
  let saved: Submission | undefined;
  await assert.rejects(
    h.adapter.submit(preview, { persistSubmission: (s) => (saved = structuredClone(s)) }),
    rejectCode('VENUE_UNAVAILABLE'),
  );
  assert.ok(saved);
  assert.equal(saved.data.venueId, undefined);
  const restarted = new BinanceAdapter(
    h.accountConfig,
    { 'binance-main': h.client },
    { now: () => FIXTURE_TIME },
  );
  assert.equal((await restarted.reconcile(operation(preview, saved))).status, 'completed');
  assert.equal(h.state.writes, 1);
  assert.equal((await restarted.reconcile(operation(preview, saved))).status, 'completed');
  assert.equal(h.state.writes, 1);
});
test('successful transfer HTTP response remains pending until destination balance credit is observed', async () => {
  const h = harness();
  const r = await submitted(h, 'binance.transfer', TRANSFER);
  h.state.mainFree = '1';
  const result = await h.adapter.reconcile(r.record);
  assert.equal(result.status, 'pending');
  h.state.transferHistory = [];
  assert.equal((await h.adapter.reconcile(r.record)).status, 'unknown');
  assert.equal(h.state.writes, 1);
});
test('transfer mismatched history is never claimed complete', async () => {
  const h = harness();
  const r = await submitted(h, 'binance.transfer', TRANSFER);
  h.state.transferHistory[0]!.amount = '0.002';
  assert.equal((await h.adapter.reconcile(r.record)).status, 'unknown');
});
test('withdraw preview binds fee, debit and receipt bounds, exact network and named destination without POST', async () => {
  const h = harness();
  const preview = await h.adapter.preview('binance.withdraw', WITHDRAW);
  assert.equal(h.state.writes, 0);
  assert.equal(preview.binding.maximumGrossDebit, '0.0102');
  assert.equal(preview.binding.minimumReceive, '0.0098');
  assert.equal(preview.binding.chainId, 1);
  assert.equal(preview.valuation!.movementUsdMicros, '25001235');
  assert.equal(preview.valuation!.maximumFeeUsdMicros, '500025');
  assert.ok(preview.warnings.some((w) => w.includes('does not specify')));
  assert.deepEqual((preview.binding.params as Input).address, FIXTURE_ADDRESS);
});
test('withdraw fails closed on Travel Rule requirements or unavailable requirement lookup', async () => {
  const h = harness();
  h.state.travelRule.questionnaireCountryCode = 'AE';
  await assert.rejects(
    h.adapter.preview('binance.withdraw', WITHDRAW),
    rejectCode('TRAVEL_RULE_REQUIRED'),
  );
  delete h.client.sapiGetLocalentityQuestionnaireRequirements;
  await assert.rejects(
    h.adapter.preview('binance.withdraw', WITHDRAW),
    rejectCode('TRAVEL_RULE_UNKNOWN'),
  );
  assert.equal(h.state.writes, 0);
});
test('fixed CCXT request fallback checks account Travel Rule before standard withdraw', async () => {
  const h = harness();
  delete h.client.sapiGetLocalentityQuestionnaireRequirements;
  const calls: unknown[][] = [];
  h.client.request = async (...args: unknown[]) => {
    calls.push(args);
    return { questionnaireCountryCode: 'NIL' };
  };
  await h.adapter.preview('binance.withdraw', WITHDRAW);
  assert.deepEqual(calls, [['localentity/questionnaire-requirements', 'sapi', 'GET', {}]]);
  assert.equal(h.state.writes, 0);
});
test('withdraw prerequisite guards reject permissions, balances, precision, fees, minima, disabled and busy networks', async () => {
  const cases: { mutate: (h: ReturnType<typeof harness>) => void; patch?: Input; code: string }[] =
    [
      {
        mutate: (h) => {
          h.state.permissions.enableWithdrawals = false;
        },
        code: 'MISSING_PERMISSION',
      },
      {
        mutate: (h) => {
          h.state.mainFree = '0.0101';
        },
        code: 'INSUFFICIENT_BALANCE',
      },
      { mutate: () => {}, patch: { amount: '0.010000001' }, code: 'INVALID_PRECISION' },
      {
        mutate: (h) => {
          h.state.network.withdrawFee = '0.0003';
        },
        code: 'FEE_BOUND_EXCEEDED',
      },
      {
        mutate: (h) => {
          h.state.network.withdrawMin = '0.02';
        },
        code: 'WITHDRAWAL_LIMIT',
      },
      {
        mutate: (h) => {
          h.state.network.withdrawEnable = false;
        },
        code: 'WITHDRAWAL_DISABLED',
      },
      {
        mutate: (h) => {
          h.state.network.busy = true;
        },
        code: 'WITHDRAWAL_DISABLED',
      },
      {
        mutate: (h) => {
          h.state.network.withdrawTag = true;
        },
        code: 'MEMO_REQUIRED',
      },
      {
        mutate: (h) => {
          h.state.network.denomination = 1_000_000;
        },
        code: 'UNSUPPORTED_DENOMINATION',
      },
      {
        mutate: () => {},
        patch: { minReceiveAmount: '0.0099' },
        code: 'RECEIPT_BOUND_UNSUPPORTED',
      },
      {
        mutate: (h) => {
          h.accountConfig.ethereum.chainId = 10;
        },
        code: 'WRONG_NETWORK',
      },
    ];
  for (const scenario of cases) {
    const h = harness();
    scenario.mutate(h);
    await assert.rejects(
      h.adapter.preview('binance.withdraw', { ...WITHDRAW, ...scenario.patch }),
      rejectCode(scenario.code),
    );
    assert.equal(h.state.writes, 0);
  }
});
test('ERC20 withdrawal requires exact mainnet token contract and base-unit representation', async () => {
  const h = harness();
  h.state.currency.coin = 'USDC';
  h.state.network.coin = 'USDC';
  h.state.network.contractAddress = `0x${'33'.repeat(20)}`;
  await assert.rejects(
    h.adapter.preview('binance.withdraw', { ...WITHDRAW, coin: 'USDC' }),
    rejectCode('ASSET_IDENTITY_MISMATCH'),
  );
  h.state.network.contractAddress = '0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48';
  await assert.rejects(
    h.adapter.preview('binance.withdraw', {
      ...WITHDRAW,
      coin: 'USDC',
      amount: '0.01000001',
      minReceiveAmount: '0.0098',
    }),
    rejectCode('INVALID_PRECISION'),
  );
});
test('missing recipient verification and stale USD values block safe live preparation', async () => {
  const h = harness({ recipientVerifier: undefined });
  await assert.rejects(
    h.adapter.preview('binance.withdraw', WITHDRAW),
    rejectCode('RECEIPT_VERIFIER_MISSING'),
  );
  const stale = harness({
    priceProvider: {
      quote: async () => ({
        usdPerUnit: '2500',
        observedAt: '2026-10-01T09:58:00.000Z',
        source: 'isolated stale fixture',
      }),
    },
  });
  await assert.rejects(
    stale.adapter.preview('binance.transfer', TRANSFER),
    rejectCode('STALE_VALUATION'),
  );
  const missing = harness({ priceProvider: undefined });
  const p = await missing.adapter.preview('binance.transfer', TRANSFER);
  assert.equal(p.valuation, undefined);
  assert.ok(p.warnings.some((w) => w.includes('USD')));
});
test('withdraw records submitted identity before POST and requires venue plus actual recipient and source evidence', async () => {
  const h = harness();
  const preview = await h.adapter.preview('binance.withdraw', WITHDRAW);
  let saved: Submission | undefined;
  const post = h.client.sapiPostCapitalWithdrawApply as (p: Input) => Promise<unknown>;
  h.client.sapiPostCapitalWithdrawApply = async (p) => {
    assert.ok(saved);
    assert.equal(saved.reference, (p as Input).withdrawOrderId);
    assert.equal(saved.data.sourceFreeBefore, '1');
    return post(p as Input);
  };
  const submission = await h.adapter.submit(preview, {
    persistSubmission: (s) => (saved = structuredClone(s)),
  });
  const result = await h.adapter.reconcile(operation(preview, submission));
  assert.equal(result.status, 'completed');
  assert.equal(result.evidence.sourceDebitObserved, '0.01');
  assert.equal(h.receiptCalls[0]!.chainId, 1);
  assert.equal(h.receiptCalls[0]!.recipient, FIXTURE_ADDRESS);
  assert.equal(h.receiptCalls[0]!.confirmations, 2);
  assert.equal(JSON.stringify(result).includes('SYNTHETIC_SECRET'), false);
  assert.equal(JSON.stringify(result).includes('PRIVATE_RAW_MESSAGE'), false);
  assert.equal(h.state.writes, 1);
});
test('accepted withdrawal with response loss recovers original venue/chain evidence without resubmission', async () => {
  const h = harness();
  h.state.failWithdrawResponse = true;
  const preview = await h.adapter.preview('binance.withdraw', WITHDRAW);
  let saved: Submission | undefined;
  await assert.rejects(
    h.adapter.submit(preview, { persistSubmission: (s) => (saved = structuredClone(s)) }),
    rejectCode('VENUE_UNAVAILABLE'),
  );
  assert.ok(saved);
  const result = await h.adapter.reconcile(operation(preview, saved));
  assert.equal(result.status, 'completed');
  assert.equal(h.state.writes, 1);
  assert.equal((await h.adapter.reconcile(operation(preview, saved))).status, 'completed');
  assert.equal(h.state.writes, 1);
});
test('withdraw status and mainnet receipt states remain separate; completed venue alone is insufficient', async () => {
  const h = harness();
  const r = await submitted(h);
  h.state.recipientStatus = 'pending';
  assert.equal((await h.adapter.reconcile(r.record)).status, 'pending');
  h.state.recipientStatus = 'completed';
  h.state.withdrawalHistory[0]!.status = 4;
  assert.equal((await h.adapter.reconcile(r.record)).status, 'pending');
  h.state.withdrawalHistory[0]!.status = 3;
  assert.equal((await h.adapter.reconcile(r.record)).status, 'failed');
  h.state.withdrawalHistory = [];
  assert.equal((await h.adapter.reconcile(r.record)).status, 'unknown');
  assert.equal(h.state.writes, 1);
});
test('reverted payout remains unresolved and replacement payout recovers the same withdrawal identity without replay', async () => {
  const replacementTx = `0x${'33'.repeat(32)}`;
  const verifiedHashes: string[] = [];
  let reverted = true;
  const h = harness({
    recipientVerifier: {
      verify: async (p) => {
        verifiedHashes.push(p.txHash);
        return {
          status: reverted ? 'failed' : 'completed',
          evidence: {
            transactionHash: p.txHash,
            recipient: p.recipient,
            receiptStatus: reverted ? 'reverted' : 'success',
            actualUnits: '9900000000000000',
            confirmations: '2',
          },
        };
      },
    },
  });
  const r = await submitted(h);
  const unresolved = await h.adapter.reconcile(r.record);
  assert.equal(unresolved.status, 'unknown');
  assert.equal(unresolved.evidence.venueState, 'completed');
  assert.equal((unresolved.evidence.recipient as Input).receiptStatus, 'reverted');
  assert.equal(unresolved.evidence.withdrawOrderId, r.submission.reference);
  assert.equal(h.state.writes, 1);
  h.state.withdrawalHistory[0]!.txId = replacementTx;
  reverted = false;
  const recovered = await h.adapter.reconcile({
    ...r.record,
    status: 'unknown',
    evidence: unresolved.evidence,
  });
  assert.equal(recovered.status, 'completed');
  assert.equal(recovered.evidence.withdrawOrderId, r.submission.reference);
  assert.deepEqual(verifiedHashes, [FIXTURE_TX, replacementTx]);
  assert.equal(h.state.writes, 1);
  assert.equal(h.calls.filter((c) => c.method === 'sapiPostCapitalWithdrawApply').length, 1);
  assert.ok(
    h.calls
      .filter((c) => c.method === 'sapiGetCapitalWithdrawHistory')
      .every((c) => c.params.withdrawOrderId === r.submission.reference),
  );
});
test('withdraw reconciliation rejects mismatched identity/recipient/network/internal transfer and fee overrun', async () => {
  for (const patch of [
    { coin: 'USDC' },
    { network: 'BSC' },
    { address: `0x${'44'.repeat(20)}` },
    { transferType: 1 },
    { walletType: 1 },
    { amount: '0.009' },
    { amount: '0.011' },
    { transactionFee: '0.0003' },
  ]) {
    const h = harness();
    const r = await submitted(h);
    Object.assign(h.state.withdrawalHistory[0]!, patch);
    assert.equal((await h.adapter.reconcile(r.record)).status, 'unknown');
    assert.equal(h.state.writes, 1);
  }
});
test('approval-bound destination changes and expired previews fail before financial submission', async () => {
  const h = harness();
  const preview = await h.adapter.preview('binance.withdraw', WITHDRAW);
  h.accountConfig.connections.find((c) => c.name === 'hot-wallet')!.address =
    `0x${'55'.repeat(20)}`;
  await assert.rejects(
    h.adapter.submit(preview, {
      persistSubmission: () => assert.fail('must not persist submission'),
    }),
    rejectCode('PREVIEW_CHANGED'),
  );
  const expired = { ...preview, expiresAt: '2026-10-01T09:59:59.000Z' };
  await assert.rejects(
    h.adapter.submit(expired, {
      persistSubmission: () => assert.fail('must not persist submission'),
    }),
    rejectCode('PREVIEW_EXPIRED'),
  );
  assert.equal(h.state.writes, 0);
});
test('current fee increases beyond approved bound fail submission and persist no action', async () => {
  const h = harness();
  const preview = await h.adapter.preview('binance.withdraw', WITHDRAW);
  h.state.network.withdrawFee = '0.0003';
  await assert.rejects(
    h.adapter.submit(preview, { persistSubmission: () => assert.fail('no submission') }),
    rejectCode('FEE_BOUND_EXCEEDED'),
  );
  assert.equal(h.state.writes, 0);
});
test('deposit instructions and credit lookup are read-only and distinguish credited from locked Travel Rule credit', async () => {
  const h = harness();
  const address = (await h.adapter.read('binance.deposit-address', {
    account: 'binance-main',
    coin: 'ETH',
    network: 'ETH',
  })) as Input;
  assert.equal(address.address, FIXTURE_ADDRESS);
  assert.equal(address.memo, '');
  assert.equal(address.minimumConfirmations, 2);
  h.state.depositHistory = [
    {
      coin: 'ETH',
      network: 'ETH',
      amount: '0.01',
      txId: FIXTURE_TX,
      status: 1,
      travelRuleStatus: 0,
    },
    { coin: 'ETH', amount: '0.02', txId: FIXTURE_TX, status: 6 },
    { coin: 'ETH', amount: '0.01', txId: FIXTURE_TX, status: 1, travelRuleStatus: 1 },
  ];
  const result = (await h.adapter.read('binance.deposit-history', {
    account: 'binance-main',
    coin: 'ETH',
    txHash: FIXTURE_TX,
  })) as Input;
  assert.deepEqual(
    (result.records as Input[]).map((r) => r.creditState),
    ['credited', 'credited_locked', 'pending'],
  );
  assert.equal(h.state.writes, 0);
});
test('capital history pagination queries offsets and signals bounded incomplete scans', async () => {
  const h = harness();
  const fixtureRow = { coin: 'ETH', amount: '0.01', status: 1, txId: FIXTURE_TX };
  const calls: Input[] = [];
  h.client.sapiGetCapitalDepositHisrec = async (p) => {
    calls.push(p as Input);
    return (p as Input).offset === 0
      ? Array.from({ length: 1000 }, () => fixtureRow)
      : [fixtureRow];
  };
  const result = (await h.adapter.read('binance.deposit-history', {
    account: 'binance-main',
  })) as Input;
  assert.equal(result.complete, true);
  assert.equal((result.records as Input[]).length, 1001);
  assert.deepEqual(
    calls.map((c) => c.offset),
    [0, 1000],
  );
  h.client.sapiGetCapitalDepositHisrec = async () => Array.from({ length: 1000 }, () => fixtureRow);
  const incomplete = (await h.adapter.read('binance.deposit-history', {
    account: 'binance-main',
  })) as Input;
  assert.equal(incomplete.complete, false);
});
test('history windows obey endpoint seven-day restriction when filtering client withdrawal identity', async () => {
  const h = harness();
  await assert.rejects(
    h.adapter.read('binance.withdrawal-history', {
      account: 'binance-main',
      withdrawOrderId: 'fixture-request',
      startTime: FIXTURE_TIME.getTime() - 8 * 86_400_000,
      endTime: FIXTURE_TIME.getTime(),
    }),
    rejectCode('INVALID_HISTORY_WINDOW'),
  );
  await h.adapter.read('binance.withdrawal-history', {
    account: 'binance-main',
    withdrawOrderId: 'fixture-request',
  });
  const query = h.calls.at(-1)!.params;
  assert.ok(Number(query.endTime) - Number(query.startTime) < 7 * 86_400_000);
});
test('rate-limit and signed error text are sanitized and never cause automatic financial retries', async () => {
  const h = harness();
  h.state.permissionsFailure = true;
  try {
    await h.adapter.read('binance.permissions', { account: 'binance-main' });
    assert.fail('expected failure');
  } catch (error) {
    assert.ok(error instanceof BinanceAdapterError);
    assert.equal(error.code, 'VENUE_PERMISSION');
    assert.equal(error.message.includes('SYNTHETIC_SECRET'), false);
  }
  h.client.sapiGetCapitalDepositHisrec = async () => {
    throw Object.assign(new Error('SYNTHETIC_SIGNED_URL_SECRET'), { name: 'RateLimitExceeded' });
  };
  await assert.rejects(
    h.adapter.read('binance.deposit-history', { account: 'binance-main' }),
    rejectCode('VENUE_RATE_LIMIT'),
  );
  assert.equal(h.state.writes, 0);
});
test('pinned CCXT implicit methods map to reviewed SAPI endpoints using exact decimal strings', async () => {
  const exchange = new ccxt.binance({ enableRateLimit: false });
  const calls: { path: string; api: string; method: string; params: Input }[] = [];
  exchange.request = async (path: string, api: string, method: string, params: Input) => {
    calls.push({ path, api, method, params });
    return { fixture: true };
  };
  await exchange.sapiPostSubAccountUniversalTransfer({
    amount: '0.00000001',
    clientTranId: 'isolated-fixture-only',
  });
  await exchange.sapiGetSubAccountUniversalTransfer({ clientTranId: 'isolated-fixture-only' });
  await exchange.sapiPostCapitalWithdrawApply({
    amount: '0.00000001',
    withdrawOrderId: 'isolated-fixture-only',
  });
  await exchange.sapiGetCapitalWithdrawHistory({ withdrawOrderId: 'isolated-fixture-only' });
  await exchange.sapiV4GetSubAccountAssets({ email: 'synthetic-sub-a@example.invalid' });
  assert.deepEqual(
    calls.map((c) => [c.path, c.api, c.method]),
    [
      ['sub-account/universalTransfer', 'sapi', 'POST'],
      ['sub-account/universalTransfer', 'sapi', 'GET'],
      ['capital/withdraw/apply', 'sapi', 'POST'],
      ['capital/withdraw/history', 'sapi', 'GET'],
      ['sub-account/assets', 'sapiV4', 'GET'],
    ],
  );
  assert.equal(calls[0]!.params.amount, '0.00000001');
});
test('Binance adapter plus durable engine executes concurrent identical requests exactly once', async () => {
  const h = harness();
  const store = new OperationStore(':memory:');
  let approvals = 0;
  try {
    const engine = new OperationEngine(
      store,
      [h.adapter],
      isolatedAuthorizer(() => approvals++),
      { now: () => FIXTURE_TIME.getTime() },
    );
    const records = await Promise.all(
      Array.from({ length: 5 }, () => engine.execute('binance.transfer', TRANSFER)),
    );
    assert.equal(new Set(records.map((r) => r.id)).size, 1);
    assert.equal(h.state.writes, 1);
    assert.equal(approvals, 1);
    assert.ok(records.every((r) => r.status === 'completed'));
    assert.equal(store.list().length, 1);
    await assert.rejects(
      engine.execute('binance.transfer', { ...TRANSFER, amount: '0.002' }),
      (error) => error instanceof FinError && error.code === 'REQUEST_ID_CONFLICT',
    );
    assert.equal(h.state.writes, 1);
  } finally {
    store.close();
  }
});
test('durable Binance operation with accepted timeout survives database restart without another approval or submission', async () => {
  const directory = mkdtempSync(join(tmpdir(), 'fin-binance-isolated-'));
  const path = join(directory, 'operations.sqlite');
  const h = harness();
  h.state.failWithdrawResponse = true;
  let approvals = 0;
  let store = new OperationStore(path);
  try {
    const engine = new OperationEngine(
      store,
      [h.adapter],
      isolatedAuthorizer(() => approvals++),
      { now: () => FIXTURE_TIME.getTime() },
    );
    const first = await engine.execute('binance.withdraw', WITHDRAW);
    assert.equal(first.status, 'unknown');
    assert.ok(first.submission);
    store.close();
    store = new OperationStore(path);
    store.recoverInterrupted();
    const restarted = new OperationEngine(
      store,
      [h.adapter],
      isolatedAuthorizer(() => approvals++),
      { now: () => FIXTURE_TIME.getTime() },
    );
    const recovered = await restarted.execute('binance.withdraw', WITHDRAW);
    assert.equal(recovered.id, first.id);
    assert.equal(recovered.status, 'completed');
    assert.equal(h.state.writes, 1);
    assert.equal(approvals, 1);
  } finally {
    store.close();
    rmSync(directory, { recursive: true, force: true });
  }
});
test('a rejected or mismatched approval cannot submit Binance funds', async () => {
  for (const mode of ['decline', 'mismatch']) {
    const h = harness();
    const store = new OperationStore(':memory:');
    try {
      const authorizer: Authorizer = {
        approve: async (_p, hash) => {
          if (mode === 'decline')
            throw new FinError(
              'APPROVAL_DECLINED',
              'approval',
              'Synthetic decline',
              'Obtain fresh approval.',
            );
          return {
            mechanism: 'isolated-test-only',
            bindingHash: `${hash}changed`,
            approvedAt: FIXTURE_TIME.toISOString(),
            expiresAt: new Date(FIXTURE_TIME.getTime() + 60_000).toISOString(),
          };
        },
      };
      const engine = new OperationEngine(store, [h.adapter], authorizer, {
        now: () => FIXTURE_TIME.getTime(),
      });
      const result = await engine.execute('binance.withdraw', WITHDRAW);
      assert.equal(result.status, mode === 'decline' ? 'rejected' : 'failed');
      assert.equal(result.submission, undefined);
      assert.equal(h.state.writes, 0);
    } finally {
      store.close();
    }
  }
});
test('partial consolidation retains completed first subaccount and reports unavailable second without compensation or replay', async () => {
  const h = harness();
  const store = new OperationStore(':memory:');
  let approvals = 0;
  try {
    const engine = new OperationEngine(
      store,
      [h.adapter],
      isolatedAuthorizer(() => approvals++),
      { now: () => FIXTURE_TIME.getTime() },
    );
    const first = await engine.execute('binance.transfer', TRANSFER);
    assert.equal(first.status, 'completed');
    h.state.subBFree = '0';
    const second = await engine.execute('binance.transfer', {
      ...TRANSFER,
      requestId: 'isolated-transfer-002',
      from: 'binance-sub-b',
    });
    assert.equal(second.status, 'failed');
    assert.equal(second.error!.code, 'INSUFFICIENT_BALANCE');
    assert.equal((await engine.execute('binance.transfer', TRANSFER)).status, 'completed');
    assert.equal(h.state.writes, 1);
    assert.equal(approvals, 1);
    assert.equal(store.list().length, 2);
    assert.equal(h.state.mainFree, '1.001');
  } finally {
    store.close();
  }
});
test('Binance live-verification engine rejects missing baseline, unknown value, movements above $100 and exhausted unwind reserves before approval', async () => {
  const cases: {
    code: string;
    input: Input;
    plan?: Partial<RunPlan>;
    options?: BinanceOptions;
    noRun?: boolean;
  }[] = [
    { code: 'RUN_REQUIRED', input: WITHDRAW, noRun: true },
    { code: 'VALUATION_UNAVAILABLE', input: WITHDRAW, options: { priceProvider: undefined } },
    { code: 'MOVEMENT_LIMIT', input: { ...WITHDRAW, amount: '0.04', minReceiveAmount: '0.0398' } },
    {
      code: 'UNALLOCATED_FUNDS',
      input: { ...WITHDRAW, amount: '0.038', minReceiveAmount: '0.0378' },
    },
    {
      code: 'UNWIND_RESERVE',
      input: WITHDRAW,
      plan: { initialAllocationUsdMicros: '99000000', costReserveUsdMicros: '500000' },
    },
  ];
  for (const scenario of cases) {
    const h = harness(scenario.options);
    const store = new OperationStore(':memory:');
    let approvals = 0;
    try {
      const guard = new BudgetGuard(store, () => FIXTURE_TIME.getTime());
      const plan: RunPlan = {
        id: 'isolated-live-contract',
        initialAllocationUsdMicros: '95000000',
        borrowReserveUsdMicros: '0',
        costReserveUsdMicros: '5000000',
        returnRoutesVerified: true,
        unwind: ['Synthetic wallet returns ETH to Binance main; venue credit verified.'],
        baseline: {
          observedAt: FIXTURE_TIME.toISOString(),
          quantities: { 'binance-main/balance/ETH': '1000000000000000000' },
          pending: [],
          metadata: { environment: 'isolated fixture only' },
        },
        ...scenario.plan,
      };
      if (!scenario.noRun) guard.begin(plan);
      const engine = new OperationEngine(
        store,
        [h.adapter],
        isolatedAuthorizer(() => approvals++),
        { now: () => FIXTURE_TIME.getTime(), liveTestOnly: true },
      );
      const result = await engine.execute(
        'binance.withdraw',
        scenario.input,
        scenario.noRun ? undefined : plan.id,
      );
      assert.equal(result.status, 'failed');
      assert.equal(result.error!.code, scenario.code);
      assert.equal(h.state.writes, 0);
      assert.equal(approvals, 0);
    } finally {
      store.close();
    }
  }
});
