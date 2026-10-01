export class FinError extends Error {
  constructor(
    public code: string,
    public stage: string,
    message: string,
    public nextAction: string,
  ) {
    super(message);
    this.name = 'FinError';
  }
}
/** Never serialize arbitrary external exceptions: CCXT/RPC errors may contain signed URLs. */
export function publicError(error: unknown): {
  code: string;
  stage: string;
  message: string;
  nextAction: string;
} {
  if (error instanceof FinError)
    return {
      code: error.code,
      stage: error.stage,
      message: error.message,
      nextAction: error.nextAction,
    };
  return {
    code: 'EXTERNAL_ERROR',
    stage: 'adapter',
    message: 'The external operation could not be established.',
    nextAction:
      'Inspect the existing operation and reconcile its original identity; do not blindly resubmit.',
  };
}
