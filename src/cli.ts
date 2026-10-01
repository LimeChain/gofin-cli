#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { createConnection } from 'node:net';
import { z } from 'zod';
import { binanceDefinitions } from './adapters/binance.js';
import { ethereumDefinitions } from './adapters/ethereum.js';
import { createAdapters } from './runtime.js';
import { loadConfig } from './config.js';
import { FinError, publicError } from './core/errors.js';
import { planSchema } from './core/run-coordinator.js';
import type { Input } from './core/types.js';
const definitions = [...binanceDefinitions, ...ethereumDefinitions];
const args = process.argv.slice(2);
const command = args.shift();
function flag(name: string): string | undefined {
  const i = args.indexOf(name);
  if (i < 0) return undefined;
  const value = args[i + 1];
  if (!value || value.startsWith('--'))
    throw new FinError('INVALID_ARGUMENT', 'cli', `Missing value for ${name}.`, 'Use fin --help.');
  args.splice(i, 2);
  return value;
}
function has(name: string): boolean {
  const i = args.indexOf(name);
  if (i < 0) return false;
  args.splice(i, 1);
  return true;
}
function output(value: unknown) {
  process.stdout.write(
    JSON.stringify(value, (_k, v) => (typeof v === 'bigint' ? v.toString() : v), 2) + '\n',
  );
}
function readInput(): Input {
  const json = flag('--input'),
    file = flag('--file');
  if (Boolean(json) === Boolean(file))
    throw new FinError(
      'INPUT_REQUIRED',
      'cli',
      'Choose exactly one of --input JSON or --file PATH.',
      'Run fin schema for the operation.',
    );
  let input: unknown;
  try {
    input = JSON.parse(json ?? readFileSync(file!, 'utf8'));
  } catch {
    throw new FinError(
      'INVALID_JSON',
      'cli',
      'The input is not valid JSON.',
      'Provide an operation object.',
    );
  }
  if (!input || typeof input !== 'object' || Array.isArray(input))
    throw new FinError(
      'INVALID_INPUT',
      'cli',
      'Input must be an object.',
      'Use the operation schema.',
    );
  return input as Input;
}
export function brokerRequest(request: unknown): Promise<unknown> {
  return new Promise((resolve, reject) => {
    const socket = createConnection('/Library/Fin/private/fin.sock');
    let data = '',
      settled = false;
    const fail = (error: FinError) => {
      if (!settled) {
        settled = true;
        reject(error);
      }
      socket.destroy();
    };
    socket.setTimeout(120000);
    socket.on('connect', () => socket.write(JSON.stringify(request) + '\n'));
    socket.on('data', (chunk) => {
      data += chunk.toString();
      if (Buffer.byteLength(data) > 4 * 1024 * 1024) {
        fail(
          new FinError(
            'RESPONSE_LIMIT',
            'transport',
            'Broker response exceeded the response limit.',
            'Use a narrower read.',
          ),
        );
        return;
      }
      if (data.includes('\n') && !settled) {
        settled = true;
        socket.end();
        try {
          const response = JSON.parse(data.split('\n')[0]!);
          if (response.ok) resolve(response.result);
          else
            reject(
              new FinError(
                response.error.code,
                response.error.stage,
                response.error.message,
                response.error.nextAction,
              ),
            );
        } catch {
          reject(
            new FinError(
              'BROKER_RESPONSE_UNKNOWN',
              'transport',
              'The broker returned an invalid response.',
              'Inspect and reconcile the original request identity.',
            ),
          );
        }
      }
    });
    socket.on('timeout', () =>
      fail(
        new FinError(
          'BROKER_TIMEOUT',
          'transport',
          'The broker response is unknown.',
          'Inspect operations and reconcile the original request identity.',
        ),
      ),
    );
    socket.on('error', () =>
      fail(
        new FinError(
          'BROKER_UNAVAILABLE',
          'transport',
          'The protected Fin broker is unavailable.',
          'Follow human-admin setup in docs/security.md; source reads/previews can use --config.',
        ),
      ),
    );
    socket.on('close', () => {
      if (!settled)
        fail(
          new FinError(
            'BROKER_RESPONSE_UNKNOWN',
            'transport',
            'The broker closed before its outcome was returned.',
            'Inspect and reconcile the original request identity before retrying.',
          ),
        );
    });
  });
}

