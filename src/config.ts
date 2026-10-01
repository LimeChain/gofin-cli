import { readFileSync } from 'node:fs';
import { z } from 'zod';
import type { Connection, FinConfig } from './core/types.js';
import { FinError } from './core/errors.js';
export const MAINNET: FinConfig['ethereum'] = {
  rpcUrls: ['https://ethereum-rpc.publicnode.com', 'https://cloudflare-eth.com'],
  chainId: 1,
  confirmations: 3,
  uniswapRouter: '0xE592427A0AEce92De3Edee1F18E0157C05861564',
  uniswapQuoter: '0x61fFE014bA17989E743c5F6cB21bF9697530B21e',
  uniswapFactory: '0x1F98431c8aD98523631AE4a59f267346ea31F984',
  aavePool: '0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2',
  aaveOracle: '0x54586bE62E3c3580375aE3723C145253060Ca0C2',
};
const address = z.string().regex(/^0x[0-9a-fA-F]{40}$/);
export const configSchema = z
  .object({
    version: z.literal(1),
    connections: z.array(
      z
        .object({
          name: z.string().min(1),
          kind: z.enum(['binance', 'ethereum']),
          role: z.enum(['main', 'sub']).optional(),
          parent: z.string().optional(),
          email: z.string().optional(),
          address: address.optional(),
          credential: z.string().optional(),
          walletType: z.literal('SPOT').optional(),
        })
        .strict(),
    ),
    ethereum: z
      .object({
        rpcUrls: z.array(z.url()).min(1),
        chainId: z.literal(1),
        confirmations: z.number().int().min(1),
        uniswapRouter: address,
        uniswapQuoter: address,
        uniswapFactory: address,
        aavePool: address,
        aaveOracle: address,
      })
      .strict(),
  })
  .strict();
/** Reject account aliases before they can count one source holding more than once. */
export function assertUniqueConnectionIdentities(connections: Connection[]): void {
  const names = new Set<string>();
  const ethereumAddresses = new Set<string>();
  const binanceChildren = new Set<string>();
  const credentials = new Set<string>();
  const duplicate = (code: string, message: string): never => {
    throw new FinError(
      code,
      'configuration',
      message,
      'Use one configured name for each account or signer identity.',
    );
  };
  for (const connection of connections) {
    if (names.has(connection.name))
      duplicate('DUPLICATE_CONNECTION_NAME', 'Configured connection names must be unique.');
    names.add(connection.name);
    if (connection.kind === 'ethereum' && connection.address) {
      const identity = connection.address.toLowerCase();
      if (ethereumAddresses.has(identity))
        duplicate(
          'DUPLICATE_WALLET_IDENTITY',
          'An Ethereum wallet cannot be configured under multiple names.',
        );
      ethereumAddresses.add(identity);
    }
    if (
      connection.kind === 'binance' &&
      connection.role === 'sub' &&
      connection.parent &&
      connection.email
    ) {
      const identity = JSON.stringify([connection.parent, connection.email.trim().toLowerCase()]);
      if (binanceChildren.has(identity))
        duplicate(
          'DUPLICATE_CHILD_IDENTITY',
          'A Binance child account cannot be configured under multiple names within one authority.',
        );
      binanceChildren.add(identity);
    }
    if (connection.credential?.trim()) {
      const identity = JSON.stringify([
        connection.kind,
        connection.role ?? null,
        connection.credential.trim(),
      ]);
      if (credentials.has(identity))
        duplicate(
          'DUPLICATE_SIGNER_IDENTITY',
          'A signer credential cannot be reused under multiple names of the same connection kind and role.',
        );
      credentials.add(identity);
    }
  }
}
export function loadConfig(path: string): FinConfig {
  const config = configSchema.parse(JSON.parse(readFileSync(path, 'utf8')));
  assertUniqueConnectionIdentities(config.connections);
  for (const url of config.ethereum.rpcUrls) {
    const u = new URL(url);
    if (u.protocol !== 'https:' || u.username || u.password || u.search)
      throw new Error('RPC must be HTTPS without embedded credentials');
  }
  return config;
}
/** Import provisioning as data, never source a credential file as shell code. */
export function parseCredentials(text: string): Record<string, string> {
  const result: Record<string, string> = {};
  const pattern =
    /^\s*(?:export\s+)?([A-Z][A-Z0-9_]*)\s*=\s*("(?:[^"\\]|\\[\s\S])*"|'[^']*'|[^\r\n]*)\s*$/gm;
  for (const match of text.matchAll(pattern)) {
    let value = match[2]!.trim();
    if (value.startsWith('"'))
      value = value
        .slice(1, -1)
        .replace(/\\n/g, '\n')
        .replace(/\\r/g, '\r')
        .replace(/\\"/g, '"')
        .replace(/\\\\/g, '\\');
    else if (value.startsWith("'")) value = value.slice(1, -1);
    if (match[1]!.endsWith('_PEM')) value = value.replace(/\\n/g, '\n').replace(/\\r/g, '\r');
    result[match[1]!] = value;
  }
  return result;
}
