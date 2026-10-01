import { test } from 'node:test';
import assert from 'node:assert/strict';
import { privateKeyToAccount } from 'viem/accounts';
import {
  encodeAbiParameters,
  encodeEventTopics,
  decodeFunctionResult,
  formatUnits,
  keccak256,
  maxUint256,
  parseTransaction,
  type Hex,
  type Abi,
  type TransactionSerializableEIP1559,
} from 'viem';
import { EthereumAdapter, exactUnits, type EthereumTransaction } from '../src/adapters/ethereum.js';
import { WETH, USDC, erc20Abi, poolAbi, aTokenAbi } from '../src/adapters/ethereum-abi.js';
import type { FinConfig, Input, Preview, RecordOperation, Submission } from '../src/core/types.js';

// Unmistakably synthetic signing fixture. All clients in this file are local mocks;
// no transport exists and this key is never an accepted configured live account.
const syntheticAccount = privateKeyToAccount(`0x${'11'.repeat(32)}`);
const recipient = '0x0000000000000000000000000000000000000001';
const pool = '0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2';
const aToken = '0x0000000000000000000000000000000000000010';
const debtToken = '0x0000000000000000000000000000000000000011';
const provider = '0x0000000000000000000000000000000000000012';
const dataProvider = '0x0000000000000000000000000000000000000013';
const poolAddress = '0x0000000000000000000000000000000000000014';
const blockHash = `0x${'ab'.repeat(32)}` as Hex;
const config: FinConfig = {
  version: 1,
  connections: [{ name: 'test-wallet', kind: 'ethereum', address: syntheticAccount.address }],
  ethereum: {
    rpcUrls: [],
    chainId: 1,
    confirmations: 2,
    uniswapRouter: '0xE592427A0AEce92De3Edee1F18E0157C05861564',
    uniswapQuoter: '0x61fFE014bA17989E743c5F6cB21bF9697530B21e',
    uniswapFactory: '0x1F98431c8aD98523631AE4a59f267346ea31F984',
    aavePool: pool,
    aaveOracle: '0x54586bE62E3c3580375aE3723C145253060Ca0C2',
  },
};
const base = { wallet: 'test-wallet', network: 'ethereum', chainId: 1 };
const write = { ...base, requestId: 'synthetic-request', maxFeeEth: '0.001' };
const yes = `0x${'0'.repeat(63)}1`;

