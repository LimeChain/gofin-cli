import { createHash } from 'node:crypto';
import { z } from 'zod';
import { FinError } from '../core/errors.js';
import type {
  Adapter,
  Connection,
  Definition,
  FinConfig,
  Input,
  Preview,
  Reconciliation,
  RecordOperation,
  Submission,
  SubmissionHooks,
  Valuation,
} from '../core/types.js';

/** A pinned CCXT Binance instance, injected by the protected executor. No credential loading here. */
export interface BinanceClient {
  [method: string]: unknown;
}
export interface BinancePriceProvider {
  quote(coin: string): Promise<{ usdPerUnit: string; observedAt: string; source: string }>;
}
export interface BinanceEthereumAsset {
  coin: string;
  decimals: number;
  tokenAddress?: string;
}
export interface BinanceRecipientVerifier {
  verify(input: {
    txHash: string;
    chainId: 1;
    recipient: string;
    asset: BinanceEthereumAsset;
    minimumAmount: string;
    maximumAmount: string;
    confirmations: number;
  }): Promise<Reconciliation>;
}
export interface BinanceOptions {
  priceProvider?: BinancePriceProvider;
  recipientVerifier?: BinanceRecipientVerifier;
  now?: () => Date;
  ethereumAssets?: BinanceEthereumAsset[];
}
/** Internal run preparation only; these are independently observed child funds, never an execution override. */
export interface FundedWithdrawalSource {
  authority: string;
  from: string;
  coin: string;
  amount: string;
}
interface ObservedFunding {
  additionalAmount: string;
  maximumFundingUsdMicros: string;
  sourceValuationUsdMicros: string;
  dependencies: Input[];
}

export class BinanceAdapterError extends FinError {
  constructor(
    code: string,
    message: string,
    stage = 'validation',
    nextAction = 'Correct the request or account prerequisite before requesting a new approval.',
  ) {
    super(code, stage, message, nextAction);
    this.name = 'BinanceAdapterError';
  }
}
const fail = (code: string, message: string, stage?: string): never => {
  throw new BinanceAdapterError(code, message, stage);
};
const decimal = z
  .string()
  .max(80)
  .regex(/^(0|[1-9]\d*)(\.\d{1,18})?$/, 'Use a decimal string with at most 18 fractional digits.');
const positive = decimal.refine(
  (v) => /^(0|[1-9]\d*)(\.\d{1,18})?$/.test(v) && units(v) > 0n,
  'Amount must be positive.',
);
const name = z
  .string()
  .min(1)
  .max(80)
  .regex(/^[a-zA-Z0-9][a-zA-Z0-9_-]*$/);
const coin = z
  .string()
  .min(1)
  .max(32)
  .regex(/^[A-Z0-9]+$/);
const requestId = z
  .string()
  .min(8)
  .max(128)
  .regex(/^[a-zA-Z0-9][a-zA-Z0-9._:-]*$/);
const time = z.number().int().nonnegative().safe();
const accountSchema = z.object({ account: name, authority: name.optional() }).strict();
const historySchema = z
  .object({
    account: name,
    coin: coin.optional(),
    startTime: time.optional(),
    endTime: time.optional(),
    txHash: z.string().max(100).optional(),
  })
  .strict();
const transferSchema = z
  .object({
    requestId,
    authority: name,
    from: name,
    to: name,
    coin,
    amount: positive,
    fromWallet: z.literal('SPOT'),
    toWallet: z.literal('SPOT'),
  })
  .strict();
const withdrawSchema = z
  .object({
    requestId,
    account: name,
    destination: name,
    coin,
    amount: positive,
    network: z.literal('ETH'),
    walletType: z.literal('SPOT'),
    maxFee: decimal,
    minReceiveAmount: positive,
  })
  .strict();
const coverage =
  'Endpoint fixtures; production writes and real approval verification require a live run.';
const definition = (
  operation: string,
  kind: 'read' | 'write',
  schema: Definition['schema'],
  description: string,
  examples: Input[],
  completion: string,
): Definition => ({ name: operation, kind, schema, description, examples, coverage, completion });
export const binanceDefinitions: Definition[] = [
  definition(
    'binance.balances',
    'read',
    accountSchema,
    'SPOT free and committed balances. An explicit main authority can read a configured child.',
    [{ account: 'binance-main' }],
    'Observed balances; no financial submission.',
  ),
  definition(
    'binance.permissions',
    'read',
    z.object({ account: name }).strict(),
    'Observed API permissions, preserving unknown values.',
    [{ account: 'binance-main' }],
    'Permissions observed; endpoint availability is not eligibility.',
  ),
  definition(
    'binance.open-orders',
    'read',
    z
      .object({
        account: name,
        symbol: z
          .string()
          .min(1)
          .max(32)
          .regex(/^[A-Z0-9]+$/)
          .optional(),
      })
      .strict(),
    'Current SPOT open orders with exact venue IDs and quantities; requires the selected account read credential.',
    [{ account: 'binance-main' }],
    'Open orders observed; main authority cannot silently substitute for a child trading credential.',
  ),
  definition(
    'binance.hierarchy',
    'read',
    z.object({ authority: name }).strict(),
    'Configured main/sub hierarchy compared with the main account subaccount list.',
    [{ authority: 'binance-main' }],
    'Configured children matched against venue identities.',
  ),
  definition(
    'binance.networks',
    'read',
    z.object({ account: name, coin: coin.optional() }).strict(),
    'Exact Binance network IDs, availability, fees, limits, and token contracts.',
    [{ account: 'binance-main', coin: 'ETH' }],
    'Funding network metadata observed.',
  ),
  definition(
    'binance.deposit-address',
    'read',
    z.object({ account: name, coin, network: z.string().min(1).max(32) }).strict(),
    'Selected deposit instructions including memo and confirmations; creates no transfer.',
    [{ account: 'binance-main', coin: 'ETH', network: 'ETH' }],
    'Address observed; deposit credit requires separate history lookup.',
  ),
  definition(
    'binance.deposit-history',
    'read',
    historySchema,
    'Paginated venue deposit history; correlates a supplied txHash and distinguishes locked credit.',
    [{ account: 'binance-main', coin: 'ETH' }],
    'A matching status 1 credit is usable; chain receipt alone is insufficient.',
  ),
  definition(
    'binance.withdrawal-history',
    'read',
    historySchema
      .omit({ txHash: true })
      .extend({ withdrawOrderId: z.string().min(1).max(128).optional() })
      .strict(),
    'Paginated withdrawals; venue completion remains distinct from recipient receipt.',
    [{ account: 'binance-main', coin: 'ETH' }],
    'Withdrawal venue history observed.',
  ),
  definition(
    'binance.transfer',
    'write',
    transferSchema,
    'Main-authorized SPOT transfer to/from a configured child; stable clientTranId prevents blind retries.',
    [
      {
        requestId: 'example-transfer-1',
        authority: 'binance-main',
        from: 'binance-sub-a',
        to: 'binance-main',
        coin: 'ETH',
        amount: '0.001',
        fromWallet: 'SPOT',
        toWallet: 'SPOT',
      },
    ],
    'Matching SUCCESS history plus observed source debit and destination credit.',
  ),
  definition(
    'binance.withdraw',
    'write',
    withdrawSchema,
    'Ethereum mainnet withdrawal to a named wallet. Quantity is the Binance API amount; preview binds conservative gross debit and net receipt bounds.',
    [
      {
        requestId: 'example-withdraw-1',
        account: 'binance-main',
        destination: 'hot-wallet',
        coin: 'ETH',
        amount: '0.005',
        network: 'ETH',
        walletType: 'SPOT',
        maxFee: '0.001',
        minReceiveAmount: '0.004',
      },
    ],
    'Matching completed venue record, bounded fee/debit, and verified mainnet asset/recipient receipt.',
  ),
];

