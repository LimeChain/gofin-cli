/** Production read-only evidence. Imports perform no file, credential, RPC or exchange reads. */
import { readFileSync, mkdirSync, writeFileSync, chmodSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { randomUUID } from 'node:crypto';
import { DatabaseSync } from 'node:sqlite';
import ccxt from 'ccxt';
import { createPublicClient, http, fallback } from 'viem';
import { mainnet } from 'viem/chains';
import { loadConfig, parseCredentials, assertUniqueConnectionIdentities } from '../src/config.js';
import { BinanceAdapter, type BinanceClient } from '../src/adapters/binance.js';
import { EthereumAdapter } from '../src/adapters/ethereum.js';
import { RunCoordinator } from '../src/core/run-coordinator.js';
import { fingerprint } from '../src/core/canonical.js';
import { FinError, publicError } from '../src/core/errors.js';
import type { OperationStore } from '../src/core/store.js';
import type { FinancialSnapshot } from '../src/core/budget.js';
import type { Adapter, FinConfig, Input, RecordOperation } from '../src/core/types.js';

const readRpcMethods = new Set([
  'eth_chainId',
  'eth_call',
  'eth_getBalance',
  'eth_getCode',
  'eth_blockNumber',
  'eth_getBlockByNumber',
  'eth_getBlockByHash',
  'eth_getTransactionCount',
  'eth_getTransactionByHash',
  'eth_getTransactionReceipt',
  'eth_getLogs',
  'eth_estimateGas',
  'eth_gasPrice',
  'eth_maxPriorityFeePerGas',
  'eth_feeHistory',
]);
const getVenuePaths = new Set([
  'private:account',
  'private:openOrders',
  'public:time',
  'sapiV4:sub-account/assets',
  'sapi:account/apiRestrictions',
  'sapi:sub-account/list',
  'sapi:capital/config/getall',
  'sapi:capital/deposit/address',
  'sapi:capital/deposit/hisrec',
  'sapi:capital/withdraw/history',
  'sapi:sub-account/universalTransfer',
]);
function fail(code: string, message: string): never {
  throw new FinError(
    code,
    'read-only-snapshot',
    message,
    'Correct read access or configuration and capture a new complete snapshot; do not use partial evidence as a baseline.',
  );
}
/** JSON-RPC uses HTTP POST for reads. Reject mutation methods, including within a mixed batch, before fetch. */
export function assertReadOnlyRpcPayload(payload: unknown): void {
  const requests = Array.isArray(payload) ? payload : [payload];
  if (
    !requests.length ||
    requests.some(
      (r) => !r || typeof r !== 'object' || !readRpcMethods.has(String((r as Input).method)),
    )
  )
    fail('READ_ONLY_RPC', 'Only reviewed read-only Ethereum JSON-RPC methods are permitted.');
}
export function assertReadOnlyVenueRequest(path: unknown, api: unknown, method: unknown): void {
  if (
    method !== 'GET' ||
    typeof path !== 'string' ||
    typeof api !== 'string' ||
    !getVenuePaths.has(`${api}:${path}`)
  )
    fail('READ_ONLY_VENUE', 'Only reviewed Binance GET endpoints are permitted.');
}
interface SnapshotEvidence {
  observedAt: string;
  environment: 'production-read-only';
  financialWrites: 0;
  authorizesLiveExecution: false;
  status: 'complete' | 'blocked';
  usableBaseline: boolean;
  snapshotComplete: boolean;
  walletsObserved: number;
  accountsObserved: number;
  configurationFingerprint: string;
  snapshot?: FinancialSnapshot;
  observations: Record<string, unknown>;
  failure?: ReturnType<typeof publicError>;
}
/** Injected adapters enable isolated fixtures without reading files, credentials or a live network. */
export async function captureSnapshot(
  config: FinConfig,
  adapters: Adapter[],
  operationState: RecordOperation[] | (() => RecordOperation[]),
): Promise<SnapshotEvidence> {
  const observations: Record<string, unknown> = {};
  const wallets = config.connections.filter((c) => c.kind === 'ethereum').map((c) => c.name);
  const accounts = config.connections.filter((c) => c.kind === 'binance').map((c) => c.name);
  const report: SnapshotEvidence = {
    observedAt: new Date().toISOString(),
    environment: 'production-read-only',
    financialWrites: 0,
    authorizesLiveExecution: false,
    status: 'blocked',
    usableBaseline: false,
    snapshotComplete: false,
    walletsObserved: wallets.length,
    accountsObserved: accounts.length,
    configurationFingerprint: fingerprint(config),
    observations,
  };
  try {
    assertUniqueConnectionIdentities(config.connections);
    const readOperations =
      typeof operationState === 'function' ? operationState : () => operationState;
    const initialOperationFingerprint = fingerprint(readOperations());
    if (!wallets.length || !accounts.length || config.ethereum.chainId !== 1)
      fail(
        'SNAPSHOT_SCOPE',
        'Configured production exchange accounts and Ethereum mainnet wallets are required.',
      );
    // Cache only successfully completed read observations. Coordinator and private detail use the same exact observation.
    const cached = adapters.map((adapter): Adapter => ({
      definitions: adapter.definitions,
      read: async (operation, input) => {
        const key = `${operation}:${fingerprint(input)}`;
        if (!(key in observations)) observations[key] = await adapter.read(operation, input);
        return observations[key];
      },
      preview: async () => fail('READ_ONLY_SNAPSHOT', 'This snapshot cannot preview writes.'),
      submit: async () => fail('READ_ONLY_SNAPSHOT', 'This snapshot cannot submit writes.'),
      reconcile: async () =>
        fail(
          'READ_ONLY_SNAPSHOT',
          'This snapshot cannot reconcile a write through an execution adapter.',
        ),
    }));
    // RunCoordinator.snapshot uses list() only. No writable OperationStore constructor is instantiated.
    const readStore = { list: readOperations } as OperationStore;
    const coordinator = new RunCoordinator(config, cached, readStore);
    const snapshot = await coordinator.snapshot(wallets, accounts);
    const binance = cached.find((a) => a.definitions.some((d) => d.name === 'binance.balances'));
    if (!binance) fail('ADAPTER_UNAVAILABLE', 'The required Binance read adapter is unavailable.');
    for (const connection of config.connections.filter((c) => c.kind === 'binance')) {
      await binance.read('binance.permissions', { account: connection.name });
      if (connection.role === 'main') {
        await binance.read('binance.hierarchy', { authority: connection.name });
        await binance.read('binance.networks', { account: connection.name });
      }
    }
    // Interrupted or currently awaiting human authorization is not an empty pending-state baseline.
    const currentOperations = readOperations();
    if (fingerprint(currentOperations) !== initialOperationFingerprint)
      fail(
        'SNAPSHOT_STATE_CHANGED',
        'Durable operation state changed during the observation; capture a new stable read-only snapshot.',
      );
    for (const r of currentOperations)
      if (r.status === 'awaiting_approval') snapshot.pending.push(`fin:${r.id}`);
    report.snapshot = snapshot;
    report.snapshotComplete = true;
    if (snapshot.pending.length)
      fail(
        'PENDING_BASELINE',
        'The complete read-only snapshot contains unresolved or awaiting financial operations.',
      );
    report.status = 'complete';
    report.usableBaseline = true;
  } catch (error) {
    report.failure = publicError(error);
    report.status = 'blocked';
    report.usableBaseline = false;
  }
  return report;
}
function readOperationState(path: string): { records(): RecordOperation[]; close(): void } {
  let db: DatabaseSync;
  try {
    db = new DatabaseSync(path, { readOnly: true });
  } catch {
    return fail(
      'OPERATION_STORE_UNAVAILABLE',
      'The required operation store could not be opened read-only. Protected storage is intentionally human-only; installed workflows capture their authoritative baseline through the broker.',
    );
  }
  try {
    const records = (): RecordOperation[] => {
      try {
        const rows = db.prepare('SELECT record FROM operations ORDER BY rowid DESC').all() as {
          record: string;
        }[];
        const result = rows.map((row) => JSON.parse(row.record) as RecordOperation);
        if (
          result.some(
            (r) =>
              !r ||
              typeof r.id !== 'string' ||
              ![
                'awaiting_approval',
                'rejected',
                'expired',
                'cancelled',
                'submitted',
                'pending',
                'completed',
                'failed',
                'unknown',
              ].includes(r.status),
          )
        )
          fail('OPERATION_STATE_INVALID', 'The durable operation state is incomplete or invalid.');
        return result;
      } catch {
        return fail(
          'OPERATION_STORE_UNAVAILABLE',
          'The required complete operation state could not be read.',
        );
      }
    };
    records();
    return { records, close: () => db.close() };
  } catch {
    db.close();
    return fail(
      'OPERATION_STORE_UNAVAILABLE',
      'The required complete operation state could not be read.',
    );
  }
}
function createReadOnlyAdapters(
  config: FinConfig,
  secrets: Record<string, string>,
): { adapters: Adapter[]; close(): Promise<void> } {
  const publicClient = createPublicClient({
    chain: mainnet,
    transport: fallback(
      config.ethereum.rpcUrls.map((url) =>
        http(url, {
          timeout: 15000,
          retryCount: 0,
          onFetchRequest: async (request) => assertReadOnlyRpcPayload(await request.clone().json()),
        }),
      ),
      { rank: false, retryCount: 0 },
    ),
  });
  // Configured public addresses suffice. Never import/derive a private account or instantiate signing options.
  const ethereum = new EthereumAdapter(config, { publicClient });
  const clients: Record<string, BinanceClient> = {};
  const venues: Array<{ close(): Promise<unknown> }> = [];
  for (const connection of config.connections.filter((c) => c.kind === 'binance')) {
    const prefix = connection.credential;
    const apiKey = prefix ? secrets[`${prefix}_API_KEY`] : undefined;
    const pem = prefix ? secrets[`${prefix}_API_PEM`] : undefined;
    if (!apiKey || !pem)
      fail(
        'BINANCE_READ_ACCESS_MISSING',
        'Every selected account requires its configured read credential; master authority cannot substitute for child private histories or orders.',
      );
    const venue = new ccxt.binance({
      apiKey,
      secret: pem,
      enableRateLimit: true,
      timeout: 20000,
      verbose: false,
    });
    const originalRequest = venue.request.bind(venue);
    venue.request = async (path, api, method, params, ...rest) => {
      assertReadOnlyVenueRequest(path, api, method);
      return originalRequest(path, api, method, params, ...rest);
    };
    venues.push(venue);
    clients[connection.name] = venue as unknown as BinanceClient;
  }
  return {
    adapters: [new BinanceAdapter(config, clients), ethereum],
    close: async () => {
      await Promise.allSettled(venues.map((v) => v.close()));
    },
  };
}
function saveEvidence(outputDir: string, report: unknown): void {
  mkdirSync(outputDir, { recursive: true, mode: 0o700 });
  chmodSync(outputDir, 0o700);
  const path = resolve(
    outputDir,
    `snapshot-${new Date().toISOString().replace(/[:.]/g, '-')}-${randomUUID()}.json`,
  );
  writeFileSync(
    path,
    JSON.stringify(
      report,
      (_key, value) => (typeof value === 'bigint' ? value.toString() : value),
      2,
    ),
    { mode: 0o600, flag: 'wx' },
  );
}
async function cli(): Promise<void> {
  if (process.argv.includes('--help')) {
    console.log(
      'Read-only evidence: --config .context/connections.json --credentials .context/.ai.env --store /Library/Fin/private/operations.sqlite --output-dir .context/test-runs. Installed authoritative baselines use fin run begin through the protected broker.',
    );
    return;
  }
  const options: Record<string, string> = {
    '--config': '.context/connections.json',
    '--credentials': '.context/.ai.env',
    '--store': '/Library/Fin/private/operations.sqlite',
    '--output-dir': '.context/test-runs',
  };
  for (let i = 2; i < process.argv.length; i += 2) {
    const flag = process.argv[i]!,
      value = process.argv[i + 1];
    if (!(flag in options) || !value || value.startsWith('--'))
      fail('SNAPSHOT_ARGUMENTS', 'Snapshot options require an explicit supported path value.');
    options[flag] = value;
  }
  let privateReportSaved = false;
  let state: ReturnType<typeof readOperationState> | undefined;
  let runtime: ReturnType<typeof createReadOnlyAdapters> | undefined;
  let report: SnapshotEvidence | undefined;
  try {
    const config = loadConfig(resolve(options['--config']!));
    state = readOperationState(resolve(options['--store']!));
    runtime = createReadOnlyAdapters(
      config,
      parseCredentials(readFileSync(resolve(options['--credentials']!), 'utf8')),
    );
    report = await captureSnapshot(config, runtime.adapters, state.records);
    saveEvidence(resolve(options['--output-dir']!), report);
    privateReportSaved = true;
  } catch (error) {
    try {
      saveEvidence(resolve(options['--output-dir']!), {
        status: 'blocked',
        snapshotComplete: false,
        usableBaseline: false,
        authorizesLiveExecution: false,
        financialWrites: 0,
        observedAt: new Date().toISOString(),
        failure: publicError(error),
      });
      privateReportSaved = true;
    } catch {}
  } finally {
    await runtime?.close();
    state?.close();
  }
  console.log(
    JSON.stringify({
      blocked: report?.status !== 'complete',
      snapshotComplete: report?.snapshotComplete ?? false,
      usableReadOnlyEvidence: report?.usableBaseline ?? false,
      authorizesLiveExecution: false,
      financialWrites: 0,
      privateReportSaved,
      walletCount: report?.walletsObserved ?? 0,
      accountCount: report?.accountsObserved ?? 0,
      pendingClear: !!report?.snapshot && report.snapshot.pending.length === 0,
    }),
  );
  if (report?.status !== 'complete') process.exitCode = 1;
}
if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url)
  cli().catch(() => {
    console.log(
      JSON.stringify({
        blocked: true,
        snapshotComplete: false,
        usableReadOnlyEvidence: false,
        authorizesLiveExecution: false,
        financialWrites: 0,
        privateReportSaved: false,
      }),
    );
    process.exitCode = 1;
  });
