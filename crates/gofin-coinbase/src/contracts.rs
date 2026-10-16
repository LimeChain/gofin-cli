//! Closed Fin operation names and schemas for Coinbase REST products.
//! The full pinned-client inventory and the relation between native methods,
//! Fin operations, and endpoint evidence is maintained in docs/coinbase.
use gofin_domain::{Definition, ExecutionActor, OperationKind};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Product {
    Advanced,
    Exchange,
    International,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Read,
    Write,
    Configure,
}

#[derive(Clone, Copy, Debug)]
pub struct Contract {
    pub operation: &'static str,
    pub product: Product,
    pub action: Action,
    pub description: &'static str,
    pub route: &'static str,
    pub schema: fn() -> Value,
}

fn read_schema(required: &[&str], optional: &[(&str, Value)]) -> Value {
    let mut properties = serde_json::Map::new();
    for name in required {
        properties.insert((*name).into(), input_field_schema(name));
    }
    for (name, schema) in optional {
        properties.insert((*name).into(), schema.clone());
    }
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}
fn input_field_schema(name: &str) -> Value {
    match name {
        "limit" => json!({"type":"integer","minimum":1,"maximum":250}),
        "cursor" => json!({"type":"string","maxLength":1024}),
        _ => json!({"type":"string","minLength":1,"maxLength":256}),
    }
}
fn account_list() -> Value {
    read_schema(
        &["account"],
        &[
            ("cursor", json!({"type":"string"})),
            ("limit", json!({"type":"integer","minimum":1,"maximum":250})),
        ],
    )
}
fn product_list() -> Value {
    read_schema(
        &[],
        &[
            ("account", json!({"type":"string","minLength":1})),
            ("cursor", json!({"type":"string"})),
            ("limit", json!({"type":"integer","minimum":1,"maximum":250})),
            ("productId", json!({"type":"string"})),
        ],
    )
}
fn product_get() -> Value {
    read_schema(
        &["id"],
        &[("account", json!({"type":"string","minLength":1}))],
    )
}
fn product_book() -> Value {
    product_get()
}
fn account_get() -> Value {
    read_schema(&["account", "accountId"], &[])
}
fn portfolio_get() -> Value {
    read_schema(&["account", "portfolioId"], &[])
}
fn id_get() -> Value {
    read_schema(&["account", "id"], &[])
}
fn fills() -> Value {
    read_schema(
        &["account"],
        &[
            ("cursor", json!({"type":"string"})),
            ("limit", json!({"type":"integer","minimum":1,"maximum":250})),
            ("productId", json!({"type":"string"})),
            ("id", json!({"type":"string"})),
        ],
    )
}
fn simple() -> Value {
    read_schema(&["account"], &[])
}
fn transaction_get() -> Value {
    read_schema(&["account", "accountId", "transactionId"], &[])
}
fn exchange_transfers() -> Value {
    read_schema(
        &["account", "accountId"],
        &[
            ("limit", json!({"type":"integer","minimum":1,"maximum":250})),
            ("startingAfter", json!({"type":"string"})),
            ("endingBefore", json!({"type":"string"})),
        ],
    )
}
fn decimal_schema() -> Value {
    json!({"type":"string","pattern":"^(0|[1-9][0-9]*)(\\.[0-9]+)?$","maxLength":80})
}
fn order_configuration() -> Value {
    let decimal = decimal_schema();
    let market = json!({"type":"object","properties":{"base_size":decimal,"quote_size":decimal},"minProperties":1,"maxProperties":1,"additionalProperties":false});
    let sized_limit = json!({"type":"object","required":["limit_price"],"properties":{"base_size":decimal,"quote_size":decimal,"limit_price":decimal,"post_only":{"type":"boolean"}},"minProperties":2,"additionalProperties":false});
    let timed_limit = json!({"type":"object","required":["limit_price","end_time"],"properties":{"base_size":decimal,"quote_size":decimal,"limit_price":decimal,"end_time":{"type":"string","minLength":20,"maxLength":40},"post_only":{"type":"boolean"}},"minProperties":3,"additionalProperties":false});
    let stop_direction =
        json!({"type":"string","enum":["STOP_DIRECTION_STOP_UP","STOP_DIRECTION_STOP_DOWN"]});
    let stop_limit = json!({"type":"object","required":["base_size","limit_price","stop_price","stop_direction"],"properties":{"base_size":decimal,"limit_price":decimal,"stop_price":decimal,"stop_direction":stop_direction,"end_time":{"type":"string","minLength":20,"maxLength":40}},"additionalProperties":false});
    let stop_limit_gtd = json!({"type":"object","required":["base_size","limit_price","stop_price","stop_direction","end_time"],"properties":{"base_size":decimal,"limit_price":decimal,"stop_price":decimal,"stop_direction":stop_direction,"end_time":{"type":"string","minLength":20,"maxLength":40}},"additionalProperties":false});
    json!({"oneOf":[
        {"type":"object","required":["market_market_ioc"],"properties":{"market_market_ioc":market},"additionalProperties":false},
        {"type":"object","required":["sor_limit_ioc"],"properties":{"sor_limit_ioc":sized_limit},"additionalProperties":false},
        {"type":"object","required":["limit_limit_gtc"],"properties":{"limit_limit_gtc":sized_limit},"additionalProperties":false},
        {"type":"object","required":["limit_limit_gtd"],"properties":{"limit_limit_gtd":timed_limit},"additionalProperties":false},
        {"type":"object","required":["limit_limit_fok"],"properties":{"limit_limit_fok":sized_limit},"additionalProperties":false},
        {"type":"object","required":["stop_limit_stop_limit_gtc"],"properties":{"stop_limit_stop_limit_gtc":stop_limit},"additionalProperties":false},
        {"type":"object","required":["stop_limit_stop_limit_gtd"],"properties":{"stop_limit_stop_limit_gtd":stop_limit_gtd},"additionalProperties":false}
    ]})
}
fn create_order() -> Value {
    json!({"type":"object","required":["account","requestId","productId","side","orderConfiguration","maximumSpend","minimumReceive","maximumFee"],"properties":{
    "account":{"type":"string","minLength":1},"requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},"productId":{"type":"string","minLength":1,"maxLength":128},"side":{"type":"string","enum":["BUY","SELL"]},"orderConfiguration":order_configuration(),"maximumSpend":decimal_schema(),"minimumReceive":decimal_schema(),"maximumFee":decimal_schema()
},"additionalProperties":false})
}
fn cancel_order() -> Value {
    json!({"type":"object","required":["account","requestId","venueOrderId","clientOrderId","productId"],"properties":{
        "account":{"type":"string","minLength":1},"requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},"venueOrderId":{"type":"string","minLength":1,"maxLength":256},"clientOrderId":{"type":"string","minLength":1,"maxLength":128},"productId":{"type":"string","minLength":1,"maxLength":128}
    },"additionalProperties":false})
}
fn cancel_orders() -> Value {
    let text = |max| json!({"type":"string","minLength":1,"maxLength":max});
    json!({"type":"object","required":["account","requestId","orders"],"properties":{
        "account":{"type":"string","minLength":1,"maxLength":128},
        "requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},
        "orders":{"type":"array","minItems":1,"maxItems":100,"items":{"type":"object","required":["operationId","venueOrderId","clientOrderId","productId"],"properties":{
            "operationId":text(256),"venueOrderId":text(256),"clientOrderId":text(128),"productId":text(128)
        },"additionalProperties":false}},
    },"additionalProperties":false})
}
fn edit_order() -> Value {
    json!({"type":"object","required":["account","requestId","operationId","venueOrderId","clientOrderId","productId","newPrice"],"properties":{
        "account":{"type":"string","minLength":1},"requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},
        "operationId":{"type":"string","minLength":1,"maxLength":256},"venueOrderId":{"type":"string","minLength":1,"maxLength":256},
        "clientOrderId":{"type":"string","minLength":1,"maxLength":128},"productId":{"type":"string","minLength":1,"maxLength":128},"newPrice":decimal_schema()
    },"additionalProperties":false})
}
fn convert() -> Value {
    json!({"type":"object","required":["account","requestId","fromCurrency","fromAccountId","toCurrency","toAccountId","amount","maximumSpend","minimumReceive","maximumFee"],"properties":{
        "account":{"type":"string","minLength":1},"requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},
        "fromCurrency":{"type":"string","minLength":1,"maxLength":128},"fromAccountId":{"type":"string","minLength":1,"maxLength":128},
        "toCurrency":{"type":"string","minLength":1,"maxLength":128},"toAccountId":{"type":"string","minLength":1,"maxLength":128},
        "amount":decimal_schema(),"maximumSpend":decimal_schema(),"minimumReceive":decimal_schema(),"maximumFee":decimal_schema()
    },"additionalProperties":false})
}
fn move_portfolio_funds() -> Value {
    json!({"type":"object","required":["account","requestId","targetAccount","currency","sourceCurrencyAccountId","targetCurrencyAccountId","amount","maximumAmount","minimumAmount"],"properties":{
        "account":{"type":"string","minLength":1},"requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},
        "targetAccount":{"type":"string","minLength":1},"currency":{"type":"string","minLength":1,"maxLength":128},
        "sourceCurrencyAccountId":{"type":"string","minLength":1,"maxLength":128},"targetCurrencyAccountId":{"type":"string","minLength":1,"maxLength":128},
        "amount":decimal_schema(),"maximumAmount":decimal_schema(),"minimumAmount":decimal_schema()
    },"additionalProperties":false})
}
fn withdraw() -> Value {
    json!({"type":"object","required":["account","accountId","destination","address","network","amount","feeMode","feeReserveUsdc","requestId"],"properties":{
        "account":{"type":"string","minLength":1,"maxLength":128},
        "accountId":{"type":"string","minLength":1,"maxLength":128},
        "destination":{"type":"string","minLength":1,"maxLength":128},
        "address":{"type":"string","minLength":1,"maxLength":256},
        "network":{"type":"string","enum":["base"]},
        "amount":decimal_schema(),
        "feeMode":{"type":"string","const":"venue_calculated","enum":["venue_calculated"],"description":"Explicitly consent to Coinbase-calculated network fees; the actual fee is known after processing."},
        "feeReserveUsdc":{"description":"Positive finite accounting reservation and attention threshold, not a fee estimate or Coinbase-enforced maximum.","allOf":[decimal_schema()]},
        "travelRuleData":{"description":"Optional Coinbase Travel Rule data for the enrolled self-hosted wallet. Supply exact beneficiary name and ISO 3166-1 alpha-2 country when required by your Coinbase entity.","type":"object","required":["beneficiary_wallet_type","is_self","beneficiary_name","beneficiary_address"],"properties":{"beneficiary_wallet_type":{"type":"string","const":"WALLET_TYPE_SELF_HOSTED"},"is_self":{"type":"string","const":"IS_SELF_TRUE"},"beneficiary_name":{"type":"string","minLength":1,"maxLength":200,"pattern":r"^[^\x00-\x1F\x7F-\x9F]+$","allOf":[{"pattern":r"\S"}]},"beneficiary_address":{"type":"object","required":["country"],"properties":{"country":{"type":"string","pattern":"^[A-Z]{2}$"}},"additionalProperties":false}},"additionalProperties":false},
        "requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"}
    },"additionalProperties":false})
}
fn create_deposit_address() -> Value {
    json!({"type":"object","required":["account","accountId","requestId"],"properties":{
        "account":{"type":"string","minLength":1},"accountId":{"type":"string","minLength":1,"maxLength":128},
        "requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"}
    },"additionalProperties":false})
}
fn create_portfolio() -> Value {
    json!({"type":"object","required":["account","requestId","name"],"properties":{
        "account":{"type":"string","minLength":1,"maxLength":128},
        "requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},
        "name":{"type":"string","minLength":1,"maxLength":100}
    },"additionalProperties":false})
}
fn rename_portfolio() -> Value {
    json!({"type":"object","required":["account","requestId","newName"],"properties":{
        "account":{"type":"string","minLength":1,"maxLength":128},
        "requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},
        "newName":{"type":"string","minLength":1,"maxLength":100}
    },"additionalProperties":false})
}
fn delete_portfolio() -> Value {
    json!({"type":"object","required":["account","requestId","portfolioId"],"properties":{
        "account":{"type":"string","minLength":1,"maxLength":128},
        "requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},
        "portfolioId":{"type":"string","minLength":1,"maxLength":128}
    },"additionalProperties":false})
}
fn cfm_margin_setting() -> Value {
    json!({"type":"object","required":["account","requestId","setting"],"properties":{
        "account":{"type":"string","minLength":1,"maxLength":128},
        "requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},
        "setting":{"type":"string","enum":["INTRADAY_MARGIN_SETTING_STANDARD","INTRADAY_MARGIN_SETTING_INTRADAY"]}
    },"additionalProperties":false})
}

