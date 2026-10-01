import ccxt from 'ccxt';
import {
  createPublicClient,
  http,
  fallback,
  decodeEventLog,
  parseAbi,
  formatUnits,
  type Hex,
  type Address,
  type PublicClient,
} from 'viem';
import { mainnet } from 'viem/chains';
import { privateKeyToAccount } from 'viem/accounts';
import {
  BinanceAdapter,
  type BinanceClient,
  type BinanceRecipientVerifier,
} from './adapters/binance.js';
import { EthereumAdapter, type EthereumTransaction } from './adapters/ethereum.js';
import type { FinConfig, Adapter } from './core/types.js';
import { FinError } from './core/errors.js';
export function createAdapters(config: FinConfig, secrets?: Record<string, string>): Adapter[] {
  if (secrets)
    for (const connection of config.connections.filter(
      (c) => c.kind === 'ethereum' && c.credential,
    )) {
      const key = secrets[connection.credential!];
      if (!key)
        throw new FinError(
          'SIGNER_UNAVAILABLE',
          'setup',
          'A configured protected signer is missing.',
          'Provision all configured forward and return signers.',
        );
      let address: string;
      try {
        address = privateKeyToAccount((key.startsWith('0x') ? key : `0x${key}`) as Hex).address;
      } catch {
        throw new FinError(
          'SIGNER_INVALID',
          'setup',
          'A protected signer could not be validated.',
          'Correct private provisioning without sharing the key.',
        );
      }
      if (address.toLowerCase() !== connection.address?.toLowerCase())
        throw new FinError(
          'SIGNER_MISMATCH',
          'setup',
          'Protected signer and configured address differ.',
          'Correct the protected wallet metadata before initiating a test.',
        );
    }
  const publicClient = createPublicClient({
    chain: mainnet,
    transport: fallback(
      config.ethereum.rpcUrls.map((u) => http(u, { timeout: 15000, retryCount: 0 })),
    ),
  });
  const ethereum = new EthereumAdapter(config, {
    publicClient,
    ...(secrets
      ? {
          signTransaction: async (tx: EthereumTransaction, wallet: string): Promise<Hex> => {
            const connection = config.connections.find(
              (c) => c.name === wallet && c.kind === 'ethereum',
            );
            const key = connection?.credential ? secrets[connection.credential] : undefined;
            if (!key)
              throw new FinError(
                'SIGNER_UNAVAILABLE',
                'signing',
                'No protected signer is configured for this wallet.',
                'Provision the selected signer through human-admin setup.',
              );
            const account = privateKeyToAccount((key.startsWith('0x') ? key : `0x${key}`) as Hex);
            if (account.address.toLowerCase() !== tx.account.toLowerCase())
              throw new FinError(
                'SIGNER_MISMATCH',
                'signing',
                'Signer differs from the configured wallet.',
                'Correct protected configuration; do not substitute a different wallet.',
              );
            return account.signTransaction({
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
        }
      : {}),
  });
  const clients: Record<string, BinanceClient> = {};
  if (secrets)
    for (const c of config.connections.filter((c) => c.kind === 'binance')) {
      if (!c.credential) continue;
      const apiKey = secrets[`${c.credential}_API_KEY`],
        pem = secrets[`${c.credential}_API_PEM`];
      if (apiKey && pem)
        clients[c.name] = new ccxt.binance({
          apiKey,
          secret: pem,
          enableRateLimit: true,
          timeout: 20000,
          verbose: false,
        }) as unknown as BinanceClient;
    }
  const verifier = createBinanceRecipientVerifier(publicClient);
  const binance = new BinanceAdapter(config, clients, {
    recipientVerifier: verifier,
    priceProvider: {
      quote: async (coin) => {
        const token =
          coin === 'ETH'
            ? 'native'
            : coin === 'USDC'
              ? '0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48'
              : coin === 'USDT'
                ? '0xdAC17F958D2ee523a2206206994597C13D831ec7'
                : coin === 'DAI'
                  ? '0x6B175474E89094C44Da98b954EedeAC495271d0F'
                  : undefined;
        if (!token)
          throw new FinError(
            'VALUATION_UNAVAILABLE',
            'budget',
            'No reviewed USD oracle mapping exists for this asset.',
            'Configure and verify a price source before live verification.',
          );
        const price = await ethereum.quoteUsd(token);
        return {
          usdPerUnit: formatUnits(BigInt(price.priceUsdMicros), 6),
          observedAt: price.observedAt,
          source: price.source,
        };
      },
    },
  });
  return [binance, ethereum];
}

/** Read-only payout verification; no credentials, authorizer, or signing interface. */
export function createBinanceRecipientVerifier(
  publicClient: Pick<
    PublicClient,
    'getChainId' | 'getTransactionReceipt' | 'getBlock' | 'getBlockNumber' | 'getTransaction'
  >,
): BinanceRecipientVerifier {
  return {
    verify: async (p) => {
      try {
        const isHash = (value: unknown): value is Hex =>
          typeof value === 'string' && /^0x[0-9a-fA-F]{64}$/.test(value);
        if (
          !isHash(p.txHash) ||
          p.chainId !== 1 ||
          !Number.isSafeInteger(p.confirmations) ||
          p.confirmations < 1
        )
          return {
            status: 'unknown',
            evidence: { reason: 'Invalid payout verification identity or finality requirement' },
          };
        if ((await publicClient.getChainId()) !== 1)
          return { status: 'unknown', evidence: { reason: 'RPC chain mismatch' } };
        const hash = p.txHash as Hex;
        const receipt = await publicClient.getTransactionReceipt({ hash });
        if (
          !isHash(receipt.transactionHash) ||
          receipt.transactionHash.toLowerCase() !== hash.toLowerCase() ||
          !isHash(receipt.blockHash) ||
          typeof receipt.blockNumber !== 'bigint' ||
          receipt.blockNumber < 0n ||
          (receipt.status !== 'success' && receipt.status !== 'reverted')
        )
          return {
            status: 'unknown',
            evidence: {
              reason:
                'Payout receipt identity or fields are invalid; preserve original withdrawal identity',
            },
          };
        const canonical = await publicClient.getBlock({ blockNumber: receipt.blockNumber });
        const head = await publicClient.getBlockNumber();
        if (
          typeof head !== 'bigint' ||
          head < 0n ||
          canonical.number !== receipt.blockNumber ||
          !isHash(canonical.hash) ||
          canonical.hash.toLowerCase() !== receipt.blockHash.toLowerCase()
        )
          return { status: 'unknown', evidence: { reason: 'Receipt is no longer canonical' } };
        const confirmations = head >= receipt.blockNumber ? head - receipt.blockNumber + 1n : 0n;
        if (confirmations < BigInt(p.confirmations))
          return {
            status: 'pending',
            evidence: { transactionHash: hash, confirmations: confirmations.toString() },
          };
        if (receipt.status === 'reverted')
          return {
            status: 'failed',
            evidence: {
              transactionHash: hash,
              receiptStatus: 'reverted',
              confirmations: confirmations.toString(),
            },
          };
        let received = 0n;
        if (!p.asset.tokenAddress) {
          const tx = await publicClient.getTransaction({ hash });
          if (
            !isHash(tx.hash) ||
            tx.hash.toLowerCase() !== hash.toLowerCase() ||
            tx.blockNumber !== receipt.blockNumber ||
            !isHash(tx.blockHash) ||
            tx.blockHash.toLowerCase() !== receipt.blockHash.toLowerCase() ||
            typeof tx.value !== 'bigint' ||
            tx.value < 0n
          )
            return {
              status: 'unknown',
              evidence: {
                reason: 'Native payout transaction differs from the confirmed receipt identity',
              },
            };
          if (tx.to?.toLowerCase() !== p.recipient.toLowerCase())
            return {
              status: 'unknown',
              evidence: {
                reason:
                  'Native receipt requires a direct transfer; internal/batched transfers need a trace-capable verifier',
              },
            };
          received = tx.value;
        } else
          for (const log of receipt.logs) {
            if (log.address.toLowerCase() !== p.asset.tokenAddress.toLowerCase()) continue;
            try {
              const event = decodeEventLog({
                abi: parseAbi([
                  'event Transfer(address indexed from,address indexed to,uint256 value)',
                ]),
                data: log.data,
                topics: log.topics,
              });
              if (event.args.to.toLowerCase() === p.recipient.toLowerCase())
                received += event.args.value;
            } catch {}
          }
        const amount = formatUnits(received, p.asset.decimals);
        const { exactUnits } = await import('./core/canonical.js');
        if (
          received < exactUnits(p.minimumAmount, p.asset.decimals) ||
          received > exactUnits(p.maximumAmount, p.asset.decimals)
        )
          return {
            status: 'unknown',
            evidence: {
              reason: 'Recipient amount differs from confirmed bounds',
              actualAmount: amount,
              transactionHash: hash,
            },
          };
        return {
          status: 'completed',
          evidence: {
            transactionHash: hash,
            recipient: p.recipient,
            asset: p.asset,
            actualUnits: received.toString(),
            actualAmount: amount,
            blockNumber: receipt.blockNumber.toString(),
            confirmations: confirmations.toString(),
            receiptStatus: receipt.status,
          },
        };
      } catch {
        return {
          status: 'unknown',
          evidence: {
            reason:
              'Mainnet receipt could not be established; preserve original withdrawal identity',
          },
        };
      }
    },
  };
}
