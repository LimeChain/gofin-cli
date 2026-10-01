import { DatabaseSync } from 'node:sqlite';
import { randomUUID } from 'node:crypto';
import { mkdirSync, chmodSync } from 'node:fs';
import { dirname } from 'node:path';
import { fingerprint } from './canonical.js';
import { FinError } from './errors.js';
import type { Input, RecordOperation } from './types.js';
export class OperationStore {
  readonly db: DatabaseSync;
  constructor(path: string) {
    if (path !== ':memory:') mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
    this.db = new DatabaseSync(path);
    if (path !== ':memory:') chmodSync(path, 0o600);
    this.db.exec(
      'PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000; CREATE TABLE IF NOT EXISTS operations (id TEXT PRIMARY KEY, request_id TEXT UNIQUE NOT NULL, fingerprint TEXT NOT NULL, record TEXT NOT NULL); CREATE TABLE IF NOT EXISTS runs (id TEXT PRIMARY KEY, record TEXT NOT NULL);',
    );
  }
  create(
    operation: string,
    input: Input,
    requestId: string,
    runId?: string,
  ): { record: RecordOperation; created: boolean } {
    const hash = fingerprint({ operation, input, runId: runId ?? null });
    this.db.exec('BEGIN IMMEDIATE');
    try {
      const existing = this.byRequest(requestId);
      if (existing) {
        if (existing.fingerprint !== hash)
          throw new FinError(
            'REQUEST_ID_CONFLICT',
            'validation',
            'This request identity already names different parameters.',
            'Inspect the original operation; use a fresh identity only for a deliberate new action.',
          );
        this.db.exec('COMMIT');
        return { record: existing, created: false };
      }
      const now = new Date().toISOString();
      const record: RecordOperation = {
        id: randomUUID(),
        requestId,
        fingerprint: hash,
        operation,
        input,
        status: 'awaiting_approval',
        createdAt: now,
        updatedAt: now,
        ...(runId ? { runId } : {}),
      };
      this.db
        .prepare('INSERT INTO operations VALUES(?,?,?,?)')
        .run(record.id, requestId, hash, JSON.stringify(record));
      this.db.exec('COMMIT');
      return { record, created: true };
    } catch (e) {
      this.db.exec('ROLLBACK');
      throw e;
    }
  }
  save(record: RecordOperation): RecordOperation {
    record.updatedAt = new Date().toISOString();
    this.db
      .prepare('UPDATE operations SET record=? WHERE id=?')
      .run(JSON.stringify(record), record.id);
    return record;
  }
  get(id: string): RecordOperation | undefined {
    const row = this.db.prepare('SELECT record FROM operations WHERE id=?').get(id) as
      { record: string } | undefined;
    return row ? JSON.parse(row.record) : undefined;
  }
  byRequest(id: string): RecordOperation | undefined {
    const row = this.db.prepare('SELECT record FROM operations WHERE request_id=?').get(id) as
      { record: string } | undefined;
    return row ? JSON.parse(row.record) : undefined;
  }
  list(): RecordOperation[] {
    return (
      this.db.prepare('SELECT record FROM operations ORDER BY rowid DESC').all() as {
        record: string;
      }[]
    ).map((r) => JSON.parse(r.record));
  }
  recoverInterrupted(): void {
    for (const record of this.list()) {
      if (record.status === 'awaiting_approval') {
        record.status = 'cancelled';
        record.error = {
          code: 'INTERRUPTED_APPROVAL',
          stage: 'approval',
          message: 'Approval was interrupted before submission.',
          nextAction: 'Inspect the operation; any new write requires fresh approval.',
        };
        this.save(record);
      } else if (record.status === 'submitted') {
        record.status = 'unknown';
        this.save(record);
      }
    }
  }
  getRun<T>(id: string): T | undefined {
    const row = this.db.prepare('SELECT record FROM runs WHERE id=?').get(id) as
      { record: string } | undefined;
    return row ? JSON.parse(row.record) : undefined;
  }
  saveRun(id: string, record: unknown): void {
    this.db
      .prepare('INSERT INTO runs VALUES(?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record')
      .run(id, JSON.stringify(record));
  }
  close(): void {
    this.db.close();
  }
}