function fixture() {
  const state = {
    chainId: 1,
    nonce: 5,
    gas: 50000n,
    maxFeePerGas: 2_000_000_000n,
    priority: 1_000_000_000n,
    native: 10n ** 18n,
    simulatedFees: [] as Array<{ gas: bigint; maxFeePerGas: bigint; maxPriorityFeePerGas: bigint }>,
    signedTransactions: [] as EthereumTransaction[],
    allowance: 100_000_000n,
    stableDebt: 0n,
    providerResolutions: 0,
    readFunctions: [] as string[],
    debt: 1_000_000n,
    supplied: 1_000_000_000_000_000_000n,
    aTokenRevision: 5n,
    scaledSupplied: 1_000_000_000_000_000_000n,
    previousSupplyIndex: 10n ** 27n,
    normalizedIncome: 10n ** 27n,
    active: true,
    frozen: false,
    paused: false,
    borrowing: true,
    price: 99_700_000n,
    liquidity: 100_000_000_000_000_000_000n,
    supplyCap: 0n,
    borrowCap: 0n,
    totalSupply: 1_000_000_000_000_000_000n,
    totalDebt: 0n,
    head: 101n,
    receipt: undefined as Record<string, unknown> | undefined,
    tx: undefined as Record<string, unknown> | undefined,
    callData: yes as Hex,
    broadcastFailure: false,
    broadcasts: 0,
    signatures: 0,
    quotes: [] as number[],
    clock: new Date('2026-10-01T12:00:00Z'),
    priceAgeSeconds: 60n,
    capFeed: false,
  };
  const rpc = {
    getChainId: async () => state.chainId,
    getBytecode: async ({ address }: { address: string }) =>
      address.toLowerCase() === recipient.toLowerCase() ? '0x' : '0x6001',
    readContract: async ({
      address,
      functionName,
      args = [],
    }: {
      address: string;
      functionName: string;
      args?: unknown[];
      abi?: Abi;
      blockNumber?: bigint;
    }) => {
      state.readFunctions.push(functionName);
      const usdc = address.toLowerCase() === USDC.toLowerCase();
      if (functionName === 'decimals') return usdc ? 6 : 18;
      if (functionName === 'symbol') return usdc ? 'USDC' : 'WETH';
      if (functionName === 'balanceOf')
        return String(args[0]).toLowerCase() === aToken.toLowerCase()
          ? state.liquidity
          : usdc
            ? 100_000_000n
            : 10n ** 18n;
      if (functionName === 'allowance') return state.allowance;
      if (functionName === 'ATOKEN_REVISION') return state.aTokenRevision;
      if (functionName === 'scaledBalanceOf') return state.scaledSupplied;
      if (functionName === 'getPreviousIndex') return state.previousSupplyIndex;
      if (functionName === 'getReserveNormalizedIncome') return state.normalizedIncome;
      if (functionName === 'totalSupply')
        return address.toLowerCase() === debtToken.toLowerCase()
          ? state.totalDebt
          : state.totalSupply;
      if (functionName === 'BASE_CURRENCY') return '0x0000000000000000000000000000000000000000';
      if (functionName === 'BASE_CURRENCY_UNIT') return 100_000_000n;
      if (functionName === 'getAssetPrice')
        return String(args[0]).toLowerCase() === WETH.toLowerCase()
          ? 2000n * 100_000_000n
          : state.price;
      if (functionName === 'getSourceOfAsset') return provider;
      if (functionName === 'ASSET_TO_USD_AGGREGATOR') return dataProvider;
      if (functionName === 'latestRoundData') {
        if (state.capFeed && address.toLowerCase() === provider.toLowerCase())
          throw new Error('Synthetic capped adapter has no direct round data');
        return [
          1n,
          99_700_000n,
          0n,
          BigInt(Math.floor(state.clock.getTime() / 1000)) - state.priceAgeSeconds,
          1n,
        ];
      }
      if (functionName === 'ADDRESSES_PROVIDER') {
        state.providerResolutions++;
        return provider;
      }
      if (functionName === 'getPool')
        return address.toLowerCase() === provider.toLowerCase() ? pool : poolAddress;
      if (functionName === 'getPriceOracle') return config.ethereum.aaveOracle;
      if (functionName === 'getPoolDataProvider') return dataProvider;
      if (functionName === 'getReserveConfigurationData')
        return [
          usdc ? 6n : String(args[0]).toLowerCase() === USDC.toLowerCase() ? 6n : 18n,
          8000n,
          8500n,
          10500n,
          1000n,
          true,
          state.borrowing,
          false,
          state.active,
          state.frozen,
        ];
      if (functionName === 'getReserveTokensAddresses')
        return [aToken, '0x0000000000000000000000000000000000000000', debtToken];
      if (functionName === 'getReserveCaps') return [state.borrowCap, state.supplyCap];
      if (functionName === 'getPaused') return state.paused;
      if (functionName === 'getUserReserveData')
        return [state.supplied, state.stableDebt, state.debt, 0n, state.debt, 0n, 0n, 0n, true];
      if (functionName === 'getUserAccountData')
        return [1_000_000_000_000n, 0n, 100_000_000_000n, 8500n, 8000n, maxUint256];
      if (functionName === 'getReservesList') return [WETH, USDC];
      throw new Error(`Unexpected test read ${functionName}`);
    },
    simulateContract: async ({ args }: { args: Array<{ fee: number }> }) => {
      const fee = args[0]!.fee;
      state.quotes.push(fee);
      return { result: [BigInt(fee === 500 ? 2_000_000 : 1_900_000), 0n, 0, 50000n] };
    },
    getBalance: async () => state.native,
    estimateGas: async () => state.gas,
    estimateFeesPerGas: async () => ({
      maxFeePerGas: state.maxFeePerGas,
      maxPriorityFeePerGas: state.priority,
    }),
    call: async (fees: { gas: bigint; maxFeePerGas: bigint; maxPriorityFeePerGas: bigint }) => {
      state.simulatedFees.push(fees);
      return { data: state.callData };
    },
    getTransactionCount: async () => state.nonce,
    sendRawTransaction: async ({ serializedTransaction }: { serializedTransaction: Hex }) => {
      state.broadcasts++;
      if (state.broadcastFailure) throw new Error('Synthetic RPC timeout');
      return keccak256(serializedTransaction);
    },
    getTransactionReceipt: async () => {
      if (!state.receipt) {
        const e = new Error('Synthetic missing receipt');
        e.name = 'TransactionReceiptNotFoundError';
        throw e;
      }
      return state.receipt;
    },
    getTransaction: async () => state.tx,
    getBlockNumber: async () => state.head,
    getBlock: async ({ blockNumber }: { blockNumber: bigint }) => ({
      hash: blockHash,
      number: blockNumber,
    }),
  };
  const signTransaction = async (tx: EthereumTransaction) => {
    state.signatures++;
    state.signedTransactions.push(tx);
    return syntheticAccount.signTransaction({
      chainId: tx.chainId,
      to: tx.to,
      data: tx.data,
      value: BigInt(tx.value),
      gas: BigInt(tx.gas),
      nonce: tx.nonce,
      maxFeePerGas: BigInt(tx.maxFeePerGas),
      maxPriorityFeePerGas: BigInt(tx.maxPriorityFeePerGas),
      type: 'eip1559',
    });
  };
  const adapter = new EthereumAdapter(config, {
    publicClient: rpc,
    signTransaction,
    now: () => state.clock,
  });
  return { state, rpc, adapter, signTransaction };
}
function record(preview: Preview, submission?: Submission): RecordOperation {
  return {
    id: 'synthetic-operation',
    requestId: 'synthetic-request',
    fingerprint: 'synthetic',
    operation: preview.operation,
    input: preview.request,
    status: 'submitted',
    createdAt: preview.observedAt,
    updatedAt: preview.observedAt,
    preview,
    submission,
  };
}
function confirmed(f: ReturnType<typeof fixture>, p: Preview, logs: unknown[] = []) {
  const hash = `0x${'01'.repeat(32)}`;
  f.state.receipt = {
    transactionHash: hash,
    from: p.binding.account,
    to: p.binding.to,
    status: 'success',
    blockNumber: 100n,
    blockHash,
    transactionIndex: 0,
    gasUsed: 21000n,
    effectiveGasPrice: 1_000_000_000n,
    logs,
  };
  f.state.tx = {
    hash,
    blockNumber: 100n,
    blockHash,
    transactionIndex: 0,
    type: 'eip1559',
    chainId: 1,
    from: p.binding.account,
    to: p.binding.to,
    input: p.binding.data,
    value: BigInt(String(p.binding.value)),
    nonce: p.binding.nonce,
    gas: BigInt(String(p.binding.gas)),
    maxFeePerGas: BigInt(String(p.binding.maxFeePerGas)),
    maxPriorityFeePerGas: BigInt(String(p.binding.maxPriorityFeePerGas)),
    gasPrice: 1_000_000_000n,
    accessList: [],
  };
  return { kind: 'ethereum', reference: hash, data: { nonce: p.binding.nonce } } as Submission;
}
function transferLog(token: string, from: string, to: string, units: bigint) {
  return {
    address: token,
    data: encodeAbiParameters([{ type: 'uint256' }], [units]),
    topics: encodeEventTopics({
      abi: erc20Abi,
      eventName: 'Transfer',
      args: { from: from as Hex, to: to as Hex },
    }),
  };
}
async function suppliedFixture(
  options: {
    underlying?: bigint;
    scaledBefore?: bigint;
    mintIndex?: bigint;
    previousIndex?: bigint;
    normalizedIncome?: bigint;
  } = {},
) {
  const f = fixture();
  const ray = 10n ** 27n;
  const underlying = options.underlying ?? 101n;
  const scaledBefore = options.scaledBefore ?? 0n;
  const mintIndex = options.mintIndex ?? (11n * ray) / 10n;
  const previousIndex = options.previousIndex ?? (scaledBefore === 0n ? 0n : ray);
  const normalizedIncome = options.normalizedIncome ?? mintIndex;
  const scaledMinted = (underlying * ray) / mintIndex;
  const scaledAfter = scaledBefore + scaledMinted;
  const beforeAtMint = (scaledBefore * mintIndex) / ray;
  const previousBalance = (scaledBefore * previousIndex) / ray;
  const totalAtMint = (scaledAfter * mintIndex) / ray;
  const suppliedAfter = (scaledAfter * normalizedIncome) / ray;
  const mintValue = totalAtMint - previousBalance;
  const balanceIncrease = beforeAtMint - previousBalance;
  f.state.scaledSupplied = scaledBefore;
  f.state.supplied = previousBalance;
  f.state.debt = 0n;
  const p = await f.adapter.preview('aave.supply', {
    ...write,
    token: WETH,
    amount: formatUnits(underlying, 18),
  });
  const historyReads: Array<{ functionName: string; blockNumber?: bigint }> = [];
  const original = f.rpc.readContract;
  f.rpc.readContract = async (params) => {
    if (
      [
        'scaledBalanceOf',
        'getPreviousIndex',
        'getReserveNormalizedIncome',
        'ATOKEN_REVISION',
      ].includes(params.functionName)
    )
      historyReads.push(params);
    if (params.functionName === 'scaledBalanceOf')
      return params.blockNumber === 99n ? scaledBefore : scaledAfter;
    if (params.functionName === 'getPreviousIndex')
      return params.blockNumber === 99n ? previousIndex : mintIndex;
    if (params.functionName === 'getReserveNormalizedIncome') return normalizedIncome;
    return original(params);
  };
  f.state.supplied = suppliedAfter;
  const mintLog = {
    address: aToken,
    data: encodeAbiParameters(
      [{ type: 'uint256' }, { type: 'uint256' }, { type: 'uint256' }],
      [mintValue, balanceIncrease, mintIndex],
    ),
    topics: encodeEventTopics({
      abi: aTokenAbi,
      eventName: 'Mint',
      args: { caller: syntheticAccount.address, onBehalfOf: syntheticAccount.address },
    }),
  };
  const supplyLog = {
    address: pool,
    data: encodeAbiParameters(
      [{ type: 'address' }, { type: 'uint256' }],
      [syntheticAccount.address, underlying],
    ),
    topics: encodeEventTopics({
      abi: poolAbi,
      eventName: 'Supply',
      args: { reserve: WETH, onBehalfOf: syntheticAccount.address, referralCode: 0 },
    }),
  };
  const s = confirmed(f, p, [
    supplyLog,
    transferLog(WETH, syntheticAccount.address, aToken, underlying),
    transferLog(
      aToken,
      '0x0000000000000000000000000000000000000000',
      syntheticAccount.address,
      mintValue,
    ),
    mintLog,
  ]);
  return { ...f, p, s, historyReads, mintLog, supplyLog };
}

