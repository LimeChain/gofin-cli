import { spawn, execFile } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { readFile, stat } from 'node:fs/promises';
import { promisify } from 'node:util';
import { canonical, fingerprint } from '../core/canonical.js';
import { FinError } from '../core/errors.js';
import type { Preview, RecordOperation } from '../core/types.js';
import { validateProtectedInstallation, type ProtectedPaths } from './protected-installation.js';

const run = promisify(execFile);
export type ApprovalReceipt = NonNullable<RecordOperation['approval']>;
export interface ApprovalChallenge {
  version: 1;
  mode: 'approve';
  nonce: string;
  bindingHash: string;
  canonicalBinding: string;
  operation: string;
  summary: string;
  expiresAt: string;
}
export interface NativeApprovalResponse {
  version: number;
  status: string;
  nonce?: string;
  bindingHash?: string;
  approvedAt?: string;
  expiresAt?: string;
}

/** Checks are shared with tests, but no response/parser API can authorize a production write. */
export function validateApprovalResponse(
  challenge: ApprovalChallenge,
  response: NativeApprovalResponse,
  now = Date.now(),
): ApprovalReceipt {
  if (
    response.version !== 1 ||
    response.nonce !== challenge.nonce ||
    response.bindingHash !== challenge.bindingHash
  ) {
    throw new FinError(
      'APPROVAL_BINDING_MISMATCH',
      'approval',
      'The approval response does not match this write.',
      'Request a fresh approval of the actual operation.',
    );
  }
  const statusCodes: Record<string, string> = {
    rejected: 'APPROVAL_REJECTED',
    cancelled: 'APPROVAL_CANCELLED',
    expired: 'APPROVAL_EXPIRED',
    unavailable: 'APPROVAL_UNAVAILABLE',
  };
  if (response.status !== 'accepted') {
    throw new FinError(
      statusCodes[response.status] ?? 'APPROVAL_INVALID',
      'approval',
      `Financial approval ${Object.hasOwn(statusCodes, response.status) ? response.status : 'was invalid'}.`,
      'Inspect the operation; obtain fresh Touch ID approval to perform the write.',
    );
  }
  const approved = Date.parse(response.approvedAt ?? '');
  const expiry = Date.parse(response.expiresAt ?? '');
  if (
    !Number.isFinite(approved) ||
    !Number.isFinite(expiry) ||
    approved > now + 1_000 ||
    approved < now - 90_000 ||
    expiry > Date.parse(challenge.expiresAt) ||
    expiry <= now ||
    expiry <= approved
  ) {
    throw new FinError(
      'APPROVAL_EXPIRED',
      'approval',
      'The financial approval is expired or has invalid timing.',
      'Obtain a fresh approval after checking the current preview.',
    );
  }
  return {
    mechanism: 'touch-id',
    bindingHash: challenge.bindingHash,
    approvedAt: new Date(approved).toISOString(),
    expiresAt: new Date(expiry).toISOString(),
  };
}

/** An instance belongs to one broker; persisted/client-supplied receipts are not capabilities. */
export class SingleUseApprovalLedger {
  private receipts = new WeakSet<ApprovalReceipt>();
  private issued = new WeakSet<ApprovalReceipt>();
  issue(receipt: ApprovalReceipt): void {
    if (this.issued.has(receipt))
      throw new FinError(
        'APPROVAL_REPLAY',
        'approval',
        'This receipt has already been issued.',
        'Obtain a newly authenticated approval.',
      );
    Object.freeze(receipt);
    this.issued.add(receipt);
    this.receipts.add(receipt);
  }
  consume(receipt: ApprovalReceipt, bindingHash: string, now = Date.now()): void {
    if (!this.receipts.delete(receipt))
      throw new FinError(
        'APPROVAL_REPLAY',
        'approval',
        'This approval was not issued here or has already been consumed.',
        'Obtain fresh Touch ID approval for a deliberate new write.',
      );
    if (receipt.bindingHash !== bindingHash)
      throw new FinError(
        'APPROVAL_BINDING_MISMATCH',
        'approval',
        'The operation changed after approval.',
        'Generate a fresh preview and obtain new approval.',
      );
    if (Date.parse(receipt.expiresAt) <= now)
      throw new FinError(
        'APPROVAL_EXPIRED',
        'approval',
        'The financial approval expired before submission.',
        'Recheck current conditions and obtain fresh approval.',
      );
  }
}

export class TouchIdAuthorizer {
  private readonly ledger = new SingleUseApprovalLedger();
  constructor(private readonly options: ProtectedPaths = {}) {}