pub const CONTRACTS: &[Contract] = &[
    Contract {
        operation: "coinbase.get-key-permissions",
        product: Product::Advanced,
        action: Action::Read,
        description: "Read the permissions bound to the enrolled CDP key.",
        route: "key_permissions",
        schema: simple,
    },
    Contract {
        operation: "coinbase.get-accounts",
        product: Product::Advanced,
        action: Action::Read,
        description: "List accounts visible to the key's permissioned portfolio.",
        route: "accounts",
        schema: account_list,
    },
    Contract {
        operation: "coinbase.get-account",
        product: Product::Advanced,
        action: Action::Read,
        description: "Read one exact currency account identity and balance.",
        route: "account",
        schema: account_get,
    },
    Contract {
        operation: "coinbase.get-portfolios",
        product: Product::Advanced,
        action: Action::Read,
        description: "Read portfolios visible to the key.",
        route: "portfolios",
        schema: simple,
    },
    Contract {
        operation: "coinbase.get-portfolio-accounts",
        product: Product::Advanced,
        action: Action::Read,
        description: "Read accounts in one exact permissioned portfolio.",
        route: "portfolio_accounts",
        schema: portfolio_get,
    },
    Contract {
        operation: "coinbase.get-products",
        product: Product::Advanced,
        action: Action::Read,
        description: "Read spot and derivative product metadata.",
        route: "products",
        schema: product_list,
    },
    Contract {
        operation: "coinbase.get-product",
        product: Product::Advanced,
        action: Action::Read,
        description: "Read metadata for one exact product ID.",
        route: "product",
        schema: product_get,
    },
    Contract {
        operation: "coinbase.get-product-book",
        product: Product::Advanced,
        action: Action::Read,
        description: "Read the product order book without converting decimal strings to binary floats.",
        route: "product_book",
        schema: product_book,
    },
    Contract {
        operation: "coinbase.get-order",
        product: Product::Advanced,
        action: Action::Read,
        description: "Read one exact order by venue order ID.",
        route: "order",
        schema: id_get,
    },
    Contract {
        operation: "coinbase.get-fills",
        product: Product::Advanced,
        action: Action::Read,
        description: "Read independently recorded fills and fees.",
        route: "fills",
        schema: fills,
    },
    Contract {
        operation: "coinbase.get-fees",
        product: Product::Advanced,
        action: Action::Read,
        description: "Read transaction fee summary.",
        route: "fees",
        schema: simple,
    },
    Contract {
        operation: "coinbase.get-deposit-addresses",
        product: Product::Advanced,
        action: Action::Read,
        description: "Read deposit addresses for one exact currency account.",
        route: "deposit_addresses",
        schema: account_get,
    },
    Contract {
        operation: "coinbase.create-deposit-address",
        product: Product::Advanced,
        action: Action::Configure,
        description: "Create a new receive address for one exact crypto currency account.",
        route: "deposit_address_create",
        schema: create_deposit_address,
    },
    Contract {
        operation: "coinbase.create-portfolio",
        product: Product::Advanced,
        action: Action::Configure,
        description: "Create a Coinbase Advanced portfolio using the key-bound portfolio scope.",
        route: "portfolio_create",
        schema: create_portfolio,
    },
    Contract {
        operation: "coinbase.rename-portfolio",
        product: Product::Advanced,
        action: Action::Configure,
        description: "Rename only the enrolled Coinbase Advanced portfolio bound to this CDP key.",
        route: "portfolio_edit",
        schema: rename_portfolio,
    },
    Contract {
        operation: "coinbase.delete-portfolio",
        product: Product::Advanced,
        action: Action::Configure,
        description: "Delete only the empty nondefault Coinbase Advanced portfolio bound to this key after native balance, active-order and Fin-journal checks.",
        route: "portfolio_delete",
        schema: delete_portfolio,
    },
    Contract {
        operation: "coinbase.set-cfm-margin-setting",
        product: Product::Advanced,
        action: Action::Configure,
        description: "Set the enrolled Advanced account's US derivatives intraday margin mode and verify it by native readback.",
        route: "cfm_margin_setting",
        schema: cfm_margin_setting,
    },
    Contract {
        operation: "coinbase.get-transactions",
        product: Product::Advanced,
        action: Action::Read,
        description: "Read funding transaction history for one exact currency account.",
        route: "transactions",
        schema: account_get,
    },
    Contract {
        operation: "coinbase.get-transaction",
        product: Product::Advanced,
        action: Action::Read,
        description: "Read one funding transaction by account and transaction identity.",
        route: "transaction",
        schema: transaction_get,
    },
    Contract {
        operation: "coinbase.preview-order",
        product: Product::Advanced,
        action: Action::Read,
        description: "Preview a typed Advanced Trade order without dispatching a trade.",
        route: "order_preview",
        schema: create_order,
    },
    Contract {
        operation: "coinbase.create-order",
        product: Product::Advanced,
        action: Action::Write,
        description: "Create one spot order after exact preview and financial authorization.",
        route: "create_order",
        schema: create_order,
    },
    Contract {
        operation: "coinbase.cancel-order",
        product: Product::Advanced,
        action: Action::Write,
        description: "Cancel the open remainder of one verified order; fills remain owned by the original operation.",
        route: "cancel_order",
        schema: cancel_order,
    },
    Contract {
        operation: "coinbase.cancel-orders",
        product: Product::Advanced,
        action: Action::Write,
        description: "Batch-cancel a bounded set of individually journal-linked Advanced orders.",
        route: "orders_batch_cancel",
        schema: cancel_orders,
    },
    Contract {
        operation: "coinbase.edit-order",
        product: Product::Advanced,
        action: Action::Write,
        description: "Edit the price of one journal-linked open Advanced limit GTC order.",
        route: "order_edit_preview+order_edit",
        schema: edit_order,
    },
    Contract {
        operation: "coinbase.convert",
        product: Product::Advanced,
        action: Action::Write,
        description: "Convert between two verified cash accounts using a bounded Coinbase quote and reconciled trade status.",
        route: "convert",
        schema: convert,
    },
    Contract {
        operation: "coinbase.move-portfolio-funds",
        product: Product::Advanced,
        action: Action::Write,
        description: "Move one exact asset amount between two verified Coinbase Advanced portfolios.",
        route: "move_portfolio_funds",
        schema: move_portfolio_funds,
    },
    Contract {
        operation: "coinbase.withdraw",
        product: Product::Advanced,
        action: Action::Write,
        description: "Send an exact USDC amount from an enrolled Coinbase Advanced account to an enrolled Base wallet with explicit consent to Coinbase-calculated fees and a finite fee accounting reserve.",
        route: "transaction_send",
        schema: withdraw,
    },
    Contract {
        operation: "coinbaseexchange.get-accounts",
        product: Product::Exchange,
        action: Action::Read,
        description: "List institutional Coinbase Exchange accounts.",
        route: "exchange_accounts",
        schema: simple,
    },
    Contract {
        operation: "coinbaseexchange.get-account",
        product: Product::Exchange,
        action: Action::Read,
        description: "Read one exact institutional account.",
        route: "exchange_account",
        schema: account_get,
    },
    Contract {
        operation: "coinbaseexchange.get-order",
        product: Product::Exchange,
        action: Action::Read,
        description: "Read one exact institutional order.",
        route: "exchange_order",
        schema: id_get,
    },
    Contract {
        operation: "coinbaseexchange.get-fills",
        product: Product::Exchange,
        action: Action::Read,
        description: "Read institutional fills.",
        route: "exchange_fills",
        schema: simple,
    },
    Contract {
        operation: "coinbaseexchange.get-fees",
        product: Product::Exchange,
        action: Action::Read,
        description: "Read institutional fee tiers.",
        route: "exchange_fees",
        schema: simple,
    },
    Contract {
        operation: "coinbaseexchange.get-transfers",
        product: Product::Exchange,
        action: Action::Read,
        description: "Read institutional transfer history, including deposits and withdrawals.",
        route: "exchange_transfers",
        schema: exchange_transfers,
    },
    Contract {
        operation: "coinbaseinternational.get-portfolios",
        product: Product::International,
        action: Action::Read,
        description: "Read International portfolios and account identity.",
        route: "international_accounts",
        schema: simple,
    },
    Contract {
        operation: "coinbaseinternational.get-order",
        product: Product::International,
        action: Action::Read,
        description: "Read one exact International spot or derivatives order.",
        route: "international_order",
        schema: id_get,
    },
    Contract {
        operation: "coinbaseinternational.get-fills",
        product: Product::International,
        action: Action::Read,
        description: "Read International spot, margin and perpetual fills.",
        route: "international_fills",
        schema: simple,
    },
];

