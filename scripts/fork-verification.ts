/** Isolated integration harness: this script never loads live credentials. */
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir, homedir } from 'node:os';
import { join } from 'node:path';
import assert from 'node:assert/strict';
import { createPublicClient, http, formatUnits, parseUnits, type Hex } from 'viem';
import { privateKeyToAccount, generatePrivateKey } from 'viem/accounts';
import { mainnet } from 'viem/chains';
import { MAINNET } from '../src/config.js';
import { EthereumAdapter, type EthereumTransaction } from '../src/adapters/ethereum.js';
import { WETH, USDC, erc20Abi } from '../src/adapters/ethereum-abi.js';
import { OperationStore } from '../src/core/store.js';
import { OperationEngine } from '../src/core/engine.js';
import { RunCoordinator } from '../src/core/run-coordinator.js';
import { holdingKey } from '../src/core/inventory.js';
import type { TestRun } from '../src/core/budget.js';
import type { FinConfig, Input, RecordOperation } from '../src/core/types.js';

// PublicNode's anonymous endpoint rejects older uncached state; use the official
// public dRPC endpoint for this fixed archive fixture without adding credentials.
const upstream = 'https://eth.drpc.org';
const forkBlock = 26099326n;
const forkHash = '0xe24db6a0bd48918550f6a7c35c0e129172816d1f6641ac16a37459627551f7bf';
const localUrl = 'http://127.0.0.1:18545';
const runTimestamp = new Date().toISOString();
// Ephemeral synthetic fixtures confined to this local Anvil-only process. Random
// addresses avoid upstream EIP-7702 code or state at commonly reused test keys.
const accounts = [
  privateKeyToAccount(generatePrivateKey()),
  privateKeyToAccount(generatePrivateKey()),
];
const anvil = process.env.FIN_ANVIL_PATH ?? join(homedir(), '.foundry/bin/anvil');
const version = spawnSync(anvil, ['--version'], { encoding: 'utf8' });
assert.equal(version.status, 0, 'Install Foundry Anvil or set FIN_ANVIL_PATH to its executable');
const directory = mkdtempSync(join(tmpdir(), 'fin-isolated-fork-'));
const child = spawn(
  anvil,
  [
    '--fork-url',
    upstream,
    '--fork-block-number',
    String(forkBlock),
    '--chain-id',
    '1',
    '--host',
    '127.0.0.1',
    '--port',
    '18545',
    '--accounts',
    '0',
    '--quiet',
    '--timeout',
    '15000',
    '--retries',
    '2',
  ],
  { stdio: 'ignore' },
);
const client = createPublicClient({
  chain: mainnet,
  transport: http(localUrl, { timeout: 60000, retryCount: 0 }),
});
const rpc = async (method: string, params: unknown[] = []) =>
  client.request({ method, params } as never) as Promise<unknown>;
const report: Input = {
  environment: 'isolated local Anvil fork; NOT mainnet live-write acceptance',
  runTimestamp,
  anvilVersion: version.stdout.trim(),
  upstream,
  forkBlock: String(forkBlock),
  forkHash,
  upstreamSelection:
    'Official public dRPC Ethereum endpoint; fixed hash verified. PublicNode anonymous archive access returned 403; no credentials or block repinning were used.',
  setup:
    'Synthetic local signers; anvil_setBalance and deterministic local 12-second block timestamps. No credentials, upstream signatures, impersonation, or production financial movement.',
  approval: 'Injected test-only fake authorizer; NOT Touch ID or credential-isolation evidence',
  cases: [],
};
let store: OperationStore | undefined;
let now = Date.now();
let sequence = 0;
let approvalCount = 0;
const base = { wallet: 'fork-wallet', network: 'ethereum', chainId: 1 };
const runId = 'isolated-fork-ledger-round-trip';
const cases = report.cases as Input[];

