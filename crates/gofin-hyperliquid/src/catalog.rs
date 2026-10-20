//! Hyperliquid capability catalog. Definitions advertise typed intent and exact coverage status.
use crate::types::*;
use gofin_domain::{Definition, ExecutionActor, OperationKind};
use schemars::{JsonSchema, schema_for};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadOutput {
    /// Native response with all supported decimal-valued fields serialized as exact strings.
    pub data: Value,
    pub source: String,
    pub observed_at: String,
}
#[derive(Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteOutput {
    pub status: String,
    pub operation_id: String,
    pub result: Value,
}
#[derive(Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ErrorOutput {
    pub code: String,
    pub stage: String,
    pub message: String,
    pub next_action: String,
}

fn schema<T: JsonSchema>() -> Value {
    serde_json::to_value(schema_for!(T)).expect("schema serializes")
}
fn definition(
    name: &str,
    description: &str,
    contract: (OperationKind, ExecutionActor),
    input: Value,
    output: Value,
    completion: &str,
    semantics: Value,
) -> Definition {
    let (kind, actor) = contract;
    let mut semantics = semantics;
    if kind != OperationKind::Read {
        semantics["example_inputs"] = json!(
            "Examples use synthetic account aliases, request IDs, and where applicable addresses/order IDs; replace them only after independently selecting the real reviewed scope."
        );
    }
    Definition {
        name: name.into(),
        description: description.into(),
        kind,
        execution_actor: actor,
        input_schema: input,
        output_schema: output,
        error_schema: schema::<ErrorOutput>(),
        examples: examples(name),
        coverage: coverage(name).into(),
        completion: completion.into(),
        semantics,
    }
}