pub fn contract(operation: &str) -> Option<&'static Contract> {
    CONTRACTS.iter().find(|entry| entry.operation == operation)
}

pub fn definitions() -> Vec<Definition> {
    let mut definitions: Vec<Definition> = CONTRACTS.iter().map(|item| Definition {
        name:item.operation.into(), description:item.description.into(), kind:match item.action {Action::Read=>OperationKind::Read,Action::Write=>OperationKind::Write,Action::Configure=>OperationKind::Configuration},
        execution_actor:match item.action {Action::Read=>ExecutionActor::AgentReadable,Action::Write=>ExecutionActor::AgentInvocable,Action::Configure=>ExecutionActor::HumanInteractive}, input_schema:(item.schema)(),
        output_schema:json!({"type":"object"}), error_schema:json!({"type":"object"}),
        examples: if item.operation == "coinbase.withdraw" { vec![json!({"account":"coinbase","accountId":"00000000-0000-4000-8000-000000000001","destination":"evm-dev","address":"0x0000000000000000000000000000000000000001","network":"base","amount":"1","feeMode":"venue_calculated","feeReserveUsdc":"0.1","requestId":"withdraw-1"})] } else { vec![] },
        coverage:if item.operation == "coinbase.withdraw" { "Coinbase App v2 send; enrolled Base native USDC destination proof and original transaction reconciliation.".into() } else { "Coinbase native REST endpoint; exact wire response preserved.".into() },
        completion:if item.operation == "coinbase.withdraw" { "Submission acceptance is not settlement. Completion requires the original completed native transaction, actual Coinbase fee and signed source debit, plus an independent receipt for exact principal on Base.".into() } else { "A successful HTTP response proves an observation only; transaction acceptance is not settlement.".into() },
        semantics: if item.operation == "coinbase.withdraw" {
            json!({"product":"Advanced","nativeRoute":"transaction_send","clientVersion":"ccxt 4.5.85","feeConsentScopes":["standalone_exact","named_plan_exact"],"feePolicy":{"mode":"venue_calculated","reserveIsFeeCap":false,"actualFeeRecorded":true},"approvedFields":["source account","destination alias and exact address","network","principal","fee consent","fee accounting reserve"],"nativeIdempotency":"broker-generated UUIDv4, persisted before dispatch and reused for reconciliation only","acceptanceNotSettlement":true})
        } else if item.operation == "coinbase.create-order" {
            json!({"product":format!("{:?}",item.product),"nativeRoute":item.route,"clientVersion":"ccxt 4.5.85","broker_generated_attempt_input_fields":["/requestId"]})
        } else {
            json!({"product":format!("{:?}",item.product),"nativeRoute":item.route,"clientVersion":"ccxt 4.5.85"})
        },
    }).collect();
    definitions.extend(crate::reads::definitions());
    definitions.extend(crate::institutional::definitions());
    definitions
}