test('exact decimal parsing never silently rounds', () => {
  assert.equal(exactUnits('1.000001', 6), 1_000_001n);
  for (const s of ['1.0000001', '1e3', '-1', '01', 'NaN']) assert.throws(() => exactUnits(s, 6));
});
test('wallet discovery preserves token units and does not sign', async () => {
  const f = fixture();
  const read = (await f.adapter.read('wallet.balances', { ...base, tokens: [USDC] })) as {
    data: { holdings: Array<{ units: string; decimals: number }> };
  };
  assert.equal(read.data.holdings[1]?.units, '100000000');
  assert.equal(read.data.holdings[1]?.decimals, 6);
  assert.equal(f.state.signatures, 0);
});
test('legacy MKR bytes32 symbol decodes as an optional label without changing exact asset identity', async () => {
  const f = fixture();
  const mkr = '0x9f8F72aA9304c8B593d555F12eF6589cC3A579A2';
  const original = f.rpc.readContract;
  let symbolReads = 0;
  f.rpc.readContract = async (params) => {
    if (params.address.toLowerCase() === mkr.toLowerCase() && params.functionName === 'symbol') {
      symbolReads++;
      return decodeFunctionResult({
        abi: params.abi!,
        functionName: 'symbol',
        data: `0x4d4b52${'00'.repeat(29)}`,
      });
    }
    return original(params);
  };
  const token = await f.adapter.token(mkr);
  assert.equal(token.address, mkr);
  assert.equal(token.decimals, 18);
  assert.equal(token.symbol, 'MKR');
  assert.equal(symbolReads, 2);
  assert.equal(f.state.signatures, 0);
});
test('unsupported or unsafe display labels use bounded sanitized text while contract and decimals remain mandatory', async () => {
  const f = fixture();
  const original = f.rpc.readContract;
  let mode: 'missing' | 'unsafe' | 'missing-decimals' | 'null-decimals' = 'missing';
  f.rpc.readContract = async (params) => {
    if (params.functionName === 'decimals' && mode === 'missing-decimals')
      throw new Error('Synthetic missing decimals');
    if (params.functionName === 'decimals' && mode === 'null-decimals') return null;
    if (params.functionName === 'symbol') {
      if (mode === 'unsafe') return `\u001b\n\u202e${'x'.repeat(100)}`;
      throw new Error('Synthetic unsupported label');
    }
    return original(params);
  };
  const fallback = await f.adapter.token(USDC);
  assert.equal(fallback.symbol, fallback.address);
  assert.equal(fallback.decimals, 6);
  mode = 'unsafe';
  assert.equal((await f.adapter.token(USDC)).symbol, 'x'.repeat(64));
  mode = 'missing-decimals';
  await assert.rejects(f.adapter.token(USDC), /Synthetic missing decimals/);
  mode = 'null-decimals';
  await assert.rejects(f.adapter.token(USDC), { code: 'TOKEN_METADATA_INVALID' });
  await assert.rejects(f.adapter.token(recipient), { code: 'MISSING_CONTRACT' });
});
test('wrong chain, excess token precision, and insufficient fee bound fail without signing', async () => {
  const f = fixture();
  f.state.chainId = 31337;
  await assert.rejects(f.adapter.preview('wallet.wrap', { ...write, amount: '0.001' }), {
    code: 'WRONG_CHAIN',
  });
  f.state.chainId = 1;
  await assert.rejects(
    f.adapter.preview('wallet.transfer', { ...write, token: USDC, amount: '0.0000001', recipient }),
    { code: 'EXCESS_PRECISION' },
  );
  await assert.rejects(
    f.adapter.preview('wallet.wrap', { ...write, maxFeeEth: '0.000001', amount: '0.001' }),
    { code: 'FEE_LIMIT' },
  );
  assert.equal(f.state.signatures, 0);
});
test('oracle valuation uses actual USDC quote rather than assuming one dollar', async () => {
  const f = fixture();
  const q = await f.adapter.quoteUsd(USDC, 1_000_000n);
  assert.equal(q.valueUsdMicros, '997000');
  f.state.price = 0n;
  await assert.rejects(f.adapter.quoteUsd(USDC), { code: 'PRICE_UNAVAILABLE' });
});
test('oracle requires fresh complete underlying rounds including capped USDC adapters', async () => {
  const f = fixture();
  f.state.capFeed = true;
  const q = await f.adapter.quoteUsd(USDC);
  assert.ok(q.source.includes(dataProvider));
  assert.equal(q.maximumPriceAgeSeconds, 86400);
  f.state.priceAgeSeconds = 86401n;
  await assert.rejects(f.adapter.quoteUsd(USDC), { code: 'STALE_PRICE' });
  f.state.priceAgeSeconds = 7201n;
  await assert.rejects(f.adapter.quoteUsd('native'), { code: 'STALE_PRICE' });
  f.state.priceAgeSeconds = -1n;
  await assert.rejects(f.adapter.quoteUsd(USDC), { code: 'STALE_PRICE' });
});
test('spending authority and collateral configuration value full affected exposure', async () => {
  const f = fixture();
  const approval = await f.adapter.preview('wallet.approve', {
    ...write,
    token: USDC,
    spender: pool,
    amount: '101',
  });
  assert.equal(approval.valuation?.movementUsdMicros, '100697000');
  f.state.supplied = 200_000_000n;
  const collateral = await f.adapter.preview('aave.collateral', {
    ...write,
    token: USDC,
    enabled: true,
  });
  assert.equal(collateral.valuation?.movementUsdMicros, '199400000');
});
test('refreshed fee reservation uses original approved transaction maximum', async () => {
  const f = fixture();
  const p = await f.adapter.preview('wallet.wrap', { ...write, amount: '0.001' });
  f.state.gas = 40000n;
  f.state.maxFeePerGas = 1_500_000_000n;
  const refreshed = await f.adapter.revalidate(p);
  assert.equal(refreshed.valuation?.maximumFeeUsdMicros, p.valuation?.maximumFeeUsdMicros);
});
test('new previews disclose a minimum 0.01 Gwei tip while preserving the RPC base-fee allowance', async () => {
  for (const [rpcTip, expectedMaximum] of [
    [0n, 3_015_000_000n],
    [7000n, 3_014_989_500n],
  ]) {
    const f = fixture();
    f.state.priority = rpcTip!;
    const p = await f.adapter.preview('wallet.wrap', { ...write, amount: '0.001' });
    assert.equal(p.binding.maxPriorityFeePerGas, '10000000');
    assert.equal(p.binding.maxFeePerGas, String(expectedMaximum));
    assert.match(
      p.checks.find((check) => check.name === 'fees')!.detail,
      /priority fee cap 10000000 wei/,
    );
    assert.match(p.checks.find((check) => check.name === 'fees')!.detail, /minimum 0\.01 Gwei tip/);
    const refreshed = await f.adapter.revalidate(p);
    assert.deepEqual(refreshed.binding, p.binding);
    assert.equal(refreshed.valuation?.maximumFeeUsdMicros, p.valuation?.maximumFeeUsdMicros);
    const submission = await f.adapter.submit(p, { persistSubmission() {} });
    assert.equal(submission.data.maxPriorityFeePerGas, '10000000');
    assert.ok(f.state.simulatedFees.every((fees) => fees.maxPriorityFeePerGas === 10_000_000n));
    assert.ok(
      BigInt(String(submission.data.gas)) * BigInt(String(submission.data.maxFeePerGas)) <=
        exactUnits(String(p.request.maxFeeEth), 18),
    );
  }
});
test('new minimum tip must fit maxFeeEth and is never lowered to make a preview pass', async () => {
  const f = fixture();
  f.state.priority = 0n;
  await assert.rejects(
    f.adapter.preview('wallet.wrap', { ...write, amount: '0.001', maxFeeEth: '0.00012' }),
    { code: 'FEE_LIMIT' },
  );
  const p = await f.adapter.preview('wallet.wrap', {
    ...write,
    amount: '0.001',
    maxFeeEth: '0.0001206',
  });
  assert.equal(p.binding.maxPriorityFeePerGas, '10000000');
  assert.equal(p.binding.maxFeePerGas, '2010000000');
  assert.equal(p.request.maxFeeEth, '0.0001206');
  assert.equal(p.valuation?.maximumFeeUsdMicros, '241200');
  assert.equal(f.state.signatures, 0);
});
test('bounded fee headroom absorbs small recommendations while signing only the approved caps', async () => {
  const f = fixture();
  const p = await f.adapter.preview('wallet.wrap', { ...write, amount: '0.001' });
  assert.equal(p.binding.gas, '60000');
  assert.equal(p.binding.maxFeePerGas, '3000000000');
  assert.equal(p.binding.maxPriorityFeePerGas, '1000000000');
  assert.equal(p.valuation?.maximumFeeUsdMicros, '360000');
  assert.match(p.summary, /maximum gas 0\.00018 ETH/);
  assert.match(p.checks.find((check) => check.name === 'fees')!.detail, /3000000000 wei/);

  // This balance funds the approved cap but not a newly enlarged candidate cap.
  f.state.native = 1_180_000_000_000_000n;
  f.state.maxFeePerGas = 2_500_000_000n;
  f.state.priority = 1_200_000_000n;
  const refreshed = await f.adapter.revalidate(p);
  assert.deepEqual(refreshed.binding, p.binding);
  assert.equal(refreshed.summary, p.summary);
  assert.equal(refreshed.valuation?.maximumFeeUsdMicros, p.valuation?.maximumFeeUsdMicros);
  assert.deepEqual(
    refreshed.checks.find((check) => check.name === 'fees'),
    p.checks.find((check) => check.name === 'fees'),
  );
  const submission = await f.adapter.submit(p, { persistSubmission() {} });
  assert.equal(submission.data.maxFeePerGas, '3000000000');
  const tx = f.state.signedTransactions[0]!;
  assert.equal(tx.maxFeePerGas, '3000000000');
  assert.equal(tx.maxPriorityFeePerGas, '1000000000');
  assert.ok(
    BigInt(tx.gas) * BigInt(tx.maxFeePerGas) <= exactUnits(String(p.request.maxFeeEth), 18),
  );
  assert.equal(f.state.signatures, 1);
  assert.equal(f.state.broadcasts, 1);
  assert.ok(
    f.state.simulatedFees.every(
      (fees) =>
        fees.gas === 60000n &&
        fees.maxFeePerGas === 3_000_000_000n &&
        fees.maxPriorityFeePerGas === 1_000_000_000n,
    ),
  );
});
test('fee headroom rounds upward then clamps to the caller total without raising maxFeeEth', async () => {
  const f = fixture();
  f.state.maxFeePerGas = 2_000_000_001n;
  const rounded = await f.adapter.preview('wallet.wrap', { ...write, amount: '0.001' });
  assert.equal(rounded.binding.maxFeePerGas, '3000000002');
  f.state.maxFeePerGas = 2_000_000_000n;
  const p = await f.adapter.preview('wallet.wrap', {
    ...write,
    maxFeeEth: '0.00015',
    amount: '0.001',
  });
  assert.equal(p.request.maxFeeEth, '0.00015');
  assert.equal(p.binding.maxFeePerGas, '2500000000');
  assert.equal(p.valuation?.maximumFeeUsdMicros, '300000');
  f.state.maxFeePerGas = 2_400_000_000n;
  assert.deepEqual((await f.adapter.revalidate(p)).binding, p.binding);
  f.state.maxFeePerGas = 2_500_000_001n;
  await assert.rejects(
    f.adapter.submit(p, {
      persistSubmission() {
        assert.fail('Fee breach must not persist');
      },
    }),
    { code: 'FEE_LIMIT' },
  );
  assert.equal(f.state.signatures, 0);
  assert.equal(f.state.broadcasts, 0);
});
test('recommendations beyond the approved headroom require fresh approval even inside maxFeeEth', async () => {
  const f = fixture();
  const p = await f.adapter.preview('wallet.wrap', { ...write, amount: '0.001' });
  f.state.maxFeePerGas = BigInt(String(p.binding.maxFeePerGas)) + 1n;
  await assert.rejects(
    f.adapter.submit(p, {
      persistSubmission() {
        assert.fail('Unapproved fee must not persist');
      },
    }),
    { code: 'FEE_BOUND_CHANGED' },
  );
  assert.equal(f.state.signatures, 0);
  assert.equal(f.state.broadcasts, 0);
});
test('native funding must cover the displayed headroom and malformed EIP-1559 fees cannot sign', async () => {
  const f = fixture();
  // Enough ETH for value plus recommended fees, but not the displayed bound.
  f.state.native = 1_120_000_000_000_000n;
  await assert.rejects(f.adapter.preview('wallet.wrap', { ...write, amount: '0.001' }), {
    code: 'INSUFFICIENT_GAS',
  });
  f.state.native = 10n ** 18n;
  f.state.priority = f.state.maxFeePerGas + 1n;
  await assert.rejects(f.adapter.preview('wallet.wrap', { ...write, amount: '0.001' }), {
    code: 'FEE_UNAVAILABLE',
  });
  f.state.priority = -1n;
  await assert.rejects(f.adapter.preview('wallet.wrap', { ...write, amount: '0.001' }), {
    code: 'FEE_UNAVAILABLE',
  });
  assert.equal(f.state.signatures, 0);
});
test('Uniswap quote chooses best on-chain fee tier and swap enforces explicit slippage/deadline/allowance', async () => {
  const f = fixture();
  const q = (await f.adapter.read('uniswap.quote', {
    ...base,
    tokenIn: WETH,
    tokenOut: USDC,
    amount: '0.001',
  })) as { data: { feeTier: number } };
  assert.equal(q.data.feeTier, 500);
  assert.deepEqual(f.state.quotes, [100, 500, 3000, 10000]);
  f.state.allowance = 10n ** 18n;
  const swap = {
    ...write,
    tokenIn: WETH,
    tokenOut: USDC,
    amount: '0.001',
    recipient,
    minOutput: '1.99',
    slippageBps: 50,
    deadline: Math.floor(f.state.clock.getTime() / 1000) + 100,
  };
  const p = await f.adapter.preview('uniswap.swap', swap);
  assert.equal((p.binding.effect as Input).feeTier, 500);
  await assert.rejects(f.adapter.preview('uniswap.swap', { ...swap, minOutput: '1.9' }), {
    code: 'SLIPPAGE_BOUND',
  });
  await assert.rejects(f.adapter.preview('uniswap.swap', { ...swap, deadline: 1 }), {
    code: 'INVALID_DEADLINE',
  });
  f.state.allowance = 0n;
  await assert.rejects(f.adapter.preview('uniswap.swap', swap), { code: 'ALLOWANCE_REQUIRED' });
});
test('ERC20 approval is separate and rejects unlimited approval or false return', async () => {
  const f = fixture();
  await assert.rejects(
    f.adapter.preview('wallet.approve', {
      ...write,
      token: USDC,
      spender: pool,
      amount: formatUnits(maxUint256, 6),
    }),
    { code: 'UNLIMITED_APPROVAL' },
  );
  f.state.callData = `0x${'0'.repeat(64)}`;
  await assert.rejects(
    f.adapter.preview('wallet.approve', { ...write, token: USDC, spender: pool, amount: '1' }),
    { code: 'TOKEN_OPERATION_FALSE' },
  );
  assert.equal(f.state.signatures, 0);
});
test('Aave exposes position claims/debt and rejects frozen/paused reserve and caps', async () => {
  const f = fixture();
  const read = (await f.adapter.read('aave.positions', { ...base, tokens: [USDC] })) as {
    data: { positions: Array<{ variableDebt: { units: string }; supplied: { units: string } }> };
  };
  assert.equal(read.data.positions[0]?.variableDebt.units, '1000000');
  f.state.frozen = true;
  await assert.rejects(f.adapter.preview('aave.supply', { ...write, token: USDC, amount: '1' }), {
    code: 'FROZEN_RESERVE',
  });
  f.state.frozen = false;
  f.state.paused = true;
  await assert.rejects(f.adapter.preview('aave.repay', { ...write, token: USDC, amount: '1' }), {
    code: 'UNAVAILABLE_RESERVE',
  });
  f.state.paused = false;
  f.state.supplyCap = 1n;
  await assert.rejects(f.adapter.preview('aave.supply', { ...write, token: USDC, amount: '1' }), {
    code: 'SUPPLY_CAP',
  });
});
test('Aave position discovery preserves stable debt separately from variable debt', async () => {
  const f = fixture();
  f.state.stableDebt = 2_000_001n;
  const read = (await f.adapter.read('aave.positions', { ...base, tokens: [USDC] })) as {
    data: {
      positions: Array<{
        stableDebt: { units: string; decimals: number };
        variableDebt: { units: string };
      }>;
    };
  };
  assert.equal(read.data.positions[0]?.stableDebt.units, '2000001');
  assert.equal(read.data.positions[0]?.stableDebt.decimals, 6);
  assert.equal(read.data.positions[0]?.variableDebt.units, '1000000');
  assert.equal(f.state.signatures, 0);
});
test('revision-5 supply proves the exact underlying debit and floor-scaled backing without a unit tolerance', async () => {
  const f = await suppliedFixture();
  const result = await f.adapter.reconcile(record(f.p, f.s));
  assert.equal(result.status, 'completed');
  const effects = result.evidence.effects as Input;
  assert.equal(effects.actualAmountUnits, '101');
  assert.equal(((effects.position as Input).supplied as Input).units, '100');
  const proof = effects.supplyBacking as Input;
  assert.deepEqual(proof, {
    method: 'aave-v3-revision-5-floor-scaled-mint',
    ray: (10n ** 27n).toString(),
    aToken,
    underlyingUnits: '101',
    mintIndexRay: (11n * 10n ** 26n).toString(),
    normalizedIncomeRay: (11n * 10n ** 26n).toString(),
    previousIndexRay: '0',
    scaledBeforeUnits: '0',
    scaledMintedUnits: '91',
    scaledAfterUnits: '91',
    suppliedBeforeAtMintUnits: '0',
    backedPrincipalUnits: '100',
    suppliedAfterUnits: '100',
    roundingDeltaUnits: '-1',
    mintBalanceIncreaseUnits: '0',
    mintValueUnits: '100',
    inclusionIncomeUnits: '0',
  });
  assert.equal(f.p.binding.effect && (f.p.binding.effect as Input).aTokenRevision, '5');
  assert.equal((f.p.binding.effect as Input).scaledSuppliedBeforeUnits, '0');
  assert.ok(f.historyReads.every((read) => read.blockNumber === 99n || read.blockNumber === 100n));
  assert.equal(f.state.signatures, 0);
});
test('supply backing separates old-position interest, floor carry, and inclusion income', async () => {
  const ray = 10n ** 27n;
  const carry = await suppliedFixture({
    underlying: 2n,
    scaledBefore: 1n,
    mintIndex: (3n * ray) / 2n,
    normalizedIncome: 2n * ray,
  });
  const carryResult = await carry.adapter.reconcile(record(carry.p, carry.s));
  assert.equal(carryResult.status, 'completed');
  const carryProof = (carryResult.evidence.effects as Input).supplyBacking as Input;
  assert.equal(carryProof.scaledMintedUnits, '1');
  assert.equal(carryProof.suppliedBeforeAtMintUnits, '1');
  assert.equal(
    carryProof.backedPrincipalUnits,
    '2',
    'Total balance floor carry belongs to the newly minted backing',
  );
  assert.equal(carryProof.roundingDeltaUnits, '0');
  assert.equal(carryProof.inclusionIncomeUnits, '1');
  assert.equal(carryProof.suppliedAfterUnits, '4');
  const accrued = await suppliedFixture({
    scaledBefore: 10n,
    mintIndex: (12n * ray) / 10n,
    normalizedIncome: (13n * ray) / 10n,
  });
  const accruedResult = await accrued.adapter.reconcile(record(accrued.p, accrued.s));
  assert.equal(accruedResult.status, 'completed');
  const proof = (accruedResult.evidence.effects as Input).supplyBacking as Input;
  assert.equal(proof.suppliedBeforeAtMintUnits, '12');
  assert.equal(proof.backedPrincipalUnits, '100');
  assert.equal(proof.mintBalanceIncreaseUnits, '2');
  assert.equal(proof.mintValueUnits, '102');
  assert.equal(proof.roundingDeltaUnits, '-1');
  assert.equal(proof.inclusionIncomeUnits, '10');
  assert.equal(proof.suppliedAfterUnits, '122');
});
test('supply backing refuses changed versions, scaled baseline, or unproved historical quantities', async (t) => {
  const variants: Array<{
    name: string;
    change: (f: Awaited<ReturnType<typeof suppliedFixture>>) => void;
  }> = [
    {
      name: 'wrong inclusion revision',
      change: (f) => {
        f.state.aTokenRevision = 6n;
      },
    },
    {
      name: 'historical backing is one unit smaller',
      change: (f) => {
        f.state.supplied -= 1n;
      },
    },
    {
      name: 'historical backing is one unit larger',
      change: (f) => {
        f.state.supplied += 1n;
      },
    },
    {
      name: 'mint event missing',
      change: (f) => {
        f.state.receipt!.logs = (f.state.receipt!.logs as unknown[]).slice(0, 3);
      },
    },
    {
      name: 'underlying debit is one unit smaller',
      change: (f) => {
        (f.state.receipt!.logs as unknown[])[1] = transferLog(
          WETH,
          syntheticAccount.address,
          aToken,
          100n,
        );
      },
    },
    {
      name: 'old half-up scaled balance is unsupported',
      change: (f) => {
        const read = f.rpc.readContract;
        f.rpc.readContract = async (p) =>
          p.functionName === 'scaledBalanceOf' && p.blockNumber === 100n ? 92n : read(p);
      },
    },
    {
      name: 'post-supply index changed',
      change: (f) => {
        const read = f.rpc.readContract;
        f.rpc.readContract = async (p) =>
          p.functionName === 'getPreviousIndex' && p.blockNumber === 100n
            ? 11n * 10n ** 26n + 1n
            : read(p);
      },
    },
    {
      name: 'approved scaled balance changed',
      change: (f) => {
        (f.p.binding.effect as Input).scaledSuppliedBeforeUnits = '1';
      },
    },
    {
      name: 'mint value claims underlying instead of actual backing',
      change: (f) => {
        f.mintLog.data = encodeAbiParameters(
          [{ type: 'uint256' }, { type: 'uint256' }, { type: 'uint256' }],
          [101n, 0n, 11n * 10n ** 26n],
        );
      },
    },
  ];
  for (const variant of variants)
    await t.test(variant.name, async () => {
      const f = await suppliedFixture();
      variant.change(f);
      const result = await f.adapter.reconcile(record(f.p, f.s));
      assert.equal(result.status, 'unknown');
      assert.equal(result.evidence.effectVerification, 'unresolved');
      assert.equal(f.state.broadcasts, 0);
    });
  const f = fixture();
  f.state.aTokenRevision = 6n;
  await assert.rejects(
    f.adapter.preview('aave.supply', { ...write, token: WETH, amount: '0.001' }),
    { code: 'AAVE_TOKEN_VERSION_UNSUPPORTED' },
  );
  f.state.aTokenRevision = 5n;
  f.state.allowance = 10n ** 18n;
  const p = await f.adapter.preview('aave.supply', { ...write, token: WETH, amount: '0.001' });
  f.state.scaledSupplied += 1n;
  await assert.rejects(
    f.adapter.submit(p, {
      persistSubmission() {
        assert.fail('Changed scaled baseline must not persist');
      },
    }),
    { code: 'APPROVED_STATE_CHANGED' },
  );
  assert.equal(f.state.signatures, 0);
});
test('all-reserve reads validate one provider per operation and writes still resolve fresh', async () => {
  const f = fixture();
  f.state.stableDebt = 2_000_001n;
  const read = (await f.adapter.read('aave.positions', base)) as {
    data: {
      positions: Array<{
        token: { address: string; decimals: number };
        supplied: { units: string };
        stableDebt: { units: string };
        variableDebt: { units: string };
      }>;
    };
  };
  assert.equal(f.state.providerResolutions, 1);
  assert.equal(read.data.positions.length, 2);
  const usdc = read.data.positions.find(
    (position) => position.token.address.toLowerCase() === USDC.toLowerCase(),
  )!;
  assert.equal(usdc.token.decimals, 6);
  assert.equal(usdc.supplied.units, f.state.supplied.toString());
  assert.equal(usdc.stableDebt.units, '2000001');
  assert.equal(usdc.variableDebt.units, '1000000');
  for (const unused of ['getReserveCaps', 'getPaused', 'totalSupply', 'balanceOf']) {
    assert.equal(
      f.state.readFunctions.includes(unused),
      false,
      'Position exposure does not need global reserve statistics',
    );
  }
  await f.adapter.read('aave.reserves', base);
  assert.equal(
    f.state.providerResolutions,
    2,
    'A subsequent read validates a new provider observation',
  );
  await f.adapter.preview('aave.borrow', {
    ...write,
    token: USDC,
    amount: '1',
    minHealthFactor: '1.5',
  });
  assert.ok(
    f.state.providerResolutions > 2,
    'Read-scoped provider resolution cannot bypass per-write identity checks',
  );
});
test('lean all-market positions retain all 67 reserves and hidden debts with bounded concurrency', async () => {
  const f = fixture();
  const tokens = [
    WETH,
    USDC,
    ...Array.from(
      { length: 65 },
      (_, index) => `0x${(400 + index).toString(16).padStart(40, '0')}`,
    ),
  ];
  const hidden = tokens.at(-1)!;
  const original = f.rpc.readContract;
  let active = 0;
  let peak = 0;
  f.rpc.readContract = async (params) => {
    if (['getReserveCaps', 'getPaused', 'totalSupply', 'balanceOf'].includes(params.functionName))
      assert.fail('Exposure discovery must not query unused global reserve statistics');
    if (params.functionName === 'getReservesList') return tokens;
    if (params.functionName === 'getUserReserveData') {
      active++;
      peak = Math.max(peak, active);
      await new Promise((resolve) => setTimeout(resolve, 2));
      active--;
      return String(params.args?.[0]).toLowerCase() === hidden.toLowerCase()
        ? [123n, 9n, 7n, 0n, 7n, 0n, 0n, 0n, true]
        : [0n, 0n, 0n, 0n, 0n, 0n, 0n, 0n, false];
    }
    return original(params);
  };
  for (let readNumber = 1; readNumber <= 2; readNumber++) {
    const result = (await f.adapter.read('aave.positions', base)) as {
      data: {
        positions: Array<{
          token: { address: string };
          supplied: { units: string };
          stableDebt: { units: string };
          variableDebt: { units: string };
          collateralEnabled: boolean;
        }>;
      };
    };
    assert.equal(f.state.providerResolutions, readNumber);
    assert.deepEqual(
      result.data.positions.map((position) => position.token.address.toLowerCase()),
      tokens.map((token) => token.toLowerCase()),
    );
    const exposure = result.data.positions.at(-1)!;
    assert.equal(exposure.supplied.units, '123');
    assert.equal(exposure.stableDebt.units, '9');
    assert.equal(exposure.variableDebt.units, '7');
    assert.equal(exposure.collateralEnabled, true);
  }
  assert.equal(peak, 4);
  assert.equal(f.state.signatures, 0);
});
test('lean positions still reject mismatched reserve decimals or missing claim/debt contract code', async () => {
  const f = fixture();
  const original = f.rpc.readContract;
  f.rpc.readContract = async (params) =>
    params.functionName === 'getReserveConfigurationData'
      ? [18n, 8000n, 8500n, 10500n, 1000n, true, true, false, true, false]
      : original(params);
  await assert.rejects(f.adapter.read('aave.positions', { ...base, tokens: [USDC] }), {
    code: 'TOKEN_METADATA_CHANGED',
  });
  f.rpc.readContract = original;
  const originalCode = f.rpc.getBytecode;
  f.rpc.getBytecode = async (params) =>
    params.address.toLowerCase() === debtToken.toLowerCase() ? '0x' : originalCode(params);
  await assert.rejects(f.adapter.read('aave.positions', { ...base, tokens: [USDC] }), {
    code: 'MISSING_CONTRACT',
  });
});
test('Aave borrow counts new assets as additional allocation and respects collateral and liquidity', async () => {
  const f = fixture();
  const p = await f.adapter.preview('aave.borrow', {
    ...write,
    token: USDC,
    amount: '1',
    minHealthFactor: '1.5',
  });
  assert.equal(p.valuation?.additionalAllocationUsdMicros, '997000');
  f.state.liquidity = 0n;
  await assert.rejects(
    f.adapter.preview('aave.borrow', {
      ...write,
      token: USDC,
      amount: '1',
      minHealthFactor: '1.5',
    }),
    { code: 'INSUFFICIENT_LIQUIDITY' },
  );
  f.state.liquidity = 100_000_000n;
  f.state.borrowing = false;
  await assert.rejects(
    f.adapter.preview('aave.borrow', {
      ...write,
      token: USDC,
      amount: '1',
      minHealthFactor: '1.5',
    }),
    { code: 'BORROWING_DISABLED' },
  );
});
test('Aave repay-all bounds accrual with finite allowance and maxAmount, not original principal', async () => {
  const f = fixture();
  f.state.debt = 1_000_010n;
  f.state.allowance = 1_010_000n;
  const p = await f.adapter.preview('aave.repay', {
    ...write,
    token: USDC,
    amount: 'all',
    maxAmount: '1.01',
  });
  assert.equal(p.binding.amountUnits, '1010000');
  assert.equal((p.binding.effect as Input).repayAll, true);
  f.state.allowance = maxUint256;
  await assert.rejects(
    f.adapter.preview('aave.repay', { ...write, token: USDC, amount: 'all', maxAmount: '1.01' }),
    { code: 'BOUNDED_ALLOWANCE_REQUIRED' },
  );
  f.state.allowance = 1_010_000n;
  f.state.debt = 1_010_001n;
  await assert.rejects(
    f.adapter.preview('aave.repay', { ...write, token: USDC, amount: 'all', maxAmount: '1.01' }),
    { code: 'DEBT_EXCEEDS_BOUND' },
  );
});
test('expired preview and changed execution request require a new approval', async () => {
  const f = fixture();
  const p = await f.adapter.preview('wallet.wrap', { ...write, amount: '0.001' });
  await assert.rejects(
    f.adapter.submit(
      { ...p, request: { ...p.request, amount: '0.002' } },
      { persistSubmission() {} },
    ),
    { code: 'APPROVED_REQUEST_CHANGED' },
  );
  f.state.clock = new Date(Date.parse(p.expiresAt));
  await assert.rejects(f.adapter.submit(p, { persistSubmission() {} }), {
    code: 'PREVIEW_EXPIRED',
  });
  assert.equal(f.state.signatures, 0);
  assert.equal(f.state.broadcasts, 0);
});
test('revalidation preserves approved execution caps and rejects increased current gas/fee', async () => {
  const f = fixture();
  const p = await f.adapter.preview('wallet.wrap', { ...write, amount: '0.001' });
  f.state.gas = 49000n;
  assert.deepEqual((await f.adapter.revalidate(p)).binding, p.binding);
  f.state.gas = 60001n;
  await assert.rejects(f.adapter.revalidate(p), { code: 'GAS_BOUND_CHANGED' });
  f.state.gas = 50000n;
  f.state.maxFeePerGas = BigInt(String(p.binding.maxFeePerGas)) + 1n;
  await assert.rejects(f.adapter.revalidate(p), { code: 'FEE_BOUND_CHANGED' });
});
test('pending nonce is approval-bound and external wallet activity requires fresh approval', async () => {
  const f = fixture();
  const p = await f.adapter.preview('wallet.wrap', { ...write, amount: '0.001' });
  assert.equal(p.binding.nonce, 5);
  f.state.nonce = 6;
  await assert.rejects(f.adapter.revalidate(p), { code: 'NONCE_CHANGED' });
  await assert.rejects(
    f.adapter.submit(p, {
      persistSubmission() {
        assert.fail('Must not persist a changed nonce');
      },
    }),
    { code: 'NONCE_CHANGED' },
  );
  assert.equal(f.state.signatures, 0);
  assert.equal(f.state.broadcasts, 0);
});
test('nonce drift after revalidation is rejected immediately before signer dispatch', async () => {
  const f = fixture();
  const p = await f.adapter.preview('wallet.wrap', { ...write, amount: '0.001' });
  let reads = 0;
  f.rpc.getTransactionCount = async () => (++reads === 1 ? 5 : 6);
  await assert.rejects(
    f.adapter.submit(p, {
      persistSubmission() {
        assert.fail('Must not persist after nonce drift');
      },
    }),
    { code: 'NONCE_CHANGED' },
  );
  assert.equal(reads, 2);
  assert.equal(f.state.signatures, 0);
  assert.equal(f.state.broadcasts, 0);
});
test('authorization expiry guard executes immediately before signing', async () => {
  const f = fixture();
  const p = await f.adapter.preview('wallet.wrap', { ...write, amount: '0.001' });
  let guarded = false;
  await assert.rejects(
    f.adapter.submit(p, {
      assertAuthorized() {
        guarded = true;
        throw new Error('Synthetic approval expired');
      },
      persistSubmission() {
        assert.fail('Must not persist after approval expiry');
      },
    }),
    /Synthetic approval expired/,
  );
  assert.equal(guarded, true);
  assert.equal(f.state.signatures, 0);
  assert.equal(f.state.broadcasts, 0);
});
test('transaction identity is persisted before broadcast and persistence failure cannot broadcast', async () => {
  const f = fixture();
  const p = await f.adapter.preview('wallet.wrap', { ...write, amount: '0.001' });
  await assert.rejects(
    f.adapter.submit(p, {
      persistSubmission() {
        throw new Error('Synthetic durable storage failure');
      },
    }),
  );
  assert.equal(f.state.broadcasts, 0);
  let stored: Submission | undefined;
  const s = await f.adapter.submit(p, {
    persistSubmission(s) {
      assert.equal(f.state.broadcasts, 0);
      stored = s;
    },
  });
  assert.equal(stored?.reference, s.reference);
  assert.equal(s.data.nonce, 5);
  assert.equal('serializedTransaction' in s.data, false);
  assert.equal(f.state.broadcasts, 1);
});
test('broadcast timeout preserves original identity and reconciliation never blindly resubmits', async () => {
  const f = fixture();
  const p = await f.adapter.preview('wallet.wrap', { ...write, amount: '0.001' });
  f.state.broadcastFailure = true;
  let persisted: Submission | undefined;
  await assert.rejects(
    f.adapter.submit(p, {
      persistSubmission(s) {
        persisted = s;
      },
    }),
  );
  assert.ok(persisted);
  assert.equal((await f.adapter.reconcile(record(p, persisted))).status, 'pending');
  assert.equal(f.state.broadcasts, 1);
});
test('injected signer must return the exact approved transaction', async () => {
  const f = fixture();
  const adapter = new EthereumAdapter(config, {
    publicClient: f.rpc,
    now: () => f.state.clock,
    signTransaction: (tx) => f.signTransaction({ ...tx, value: '999' }),
  });
  const p = await adapter.preview('wallet.wrap', { ...write, amount: '0.001' });
  await assert.rejects(adapter.submit(p, { persistSubmission() {} }), {
    code: 'SIGNED_TRANSACTION_MISMATCH',
  });
  assert.equal(f.state.broadcasts, 0);
});
test('legacy approved zero tip and zero nonce remain exact and viem canonical zero signs successfully', async () => {
  const f = fixture();
  f.state.nonce = 0;
  const adapter = new EthereumAdapter(config, {
    publicClient: f.rpc,
    now: () => f.state.clock,
    signTransaction: async (tx) => {
      const raw = await f.signTransaction(tx);
      const decoded = parseTransaction(raw);
      assert.equal(decoded.nonce, 0);
      assert.equal(decoded.value, undefined, 'Canonical RLP zero value is omitted by viem');
      assert.equal(
        decoded.maxPriorityFeePerGas,
        undefined,
        'Canonical RLP zero tip is omitted by viem',
      );
      return raw;
    },
  });
  const p = await adapter.preview('wallet.approve', {
    ...write,
    token: WETH,
    spender: pool,
    amount: '0.001',
    maxFeeEth: '0.00012',
  });
  // Synthetic historical authorization from before the NEW-preview floor. Its
  // exact maximum fee fits the caller, while adding today's floor would not.
  p.binding = { ...p.binding, maxPriorityFeePerGas: '0' };
  f.state.priority = 0n;
  f.state.native = 120_000_000_000_000n;
  const refreshed = await adapter.revalidate(p);
  assert.deepEqual(refreshed.binding, p.binding);
  assert.equal(refreshed.valuation?.maximumFeeUsdMicros, p.valuation?.maximumFeeUsdMicros);
  assert.equal(refreshed.valuation?.maximumFeeUsdMicros, '240000');
  const submission = await adapter.submit(p, { persistSubmission() {} });
  assert.equal(submission.data.nonce, 0);
  assert.equal(submission.data.maxPriorityFeePerGas, '0');
  assert.equal(submission.data.maxFeePerGas, '2000000000');
  assert.ok(
    f.state.simulatedFees
      .slice(1)
      .every((fees) => fees.maxPriorityFeePerGas === 0n && fees.maxFeePerGas === 2_000_000_000n),
  );
  assert.equal(f.state.broadcasts, 1);
});
test('decoded zero fee equivalence does not accept any altered EIP-1559 payload field', async (t) => {
  const variants: Array<{
    name: string;
    tip?: bigint;
    changes: Partial<TransactionSerializableEIP1559>;
  }> = [
    { name: 'zero tip changed to one', tip: 0n, changes: { maxPriorityFeePerGas: 1n } },
    { name: 'nonzero tip removed', changes: { maxPriorityFeePerGas: 0n } },
    { name: 'maximum fee increased', changes: { maxFeePerGas: 3_000_000_001n } },
    { name: 'maximum fee removed', changes: { maxFeePerGas: 0n, maxPriorityFeePerGas: 0n } },
    { name: 'gas increased', changes: { gas: 60001n } },
    { name: 'zero nonce increased', changes: { nonce: 1 } },
    { name: 'calldata removed', changes: { data: '0x' } },
    { name: 'recipient changed', changes: { to: recipient } },
    { name: 'zero value increased', changes: { value: 1n } },
    { name: 'chain changed', changes: { chainId: 2 } },
    {
      name: 'unapproved access list added',
      changes: { accessList: [{ address: aToken, storageKeys: [] }] },
    },
  ];
  for (const variant of variants)
    await t.test(variant.name, async () => {
      const f = fixture();
      f.state.nonce = 0;
      if (variant.tip !== undefined) f.state.priority = variant.tip;
      const adapter = new EthereumAdapter(config, {
        publicClient: f.rpc,
        now: () => f.state.clock,
        signTransaction: (tx) =>
          syntheticAccount.signTransaction({
            chainId: tx.chainId,
            to: tx.to,
            data: tx.data,
            value: BigInt(tx.value),
            gas: BigInt(tx.gas),
            nonce: tx.nonce,
            maxFeePerGas: BigInt(tx.maxFeePerGas),
            maxPriorityFeePerGas: BigInt(tx.maxPriorityFeePerGas),
            type: 'eip1559',
            ...variant.changes,
          }),
      });
      const p = await adapter.preview('wallet.approve', {
        ...write,
        token: WETH,
        spender: pool,
        amount: '0.001',
      });
      if (variant.tip === 0n) p.binding = { ...p.binding, maxPriorityFeePerGas: '0' }; // Synthetic historical exact zero-tip authorization.
      await assert.rejects(
        adapter.submit(p, {
          persistSubmission() {
            assert.fail('Altered payload must not persist');
          },
        }),
        { code: 'SIGNED_TRANSACTION_MISMATCH' },
      );
      assert.equal(f.state.broadcasts, 0);
    });
});
test('revalidation uses the approved gas headroom once and rejects raw estimates above it', async () => {
  const f = fixture();
  const p = await f.adapter.preview('wallet.wrap', {
    ...write,
    maxFeeEth: '0.00012',
    amount: '0.001',
  });
  assert.equal(p.binding.gas, '60000');
  assert.equal(p.binding.maxFeePerGas, '2000000000');
  f.state.gas = 60000n;
  const refreshed = await f.adapter.revalidate(p);
  assert.deepEqual(refreshed.binding, p.binding);
  assert.equal(refreshed.summary, p.summary);
  assert.equal(refreshed.valuation?.maximumFeeUsdMicros, p.valuation?.maximumFeeUsdMicros);
  const submission = await f.adapter.submit(p, { persistSubmission() {} });
  assert.equal(submission.data.gas, '60000');
  assert.equal(f.state.signedTransactions[0]!.gas, '60000');
  f.state.gas = 60001n;
  await assert.rejects(
    f.adapter.submit(p, {
      persistSubmission() {
        assert.fail('Gas breach must not persist');
      },
    }),
    { code: 'GAS_BOUND_CHANGED' },
  );
  assert.equal(f.state.signatures, 1);
  assert.equal(f.state.broadcasts, 1);
});
test('missing protected signer fails without a live signing fallback', async () => {
  const f = fixture();
  const adapter = new EthereumAdapter(config, { publicClient: f.rpc, now: () => f.state.clock });
  const p = await adapter.preview('wallet.wrap', { ...write, amount: '0.001' });
  await assert.rejects(adapter.submit(p, { persistSubmission() {} }), {
    code: 'PROTECTED_SIGNER_REQUIRED',
  });
  assert.equal(f.state.broadcasts, 0);
});
test('submission and one confirmation are insufficient; exact transfer logs prove reconciled amount', async () => {
  const f = fixture();
  const p = await f.adapter.preview('wallet.transfer', {
    ...write,
    token: USDC,
    amount: '1',
    recipient,
  });
  const s = confirmed(f, p, [transferLog(USDC, syntheticAccount.address, recipient, 1_000_000n)]);
  f.state.head = 100n;
  assert.equal((await f.adapter.reconcile(record(p, s))).status, 'pending');
  f.state.head = 101n;
  const outcome = await f.adapter.reconcile(record(p, s));
  assert.equal(outcome.status, 'completed');
  assert.equal((outcome.evidence.effects as Input).receivedUnits, '1000000');
});
test('successful receipt without expected effect stays unresolved, revert fails with gas evidence', async () => {
  const f = fixture();
  const p = await f.adapter.preview('wallet.transfer', {
    ...write,
    token: USDC,
    amount: '1',
    recipient,
  });
  const s = confirmed(f, p, []);
  assert.equal((await f.adapter.reconcile(record(p, s))).status, 'unknown');
  f.state.receipt!.status = 'reverted';
  const r = await f.adapter.reconcile(record(p, s));
  assert.equal(r.status, 'failed');
  assert.equal(r.evidence.feeWei, '21000000000000');
});
test('unconfirmed revert remains pending until the configured canonical finality policy', async () => {
  const f = fixture();
  const p = await f.adapter.preview('wallet.transfer', {
    ...write,
    token: 'native',
    amount: '0.001',
    recipient,
  });
  const s = confirmed(f, p);
  f.state.receipt!.status = 'reverted';
  f.state.head = 100n;
  assert.equal((await f.adapter.reconcile(record(p, s))).status, 'pending');
  f.state.head = 101n;
  assert.equal((await f.adapter.reconcile(record(p, s))).status, 'failed');
});
test('canonical block mismatch cannot complete an operation', async () => {
  const f = fixture();
  const p = await f.adapter.preview('wallet.transfer', {
    ...write,
    token: 'native',
    amount: '0.001',
    recipient,
  });
  const s = confirmed(f, p);
  f.state.receipt!.blockHash = `0x${'00'.repeat(32)}`;
  assert.equal((await f.adapter.reconcile(record(p, s))).status, 'unknown');
});
test('wrong or malformed canonical block numbers cannot settle success or revert', async (t) => {
  for (const status of ['success', 'reverted'])
    for (const number of [99n, -1n, undefined, null, '100.5', Number.MAX_SAFE_INTEGER + 1])
      await t.test(`${status}: canonical number ${String(number)}`, async () => {
        const f = fixture();
        const p = await f.adapter.preview('wallet.transfer', {
          ...write,
          token: USDC,
          amount: '1',
          recipient,
        });
        const s = confirmed(f, p, [
          transferLog(USDC, syntheticAccount.address, recipient, 1_000_000n),
        ]);
        f.state.receipt!.status = status;
        f.rpc.getBlock = async () => ({ hash: blockHash, number: number as bigint });
        const result = await f.adapter.reconcile(record(p, s));
        assert.equal(result.status, 'unknown');
        assert.equal(result.evidence.feeWei, undefined);
      });
});
test('unrelated or malformed receipts cannot settle success or revert gas costs', async (t) => {
  const variants: Array<{
    name: string;
    receipt?: Record<string, unknown>;
    transaction?: Record<string, unknown>;
    persistedNonce?: unknown;
    missingPreview?: boolean;
  }> = [
    { name: 'different receipt hash', receipt: { transactionHash: `0x${'02'.repeat(32)}` } },
    { name: 'missing receipt hash', receipt: { transactionHash: undefined } },
    { name: 'missing receipt status', receipt: { status: undefined } },
    { name: 'unknown receipt status', receipt: { status: 'pending' } },
    { name: 'raw receipt status is unsupported', receipt: { status: '0x1' } },
    { name: 'missing gas', receipt: { gasUsed: undefined } },
    { name: 'zero gas', receipt: { gasUsed: 0n } },
    { name: 'negative gas', receipt: { gasUsed: -1n } },
    { name: 'fractional gas', receipt: { gasUsed: '1.5' } },
    { name: 'unsafe numeric gas', receipt: { gasUsed: Number.MAX_SAFE_INTEGER + 1 } },
    { name: 'oversized gas', receipt: { gasUsed: maxUint256 + 1n } },
    { name: 'gas exceeds exact limit', receipt: { gasUsed: 60001n } },
    { name: 'missing fee', receipt: { effectiveGasPrice: undefined } },
    { name: 'zero fee', receipt: { effectiveGasPrice: 0n } },
    { name: 'negative fee', receipt: { effectiveGasPrice: -1n } },
    { name: 'null fee', receipt: { effectiveGasPrice: null } },
    {
      name: 'fee exceeds authorized maximum',
      receipt: { effectiveGasPrice: 3_000_000_001n },
      transaction: { gasPrice: 3_000_000_001n },
    },
    {
      name: 'receipt fee differs from mined transaction',
      receipt: { effectiveGasPrice: 1_000_000_001n },
    },
    { name: 'malformed logs', receipt: { logs: {} } },
    { name: 'missing block number', receipt: { blockNumber: undefined } },
    { name: 'negative block number', receipt: { blockNumber: -1n } },
    { name: 'missing transaction index', receipt: { transactionIndex: undefined } },
    { name: 'receipt sender mismatch', receipt: { from: recipient } },
    { name: 'receipt destination mismatch', receipt: { to: pool } },
    { name: 'mined transaction hash mismatch', transaction: { hash: `0x${'03'.repeat(32)}` } },
    {
      name: 'mined transaction block hash mismatch',
      transaction: { blockHash: `0x${'04'.repeat(32)}` },
    },
    { name: 'mined transaction block number mismatch', transaction: { blockNumber: 99n } },
    { name: 'mined transaction index mismatch', transaction: { transactionIndex: 1 } },
    { name: 'mined sender altered', transaction: { from: recipient } },
    { name: 'mined destination altered', transaction: { to: pool } },
    { name: 'mined payload altered', transaction: { input: '0x' } },
    { name: 'mined value altered', transaction: { value: 1n } },
    { name: 'mined nonce altered', transaction: { nonce: 6 } },
    { name: 'mined gas limit altered', transaction: { gas: 60001n } },
    { name: 'mined maximum fee altered', transaction: { maxFeePerGas: 3_000_000_001n } },
    { name: 'mined priority fee altered', transaction: { maxPriorityFeePerGas: 1_000_000_001n } },
    { name: 'mined chain altered', transaction: { chainId: 2 } },
    { name: 'mined type altered', transaction: { type: 'legacy' } },
    {
      name: 'mined access list altered',
      transaction: { accessList: [{ address: recipient, storageKeys: [] }] },
    },
    { name: 'persisted nonce altered', persistedNonce: 6 },
    { name: 'original preview missing', missingPreview: true },
  ];
  for (const status of ['success', 'reverted'])
    for (const variant of variants)
      await t.test(`${status}: ${variant.name}`, async () => {
        const f = fixture();
        const p = await f.adapter.preview('wallet.transfer', {
          ...write,
          token: USDC,
          amount: '1',
          recipient,
        });
        const s = confirmed(f, p, [
          transferLog(USDC, syntheticAccount.address, recipient, 1_000_000n),
        ]);
        Object.assign(f.state.receipt!, { status }, variant.receipt);
        Object.assign(f.state.tx!, variant.transaction);
        if (variant.persistedNonce !== undefined) s.data.nonce = variant.persistedNonce;
        const operation = record(p, s);
        if (variant.missingPreview) operation.preview = undefined;
        const result = await f.adapter.reconcile(operation);
        assert.equal(result.status, 'unknown');
        assert.equal(result.evidence.transactionHash, s.reference);
        assert.equal(
          result.evidence.feeWei,
          undefined,
          'Unverified fees must not be settlement evidence',
        );
        assert.equal(f.state.broadcasts, 0);
      });
});
test('exact receipt quantities and zero-tip mined identity permit only proved terminal fees', async () => {
  const f = fixture();
  f.state.priority = 0n;
  f.state.nonce = 0;
  const p = await f.adapter.preview('wallet.transfer', {
    ...write,
    token: USDC,
    amount: '1',
    recipient,
  });
  p.binding = { ...p.binding, maxPriorityFeePerGas: '0' }; // Synthetic historical exact zero-tip authorization.
  const s = confirmed(f, p, [transferLog(USDC, syntheticAccount.address, recipient, 1_000_000n)]);
  Object.assign(f.state.receipt!, {
    gasUsed: '21000',
    effectiveGasPrice: '1000000000',
    transactionIndex: '0',
    blockNumber: '100',
  });
  const completed = await f.adapter.reconcile(record(p, s));
  assert.equal(completed.status, 'completed');
  assert.equal(completed.evidence.feeWei, '21000000000000');
  f.state.receipt!.status = 'reverted';
  const reverted = await f.adapter.reconcile(record(p, s));
  assert.equal(reverted.status, 'failed');
  assert.equal(reverted.evidence.receiptStatus, 'reverted');
  assert.equal(reverted.evidence.feeWei, '21000000000000');
});
test('receipt reads reject unrelated hashes and unknown statuses without signing', async () => {
  const f = fixture();
  const p = await f.adapter.preview('wallet.transfer', {
    ...write,
    token: USDC,
    amount: '1',
    recipient,
  });
  const s = confirmed(f, p);
  f.state.receipt!.transactionHash = `0x${'09'.repeat(32)}`;
  await assert.rejects(
    f.adapter.read('wallet.receipt', { ...base, transactionHash: s.reference }),
    { code: 'RECEIPT_HASH_MISMATCH' },
  );
  f.state.receipt!.transactionHash = s.reference;
  f.state.receipt!.status = 'unknown';
  await assert.rejects(
    f.adapter.read('wallet.receipt', { ...base, transactionHash: s.reference }),
    { code: 'RECEIPT_INVALID' },
  );
  assert.equal(f.state.signatures, 0);
});
test('Aave repayment verifies remaining variable debt before declaring cleanup', async () => {
  const f = fixture();
  f.state.allowance = 1_010_000n;
  const p = await f.adapter.preview('aave.repay', {
    ...write,
    token: USDC,
    amount: 'all',
    maxAmount: '1.01',
  });
  const log = {
    address: pool,
    data: encodeAbiParameters([{ type: 'uint256' }, { type: 'bool' }], [1_000_000n, false]),
    topics: encodeEventTopics({
      abi: poolAbi,
      eventName: 'Repay',
      args: { reserve: USDC, user: syntheticAccount.address, repayer: syntheticAccount.address },
    }),
  };
  const s = confirmed(f, p, [log]);
  f.state.debt = 1n;
  assert.equal((await f.adapter.reconcile(record(p, s))).status, 'unknown');
  f.state.debt = 0n;
  assert.equal((await f.adapter.reconcile(record(p, s))).status, 'completed');
});
