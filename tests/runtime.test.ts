import test from 'node:test';
import assert from 'node:assert/strict';
import { encodeAbiParameters, encodeEventTopics, parseAbi } from 'viem';
import { createBinanceRecipientVerifier } from '../src/runtime.js';
import type { Input } from '../src/core/types.js';

// Synthetic read-only RPC evidence. No keys, signer, credentials, or transport.
const HASH = `0x${'ab'.repeat(32)}`;
const BLOCK = `0x${'cd'.repeat(32)}`;
const RECIPIENT = `0x${'11'.repeat(20)}`;
const SENDER = `0x${'22'.repeat(20)}`;
const TOKEN = `0x${'33'.repeat(20)}`;
const OTHER = `0x${'44'.repeat(20)}`;
function fixture() {
  const state = {
    chainId: 1,
    head: 12n as unknown,
    receipt: {
      transactionHash: HASH,
      blockHash: BLOCK,
      blockNumber: 10n,
      status: 'success',
      logs: [],
    } as Input,
    canonical: { hash: BLOCK, number: 10n } as Input,
    transaction: {
      hash: HASH,
      blockHash: BLOCK,
      blockNumber: 10n,
      to: RECIPIENT,
      value: 2n,
    } as Input,
    transactionCalls: 0,
    fail: false,
  };
  const client = {
    getChainId: async () => state.chainId,
    getTransactionReceipt: async ({ hash }: { hash: string }) => {
      assert.equal(hash, HASH);
      if (state.fail) throw new Error('SYNTHETIC_RPC_SENSITIVE_ERROR');
      return state.receipt;
    },
    getBlock: async ({ blockNumber }: { blockNumber: bigint }) => {
      assert.equal(blockNumber, 10n);
      return state.canonical;
    },
    getBlockNumber: async () => state.head,
    getTransaction: async ({ hash }: { hash: string }) => {
      assert.equal(hash, HASH);
      state.transactionCalls++;
      return state.transaction;
    },
  } as unknown as Parameters<typeof createBinanceRecipientVerifier>[0];
  const input: Parameters<ReturnType<typeof createBinanceRecipientVerifier>['verify']>[0] = {
    txHash: HASH,
    chainId: 1,
    recipient: RECIPIENT,
    asset: { coin: 'ETH', decimals: 18 },
    minimumAmount: '0.000000000000000001',
    maximumAmount: '0.000000000000000002',
    confirmations: 3,
  };
  return { state, input, verifier: createBinanceRecipientVerifier(client) };
}

test('payout verification requires original receipt and mined native transaction identity', async () => {
  const f = fixture();
  f.state.receipt.transactionHash = `0x${'AB'.repeat(32)}`;
  f.state.receipt.blockHash = `0x${'CD'.repeat(32)}`;
  const result = await f.verifier.verify(f.input);
  assert.equal(result.status, 'completed');
  assert.equal(result.evidence.transactionHash, HASH);
  assert.equal(result.evidence.actualUnits, '2');
  assert.equal(result.evidence.confirmations, '3');
  assert.equal(f.state.transactionCalls, 1);
});

test('mismatched or malformed receipt and canonical fields cannot prove payout or original failure', async () => {
  const mutations: ((f: ReturnType<typeof fixture>) => void)[] = [
    (f) => {
      f.state.receipt.transactionHash = `0x${'99'.repeat(32)}`;
      f.state.receipt.status = 'reverted';
    },
    (f) => {
      delete f.state.receipt.transactionHash;
    },
    (f) => {
      f.state.receipt.status = 'unsupported';
    },
    (f) => {
      delete f.state.receipt.status;
    },
    (f) => {
      f.state.receipt.blockNumber = -1n;
    },
    (f) => {
      f.state.receipt.blockNumber = '10';
    },
    (f) => {
      f.state.receipt.blockHash = 'malformed';
    },
    (f) => {
      f.state.canonical.hash = `0x${'99'.repeat(32)}`;
    },
    (f) => {
      f.state.canonical.number = 11n;
    },
    (f) => {
      f.state.head = -1n;
    },
    (f) => {
      f.state.head = '12';
    },
    (f) => {
      f.state.chainId = 2;
    },
  ];
  for (const mutate of mutations) {
    const f = fixture();
    mutate(f);
    assert.equal((await f.verifier.verify(f.input)).status, 'unknown');
    assert.equal(f.state.transactionCalls, 0);
  }
});

test('canonical revert remains pending until finality and only then proves failed payout', async () => {
  const f = fixture();
  f.state.receipt.status = 'reverted';
  f.state.head = 10n;
  assert.equal((await f.verifier.verify(f.input)).status, 'pending');
  f.state.head = 12n;
  const result = await f.verifier.verify(f.input);
  assert.equal(result.status, 'failed');
  assert.equal(result.evidence.receiptStatus, 'reverted');
  assert.equal(result.evidence.transactionHash, HASH);
  assert.equal(f.state.transactionCalls, 0);
});

test('native payout cannot use an unrelated transaction, another block, invalid value or indirect recipient', async () => {
  const mutations: ((f: ReturnType<typeof fixture>) => void)[] = [
    (f) => {
      f.state.transaction.hash = `0x${'99'.repeat(32)}`;
    },
    (f) => {
      delete f.state.transaction.hash;
    },
    (f) => {
      f.state.transaction.blockHash = `0x${'99'.repeat(32)}`;
    },
    (f) => {
      f.state.transaction.blockNumber = 11n;
    },
    (f) => {
      f.state.transaction.value = -1n;
    },
    (f) => {
      f.state.transaction.value = '2';
    },
    (f) => {
      f.state.transaction.to = OTHER;
    },
    (f) => {
      f.state.transaction.value = 3n;
    },
  ];
  for (const mutate of mutations) {
    const f = fixture();
    mutate(f);
    assert.equal((await f.verifier.verify(f.input)).status, 'unknown');
  }
});

function transferLog(address: string, to: string) {
  return {
    address,
    topics: encodeEventTopics({
      abi: parseAbi(['event Transfer(address indexed from,address indexed to,uint256 value)']),
      eventName: 'Transfer',
      args: { from: SENDER as `0x${string}`, to: to as `0x${string}` },
    }),
    data: encodeAbiParameters([{ type: 'uint256' }], [2n]),
  };
}
test('ERC20 payout uses only matching contract and recipient logs in the original canonical receipt', async () => {
  const f = fixture();
  f.input.asset.tokenAddress = TOKEN;
  f.state.receipt.logs = [
    transferLog(OTHER, RECIPIENT),
    transferLog(TOKEN, OTHER),
    transferLog(TOKEN, RECIPIENT),
  ];
  assert.equal((await f.verifier.verify(f.input)).status, 'completed');
  assert.equal(f.state.transactionCalls, 0);
  f.state.receipt.logs = [transferLog(OTHER, RECIPIENT), transferLog(TOKEN, OTHER)];
  assert.equal((await f.verifier.verify(f.input)).status, 'unknown');
  f.state.receipt.logs = [transferLog(TOKEN, RECIPIENT), transferLog(TOKEN, RECIPIENT)];
  assert.equal((await f.verifier.verify(f.input)).status, 'unknown');
});

test('invalid finality requirements and unavailable RPCs stay unknown without leaking diagnostics', async () => {
  const f = fixture();
  f.input.confirmations = 0;
  assert.equal((await f.verifier.verify(f.input)).status, 'unknown');
  f.input.confirmations = 3;
  f.state.fail = true;
  const result = await f.verifier.verify(f.input);
  assert.equal(result.status, 'unknown');
  assert.equal(JSON.stringify(result).includes('SENSITIVE'), false);
});
