import test from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { access, chmod, mkdir, mkdtemp, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir, userInfo } from 'node:os';
import { join } from 'node:path';
import { promisify } from 'node:util';
import { canonical, fingerprint } from '../src/core/canonical.js';
import {
  metadataIssues,
  validateProtectedInstallation,
} from '../src/security/protected-installation.js';
import {
  SingleUseApprovalLedger,
  TouchIdAuthorizer,
  validateApprovalResponse,
  type ApprovalChallenge,
  type NativeApprovalResponse,
} from '../src/security/touch-id.js';
import { FinError } from '../src/core/errors.js';
import type { Preview } from '../src/core/types.js';

const now = Date.parse('2026-10-01T12:00:00.000Z');
const binding = {
  operation: 'wallet.transfer',
  chainId: 1,
  from: 'synthetic-wallet',
  to: '0x0000000000000000000000000000000000000001',
  amount: '0.001',
  maxFeeWei: '1000000000000',
};
const challenge: ApprovalChallenge = {
  version: 1,
  mode: 'approve',
  nonce: '11'.repeat(32),
  bindingHash: fingerprint(binding),
  canonicalBinding: canonical(binding),
  operation: 'wallet.transfer',
  summary: 'Synthetic isolated fixture: no credentials and no submission.',
  expiresAt: new Date(now + 60_000).toISOString(),
};
const accepted: NativeApprovalResponse = {
  version: 1,
  status: 'accepted',
  nonce: challenge.nonce,
  bindingHash: challenge.bindingHash,
  approvedAt: new Date(now - 1_000).toISOString(),
  expiresAt: challenge.expiresAt,
};
const hasCode = (code: string) => (error: unknown) =>
  error instanceof FinError && error.code === code;

test('protected installation permission rule rejects ownership, writable code, symlinks and accessible secrets', () => {
  const rootFile = { uid: 0, mode: 0o100644, symbolicLink: false, directory: false };
  assert.deepEqual(metadataIssues(rootFile), []);
  assert.deepEqual(metadataIssues({ ...rootFile, mode: 0o100600 }, true), []);
  assert.ok(metadataIssues({ ...rootFile, uid: 501 }).includes('must be owned by root'));
  assert.ok(
    metadataIssues({ ...rootFile, mode: 0o100664 }).includes(
      'must not be writable by group or others',
    ),
  );
  assert.ok(
    metadataIssues({ ...rootFile, mode: 0o100646 }).includes(
      'must not be writable by group or others',
    ),
  );
  assert.ok(
    metadataIssues({ ...rootFile, symbolicLink: true }).includes(
      'symbolic links are not permitted',
    ),
  );
  assert.ok(
    metadataIssues(rootFile, true).includes('private files must deny group and other access'),
  );
});