fn examples(name: &str) -> Vec<Value> {
    if matches!(
        name,
        "hyperliquid.account.set_abstraction" | "hyperliquid.account.set_agent_abstraction"
    ) {
        let operation_id = if name.ends_with("set_agent_abstraction") {
            "AGENT-ABSTRACTION"
        } else {
            "ABSTRACTION"
        };
        return ["unified", "separate", "portfolio_margin"]
            .into_iter()
            .map(|mode| {
                json!({"request_id":format!("SYNTHETIC-{operation_id}-{}", mode.to_ascii_uppercase()),"account":"SYNTHETIC-ACCOUNT","mode":mode})
            })
            .collect();
    }
    let value = match name {
        "hyperliquid.markets" => json!({"kind":"perpetual"}),
        "hyperliquid.currencies" | "hyperliquid.tickers" | "hyperliquid.funding_rates" => {
            json!({"symbols":["BTC/USDC:USDC"]})
        }
        "hyperliquid.ticker"
        | "hyperliquid.order_book"
        | "hyperliquid.funding_rate"
        | "hyperliquid.open_interest" => json!({"symbol":"BTC/USDC:USDC"}),
        "hyperliquid.candles" => {
            json!({"symbol":"BTC/USDC:USDC","interval":"1h","start_ms":1893456000000u64,"end_ms":1893459600000u64,"limit":100})
        }
        "hyperliquid.trades" | "hyperliquid.funding_rate_history" => {
            json!({"symbol":"BTC/USDC:USDC","since_ms":1893456000000u64,"until_ms":1893459600000u64,"limit":100})
        }
        "hyperliquid.time" | "hyperliquid.status" => json!({}),
        "hyperliquid.balances" | "hyperliquid.positions" | "hyperliquid.open_orders" => {
            return vec![
                json!({"account":"SYNTHETIC-ACCOUNT"}),
                json!({"account":"SYNTHETIC-ACCOUNT","dex":"SYNTHETIC-HIP3-DEX"}),
            ];
        }
        "hyperliquid.trading_state" => {
            json!({"account":"SYNTHETIC-ACCOUNT","symbol":"ETH/USDC:USDC"})
        }
        "hyperliquid.fees" | "hyperliquid.abstraction" => json!({"account":"SYNTHETIC-ACCOUNT"}),
        "hyperliquid.order" => {
            json!({"account":"SYNTHETIC-ACCOUNT","symbol":"BTC/USDC:USDC","order_id":"SYNTHETIC-ORDER-ID"})
        }
        "hyperliquid.closed_orders"
        | "hyperliquid.canceled_orders"
        | "hyperliquid.orders"
        | "hyperliquid.my_trades"
        | "hyperliquid.my_liquidations"
        | "hyperliquid.funding_history"
        | "hyperliquid.ledger"
        | "hyperliquid.deposits"
        | "hyperliquid.withdrawals" => {
            json!({"account":"SYNTHETIC-ACCOUNT","since_ms":1893456000000u64,"until_ms":1893459600000u64,"limit":100})
        }
        "hyperliquid.orders.place" => limit_order_example("SYNTHETIC-REQUEST-001"),
        "hyperliquid.orders.batch" => {
            json!({"request_id":"SYNTHETIC-BATCH-001","account":"SYNTHETIC-ACCOUNT","orders":[limit_order_example("SYNTHETIC-CHILD-001")]})
        }
        "hyperliquid.orders.edit" => {
            json!({"request_id":"SYNTHETIC-EDIT-001","account":"SYNTHETIC-ACCOUNT","symbol":"BTC/USDC:USDC","market_type":"perpetual","order_id":"SYNTHETIC-ORDER-ID","size":"0.001","limit_price":"50000","max_notional_usdc":"100","max_fee_usdc":"0.1"})
        }
        "hyperliquid.orders.edit_batch" => {
            json!({"request_id":"SYNTHETIC-EDIT-BATCH-001","account":"SYNTHETIC-ACCOUNT","orders":[{"symbol":"BTC/USDC:USDC","market_type":"perpetual","order_id":"SYNTHETIC-ORDER-ID","size":"0.001","limit_price":"50000","max_notional_usdc":"100","max_fee_usdc":"0.1"}]})
        }
        "hyperliquid.orders.cancel" => {
            json!({"request_id":"SYNTHETIC-CANCEL-001","account":"SYNTHETIC-ACCOUNT","symbol":"BTC/USDC:USDC","order_id":"SYNTHETIC-ORDER-ID"})
        }
        "hyperliquid.orders.cancel_batch" => {
            json!({"request_id":"SYNTHETIC-CANCEL-BATCH-001","account":"SYNTHETIC-ACCOUNT","orders":[{"request_id":"SYNTHETIC-CANCEL-CHILD-001","account":"SYNTHETIC-ACCOUNT","symbol":"BTC/USDC:USDC","order_id":"SYNTHETIC-ORDER-ID"}]})
        }
        "hyperliquid.orders.twap" => {
            json!({"request_id":"SYNTHETIC-TWAP-001","account":"SYNTHETIC-ACCOUNT","symbol":"BTC/USDC:USDC","market_type":"perpetual","side":"buy","size":"0.001","duration_minutes":15,"estimated_notional_usdc":"100","estimated_fee_reserve_usdc":"0.1","estimated_realized_loss_reserve_usdc":"0","estimated_realized_profit_usdc":"0","accept_native_price_risk":true,"randomize":true,"reduce_only":false})
        }
        "hyperliquid.orders.twap_cancel" => {
            json!({"request_id":"SYNTHETIC-TWAP-CANCEL-001","account":"SYNTHETIC-ACCOUNT","symbol":"BTC/USDC:USDC","market_type":"perpetual","twap_id":"123"})
        }
        "hyperliquid.orders.schedule_cancel" => {
            json!({"request_id":"SYNTHETIC-CANCEL-DEADLINE-001","account":"SYNTHETIC-ACCOUNT","after_ms":1893456000000u64})
        }
        "hyperliquid.account.set_leverage" => {
            json!({"request_id":"SYNTHETIC-LEVERAGE-001","account":"SYNTHETIC-ACCOUNT","symbol":"BTC/USDC:USDC","leverage":1,"cross":false})
        }
        "hyperliquid.account.add_margin" | "hyperliquid.account.reduce_margin" => {
            json!({"request_id":"SYNTHETIC-MARGIN-001","account":"SYNTHETIC-ACCOUNT","symbol":"BTC/USDC:USDC","amount":"1","max_amount_usdc":"1","max_fee_usdc":"0"})
        }
        "hyperliquid.account.transfer" => {
            json!({"request_id":"SYNTHETIC-TRANSFER-001","account":"SYNTHETIC-ACCOUNT","asset":"USDC","amount":"1","destination":"address","destination_address":"0x1111111111111111111111111111111111111111","max_amount_usdc":"1","max_fee_usdc":"0.01"})
        }
        "hyperliquid.account.invalidate_nonce" => {
            json!({"request_id":"SYNTHETIC-NONCE-RECOVERY-001","account":"SYNTHETIC-ACCOUNT","original_operation_id":"SYNTHETIC-ORIGINAL-OPERATION-ID"})
        }
        "hyperliquid.account.withdraw" => {
            json!({"request_id":"SYNTHETIC-WITHDRAW-001","account":"SYNTHETIC-ACCOUNT","asset":"USDC","amount":"1","address":"0x1111111111111111111111111111111111111111","max_amount_usdc":"1","max_fee_usdc":"0"})
        }
        "hyperliquid.account.deposit" => {
            json!({"requestId":"SYNTHETIC-DEPOSIT-001","account":"SYNTHETIC-ACCOUNT","wallet":"SYNTHETIC-WALLET","amount":"5","maxFeeEth":"0.001","maxAmountUsdc":"5"})
        }
        "hyperliquid.vault.deposit" | "hyperliquid.vault.withdraw" => {
            json!({"request_id":"SYNTHETIC-VAULT-001","account":"SYNTHETIC-ACCOUNT","vault_address":"0x1111111111111111111111111111111111111111","amount":"1","max_amount_usdc":"1","max_fee_usdc":"0"})
        }
        "hyperliquid.vault.create" => {
            json!({"request_id":"SYNTHETIC-CREATE-VAULT-001","account":"SYNTHETIC-ACCOUNT","name":"Synthetic vault","description":"Synthetic example only","seed_amount_usdc":"1","max_fee_usdc":"0"})
        }
        "hyperliquid.account.reserve_request_weight" => {
            json!({"request_id":"SYNTHETIC-WEIGHT-001","account":"SYNTHETIC-ACCOUNT","weight":1,"destination":"0x2222222222222222222222222222222222222222"})
        }
        "hyperliquid.account.set_abstraction" => {
            json!({"request_id":"SYNTHETIC-ABSTRACTION-001","account":"SYNTHETIC-ACCOUNT","mode":"unified"})
        }
        "hyperliquid.account.set_dex_abstraction" => {
            json!({"request_id":"SYNTHETIC-DEX-ABSTRACTION-001","account":"SYNTHETIC-ACCOUNT","enabled":false})
        }
        "hyperliquid.account.set_agent_abstraction" => {
            json!({"request_id":"SYNTHETIC-AGENT-ABSTRACTION-001","account":"SYNTHETIC-ACCOUNT","mode":"unified"})
        }
        "hyperliquid.referral.set" => {
            json!({"request_id":"SYNTHETIC-REFERRAL-001","account":"SYNTHETIC-ACCOUNT","referral_code":"SYNTHETIC-CODE"})
        }
        "hyperliquid.builder.approve_fee" => {
            json!({"request_id":"SYNTHETIC-BUILDER-FEE-001","account":"SYNTHETIC-ACCOUNT","builder_address":"0x1111111111111111111111111111111111111111","fee_tenths_bps":1})
        }
        "hyperliquid.account.create_subaccount" => {
            json!({"request_id":"SYNTHETIC-SUBACCOUNT-001","account":"SYNTHETIC-ACCOUNT","name":"synthetic-child"})
        }
        _ => return Vec::new(),
    };
    vec![value]
}

