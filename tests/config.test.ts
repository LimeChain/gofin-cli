import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { loadConfig, assertUniqueConnectionIdentities, MAINNET } from '../src/config.js';
import { publicError } from '../src/core/errors.js';
import type { Connection } from '../src/core/types.js';

const addressA = `0x${'abcd'.repeat(10)}`;
const addressB = `0x${'1234'.repeat(10)}`;
// These are unmistakably synthetic configuration handles, never signing material.
const wallet = (
  name = 'synthetic-wallet-a',
  address = addressA,
  credential = 'SYNTHETIC_WALLET_A',
): Connection => ({ name, kind: 'ethereum', address, credential });
const main = (name = 'synthetic-main-a', credential = 'SYNTHETIC_MAIN_A'): Connection => ({
  name,
  kind: 'binance',
  role: 'main',
  credential,
});
const child = (
  name: string,
  parent = 'synthetic-main-a',
  email = 'synthetic-child@example.invalid',
): Connection => ({ name, kind: 'binance', role: 'sub', parent, email });
function loadSynthetic(connections: Connection[]) {
  const directory = mkdtempSync(join(tmpdir(), 'fin-synthetic-config-'));
  try {
    const path = join(directory, 'config.json');
    writeFileSync(path, JSON.stringify({ version: 1, connections, ethereum: MAINNET }), {
      mode: 0o600,
    });
    return loadConfig(path);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}
test('loadConfig rejects duplicate Ethereum addresses even with different names, case and credential handles', () => {
  assert.throws(
    () =>
      loadSynthetic([
        wallet(),
        wallet('synthetic-wallet-b', `0x${'ABCD'.repeat(10)}`, 'SYNTHETIC_WALLET_B'),
      ]),
    { code: 'DUPLICATE_WALLET_IDENTITY' },
  );
});
test('loadConfig rejects duplicate canonical Binance child parent/email identities without requiring child credentials', () => {
  assert.throws(
    () =>
      loadSynthetic([
        main(),
        child('synthetic-child-a'),
        child('synthetic-child-b', 'synthetic-main-a', ' SYNTHETIC-CHILD@EXAMPLE.INVALID '),
      ]),
    { code: 'DUPLICATE_CHILD_IDENTITY' },
  );
});
test('loadConfig rejects one main signer under two names and one Ethereum signer under different addresses', () => {
  assert.throws(() => loadSynthetic([main(), main('synthetic-main-b')]), {
    code: 'DUPLICATE_SIGNER_IDENTITY',
  });
  assert.throws(() => loadSynthetic([wallet(), wallet('synthetic-wallet-b', addressB)]), {
    code: 'DUPLICATE_SIGNER_IDENTITY',
  });
});
test('valid master-access child configuration remains loadable with no child credential', () => {
  const connections = [
    main(),
    child('synthetic-child-a'),
    child('synthetic-child-b', 'synthetic-main-a', 'synthetic-other-child@example.invalid'),
    wallet(),
    wallet('synthetic-wallet-b', addressB, 'SYNTHETIC_WALLET_B'),
  ];
  const loaded = loadSynthetic(connections);
  assert.deepEqual(loaded.connections, connections);
  assert.equal(loaded.connections[1]!.credential, undefined);
});
test('canonical child identity includes its authority and credential comparisons preserve connection kind and role', () => {
  const connections: Connection[] = [
    main(),
    main('synthetic-main-b', 'SYNTHETIC_MAIN_B'),
    child('synthetic-child-a'),
    child('synthetic-child-b', 'synthetic-main-b'),
    wallet('synthetic-wallet-a', addressA, 'SYNTHETIC_MAIN_A'),
  ];
  assert.doesNotThrow(() => loadSynthetic(connections));
  assert.doesNotThrow(() =>
    assertUniqueConnectionIdentities([
      main('synthetic-main-a', 'SYNTHETIC_ROLE_SCOPED_HANDLE'),
      { ...child('synthetic-child-a'), credential: 'SYNTHETIC_ROLE_SCOPED_HANDLE' },
    ]),
  );
});
test('configuration alias errors never expose private mapping values', () => {
  const connections = [main(), main('synthetic-main-b')];
  let caught: unknown;
  try {
    loadSynthetic(connections);
  } catch (error) {
    caught = error;
  }
  assert.ok(caught);
  const displayed = JSON.stringify(publicError(caught));
  for (const value of [connections[0]!.name, connections[1]!.name, connections[0]!.credential!])
    assert.equal(displayed.includes(value), false);
  assert.equal(publicError(caught).code, 'DUPLICATE_SIGNER_IDENTITY');
  assert.throws(() => loadSynthetic([wallet(), { ...wallet(), address: addressB }]), {
    code: 'DUPLICATE_CONNECTION_NAME',
  });
});
