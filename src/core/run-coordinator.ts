import type { Adapter, FinConfig, Input, Preview, RecordOperation } from './types.js';
import type { FundedWithdrawalSource } from '../adapters/binance.js';
import { formatUnits } from 'viem';
import {
  BudgetGuard,
  type FinancialSnapshot,
  type RunPlan,
  type TestRun,
  TEST_LIMIT,
} from './budget.js';
import { OperationStore } from './store.js';
import { FinError } from './errors.js';
import { exactUnits, observedUnits } from './canonical.js';
import {
  createInventory,
  holdingKey,
  binanceInventoryAsset,
  type InventoryScope,
} from './inventory.js';
import { z } from 'zod';
const WETH = '0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2',
  USDC = '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48';
const micros = z.string().regex(/^\d+$/);
const routeSchema = z
  .object({
    account: z.string(),
    coin: z.enum(['ETH', 'USDC']),
    network: z.literal('ETH'),
    wallet: z.string(),
    withdrawAmount: z.string(),
    maxWithdrawalFee: z.string(),
    minimumReceipt: z.string(),
    minimumReturnAmount: z.string(),
  })
  .strict();
export const planSchema = z
  .object({
    id: z.string().min(1).max(128),
    allocations: z.record(z.string(), micros),
    borrowReserveUsdMicros: micros,
    costReserveUsdMicros: micros,
    unwind: z.array(z.string().min(1)).min(1),
    returnRoutes: z.array(routeSchema).default([]),
    wallets: z.array(z.string()).min(1),
    accounts: z.array(z.string()).default([]),
  })
  .strict();