fn limit_order_example(request_id: &str) -> Value {
    json!({"request_id":request_id,"account":"SYNTHETIC-ACCOUNT","symbol":"BTC/USDC:USDC","market_type":"perpetual","side":"buy","order_type":"limit","size":"0.001","limit_price":"50000","min_price":"50000","max_price":"50000","time_in_force":"gtc","max_notional_usdc":"100","max_fee_usdc":"0.1","reduce_only":false})
}

fn coverage(name: &str) -> &'static str {
    match name {
        "hyperliquid.markets"
        | "hyperliquid.currencies"
        | "hyperliquid.ticker"
        | "hyperliquid.tickers"
        | "hyperliquid.funding_rate"
        | "hyperliquid.funding_rates"
        | "hyperliquid.open_interest" => {
            "Typed read route; metadata identity and exact decimal fixtures cover parsing. Results are bounded observations, not exhaustive state. Live endpoint verification is pending."
        }
        "hyperliquid.order_book"
        | "hyperliquid.candles"
        | "hyperliquid.trades"
        | "hyperliquid.funding_rate_history"
        | "hyperliquid.time"
        | "hyperliquid.status" => {
            "Typed public REST route with synthetic parsing/filter fixtures. Native response limits apply; live endpoint verification is pending."
        }
        "hyperliquid.balances"
        | "hyperliquid.positions"
        | "hyperliquid.open_orders"
        | "hyperliquid.fees"
        | "hyperliquid.abstraction"
        | "hyperliquid.order"
        | "hyperliquid.closed_orders"
        | "hyperliquid.canceled_orders"
        | "hyperliquid.orders"
        | "hyperliquid.my_trades"
        | "hyperliquid.my_liquidations"
        | "hyperliquid.funding_history"
        | "hyperliquid.ledger"
        | "hyperliquid.deposits"
        | "hyperliquid.withdrawals" => {
            "Owner-scoped typed REST route; balances, positions and open orders accept an optional exact HIP-3 DEX name, while fees and abstraction reject DEX filters. Synthetic owner/filter fixtures apply where listed in the capability guide. Native coverage and pagination limits remain; live account reads are unverified."
        }
        "hyperliquid.orders.twap" => {
            "Synthetic Engine lifecycle covers partial fills, restart, exact-quantity completion, changed receipts and estimate overruns. Native TWAP price/fee/PnL estimates are not venue-enforced; no live-write verification."
        }
        "hyperliquid.orders.edit" | "hyperliquid.orders.edit_batch" => {
            "Synthetic tests cover exact parent identity and replacement binding; live write and settlement verification are pending."
        }
        "hyperliquid.orders.place"
        | "hyperliquid.orders.batch"
        | "hyperliquid.orders.cancel"
        | "hyperliquid.orders.cancel_batch" => {
            "Synthetic order lifecycle tests cover request binding and fill/status reconciliation; live write and approval verification are pending."
        }
        _ if name.starts_with("hyperliquid.") => {
            "Typed fixed action or human-only configuration route; synthetic contract tests only. Runtime availability is determined by the installed broker. Live write and protected approval verification are pending."
        }
        _ => {
            "Typed route; runtime availability is determined by the installed broker. Synthetic and live evidence are listed in the capability guide."
        }
    }
}
fn read<T: JsonSchema>(name: &str, desc: &str, semantics: Value) -> Definition {
    definition(
        name,
        desc,
        (OperationKind::Read, ExecutionActor::AgentReadable),
        schema::<T>(),
        schema::<ReadOutput>(),
        "Returns only after a successful read; any market/account fields are source observations, not proof of exhaustive coverage.",
        semantics,
    )
}
fn write<T: JsonSchema>(
    name: &str,
    desc: &str,
    actor: ExecutionActor,
    semantics: Value,
) -> Definition {
    definition(
        name,
        desc,
        (OperationKind::Write, actor),
        schema::<T>(),
        schema::<WriteOutput>(),
        "A successful response records submission evidence; settlement and reconciliation remain adapter responsibilities.",
        semantics,
    )
}