async function assertIsolated() {
  assert.equal(new URL(localUrl).hostname, '127.0.0.1');
  const metadata = (await rpc('anvil_metadata')) as {
    forkedNetwork?: { chainId?: number; forkBlockNumber?: number; forkBlockHash?: string };
  };
  assert.ok(metadata.forkedNetwork, 'Signing and funding require an Anvil fork');
  assert.equal(metadata.forkedNetwork.chainId, 1);
  assert.equal(BigInt(metadata.forkedNetwork.forkBlockNumber!), forkBlock);
  assert.equal(metadata.forkedNetwork.forkBlockHash?.toLowerCase(), forkHash);
}
async function refreshClock() {
  now = Number((await client.getBlock()).timestamp) * 1000;
}
async function waitForNode() {
  for (let attempt = 0; attempt < 150; attempt++) {
    if (child.exitCode !== null) throw new Error('Anvil exited before the isolated fork was ready');
    try {
      await assertIsolated();
      return;
    } catch {
      await new Promise((resolve) => setTimeout(resolve, 200));
    }
  }
  throw new Error('Isolated Anvil startup or pinned upstream fork unavailable');
}
const tokenBalance = (token: Hex, account = accounts[0]!.address) =>
  client.readContract({
    address: token,
    abi: erc20Abi,
    functionName: 'balanceOf',
    args: [account],
  });

