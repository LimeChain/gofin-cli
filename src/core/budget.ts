import { FinError } from './errors.js';
import type { Preview, RecordOperation } from './types.js';
import { OperationStore } from './store.js';
import {
  reserveInventory,
  settleInventory,
  assertInventoryRestorable,
  type RunInventory,
} from './inventory.js';
export const TEST_LIMIT = 100_000_000n;
export interface FinancialSnapshot {
  observedAt: string;
  quantities: Record<string, string>;
  pending: string[];
  metadata: Record<string, unknown>;
}
export interface RunPlan {
  id: string;
  initialAllocationUsdMicros: string;
  borrowReserveUsdMicros: string;
  costReserveUsdMicros: string;
  unwind: string[];
  returnRoutesVerified: boolean;
  baseline: FinancialSnapshot;
  inventory?: RunInventory;
  /** Explicit, asset-unit costs reconciled from external evidence. No arbitrary tolerances. */
}
export interface TestRun extends RunPlan {
  status: 'active' | 'cleanup_outstanding' | 'restored';
  feeReservations: Record<string, string>;
  borrowReservations: Record<string, string>;
  final?: FinancialSnapshot;
  differences?: Record<string, string>;
}
function nonnegative(value: string): bigint {
  if (!/^\d+$/.test(value))
    throw new FinError(
      'INVALID_VALUATION',
      'budget',
      'USD values must be nonnegative integer microdollars.',
      'Provide an exact current valuation.',
    );
  return BigInt(value);
}
function settledBinanceFailure(run: TestRun, record: RecordOperation): boolean {
  if (
    record.submission?.kind !== 'binance' ||
    !run.inventory?.settled.includes(record.id) ||
    run.inventory.inFlight[record.id]
  )
    return false;
  const params = record.preview?.binding.params as Record<string, unknown> | undefined;
  const reference = record.submission.reference;
  // The exact reservation must have passed venue-specific inventory settlement.
  // Recheck the durable identity and failure evidence; a terminal status alone is insufficient.
  if (record.operation === 'binance.transfer')
    return (
      params?.clientTranId === reference &&
      record.evidence?.clientTranId === reference &&
      ['FAILED', 'FAILURE'].includes(String(record.evidence.venueStatus))
    );
  if (record.operation === 'binance.withdraw')
    return (
      params?.withdrawOrderId === reference &&
      record.evidence?.withdrawOrderId === reference &&
      record.evidence?.venueState === 'failed'
    );
  return false;
}
export class BudgetGuard {
  constructor(
    private store: OperationStore,
    private now: () => number = Date.now,
  ) {}
  begin(plan: RunPlan): TestRun {
    if (this.store.getRun(plan.id))
      throw new FinError(
        'RUN_EXISTS',
        'budget',
        'A run with this identity already exists.',
        'Inspect or finish the existing run.',
      );
    const unfinished = this.store.db.prepare('SELECT record FROM runs').all() as {
      record: string;
    }[];
    if (unfinished.some((r) => JSON.parse(r.record).status !== 'restored'))
      throw new FinError(
        'CLEANUP_OUTSTANDING',
        'budget',
        'An earlier test run has unresolved restoration.',
        'Reconcile and restore the earlier run before allocating new test funds.',
      );
    if (
      nonnegative(plan.initialAllocationUsdMicros) +
        nonnegative(plan.borrowReserveUsdMicros) +
        nonnegative(plan.costReserveUsdMicros) >
      TEST_LIMIT
    )
      throw new FinError(
        'ALLOCATION_LIMIT',
        'budget',
        'Initial funds, borrowed proceeds, and full return costs exceed $100.',
        'Reduce the complete allocation.',
      );
    if (
      !plan.returnRoutesVerified ||
      !plan.unwind.length ||
      !Object.keys(plan.baseline.quantities).length ||
      plan.baseline.pending.length
    )
      throw new FinError(
        'UNWIND_NOT_READY',
        'budget',
        'A complete baseline and available return path are required.',
        'Resolve pending movements and verify the entire return route before writing.',
      );
    if (
      !Number.isFinite(Date.parse(plan.baseline.observedAt)) ||
      this.now() - Date.parse(plan.baseline.observedAt) > 120000 ||
      Date.parse(plan.baseline.observedAt) > this.now() + 5000
    )
      throw new FinError(
        'STALE_BASELINE',
        'budget',
        'The financial baseline is not current.',
        'Capture a fresh baseline.',
      );
    const run: TestRun = { ...plan, status: 'active', feeReservations: {}, borrowReservations: {} };
    this.store.saveRun(run.id, run);
    return run;
  }
  check(preview: Preview, runId: string, operationId: string, reserve = false): TestRun {
    const run = this.store.getRun<TestRun>(runId);
    if (!run || run.status === 'restored')
      throw new FinError(
        'RUN_REQUIRED',
        'budget',
        'An active live-test allocation is required.',
        'Capture a baseline and prepare a funded unwind plan.',
      );
    const v = preview.valuation;
    if (run.inventory) {
      run.inventory.cleanupOutstanding = run.status === 'cleanup_outstanding';
      const scoped = reserveInventory(run.inventory, operationId, preview);
      if (reserve) run.inventory = scoped;
    }
    if (
      !v ||
      !v.source ||
      !Number.isFinite(Date.parse(v.observedAt)) ||
      this.now() - Date.parse(v.observedAt) > 120000 ||
      Date.parse(v.observedAt) > this.now() + 5000
    )
      throw new FinError(
        'VALUATION_UNAVAILABLE',
        'budget',
        'A fresh USD valuation is required for every movement and cleanup.',
        'Refresh the asset and cost valuations.',
      );
    const movement = nonnegative(v.movementUsdMicros),
      cost = nonnegative(v.maximumFeeUsdMicros),
      borrow = nonnegative(v.additionalAllocationUsdMicros ?? '0');
    if (movement + cost > TEST_LIMIT)
      throw new FinError(
        'MOVEMENT_LIMIT',
        'budget',
        'Maximum gross movement and execution costs exceed $100.',
        'Choose a smaller movement and bounded fees.',
      );
    if (
      movement >
      nonnegative(run.initialAllocationUsdMicros) + nonnegative(run.borrowReserveUsdMicros)
    )
      throw new FinError(
        'UNALLOCATED_FUNDS',
        'budget',
        'This movement exceeds the funds allocated to this run.',
        'Use only the selected test allocation.',
      );
    const fees = { ...run.feeReservations, [operationId]: cost.toString() },
      borrows = { ...run.borrowReservations, [operationId]: borrow.toString() };
    const total = (entries: Record<string, string>) =>
      Object.values(entries).reduce((sum, n) => sum + nonnegative(n), 0n);
    if (total(fees) > nonnegative(run.costReserveUsdMicros))
      throw new FinError(
        'UNWIND_RESERVE',
        'budget',
        'This operation would consume funds reserved for the complete return.',
        'Revise the planned sequence within the original allocation.',
      );
    if (total(borrows) > nonnegative(run.borrowReserveUsdMicros))
      throw new FinError(
        'BORROW_ALLOCATION',
        'budget',
        'Borrowed proceeds exceed their allocation; debt cannot offset this amount.',
        'Reduce borrowing within the original reserve.',
      );
    if (reserve) {
      run.feeReservations = fees;
      run.borrowReservations = borrows;
      this.store.saveRun(run.id, run);
    }
    return run;
  }
  settle(record: RecordOperation): void {
    if (!record.runId) return;
    const run = this.store.getRun<TestRun>(record.runId);
    if (!run?.inventory) return;
    const reservation = run.inventory.inFlight[record.id];
    if (!reservation) return;
    const fee = reservation.fee;
    const costsBefore = fee ? BigInt(run.inventory.costs[fee.key] ?? '0') : 0n;
    run.inventory = settleInventory(run.inventory, record);
    // Only validated inventory settlement may release a USD reservation. A raw
    // terminal status or fee field is insufficient when its effects do not match.
    if (run.inventory.settled.includes(record.id) && !run.inventory.inFlight[record.id]) {
      if (!record.submission) {
        delete run.feeReservations[record.id];
        delete run.borrowReservations[record.id];
      } else {
        const consumed = fee ? BigInt(run.inventory.costs[fee.key] ?? '0') - costsBefore : 0n;
        const maximum = fee ? BigInt(fee.maximumUnits) : 0n;
        const reservedUsd = nonnegative(run.feeReservations[record.id] ?? '0');
        // Preserve proven spent costs, round up to a microdollar, and retain
        // maximum reservations for every unresolved operation. Zero-fee venue
        // transfers have no fee bucket and consume no execution-cost reserve.
        run.feeReservations[record.id] =
          consumed === 0n ? '0' : ((consumed * reservedUsd + maximum - 1n) / maximum).toString();
      }
    }
    // Inventory effects and their USD cost ledger persist in one atomic run row.
    this.store.saveRun(run.id, run);
  }
  finish(
    runId: string,
    final: FinancialSnapshot,
    verifiedCosts: Record<string, string>,
    operations: RecordOperation[],
  ): TestRun {
    const run = this.store.getRun<TestRun>(runId);
    if (!run)
      throw new FinError(
        'RUN_NOT_FOUND',
        'reconciliation',
        'Test run does not exist.',
        'Inspect available runs.',
      );
    const differences: Record<string, string> = {};
    if (run.inventory) {
      try {
        assertInventoryRestorable(run.inventory);
      } catch (e) {
        differences.inventory = (e as FinError).code;
      }
    }
    const keys = new Set([
      ...Object.keys(run.baseline.quantities),
      ...Object.keys(final.quantities),
    ]);
    for (const key of keys) {
      if (!(key in run.baseline.quantities) || !(key in final.quantities)) {
        differences[key] = 'missing observation';
        continue;
      }
      const difference = BigInt(final.quantities[key]!) - BigInt(run.baseline.quantities[key]!);
      const cost = BigInt(verifiedCosts[key] ?? '0');
      // Costs can explain a debit on an asset holding, never debt/positions/allowance differences.
      if (
        (/\/(debt|supplied|allowance|collateral|order|position|residual)\//.test(key) &&
          difference !== 0n) ||
        difference !== -cost
      )
        differences[key] = difference.toString();
    }
    const unresolved = operations.some(
      (o) =>
        o.runId === runId &&
        !['completed', 'rejected', 'cancelled', 'expired'].includes(o.status) &&
        !(
          o.status === 'failed' &&
          (!o.submission ||
            (o.submission.kind === 'ethereum' && o.evidence?.receiptStatus === 'reverted') ||
            settledBinanceFailure(run, o))
        ),
    );
    run.final = final;
    run.differences = differences;
    run.status =
      final.pending.length || unresolved || Object.keys(differences).length
        ? 'cleanup_outstanding'
        : 'restored';
    this.store.saveRun(run.id, run);
    return run;
  }
}