fn admin<T: JsonSchema>(name: &str, desc: &str, semantics: Value) -> Definition {
    definition(
        name,
        desc,
        (
            OperationKind::Configuration,
            ExecutionActor::HumanInteractive,
        ),
        schema::<T>(),
        schema::<WriteOutput>(),
        "Configuration changes require the protected human approval path; no agent invocation grants authority.",
        semantics,
    )
}

/// All public read definitions and reviewed Hyperliquid write intent schemas.
pub fn definitions() -> Vec<Definition> {
    let mut d = vec![
        read::<MarketsInput>(
            "hyperliquid.markets",
            "Read native perpetual, HIP-3 perpetual and spot market metadata.",
            json!({"market_scopes":["perpetual","hip3_perpetual","spot"],"source":"native Hyperliquid metadata"}),
        ),
        read::<MarketListInput>(
            "hyperliquid.currencies",
            "Read native spot token and currency metadata.",
            json!({"source":"spot metadata"}),
        ),
        read::<MarketInput>(
            "hyperliquid.ticker",
            "Read one market's current ticker snapshot.",
            json!({"decimals":"exact strings"}),
        ),
        read::<MarketListInput>(
            "hyperliquid.tickers",
            "Read ticker snapshots for selected markets.",
            json!({"symbols":"omission means all markets only when adapter explicitly bounds the result"}),
        ),
        read::<MarketInput>(
            "hyperliquid.order_book",
            "Read a bounded native L2 order book.",
            json!({"depth":"venue bounded; exact price and size strings"}),
        ),
        read::<CandleInput>(
            "hyperliquid.candles",
            "Read native candle snapshots for a supported interval and time window.",
            json!({"intervals":["1m","3m","5m","15m","30m","1h","2h","4h","8h","12h","1d","3d","1w","1M"],"max_window":"venue bounded"}),
        ),
        read::<MarketHistoryInput>(
            "hyperliquid.trades",
            "Read recent public trades for a market.",
            json!({"history":"venue bounded"}),
        ),
        read::<MarketInput>(
            "hyperliquid.funding_rate",
            "Read the current funding rate for a perpetual market.",
            json!({"scope":"perpetual markets"}),
        ),
        read::<MarketListInput>(
            "hyperliquid.funding_rates",
            "Read current funding rates across selected perpetual markets.",
            json!({"scope":"perpetual markets"}),
        ),
        read::<HistoryInput>(
            "hyperliquid.funding_history",
            "Read funding payments for an enrolled account, separate from market funding-rate history.",
            json!({"endpoint":"userFunding","timestamps":"milliseconds","coverage":"venue result and limit bound apply"}),
        ),
        read::<MarketHistoryInput>(
            "hyperliquid.funding_rate_history",
            "Read public historical funding rates for a perpetual market.",
            json!({"endpoint":"fundingHistory","timestamps":"milliseconds","coverage":"native market history endpoint"}),
        ),
        read::<MarketInput>(
            "hyperliquid.open_interest",
            "Read current open interest from native perpetual market contexts.",
            json!({"scope":"perpetual markets"}),
        ),
        read::<EmptyInput>(
            "hyperliquid.time",
            "Read server time from the Hyperliquid exchange-status response.",
            json!({"endpoint":"exchangeStatus","unit":"milliseconds since Unix epoch"}),
        ),
        read::<EmptyInput>(
            "hyperliquid.status",
            "Read Hyperliquid REST exchange status.",
            json!({"source":"exchange status endpoint"}),
        ),
        read::<TradingStateInput>(
            "hyperliquid.trading_state",
            "Read the account's current leverage, margin mode and trading capacity for a perpetual market, including when flat.",
            json!({"source":"activeAssetData","account":"exact protected execution address","dex":"optional exact named HIP-3 DEX"}),
        ),
        read::<AccountMarketInput>(
            "hyperliquid.balances",
            "Read primary or named HIP-3 perpetual collateral and account-wide spot balances for an enrolled account.",
            json!({"account":"protected connection alias; never an address supplied in this request","dex":"optional exact name from hyperliquid.markets; omitted selects primary perpetual state, while spot balances remain account-wide"}),
        ),
        read::<AccountMarketInput>(
            "hyperliquid.positions",
            "Read current primary or named HIP-3 perpetual positions for an enrolled account.",
            json!({"position_size":"signed native size string","dex":"optional exact name from hyperliquid.markets; omitted selects primary DEX"}),
        ),
        read::<OrderInput>(
            "hyperliquid.order",
            "Read one order by its venue or client order identifier.",
            json!({"lookup":"one identifier required"}),
        ),
        read::<AccountMarketInput>(
            "hyperliquid.open_orders",
            "Read currently open primary or named HIP-3 perpetual orders for an enrolled account.",
            json!({"coverage":"open-order snapshot","dex":"optional exact name from hyperliquid.markets; omitted selects primary DEX"}),
        ),
        read::<HistoryInput>(
            "hyperliquid.closed_orders",
            "Read closed order history for an enrolled account.",
            json!({"history":"venue bounded"}),
        ),
        read::<HistoryInput>(
            "hyperliquid.canceled_orders",
            "Read canceled order history for an enrolled account.",
            json!({"history":"venue bounded"}),
        ),
        read::<HistoryInput>(
            "hyperliquid.orders",
            "Read bounded historical orders for an enrolled account.",
            json!({"history":"venue bounded; current orders use open_orders"}),
        ),
        read::<HistoryInput>(
            "hyperliquid.my_trades",
            "Read private fills for an enrolled account.",
            json!({"history":"bounded by explicit start time and venue limit"}),
        ),
        read::<HistoryInput>(
            "hyperliquid.my_liquidations",
            "Read an enrolled account's liquidation fills, derived from user fills carrying a matching liquidation.liquidatedUser field.",
            json!({"source":"userFills or userFillsByTime","identity":"liquidation.liquidatedUser must match the enrolled owner","coverage":"CCXT-style derived results; native history limits apply"}),
        ),
        read::<HistoryInput>(
            "hyperliquid.ledger",
            "Read bounded non-funding account ledger updates.",
            json!({"coverage":"venue ledger update categories only"}),
        ),
        read::<HistoryInput>(
            "hyperliquid.deposits",
            "Read deposit records represented in supported account ledger updates.",
            json!({"coverage":"not an exhaustive external-chain deposit index"}),
        ),
        read::<HistoryInput>(
            "hyperliquid.withdrawals",
            "Read withdrawal records represented in supported account ledger updates.",
            json!({"coverage":"not an exhaustive external-chain withdrawal index"}),
        ),
        read::<AccountInput>(
            "hyperliquid.fees",
            "Read account fee tier and available fee metadata.",
            json!({"source":"user fee endpoint"}),
        ),
        read::<AccountInput>(
            "hyperliquid.abstraction",
            "Read account abstraction settings and state.",
            json!({"source":"explicit abstraction endpoints"}),
        ),
    ];
    d.extend([
        write::<LimitOrderInput>("hyperliquid.orders.place", "Place one exact-size limit order.", ExecutionActor::AgentInvocable, json!({"approval":"fresh per-write approval","precision":"native market metadata required"})),
        write::<BatchOrdersInput>("hyperliquid.orders.batch", "Submit a bounded batch of exact-size limit orders.", ExecutionActor::AgentInvocable, json!({"approval":"fresh per-write approval per submitted action","atomicity":"venue dependent; each outcome is reconciled"})),
        write::<EditOrderInput>("hyperliquid.orders.edit", "Edit one order using exact size and limit price.", ExecutionActor::AgentInvocable, json!({"approval":"fresh per-write approval","replacement":{"version":1}})),
        write::<EditOrdersInput>("hyperliquid.orders.edit_batch", "Edit a bounded batch of currently open orders with exact size and limit price.", ExecutionActor::AgentInvocable, json!({"approval":"fresh per-write approval per submitted action","atomicity":"venue dependent; each replacement is reconciled independently","replacement":{"version":1}})),
        write::<CancelOrderInput>("hyperliquid.orders.cancel", "Cancel one identified order.", ExecutionActor::AgentInvocable, json!({"approval":"fresh per-write approval"})),
        write::<CancelOrdersInput>("hyperliquid.orders.cancel_batch", "Cancel a bounded set of identified orders.", ExecutionActor::AgentInvocable, json!({"approval":"fresh per-write approval"})),
        write::<TwapInput>("hyperliquid.orders.twap", "Schedule one native perpetual TWAP with explicit turnover, fee, realized-loss and realized-profit estimates and native price-risk acknowledgement.", ExecutionActor::AgentInvocable, json!({"approval":"fresh per-write approval","durability":"scheduled venue-side execution continues after acceptance","notional":"estimate/reservation only; not an absolute quote-price ceiling","native_price_rule":"relative slice pricing; no absolute lifetime quote-price bound","fees":"estimate/reservation only; not a venue-enforced maximum","realized_loss":"gross per-slice losses are bounded cumulatively against the explicit estimate; an overrun remains unresolved","realized_profit":"gross per-slice profits are bounded against the explicit estimate; an overrun remains unresolved","estimates":"GoFin reconciliation limits only; none are guaranteed by the native venue schedule"})),
        write::<ScheduleCancelInput>("hyperliquid.orders.schedule_cancel", "Set or update the account's venue-side cancel-after deadline.", ExecutionActor::AgentInvocable, json!({"approval":"fresh per-write approval","unit":"milliseconds"})),
        write::<LeverageInput>("hyperliquid.account.set_leverage", "Set leverage for one perpetual market.", ExecutionActor::AgentInvocable, json!({"approval":"fresh per-write approval"})),
        write::<MarginInput>("hyperliquid.account.add_margin", "Add exact margin to an isolated perpetual position.", ExecutionActor::AgentInvocable, json!({"approval":"fresh per-write approval"})),
        write::<MarginInput>("hyperliquid.account.reduce_margin", "Remove exact margin from an isolated perpetual position.", ExecutionActor::AgentInvocable, json!({"approval":"fresh per-write approval"})),
        write::<TransferInput>("hyperliquid.account.transfer", "Transfer USDC between spot/perpetual custody or send USDC to an existing Hyperliquid user or subaccount.", ExecutionActor::AgentInvocable, json!({"approval":"fresh per-write approval","destination":"closed enum","address_send":{"destination_address":"exact address bound in approval","source":"perpetual USDC only","recipient":"existing active user or subaccount only","settlement":"unique nonce-matched native action and same-hash sender/recipient ledger rows required","account_activation":"not supported; any correlated activation fee leaves the operation unresolved","vaults":"use the dedicated vault deposit operation"}})),
        write::<InvalidateNonceInput>("hyperliquid.account.invalidate_nonce", "Consume the exact native nonce of one unresolved same-owner spot/perpetual USDC class transfer.", ExecutionActor::AgentInvocable, json!({"approval":"fresh per-write approval","recovery_child":{"version":1},"recovery":{"version":1,"zero_financial_effect":true},"scope":"owner-signed USDC class transfers only; vault, subaccount, API-wallet, order and other action nonces are unsupported","nonce":"loaded from the immutable original GoFin submission; caller cannot supply a nonce","proof":"only exact successful Noop acknowledgement completes this child and proves the original class transfer was not accepted","failure":"native rejection, timeout or lost response keeps the parent unresolved and blocks another Noop; only terminal confirmed non-dispatch without any submission, effects or dispatch evidence permits a fresh approved child; never replay the parent"})),
        write::<WithdrawalInput>("hyperliquid.account.withdraw", "Withdraw an exact amount to the approved address.", ExecutionActor::AgentInvocable, json!({"approval":"fresh per-write approval","destination":"exact address bound in approval"})),
        write::<DepositInput>("hyperliquid.account.deposit", "Deposit native USDC from the enrolled wallet through the legacy Hyperliquid L1 funding route.", ExecutionActor::AgentInvocable, json!({"approval":"fresh per-write approval","asset":"native USDC only","source_chain_id":42161,"cctp":"deferred","receipt":"must be independently verified before settlement"})),
        write::<VaultActionInput>("hyperliquid.vault.deposit", "Deposit the specified amount into a selected vault.", ExecutionActor::AgentInvocable, json!({"approval":"fresh per-write approval"})),
        write::<VaultActionInput>("hyperliquid.vault.withdraw", "Withdraw from a selected vault.", ExecutionActor::AgentInvocable, json!({"approval":"fresh per-write approval"})),
        admin::<CreateVaultInput>("hyperliquid.vault.create", "Create a vault with an explicitly bounded seed contribution.", json!({"approval":"human-only administrative authorization","seed_amount":"exact USD string"})),
        write::<TwapCancelInput>("hyperliquid.orders.twap_cancel", "Cancel one venue-side perpetual TWAP by its exact identifier and market.", ExecutionActor::AgentInvocable, json!({"approval":"fresh per-write approval","effect":"stops remaining schedule only; completed slices remain executed"})),
        write::<ReserveWeightInput>("hyperliquid.account.reserve_request_weight", "Reserve bounded request-weight capacity for a named account action.", ExecutionActor::AgentInvocable, json!({"approval":"no financial movement; rate-limit reservation only","destination":"optional existing Hyperliquid user; exact userRole is verified before dispatch"})),
        admin::<AbstractionInput>("hyperliquid.account.set_abstraction", "Change account abstraction settings.", json!({"approval":"human-only administrative authorization"})),
        admin::<DexAbstractionInput>("hyperliquid.account.set_dex_abstraction", "Change DEX abstraction settings.", json!({"approval":"human-only administrative authorization"})),
        admin::<AbstractionInput>("hyperliquid.account.set_agent_abstraction", "Change agent abstraction settings.", json!({"approval":"human-only administrative authorization"})),
        admin::<ReferralInput>("hyperliquid.referral.set", "Set a referral relationship for an account.", json!({"approval":"human-only administrative authorization"})),
        admin::<BuilderFeeInput>("hyperliquid.builder.approve_fee", "Approve a named builder fee recipient and exact maximum rate.", json!({"approval":"human-only administrative authorization","fee_unit":"tenths of one basis point"})),
        admin::<SubaccountInput>("hyperliquid.account.create_subaccount", "Create a named subaccount.", json!({"approval":"human-only administrative authorization"})),
    ]);
    for definition in &mut d {
        if matches!(
            definition.name.as_str(),
            "hyperliquid.orders.place"
                | "hyperliquid.orders.batch"
                | "hyperliquid.orders.edit"
                | "hyperliquid.orders.edit_batch"
        ) {
            definition.semantics["plan_immediate_orders"] = json!(true);
            definition.semantics["plan_funding_accounting"] = json!({
                "spot": "Uses the exact input-asset principal debit and separately classified costs.",
                "perpetual": "Reserves margin separately from notional policy valuation; opening exposure does not spend notional as cash principal.",
                "dependencies": "Declare funding only for actual principal or additional costs, not ordering or margin capacity. A caller-supplied maximum_debits ceiling does not establish consumption. Ordered steps still wait for verified predecessor completion."
            });
        }
        if definition.kind == OperationKind::Write
            && !matches!(
                definition.name.as_str(),
                "hyperliquid.orders.place"
                    | "hyperliquid.orders.batch"
                    | "hyperliquid.orders.edit"
                    | "hyperliquid.orders.edit_batch"
                    | "hyperliquid.orders.schedule_cancel"
                    | "hyperliquid.orders.twap"
            )
        {
            definition.semantics["plan_allowance_seconds"] =
                json!(if definition.name.ends_with(".withdraw") {
                    gofin_domain::plan_timing::WITHDRAWAL_ALLOWANCE_SECONDS
                } else {
                    gofin_domain::plan_timing::ONCHAIN_ALLOWANCE_SECONDS
                });
        }
    }
    d
}