try {
  await waitForNode();
  assert.equal((await client.getBlock({ blockNumber: forkBlock })).hash, forkHash);
  await rpc('anvil_setBalance', [
    accounts[0]!.address,
    `0x${parseUnits('0.024', 18).toString(16)}`,
  ]);
  await rpc('anvil_setBalance', [
    accounts[1]!.address,
    `0x${parseUnits('0.001', 18).toString(16)}`,
  ]);
  await refreshClock();
  const config: FinConfig = {
    version: 1,
    connections: accounts.map((account, i) => ({
      name: i === 0 ? 'fork-wallet' : 'fork-recipient',
      kind: 'ethereum',
      address: account.address,
      credential: `isolated-local-synthetic-signer-${i}`,
    })),
    ethereum: { ...MAINNET, confirmations: 1, rpcUrls: [localUrl] },
  };
  const adapter = new EthereumAdapter(config, {
    publicClient: client,
    now: () => new Date(now),
    signTransaction: async (tx: EthereumTransaction, wallet: string) => {
      await assertIsolated();
      const signer = accounts[wallet === 'fork-wallet' ? 0 : 1]!;
      assert.equal(tx.account, signer.address);
      return signer.signTransaction({
        chainId: 1,
        type: 'eip1559',
        to: tx.to,
        data: tx.data,
        value: BigInt(tx.value),
        gas: BigInt(tx.gas),
        maxFeePerGas: BigInt(tx.maxFeePerGas),
        maxPriorityFeePerGas: BigInt(tx.maxPriorityFeePerGas),
        nonce: tx.nonce,
      });
    },
  });
  store = new OperationStore(join(directory, 'operations.sqlite'));
  const runs = new RunCoordinator(config, [adapter], store, { now: () => now });
  const engine = new OperationEngine(
    store,
    [adapter],
    {
      approve: async (_preview, bindingHash) => {
        approvalCount++;
        return {
          mechanism: 'isolated-fork-mock',
          bindingHash,
          approvedAt: new Date(now).toISOString(),
          expiresAt: new Date(now + 60000).toISOString(),
        };
      },
    },
    {
      liveTestOnly: true,
      now: () => now,
      revalueAllocation: (id) => runs.revalueAllocation(id),
      enrichReconciliation: (record) => runs.enrichReconciliation(record),
    },
  );
  async function reject(operation: string, input: Input, expectedCode: string, label: string) {
    await refreshClock();
    const nonce = await client.getTransactionCount({
      address: accounts[0]!.address,
      blockTag: 'pending',
    });
    const previousApprovals = approvalCount;
    const result = await engine.execute(
      operation,
      { ...base, requestId: `isolated-fork-negative-${++sequence}`, maxFeeEth: '0.001', ...input },
      runId,
    );
    assert.equal(result.status, 'failed');
    assert.equal(result.error?.code, expectedCode);
    assert.equal(result.submission, undefined);
    assert.equal(approvalCount, previousApprovals, 'Guard failure must precede approval');
    assert.equal(
      await client.getTransactionCount({ address: accounts[0]!.address, blockTag: 'pending' }),
      nonce,
      'Guard failure must not broadcast',
    );
    assert.deepEqual(
      Object.keys(store!.getRun<TestRun>(runId)!.inventory!.inFlight),
      [],
      'Rejected request cannot strand an inventory reservation',
    );
    cases.push({
      label,
      operation,
      outcome: 'expected guard rejection passed',
      code: expectedCode,
    });
    process.stdout.write(
      JSON.stringify({ label, outcome: 'expected guard rejection passed', code: expectedCode }) +
        '\n',
    );
  }
  async function execute(
    operation: string,
    input: Input,
    label = operation,
  ): Promise<RecordOperation> {
    await refreshClock();
    await rpc('evm_setNextBlockTimestamp', [Math.floor(now / 1000) + 12]);
    const request = {
      ...base,
      requestId: `isolated-fork-${++sequence}`,
      maxFeeEth: '0.001',
      ...input,
    };
    const preview = await engine.preview(operation, request);
    // Also constrain local synthetic tests to realistic $100 per operation valuations.
    assert.ok(
      BigInt(preview.valuation!.movementUsdMicros) +
        BigInt(preview.valuation!.maximumFeeUsdMicros) <=
        100_000_000n,
    );
    let record = await engine.execute(operation, request, runId);
    for (let attempt = 0; attempt < 10 && record.status === 'pending'; attempt++) {
      await new Promise((resolve) => setTimeout(resolve, 300));
      record = await engine.status(record.id, true);
    }
    cases.push({
      label,
      operation,
      status: record.status,
      transactionHash: record.submission?.reference,
      evidence: record.evidence,
      error: record.error,
    });
    process.stdout.write(
      JSON.stringify({ label, status: record.status, code: record.error?.code }) + '\n',
    );
    assert.equal(
      record.status,
      'completed',
      `${label}: ${record.error?.code ?? JSON.stringify(record.evidence)}`,
    );
    const run = store!.getRun<TestRun>(runId)!;
    assert.deepEqual(
      Object.keys(run.inventory!.inFlight),
      [],
      `${label}: inventory reservation did not settle ${JSON.stringify(run.inventory!.inFlight)}`,
    );
    assert.ok(
      run.inventory!.settled.includes(record.id),
      `${label}: operation did not settle in the test inventory`,
    );
    const duplicate = await engine.execute(operation, request, runId);
    assert.equal(duplicate.id, record.id);
    assert.equal(duplicate.submission?.reference, record.submission?.reference);
    return record;
  }
  const baseline = await adapter.read('wallet.balances', { ...base, tokens: [WETH, USDC] });
  report.baseline = baseline;
  report.baselineRecipient = await adapter.read('wallet.balances', {
    ...base,
    wallet: 'fork-recipient',
    tokens: [WETH, USDC],
  });
  report.baselinePositions = await adapter.read('aave.positions', {
    ...base,
    tokens: [WETH, USDC],
  });
  report.initialSyntheticAllocationUsdMicros = (
    await adapter.quoteUsd('native', parseUnits('0.025', 18))
  ).valueUsdMicros;
  assert.ok(BigInt(String(report.initialSyntheticAllocationUsdMicros)) + 3_100_000n < 100_000_000n);
  report.runPlan = await runs.begin({
    id: runId,
    allocations: {
      [holdingKey('fork-wallet', 'native')]: parseUnits('0.024', 18).toString(),
      [holdingKey('fork-recipient', 'native')]: parseUnits('0.001', 18).toString(),
    },
    borrowReserveUsdMicros: '4000000',
    costReserveUsdMicros: '30000000',
    wallets: ['fork-wallet', 'fork-recipient'],
    accounts: [],
    returnRoutes: [],
    unwind: [
      'Return native and USDC transfers to their starting wallets, preserving each wallet gas allocation.',
      'Repay all accrued USDC variable debt within a finite 3.01-USDC Pool allowance funded by the forward swap and borrow proceeds.',
      'Withdraw WETH principal and refresh separately approved exact supply-dust cleanup until the supplied and debt positions are zero.',
      'Reverse swap remaining test USDC into WETH, restore all four protocol allowances to zero, and unwrap all test WETH.',
      'Verify both wallet allocations, holdings, Aave debt/supply/collateral, allowances, inventory settlement and native cost reconciliation.',
    ],
  });
  await reject(
    'wallet.transfer',
    {
      token: 'native',
      amount: '0.0001',
      recipient: privateKeyToAccount(generatePrivateKey()).address,
    },
    'INVENTORY_SCOPE',
    'untracked recipient rejected before approval',
  );
  await reject(
    'wallet.approve',
    { token: WETH, spender: MAINNET.uniswapRouter, amount: '0.005' },
    'INVENTORY_AUTHORITY',
    'authority outside current selected holdings rejected before approval',
  );
  await reject(
    'wallet.transfer',
    { token: 'native', amount: '0.02', recipient: accounts[1]!.address },
    'UNALLOCATED_FUNDS',
    'movement invading funded cost reserve rejected before approval',
  );
  await adapter.read('aave.reserves', { ...base, tokens: [WETH, USDC] });
  await execute(
    'wallet.transfer',
    { token: 'native', amount: '0.0001', recipient: accounts[1]!.address },
    'native transfer',
  );
  await execute(
    'wallet.transfer',
    {
      wallet: 'fork-recipient',
      token: 'native',
      amount: '0.0001',
      recipient: accounts[0]!.address,
    },
    'native transfer return',
  );
  await execute('wallet.wrap', { amount: '0.012' });
  await execute(
    'wallet.approve',
    { token: WETH, spender: MAINNET.uniswapRouter, amount: '0.002' },
    'WETH router authorization',
  );
  const quote = (await adapter.read('uniswap.quote', {
    ...base,
    tokenIn: WETH,
    tokenOut: USDC,
    amount: '0.002',
  })) as { data: { output: { units: string }; feeTier: number } };
  await execute(
    'uniswap.swap',
    {
      tokenIn: WETH,
      tokenOut: USDC,
      amount: '0.002',
      feeTier: quote.data.feeTier,
      recipient: accounts[0]!.address,
      minOutput: formatUnits((BigInt(quote.data.output.units) * 995n) / 1000n, 6),
      slippageBps: 50,
      deadline: Math.floor(now / 1000) + 120,
    },
    'WETH to USDC',
  );
  await execute(
    'wallet.transfer',
    { token: USDC, amount: '0.1', recipient: accounts[1]!.address },
    'ERC20 transfer',
  );
  await execute(
    'wallet.transfer',
    { wallet: 'fork-recipient', token: USDC, amount: '0.1', recipient: accounts[0]!.address },
    'ERC20 transfer return',
  );
  await execute(
    'wallet.approve',
    { token: WETH, spender: MAINNET.aavePool, amount: '0.008' },
    'WETH Pool authorization',
  );
  await execute('aave.supply', { token: WETH, amount: '0.008' });
  await execute('aave.collateral', { token: WETH, enabled: false }, 'collateral disable');
  await execute('aave.collateral', { token: WETH, enabled: true }, 'collateral enable');
  await execute('aave.borrow', { token: USDC, amount: '3', minHealthFactor: '1.5' });
  await execute(
    'wallet.approve',
    { token: USDC, spender: MAINNET.aavePool, amount: '3.01' },
    'bounded USDC repayment authorization',
  );
  await execute(
    'aave.repay',
    { token: USDC, amount: 'all', maxAmount: '3.01' },
    'repay refreshed accrued debt',
  );
  await execute(
    'aave.withdraw',
    { token: WETH, amount: '0.008', recipient: accounts[0]!.address },
    'withdraw supplied principal',
  );
  for (let attempt = 0; attempt < 3; attempt++) {
    const positions = (await adapter.read('aave.positions', { ...base, tokens: [WETH] })) as {
      data: { positions: Array<{ supplied: { units: string } }> };
    };
    const remaining = BigInt(positions.data.positions[0]!.supplied.units);
    if (remaining === 0n) break;
    await execute(
      'aave.withdraw',
      { token: WETH, amount: formatUnits(remaining, 18), recipient: accounts[0]!.address },
      `separately authorized exact supply-dust cleanup ${attempt + 1}`,
    );
  }
  const remainder = await tokenBalance(USDC);
  await execute(
    'wallet.approve',
    { token: USDC, spender: MAINNET.uniswapRouter, amount: formatUnits(remainder, 6) },
    'USDC reverse-swap authorization',
  );
  const reverse = (await adapter.read('uniswap.quote', {
    ...base,
    tokenIn: USDC,
    tokenOut: WETH,
    amount: formatUnits(remainder, 6),
  })) as { data: { output: { units: string }; feeTier: number } };
  await execute(
    'uniswap.swap',
    {
      tokenIn: USDC,
      tokenOut: WETH,
      amount: formatUnits(remainder, 6),
      feeTier: reverse.data.feeTier,
      recipient: accounts[0]!.address,
      minOutput: formatUnits((BigInt(reverse.data.output.units) * 995n) / 1000n, 18),
      slippageBps: 50,
      deadline: Math.floor(now / 1000) + 120,
    },
    'USDC reverse swap',
  );
  for (const token of [WETH, USDC])
    for (const spender of [MAINNET.aavePool, MAINNET.uniswapRouter])
      await execute('wallet.approve', { token, spender, amount: '0' }, 'restore zero allowance');
  await execute('wallet.unwrap', { amount: formatUnits(await tokenBalance(WETH), 18) });
  const finalPositions = (await adapter.read('aave.positions', {
    ...base,
    tokens: [WETH, USDC],
  })) as {
    data: {
      positions: Array<{
        token: { symbol: string };
        supplied: { units: string };
        variableDebt: { units: string };
      }>;
    };
  };
  report.finalPositions = finalPositions;
  report.finalBalances = await adapter.read('wallet.balances', { ...base, tokens: [WETH, USDC] });
  report.finalRecipient = await adapter.read('wallet.balances', {
    ...base,
    wallet: 'fork-recipient',
    tokens: [WETH, USDC],
  });
  for (const account of accounts) {
    assert.equal(
      await tokenBalance(WETH, account.address),
      0n,
      'All test WETH must be unwrapped or returned',
    );
    assert.equal(
      await tokenBalance(USDC, account.address),
      0n,
      'All test USDC must be returned through the reverse swap',
    );
  }
  const allowances: Input[] = [];
  for (const token of [WETH, USDC])
    for (const spender of [MAINNET.aavePool, MAINNET.uniswapRouter]) {
      const remaining = await client.readContract({
        address: token,
        abi: erc20Abi,
        functionName: 'allowance',
        args: [accounts[0]!.address, spender as Hex],
      });
      assert.equal(remaining, 0n, 'Every test-modified allowance must return to zero');
      allowances.push({ token, spender, remainingUnits: remaining.toString() });
    }
  report.finalAllowances = allowances;
  report.operationCount = store.list().length;
  report.completedWrites = store
    .list()
    .filter((operation) => operation.status === 'completed').length;
  report.expectedRejectedWrites = store
    .list()
    .filter((operation) => operation.status === 'failed' && !operation.submission).length;
  report.approvalCount = approvalCount;
  report.gasCostWei = store
    .list()
    .reduce((total, op) => total + BigInt(String(op.evidence?.feeWei ?? '0')), 0n)
    .toString();
  const gasByWallet = Object.fromEntries(
    ['fork-wallet', 'fork-recipient'].map((wallet) => [
      wallet,
      store!
        .list()
        .filter((op) => op.input.wallet === wallet)
        .reduce((total, op) => total + BigInt(String(op.evidence?.feeWei ?? '0')), 0n),
    ]),
  );
  const sourceNativeCost =
    parseUnits('0.024', 18) - (await client.getBalance({ address: accounts[0]!.address }));
  const recipientNativeCost =
    parseUnits('0.001', 18) - (await client.getBalance({ address: accounts[1]!.address }));
  assert.equal(
    recipientNativeCost,
    gasByWallet['fork-recipient'],
    'Recipient return allocation must reconcile exactly net gas',
  );
  report.reconciledCosts = {
    gasByWalletWei: Object.fromEntries(
      Object.entries(gasByWallet).map(([wallet, amount]) => [wallet, amount.toString()]),
    ),
    sourceNativeDebitWei: sourceNativeCost.toString(),
    recipientNativeDebitWei: recipientNativeCost.toString(),
    sourceSwapAndNetInterestCostWei: (sourceNativeCost - gasByWallet['fork-wallet']!).toString(),
    explanation:
      'Native recipient net debit equals its own verified gas exactly; source residual is the actual reverse-swap and net Aave interest cost, after all tokens/debt/supply/allowances return to zero.',
  };
  assert.ok(
    finalPositions.data.positions.every((position) => position.variableDebt.units === '0'),
    'Variable debt must be zero',
  );
  const dust = finalPositions.data.positions.filter((position) => position.supplied.units !== '0');
  await refreshClock();
  const finished = await runs.finish(runId);
  report.runFinish = finished;
  assert.deepEqual(
    Object.keys(finished.inventory!.inFlight),
    [],
    'All real test-inventory reservations must be settled',
  );
  if (finished.status !== 'restored') process.exitCode = 2;
  assert.equal(
    finished.status,
    'restored',
    `Coordinator restoration must pass: ${JSON.stringify(finished.differences)}`,
  );
  report.protocolRegression = 'passed';
  report.restoration = dust.length
    ? {
        status: 'incomplete',
        reason:
          'Exact bounded withdrawals still leave accrued Aave supply dust. No complete round-trip/live-acceptance pass is claimed.',
        remainingSupplied: dust,
      }
    : {
        status:
          'restored synthetic account allocations, holdings, positions and allowances net recorded costs',
      };
  if (dust.length) process.exitCode = 2;
  process.stdout.write(
    JSON.stringify({
      protocolRegression: report.protocolRegression,
      restoration: report.restoration,
      operationCount: report.operationCount,
    }) + '\n',
  );
} catch (error) {
  report.protocolRegression = 'failed';
  report.failure =
    error instanceof Error
      ? { name: error.name, message: error.message }
      : { message: 'Unknown isolated harness failure' };
  process.stderr.write(JSON.stringify({ failed: report.failure }) + '\n');
  if (process.exitCode !== 2) process.exitCode = 1;
} finally {
  const archived = join(
    process.cwd(),
    '.context',
    'test-runs',
    `fork-${runTimestamp.replace(/[:.]/g, '-')}.json`,
  );
  const output = join(process.cwd(), '.context', 'fork-verification.json');
  try {
    mkdirSync(join(process.cwd(), '.context', 'test-runs'), { recursive: true, mode: 0o700 });
    writeFileSync(archived, JSON.stringify(report, null, 2) + '\n', { mode: 0o600, flag: 'wx' });
    writeFileSync(output, JSON.stringify({ ...report, archivedReport: archived }, null, 2) + '\n', {
      mode: 0o600,
    });
    process.stdout.write(`Report: ${archived}\n`);
  } catch {
    process.stderr.write('Could not save the optional ignored fork report.\n');
  }
  store?.close();
  if (child.exitCode === null) {
    const exited = new Promise<void>((resolve) => child.once('exit', () => resolve()));
    child.kill('SIGTERM');
    await Promise.race([exited, new Promise((resolve) => setTimeout(resolve, 2000))]);
    if (child.exitCode === null) child.kill('SIGKILL');
  }
  rmSync(directory, { recursive: true, force: true });
}