test('validator checks nested runtime files after their ancestors, including symlinks and exposed private state', async () => {
  const root = await mkdtemp(join(tmpdir(), 'fin-synthetic-security-'));
  try {
    await mkdir(join(root, 'runtime/dependency/deep'), { recursive: true });
    await mkdir(join(root, 'private'));
    const nested = join(root, 'runtime/dependency/deep/file.js');
    await writeFile(nested, '// Isolated synthetic fixture; no credentials or live adapters.\n');
    await chmod(nested, 0o666);
    const link = join(root, 'runtime/dependency/link.js');
    await symlink(nested, link);
    const config = join(root, 'private/config.json');
    await writeFile(config, '{}', { mode: 0o644 });
    const report = await validateProtectedInstallation({
      installationRoot: root,
      executablePath: join(root, 'runtime/node'),
    });
    assert.equal(report.ok, false);
    assert.ok(
      report.checks.some(
        (check) =>
          check.path === nested && check.detail.includes('must not be writable by group or others'),
      ),
    );
    assert.ok(
      report.checks.some(
        (check) => check.path === link && check.detail.includes('symbolic links are not permitted'),
      ),
    );
    assert.ok(
      report.checks.some(
        (check) =>
          check.path === config &&
          check.detail.includes('private files must deny group and other access'),
      ),
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('validator rejects extended ACL grants on an existing private SQLite database and sidecar', async (context) => {
  if (process.platform !== 'darwin') {
    context.skip('extended ACL regression requires macOS');
    return;
  }
  const root = await mkdtemp(join(tmpdir(), 'fin-synthetic-acl-'));
  const run = promisify(execFile);
  try {
    await mkdir(join(root, 'runtime'));
    await mkdir(join(root, 'private'));
    for (const name of ['config.json', 'credentials.env', 'approval.json'])
      await writeFile(join(root, 'private', name), '{}', { mode: 0o600 });
    const database = join(root, 'private/operations.sqlite');
    const sidecar = `${database}-wal`;
    for (const path of [database, sidecar])
      await writeFile(path, 'Synthetic fixture only; no financial records.', { mode: 0o600 });
    for (const path of [database, sidecar]) {
      await run('/bin/chmod', ['+a', `user:${userInfo().username} allow read,write`, path]);
      const report = await validateProtectedInstallation({
        installationRoot: root,
        executablePath: join(root, 'runtime/node'),
      });
      assert.ok(
        report.issues.some((issue) => issue.includes('extended ACLs are not permitted')),
        `ACL grant must be detected on ${path}`,
      );
      await run('/bin/chmod', ['-N', path]);
    }
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('synthetic accepted response is bound to nonce and exact canonical operation', () => {
  const receipt = validateApprovalResponse(challenge, accepted, now);
  assert.equal(receipt.bindingHash, fingerprint(binding));
  assert.equal(receipt.mechanism, 'touch-id');
  assert.throws(
    () => validateApprovalResponse(challenge, { ...accepted, nonce: '22'.repeat(32) }, now),
    hasCode('APPROVAL_BINDING_MISMATCH'),
  );
  for (const changed of [
    { ...binding, amount: '0.002' },
    { ...binding, chainId: 10 },
    { ...binding, to: '0x0000000000000000000000000000000000000002' },
    { ...binding, maxFeeWei: '2000000000000' },
  ]) {
    assert.throws(
      () =>
        validateApprovalResponse(
          { ...challenge, bindingHash: fingerprint(changed) },
          accepted,
          now,
        ),
      hasCode('APPROVAL_BINDING_MISMATCH'),
    );
  }
});

test('cancelled, rejected, unavailable and malformed approval cannot submit', () => {
  for (const [status, code] of [
    ['rejected', 'APPROVAL_REJECTED'],
    ['cancelled', 'APPROVAL_CANCELLED'],
    ['expired', 'APPROVAL_EXPIRED'],
    ['unavailable', 'APPROVAL_UNAVAILABLE'],
    ['yes', 'APPROVAL_INVALID'],
  ]) {
    assert.throws(
      () => validateApprovalResponse(challenge, { ...accepted, status: status! }, now),
      hasCode(code!),
    );
  }
  assert.throws(
    () => validateApprovalResponse(challenge, { ...accepted, version: 2 }, now),
    hasCode('APPROVAL_BINDING_MISMATCH'),
  );
});

test('approval deadlines reject stale, future, excessive or invalid timing', () => {
  for (const changed of [
    { expiresAt: new Date(now).toISOString() },
    { expiresAt: new Date(now + 60_001).toISOString() },
    { approvedAt: new Date(now + 2_000).toISOString() },
    { approvedAt: new Date(now - 90_001).toISOString() },
    { approvedAt: 'invalid' },
    { expiresAt: 'invalid' },
    {
      approvedAt: new Date(now + 1_000).toISOString(),
      expiresAt: new Date(now + 500).toISOString(),
    },
  ])
    assert.throws(
      () => validateApprovalResponse(challenge, { ...accepted, ...changed }, now),
      hasCode('APPROVAL_EXPIRED'),
    );
});

test('approval ledger consumes one issued object only, denies copied and cross-instance receipts', () => {
  const ledger = new SingleUseApprovalLedger();
  const receipt = validateApprovalResponse(challenge, accepted, now);
  assert.throws(
    () => ledger.consume(receipt, receipt.bindingHash, now),
    hasCode('APPROVAL_REPLAY'),
  );
  ledger.issue(receipt);
  assert.throws(
    () => ledger.consume({ ...receipt }, receipt.bindingHash, now),
    hasCode('APPROVAL_REPLAY'),
  );
  assert.throws(
    () => new SingleUseApprovalLedger().consume(receipt, receipt.bindingHash, now),
    hasCode('APPROVAL_REPLAY'),
  );
  ledger.consume(receipt, receipt.bindingHash, now);
  assert.throws(
    () => ledger.consume(receipt, receipt.bindingHash, now),
    hasCode('APPROVAL_REPLAY'),
  );
  assert.throws(() => ledger.issue(receipt), hasCode('APPROVAL_REPLAY'));
  assert.equal(Object.isFrozen(receipt), true);
});

test('changed or expired approval is burned, never reusable after a failed attempt', () => {
  for (const expected of ['APPROVAL_BINDING_MISMATCH', 'APPROVAL_EXPIRED']) {
    const ledger = new SingleUseApprovalLedger();
    const receipt = validateApprovalResponse(challenge, accepted, now);
    ledger.issue(receipt);
    assert.throws(
      () =>
        ledger.consume(
          receipt,
          expected === 'APPROVAL_EXPIRED' ? receipt.bindingHash : '00'.repeat(32),
          expected === 'APPROVAL_EXPIRED' ? now + 60_001 : now,
        ),
      hasCode(expected),
    );
    assert.throws(
      () => ledger.consume(receipt, receipt.bindingHash, now),
      hasCode('APPROVAL_REPLAY'),
    );
  }
});

test('writable source mode cannot approve, regardless of synthetic accepted response or environment flag', async () => {
  const preview: Preview = {
    operation: challenge.operation,
    request: {},
    binding,
    summary: challenge.summary,
    checks: [],
    warnings: [],
    observedAt: new Date().toISOString(),
    expiresAt: new Date(Date.now() + 60_000).toISOString(),
  };
  const report = await validateProtectedInstallation({ installationRoot: process.cwd() });
  assert.equal(report.ok, false);
  assert.ok(
    report.issues.some((issue) => issue.includes('production installation must use /Library/Fin')),
  );
  const old = process.env.FIN_SKIP_TOUCH_ID;
  process.env.FIN_SKIP_TOUCH_ID = '1';
  try {
    await assert.rejects(
      new TouchIdAuthorizer({ installationRoot: process.cwd() }).approve(
        preview,
        fingerprint(binding),
      ),
      hasCode('PROTECTED_INSTALLATION_REQUIRED'),
    );
  } finally {
    if (old === undefined) delete process.env.FIN_SKIP_TOUCH_ID;
    else process.env.FIN_SKIP_TOUCH_ID = old;
  }
});

test('native read-only probe cannot return financial acceptance', async (context) => {
  if (process.platform !== 'darwin') {
    context.skip('native helper requires macOS');
    return;
  }
  const path = `${process.cwd()}/native/.build/fin-touch-id`;
  try {
    await access(path);
  } catch {
    context.skip('build native helper with sh native/build.sh');
    return;
  }
  const run = promisify(execFile);
  const flagProbe = JSON.parse((await run(path, ['--probe'], { timeout: 10_000 })).stdout);
  assert.equal(flagProbe.approvalRequested, false);
  assert.ok(['available', 'unavailable'].includes(flagProbe.status));
  assert.notEqual(flagProbe.status, 'accepted');
  for (const flags of [['--approve'], ['--probe', '--approve']]) {
    const response = JSON.parse((await run(path, flags, { timeout: 10_000 })).stdout);
    assert.equal(response.status, 'invalid');
    assert.notEqual(response.status, 'accepted');
  }
  const child = execFile(path, [], { timeout: 10_000 }, () => {});
  const result = new Promise<{ stdout: string }>((resolve, reject) => {
    let stdout = '';
    child.stdout?.on('data', (data) => {
      stdout += String(data);
    });
    child.on('error', reject);
    child.on('close', (code) =>
      code === 0 ? resolve({ stdout }) : reject(new Error('native probe failed')),
    );
  });
  child.stdin?.end(JSON.stringify({ version: 1, mode: 'probe' }));
  const response = JSON.parse((await result).stdout);
  assert.equal(response.approvalRequested, false);
  assert.ok(['available', 'unavailable'].includes(response.status));
  assert.notEqual(response.status, 'accepted');
  // Invalid challenge must fail before any native display or biometric request.
  const invalid = execFile(path, [], { timeout: 10_000 }, () => {});
  const invalidResult = new Promise<string>((resolve, reject) => {
    let stdout = '';
    invalid.stdout?.on('data', (data) => {
      stdout += String(data);
    });
    invalid.on('error', reject);
    invalid.on('close', (code) =>
      code === 0 ? resolve(stdout) : reject(new Error('native invalid-input check failed')),
    );
  });
  invalid.stdin?.end(JSON.stringify({ version: 1, mode: 'approve' }));
  assert.equal(JSON.parse(await invalidResult).status, 'invalid');
});
