import { createServer } from 'node:net';
import { readFileSync, chmodSync, chownSync, lstatSync, unlinkSync } from 'node:fs';
import { z } from 'zod';
import { validateProtectedInstallation } from './security/protected-installation.js';
import { TouchIdAuthorizer } from './security/touch-id.js';
import { loadConfig, parseCredentials } from './config.js';
import { createAdapters } from './runtime.js';
import { OperationStore } from './core/store.js';
import { OperationEngine } from './core/engine.js';
import { RunCoordinator } from './core/run-coordinator.js';
import { publicError, FinError } from './core/errors.js';
const paths = {
  configPath: '/Library/Fin/private/config.json',
  credentialsPath: '/Library/Fin/private/credentials.env',
  storePath: '/Library/Fin/private/operations.sqlite',
  policyPath: '/Library/Fin/private/approval.json',
  helperPath: '/Library/Fin/runtime/native/fin-touch-id',
};
const envelope = z.discriminatedUnion('action', [
  z
    .object({
      action: z.enum(['read', 'preview', 'execute']),
      operation: z.string(),
      input: z.record(z.string(), z.unknown()),
      runId: z.string().optional(),
    })
    .strict(),
  z
    .object({ action: z.literal('status'), id: z.string(), reconcile: z.boolean().default(false) })
    .strict(),
  z.object({ action: z.enum(['operations', 'connections', 'doctor']) }).strict(),
  z.object({ action: z.literal('run.begin'), plan: z.record(z.string(), z.unknown()) }).strict(),
  z.object({ action: z.enum(['run.status', 'run.finish']), id: z.string() }).strict(),
]);
async function start() {
  process.umask(0o077);
  const protectedInstall = await validateProtectedInstallation(paths);
  if (!protectedInstall.ok)
    throw new FinError(
      'PROTECTED_INSTALLATION_REQUIRED',
      'startup',
      'Protected installation validation failed.',
      'Review installation ownership, permissions, signature and runtime.',
    );
  // No credential read happens before the installation and execution boundary is checked.
  const config = loadConfig(paths.configPath);
  const secrets = parseCredentials(readFileSync(paths.credentialsPath, 'utf8'));
  const adapters = createAdapters(config, secrets);
  const store = new OperationStore(paths.storePath);
  const authorizer = new TouchIdAuthorizer(paths);
  const runs = new RunCoordinator(config, adapters, store);
  const engine = new OperationEngine(store, adapters, authorizer, {
    liveTestOnly: true,
    revalueAllocation: (id) => runs.revalueAllocation(id),
    enrichReconciliation: (record) => runs.enrichReconciliation(record),
  });
  engine.recoverInterrupted();
  const policy = JSON.parse(readFileSync(paths.policyPath, 'utf8'));
  const socketPath = '/Library/Fin/private/fin.sock';
  try {
    const s = lstatSync(socketPath);
    if (!s.isSocket()) throw new Error('Unexpected socket path');
    unlinkSync(socketPath);
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code !== 'ENOENT') throw e;
  }
  const server = createServer((socket) => {
    let buffer = '',
      handled = false;
    socket.setTimeout(150000, () => socket.destroy());
    socket.on('data', async (chunk) => {
      if (handled) return;
      buffer += chunk.toString('utf8');
      if (Buffer.byteLength(buffer) > 65536) {
        handled = true;
        socket.destroy();
        return;
      }
      if (!buffer.includes('\n')) return;
      handled = true;
      const respond = (response: unknown) =>
        socket.end(
          JSON.stringify(response, (_k, v) => (typeof v === 'bigint' ? v.toString() : v)) + '\n',
        );
      try {
        let raw: unknown;
        try {
          raw = JSON.parse(buffer.split('\n')[0]!);
        } catch {
          throw new FinError(
            'INVALID_JSON',
            'transport',
            'Request must be one JSON message.',
            'Use the Fin CLI.',
          );
        }
        const parsed = envelope.safeParse(raw);
        if (!parsed.success)
          throw new FinError(
            'INVALID_REQUEST',
            'transport',
            'The requested broker action is not allowed.',
            'Use the discoverable Fin operations.',
          );
        const request = parsed.data;
        let result: unknown;
        switch (request.action) {
          case 'read':
            result = await engine.read(request.operation, request.input);
            break;
          case 'preview':
            result = await engine.preview(request.operation, request.input);
            break;
          case 'execute':
            result = await engine.execute(request.operation, request.input, request.runId);
            break;
          case 'status':
            result = await engine.status(request.id, request.reconcile);
            break;
          case 'operations':
            result = store.list();
            break;
          case 'connections':
            result = config.connections.map(({ credential, email, ...metadata }) => ({
              ...metadata,
              permissions:
                'query with binance.permissions; configured role is not observed authority',
            }));
            break;
          case 'doctor':
            result = await validateProtectedInstallation(paths);
            break;
          case 'run.begin':
            result = await engine.serialize(() => runs.begin(request.plan));
            break;
          case 'run.status':
            result = store.getRun(request.id) ?? { code: 'RUN_NOT_FOUND' };
            break;
          case 'run.finish':
            result = await engine.serialize(() => runs.finish(request.id));
            break;
        }
        respond({ ok: true, result });
      } catch (error) {
        respond({ ok: false, error: publicError(error) });
      }
    });
    socket.on('error', () => {});
  });
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject);
    server.listen(socketPath, () => {
      chownSync(socketPath, policy.approverUid, -1);
      chmodSync(socketPath, 0o600);
      resolve();
    });
  });
  process.on('SIGTERM', () =>
    server.close(() => {
      store.close();
      process.exit(0);
    }),
  );
}
process.on('uncaughtException', () => {
  console.error('Fin broker stopped after an internal error; private details withheld.');
  process.exit(1);
});
start().catch((error) => {
  console.error(JSON.stringify({ ok: false, error: publicError(error) }));
  process.exit(1);
});