#[derive(Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmptyInput {}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_accepts_example(schema: &Value, example: &Value) -> bool {
        if schema.get("type").and_then(Value::as_str) == Some("object") {
            let Some(object) = example.as_object() else {
                return false;
            };
            if schema
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|required| {
                    required
                        .iter()
                        .any(|key| key.as_str().is_none_or(|key| !object.contains_key(key)))
                })
            {
                return false;
            }
            let properties = schema.get("properties").and_then(Value::as_object);
            if schema.get("additionalProperties") == Some(&Value::Bool(false))
                && properties.is_some_and(|properties| {
                    object.keys().any(|key| !properties.contains_key(key))
                })
            {
                return false;
            }
            if let Some(properties) = properties {
                for (key, value) in object {
                    if let Some(child) = properties.get(key)
                        && !schema_accepts_example(child, value)
                    {
                        return false;
                    }
                }
            }
        }
        if schema.get("type").and_then(Value::as_str) == Some("array") {
            let Some(items) = example.as_array() else {
                return false;
            };
            if let Some(item_schema) = schema.get("items")
                && items
                    .iter()
                    .any(|item| !schema_accepts_example(item_schema, item))
            {
                return false;
            }
        }
        true
    }

    #[test]
    fn definitions_are_unique_typed_and_separate_human_admin_actions() {
        let defs = definitions();
        let mut names = std::collections::BTreeSet::new();
        for d in &defs {
            assert!(names.insert(d.name.clone()), "duplicate {}", d.name);
            assert!(d.input_schema.is_object());
            assert!(d.output_schema.is_object());
            assert!(d.error_schema.is_object());
            assert!(!d.examples.is_empty(), "missing example for {}", d.name);
            for example in &d.examples {
                assert!(
                    schema_accepts_example(&d.input_schema, example),
                    "example does not satisfy input schema for {}: {example}",
                    d.name
                );
            }
            fn schema_is_closed(value: &Value) -> bool {
                match value {
                    Value::Object(map) => {
                        map.iter().all(|(key, child)| {
                            key != "additionalProperties" || child == &Value::Bool(false)
                        }) && map.values().all(schema_is_closed)
                    }
                    Value::Array(items) => items.iter().all(schema_is_closed),
                    _ => true,
                }
            }
            assert!(
                schema_is_closed(&d.input_schema),
                "open schema for {}",
                d.name
            );
        }
        let admin = defs
            .iter()
            .find(|d| d.name == "hyperliquid.account.set_abstraction")
            .unwrap();
        assert_eq!(admin.execution_actor, ExecutionActor::HumanInteractive);
        assert_eq!(admin.kind, OperationKind::Configuration);
        assert_eq!(
            defs.iter()
                .filter(|d| d.kind == OperationKind::Read)
                .count(),
            29
        );
        assert!(
            defs.iter()
                .filter(|d| d.kind == OperationKind::Read)
                .all(|d| d.execution_actor == ExecutionActor::AgentReadable)
        );
        let account_funding = defs
            .iter()
            .find(|d| d.name == "hyperliquid.funding_history")
            .unwrap();
        assert!(
            account_funding.input_schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "account")
        );
        let market_funding = defs
            .iter()
            .find(|d| d.name == "hyperliquid.funding_rate_history")
            .unwrap();
        assert!(
            market_funding.input_schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "symbol")
        );
        assert!(
            !market_funding.input_schema["properties"]
                .as_object()
                .unwrap()
                .contains_key("account")
        );
        assert!(defs.iter().any(|d| d.name == "hyperliquid.account.deposit"));
        let weight = defs
            .iter()
            .find(|definition| definition.name == "hyperliquid.account.reserve_request_weight")
            .unwrap();
        assert_eq!(
            weight.examples[0]["destination"],
            "0x2222222222222222222222222222222222222222"
        );
    }

    #[test]
    fn only_market_scoped_account_reads_expose_optional_hip3_dex() {
        let defs = definitions();
        for name in [
            "hyperliquid.balances",
            "hyperliquid.positions",
            "hyperliquid.open_orders",
        ] {
            let def = defs
                .iter()
                .find(|definition| definition.name == name)
                .unwrap();
            assert!(def.input_schema["properties"].get("dex").is_some());
            assert!(
                def.examples
                    .iter()
                    .any(|example| example.get("dex").is_some())
            );
        }
        for name in ["hyperliquid.fees", "hyperliquid.abstraction"] {
            let def = defs
                .iter()
                .find(|definition| definition.name == name)
                .unwrap();
            assert!(def.input_schema["properties"].get("dex").is_none());
        }
    }
}