interface UsdReader {
  quoteUsd(
    token: string,
    units?: bigint,
  ): Promise<{ valueUsdMicros?: string; observedAt: string; source: string }>;
}
export class RunCoordinator {
  constructor(
    private config: FinConfig,
    private adapters: Adapter[],
    private store: OperationStore,
    private options: { now?: () => number } = {},
  ) {}
  private now() {
    return this.options.now?.() ?? Date.now();
  }
  private async read(operation: string, input: Input): Promise<any> {
    const a = this.adapters.find((a) => a.definitions.some((d) => d.name === operation));
    if (!a)
      throw new FinError(
        'ADAPTER_UNAVAILABLE',
        'snapshot',
        'Required snapshot adapter is unavailable.',
        'Restore adapter access before testing.',
      );
    return a.read(operation, input);
  }
  async snapshot(wallets: string[], accounts: string[]): Promise<FinancialSnapshot> {
    const quantities: Record<string, string> = {},
      pending: string[] = [],
      metadata: Record<string, unknown> = { wallets, accounts };
    for (const wallet of wallets) {
      const base = { wallet, network: 'ethereum', chainId: 1 };
      const balances = await this.read('wallet.balances', { ...base, tokens: [WETH, USDC] });
      for (const holding of balances.data.holdings)
        quantities[holdingKey(wallet, holding.address)] = holding.units;
      // Discover every reserve: unrelated collateral/debt must not finance a scoped test.
      const positions = await this.read('aave.positions', base);
      for (const position of positions.data.positions) {
        const token = position.token.address.toLowerCase();
        quantities[`${wallet}/supplied/${token}`] = position.supplied.units;
        quantities[`${wallet}/debt/${token}`] = position.variableDebt.units;
        quantities[`${wallet}/debt/stable-${token}`] = position.stableDebt.units;
        quantities[`${wallet}/collateral/${token}`] = position.collateralEnabled ? '1' : '0';
      }
      quantities[`${wallet}/position/aave-collateral-base`] =
        positions.data.account.totalCollateralBase;
      quantities[`${wallet}/position/aave-debt-base`] = positions.data.account.totalDebtBase;
      const allowanceReads = await Promise.all(
        [WETH, USDC].flatMap((token) =>
          [this.config.ethereum.aavePool, this.config.ethereum.uniswapRouter].map(
            async (spender) => ({
              token,
              spender,
              observation: await this.read('wallet.allowance', { ...base, token, spender }),
            }),
          ),
        ),
      );
      for (const { token, spender, observation } of allowanceReads)
        quantities[`${wallet}/allowance/${token}/${spender.toLowerCase()}`] =
          observation.data.units;
    }
    for (const account of accounts) {
      const connection = this.config.connections.find(
        (c) => c.name === account && c.kind === 'binance',
      );
      if (!connection)
        throw new FinError(
          'UNKNOWN_CONNECTION',
          'snapshot',
          'Unknown selected exchange account.',
          'Select configured accounts.',
        );
      const balances = await this.read('binance.balances', {
        account,
        ...(connection.role === 'sub' ? { authority: connection.parent } : {}),
      });
      metadata[`${account}:quantityUnits`] = {
        venueDecimals: 18,
        supportedHoldingDecimals: { native: 18, [USDC]: 6 },
        otherHoldingDecimals: 18,
        orderDecimals: 18,
        residualDecimals: 18,
        projection:
          'supported free holding=floor(venue units/factor); exact residual=venue units%factor; factor=10^(18-holding decimals)',
        residualPolicy: 'unallocated and immutable; exact change prevents restoration',
      };
      for (const balance of balances.balances) {
        const token = ['ETH', 'USDC'].includes(balance.asset)
          ? binanceInventoryAsset(balance.asset).token
          : `binance:${balance.asset}`;
        for (const field of ['freeze', 'withdrawing'])
          if (balance[field] !== undefined)
            quantities[`${account}/order/${field}/${token}`] = observedUnits(
              balance[field],
              18,
            ).toString();
        const venueFree = observedUnits(balance.free, 18);
        quantities[`${account}/order/locked/${token}`] = observedUnits(
          balance.locked,
          18,
        ).toString();
        if (balance.asset === 'ETH' || balance.asset === 'USDC') {
          const asset = binanceInventoryAsset(balance.asset);
          const factor = 10n ** BigInt(18 - asset.decimals);
          quantities[holdingKey(account, asset.token)] = (venueFree / factor).toString();
          quantities[`${account}/residual/${asset.token}`] = (venueFree % factor).toString();
        } else {
          quantities[`${account}/holding/binance:${balance.asset}`] = venueFree.toString();
        }
      }
      for (const operation of ['binance.withdrawal-history', 'binance.deposit-history']) {
        const result = await this.read(operation, { account });
        if (result.complete === false)
          throw new FinError(
            'SNAPSHOT_INCOMPLETE',
            'snapshot',
            'Funding history scan was incomplete.',
            'Use a complete current observation before testing.',
          );
        for (const row of result.records ?? [])
          if (
            row.venueState === 'pending' ||
            row.creditState === 'pending' ||
            row.creditState === 'credited_locked'
          )
            pending.push(`${account}:${operation}:${row.id ?? row.txId ?? 'unidentified'}`);
      }
      const orders = await this.read('binance.open-orders', { account });
      metadata[`${account}:orders`] = orders.orders;
    }
    for (const record of this.store.list())
      if (['submitted', 'pending', 'unknown'].includes(record.status))
        pending.push(`fin:${record.id}`);
    return { observedAt: new Date(this.now()).toISOString(), quantities, pending, metadata };
  }
  async begin(raw: Input): Promise<TestRun> {
    const parsed = planSchema.safeParse(raw);
    if (!parsed.success)
      throw new FinError(
        'INVALID_RUN_PLAN',
        'budget',
        'The run plan does not match its schema.',
        'Use selected exact-unit allocations, borrowed-proceeds and cost reserves, scoped locations, and a complete unwind.',
      );
    const p = parsed.data;
    if (
      new Set(p.wallets).size !== p.wallets.length ||
      new Set(p.accounts).size !== p.accounts.length
    )
      throw new FinError(
        'DUPLICATE_SCOPE',
        'budget',
        'A location can be selected only once.',
        'Avoid counting the same funds twice.',
      );
    const walletAddresses: Record<string, string> = {};
    const physicalIdentities = new Set<string>();
    for (const wallet of p.wallets) {
      const c = this.config.connections.find((c) => c.name === wallet && c.kind === 'ethereum');
      if (!c?.address || !c.credential)
        throw new FinError(
          'RETURN_SIGNER_REQUIRED',
          'budget',
          'Every selected live wallet requires a protected return signer.',
          'Configure all forward and return signers before testing.',
        );
      const identity = `ethereum:${c.address.toLowerCase()}`;
      if (physicalIdentities.has(identity))
        throw new FinError(
          'DUPLICATE_SCOPE',
          'budget',
          'Selected names resolve to the same wallet.',
          'Select each physical source once.',
        );
      physicalIdentities.add(identity);
      walletAddresses[wallet] = c.address;
    }
    for (const account of p.accounts) {
      const connection = this.config.connections.find(
        (c) => c.name === account && c.kind === 'binance',
      );
      if (!connection)
        throw new FinError(
          'UNKNOWN_CONNECTION',
          'budget',
          'A selected exchange connection is unavailable.',
          'Select configured accounts.',
        );
      const identity =
        connection.role === 'sub'
          ? `binance:sub:${connection.parent}:${connection.email?.trim().toLowerCase()}`
          : `binance:main:${connection.credential ?? connection.name}`;
      if (physicalIdentities.has(identity))
        throw new FinError(
          'DUPLICATE_SCOPE',
          'budget',
          'Selected names resolve to the same exchange source.',
          'Select each physical source once.',
        );
      physicalIdentities.add(identity);
    }
    const scope: InventoryScope = {
      walletAddresses,
      accounts: p.accounts,
      returnDeposits: {},
      returnMinimums: {},
      tokens: ['native', WETH, USDC],
      spenders: [this.config.ethereum.aavePool, this.config.ethereum.uniswapRouter],
      confirmations: this.config.ethereum.confirmations,
    };
    for (const route of p.returnRoutes) {
      if (!p.accounts.includes(route.account) || !p.wallets.includes(route.wallet))
        throw new FinError(
          'RETURN_SCOPE',
          'budget',
          'Return routes must use selected accounts and wallets.',
          'Include each affected location in the snapshot.',
        );
      const instructions = await this.read('binance.deposit-address', {
        account: route.account,
        coin: route.coin,
        network: route.network,
      });
      const networks = await this.read('binance.networks', {
        account: route.account,
        coin: route.coin,
      });
      const network = networks.assets?.[0]?.networks?.find((n: any) => n.network === route.network);
      if (
        !network?.depositEnable ||
        !network?.withdrawEnable ||
        network.busy ||
        instructions.memo ||
        instructions.tag ||
        instructions.addressTag
      )
        throw new FinError(
          'RETURN_ROUTE_UNAVAILABLE',
          'budget',
          'The requested round-trip funding route is unavailable or requires an unsupported memo.',
          'Verify the entire return before initiating.',
        );
      const routeAsset = binanceInventoryAsset(route.coin);
      let depositMinimum: bigint, plannedReturn: bigint, outwardMinimum: bigint;
      try {
        const factor = 10n ** BigInt(18 - routeAsset.decimals);
        const depositVenueUnits = observedUnits(network.depositDust, 18);
        depositMinimum = (depositVenueUnits + factor - 1n) / factor;
        plannedReturn = exactUnits(route.minimumReturnAmount, routeAsset.decimals);
        outwardMinimum = exactUnits(route.minimumReceipt, routeAsset.decimals);
      } catch {
        throw new FinError(
          'RETURN_MINIMUM_UNAVAILABLE',
          'budget',
          'Exact deposit and planned return minimums are required.',
          'Read the route deposit minimum and bound the net return after the complete unwind.',
        );
      }
      if (network.withdrawFee !== undefined) {
        try {
          observedUnits(network.withdrawFee, routeAsset.decimals);
        } catch {
          throw new FinError(
            'RETURN_FEE_UNREPRESENTABLE',
            'budget',
            'The observed withdrawal fee has meaningful precision below the mainnet asset unit.',
            'Select a return route with an exactly representable fee before consolidation.',
          );
        }
      }
      if (plannedReturn <= 0n || plannedReturn < depositMinimum || plannedReturn > outwardMinimum)
        throw new FinError(
          'RETURN_MINIMUM',
          'budget',
          'The planned net return cannot meet the exchange deposit minimum within the outward receipt.',
          'Choose a route and funded unwind that return usable exchange credit.',
        );
      const sourceLocations = [
        route.wallet,
        ...p.accounts.filter((name) => {
          const connection = this.config.connections.find(
            (c) => c.name === name && c.kind === 'binance',
          );
          return (
            name === route.account ||
            (connection?.role === 'sub' && connection.parent === route.account)
          );
        }),
      ];
      const selectedFunding = sourceLocations.reduce(
        (sum, name) => sum + BigInt(p.allocations[holdingKey(name, routeAsset.token)] ?? '0'),
        0n,
      );
      const requiredFunding =
        exactUnits(route.withdrawAmount, routeAsset.decimals) +
        exactUnits(route.maxWithdrawalFee, routeAsset.decimals);
      if (selectedFunding < requiredFunding)
        throw new FinError(
          'RETURN_UNFUNDED',
          'budget',
          'The selected exact-asset sources cannot fund the withdrawal and its maximum fee.',
          'Fund the entire return from selected holdings; unrelated venue balances cannot establish readiness.',
        );
      const adapter = this.adapters.find((a) =>
        a.definitions.some((d) => d.name === 'binance.withdraw'),
      )!;
      // Validate permissions, Travel Rule, minimum, precision, fees, asset identity, available
      // allocation and named recipient with the same executable preview as the outward route.
      const withdrawalRequest = {
        requestId: `preflight-${p.id}`,
        account: route.account,
        destination: route.wallet,
        coin: route.coin,
        amount: route.withdrawAmount,
        network: 'ETH',
        walletType: 'SPOT',
        maxFee: route.maxWithdrawalFee,
        minReceiveAmount: route.minimumReceipt,
      };
      const sources: FundedWithdrawalSource[] = p.accounts.flatMap((from) => {
        const child = this.config.connections.find((c) => c.name === from && c.kind === 'binance');
        const units = p.allocations[holdingKey(from, routeAsset.token)];
        return child?.role === 'sub' &&
          child.parent === route.account &&
          units &&
          BigInt(units) > 0n
          ? [
              {
                authority: route.account,
                from,
                coin: route.coin,
                amount: formatUnits(BigInt(units), routeAsset.decimals),
              },
            ]
          : [];
      });
      const fundingAdapter = adapter as Adapter & {
        preflightFundedWithdrawal?: (
          request: Input,
          sources: FundedWithdrawalSource[],
          maximumFundingUsdMicros: string,
        ) => Promise<Preview>;
      };
      if (sources.length && !fundingAdapter.preflightFundedWithdrawal)
        throw new FinError(
          'FUNDED_PREFLIGHT_UNAVAILABLE',
          'budget',
          'The adapter cannot independently preflight the selected consolidation dependency.',
          'Use an adapter that verifies source funds and the full return before consolidation.',
        );
      const preflight = sources.length
        ? await fundingAdapter.preflightFundedWithdrawal!(
            withdrawalRequest,
            sources,
            TEST_LIMIT.toString(),
          )
        : await adapter.preview('binance.withdraw', withdrawalRequest);
      if (
        !preflight.valuation ||
        BigInt(preflight.valuation.movementUsdMicros) +
          BigInt(preflight.valuation.maximumFeeUsdMicros) >
          TEST_LIMIT
      )
        throw new FinError(
          'RETURN_BUDGET',
          'budget',
          'Funding return minimum and fee do not fit $100.',
          'Select a route whose complete outward and return minimums fit.',
        );
      const address = instructions.address ?? instructions.instructions?.address;
      if (typeof address !== 'string')
        throw new FinError(
          'RETURN_ROUTE_UNAVAILABLE',
          'budget',
          'Exact deposit instructions could not be established.',
          'Inspect the account funding instructions.',
        );
      const deposit = address.toLowerCase();
      if (scope.returnDeposits[deposit] && scope.returnDeposits[deposit] !== route.account)
        throw new FinError(
          'AMBIGUOUS_RETURN_ACCOUNT',
          'budget',
          'A shared deposit address cannot distinguish the selected return accounts.',
          'Use unambiguous deposit instructions before moving funds.',
        );
      scope.returnDeposits[deposit] = route.account;
      const minimums = (scope.returnMinimums![deposit] ??= {});
      minimums[routeAsset.token] = plannedReturn.toString();
      if (route.coin === 'ETH') {
        const walletAdapter = this.adapters.find((a) =>
          a.definitions.some((d) => d.name === 'wallet.balances'),
        ) as unknown as {
          client: { getBytecode: (p: { address: string }) => Promise<string | undefined> };
        };
        const code = await walletAdapter.client.getBytecode({ address });
        if (code && code !== '0x')
          throw new FinError(
            'RETURN_ROUTE_UNSUPPORTED',
            'budget',
            'Native return recipient is a contract unsupported by direct-transfer reconciliation.',
            'Select a fully verifiable return route before initiating.',
          );
      }
    }
    if (
      p.accounts.some(
        (a) =>
          !p.returnRoutes.some((r) => r.account === a) &&
          this.config.connections.find((c) => c.name === a)?.role !== 'sub',
      )
    )
      throw new FinError(
        'RETURN_ROUTE_REQUIRED',
        'budget',
        'Each selected main account requires verified deposit and withdrawal routes.',
        'Include the complete source exchange return route.',
      );
    for (const account of p.accounts) {
      const c = this.config.connections.find((c) => c.name === account);
      if (c?.role === 'sub') {
        if (!c.parent || !p.accounts.includes(c.parent))
          throw new FinError(
            'RETURN_AUTHORITY_REQUIRED',
            'budget',
            'Selected child requires its selected main authority.',
            'Include the original source account and main authority.',
          );
        const permissions = await this.read('binance.permissions', { account: c.parent });
        const hierarchy = await this.read('binance.hierarchy', { authority: c.parent });
        if (
          permissions.permissions.enableInternalTransfer !== true ||
          !hierarchy.accounts.some((a: any) => a.name === account && a.ownership === 'observed')
        )
          throw new FinError(
            'RETURN_AUTHORITY_UNVERIFIED',
            'budget',
            'Main-to-sub return authority could not be established.',
            'Verify hierarchy and internal-transfer access before consolidation.',
          );
      }
    }
    const baseline = await this.snapshot(p.wallets, p.accounts);
    const inventory = createInventory(scope, p.allocations);
    let selected = 0n;
    const valuations: Input[] = [];
    // One principal representation per beta run plus native gas keeps a closed return
    // provable; a one-way conversion cannot masquerade as a different asset's gain.
    const principalTokens = new Set(
      Object.entries(inventory.initial)
        .filter(([, units]) => BigInt(units) > 0n)
        .map(([key]) => key.split('/')[2])
        .filter((token) => token !== 'native'),
    );
    if (principalTokens.size > 1)
      throw new FinError(
        'MULTI_ASSET_RUN_UNSUPPORTED',
        'budget',
        'This verification profile selects one non-native principal asset plus native gas.',
        'Run independently closed asset journeys; do not mix WETH and USDC starting allocations.',
      );
    const price = this.adapters.find((a) =>
      a.definitions.some((d) => d.name === 'wallet.balances'),
    ) as unknown as UsdReader;
    for (const [key, units] of Object.entries(inventory.initial)) {
      if (!(key in baseline.quantities) || BigInt(units) > BigInt(baseline.quantities[key]!))
        throw new FinError(
          'ALLOCATION_BALANCE',
          'budget',
          'Selected units exceed the independently observed source holding.',
          'Allocate only available selected funds.',
        );
      if (!key.includes('/holding/'))
        throw new FinError(
          'ISOLATED_POSITION_REQUIRED',
          'budget',
          'This live profile starts from holdings and zero test-market positions.',
          'Use isolated positions; do not adopt unrelated collateral or debt.',
        );
      const token = key.split('/')[2]!;
      const valuation = await price.quoteUsd(token, BigInt(units));
      if (
        !valuation.valueUsdMicros ||
        !Number.isFinite(Date.parse(valuation.observedAt)) ||
        this.now() - Date.parse(valuation.observedAt) > 120000 ||
        Date.parse(valuation.observedAt) > this.now() + 5000
      )
        throw new FinError(
          'VALUATION_UNAVAILABLE',
          'budget',
          'Selected funds lack a fresh trusted USD valuation.',
          'Refresh asset prices.',
        );
      selected += BigInt(valuation.valueUsdMicros);
      valuations.push({ key, units, ...valuation });
    }
    if (
      selected + BigInt(p.borrowReserveUsdMicros) > TEST_LIMIT ||
      selected < BigInt(p.costReserveUsdMicros)
    )
      throw new FinError(
        'ALLOCATION_LIMIT',
        'budget',
        'Selected source holdings, borrowed proceeds and funded return costs do not fit $100.',
        'Reduce selected units and keep the entire return funded.',
      );
    for (const [key, units] of Object.entries(baseline.quantities))
      if (/\/(debt|supplied|position)\//.test(key) && BigInt(units) !== 0n)
        throw new FinError(
          'ISOLATED_POSITION_REQUIRED',
          'budget',
          'A selected wallet already has Aave supplied assets or debt outside the isolated test.',
          'Use a wallet with zero Aave positions to avoid changing unrelated exposure.',
        );
    for (const [key, units] of Object.entries(baseline.quantities))
      if (key.includes('/allowance/') && BigInt(units) !== 0n)
        throw new FinError(
          'ISOLATED_ALLOWANCE_REQUIRED',
          'budget',
          'A selected wallet has existing spender authority that may be impossible to restore within the test allocation.',
          'Use zero baseline Pool/router allowances for this isolated verification profile.',
        );
    baseline.metadata.valuations = valuations;
    const plan: RunPlan = {
      id: p.id,
      initialAllocationUsdMicros: (selected - BigInt(p.costReserveUsdMicros)).toString(),
      borrowReserveUsdMicros: p.borrowReserveUsdMicros,
      costReserveUsdMicros: p.costReserveUsdMicros,
      unwind: p.unwind,
      returnRoutesVerified: true,
      baseline,
      inventory,
    };
    return new BudgetGuard(this.store, () => this.now()).begin(plan);
  }
  async enrichReconciliation(record: RecordOperation): Promise<RecordOperation> {
    if (
      record.status !== 'completed' ||
      record.operation !== 'wallet.transfer' ||
      !record.runId ||
      !record.preview ||
      !record.evidence
    )
      return record;
    const run = this.store.getRun<TestRun>(record.runId);
    const recipient = String((record.preview.binding.effect as Input).recipient).toLowerCase();
    const account = run?.inventory?.scope.returnDeposits[recipient];
    if (!account) return record;
    const token = record.preview.binding.token;
    const coin =
      token === 'native' ? 'ETH' : String(token).toLowerCase() === USDC ? 'USDC' : undefined;
    if (!coin) {
      record.status = 'unknown';
      return record;
    }
    const history = await this.read('binance.deposit-history', {
      account,
      coin,
      txHash: record.submission!.reference,
    });
    const rows = (history.records ?? []).filter(
      (r: any) =>
        r.txId === record.submission!.reference &&
        r.coin === coin &&
        r.network === 'ETH' &&
        r.creditState === 'credited',
    );
    if (rows.length !== 1) {
      record.status = 'pending';
      record.evidence.depositCredit = { state: 'pending', account };
      return record;
    }
    const asset = binanceInventoryAsset(coin);
    const units = observedUnits(rows[0].amount, asset.decimals).toString();
    if (units !== String((record.evidence.effects as Input).receivedUnits)) {
      record.status = 'unknown';
      record.evidence.depositCredit = { state: 'mismatched', account };
      return record;
    }
    record.evidence.depositCredit = {
      state: 'credited',
      account,
      coin,
      transactionHash: record.submission!.reference,
      amountUnits: units,
    };
    return record;
  }
  async revalueAllocation(id: string): Promise<void> {
    const run = this.store.getRun<TestRun>(id);
    if (!run?.inventory)
      throw new FinError(
        'RUN_REQUIRED',
        'budget',
        'A scoped live allocation is required.',
        'Begin a verified run.',
      );
    const price = this.adapters.find((a) =>
      a.definitions.some((d) => d.name === 'wallet.balances'),
    ) as unknown as UsdReader;
    let total = 0n;
    const prices: Input[] = [];
    for (const [key, trackedUnits] of Object.entries(run.inventory.available)) {
      if (BigInt(trackedUnits) === 0n) continue;
      const [wallet, kind, token] = key.split('/');
      let units = trackedUnits;
      if (kind === 'supplied' && run.inventory.introducedPositions[key]) {
        const observation = await this.read('aave.positions', {
          wallet,
          network: 'ethereum',
          chainId: 1,
          tokens: [token],
        });
        const position = observation.data.positions.find(
          (p: any) => p.token.address.toLowerCase() === token,
        );
        if (!position || BigInt(position.supplied.units) < BigInt(trackedUnits))
          throw new FinError(
            'INVENTORY_POSITION_CHANGED',
            'budget',
            'The supplied test position decreased outside its recorded operation.',
            'Reconcile the original run before another movement.',
          );
        units = position.supplied.units;
      }
      const quote = await price.quoteUsd(token!, BigInt(units));
      if (
        !quote.valueUsdMicros ||
        !Number.isFinite(Date.parse(quote.observedAt)) ||
        this.now() - Date.parse(quote.observedAt) > 120000 ||
        Date.parse(quote.observedAt) > this.now() + 5000
      )
        throw new FinError(
          'VALUATION_UNAVAILABLE',
          'budget',
          'Allocation could not be revalued with current prices.',
          'Refresh valuations before any movement.',
        );
      total += BigInt(quote.valueUsdMicros);
      prices.push({ key, units, ...quote });
    }
    const borrowed = Object.values(run.borrowReservations).reduce((n, v) => n + BigInt(v), 0n);
    const unusedBorrow =
      BigInt(run.borrowReserveUsdMicros) > borrowed
        ? BigInt(run.borrowReserveUsdMicros) - borrowed
        : 0n;
    const committedCosts = Object.values(run.feeReservations).reduce((n, v) => n + BigInt(v), 0n);
    total += unusedBorrow + committedCosts;
    if (total > TEST_LIMIT)
      throw new FinError(
        'ALLOCATION_REVALUED_LIMIT',
        'budget',
        'Current selected holdings, collateral, borrowed proceeds and execution costs exceed $100.',
        'Stop new exposure; reconcile the existing run and request an allowed recovery plan.',
      );
    run.baseline.metadata.latestAllocation = {
      observedAt: new Date(this.now()).toISOString(),
      maximumUsdMicros: total.toString(),
      prices,
    };
    this.store.saveRun(id, run);
  }
  async finish(id: string): Promise<TestRun> {
    const run = this.store.getRun<TestRun>(id);
    if (!run)
      throw new FinError(
        'RUN_NOT_FOUND',
        'status',
        'Run does not exist.',
        'Inspect the requested run identity.',
      );
    const final = await this.snapshot(
      run.baseline.metadata.wallets as string[],
      run.baseline.metadata.accounts as string[],
    );
    const costs: Record<string, string> = {};
    if (run.inventory) {
      // Principal relocation never enters this pool. Confirmed gas/venue/interest costs and
      // closed swap/supply effects may be attributed only to selected original holdings.
      const wethClosed =
        !Object.entries(run.inventory.initial).some(
          ([key, units]) => key.endsWith(WETH) && BigInt(units) > 0n,
        ) &&
        Object.keys(run.baseline.quantities)
          .filter((k) => k.endsWith(WETH) && k.includes('/holding/'))
          .every((k) => run.baseline.quantities[k] === final.quantities[k]);
      const economicAsset = (key: string) => {
        const token = key.split('/')[2]!;
        return wethClosed && token === WETH ? 'native' : token;
      };
      const proved: Record<string, bigint> = {};
      for (const [key, value] of Object.entries(run.inventory.costs)) {
        const asset = economicAsset(key);
        proved[asset] = (proved[asset] ?? 0n) + BigInt(value);
      }
      for (const [key, value] of Object.entries(run.inventory.economicEffects)) {
        const asset = economicAsset(key);
        proved[asset] = (proved[asset] ?? 0n) - BigInt(value);
      }
      const observed: Record<string, bigint> = {};
      const attribution: Record<string, string> = {};
      const selectedAssets = new Set(
        Object.entries(run.inventory.initial)
          .filter(([, v]) => BigInt(v) > 0n)
          .map(([key]) => key.split('/')[2]),
      );
      const intermediateClosed = Object.keys(run.baseline.quantities)
        .filter((key) => key.includes('/holding/') && !selectedAssets.has(key.split('/')[2]))
        .every((key) => final.quantities[key] === run.baseline.quantities[key]);
      for (const [key, initial] of Object.entries(run.inventory.initial)) {
        if (!key.includes('/holding/') || BigInt(initial) === 0n || !(key in final.quantities))
          continue;
        const difference = BigInt(run.baseline.quantities[key]!) - BigInt(final.quantities[key]!);
        const asset = economicAsset(key);
        // A proved loss cannot excuse a credit at another custody location.
        if (difference < 0n && (proved[asset] ?? 0n) >= 0n) continue;
        if (difference > 0n && (proved[asset] ?? 0n) < 0n) continue;
        observed[asset] = (observed[asset] ?? 0n) + difference;
        attribution[key] = difference.toString();
      }
      for (const [key, value] of Object.entries(attribution)) {
        const asset = economicAsset(key);
        if (intermediateClosed && observed[asset] === (proved[asset] ?? 0n)) costs[key] = value;
      }
      final.metadata.verifiedCosts = {
        provedAssetCosts: Object.fromEntries(
          Object.entries(proved).map(([k, v]) => [k, v.toString()]),
        ),
        attributedHoldingCosts: costs,
      };
    }
    const result = new BudgetGuard(this.store).finish(id, final, costs, this.store.list());
    for (const account of run.baseline.metadata.accounts as string[])
      if (
        JSON.stringify(run.baseline.metadata[`${account}:orders`]) !==
        JSON.stringify(final.metadata[`${account}:orders`])
      ) {
        result.status = 'cleanup_outstanding';
        result.differences![`${account}/order/state`] = 'changed';
      }
    this.store.saveRun(id, result);
    return result;
  }
}
