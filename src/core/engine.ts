import { FinError, publicError } from './errors.js';
import { fingerprint } from './canonical.js';
import { OperationStore } from './store.js';
import { BudgetGuard, type TestRun } from './budget.js';
import type { Adapter, Definition, Input, Preview, RecordOperation } from './types.js';
export interface Authorizer {
  approve(preview: Preview, bindingHash: string): Promise<NonNullable<RecordOperation['approval']>>;
  consume?(receipt: NonNullable<RecordOperation['approval']>, bindingHash: string): void;
}
export class OperationEngine {
  private registry = new Map<string, { adapter: Adapter; definition: Definition }>();
  private tail: Promise<void> = Promise.resolve();
  constructor(
    readonly store: OperationStore,
    adapters: Adapter[],
    private authorizer: Authorizer,
    private options: {
      liveTestOnly?: boolean;
      now?: () => number;
      revalueAllocation?: (runId: string) => Promise<void>;
      enrichReconciliation?: (record: RecordOperation) => Promise<RecordOperation>;
    } = {},
  ) {
    for (const adapter of adapters)
      for (const definition of adapter.definitions) {
        if (this.registry.has(definition.name)) throw new Error('Duplicate operation definition');
        this.registry.set(definition.name, { adapter, definition });
      }
  }
  definitions(): Definition[] {
    return [...this.registry.values()].map((r) => r.definition);
  }
  recoverInterrupted(): void {
    this.store.recoverInterrupted();
    for (const record of this.store.list())
      if (['completed', 'cancelled', 'rejected', 'expired', 'failed'].includes(record.status))
        this.persistReconciliation(structuredClone(record));
  }
  async serialize<T>(work: () => Promise<T>): Promise<T> {
    const previous = this.tail;
    let release!: () => void;
    this.tail = new Promise<void>((r) => {
      release = r;
    });
    await previous;
    try {
      return await work();
    } finally {
      release();
    }
  }
  private resolve(operation: string, input: Input) {
    const entry = this.registry.get(operation);
    if (!entry)
      throw new FinError(
        'UNSUPPORTED_OPERATION',
        'discovery',
        'This operation is not implemented.',
        'Use fin capabilities and fin explain.',
      );
    const parsed = entry.definition.schema.safeParse(input);
    if (!parsed.success)
      throw new FinError(
        'INVALID_INPUT',
        'validation',
        'Input does not match the strict operation schema.',
        'Run fin schema for exact fields, units, and bounds.',
      );
    return { ...entry, input: parsed.data as Input };
  }
  async read(operation: string, input: Input): Promise<unknown> {
    const e = this.resolve(operation, input);
    if (e.definition.kind !== 'read')
      throw new FinError(
        'WRITE_REQUIRES_APPROVAL',
        'validation',
        'This operation changes financial state.',
        'Use preview and execute.',
      );
    return e.adapter.read(operation, e.input);
  }
  async preview(operation: string, input: Input): Promise<Preview> {
    const e = this.resolve(operation, input);
    if (e.definition.kind !== 'write')
      throw new FinError(
        'READ_OPERATION',
        'validation',
        'This is a read operation.',
        'Use fin read.',
      );
    return e.adapter.preview(operation, e.input);
  }
  async execute(operation: string, input: Input, runId?: string): Promise<RecordOperation> {
    // One broker serializes writes across connections, preserving nonce and allocation reservations.
    return this.serialize(() => this.executeSerial(operation, input, runId));
  }
  private async executeSerial(
    operation: string,
    input: Input,
    runId?: string,
  ): Promise<RecordOperation> {
    const e = this.resolve(operation, input);
    if (e.definition.kind !== 'write')
      throw new FinError(
        'READ_OPERATION',
        'validation',
        'This is a read operation.',
        'Use fin read.',
      );
    const requestId = e.input.requestId;
    if (typeof requestId !== 'string' || !requestId.length)
      throw new FinError(
        'REQUEST_ID_REQUIRED',
        'validation',
        'A stable request identity is required.',
        'Choose one identity for this logical action.',
      );
    const { record, created } = this.store.create(operation, e.input, requestId, runId);
    if (!created) {
      if (['submitted', 'pending', 'unknown'].includes(record.status))
        return this.statusSerial(record.id, true);
      return record;
    }
    let submissionStarted = false;
    try {
      record.preview = await e.adapter.preview(operation, e.input);
      this.store.save(record);
      this.validatePreview(record.preview);
      const budget = new BudgetGuard(this.store, this.options.now);
      if (this.options.liveTestOnly) {
        if (!runId)
          throw new FinError(
            'RUN_REQUIRED',
            'budget',
            'Live verification requires a baseline and complete return plan.',
            'Begin a bounded test run before writing.',
          );
        await this.options.revalueAllocation?.(runId);
        budget.check(record.preview, runId, record.id);
      }
      const hash = fingerprint(record.preview.binding);
      record.approval = await this.authorizer.approve(record.preview, hash);
      if (
        record.approval.bindingHash !== hash ||
        !Number.isFinite(Date.parse(record.approval.expiresAt)) ||
        !Number.isFinite(Date.parse(record.approval.approvedAt)) ||
        Date.parse(record.approval.expiresAt) <= this.now() ||
        Date.parse(record.approval.approvedAt) > this.now() + 5000 ||
        Date.parse(record.approval.approvedAt) >= Date.parse(record.approval.expiresAt)
      )
        throw new FinError(
          'INVALID_APPROVAL',
          'approval',
          'Approval is expired or does not match this exact operation.',
          'Obtain fresh Touch ID for the current request.',
        );
      this.store.save(record);
      const refreshed = e.adapter.revalidate
        ? await e.adapter.revalidate(record.preview)
        : await e.adapter.preview(operation, e.input);
      this.validatePreview(refreshed);
      // Do not accept changed gas, nonce, destination, amount or material quote bounds under old approval.
      if (fingerprint(refreshed.binding) !== hash)
        throw new FinError(
          'REQUEST_CHANGED',
          'approval',
          'Execution parameters changed after approval.',
          'Preview the refreshed operation and authorize a deliberate new request.',
        );
      if (Date.parse(record.approval.expiresAt) <= this.now())
        throw new FinError(
          'APPROVAL_EXPIRED',
          'approval',
          'Approval expired during revalidation.',
          'Obtain fresh approval.',
        );
      if (this.options.liveTestOnly) {
        await this.options.revalueAllocation?.(runId!);
        budget.check(refreshed, runId!, record.id, true);
      }
      this.authorizer.consume?.(record.approval, hash);
      const assertAuthorized = () => {
        if (
          record.approval!.bindingHash !== hash ||
          Date.parse(record.approval!.expiresAt) <= this.now()
        )
          throw new FinError(
            'APPROVAL_EXPIRED',
            'approval',
            'Approval expired before signing or dispatch.',
            'Obtain fresh approval for a deliberate new request.',
          );
      };
      record.preview = refreshed;
      record.status = 'submitted';
      this.store.save(record);
      const result = await e.adapter.submit(refreshed, {
        assertAuthorized,
        persistSubmission: (submission) => {
          if (!record.submission) assertAuthorized();
          if (
            record.submission &&
            (record.submission.kind !== submission.kind ||
              record.submission.reference !== submission.reference ||
              Object.entries(record.submission.data).some(
                ([key, value]) => fingerprint(value) !== fingerprint(submission.data[key]),
              ))
          )
            throw new FinError(
              'IDENTITY_CHANGED',
              'submission',
              'Submission identity cannot change.',
              'Reconcile the existing submission.',
            );
          submissionStarted = true;
          record.submission = submission;
          this.store.save(record);
        },
      });
      if (!record.submission)
        throw new FinError(
          'IDENTITY_NOT_PERSISTED',
          'submission',
          'Adapter did not persist identity before submission.',
          'Inspect and reconcile this operation.',
        );
      if (result.reference !== record.submission.reference)
        throw new FinError(
          'IDENTITY_CHANGED',
          'submission',
          'The venue returned a different submission identity.',
          'Reconcile original and returned evidence without repeating submission.',
        );
      record.status = 'pending';
      this.store.save(record);
      return await this.statusSerial(record.id, true);
    } catch (error) {
      const safe = publicError(error);
      record.error = safe;
      const code = (error as { code?: string })?.code;
      record.status = submissionStarted
        ? 'unknown'
        : code?.includes('EXPIRED')
          ? 'expired'
          : code?.includes('CANCEL')
            ? 'cancelled'
            : code?.includes('DECLIN') || code?.includes('REJECT')
              ? 'rejected'
              : 'failed';
      this.store.save(record);
      new BudgetGuard(this.store, this.options.now).settle(record);
      return record;
    }
  }
  async status(id: string, reconcile = false): Promise<RecordOperation> {
    return reconcile
      ? this.serialize(() => this.statusSerial(id, true))
      : this.statusSerial(id, false);
  }
  private async statusSerial(id: string, reconcile = false): Promise<RecordOperation> {
    const record = this.store.get(id);
    if (!record)
      throw new FinError(
        'OPERATION_NOT_FOUND',
        'status',
        'Operation does not exist.',
        'Inspect fin operations.',
      );
    if (reconcile && ['submitted', 'pending', 'unknown'].includes(record.status)) {
      const e = this.registry.get(record.operation);
      if (!e)
        throw new FinError(
          'ADAPTER_UNAVAILABLE',
          'reconciliation',
          'The original adapter is unavailable.',
          'Restore the configured adapter without resubmitting.',
        );
      try {
        // Reads may fail after a chain outcome is known but before usable venue credit
        // or inventory effects are proved. Never mutate the durable pending record first.
        const result = await e.adapter.reconcile(structuredClone(record));
        const candidate = structuredClone(record);
        candidate.status = result.status;
        candidate.evidence = result.evidence;
        if (this.options.enrichReconciliation) {
          const enriched = await this.options.enrichReconciliation(structuredClone(candidate));
          candidate.status = enriched.status;
          candidate.evidence = enriched.evidence;
        }
        return this.persistReconciliation(candidate);
      } catch (error) {
        record.error = publicError(error);
        this.store.save(record);
      }
    }
    return record;
  }
  private persistReconciliation(candidate: RecordOperation): RecordOperation {
    // All external reads finish before this synchronous transaction. The outcome and
    // its exact inventory/fee settlement must either both commit or both roll back.
    this.store.db.exec('BEGIN IMMEDIATE');
    try {
      new BudgetGuard(this.store, this.options.now).settle(candidate);
      if (candidate.submission && ['completed', 'failed'].includes(candidate.status)) {
        const run = candidate.runId ? this.store.getRun<TestRun>(candidate.runId) : undefined;
        if (
          (this.options.liveTestOnly || run?.inventory) &&
          (!run?.inventory?.settled.includes(candidate.id) || run.inventory.inFlight[candidate.id])
        ) {
          candidate.status = 'unknown';
          candidate.error = publicError(
            new FinError(
              'RECONCILIATION_PROOF_PENDING',
              'reconciliation',
              'The submitted outcome lacks validated scoped inventory settlement.',
              'Reconcile this original submission identity; do not repeat the financial action.',
            ),
          );
        }
      }
      this.store.save(candidate);
      this.store.db.exec('COMMIT');
      return candidate;
    } catch (error) {
      this.store.db.exec('ROLLBACK');
      throw error;
    }
  }
  private now() {
    return this.options.now?.() ?? Date.now();
  }
  private validatePreview(preview: Preview) {
    if (preview.checks.some((c) => c.status === 'failed'))
      throw new FinError(
        'PREVIEW_FAILED',
        'preview',
        'A required prerequisite failed.',
        'Inspect the preview checks and revise the request.',
      );
    if (
      !Number.isFinite(Date.parse(preview.expiresAt)) ||
      Date.parse(preview.expiresAt) <= this.now()
    )
      throw new FinError(
        'PREVIEW_EXPIRED',
        'preview',
        'The preview expired.',
        'Refresh the preview.',
      );
  }
}
