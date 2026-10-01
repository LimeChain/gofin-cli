import { createHash } from 'node:crypto';
import { FinError } from './errors.js';
export function canonical(value: unknown): string {
  if (value === null || typeof value === 'string' || typeof value === 'boolean')
    return JSON.stringify(value);
  if (typeof value === 'number' && Number.isSafeInteger(value)) return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
  if (typeof value === 'object' && value !== null)
    return `{${Object.entries(value)
      .filter(([, v]) => v !== undefined)
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([k, v]) => `${JSON.stringify(k)}:${canonical(v)}`)
      .join(',')}}`;
  throw new FinError(
    'INVALID_CANONICAL_VALUE',
    'validation',
    'Requests must contain exact JSON values.',
    'Use decimal strings for financial quantities.',
  );
}
export function fingerprint(value: unknown): string {
  return createHash('sha256').update(canonical(value)).digest('hex');
}
export function exactUnits(value: string, decimals: number): bigint {
  if (
    !Number.isInteger(decimals) ||
    decimals < 0 ||
    decimals > 255 ||
    !/^(0|[1-9]\d*)(\.\d+)?$/.test(value)
  )
    throw new FinError(
      'INVALID_AMOUNT',
      'validation',
      'Amount must be an unsigned decimal string.',
      'Use explicit asset units and precision.',
    );
  const [whole, fraction = ''] = value.split('.');
  if (fraction.length > decimals)
    throw new FinError(
      'INVALID_PRECISION',
      'validation',
      'Amount exceeds the asset precision.',
      'Choose an exactly representable amount; Fin never rounds.',
    );
  return BigInt(whole!) * 10n ** BigInt(decimals) + BigInt(fraction.padEnd(decimals, '0') || '0');
}

/** Venue observations may pad fractional zeroes; meaningful precision is never rounded. */
export function observedUnits(value: string, decimals: number): bigint {
  if (typeof value !== 'string')
    throw new FinError(
      'INVALID_AMOUNT',
      'validation',
      'Observed amount must be an unsigned decimal string.',
      'Preserve the exact venue observation without numeric coercion.',
    );
  const padded = /^(0|[1-9]\d*)\.(\d+)$/.exec(value);
  if (!padded) return exactUnits(value, decimals);
  const fraction = padded[2]!.replace(/0+$/, '');
  return exactUnits(fraction ? `${padded[1]}.${fraction}` : padded[1]!, decimals);
}
