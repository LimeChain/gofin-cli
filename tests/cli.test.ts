import test from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { binanceDefinitions } from '../src/adapters/binance.js';
import { ethereumDefinitions } from '../src/adapters/ethereum.js';
import { parseCredentials } from '../src/config.js';
function cli(args: string[]) {
  const result = spawnSync(process.execPath, ['--import', 'tsx', 'src/cli.ts', ...args], {
    encoding: 'utf8',
  });
  return { status: result.status, data: JSON.parse(result.stdout) };
}
test('fresh CLI discovers implemented operations and remaining concept inventory', () => {
  const c = cli(['capabilities', '--json']);
  assert.equal(c.status, 0);
  assert.ok(c.data.implemented.some((d: any) => d.name === 'aave.repay'));
  assert.ok(c.data.remainingInventory.includes('Kraken funding journey'));
  assert.ok(c.data.deferred.includes('MCP'));
});
test('CLI help includes doctor, finish, recovery and actual commands', () => {
  const c = cli(['--help']);
  assert.ok(c.data.commands.includes('fin doctor --json'));
  assert.ok(c.data.commands.includes('fin run finish RUN_ID --json'));
});
test('all discoverable examples validate against shared strict schemas', () => {
  for (const def of [...binanceDefinitions, ...ethereumDefinitions])
    for (const example of def.examples)
      assert.equal(def.schema.safeParse(example).success, true, def.name);
});
test('CLI explains field semantics and emits executable JSON schema', () => {
  const c = cli(['schema', 'aave.repay', '--json']);
  assert.equal(c.status, 0);
  assert.equal(c.data.kind, 'write');
  assert.equal(c.data.schema.additionalProperties, false);
  assert.ok(c.data.schema.properties.requestId);
});
test('source-mode financial execution fails without touching signer or credentials', () => {
  const c = cli([
    'execute',
    'wallet.wrap',
    '--input',
    '{"requestId":"synthetic","wallet":"main-wallet","network":"ethereum","chainId":1,"amount":"1","maxFeeEth":"1"}',
    '--config',
    'config.example.json',
  ]);
  assert.equal(c.status, 1);
  assert.equal(c.data.error.code, 'PROTECTED_INSTALLATION_REQUIRED');
});
test('credential importer handles quoted/multiline and escaped PEM but never executes shell code', () => {
  const env = parseCredentials(
    'BINANCE_MASTER_ACCOUNT_API_PEM=-----BEGIN PRIVATE KEY-----\\nSYNTHETIC\\n-----END PRIVATE KEY-----\nTEST="hello\\nworld"\nCOMMAND=$(touch /tmp/fin-must-never-execute)\n',
  );
  assert.ok(env.BINANCE_MASTER_ACCOUNT_API_PEM.includes('\nSYNTHETIC\n'));
  assert.equal(env.TEST, 'hello\nworld');
  assert.equal(env.COMMAND, '$(touch /tmp/fin-must-never-execute)');
});
