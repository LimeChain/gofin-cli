import type { ZodType } from 'zod';
export type Input = Record<string, unknown>;
export type Status =
  | 'awaiting_approval'
  | 'rejected'
  | 'expired'
  | 'cancelled'
  | 'submitted'
  | 'pending'
  | 'completed'
  | 'failed'
  | 'unknown';
export interface Definition {
  name: string;
  description: string;
  kind: 'read' | 'write';
  schema: ZodType;
  examples: Input[];
  coverage: string;
  completion: string;
}
export interface Check {
  name: string;
  status: 'passed' | 'failed' | 'unchecked';
  detail: string;
}
export interface Valuation {
  movementUsdMicros: string;
  maximumFeeUsdMicros: string;
  observedAt: string;
  source: string;
  /** Borrow creates additional assets controlled by the test; it is not netted against debt. */
  additionalAllocationUsdMicros?: string;
}
export interface Preview {
  operation: string;
  request: Input;
  /** Stable exact execution payload and bounds. Observation timestamps do not belong here. */
  binding: Input;
  summary: string;
  checks: Check[];
  warnings: string[];
  observedAt: string;
  expiresAt: string;
  valuation?: Valuation;
}
export interface Submission {
  kind: 'ethereum' | 'binance';
  reference: string;
  data: Input;
}
export interface Reconciliation {
  status: 'pending' | 'completed' | 'failed' | 'unknown';
  evidence: Input;
}
export interface RecordOperation {
  id: string;
  requestId: string;
  fingerprint: string;
  operation: string;
  input: Input;
  status: Status;
  createdAt: string;
  updatedAt: string;
  preview?: Preview;
  approval?: { mechanism: string; bindingHash: string; approvedAt: string; expiresAt: string };
  submission?: Submission;
  evidence?: Input;
  error?: { code: string; stage: string; message: string; nextAction: string };
  runId?: string;
}
export interface SubmissionHooks {
  /** Recheck the in-process approval deadline immediately before signing or dispatch. */
  assertAuthorized?(): void;
  /** Must complete a durable write BEFORE any external submission. Never persist secrets or signed bytes. */
  persistSubmission(submission: Submission): void;
}
export interface Adapter {
  definitions: Definition[];
  read(operation: string, input: Input): Promise<unknown>;
  preview(operation: string, input: Input): Promise<Preview>;
  /** Recheck current state against the original exact payload and enforceable bounds. */
  revalidate?(preview: Preview): Promise<Preview>;
  submit(preview: Preview, hooks: SubmissionHooks): Promise<Submission>;
  reconcile(record: RecordOperation): Promise<Reconciliation>;
}
export interface Connection {
  name: string;
  kind: 'binance' | 'ethereum';
  role?: 'main' | 'sub';
  parent?: string;
  email?: string;
  address?: string;
  credential?: string;
  walletType?: 'SPOT';
}
export interface FinConfig {
  version: 1;
  connections: Connection[];
  ethereum: {
    rpcUrls: string[];
    chainId: number;
    confirmations: number;
    uniswapRouter: string;
    uniswapQuoter: string;
    uniswapFactory: string;
    aavePool: string;
    aaveOracle: string;
  };
}