const SCALE = 10n ** 18n;
function units(value: string): bigint {
  if (!/^(0|[1-9]\d*)(\.\d{1,18})?$/.test(value))
    return fail('INVALID_DECIMAL', 'Invalid exact decimal quantity.');
  const [whole = '', fraction = ''] = value.split('.');
  return BigInt(whole) * SCALE + BigInt(fraction.padEnd(18, '0'));
}
function amount(value: bigint): string {
  const negative = value < 0n;
  const v = negative ? -value : value;
  const fraction = (v % SCALE).toString().padStart(18, '0').replace(/0+$/, '');
  return `${negative ? '-' : ''}${v / SCALE}${fraction ? `.${fraction}` : ''}`;
}
function object(value: unknown): Input {
  if (!value || typeof value !== 'object' || Array.isArray(value))
    return fail('VENUE_RESPONSE_INVALID', 'Binance returned an unexpected response shape.', 'read');
  return value as Input;
}
function rows(value: unknown): Input[] {
  if (!Array.isArray(value))
    return fail('VENUE_RESPONSE_INVALID', 'Binance returned an unexpected list.', 'read');
  return value.map(object);
}
function exact(value: unknown): string {
  if (typeof value !== 'string')
    return fail(
      'VENUE_RESPONSE_INVALID',
      'Binance returned a non-string financial quantity.',
      'read',
    );
  units(value);
  return value;
}
function identifier(value: unknown): string {
  if (typeof value === 'string' && value.length > 0 && value.length < 160) return value;
  if (typeof value === 'number' && Number.isSafeInteger(value) && value >= 0) return String(value);
  return fail(
    'VENUE_RESPONSE_INVALID',
    'Binance identifier cannot be represented exactly.',
    'read',
  );
}
function sameAddress(a: unknown, b: unknown): boolean {
  return typeof a === 'string' && typeof b === 'string' && a.toLowerCase() === b.toLowerCase();
}
function financialId(request: string, operation: string): string {
  return `fin${createHash('sha256').update(`${operation}:${request}`).digest('hex').slice(0, 29)}`;
}
const ethereumAssets: BinanceEthereumAsset[] = [
  { coin: 'ETH', decimals: 18 },
  { coin: 'USDC', decimals: 6, tokenAddress: '0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48' },
  { coin: 'USDT', decimals: 6, tokenAddress: '0xdAC17F958D2ee523a2206206994597C13D831ec7' },
  { coin: 'DAI', decimals: 18, tokenAddress: '0x6B175474E89094C44Da98b954EedeAC495271d0F' },
];
interface SpotBalance {
  asset: string;
  free: string;
  locked: string;
  freeze?: string;
  withdrawing?: string;
}
interface TransferRoute {
  authority: Connection;
  from: Connection;
  to: Connection;
  params: Input;
}