async function main() {
  has('--json');
  if (!command || command === '--help' || command === 'help') {
    output({
      product: 'Fin',
      version: '0.1.0-beta.1',
      commands: [
        'fin capabilities --json',
        'fin connections --json',
        'fin explain OPERATION --json',
        'fin schema OPERATION --json',
        'fin read OPERATION --file INPUT.json [--config CONFIG.json] --json',
        'fin preview OPERATION --file INPUT.json [--config CONFIG.json] --json',
        'fin execute OPERATION --file INPUT.json --run RUN_ID --json',
        'fin status OPERATION_ID [--reconcile] --json',
        'fin operations --json',
        'fin run schema --json',
        'fin run begin --file PLAN.json --json',
        'fin run status RUN_ID --json',
        'fin run finish RUN_ID --json',
        'fin doctor --json',
      ],
      notes: [
        'Source-mode reads/previews never load credentials or sign; production writes require the protected broker and fresh Touch ID.',
        'Use one stable requestId per logical write; after timeout inspect status and reconcile.',
        'Live verification requires a baseline, a complete funded unwind, and the $100 allocation.',
      ],
      documentation: 'docs/security.md',
    });
    return;
  }
  if (command === 'capabilities') {
    output({
      implemented: definitions.map(({ schema, ...d }) => d),
      remainingInventory: [
        'CCXT trading/funding across other exchanges except Lighter',
        'Kraken funding journey',
        'Derive',
        'Solana wallets',
        'Jupiter',
        'Kamino',
      ],
      deferred: ['Lighter', 'general chain-to-chain bridging', 'MCP', 'unattended/plan approval'],
      approval: 'fresh Touch ID for every write',
      coverage: 'See docs/reports/foundation.md; implementation does not imply live verification.',
    });
    return;
  }
  if (command === 'explain' || command === 'schema') {
    const name = args.shift();
    const d = definitions.find((d) => d.name === name);
    if (!d)
      throw new FinError(
        'UNSUPPORTED_OPERATION',
        'discovery',
        'Unknown operation.',
        'Use fin capabilities.',
      );
    output(
      command === 'schema'
        ? {
            operation: d.name,
            kind: d.kind,
            schema: z.toJSONSchema(d.schema, { unrepresentable: 'any' }),
            examples: d.examples,
          }
        : (({ schema, ...description }) => description)(d),
    );
    return;
  }
  if (['read', 'preview', 'execute'].includes(command)) {
    const operation = args.shift();
    const input = readInput();
    const configPath = flag('--config'),
      runId = flag('--run');
    const dryRun = has('--dry-run');
    if (args.length)
      throw new FinError('INVALID_ARGUMENT', 'cli', 'Unknown CLI argument.', 'Use fin --help.');
    const action = dryRun ? 'preview' : command;
    if (configPath) {
      if (action === 'execute')
        throw new FinError(
          'PROTECTED_INSTALLATION_REQUIRED',
          'approval',
          'Source mode cannot perform financial writes.',
          'Use the protected broker.',
        );
      const config = loadConfig(configPath);
      const adapter = createAdapters(config).find((a) =>
        a.definitions.some((d) => d.name === operation),
      );
      if (!adapter)
        throw new FinError(
          'UNSUPPORTED_OPERATION',
          'discovery',
          'Unknown operation.',
          'Use fin capabilities.',
        );
      const d = adapter.definitions.find((d) => d.name === operation)!;
      if ((action === 'read') !== (d.kind === 'read'))
        throw new FinError(
          'WRONG_ACTION',
          'validation',
          'Read/write classification differs from selected CLI action.',
          'Use fin explain.',
        );
      output(
        action === 'read'
          ? await adapter.read(operation!, input)
          : await adapter.preview(operation!, input),
      );
    } else output(await brokerRequest({ action, operation, input, ...(runId ? { runId } : {}) }));
    return;
  }
  if (command === 'status') {
    const id = args.shift();
    const reconcile = has('--reconcile');
    output(await brokerRequest({ action: 'status', id, reconcile }));
    return;
  }
  if (command === 'operations' || command === 'connections' || command === 'doctor') {
    output(await brokerRequest({ action: command }));
    return;
  }
  if (command === 'run') {
    const action = args.shift();
    if (action === 'schema') {
      output({
        schema: z.toJSONSchema(planSchema),
        example: {
          id: 'chosen-run',
          allocations: { 'main-wallet/holding/native': '16000000000000000' },
          borrowReserveUsdMicros: '4000000',
          costReserveUsdMicros: '15000000',
          wallets: ['main-wallet'],
          accounts: [],
          returnRoutes: [],
          unwind: [
            'Repay all test debt; withdraw principal and accrued supply dust; restore allowances; unwrap and return all displaced funds.',
          ],
        },
        units:
          'allocations are integer asset base units; USD reserves are integer microdollars. Selected allocation is independently valued and must fund the complete return.',
      });
      return;
    }
    if (action === 'begin') {
      output(await brokerRequest({ action: 'run.begin', plan: readInput() }));
      return;
    }
    if (action === 'status' || action === 'finish') {
      output(await brokerRequest({ action: `run.${action}`, id: args.shift() }));
      return;
    }
  }
  throw new FinError('INVALID_COMMAND', 'cli', 'Unknown command.', 'Use fin --help.');
}
main().catch((error) => {
  output({ ok: false, error: publicError(error) });
  process.exitCode = 1;
});