  async approve(preview: Preview, bindingHash: string): Promise<ApprovalReceipt> {
    const installation = await validateProtectedInstallation(this.options);
    if (!installation.ok)
      throw new FinError(
        'PROTECTED_INSTALLATION_REQUIRED',
        'approval',
        'Financial writes require a valid root-owned Fin installation.',
        'Have the human install and verify the protected broker; source-mode execution cannot approve writes.',
      );
    if (bindingHash !== fingerprint(preview.binding))
      throw new FinError(
        'APPROVAL_BINDING_MISMATCH',
        'approval',
        'The requested approval hash differs from the actual operation.',
        'Generate a fresh preview and confirm its exact binding.',
      );
    const expires = Math.min(Date.parse(preview.expiresAt), Date.now() + 90_000);
    if (!Number.isFinite(expires) || expires <= Date.now())
      throw new FinError(
        'APPROVAL_EXPIRED',
        'approval',
        'The preview has expired before approval.',
        'Generate a new preview.',
      );
    const policyPath = this.options.policyPath ?? '/Library/Fin/private/approval.json';
    let approverUid: number;
    try {
      const policy = JSON.parse(await readFile(policyPath, 'utf8')) as {
        version?: number;
        approverUid?: number;
      };
      if (
        policy.version !== 1 ||
        !Number.isSafeInteger(policy.approverUid) ||
        policy.approverUid! < 501
      )
        throw new Error('invalid policy');
      approverUid = policy.approverUid!;
      if ((await stat('/dev/console')).uid !== approverUid)
        throw new Error('wrong interactive user');
    } catch {
      throw new FinError(
        'APPROVER_UNAVAILABLE',
        'approval',
        'The configured human approver is not the active console user.',
        'Sign in to the configured macOS account; changing the approver requires human-admin setup.',
      );
    }
    const { stdout } = await run('/usr/bin/id', ['-nu', String(approverUid)], {
      env: { PATH: '/usr/bin:/bin' },
      timeout: 5_000,
    });
    const username = stdout.trim();
    if (!/^[a-zA-Z0-9_.-]+$/.test(username))
      throw new FinError(
        'APPROVER_UNAVAILABLE',
        'approval',
        'The configured macOS approver could not be resolved.',
        'Repair the protected approval configuration.',
      );
    const challenge: ApprovalChallenge = {
      version: 1,
      mode: 'approve',
      nonce: randomBytes(32).toString('hex'),
      bindingHash,
      canonicalBinding: canonical(preview.binding),
      operation: preview.operation,
      summary: preview.summary,
      expiresAt: new Date(expires).toISOString(),
    };
    const helper = this.options.helperPath ?? '/Library/Fin/runtime/native/fin-touch-id';
    const response = await this.invokeHelper(approverUid, username, helper, challenge);
    const receipt = validateApprovalResponse(challenge, response);
    this.ledger.issue(receipt);
    return receipt;
  }

  /** The receipt object is private to this broker instance and can authorize one submission only. */
  consume(receipt: ApprovalReceipt, bindingHash: string): void {
    this.ledger.consume(receipt, bindingHash);
  }

  private invokeHelper(
    uid: number,
    username: string,
    helper: string,
    challenge: ApprovalChallenge,
  ): Promise<NativeApprovalResponse> {
    return new Promise((resolve, reject) => {
      // Fixed protected executable, fixed OS launchers, cleared environment, inherited private pipes.
      // There is no client-supplied helper, command, response file, approval token, or signing endpoint.
      const child = spawn(
        '/bin/launchctl',
        ['asuser', String(uid), '/usr/bin/sudo', '-n', '-u', username, '--', helper],
        { env: { PATH: '/usr/bin:/bin' }, stdio: ['pipe', 'pipe', 'pipe'] },
      );
      let output = '';
      let tooLarge = false;
      const timeout = setTimeout(
        () => {
          child.kill('SIGKILL');
          reject(
            new FinError(
              'APPROVAL_EXPIRED',
              'approval',
              'Touch ID approval did not finish before its deadline.',
              'Inspect the operation and request fresh approval.',
            ),
          );
        },
        Math.max(1, Date.parse(challenge.expiresAt) - Date.now()) + 1_000,
      );
      child.stdout.on('data', (data: Buffer) => {
        output += data.toString('utf8');
        if (Buffer.byteLength(output) > 8_192) {
          tooLarge = true;
          child.kill('SIGKILL');
        }
      });
      // Never echo native errors/stdio into client output.
      child.stderr.resume();
      child.on('error', () => {
        clearTimeout(timeout);
        reject(
          new FinError(
            'APPROVAL_UNAVAILABLE',
            'approval',
            'The protected Touch ID helper could not be started.',
            'Check the human session and protected installation.',
          ),
        );
      });
      child.on('close', (code) => {
        clearTimeout(timeout);
        if (code !== 0 || tooLarge) {
          reject(
            new FinError(
              'APPROVAL_UNAVAILABLE',
              'approval',
              'The protected Touch ID helper did not return a valid result.',
              'Check the human session and protected installation.',
            ),
          );
          return;
        }
        try {
          resolve(JSON.parse(output) as NativeApprovalResponse);
        } catch {
          reject(
            new FinError(
              'APPROVAL_INVALID',
              'approval',
              'The Touch ID response could not be established.',
              'Obtain fresh Touch ID approval.',
            ),
          );
        }
      });
      child.stdin.on('error', () => {});
      child.stdin.end(JSON.stringify(challenge));
    });
  }
}