export class BinanceAdapter implements Adapter {
  readonly definitions = binanceDefinitions;
  private readonly now: () => Date;
  constructor(
    private readonly config: FinConfig,
    private readonly clients: Record<string, BinanceClient>,
    private readonly options: BinanceOptions = {},
  ) {
    this.now = options.now ?? (() => new Date());
  }
  private parse(operation: string, input: Input): Input {
    const def = this.definitions.find((d) => d.name === operation);
    if (!def) return fail('UNSUPPORTED_OPERATION', 'This Binance operation is not implemented.');
    const parsed = def.schema.safeParse(input);
    if (!parsed.success)
      return fail(
        'INVALID_INPUT',
        `Invalid ${operation} request: ${parsed.error.issues.map((i) => `${i.path.join('.')}: ${i.message}`).join('; ')}`,
      );
    return parsed.data as Input;
  }
  private connection(value: unknown, kind: 'binance' | 'ethereum' = 'binance'): Connection {
    const connection = this.config.connections.find((c) => c.name === value && c.kind === kind);
    if (!connection)
      return fail(
        'UNKNOWN_CONNECTION',
        'The selected named connection does not exist or has the wrong kind.',
      );
    return connection;
  }
  private async call(account: string, method: string, params: Input = {}): Promise<unknown> {
    const client = this.clients[account];
    const handler = client?.[method];
    if (typeof handler !== 'function')
      return fail(
        'MISSING_ACCESS',
        'The selected Binance connection or required CCXT endpoint is unavailable.',
        'read',
      );
    try {
      return await handler.call(client, params);
    } catch (error) {
      // Exchange messages frequently contain signed URLs, headers, and private account IDs. Never retain them.
      const errorName = error instanceof Error ? error.name : '';
      const code = /RateLimit|DDoS/.test(errorName)
        ? 'VENUE_RATE_LIMIT'
        : /Authentication|Permission/.test(errorName)
          ? 'VENUE_PERMISSION'
          : /RequestTimeout|Network/.test(errorName)
            ? 'VENUE_UNAVAILABLE'
            : 'VENUE_REQUEST_FAILED';
      throw new BinanceAdapterError(
        code,
        'Binance request did not return a usable response. Reconcile any persisted submission; never retry a financial write blindly.',
        'venue',
        'Use operation status/reconciliation; inspect account access separately.',
      );
    }
  }
  private async travelRule(account: string): Promise<void> {
    const client = this.clients[account];
    let raw: unknown;
    if (typeof client?.sapiGetLocalentityQuestionnaireRequirements === 'function')
      raw = await this.call(account, 'sapiGetLocalentityQuestionnaireRequirements');
    else if (typeof client?.request === 'function') {
      try {
        raw = await client.request('localentity/questionnaire-requirements', 'sapi', 'GET', {});
      } catch {
        return fail(
          'TRAVEL_RULE_UNKNOWN',
          'Travel Rule requirements could not be established. Standard withdrawal is blocked.',
          'preview',
        );
      }
    } else
      return fail(
        'TRAVEL_RULE_UNKNOWN',
        'Travel Rule requirements endpoint is unavailable. Standard withdrawal is blocked.',
        'preview',
      );
    if (object(raw).questionnaireCountryCode !== 'NIL')
      return fail(
        'TRAVEL_RULE_REQUIRED',
        'This account requires a Travel Rule withdrawal flow, which is not yet implemented. Standard withdrawal is blocked.',
        'preview',
      );
  }
  private async permissions(account: string): Promise<Input> {
    const raw = object(await this.call(account, 'sapiGetAccountApiRestrictions'));
    const output: Input = {};
    for (const field of [
      'enableReading',
      'enableWithdrawals',
      'enableInternalTransfer',
      'permitsUniversalTransfer',
      'ipRestrict',
      'enableSpotAndMarginTrading',
    ])
      output[field] = typeof raw[field] === 'boolean' ? raw[field] : 'unknown';
    return output;
  }
  private main(authority: Connection): void {
    if (authority.role !== 'main')
      fail(
        'WRONG_AUTHORITY',
        'Universal transfer requires the explicitly selected main-account authority.',
      );
  }
  private child(child: Connection, authority: Connection): void {
    if (child.role !== 'sub' || child.parent !== authority.name || !child.email)
      fail(
        'WRONG_HIERARCHY',
        'Selected account is not a configured child of the selected authority.',
      );
  }
  private async observedChild(child: Connection, authority: Connection): Promise<void> {
    this.child(child, authority);
    const response = object(
      await this.call(authority.name, 'sapiGetSubAccountList', {
        email: child.email,
        page: 1,
        limit: 200,
      }),
    );
    const match = rows(response.subAccounts).find((r) => r.email === child.email);
    if (!match)
      return fail(
        'HIERARCHY_UNVERIFIED',
        'Configured child was not found under the selected main account.',
      );
    if (match.isFreeze !== false)
      return fail('ACCOUNT_FROZEN_OR_UNKNOWN', 'Child freeze status is unavailable or frozen.');
  }
  private async balances(connection: Connection, authority?: Connection): Promise<SpotBalance[]> {
    if (connection.walletType && connection.walletType !== 'SPOT')
      return fail('UNSUPPORTED_WALLET', 'Only Binance SPOT wallet balances are implemented.');
    let raw: Input;
    if (authority && authority.name !== connection.name) {
      this.main(authority);
      this.child(connection, authority);
      raw = object(
        // V4 returns decimal strings; V3 returns JSON numbers that cannot preserve exact financial units.
        await this.call(authority.name, 'sapiV4GetSubAccountAssets', { email: connection.email }),
      );
    } else raw = object(await this.call(connection.name, 'privateGetAccount'));
    return rows(raw.balances).map((r) => ({
      asset:
        typeof r.asset === 'string'
          ? r.asset
          : fail('VENUE_RESPONSE_INVALID', 'Missing balance asset.'),
      free: exact(r.free),
      locked: exact(r.locked),
      ...(authority && authority.name !== connection.name
        ? { freeze: exact(r.freeze), withdrawing: exact(r.withdrawing) }
        : {}),
    }));
  }
  private free(balances: SpotBalance[], asset: string): string {
    return balances.find((b) => b.asset === asset)?.free ?? '0';
  }
  private async currencies(account: string): Promise<Input[]> {
    return rows(await this.call(account, 'sapiGetCapitalConfigGetall'));
  }
  private async currency(account: string, asset: string): Promise<Input> {
    const value = (await this.currencies(account)).find((c) => c.coin === asset);
    if (!value || value.isLegalMoney === true)
      return fail('UNKNOWN_ASSET', 'Selected Binance crypto asset is unavailable.');
    return value;
  }
  private network(currency: Input, network: string): Input {
    const selected = rows(currency.networkList).find(
      (n) => n.network === network && n.coin === currency.coin,
    );
    if (!selected)
      return fail('UNKNOWN_NETWORK', 'Selected asset/network pair is not advertised by Binance.');
    if (selected.denomination !== undefined && selected.denomination !== 1)
      return fail(
        'UNSUPPORTED_DENOMINATION',
        'Non-unit exchange token denomination is not implemented.',
      );
    return selected;
  }
  private precision(value: string, network: Input): void {
    const step = units(exact(network.withdrawIntegerMultiple));
    if (step <= 0n || units(value) % step !== 0n)
      return fail(
        'INVALID_PRECISION',
        'Amount does not match the selected Binance withdrawal precision.',
      );
  }
  private async transferRoute(input: Input): Promise<TransferRoute> {
    const authority = this.connection(input.authority);
    const from = this.connection(input.from);
    const to = this.connection(input.to);
    this.main(authority);
    if (from.name === to.name) return fail('SAME_ACCOUNT', 'Source and destination must differ.');
    if (from.name !== authority.name) await this.observedChild(from, authority);
    if (to.name !== authority.name) await this.observedChild(to, authority);
    if (from.name !== authority.name && to.name !== authority.name)
      return fail('UNSUPPORTED_ROUTE', 'Beta transfers support main-to-sub and sub-to-main only.');
    const permissions = await this.permissions(authority.name);
    if (permissions.enableInternalTransfer !== true)
      return fail(
        'MISSING_PERMISSION',
        'The explicitly selected main key must have observed internal-transfer permission.',
      );
    const params: Input = {
      asset: input.coin,
      amount: input.amount,
      fromAccountType: 'SPOT',
      toAccountType: 'SPOT',
      clientTranId: financialId(String(input.requestId), 'binance.transfer'),
    };
    if (from.role === 'sub') params.fromEmail = from.email;
    if (to.role === 'sub') params.toEmail = to.email;
    return { authority, from, to, params };
  }
  private assetIdentity(asset: string, network: Input): BinanceEthereumAsset {
    const identity = (this.options.ethereumAssets ?? ethereumAssets).find((a) => a.coin === asset);
    if (!identity || identity.decimals < 0 || identity.decimals > 18)
      return fail(
        'UNSUPPORTED_ASSET_IDENTITY',
        'No reviewed Ethereum mainnet identity exists for this exchange asset.',
      );
    if (identity.tokenAddress && !sameAddress(identity.tokenAddress, network.contractAddress))
      return fail(
        'ASSET_IDENTITY_MISMATCH',
        'Binance network token contract differs from the configured mainnet identity.',
      );
    if (!identity.tokenAddress && asset !== 'ETH')
      return fail('ASSET_IDENTITY_MISMATCH', 'Only ETH is implemented as a native Ethereum asset.');
    if (
      !identity.tokenAddress &&
      network.contractAddress !== undefined &&
      network.contractAddress !== ''
    )
      return fail(
        'ASSET_IDENTITY_MISMATCH',
        'Native ETH route unexpectedly advertises a token contract.',
      );
    return { ...identity };
  }
  private async valuation(
    asset: string,
    gross: string,
    fee: string,
  ): Promise<Valuation | undefined> {
    if (!this.options.priceProvider) return undefined;
    const quote = await this.options.priceProvider.quote(asset);
    const price = units(quote.usdPerUnit);
    const age = this.now().getTime() - Date.parse(quote.observedAt);
    if (price <= 0n || !Number.isFinite(age) || age < -5000 || age > 60_000 || !quote.source)
      return fail(
        'STALE_VALUATION',
        'A positive USD price observed within 60 seconds is required.',
      );
    const usdMicros = (value: string) =>
      ((units(value) * price * 1_000_000n + SCALE * SCALE - 1n) / (SCALE * SCALE)).toString();
    // movement + maximum fee is the conservative gross debit; avoid counting this bound twice.
    return {
      movementUsdMicros: usdMicros(gross),
      maximumFeeUsdMicros: usdMicros(fee),
      observedAt: quote.observedAt,
      source: quote.source,
    };
  }
  private window(input: Input, days: number): Input {
    const now = this.now().getTime();
    const start = Number(input.startTime ?? now - days * 86_400_000 + 1);
    const end = Number(input.endTime ?? now);
    if (
      !Number.isSafeInteger(start) ||
      !Number.isSafeInteger(end) ||
      start < 0 ||
      end < start ||
      end - start >= days * 86_400_000 ||
      end > now + 5000
    )
      return fail(
        'INVALID_HISTORY_WINDOW',
        `History window must be positive and shorter than ${days} days.`,
      );
    return { startTime: start, endTime: end };
  }
  private async capitalHistory(
    account: string,
    method: string,
    params: Input,
  ): Promise<{ records: Input[]; complete: boolean }> {
    const records: Input[] = [];
    const limit = 1000;
    for (let page = 0; page < 20; page++) {
      const fetched = rows(
        await this.call(account, method, { ...params, offset: page * limit, limit }),
      );
      records.push(...fetched);
      if (fetched.length < limit) return { records, complete: true };
    }
    return { records, complete: false };
  }
  private fundingRecord(row: Input, deposit: boolean): Input {
    // Allowlist: do not return txKey, info, signed response dumps, or unknown future fields.
    const out: Input = {};
    for (const field of [
      'id',
      'coin',
      'amount',
      'transactionFee',
      'network',
      'address',
      'addressTag',
      'txId',
      'status',
      'withdrawOrderId',
      'insertTime',
      'applyTime',
      'completeTime',
      'confirmTimes',
      'travelRuleStatus',
      'transferType',
      'walletType',
    ])
      if (row[field] !== undefined) out[field] = row[field];
    if (out.amount !== undefined) exact(out.amount);
    if (out.transactionFee !== undefined) exact(out.transactionFee);
    if (deposit)
      out.creditState =
        row.status === 1 && row.travelRuleStatus !== 1
          ? 'credited'
          : row.status === 6
            ? 'credited_locked'
            : 'pending';
    else
      out.venueState =
        row.status === 6
          ? 'completed'
          : row.status === 3 || row.status === 1 || row.status === 5
            ? 'failed'
            : 'pending';
    return out;
  }
  async read(operation: string, raw: Input): Promise<unknown> {
    const input = this.parse(operation, raw);
    const observedAt = this.now().toISOString();
    if (operation === 'binance.hierarchy') {
      const authority = this.connection(input.authority);
      this.main(authority);
      const children = this.config.connections.filter(
        (c) => c.kind === 'binance' && c.parent === authority.name,
      );
      const accounts: Input[] = [];
      for (const child of children) {
        try {
          await this.observedChild(child, authority);
          accounts.push({
            name: child.name,
            role: 'sub',
            parent: authority.name,
            ownership: 'observed',
            walletTypes: ['SPOT'],
          });
        } catch (e) {
          accounts.push({
            name: child.name,
            role: 'sub',
            parent: authority.name,
            ownership: 'unverified',
            reason: e instanceof BinanceAdapterError ? e.code : 'UNAVAILABLE',
            walletTypes: ['SPOT'],
          });
        }
      }
      return {
        operation,
        authority: authority.name,
        observedAt,
        accounts,
        accountModel: 'main_authorized_SPOT',
        subAuthorizedTransfer: 'not_implemented',
      };
    }
    const connection = this.connection(input.account);
    if (operation === 'binance.balances')
      return {
        operation,
        account: connection.name,
        walletType: 'SPOT',
        observedAt,
        balances: await this.balances(
          connection,
          input.authority ? this.connection(input.authority) : undefined,
        ),
      };
    if (operation === 'binance.permissions')
      return {
        operation,
        account: connection.name,
        observedAt,
        permissions: await this.permissions(connection.name),
      };
    if (operation === 'binance.open-orders') {
      const orders = rows(
        await this.call(
          connection.name,
          'privateGetOpenOrders',
          input.symbol ? { symbol: input.symbol } : {},
        ),
      ).map((row) => {
        const order: Input = { orderId: identifier(row.orderId) };
        for (const key of ['symbol', 'clientOrderId', 'side', 'type', 'status', 'timeInForce'])
          if (typeof row[key] === 'string') order[key] = row[key];
        for (const key of [
          'price',
          'origQty',
          'executedQty',
          'cummulativeQuoteQty',
          'icebergQty',
          'stopPrice',
        ])
          if (row[key] !== undefined) order[key] = exact(row[key]);
        for (const key of ['time', 'updateTime'])
          if (typeof row[key] === 'number' && Number.isSafeInteger(row[key])) order[key] = row[key];
        if (
          typeof order.symbol !== 'string' ||
          typeof order.status !== 'string' ||
          order.origQty === undefined ||
          order.executedQty === undefined
        )
          return fail(
            'VENUE_RESPONSE_INVALID',
            'Open order identity, status, or exact quantities are unavailable.',
          );
        return order;
      });
      return { operation, account: connection.name, walletType: 'SPOT', observedAt, orders };
    }
    if (operation === 'binance.networks') {
      const currencies = input.coin
        ? [await this.currency(connection.name, String(input.coin))]
        : await this.currencies(connection.name);
      return {
        operation,
        account: connection.name,
        observedAt,
        assets: currencies.map((c) => ({
          coin: c.coin,
          depositAllEnable: c.depositAllEnable,
          withdrawAllEnable: c.withdrawAllEnable,
          networks: rows(c.networkList).map((n) => {
            const out: Input = {};
            for (const key of [
              'network',
              'coin',
              'name',
              'depositEnable',
              'withdrawEnable',
              'withdrawIntegerMultiple',
              'withdrawFee',
              'withdrawMin',
              'withdrawMax',
              'depositDust',
              'minConfirm',
              'unLockConfirm',
              'withdrawTag',
              'busy',
              'contractAddress',
              'denomination',
              'depositDesc',
              'withdrawDesc',
            ])
              if (n[key] !== undefined) out[key] = n[key];
            return out;
          }),
        })),
      };
    }
    if (operation === 'binance.deposit-address') {
      const network = this.network(
        await this.currency(connection.name, String(input.coin)),
        String(input.network),
      );
      if (network.depositEnable !== true)
        return fail('DEPOSIT_DISABLED', 'Selected deposit route is disabled.');
      const response = object(
        await this.call(connection.name, 'sapiGetCapitalDepositAddress', {
          coin: input.coin,
          network: input.network,
        }),
      );
      if (typeof response.address !== 'string' || !response.address)
        return fail('VENUE_RESPONSE_INVALID', 'Deposit address is unavailable.');
      if (input.network === 'ETH' && !/^0x[0-9a-fA-F]{40}$/.test(response.address))
        return fail('VENUE_RESPONSE_INVALID', 'Ethereum deposit address is not a valid address.');
      if (response.coin !== undefined && response.coin !== input.coin)
        return fail('ASSET_IDENTITY_MISMATCH', 'Deposit address response has another asset.');
      if (response.network !== undefined && response.network !== input.network)
        return fail('NETWORK_MISMATCH', 'Deposit address response has another network.');
      return {
        operation,
        account: connection.name,
        observedAt,
        coin: input.coin,
        network: input.network,
        address: response.address,
        memo: typeof response.tag === 'string' ? response.tag : '',
        tokenContract: network.contractAddress ?? 'unknown',
        denomination: network.denomination ?? 'unknown',
        minimumConfirmations: network.minConfirm ?? 'unknown',
        unlockConfirmations: network.unLockConfirm ?? 'unknown',
        minimumDeposit: network.depositDust ?? 'unknown',
        usableCreditRequires: 'matching Binance deposit history with creditState=credited',
      };
    }
    if (operation === 'binance.deposit-history' || operation === 'binance.withdrawal-history') {
      const deposit = operation === 'binance.deposit-history';
      const params = this.window(input, input.withdrawOrderId ? 7 : 90);
      if (input.coin) params.coin = input.coin;
      if (input.withdrawOrderId) params.withdrawOrderId = input.withdrawOrderId;
      const history = await this.capitalHistory(
        connection.name,
        deposit ? 'sapiGetCapitalDepositHisrec' : 'sapiGetCapitalWithdrawHistory',
        params,
      );
      const found = input.txHash
        ? history.records.filter((r) => sameAddress(r.txId, input.txHash))
        : history.records;
      return {
        operation,
        account: connection.name,
        observedAt,
        complete: history.complete,
        records: found.map((r) => this.fundingRecord(r, deposit)),
      };
    }
    return fail(
      'WRONG_OPERATION_KIND',
      'Write operations must use preview and the protected execution path.',
    );
  }
  async preview(operation: string, raw: Input): Promise<Preview> {
    return this.preparePreview(operation, raw);
  }
  /** Read-only conditional preparation. Only confirmed transfers can make the ordinary write preview executable. */
  async preflightFundedWithdrawal(
    raw: Input,
    sources: FundedWithdrawalSource[],
    maximumFundingUsdMicros: string,
  ): Promise<Preview> {
    const input = this.parse('binance.withdraw', raw);
    const account = this.connection(input.account);
    this.main(account);
    if (
      !/^[1-9]\d*$/.test(maximumFundingUsdMicros) ||
      BigInt(maximumFundingUsdMicros) > 100_000_000n
    )
      return fail(
        'FUNDING_CAP',
        'Conditional test preparation requires a positive exact funding cap at most $100.',
      );
    if (!Array.isArray(sources) || !sources.length)
      return fail(
        'FUNDING_SOURCES',
        'Conditional withdrawal preparation requires selected child funding sources.',
      );
    const sourceSchema = z.object({ authority: name, from: name, coin, amount: positive }).strict();
    const seen = new Set<string>();
    const seenChildIdentities = new Set<string>();
    const dependencies: Input[] = [];
    let additional = 0n;
    let valued = 0n;
    const currency = await this.currency(account.name, String(input.coin));
    const identity = this.assetIdentity(String(input.coin), this.network(currency, 'ETH'));
    for (const rawSource of sources) {
      const parsed = sourceSchema.safeParse(rawSource);
      if (!parsed.success)
        return fail(
          'FUNDING_SOURCES',
          'Funding sources must name exact child accounts, asset, and decimal quantity.',
        );
      const source = parsed.data;
      if (
        source.authority !== account.name ||
        source.coin !== input.coin ||
        source.from === account.name ||
        seen.has(source.from)
      )
        return fail(
          'FUNDING_SOURCE_MISMATCH',
          'Funding requires unique children of this main account and the exact withdrawal asset.',
        );
      seen.add(source.from);
      const child = this.connection(source.from);
      this.child(child, account);
      // Reject aliases conservatively; venue discovery still independently proves the exact configured email.
      const childIdentity = `${account.name}:${child.email!.trim().toLowerCase()}`;
      if (seenChildIdentities.has(childIdentity))
        return fail(
          'FUNDING_SOURCE_MISMATCH',
          'Multiple configured aliases cannot project the same child account balance twice.',
        );
      seenChildIdentities.add(childIdentity);
      if (units(source.amount) % 10n ** BigInt(18 - identity.decimals) !== 0n)
        return fail(
          'INVALID_PRECISION',
          'Selected funding cannot be represented in the reviewed mainnet asset base units.',
        );
      const transfer = await this.preview('binance.transfer', {
        requestId: financialId(`${input.requestId}:${source.from}`, 'funding-preflight'),
        authority: source.authority,
        from: source.from,
        to: account.name,
        coin: source.coin,
        amount: source.amount,
        fromWallet: 'SPOT',
        toWallet: 'SPOT',
      });
      if (!transfer.valuation)
        return fail(
          'VALUATION_UNAVAILABLE',
          'Conditional funding requires fresh source USD valuation.',
        );
      valued += BigInt(transfer.valuation.movementUsdMicros);
      if (valued > BigInt(maximumFundingUsdMicros))
        return fail('FUNDING_CAP', 'Selected child funding exceeds its bounded test allocation.');
      additional += units(source.amount);
      dependencies.push({
        authority: account.name,
        from: source.from,
        to: account.name,
        coin: source.coin,
        amount: source.amount,
        transferBinding: transfer.binding,
        valuation: transfer.valuation,
        completionRequired:
          'matching durable transfer identity, successful venue history and observed source debit plus main credit',
      });
    }
    const preview = await this.preparePreview('binance.withdraw', input, {
      additionalAmount: amount(additional),
      maximumFundingUsdMicros,
      sourceValuationUsdMicros: valued.toString(),
      dependencies,
    });
    if (
      !preview.valuation ||
      BigInt(preview.valuation.movementUsdMicros) + BigInt(preview.valuation.maximumFeeUsdMicros) >
        100_000_000n
    )
      return fail(
        'FUNDING_CAP',
        'Conditional withdrawal gross quantity and fees must fit the $100 test movement limit.',
      );
    return preview;
  }
  private async preparePreview(
    operation: string,
    raw: Input,
    funding?: ObservedFunding,
  ): Promise<Preview> {
    const input = this.parse(operation, raw);
    const now = this.now();
    const common = {
      operation,
      request: input,
      observedAt: now.toISOString(),
      expiresAt: new Date(now.getTime() + 60_000).toISOString(),
    };
    if (operation === 'binance.transfer') {
      const route = await this.transferRoute(input);
      const currency = await this.currency(route.authority.name, String(input.coin));
      // Asset funding precision is explicit; internal-only assets with no reviewed precision fail closed.
      const network =
        rows(currency.networkList).find((n) => n.network === 'ETH') ??
        rows(currency.networkList)[0];
      if (!network)
        return fail('PRECISION_UNKNOWN', 'No funding precision is available for this asset.');
      this.precision(String(input.amount), network);
      const source = await this.balances(route.from, route.authority);
      if (units(this.free(source, String(input.coin))) < units(String(input.amount)))
        return fail(
          'INSUFFICIENT_BALANCE',
          'SPOT free balance is insufficient; locked balances are not transferable.',
        );
      const valuation = await this.valuation(String(input.coin), String(input.amount), '0');
      return {
        ...common,
        binding: {
          authority: route.authority.name,
          from: route.from.name,
          to: route.to.name,
          params: route.params,
        },
        summary: `Transfer ${input.amount} ${input.coin} from ${route.from.name} SPOT to ${route.to.name} SPOT using ${route.authority.name}; internal fee 0.`,
        checks: [
          {
            name: 'hierarchy',
            status: 'passed',
            detail: 'Child identity and unfrozen status observed under explicit main authority.',
          },
          {
            name: 'permission',
            status: 'passed',
            detail: 'Internal-transfer permission observed.',
          },
          {
            name: 'free_balance',
            status: 'passed',
            detail: 'Source transferable balance covers quantity.',
          },
          {
            name: 'precision',
            status: 'passed',
            detail: 'Exact exchange funding increment; no rounding.',
          },
        ],
        warnings: valuation
          ? []
          : ['USD valuation is unavailable; this preview cannot establish the live-test limits.'],
        ...(valuation ? { valuation } : {}),
      };
    }
    if (operation !== 'binance.withdraw')
      return fail('WRONG_OPERATION_KIND', 'Only Binance writes support preview.');
    const account = this.connection(input.account);
    const destination = this.connection(input.destination, 'ethereum');
    if (
      this.config.ethereum.chainId !== 1 ||
      !destination.address ||
      !/^0x[0-9a-fA-F]{40}$/.test(destination.address)
    )
      return fail(
        'WRONG_NETWORK',
        'Withdrawal requires a configured Ethereum mainnet destination wallet.',
      );
    const permission = await this.permissions(account.name);
    if (permission.enableWithdrawals !== true)
      return fail(
        'MISSING_PERMISSION',
        'Selected key does not have observed withdrawal permission.',
      );
    await this.travelRule(account.name);
    const currency = await this.currency(account.name, String(input.coin));
    const network = this.network(currency, 'ETH');
    if (
      currency.withdrawAllEnable !== true ||
      network.withdrawEnable !== true ||
      network.busy === true
    )
      return fail(
        'WITHDRAWAL_DISABLED',
        'Selected Ethereum withdrawal route is unavailable or busy.',
      );
    if (network.withdrawTag === true)
      return fail(
        'MEMO_REQUIRED',
        'This route requires a memo/tag not supported by the named Ethereum wallet withdrawal.',
      );
    this.precision(String(input.amount), network);
    const fee = exact(network.withdrawFee);
    const maximumFee = String(input.maxFee);
    const quantity = String(input.amount);
    const minimumReceive = String(input.minReceiveAmount);
    if (units(fee) > units(maximumFee))
      return fail(
        'FEE_BOUND_EXCEEDED',
        'Published withdrawal fee exceeds the approved maximum fee.',
      );
    if (
      units(quantity) < units(exact(network.withdrawMin)) ||
      units(quantity) > units(exact(network.withdrawMax))
    )
      return fail(
        'WITHDRAWAL_LIMIT',
        'API quantity is outside the selected route minimum/maximum.',
      );
    const netBound = units(quantity) - units(maximumFee);
    const grossBound = amount(units(quantity) + units(maximumFee));
    if (netBound <= 0n || units(minimumReceive) > netBound)
      return fail(
        'RECEIPT_BOUND_UNSUPPORTED',
        'Minimum receipt exceeds the conservative quantity-minus-maximum-fee bound.',
      );
    const identity = this.assetIdentity(String(input.coin), network);
    if (
      units(quantity) % 10n ** BigInt(18 - identity.decimals) !== 0n ||
      units(minimumReceive) % 10n ** BigInt(18 - identity.decimals) !== 0n
    )
      return fail(
        'INVALID_PRECISION',
        'Quantity is not representable in the mainnet token base units.',
      );
    const balances = await this.balances(account);
    const actualFree = this.free(balances, String(input.coin));
    const preparedFree = units(actualFree) + (funding ? units(funding.additionalAmount) : 0n);
    if (preparedFree < units(grossBound))
      return fail(
        'INSUFFICIENT_BALANCE',
        'SPOT free balance must cover the conservative gross debit including the maximum fee.',
      );
    if (!this.options.recipientVerifier)
      return fail(
        'RECEIPT_VERIFIER_MISSING',
        'Ethereum recipient verification must be configured before a withdrawal.',
      );
    const params: Input = {
      coin: input.coin,
      amount: input.amount,
      network: 'ETH',
      address: destination.address,
      walletType: 0,
      withdrawOrderId: financialId(String(input.requestId), operation),
    };
    const valuation = await this.valuation(String(input.coin), quantity, maximumFee);
    return {
      ...common,
      binding: {
        account: account.name,
        destination: destination.name,
        chainId: 1,
        asset: identity,
        params,
        maximumFee,
        maximumGrossDebit: grossBound,
        minimumReceive,
        maximumReceive: quantity,
        confirmations: this.config.ethereum.confirmations,
        ...(funding
          ? {
              preparationOnly: true,
              fundingDependencies: funding.dependencies,
              observedMainFree: actualFree,
              projectedMainFree: amount(preparedFree),
              maximumFundingUsdMicros: funding.maximumFundingUsdMicros,
              sourceValuationUsdMicros: funding.sourceValuationUsdMicros,
            }
          : {}),
      },
      summary: `Withdraw API quantity ${quantity} ${input.coin} from ${account.name} SPOT to ${destination.name} (${destination.address}) on Ethereum mainnet; published fee ${fee}, maximum fee ${maximumFee}; maximum gross debit ${grossBound}, minimum net receipt ${minimumReceive}.`,
      checks: [
        {
          name: 'permission',
          status: 'passed',
          detail: 'Withdrawal permission observed on selected key.',
        },
        {
          name: 'travel_rule',
          status: 'passed',
          detail: 'Account returned NIL questionnaire requirement.',
        },
        {
          name: 'network_asset',
          status: 'passed',
          detail: 'Explicit ETH network and reviewed native/token identity.',
        },
        {
          name: 'precision_balance_limits',
          status: 'passed',
          detail: funding
            ? 'Exact funding increment and limits checked; available balance is conditional on separately verified child consolidation.'
            : 'Exact funding increment, available balance, and advertised minimum/maximum checked.',
        },
        ...(funding
          ? [
              {
                name: 'funding_dependency',
                status: 'unchecked' as const,
                detail:
                  'Preparation only. Execute and reconcile the selected child-to-main transfers, then obtain a fresh ordinary withdrawal preview and approval.',
              },
            ]
          : []),
        {
          name: 'recipient_receipt',
          status: 'unchecked',
          detail: 'Mainnet recipient receipt and confirmations are checked after submission.',
        },
      ],
      warnings: [
        ...(funding
          ? [
              'This conditional preparation cannot be submitted or revalidated. It does not authorize a withdrawal before live main-account credit.',
            ]
          : []),
        'Binance API documentation does not specify exact debit-versus-receipt fee semantics. Debit is bounded by quantity + maximum fee and receipt by quantity - maximum fee; final accounting requires venue history and chain evidence.',
        ...(valuation
          ? []
          : ['USD valuation is unavailable; this preview cannot establish the live-test limits.']),
      ],
      ...(valuation ? { valuation } : {}),
    };
  }
  async revalidate(preview: Preview): Promise<Preview> {
    if (preview.binding.preparationOnly === true)
      return fail(
        'PREPARATION_ONLY',
        'Conditional funding preparation cannot be revalidated for execution.',
        'submission',
      );
    if (Date.parse(preview.expiresAt) < this.now().getTime())
      return fail('PREVIEW_EXPIRED', 'The approved preview has expired.', 'submission');
    const current = await this.preview(preview.operation, preview.request);
    if (JSON.stringify(current.binding) !== JSON.stringify(preview.binding))
      return fail(
        'PREVIEW_CHANGED',
        'Material operation parameters changed; obtain fresh approval.',
        'submission',
      );
    return current;
  }
  async submit(preview: Preview, hooks: SubmissionHooks): Promise<Submission> {
    if (preview.binding.preparationOnly === true)
      return fail(
        'PREPARATION_ONLY',
        'Conditional funding preparation cannot be submitted.',
        'submission',
      );
    // This method is called only inside the protected executor after authorization. Recheck current state.
    const current = await this.revalidate(preview);
    const binding = current.binding;
    const params = object(binding.params);
    const account = String(binding.authority ?? binding.account);
    const asset = String(params.asset ?? params.coin);
    const from = this.connection(binding.from ?? binding.account);
    const authority = binding.authority ? this.connection(binding.authority) : undefined;
    const sourceFree = this.free(await this.balances(from, authority), asset);
    const data: Input = {
      operation: preview.operation,
      binding,
      submittedAt: this.now().toISOString(),
      sourceFreeBefore: sourceFree,
    };
    if (preview.operation === 'binance.transfer')
      data.destinationFreeBefore = this.free(
        await this.balances(this.connection(binding.to), authority),
        asset,
      );
    const reference = String(params.clientTranId ?? params.withdrawOrderId);
    const submission: Submission = { kind: 'binance', reference, data };
    hooks.persistSubmission(submission);
    const response = object(
      await this.call(
        account,
        preview.operation === 'binance.transfer'
          ? 'sapiPostSubAccountUniversalTransfer'
          : 'sapiPostCapitalWithdrawApply',
        params,
      ),
    );
    const venueId = identifier(
      preview.operation === 'binance.transfer' ? response.tranId : response.id,
    );
    if (response.clientTranId !== undefined && response.clientTranId !== reference)
      return fail(
        'SUBMISSION_ID_MISMATCH',
        'Venue returned an unexpected client identity. Reconcile the original persisted request.',
        'submission',
      );
    const updated = { ...submission, data: { ...data, venueId } };
    hooks.persistSubmission(updated);
    return updated;
  }
  async reconcile(record: RecordOperation): Promise<Reconciliation> {
    if (!record.submission || record.submission.kind !== 'binance')
      return {
        status: 'unknown',
        evidence: { reason: 'No persisted Binance submission identity exists.' },
      };
    try {
      const data = record.submission.data;
      const binding = object(data.binding);
      const params = object(binding.params);
      const started = Date.parse(String(data.submittedAt));
      if (!Number.isFinite(started))
        return {
          status: 'unknown',
          evidence: { reason: 'Persisted submission timestamp is invalid.' },
        };
      const startTime = Math.max(0, started - 60_000);
      const endTime = Math.min(this.now().getTime(), startTime + 7 * 86_400_000 - 1);
      if (record.operation === 'binance.transfer')
        return await this.reconcileTransfer(data, binding, params, record.submission.reference, {
          startTime,
          endTime,
        });
      if (record.operation === 'binance.withdraw')
        return await this.reconcileWithdrawal(data, binding, params, record.submission.reference, {
          startTime,
          endTime,
        });
      return {
        status: 'unknown',
        evidence: { reason: 'Unsupported persisted Binance operation.' },
      };
    } catch (error) {
      return {
        status: 'unknown',
        evidence: {
          reason: error instanceof BinanceAdapterError ? error.code : 'RECONCILIATION_UNAVAILABLE',
          nextAction:
            'Reconcile the original operation; do not submit another withdrawal or transfer.',
        },
      };
    }
  }
  private async reconcileTransfer(
    data: Input,
    binding: Input,
    params: Input,
    reference: string,
    window: Input,
  ): Promise<Reconciliation> {
    const authority = this.connection(binding.authority);
    const from = this.connection(binding.from);
    const to = this.connection(binding.to);
    const matches: Input[] = [];
    let complete = false;
    for (let page = 1; page <= 20; page++) {
      const query: Input = { ...window, clientTranId: reference, page, limit: 500 };
      if (params.fromEmail) query.fromEmail = params.fromEmail;
      else if (params.toEmail) query.toEmail = params.toEmail;
      const result = object(
        await this.call(authority.name, 'sapiGetSubAccountUniversalTransfer', query),
      );
      const fetched = rows(result.result);
      matches.push(...fetched.filter((r) => r.clientTranId === reference));
      if (
        fetched.length < 500 ||
        (typeof result.totalCount === 'number' && page * 500 >= result.totalCount)
      ) {
        complete = true;
        break;
      }
    }
    if (matches.length === 0)
      return {
        status: 'unknown',
        evidence: {
          clientTranId: reference,
          historyComplete: complete,
          reason: 'No matching history; absence does not prove no transfer.',
        },
      };
    if (matches.length !== 1)
      return {
        status: 'unknown',
        evidence: {
          clientTranId: reference,
          reason: 'Multiple matching transfers need operator reconciliation.',
        },
      };
    const row = matches[0]!;
    const correct =
      row.asset === params.asset &&
      units(exact(row.amount)) === units(String(params.amount)) &&
      row.fromAccountType === 'SPOT' &&
      row.toAccountType === 'SPOT' &&
      (!params.fromEmail || row.fromEmail === params.fromEmail) &&
      (!params.toEmail || row.toEmail === params.toEmail) &&
      (from.role !== 'main' || !from.email || row.fromEmail === from.email) &&
      (to.role !== 'main' || !to.email || row.toEmail === to.email) &&
      (!data.venueId || identifier(row.tranId) === data.venueId);
    if (!correct)
      return {
        status: 'unknown',
        evidence: {
          clientTranId: reference,
          reason: 'Transfer history identity or amount mismatch.',
        },
      };
    const evidence: Input = {
      clientTranId: reference,
      venueId: identifier(row.tranId),
      from: from.name,
      to: to.name,
      asset: row.asset,
      amount: exact(row.amount),
      venueStatus: row.status,
      historyComplete: complete,
    };
    if (row.status === 'FAILED' || row.status === 'FAILURE') return { status: 'failed', evidence };
    if (row.status !== 'SUCCESS') return { status: 'pending', evidence };
    const sourceFree = this.free(await this.balances(from, authority), String(params.asset));
    const destinationFree = this.free(await this.balances(to, authority), String(params.asset));
    const quantity = units(String(params.amount));
    const debit = units(exact(data.sourceFreeBefore)) - units(sourceFree);
    const credit = units(destinationFree) - units(exact(data.destinationFreeBefore));
    Object.assign(evidence, {
      sourceFree,
      destinationFree,
      sourceDebitObserved: amount(debit),
      destinationCreditObserved: amount(credit),
      balanceCorrelation:
        'net changes; concurrent unrelated activity can prevent or weaken attribution',
    });
    if (debit < quantity || credit < quantity)
      return {
        status: 'pending',
        evidence: {
          ...evidence,
          reason: 'Matching success history exists but account debit/credit is not yet observed.',
        },
      };
    return { status: 'completed', evidence };
  }
  private async reconcileWithdrawal(
    data: Input,
    binding: Input,
    params: Input,
    reference: string,
    window: Input,
  ): Promise<Reconciliation> {
    const account = this.connection(binding.account);
    const history = await this.capitalHistory(account.name, 'sapiGetCapitalWithdrawHistory', {
      ...window,
      coin: params.coin,
      withdrawOrderId: reference,
    });
    const matches = history.records.filter(
      (r) => r.withdrawOrderId === reference || (!!data.venueId && r.id === data.venueId),
    );
    if (matches.length !== 1)
      return {
        status: 'unknown',
        evidence: {
          withdrawOrderId: reference,
          historyComplete: history.complete,
          reason: matches.length
            ? 'Multiple matching withdrawal records.'
            : 'No matching venue record; absence does not prove no withdrawal.',
        },
      };
    const row = matches[0]!;
    if (
      row.coin !== params.coin ||
      row.network !== 'ETH' ||
      !sameAddress(row.address, params.address) ||
      row.transferType !== 0 ||
      row.walletType !== 0 ||
      (data.venueId && row.id !== data.venueId) ||
      (row.withdrawOrderId && row.withdrawOrderId !== reference)
    )
      return {
        status: 'unknown',
        evidence: {
          reason:
            'Withdrawal history identity, network, recipient, SPOT wallet, or external-transfer type mismatch.',
          withdrawOrderId: reference,
        },
      };
    const evidence: Input = {
      ...this.fundingRecord(row, false),
      historyComplete: history.complete,
      withdrawOrderId: reference,
    };
    if ([1, 3, 5].includes(Number(row.status))) return { status: 'failed', evidence };
    if (row.status !== 6) return { status: 'pending', evidence };
    const reportedAmount = units(exact(row.amount));
    if (
      reportedAmount < units(String(binding.minimumReceive)) ||
      reportedAmount > units(String(binding.maximumReceive))
    )
      return {
        status: 'unknown',
        evidence: {
          ...evidence,
          reason: 'Venue quantity lies outside the approved receipt/API-quantity bounds.',
        },
      };
    const fee = exact(row.transactionFee);
    if (units(fee) > units(String(binding.maximumFee)))
      return {
        status: 'unknown',
        evidence: {
          ...evidence,
          reason: 'Actual venue fee exceeded bound; reconcile financial state.',
        },
      };
    if (
      typeof row.txId !== 'string' ||
      !/^0x[0-9a-fA-F]{64}$/.test(row.txId) ||
      !this.options.recipientVerifier
    )
      return {
        status: 'pending',
        evidence: {
          ...evidence,
          reason: 'Venue completed; mainnet receipt verification is unavailable.',
        },
      };
    const receipt = await this.options.recipientVerifier.verify({
      txHash: row.txId,
      chainId: 1,
      recipient: String(params.address),
      asset: object(binding.asset) as unknown as BinanceEthereumAsset,
      minimumAmount: String(binding.minimumReceive),
      maximumAmount: String(binding.maximumReceive),
      confirmations: Number(binding.confirmations),
    });
    evidence.recipient = receipt.evidence;
    if (receipt.status === 'failed')
      return {
        status: 'unknown',
        evidence: {
          ...evidence,
          reason:
            'Venue completed but the payout transaction reverted; keep reconciling this withdrawal identity for a replacement payout or definitive venue resolution.',
        },
      };
    if (receipt.status !== 'completed') return { status: receipt.status, evidence };
    const sourceFree = this.free(await this.balances(account), String(params.coin));
    const debit = units(exact(data.sourceFreeBefore)) - units(sourceFree);
    Object.assign(evidence, {
      sourceFree,
      sourceDebitObserved: amount(debit),
      feeAmount: fee,
      balanceCorrelation: 'net balance change; unrelated activity can obscure debit attribution',
    });
    if (debit < units(String(params.amount)) - units(String(binding.maximumFee)))
      return {
        status: 'pending',
        evidence: { ...evidence, reason: 'Recipient verified; source debit is not yet observed.' },
      };
    if (debit > units(String(binding.maximumGrossDebit)))
      return {
        status: 'unknown',
        evidence: {
          ...evidence,
          reason:
            'Source balance change exceeds conservative debit bound; accounting requires reconciliation.',
        },
      };
    return { status: 'completed', evidence };
  }
}
