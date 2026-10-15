//! Institutional Coinbase Exchange and International operations.
//!
//! This module deliberately keeps product-specific identity, request filters,
//! and native response evidence separate from Advanced Trade. The initial
//! handlers here are authenticated reads; financial writes are admitted only
//! after a typed prepare/dispatch/reconcile lifecycle is implemented.
use crate::institutional_administration as administration;
use crate::{CoinbaseAccount, CoinbaseDispatch, CoinbaseProduct, CoinbaseProfile, contracts};
use gofin_domain::{
    AccountingLocation, Amount, Check, CheckStatus, CostBound, CostTreatment, Definition,
    EffectConstraint, ExecutionActor, FinancialEffect, FinancialIntent, FinancialState, GoFinError,
    OperationKind, OperationRecord, PreparedOperation, Quantity, Reconciliation, ResourceAmount,
    Result, SettlementProgress, SnapshotScope, StateSnapshot, Status, Submission, VerifiedEffects,
};
use gofin_exchange::{
    PlannedSubmission,
    coinbase::{CoinbaseEndpoint, CoinbaseQuery, CoinbaseRequest, CoinbaseResponse, CoinbaseRoute},
};
use serde_json::{Value, json};
use sha2::Digest;

#[path = "institutional_operations.rs"]
mod operations;

/// Product-level operation definitions are kept in `contracts`, alongside the
/// Advanced contracts so discovery has one canonical listing.
pub fn definitions() -> Vec<gofin_domain::Definition> {
    let mut definitions = vec![Definition {
        name: "coinbaseexchange.create-balance-report".into(),
        description: "Request a Coinbase Exchange balance statement for one exact historical timestamp.".into(),
        kind: OperationKind::Configuration,
        execution_actor: ExecutionActor::HumanInteractive,
        input_schema: exchange_balance_report_schema(),
        output_schema: json!({"type":"object"}),
        error_schema: json!({"type":"object"}),
        examples: vec![],
        coverage: "Requests a CSV or PDF balance report at one exact timestamp for the enrolled Exchange profile. Report creation is a separately authorized non-trading configuration action; generated files are never fetched or followed by Fin.".into(),
        completion: "The operation completes only after the captured POST receipt's native report ID is polled and GET /reports/{report_id} reports ready for the exact enrolled profile and report type. An uncertain POST is never replayed.".into(),
        semantics: json!({"product":"coinbaseexchange","route":"POST /reports then GET /reports/{report_id}","clientVersion":"ccxt 4.5.85","reportType":"balance","fileUrl":"metadata only; never followed"}),
    }, Definition {
        name: "coinbaseexchange.create-account-report".into(),
        description: "Request a Coinbase Exchange account-history report for one exact native account and date interval.".into(),
        kind: OperationKind::Configuration,
        execution_actor: ExecutionActor::HumanInteractive,
        input_schema: exchange_account_report_schema(),
        output_schema: json!({"type":"object"}),
        error_schema: json!({"type":"object"}),
        examples: vec![],
        coverage: "Requests an account report scoped to one account verified under the enrolled Exchange profile. Date range and exact native account ID are fixed in the reviewed request; report files are never fetched.".into(),
        completion: "Requires a captured report ID followed by exact profile, account, date-range, format, and ready-status readback. An uncertain POST is never replayed.".into(),
        semantics: json!({"product":"coinbaseexchange","route":"POST /reports then GET /reports/{report_id}","clientVersion":"ccxt 4.5.85","reportType":"account","fileUrl":"metadata only; never followed"}),
    }, Definition {
        name: "coinbaseexchange.create-fills-report".into(),
        description: "Request a Coinbase Exchange fills report for one exact product and date interval.".into(),
        kind: OperationKind::Configuration,
        execution_actor: ExecutionActor::HumanInteractive,
        input_schema: exchange_fills_report_schema(),
        output_schema: json!({"type":"object"}),
        error_schema: json!({"type":"object"}),
        examples: vec![],
        coverage: "Requests a fills report scoped to one verified Exchange product and the enrolled profile. Date range and product ID are fixed in the reviewed request; report files are never fetched.".into(),
        completion: "Requires a captured report ID followed by exact profile, product, date-range, format, and ready-status readback. An uncertain POST is never replayed.".into(),
        semantics: json!({"product":"coinbaseexchange","route":"POST /reports then GET /reports/{report_id}","clientVersion":"ccxt 4.5.85","reportType":"fills","fileUrl":"metadata only; never followed"}),
    }, Definition {
        name: "coinbaseexchange.create-otc-fills-report".into(),
        description: "Request a Coinbase Exchange OTC fills report for one exact product and date interval.".into(),
        kind: OperationKind::Configuration,
        execution_actor: ExecutionActor::HumanInteractive,
        input_schema: exchange_product_history_report_schema(),
        output_schema: json!({"type":"object"}),
        error_schema: json!({"type":"object"}),
        examples: vec![],
        coverage: "Requests an OTC fills report scoped to one verified Exchange product and the enrolled profile. Date range and product ID are fixed in the reviewed request; report files are never fetched.".into(),
        completion: "Requires a captured report ID followed by exact profile, product, date-range, format, and ready-status readback. An uncertain POST is never replayed.".into(),
        semantics: json!({"product":"coinbaseexchange","route":"POST /reports then GET /reports/{report_id}","clientVersion":"ccxt 4.5.85","reportType":"otc-fills","fileUrl":"metadata only; never followed"}),
    }, Definition {
        name: "coinbaseexchange.create-rfq-fills-report".into(),
        description: "Request a Coinbase Exchange RFQ fills report for one exact product and date interval.".into(),
        kind: OperationKind::Configuration,
        execution_actor: ExecutionActor::HumanInteractive,
        input_schema: exchange_product_history_report_schema(),
        output_schema: json!({"type":"object"}),
        error_schema: json!({"type":"object"}),
        examples: vec![],
        coverage: "Requests an RFQ fills report scoped to one verified Exchange product and the enrolled profile. Date range and product ID are fixed in the reviewed request; report files are never fetched.".into(),
        completion: "Requires a captured report ID followed by exact profile, product, date-range, format, and ready-status readback. An uncertain POST is never replayed.".into(),
        semantics: json!({"product":"coinbaseexchange","route":"POST /reports then GET /reports/{report_id}","clientVersion":"ccxt 4.5.85","reportType":"rfq-fills","fileUrl":"metadata only; never followed"}),
    }, Definition {
        name: "coinbaseexchange.create-tax-invoice-report".into(),
        description: "Request a Coinbase Exchange tax invoice report for one exact product and date interval.".into(),
        kind: OperationKind::Configuration,
        execution_actor: ExecutionActor::HumanInteractive,
        input_schema: exchange_product_history_report_schema(),
        output_schema: json!({"type":"object"}),
        error_schema: json!({"type":"object"}),
        examples: vec![],
        coverage: "Requests a tax invoice report scoped to one verified Exchange product and the enrolled profile. Date range and product ID are fixed in the reviewed request; report files are never fetched.".into(),
        completion: "Requires a captured report ID followed by exact profile, product, date-range, format, and ready-status readback. An uncertain POST is never replayed.".into(),
        semantics: json!({"product":"coinbaseexchange","route":"POST /reports then GET /reports/{report_id}","clientVersion":"ccxt 4.5.85","reportType":"tax-invoice","fileUrl":"metadata only; never followed"}),
    }, Definition {
        name: "coinbaseexchange.get-report".into(),
        description: "Read the status and metadata for one Coinbase Exchange report in the enrolled profile.".into(),
        kind: OperationKind::Read,
        execution_actor: ExecutionActor::AgentReadable,
        input_schema: exchange_get_report_schema(),
        output_schema: json!({"type":"object"}),
        error_schema: json!({"type":"object"}),
        examples: vec![],
        coverage: "Reads the exact native report resource and returns only whitelisted report metadata. The `file_url` is returned as opaque metadata and is never fetched or followed.".into(),
        completion: "The native response must identify the requested report and the enrolled Exchange profile.".into(),
        semantics: json!({"product":"coinbaseexchange","route":"GET /reports/{report_id}","clientVersion":"ccxt 4.5.85"}),
    }, Definition {
        name: "coinbaseexchange.get-lending-overview".into(),
        description: "Read the loan, collateral and lending overview for one enrolled Coinbase Exchange profile.".into(),
        kind: OperationKind::Read,
        execution_actor: ExecutionActor::AgentReadable,
        input_schema: exchange_lending_overview_schema(),
        output_schema: json!({"type":"object"}),
        error_schema: json!({"type":"object"}),
        examples: vec![],
        coverage: "Returns the native authenticated Exchange lending overview. USD notional loan and collateral figures are preserved as native response evidence and are not treated as cash balances or spendable asset amounts.".into(),
        completion: "Returns only the lending overview from the enrolled Exchange credential; no borrow, repay or collateral write is implemented by this read operation.".into(),
        semantics: json!({"product":"coinbaseexchange","route":"GET /loans/lending-overview","clientVersion":"ccxt 4.5.85"}),
    }, Definition {
        name: "coinbaseinternational.edit-order".into(),
        description: "Edit the price of one journal-linked Coinbase International spot limit order.".into(),
        kind: OperationKind::Write,
        execution_actor: ExecutionActor::AgentInvocable,
        input_schema: international_edit_order_schema(),
        output_schema: json!({"type":"object"}),
        error_schema: json!({"type":"object"}),
        examples: vec![],
        coverage: "Price-only edits of one exact open SPOT LIMIT GTC order linked to a protected original Fin operation. Size, portfolio and client identity remain fixed; the native edit request receives a deterministic successor client ID.".into(),
        completion: "The amendment is complete only after the captured native response and an enrolled-portfolio-scoped order read prove the exact successor price, size, client ID and portfolio. Original fills remain owned by the original create operation.".into(),
        semantics: json!({"product":"coinbaseinternational","route":"PUT /api/v1/orders/{id}","clientVersion":"ccxt 4.5.85","broker_generated_attempt_input_fields":["/requestId"]}),
    }, Definition {
        name: "coinbaseexchange.create-order".into(),
        description: "Prepare and submit one bounded Coinbase Exchange spot limit order.".into(),
        kind: OperationKind::Write,
        execution_actor: ExecutionActor::AgentInvocable,
        input_schema: create_order_schema(),
        output_schema: json!({"type":"object"}),
        error_schema: json!({"type":"object"}),
        examples: vec![],
        coverage: "Exchange spot limit orders only; the official Exchange REST API has no preview route, so preparation validates exact product metadata, protected fee tier, spendable account balance, and the frozen limit request locally.".into(),
        completion: "Submission acceptance is not settlement; order identity and every fill must be reconciled before balances are treated as final.".into(),
        semantics: json!({"product":"coinbaseexchange","route":"POST /orders","clientVersion":"ccxt 4.5.85","preview":"local exact validation; no venue preview endpoint","broker_generated_attempt_input_fields":["/requestId"]}),
    }, Definition {
        name: "coinbaseinternational.cancel-order".into(),
        description: "Cancel the verified open remainder of one Coinbase International order.".into(),
        kind: OperationKind::Write,
        execution_actor: ExecutionActor::AgentInvocable,
        input_schema: international_cancel_order_schema(),
        output_schema: json!({"type":"object"}),
        error_schema: json!({"type":"object"}),
        examples: vec![],
        coverage: "One exact venue order linked to one unresolved protected International create-order journal entry; no cancel-all route is exposed.".into(),
        completion: "Cancellation completes only after the native cancel receipt and portfolio-scoped GET order show a terminal order; the original operation reconciles all fills separately.".into(),
        semantics: json!({"product":"coinbaseinternational","route":"DELETE /api/v1/orders/{id}?portfolio=...","clientVersion":"ccxt 4.5.85"}),
    }, Definition {
        name: "coinbaseinternational.create-order".into(),
        description: "Preview and submit one bounded Coinbase International spot limit order.".into(),
        kind: OperationKind::Write,
        execution_actor: ExecutionActor::AgentInvocable,
        input_schema: international_create_order_schema(),
        output_schema: json!({"type":"object"}),
        error_schema: json!({"type":"object"}),
        examples: vec![],
        coverage: "International SPOT LIMIT GTC, IOC or FOK. Instrument increments, spot fee tiers, enrolled portfolio balances and exact request identity are checked locally before dispatch; no native preview route is assumed.".into(),
        completion: "Accepted orders remain pending until authoritative order state and every portfolio fill reconcile to exact totals.".into(),
        semantics: json!({"product":"coinbaseinternational","route":"POST /api/v1/orders","preview":"local exact validation from instrument metadata, fee tiers and portfolio balances; no native preview route","clientVersion":"ccxt 4.5.85","broker_generated_attempt_input_fields":["/requestId"]}),
    }, Definition {
        name: "coinbaseinternational.get-transfers".into(),
        description: "Read one bounded page of transfers for the enrolled Coinbase International portfolio.".into(),
        kind: OperationKind::Read,
        execution_actor: ExecutionActor::AgentReadable,
        input_schema: international_transfers_schema(),
        output_schema: json!({"type":"object"}),
        error_schema: json!({"type":"object"}),
        examples: vec![],
        coverage: "Native International transfer history filtered to the enrolled portfolio, with explicit time/status and offset bounds.".into(),
        completion: "Returns the native transfer rows and pagination metadata; a transfer record is not proof of a Fin-submitted operation unless its durable identity is independently bound.".into(),
        semantics: json!({"product":"coinbaseinternational","route":"GET /api/v1/transfers","clientVersion":"ccxt 4.5.85"}),
    }, Definition {
        name: "coinbaseexchange.cancel-order".into(),
        description: "Cancel the verified open remainder of one Coinbase Exchange order.".into(),
        kind: OperationKind::Write,
        execution_actor: ExecutionActor::AgentInvocable,
        input_schema: cancel_exchange_order_schema(),
        output_schema: json!({"type":"object"}),
        error_schema: json!({"type":"object"}),
        examples: vec![],
        coverage: "One exact Exchange order ID linked to one unresolved Fin create-order operation; cancel-all is not supported.".into(),
        completion: "The original order and fills remain owned by the original operation; cancellation is complete only after authoritative terminal order evidence.".into(),
        semantics: json!({"product":"coinbaseexchange","route":"DELETE /orders/{order_id}","clientVersion":"ccxt 4.5.85"}),
    }, Definition {
        name: "coinbaseexchange.get-orders".into(),
        description: "Read bounded, profile-scoped Coinbase Exchange order history.".into(),
        kind: OperationKind::Read,
        execution_actor: ExecutionActor::AgentReadable,
        input_schema: exchange_orders_schema(),
        output_schema: json!({"type":"object"}),
        error_schema: json!({"type":"object"}),
        examples: vec![],
        coverage: "Native Exchange order list, filtered to the enrolled profile, with bounded product/status filters and cursor exhaustion.".into(),
        completion: "Returns the exact native orders from every cursor page; this read does not establish settlement.".into(),
        semantics: json!({"product":"coinbaseexchange","route":"GET /orders","clientVersion":"ccxt 4.5.85"}),
    }];
    definitions.extend(operations::definitions());
    definitions.extend(administration::definitions());
    definitions
}

/// Dispatch an institutional read through its enrolled HMAC credential.
/// Returns `None` for Advanced operations so the profile can handle them.
pub async fn read(
    profile: &CoinbaseProfile,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>> {
    if let Some(value) = administration::read(profile, operation, input).await? {
        return Ok(Some(value));
    }
    if let Some(value) = operations::read(profile, operation, input).await? {
        return Ok(Some(value));
    }
    if operation == "coinbaseexchange.get-orders" {
        return Ok(Some(read_exchange_orders(profile, input).await?));
    }
    if operation == "coinbaseinternational.get-transfers" {
        return Ok(Some(read_international_transfers(profile, input).await?));
    }
    if operation == "coinbaseinternational.get-fills" {
        return Ok(Some(read_international_fills(profile, input).await?));
    }
    if operation == "coinbaseexchange.get-report" {
        return Ok(Some(read_exchange_report(profile, input).await?));
    }
    if operation == "coinbaseexchange.get-lending-overview" {
        return Ok(Some(read_exchange_lending_overview(profile, input).await?));
    }
    let Some(contract) = contracts::contract(operation) else {
        return Ok(None);
    };
    let product = match contract.product {
        contracts::Product::Exchange => CoinbaseProduct::Exchange,
        contracts::Product::International => CoinbaseProduct::International,
        contracts::Product::Advanced => return Ok(None),
    };
    if contract.action != contracts::Action::Read {
        return Ok(None);
    }
    let validator = jsonschema::validator_for(&(contract.schema)()).map_err(|_| {
        error(
            "COINBASE_CONTRACT_INVALID",
            "The institutional operation schema is invalid.",
        )
    })?;
    if !validator.is_valid(input) {
        return Err(error(
            "INVALID_INPUT",
            "Input does not match the closed institutional operation schema.",
        ));
    }
    let account = profile
        .accounts()
        .iter()
        .find(|account| {
            account.name == input["account"].as_str().unwrap_or_default()
                && account.product == product
        })
        .ok_or_else(|| {
            error(
                "COINBASE_ACCOUNT_UNKNOWN",
                "Select an enrolled account for this Coinbase product.",
            )
        })?;
    if !account.identity_verified {
        return Err(error(
            "COINBASE_IDENTITY_UNVERIFIED",
            "Institutional reads require an enrolled and verified account identity.",
        ));
    }
    if let Some(portfolio) = input.get("portfolioId").and_then(Value::as_str)
        && account.portfolio_id.as_deref() != Some(portfolio)
    {
        return Err(error(
            "COINBASE_PORTFOLIO_MISMATCH",
            "Requested portfolio differs from the enrolled account.",
        ));
    }
    let text = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_owned);
    let query = CoinbaseQuery {
        limit: input
            .get("limit")
            .and_then(Value::as_u64)
            .and_then(|n| u16::try_from(n).ok()),
        starting_after: text("startingAfter"),
        ending_before: text("endingBefore"),
        product_id: text("productId"),
        order_id: text("orderId"),
        account_id: text("accountId"),
        profile_id: (product == CoinbaseProduct::Exchange)
            .then(|| account.physical_identity.clone()),
        portfolio_id: (product == CoinbaseProduct::International)
            .then(|| account.portfolio_id.clone())
            .flatten(),
        ..Default::default()
    };
    let endpoint = match contract.route {
        "exchange_accounts" => CoinbaseRoute::ExchangeAccounts,
        "exchange_account" => CoinbaseRoute::ExchangeAccount {
            account_id: text("accountId").expect("schema"),
        },
        "exchange_order" => CoinbaseRoute::ExchangeOrder {
            order_id: text("id").expect("schema"),
        },
        "exchange_fills" => CoinbaseRoute::ExchangeFillsFiltered {
            query: query.clone(),
        },
        "exchange_fees" => CoinbaseRoute::ExchangeFees,
        "exchange_transfers" => CoinbaseRoute::ExchangeTransfers {
            account_id: text("accountId").expect("schema"),
            query: query.clone(),
        },
        "international_accounts" => CoinbaseRoute::InternationalAccounts,
        "international_order" => CoinbaseRoute::InternationalOrderScoped {
            order_id: text("id").expect("schema"),
            portfolio_id: account.portfolio_id.clone().ok_or_else(|| {
                error(
                    "COINBASE_PORTFOLIO_UNKNOWN",
                    "International portfolio identity is missing.",
                )
            })?,
        },
        "international_fills" => CoinbaseRoute::InternationalPortfolioFills {
            portfolio_id: account
                .portfolio_id
                .clone()
                .unwrap_or_else(|| account.physical_identity.clone()),
        },
        _ => return Ok(None),
    };
    let route_name = contract.route;
    if matches!(route_name, "exchange_fills" | "exchange_transfers") {
        return Ok(Some(
            read_all_exchange_pages(profile, account, route_name, query, endpoint).await?,
        ));
    }
    let response = profile
        .request_for_account(
            &account.name,
            product,
            endpoint,
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    let is_paged = matches!(
        route_name,
        "exchange_fills" | "exchange_transfers" | "international_fills"
    );
    let Some(value) = parse_read_response(response, is_paged)? else {
        return Ok(None);
    };
    if matches!(route_name, "exchange_order" | "international_order") {
        let expected = input["id"].as_str().expect("route schema binds id");
        let order = value.get("order").unwrap_or(&value);
        let returned = order
            .get("id")
            .or_else(|| order.get("order_id"))
            .and_then(identifier_text);
        if returned.as_deref() != Some(expected) {
            return Err(error(
                "COINBASE_ORDER_IDENTITY_MISMATCH",
                "Coinbase order response did not match the requested native order ID.",
            ));
        }
    }
    validate_identity(account, route_name, value).map(Some)
}

fn exchange_get_report_schema() -> Value {
    json!({"type":"object","required":["account","reportId"],"properties":{
        "account":{"type":"string","minLength":1,"maxLength":128},
        "reportId":{"type":"string","minLength":1,"maxLength":128}
    },"additionalProperties":false})
}

fn exchange_lending_overview_schema() -> Value {
    json!({
        "type":"object",
        "required":["account"],
        "properties":{"account":{"type":"string","minLength":1}},
        "additionalProperties":false
    })
}

async fn read_exchange_lending_overview(profile: &CoinbaseProfile, input: &Value) -> Result<Value> {
    let validator =
        jsonschema::validator_for(&exchange_lending_overview_schema()).map_err(|_| {
            error(
                "COINBASE_CONTRACT_INVALID",
                "The lending overview schema is invalid.",
            )
        })?;
    if !validator.is_valid(input) {
        return Err(error(
            "INVALID_INPUT",
            "Lending overview input does not match its closed schema.",
        ));
    }
    let account_name = input["account"].as_str().expect("validated");
    let account = profile
        .accounts()
        .iter()
        .find(|account| {
            account.name == account_name && account.product == CoinbaseProduct::Exchange
        })
        .ok_or_else(|| {
            error(
                "COINBASE_ACCOUNT_UNKNOWN",
                "Select an enrolled Exchange profile.",
            )
        })?;
    if !account.identity_verified || account.physical_identity.trim().is_empty() {
        return Err(error(
            "COINBASE_IDENTITY_UNVERIFIED",
            "Lending overview requires a verified enrolled Exchange profile.",
        ));
    }
    let response = profile
        .request_for_account(
            account_name,
            CoinbaseProduct::Exchange,
            CoinbaseRoute::ExchangeLoanLendingOverview,
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    let value = parse_required_json(
        response,
        "Coinbase Exchange lending overview could not be read.",
    )?;
    if !value.is_object()
        || value.get("overview").is_none()
        || !value.get("loans").is_some_and(Value::is_array)
    {
        return Err(error(
            "COINBASE_RESPONSE_INVALID",
            "Exchange lending overview omitted its native overview or loans collection.",
        ));
    }
    Ok(
        json!({"account":account.name,"profileId":account.physical_identity,"lendingOverview":value}),
    )
}

fn exchange_balance_report_schema() -> Value {
    json!({"type":"object","required":["account","requestId","reportAt","format"],"properties":{
        "account":{"type":"string","minLength":1,"maxLength":128},
        "requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},
        "reportAt":{"type":"string","minLength":20,"maxLength":40},
        "format":{"type":"string","enum":["csv","pdf"]}
    },"additionalProperties":false})
}

fn exchange_account_report_schema() -> Value {
    json!({"type":"object","required":["account","requestId","accountId","startDate","endDate","format"],"properties":{
        "account":{"type":"string","minLength":1,"maxLength":128},
        "requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},
        "accountId":{"type":"string","minLength":1,"maxLength":128},
        "startDate":{"type":"string","minLength":20,"maxLength":40},
        "endDate":{"type":"string","minLength":20,"maxLength":40},
        "format":{"type":"string","enum":["csv","pdf"]}
    },"additionalProperties":false})
}

fn exchange_fills_report_schema() -> Value {
    json!({"type":"object","required":["account","requestId","productId","startDate","endDate","format"],"properties":{
        "account":{"type":"string","minLength":1,"maxLength":128},
        "requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},
        "productId":{"type":"string","pattern":"^[A-Za-z0-9_-]{1,64}$"},
        "startDate":{"type":"string","minLength":20,"maxLength":40},
        "endDate":{"type":"string","minLength":20,"maxLength":40},
        "format":{"type":"string","enum":["csv","pdf"]}
    },"additionalProperties":false})
}

fn exchange_product_history_report_schema() -> Value {
    json!({"type":"object","required":["account","requestId","productId","startDate","endDate","format"],"properties":{
        "account":{"type":"string","minLength":1,"maxLength":128},
        "requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},
        "productId":{"type":"string","pattern":"^[A-Za-z0-9_-]{1,64}$"},
        "startDate":{"type":"string","minLength":20,"maxLength":40},
        "endDate":{"type":"string","minLength":20,"maxLength":40},
        "format":{"type":"string","enum":["csv","pdf"]}
    },"additionalProperties":false})
}

fn exchange_history_report_schema(report_type: &str) -> Value {
    match report_type {
        "account" => exchange_account_report_schema(),
        "fills" => exchange_fills_report_schema(),
        "otc-fills" | "rfq-fills" | "tax-invoice" => exchange_product_history_report_schema(),
        _ => json!({}),
    }
}

async fn prepare_exchange_history_report(
    profile: &CoinbaseProfile,
    input: &Value,
    report_type: &str,
) -> Result<PreparedOperation> {
    let schema = exchange_history_report_schema(report_type);
    let validator = jsonschema::validator_for(&schema)
        .map_err(|_| error("COINBASE_CONTRACT_INVALID", "The report schema is invalid."))?;
    if !validator.is_valid(input) {
        return Err(error(
            "INVALID_INPUT",
            "Historical report input does not match its closed schema.",
        ));
    }
    let account = verified_exchange_account(profile, input["account"].as_str().expect("schema"))?;
    let start_date = input["startDate"].as_str().expect("schema");
    let end_date = input["endDate"].as_str().expect("schema");
    let start = chrono::DateTime::parse_from_rfc3339(start_date).map_err(|_| {
        error(
            "INVALID_INPUT",
            "startDate must be RFC 3339 with an explicit offset.",
        )
    })?;
    let end = chrono::DateTime::parse_from_rfc3339(end_date).map_err(|_| {
        error(
            "INVALID_INPUT",
            "endDate must be RFC 3339 with an explicit offset.",
        )
    })?;
    if start >= end || end.with_timezone(&chrono::Utc) > chrono::Utc::now() {
        return Err(error(
            "INVALID_INPUT",
            "The report interval must be ordered and cannot end in the future.",
        ));
    }
    let format = input["format"].as_str().expect("schema");
    let native_scope = if report_type == "account" {
        let account_id = input["accountId"].as_str().expect("schema");
        let response = profile
            .request_for_account(
                &account.name,
                CoinbaseProduct::Exchange,
                CoinbaseRoute::ExchangeAccount {
                    account_id: account_id.into(),
                },
                CoinbaseRequest::get(),
                None,
            )
            .await?;
        let native: Value = parse_required_json(
            response,
            "The report account could not be verified under the enrolled profile.",
        )?;
        if native.get("id").and_then(Value::as_str) != Some(account_id)
            || native.get("profile_id").and_then(Value::as_str)
                != Some(account.physical_identity.as_str())
        {
            return Err(error(
                "COINBASE_REPORT_ACCOUNT_MISMATCH",
                "The requested account is not verified under the enrolled Exchange profile.",
            ));
        }
        json!({"account_id":account_id})
    } else {
        let product_id = input["productId"].as_str().expect("schema");
        let descriptor = crate::reads::descriptor("coinbaseexchange.read-public-products-id")
            .ok_or_else(|| {
                error(
                    "COINBASE_ROUTE_UNSUPPORTED",
                    "The Exchange product read route is unavailable.",
                )
            })?;
        let endpoint = crate::reads::endpoint(descriptor, &json!({"id":product_id}));
        let response = profile
            .request_for_account(
                &account.name,
                CoinbaseProduct::Exchange,
                endpoint,
                CoinbaseRequest::get(),
                None,
            )
            .await?;
        let native: Value = parse_required_json(
            response,
            "The report product could not be verified against Coinbase Exchange.",
        )?;
        if native.get("id").and_then(Value::as_str) != Some(product_id) {
            return Err(error(
                "COINBASE_REPORT_PRODUCT_MISMATCH",
                "The requested product did not match its native Exchange identity.",
            ));
        }
        json!({"product_id":product_id})
    };
    let scope_key = match report_type {
        "account" => "account",
        "fills" => "fills",
        "otc-fills" => "otc_fills",
        "rfq-fills" => "rfq_fills",
        "tax-invoice" => "tax_invoice",
        _ => unreachable!("report schema was matched above"),
    };
    let mut body_value = json!({
        "type":report_type,
        "format":format,
        "profile_id":account.physical_identity,
        (scope_key):{
            "start_date":start_date,
            "end_date":end_date
        }
    });
    body_value[scope_key]
        .as_object_mut()
        .expect("report scope is object")
        .extend(native_scope.as_object().expect("scope object").clone());
    let request_body = gofin_domain::canonical_json(&body_value)?;
    Ok(PreparedOperation {
        operation: format!("coinbaseexchange.create-{report_type}-report"),
        input: input.clone(),
        binding: json!({
            "version":1,"provider":"coinbaseexchange","account":account.name,
            "resource":account.resource,"physicalIdentity":account.physical_identity,
            "requestId":input["requestId"],"reportType":report_type,
            "format":format,"startDate":start_date,"endDate":end_date,
            "accountId":input.get("accountId"),"productId":input.get("productId"),
            "requestBodyExact":request_body
        }),
        summary: format!("Request a Coinbase Exchange {report_type} report from {start_date} through {end_date}"),
        checks: vec![Check {
            name: "exchange_report_scope_verified".into(),
            status: CheckStatus::Passed,
            detail: format!("The exact {report_type} scope was verified under the enrolled Exchange profile; the native request uses explicit bounded dates."),
        }],
        warnings: vec!["Report generation is a separately authorized configuration action. An uncertain request is never replayed, and the returned file URL is opaque metadata that Fin never fetches.".into()],
        observed_at: gofin_domain::now(),
        expires_at: (chrono::Utc::now() + chrono::Duration::seconds(45)).to_rfc3339(),
        valuation: None,
        intent: FinancialIntent { debits: vec![], maximum_costs: vec![], expected: vec![], locks: vec![account.resource.clone()] },
        operation_version: 1,
        action_deadline: None,
    })
}

async fn read_exchange_report(profile: &CoinbaseProfile, input: &Value) -> Result<Value> {
    let validator = jsonschema::validator_for(&exchange_get_report_schema()).map_err(|_| {
        error(
            "COINBASE_CONTRACT_INVALID",
            "The report-read schema is invalid.",
        )
    })?;
    if !validator.is_valid(input) {
        return Err(error(
            "INVALID_INPUT",
            "Report read input does not match its closed schema.",
        ));
    }
    let account = verified_exchange_account(profile, input["account"].as_str().expect("schema"))?;
    let report_id = input["reportId"].as_str().expect("schema");
    let descriptor = crate::reads::descriptor("coinbaseexchange.read-private-reports-report-id")
        .ok_or_else(|| {
            error(
                "COINBASE_ROUTE_UNSUPPORTED",
                "The pinned report read route is unavailable.",
            )
        })?;
    let endpoint = crate::reads::endpoint(descriptor, &json!({"report_id":report_id}));
    let response = profile
        .request_for_account(
            &account.name,
            CoinbaseProduct::Exchange,
            endpoint,
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    if !(200..300).contains(&response.status) {
        return Err(error(
            "COINBASE_REPORT_READ_FAILED",
            "Coinbase did not return the requested report.",
        ));
    }
    let report: Value = serde_json::from_slice(&response.body).map_err(|_| {
        error(
            "COINBASE_RESPONSE_INVALID",
            "Coinbase returned invalid report JSON.",
        )
    })?;
    if !matches!(
        report.get("type").and_then(Value::as_str),
        Some("balance" | "account" | "fills" | "otc-fills" | "rfq-fills" | "tax-invoice")
    ) {
        return Err(error(
            "COINBASE_REPORT_TYPE_UNSUPPORTED",
            "This report type is not exposed by Fin.",
        ));
    }
    validate_exchange_report(
        &report,
        report_id,
        &account.physical_identity,
        report["type"].as_str().expect("checked"),
    )?;
    Ok(exchange_report_metadata(&report))
}

fn verified_exchange_account<'a>(
    profile: &'a CoinbaseProfile,
    name: &str,
) -> Result<&'a CoinbaseAccount> {
    let account = profile
        .accounts()
        .iter()
        .find(|a| a.name == name && a.product == CoinbaseProduct::Exchange)
        .ok_or_else(|| {
            error(
                "COINBASE_ACCOUNT_UNKNOWN",
                "Select an enrolled Coinbase Exchange profile.",
            )
        })?;
    if !account.identity_verified || account.physical_identity.trim().is_empty() {
        return Err(error(
            "COINBASE_IDENTITY_UNVERIFIED",
            "This report operation requires a verified Exchange profile.",
        ));
    }
    Ok(account)
}

fn validate_exchange_report(
    report: &Value,
    report_id: &str,
    profile_id: &str,
    expected_type: &str,
) -> Result<()> {
    let params_profile = report
        .get("params")
        .and_then(|p| p.get("profile_id"))
        .and_then(Value::as_str);
    if report.get("id").and_then(Value::as_str) != Some(report_id)
        || report.get("type").and_then(Value::as_str) != Some(expected_type)
        || params_profile != Some(profile_id)
    {
        return Err(error(
            "COINBASE_REPORT_IDENTITY_MISMATCH",
            "Native report identity or enrolled profile did not match.",
        ));
    }
    Ok(())
}

fn exchange_report_metadata(report: &Value) -> Value {
    let params = &report["params"];
    let mut metadata = json!({
        "id": report["id"],
        "type": report["type"],
        "status": report["status"],
        "profileId": params["profile_id"],
        "format": params["format"],
        "reportAt": params.get("balance").and_then(|v|v.get("datetime")),
        "startDate": params.get("start_date").or_else(||params.get("account").or_else(||params.get("fills")).or_else(||params.get("otc_fills")).or_else(||params.get("rfq_fills")).or_else(||params.get("tax_invoice")).and_then(|v|v.get("start_date"))),
        "endDate": params.get("end_date").or_else(||params.get("account").or_else(||params.get("fills")).or_else(||params.get("otc_fills")).or_else(||params.get("rfq_fills")).or_else(||params.get("tax_invoice")).and_then(|v|v.get("end_date"))),
        "accountId": params.get("account_id").or_else(||params.get("account").and_then(|v|v.get("account_id"))),
        "productId": params.get("product_id").or_else(||params.get("fills").or_else(||params.get("otc_fills")).or_else(||params.get("rfq_fills")).or_else(||params.get("tax_invoice")).and_then(|v|v.get("product_id"))),
        "createdAt": report["created_at"],
        "completedAt": report["completed_at"],
        "expiresAt": report["expires_at"],
        "fileCount": report["file_count"]
    });
    if let Some(file_url) = report.get("file_url").and_then(Value::as_str) {
        metadata["fileUrl"] = Value::String(file_url.to_owned());
    }
    metadata
}

fn report_operation_name(report_type: &str) -> &'static str {
    match report_type {
        "account" => "create-account-report",
        "fills" => "create-fills-report",
        "otc-fills" => "create-otc-fills-report",
        "rfq-fills" => "create-rfq-fills-report",
        "tax-invoice" => "create-tax-invoice-report",
        _ => "create-balance-report",
    }
}

fn exchange_history_report_type(operation: &str) -> Option<&'static str> {
    match operation {
        "coinbaseexchange.create-account-report" => Some("account"),
        "coinbaseexchange.create-fills-report" => Some("fills"),
        "coinbaseexchange.create-otc-fills-report" => Some("otc-fills"),
        "coinbaseexchange.create-rfq-fills-report" => Some("rfq-fills"),
        "coinbaseexchange.create-tax-invoice-report" => Some("tax-invoice"),
        _ => None,
    }
}

fn report_parameters_match(report: &Value, binding: &Value) -> bool {
    let params = &report["params"];
    let report_type = binding["reportType"].as_str().unwrap_or("balance");
    if params["profile_id"] != binding["physicalIdentity"] || params["format"] != binding["format"]
    {
        return false;
    }
    match report_type {
        "balance" => params["balance"]["datetime"] == binding["reportAt"],
        "account" => {
            params
                .get("start_date")
                .or_else(|| params.get("account").and_then(|v| v.get("start_date")))
                == binding.get("startDate")
                && params
                    .get("end_date")
                    .or_else(|| params.get("account").and_then(|v| v.get("end_date")))
                    == binding.get("endDate")
                && params
                    .get("account_id")
                    .or_else(|| params.get("account").and_then(|v| v.get("account_id")))
                    == binding.get("accountId")
        }
        "fills" => {
            params
                .get("start_date")
                .or_else(|| params.get("fills").and_then(|v| v.get("start_date")))
                == binding.get("startDate")
                && params
                    .get("end_date")
                    .or_else(|| params.get("fills").and_then(|v| v.get("end_date")))
                    == binding.get("endDate")
                && params
                    .get("product_id")
                    .or_else(|| params.get("fills").and_then(|v| v.get("product_id")))
                    == binding.get("productId")
        }
        "otc-fills" | "rfq-fills" | "tax-invoice" => {
            let scope = match report_type {
                "otc-fills" => "otc_fills",
                "rfq-fills" => "rfq_fills",
                _ => "tax_invoice",
            };
            params
                .get("start_date")
                .or_else(|| params[scope].get("start_date"))
                == binding.get("startDate")
                && params
                    .get("end_date")
                    .or_else(|| params[scope].get("end_date"))
                    == binding.get("endDate")
                && params
                    .get("product_id")
                    .or_else(|| params[scope].get("product_id"))
                    == binding.get("productId")
        }
        _ => false,
    }
}

async fn reconcile_exchange_report(
    profile: &CoinbaseProfile,
    record: &OperationRecord,
    submission: &Submission,
) -> Result<Reconciliation> {
    let unknown = |reason: &str| Reconciliation {
        status: Status::Unknown,
        evidence: json!({"reason":reason}),
        verified: None,
    };
    if record.submission.as_ref() != Some(submission) {
        return Ok(unknown(
            "the report submission differs from the durable record",
        ));
    }
    let Some(prepared) = record.preview.as_ref().filter(|prepared| {
        prepared.operation == record.operation
            && matches!(
                prepared.operation.as_str(),
                "coinbaseexchange.create-balance-report"
                    | "coinbaseexchange.create-account-report"
                    | "coinbaseexchange.create-fills-report"
                    | "coinbaseexchange.create-otc-fills-report"
                    | "coinbaseexchange.create-rfq-fills-report"
                    | "coinbaseexchange.create-tax-invoice-report"
            )
    }) else {
        return Ok(unknown("the immutable Exchange report request is missing"));
    };
    let binding = &prepared.binding;
    let Some(account_name) = binding["account"].as_str() else {
        return Ok(unknown("the prepared Exchange profile is missing"));
    };
    let expected_type = binding["reportType"].as_str().unwrap_or("balance");
    let body = binding
        .get("requestBodyExact")
        .and_then(Value::as_str)
        .or_else(|| binding.get("bodyExact").and_then(Value::as_str))
        .unwrap_or_default();
    let body_digest = hex::encode(sha2::Sha256::digest(body.as_bytes()));
    let report_operation = report_operation_name(expected_type);
    if submission.kind != "coinbase"
        || submission.reference != format!("report-request:{body_digest}")
        || submission.data["product"] != "coinbaseexchange"
        || submission.data["operation"] != report_operation
        || submission.data["requestId"] != binding["requestId"]
        || submission.data["profileId"] != binding["physicalIdentity"]
        || submission.data["bodySha256"] != body_digest
        || submission.data["binding"] != *binding
    {
        return Ok(unknown(
            "the durable report submission differs from its prepared request",
        ));
    }
    let Some(account) = profile.accounts().iter().find(|account| {
        account.name == account_name && account.product == CoinbaseProduct::Exchange
    }) else {
        return Ok(unknown("the enrolled Exchange profile is unavailable"));
    };
    if account.product != CoinbaseProduct::Exchange
        || !account.identity_verified
        || binding["physicalIdentity"].as_str() != Some(account.physical_identity.as_str())
        || binding["resource"] != json!(account.resource)
    {
        return Ok(unknown("the enrolled Exchange profile identity changed"));
    }
    let Some(dispatch) = record.dispatch_evidence.as_ref() else {
        return Ok(unknown(
            "the report POST has no captured receipt and will not be replayed",
        ));
    };
    let status_code = dispatch["httpStatus"].as_u64().unwrap_or_default() as u16;
    let Some(receipt_bytes) = dispatch["bodyHex"]
        .as_str()
        .and_then(|hex| hex::decode(hex).ok())
    else {
        return Ok(unknown("the captured native report receipt is unavailable"));
    };
    let expected_response_hash = dispatch["bodySha256"].as_str().unwrap_or_default();
    if expected_response_hash.is_empty()
        || hex::encode(sha2::Sha256::digest(&receipt_bytes)) != expected_response_hash
    {
        return Ok(unknown(
            "the captured report receipt digest does not match its exact bytes",
        ));
    }
    let receipt: Value = match serde_json::from_slice(&receipt_bytes) {
        Ok(value) => value,
        Err(_) => {
            return Ok(unknown(
                "the captured native report receipt is invalid JSON",
            ));
        }
    };
    if (400..500).contains(&status_code) {
        return Ok(Reconciliation {
            status: Status::Failed,
            evidence: json!({"httpStatus":status_code,"receipt":receipt,"reason":"Coinbase rejected the report request"}),
            verified: None,
        });
    }
    let Some(report_id) = receipt
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    else {
        return Ok(unknown(
            "the report POST response did not contain a durable native report ID",
        ));
    };
    if !(200..300).contains(&status_code)
        || receipt.get("type").and_then(Value::as_str) != Some(expected_type)
        || !matches!(
            receipt.get("status").and_then(Value::as_str),
            Some("pending" | "creating" | "ready")
        )
    {
        return Ok(unknown(
            "the captured report response did not prove acceptance of the bound report type",
        ));
    }
    let descriptor = crate::reads::descriptor("coinbaseexchange.read-private-reports-report-id")
        .ok_or_else(|| {
            error(
                "COINBASE_ROUTE_UNSUPPORTED",
                "The pinned report status route is unavailable.",
            )
        })?;
    let endpoint = crate::reads::endpoint(descriptor, &json!({"report_id":report_id}));
    let response = match profile
        .request_for_account(
            account_name,
            CoinbaseProduct::Exchange,
            endpoint,
            CoinbaseRequest::get(),
            None,
        )
        .await
    {
        Ok(response) if (200..300).contains(&response.status) => response,
        _ => {
            return Ok(unknown(
                "the exact native report resource could not be read",
            ));
        }
    };
    let report: Value = match serde_json::from_slice(&response.body) {
        Ok(report) => report,
        Err(_) => return Ok(unknown("the native report status response is invalid JSON")),
    };
    if validate_exchange_report(
        &report,
        report_id,
        &account.physical_identity,
        expected_type,
    )
    .is_err()
    {
        return Ok(unknown(
            "native report status did not attest the captured ID and enrolled profile",
        ));
    }
    if !report_parameters_match(&report, binding) {
        return Ok(unknown(
            "native report parameters did not match the exact reviewed request",
        ));
    }
    let metadata = exchange_report_metadata(&report);
    match report.get("status").and_then(Value::as_str) {
        Some("pending" | "creating") => Ok(Reconciliation {
            status: Status::Pending,
            evidence: json!({"postReceipt":receipt,"report":metadata,"reason":"the exact report remains in progress"}),
            verified: None,
        }),
        Some("failed") => Ok(Reconciliation {
            status: Status::Failed,
            evidence: json!({"postReceipt":receipt,"report":metadata,"reason":"Coinbase marked the exact report failed"}),
            verified: None,
        }),
        Some("ready")
            if report
                .get("file_url")
                .and_then(Value::as_str)
                .is_some_and(|url| !url.is_empty()) =>
        {
            Ok(Reconciliation {
                status: Status::Completed,
                evidence: json!({"postReceipt":receipt,"report":metadata,"fileUrlPolicy":"opaque metadata only; not fetched"}),
                verified: None,
            })
        }
        _ => Ok(unknown(
            "the exact report has no conclusive ready/failed status",
        )),
    }
}

fn exchange_orders_schema() -> Value {
    json!({
        "type":"object",
        "required":["account"],
        "properties":{
            "account":{"type":"string","minLength":1,"maxLength":128},
            "productId":{"type":"string","pattern":"^[A-Za-z0-9_-]{1,128}$"},
            "status":{"type":"string","enum":["open","pending","active","done","settled","rejected","all"]},
            "limit":{"type":"integer","minimum":1,"maximum":250},
            "after":{"type":"string","minLength":1,"maxLength":512},
            "before":{"type":"string","minLength":1,"maxLength":512}
        },
        "additionalProperties":false
    })
}

async fn read_exchange_orders(profile: &CoinbaseProfile, input: &Value) -> Result<Value> {
    let validator = jsonschema::validator_for(&exchange_orders_schema()).map_err(|_| {
        error(
            "COINBASE_CONTRACT_INVALID",
            "The Exchange order-list schema is invalid.",
        )
    })?;
    if !validator.is_valid(input) {
        return Err(error(
            "INVALID_INPUT",
            "Input does not match the closed Exchange order-list schema.",
        ));
    }
    if input.get("after").is_some() && input.get("before").is_some() {
        return Err(error(
            "INVALID_INPUT",
            "Choose an Exchange order cursor direction: after or before.",
        ));
    }
    let account_name = input["account"].as_str().expect("validated schema");
    let account = profile
        .accounts()
        .iter()
        .find(|a| a.name == account_name && a.product == CoinbaseProduct::Exchange)
        .ok_or_else(|| {
            error(
                "COINBASE_ACCOUNT_UNKNOWN",
                "Select an enrolled Coinbase Exchange profile.",
            )
        })?;
    if !account.identity_verified || account.physical_identity.trim().is_empty() {
        return Err(error(
            "COINBASE_IDENTITY_UNVERIFIED",
            "Exchange order history requires a verified profile identity.",
        ));
    }
    let query = CoinbaseQuery {
        profile_id: Some(account.physical_identity.clone()),
        product_id: input
            .get("productId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        status: input
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_owned),
        limit: input
            .get("limit")
            .and_then(Value::as_u64)
            .and_then(|n| u16::try_from(n).ok()),
        after: input
            .get("after")
            .and_then(Value::as_str)
            .map(str::to_owned),
        before: input
            .get("before")
            .and_then(Value::as_str)
            .map(str::to_owned),
        ..Default::default()
    };
    let first = CoinbaseRoute::ExchangeOrdersList {
        query: query.clone(),
    };
    read_all_exchange_pages(profile, account, "exchange_orders", query, first).await
}

fn international_transfers_schema() -> Value {
    json!({
        "type":"object",
        "required":["account"],
        "properties":{
            "account":{"type":"string","minLength":1,"maxLength":128},
            "timeFrom":{"type":"string","minLength":1,"maxLength":64},
            "timeTo":{"type":"string","minLength":1,"maxLength":64},
            "status":{"type":"string","enum":["PENDING","PROCESSED","FAILED","CANCELED"]},
            "type":{"type":"string","enum":["INTERNAL","DEPOSIT","WITHDRAWAL"]},
            "limit":{"type":"integer","minimum":1,"maximum":250},
            "offset":{"type":"integer","minimum":0,"maximum":1000000}
        },
        "additionalProperties":false
    })
}

async fn read_international_transfers(profile: &CoinbaseProfile, input: &Value) -> Result<Value> {
    let schema = international_transfers_schema();
    let validator = jsonschema::validator_for(&schema).map_err(|_| {
        error(
            "COINBASE_CONTRACT_INVALID",
            "The International transfer read schema is invalid.",
        )
    })?;
    if !validator.is_valid(input) {
        return Err(error(
            "INVALID_INPUT",
            "Input does not match the closed transfer history contract.",
        ));
    }
    let account_name = input["account"].as_str().expect("validated schema");
    let account = profile
        .accounts()
        .iter()
        .find(|account| {
            account.name == account_name && account.product == CoinbaseProduct::International
        })
        .ok_or_else(|| {
            error(
                "COINBASE_ACCOUNT_UNKNOWN",
                "Select an enrolled Coinbase International account.",
            )
        })?;
    if !account.identity_verified {
        return Err(error(
            "COINBASE_IDENTITY_UNVERIFIED",
            "International transfer history requires a verified enrolled account.",
        ));
    }
    let portfolio = account
        .portfolio_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            error(
                "COINBASE_PORTFOLIO_UNKNOWN",
                "The enrolled International portfolio identity is missing.",
            )
        })?;
    let query = CoinbaseQuery {
        transfer_type: input.get("type").and_then(Value::as_str).map(str::to_owned),
        status: input
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_owned),
        time_from: input
            .get("timeFrom")
            .and_then(Value::as_str)
            .map(str::to_owned),
        time_to: input
            .get("timeTo")
            .and_then(Value::as_str)
            .map(str::to_owned),
        result_limit: input
            .get("limit")
            .and_then(Value::as_u64)
            .and_then(|n| u16::try_from(n).ok()),
        result_offset: input
            .get("offset")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        portfolios: Some(portfolio.to_owned()),
        ..Default::default()
    };
    let response = profile
        .request_for_account(
            &account.name,
            CoinbaseProduct::International,
            CoinbaseRoute::InternationalTransfers { query },
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    if !(200..300).contains(&response.status) {
        return Err(error(
            "COINBASE_REQUEST_FAILED",
            "Coinbase rejected the International transfer history read.",
        ));
    }
    let value: Value = serde_json::from_slice(&response.body).map_err(|_| {
        error(
            "COINBASE_RESPONSE_INVALID",
            "Coinbase returned invalid transfer history JSON.",
        )
    })?;
    let rows = value
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            error(
                "COINBASE_RESPONSE_INVALID",
                "International transfer history omitted its results array.",
            )
        })?;
    for row in rows {
        if required_text(
            row,
            "transfer_uuid",
            "International transfer row omitted its native identity.",
        )
        .is_err()
        {
            return Err(error(
                "COINBASE_RESPONSE_INVALID",
                "International transfer row omitted its native identity.",
            ));
        }
        let belongs = ["from_portfolio", "to_portfolio"].iter().any(|side| {
            let portfolio = &row[*side];
            portfolio.get("uuid").and_then(Value::as_str)
                == Some(account.portfolio_id.as_deref().unwrap_or_default())
                || portfolio.get("id").and_then(Value::as_str)
                    == Some(account.portfolio_id.as_deref().unwrap_or_default())
        });
        if !belongs {
            return Err(error(
                "COINBASE_PORTFOLIO_MISMATCH",
                "International transfer row did not attest the enrolled portfolio.",
            ));
        }
    }
    Ok(value)
}

fn international_fills_schema() -> Value {
    json!({"type":"object","required":["account"],"properties":{"account":{"type":"string","minLength":1,"maxLength":128},"orderId":{"type":"string","minLength":1,"maxLength":128},"clientOrderId":{"type":"string","minLength":1,"maxLength":128},"limit":{"type":"integer","minimum":1,"maximum":100},"offset":{"type":"integer","minimum":0,"maximum":1000000},"refDatetime":{"type":"string","minLength":20,"maxLength":40},"timeFrom":{"type":"string","minLength":20,"maxLength":40}},"additionalProperties":false})
}

async fn read_international_fills(profile: &CoinbaseProfile, input: &Value) -> Result<Value> {
    let validator = jsonschema::validator_for(&international_fills_schema()).map_err(|_| {
        error(
            "COINBASE_CONTRACT_INVALID",
            "International fills schema is invalid.",
        )
    })?;
    if !validator.is_valid(input) {
        return Err(error(
            "INVALID_INPUT",
            "International fills input does not match its closed schema.",
        ));
    }
    let name = input["account"].as_str().expect("validated");
    let account = profile
        .accounts()
        .iter()
        .find(|a| a.name == name && a.product == CoinbaseProduct::International)
        .ok_or_else(|| {
            error(
                "COINBASE_ACCOUNT_UNKNOWN",
                "Select an enrolled International account.",
            )
        })?;
    if !account.identity_verified {
        return Err(error(
            "COINBASE_IDENTITY_UNVERIFIED",
            "International fills require verified enrollment.",
        ));
    }
    let portfolio = account
        .portfolio_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            error(
                "COINBASE_PORTFOLIO_UNKNOWN",
                "Enrolled International portfolio is missing.",
            )
        })?;
    let text = |field: &str| input.get(field).and_then(Value::as_str).map(str::to_owned);
    let query = CoinbaseQuery {
        order_id: text("orderId"),
        client_order_id: text("clientOrderId"),
        result_limit: input
            .get("limit")
            .and_then(Value::as_u64)
            .and_then(|n| u16::try_from(n).ok())
            .or(Some(100)),
        result_offset: input
            .get("offset")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        ref_datetime: text("refDatetime"),
        time_from: text("timeFrom"),
        ..Default::default()
    };
    let response = profile
        .request_for_account(
            name,
            CoinbaseProduct::International,
            CoinbaseRoute::InternationalPortfolioFillsFiltered {
                portfolio_id: portfolio.into(),
                query,
            },
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    if !(200..300).contains(&response.status) {
        return Err(error(
            "COINBASE_REQUEST_FAILED",
            "Coinbase rejected the International fills read.",
        ));
    }
    let value: Value = serde_json::from_slice(&response.body).map_err(|_| {
        error(
            "COINBASE_RESPONSE_INVALID",
            "International fills response is invalid JSON.",
        )
    })?;
    let rows = value
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            error(
                "COINBASE_RESPONSE_INVALID",
                "International fills response omitted results.",
            )
        })?;
    for row in rows {
        if row.get("portfolio_uuid").and_then(Value::as_str) != Some(portfolio) {
            return Err(error(
                "COINBASE_PORTFOLIO_MISMATCH",
                "International fill row did not attest the enrolled portfolio.",
            ));
        }
    }
    Ok(
        json!({"data":value,"pagination":{"after":response.pagination.after,"before":response.pagination.before}}),
    )
}

async fn read_all_exchange_pages(
    profile: &CoinbaseProfile,
    account: &CoinbaseAccount,
    route: &str,
    mut query: CoinbaseQuery,
    first_endpoint: CoinbaseEndpoint,
) -> Result<Value> {
    let mut endpoint = first_endpoint;
    let mut after = query.after.clone().or_else(|| query.starting_after.clone());
    let mut seen = std::collections::BTreeSet::new();
    let mut rows = Vec::new();
    for page_number in 0..1000 {
        let response = profile
            .request_for_account(
                &account.name,
                CoinbaseProduct::Exchange,
                endpoint,
                CoinbaseRequest::get(),
                None,
            )
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(error(
                "COINBASE_REQUEST_FAILED",
                "Coinbase rejected the institutional read.",
            ));
        }
        let body: Value = serde_json::from_slice(&response.body).map_err(|_| {
            error(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned invalid institutional JSON.",
            )
        })?;
        let Some(page_rows) = body.as_array() else {
            return Err(error(
                "COINBASE_RESPONSE_INVALID",
                "Exchange history response was not an array.",
            ));
        };
        for row in page_rows {
            if required_text(
                row,
                "profile_id",
                "Exchange history row omitted its profile identity.",
            )? != account.physical_identity
            {
                return Err(error(
                    "COINBASE_PROFILE_MISMATCH",
                    "Exchange history row did not belong to the enrolled profile.",
                ));
            }
            rows.push(row.clone());
        }
        let Some(next) = response
            .pagination
            .after
            .filter(|cursor| !cursor.is_empty())
        else {
            return Ok(json!({
                "data": rows,
                "pagination": {"after": null, "before": response.pagination.before}
            }));
        };
        if page_number == 999 || after.as_deref() == Some(&next) || !seen.insert(next.clone()) {
            return Err(error(
                "COINBASE_PAGINATION_INVALID",
                "Exchange history pagination did not advance safely.",
            ));
        }
        after = Some(next.clone());
        if route == "exchange_transfers" {
            query.starting_after = Some(next);
            endpoint = CoinbaseRoute::ExchangeTransfers {
                account_id: query.account_id.clone().ok_or_else(|| {
                    error(
                        "COINBASE_BINDING_INVALID",
                        "Transfer account identity is missing.",
                    )
                })?,
                query: query.clone(),
            };
        } else if route == "exchange_orders" {
            query.after = Some(next);
            endpoint = CoinbaseRoute::ExchangeOrdersList {
                query: query.clone(),
            };
        } else {
            query.after = Some(next);
            endpoint = CoinbaseRoute::ExchangeFillsFiltered {
                query: query.clone(),
            };
        }
    }
    Err(error(
        "COINBASE_PAGINATION_LIMIT",
        "Exchange history page limit was reached before the response was complete.",
    ))
}

pub async fn snapshot(
    profile: &CoinbaseProfile,
    scope: Option<&SnapshotScope>,
) -> Result<Option<StateSnapshot>> {
    let empty_scope = SnapshotScope::default();
    let scope = scope.unwrap_or(&empty_scope);
    let selected = profile
        .accounts()
        .iter()
        .filter(|account| {
            scope.matches_resource(&account.resource)
                && scope.matches_network(account.product.provider())
        })
        .collect::<Vec<_>>();
    if selected.is_empty()
        || selected
            .iter()
            .any(|account| account.product == CoinbaseProduct::Advanced)
    {
        return Ok(None);
    }

    let mut holdings = Vec::new();
    let mut coverage = Vec::new();
    let mut pending = Vec::new();
    let mut excluded_exposure = false;
    for account in &selected {
        if !account.identity_verified || account.physical_identity.trim().is_empty() {
            return Err(error(
                "COINBASE_IDENTITY_UNVERIFIED",
                "Institutional cash observation requires a verified enrolled profile or portfolio identity.",
            ));
        }
        match account.product {
            CoinbaseProduct::Exchange => {
                observe_exchange_cash(
                    profile,
                    account,
                    scope,
                    &mut holdings,
                    &mut coverage,
                    &mut excluded_exposure,
                )
                .await?;
            }
            CoinbaseProduct::International => {
                observe_international_cash(
                    profile,
                    account,
                    scope,
                    &mut holdings,
                    &mut coverage,
                    &mut excluded_exposure,
                )
                .await?;
            }
            CoinbaseProduct::Advanced => unreachable!("filtered above"),
        }
        coverage.push(account.resource.0.clone());
    }

    let cash_scope = !scope.is_full()
        && (!scope.networks.is_empty() || !scope.assets.is_empty())
        && selected
            .iter()
            .all(|account| scope.matches_network(account.product.provider()))
        && scope.networks.iter().all(|network| {
            selected
                .iter()
                .any(|account| account.product.provider() == network)
        })
        && scope.assets.iter().all(|asset| {
            selected
                .iter()
                .any(|account| account.product.provider() == asset.network)
        })
        && scope
            .resources
            .iter()
            .all(|resource| selected.iter().any(|account| &account.resource == resource));
    let asset_scope_observed = selected.iter().all(|account| {
        scope
            .assets
            .iter()
            .filter(|asset| asset.network == account.product.provider())
            .all(|asset| {
                holdings.iter().any(|holding| {
                    holding.resource == account.resource && holding.amount.asset == *asset
                })
            })
    });
    let complete = cash_scope && asset_scope_observed && !excluded_exposure;
    if complete {
        coverage.push("Requested Exchange/International cash identities were fully observed; positions, loans, collateral, and open derivative exposure are outside this cash-only scope.".into());
    } else if cash_scope && excluded_exposure {
        pending.push("The exact cash scope was observed, but native loan, collateral, or derivative exposure is present and is not represented as spendable cash.".into());
        coverage.push("Cash scope includes separately reported debt/collateral/position exposure; completeness is withheld because this observer does not model those as holdings.".into());
    } else if cash_scope {
        pending.push("At least one requested currency account was absent from the native cash enumeration; no zero balance was inferred.".into());
        coverage.push("Exchange/International cash enumeration completed, but requested currencies without exact account rows remain unobserved.".into());
    } else {
        pending.push("Exchange/International cash was observed, but unscoped debt, collateral, and position exposure is not represented as cash.".into());
        coverage.push("Exchange/International cash only; derivative positions, loans, margin, and collateral are not represented in holdings.".into());
    }
    Ok(Some(StateSnapshot {
        state: FinancialState {
            holdings,
            positions: vec![],
            authorities: vec![],
            in_flight: vec![],
        },
        observed_at: gofin_domain::now(),
        complete,
        coverage,
        pending,
    }))
}

async fn observe_exchange_cash(
    profile: &CoinbaseProfile,
    account: &CoinbaseAccount,
    scope: &SnapshotScope,
    holdings: &mut Vec<ResourceAmount>,
    coverage: &mut Vec<String>,
    excluded_exposure: &mut bool,
) -> Result<()> {
    let response = profile
        .request_for_account(
            &account.name,
            CoinbaseProduct::Exchange,
            CoinbaseRoute::ExchangeAccounts,
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    let rows = parse_required_json(response, "Exchange cash accounts could not be observed.")?;
    let rows = rows.as_array().ok_or_else(|| {
        error(
            "COINBASE_RESPONSE_INVALID",
            "Exchange accounts response was not an array.",
        )
    })?;
    let mut native_ids = std::collections::BTreeSet::new();
    for row in rows {
        if required_text(
            row,
            "profile_id",
            "Exchange account omitted its profile identity.",
        )? != account.physical_identity
        {
            return Err(error(
                "COINBASE_PROFILE_MISMATCH",
                "Exchange cash row did not attest the enrolled profile.",
            ));
        }
        let id = required_text(
            row,
            "id",
            "Exchange account omitted its currency-account UUID.",
        )?;
        if !native_ids.insert(id.to_owned()) {
            return Err(error(
                "COINBASE_ACCOUNT_IDENTITY_DUPLICATE",
                "Exchange returned a duplicate currency-account UUID.",
            ));
        }
        let currency = required_text(
            row,
            "currency",
            "Exchange account omitted its currency identity.",
        )?;
        let asset = provider_asset(CoinbaseProduct::Exchange, currency);
        if !scope.matches_asset(&asset) {
            continue;
        }
        let available = exact_native_quantity(
            row.get("available"),
            "Exchange account omitted exact available balance.",
        )?;
        let held = exact_native_quantity(
            row.get("hold"),
            "Exchange account omitted exact held balance.",
        )?;
        let balance = exact_native_quantity(
            row.get("balance"),
            "Exchange account omitted exact total balance.",
        )?;
        if available.checked_add(&held)? != balance {
            return Err(error(
                "COINBASE_BALANCE_INCONSISTENT",
                "Exchange available plus held balance did not equal its native total.",
            ));
        }
        holdings.push(ResourceAmount {
            resource: account.resource.clone(),
            amount: gofin_domain::Amount {
                asset,
                atoms: available.clone(),
            },
        });
        coverage.push(format!("coinbaseexchange:currency-account:{id}:currency:{currency}:available:{}:held:{}:total:{}", available.to_decimal(36), held.to_decimal(36), balance.to_decimal(36)));
    }
    // The Exchange accounts collection does not include portfolio loan or
    // collateral exposure. Its documented lending overview is profile-scoped;
    // retain the cash snapshot as incomplete whenever that extra exposure is
    // active or the overview cannot prove it absent.
    let lending = profile
        .request_for_account(
            &account.name,
            CoinbaseProduct::Exchange,
            CoinbaseRoute::ExchangeLoanLendingOverview,
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    let lending = parse_required_json(lending, "Exchange lending exposure could not be verified.")?;
    let overview = lending
        .get("overview")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            error(
                "COINBASE_RESPONSE_INVALID",
                "Exchange lending overview omitted its overview object.",
            )
        })?;
    let loans = lending
        .get("loans")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            error(
                "COINBASE_RESPONSE_INVALID",
                "Exchange lending overview omitted its loans array.",
            )
        })?;
    let mut exposure_present = !loans.is_empty()
        || overview
            .get("withdrawal_restricted")
            .and_then(Value::as_bool)
            != Some(false);
    if let Some(collateral) = overview.get("collateral_value") {
        exposure_present |=
            !exact_native_quantity(Some(collateral), "Exchange collateral value was malformed.")?
                .is_zero();
    } else {
        exposure_present = true;
    }
    if exposure_present {
        *excluded_exposure = true;
        coverage.push(format!(
            "coinbaseexchange:profile:{}:unmodeled_lending_or_collateral_exposure:loans:{}",
            account.physical_identity,
            loans.len()
        ));
    } else {
        coverage.push(format!(
            "coinbaseexchange:profile:{}:lending_overview_verified_no_active_exposure",
            account.physical_identity
        ));
    }
    Ok(())
}

async fn observe_international_cash(
    profile: &CoinbaseProfile,
    account: &CoinbaseAccount,
    scope: &SnapshotScope,
    holdings: &mut Vec<ResourceAmount>,
    coverage: &mut Vec<String>,
    excluded_exposure: &mut bool,
) -> Result<()> {
    let portfolio = account
        .portfolio_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            error(
                "COINBASE_PORTFOLIO_MISSING",
                "International cash observation requires an enrolled portfolio ID.",
            )
        })?;
    if portfolio != account.physical_identity {
        return Err(error(
            "COINBASE_PORTFOLIO_MISMATCH",
            "International portfolio ID differs from its enrolled physical identity.",
        ));
    }
    let listed = profile
        .request_for_account(
            &account.name,
            CoinbaseProduct::International,
            CoinbaseRoute::InternationalAccounts,
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    let listed = parse_required_json(
        listed,
        "International portfolios could not be listed for identity verification.",
    )?;
    let portfolio_rows = listed
        .get("portfolios")
        .and_then(Value::as_array)
        .or_else(|| listed.as_array())
        .ok_or_else(|| {
            error(
                "COINBASE_RESPONSE_INVALID",
                "International portfolio list omitted its portfolios array.",
            )
        })?;
    let matches = portfolio_rows
        .iter()
        .filter(|row| row.get("portfolio_id").and_then(Value::as_str) == Some(portfolio))
        .count();
    if matches != 1 {
        return Err(error(
            "COINBASE_PORTFOLIO_MISMATCH",
            "International portfolio list did not return exactly one enrolled portfolio ID.",
        ));
    }
    let response = profile
        .request_for_account(
            &account.name,
            CoinbaseProduct::International,
            CoinbaseRoute::InternationalPortfolioDetail {
                portfolio_id: portfolio.into(),
            },
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    let detail = parse_required_json(
        response,
        "International portfolio cash detail could not be observed.",
    )?;
    for key in ["portfolio_id", "portfolio_uuid", "portfolio"] {
        if detail
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|id| id != portfolio)
        {
            return Err(error(
                "COINBASE_PORTFOLIO_MISMATCH",
                "International detail response did not match the enrolled portfolio.",
            ));
        }
    }
    let rows = detail
        .get("balances")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            error(
                "COINBASE_RESPONSE_INVALID",
                "International portfolio detail omitted its balances array.",
            )
        })?;
    let positions = detail
        .get("positions")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            error(
                "COINBASE_RESPONSE_INVALID",
                "International portfolio detail omitted its positions array.",
            )
        })?;
    if !positions.is_empty() {
        *excluded_exposure = true;
        coverage.push(format!(
            "coinbaseinternational:portfolio:{portfolio}:unmodeled_derivative_positions:{}",
            positions.len()
        ));
    }
    let summary = detail
        .get("summary")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            error(
                "COINBASE_RESPONSE_INVALID",
                "International portfolio detail omitted its debt summary fields.",
            )
        })?;
    for key in ["borrow", "accrued_interest", "rolling_debt"] {
        if let Some(value) = summary.get(key) {
            let amount = exact_native_quantity(
                Some(value),
                "International portfolio debt summary was malformed.",
            )?;
            if !amount.is_zero() {
                *excluded_exposure = true;
                coverage.push(format!(
                    "coinbaseinternational:portfolio:{portfolio}:unmodeled_{key}:{}",
                    amount.to_decimal(36)
                ));
            }
        } else {
            *excluded_exposure = true;
            coverage.push(format!(
                "coinbaseinternational:portfolio:{portfolio}:unobserved_{key}"
            ));
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    for row in rows {
        let uuid = required_text(
            row,
            "asset_uuid",
            "International balance omitted its native asset UUID.",
        )?;
        let native_id = required_text(
            row,
            "asset_id",
            "International balance omitted its native asset ID.",
        )?;
        let symbol = required_text(
            row,
            "asset_name",
            "International balance omitted its asset symbol.",
        )?;
        if !seen.insert(uuid.to_owned()) {
            return Err(error(
                "COINBASE_ASSET_IDENTITY_DUPLICATE",
                "International portfolio returned a duplicate native asset UUID.",
            ));
        }
        let pledged = exact_native_quantity(
            row.get("pledged_collateral_quantity"),
            "International balance omitted exact pledged collateral.",
        )?;
        let loan = exact_native_quantity(
            row.get("loan"),
            "International balance omitted exact loan amount.",
        )?;
        if !loan.is_zero() || !pledged.is_zero() {
            *excluded_exposure = true;
            coverage.push(format!(
                "coinbaseinternational:portfolio:{portfolio}:asset:{native_id}:unmodeled_loan_or_pledged_collateral:loan:{}:pledged:{}",
                loan.to_decimal(36),
                pledged.to_decimal(36)
            ));
        }
        let asset = provider_asset(CoinbaseProduct::International, symbol);
        if !scope.matches_asset(&asset) {
            continue;
        }
        let total = exact_native_quantity(
            row.get("quantity"),
            "International balance omitted exact total quantity.",
        )?;
        let hold = exact_native_quantity(
            row.get("hold"),
            "International balance omitted exact trading hold.",
        )?;
        let transfer_hold = exact_native_quantity(
            row.get("transfer_hold"),
            "International balance omitted exact transfer hold.",
        )?;
        let unavailable = hold.checked_add(&transfer_hold)?.checked_add(&pledged)?;
        let available = total.checked_sub(&unavailable).map_err(|_| {
            error(
                "COINBASE_BALANCE_INCONSISTENT",
                "International holds and pledged collateral exceed native quantity.",
            )
        })?;
        holdings.push(ResourceAmount {
            resource: account.resource.clone(),
            amount: gofin_domain::Amount {
                asset,
                atoms: available.clone(),
            },
        });
        coverage.push(format!("coinbaseinternational:portfolio:{portfolio}:asset:{native_id}:asset_uuid:{uuid}:symbol:{symbol}:available:{}:hold:{}:transfer_hold:{}:pledged:{}:loan_liability:{}:total:{}", available.to_decimal(36), hold.to_decimal(36), transfer_hold.to_decimal(36), pledged.to_decimal(36), loan.to_decimal(36), total.to_decimal(36)));
    }
    Ok(())
}

fn provider_asset(product: CoinbaseProduct, reference: &str) -> gofin_domain::AssetId {
    gofin_domain::AssetId {
        network: product.provider().into(),
        reference: reference.into(),
        decimals: 36,
    }
}

fn exact_native_quantity(value: Option<&Value>, message: &str) -> Result<Quantity> {
    let raw = match value {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Number(value)) => value.to_string(),
        _ => return Err(error("COINBASE_RESPONSE_INVALID", message)),
    };
    let amount = Quantity::from_decimal(&raw, 36)
        .map_err(|_| error("COINBASE_RESPONSE_INVALID", message))?;
    if raw.starts_with('-') {
        return Err(error(
            "COINBASE_RESPONSE_INVALID",
            "Coinbase balance quantities must not be negative.",
        ));
    }
    Ok(amount)
}

/// No institutional operation is currently allowed to reuse an Advanced
/// preparation. Institutional preparation validates native metadata locally.
pub async fn prepare(
    profile: &CoinbaseProfile,
    operation: &str,
    input: &Value,
) -> Result<Option<PreparedOperation>> {
    if let Some(prepared) = administration::prepare(profile, operation, input).await? {
        return Ok(Some(prepared));
    }
    if let Some(prepared) = operations::prepare(profile, operation, input).await? {
        return Ok(Some(prepared));
    }
    if operation == "coinbaseexchange.create-balance-report" {
        return Ok(Some(prepare_exchange_balance_report(profile, input)?));
    }
    if operation == "coinbaseexchange.create-account-report" {
        return Ok(Some(
            prepare_exchange_history_report(profile, input, "account").await?,
        ));
    }
    if operation == "coinbaseexchange.create-fills-report" {
        return Ok(Some(
            prepare_exchange_history_report(profile, input, "fills").await?,
        ));
    }
    if let Some(report_type) = exchange_history_report_type(operation)
        && matches!(report_type, "otc-fills" | "rfq-fills" | "tax-invoice")
    {
        return Ok(Some(
            prepare_exchange_history_report(profile, input, report_type).await?,
        ));
    }
    if operation == "coinbaseexchange.cancel-order" {
        return Ok(Some(prepare_exchange_cancel(profile, input).await?));
    }
    if operation == "coinbaseinternational.create-order" {
        return Ok(Some(
            prepare_international_order(profile, input, false).await?,
        ));
    }
    if operation == "coinbaseinternational.cancel-order" {
        return Ok(Some(prepare_international_cancel(profile, input).await?));
    }
    if operation == "coinbaseinternational.edit-order" {
        return Err(error(
            "COINBASE_LINKED_CONTEXT_REQUIRED",
            "International edits require protected linked-order context.",
        ));
    }
    if operation != "coinbaseexchange.create-order" {
        return Ok(None);
    }
    Ok(Some(prepare_exchange_order(profile, input, false).await?))
}

/// Prepare an institutional spot order for a named plan whose later leg may
/// fund it. All metadata, identity, fee, and route checks remain identical to
/// ordinary preparation; only a positively observed insufficient balance is
/// deferred to plan-level authority and execution-time revalidation.
pub async fn prepare_order_for_plan(
    profile: &CoinbaseProfile,
    operation: &str,
    input: &Value,
) -> Result<Option<PreparedOperation>> {
    match operation {
        "coinbaseexchange.create-order" => {
            Ok(Some(prepare_exchange_order(profile, input, true).await?))
        }
        "coinbaseinternational.create-order" => Ok(Some(
            prepare_international_order(profile, input, true).await?,
        )),
        _ => Ok(None),
    }
}

fn prepare_exchange_balance_report(
    profile: &CoinbaseProfile,
    input: &Value,
) -> Result<PreparedOperation> {
    let validator = jsonschema::validator_for(&exchange_balance_report_schema()).map_err(|_| {
        error(
            "COINBASE_CONTRACT_INVALID",
            "The balance-report schema is invalid.",
        )
    })?;
    if !validator.is_valid(input) {
        return Err(error(
            "INVALID_INPUT",
            "Balance-report input does not match its closed schema.",
        ));
    }
    let account = verified_exchange_account(profile, input["account"].as_str().expect("schema"))?;
    let report_at = input["reportAt"].as_str().expect("schema");
    let timestamp = chrono::DateTime::parse_from_rfc3339(report_at).map_err(|_| {
        error(
            "INVALID_INPUT",
            "reportAt must be an RFC 3339 timestamp with an explicit offset.",
        )
    })?;
    if timestamp.with_timezone(&chrono::Utc) > chrono::Utc::now() {
        return Err(error(
            "INVALID_INPUT",
            "A historical balance report timestamp cannot be in the future.",
        ));
    }
    let format = input["format"].as_str().expect("schema");
    let body = gofin_domain::canonical_json(&json!({
        "type":"balance",
        "format":format,
        "profile_id":account.physical_identity,
        "balance":{"datetime":report_at}
    }))?;
    let prepared = PreparedOperation {
        operation: "coinbaseexchange.create-balance-report".into(),
        input: input.clone(),
        binding: json!({"version":1,"provider":"coinbaseexchange","account":account.name,
            "resource":account.resource,"physicalIdentity":account.physical_identity,
            "requestId":input["requestId"],"reportAt":report_at,"format":format,"bodyExact":body}),
        summary: format!("Request a Coinbase Exchange {format} balance report at {report_at}"),
        checks: vec![Check {
            name: "exchange_profile_report_scope".into(),
            status: CheckStatus::Passed,
            detail: "The request is bound to the enrolled Exchange profile and an exact historical timestamp.".into(),
        }],
        warnings: vec!["Report generation is a profile-level configuration action; the resulting file URL is metadata only and Fin will not fetch it.".into()],
        observed_at: gofin_domain::now(),
        expires_at: (chrono::Utc::now() + chrono::Duration::seconds(45)).to_rfc3339(),
        valuation: None,
        intent: FinancialIntent { debits: vec![], maximum_costs: vec![], expected: vec![], locks: vec![] },
        operation_version: 1,
        action_deadline: None,
    };
    Ok(prepared)
}

/// Linked-order preparation is invoked by the profile when the broker supplies
/// the protected amendment context. Caller input alone never selects a parent.
pub async fn prepare_with_linked_context(
    profile: &CoinbaseProfile,
    operation: &str,
    input: &Value,
    linked: &gofin_domain::amendments::AmendmentContext,
) -> Result<Option<PreparedOperation>> {
    if operation != "coinbaseinternational.edit-order" {
        return Ok(None);
    }
    Ok(Some(
        prepare_international_edit(profile, input, linked).await?,
    ))
}

pub async fn refresh(
    profile: &CoinbaseProfile,
    approved: &PreparedOperation,
) -> Result<Option<PreparedOperation>> {
    if let Some(fresh) = administration::refresh(profile, approved).await? {
        return Ok(Some(fresh));
    }
    if let Some(fresh) = operations::refresh(profile, approved).await? {
        return Ok(Some(fresh));
    }
    if approved.operation == "coinbaseexchange.create-balance-report"
        || approved.operation == "coinbaseexchange.create-account-report"
        || approved.operation == "coinbaseexchange.create-fills-report"
        || exchange_history_report_type(&approved.operation).is_some()
    {
        let fresh = match approved.operation.as_str() {
            "coinbaseexchange.create-balance-report" => {
                prepare_exchange_balance_report(profile, &approved.input)?
            }
            "coinbaseexchange.create-account-report" => {
                prepare_exchange_history_report(profile, &approved.input, "account").await?
            }
            "coinbaseexchange.create-fills-report" => {
                prepare_exchange_history_report(profile, &approved.input, "fills").await?
            }
            _ => {
                let report_type =
                    exchange_history_report_type(&approved.operation).expect("matched above");
                prepare_exchange_history_report(profile, &approved.input, report_type).await?
            }
        };
        if fresh.input != approved.input || fresh.binding != approved.binding {
            return Err(error(
                "COINBASE_PREVIEW_CHANGED",
                "The enrolled Exchange profile or exact report request changed after approval.",
            ));
        }
        return Ok(Some(fresh));
    }
    if approved.operation == "coinbaseinternational.cancel-order" {
        let fresh = prepare_international_cancel(profile, &approved.input).await?;
        let immutable = [
            "account",
            "physicalIdentity",
            "portfolioId",
            "orderId",
            "clientOrderId",
            "instrument",
            "operationId",
            "originalSubmission",
        ];
        if fresh.input != approved.input
            || immutable
                .iter()
                .any(|field| fresh.binding[*field] != approved.binding[*field])
        {
            return Err(error(
                "COINBASE_PREVIEW_CHANGED",
                "The International cancel target or journal parent changed after approval.",
            ));
        }
        return Ok(Some(fresh));
    }
    if approved.operation == "coinbaseinternational.edit-order" {
        return Ok(Some(refresh_international_edit(profile, approved).await?));
    }
    if approved.operation == "coinbaseinternational.create-order" {
        let mut fresh = prepare_international_order(profile, &approved.input, false).await?;
        let immutable = [
            "account",
            "portfolioId",
            "physicalIdentity",
            "clientOrderId",
            "instrument",
            "baseAssetId",
            "quoteAssetId",
            "baseSize",
            "limitPrice",
            "requestBodyExact",
        ];
        if fresh.input != approved.input
            || immutable
                .iter()
                .any(|field| fresh.binding[*field] != approved.binding[*field])
        {
            return Err(error(
                "COINBASE_PREVIEW_CHANGED",
                "International portfolio, instrument, fee cap or exact order body changed after approval.",
            ));
        }
        gofin_domain::value_bounds::carry(approved, &mut fresh)?;
        return Ok(Some(fresh));
    }
    if approved.operation == "coinbaseexchange.cancel-order" {
        let fresh = prepare_exchange_cancel(profile, &approved.input).await?;
        let fields = [
            "account",
            "physicalIdentity",
            "orderId",
            "clientOrderId",
            "productId",
            "operationId",
            "originalSubmission",
        ];
        if fresh.input != approved.input
            || fields
                .iter()
                .any(|field| fresh.binding[*field] != approved.binding[*field])
        {
            return Err(error(
                "COINBASE_PREVIEW_CHANGED",
                "The Exchange cancellation target or original operation changed after approval.",
            ));
        }
        return Ok(Some(fresh));
    }
    if approved.operation != "coinbaseexchange.create-order" {
        return Ok(None);
    }
    let mut fresh = prepare_exchange_order(profile, &approved.input, false).await?;
    let immutable = [
        "account",
        "physicalIdentity",
        "clientOrderId",
        "productId",
        "baseCurrencyId",
        "quoteCurrencyId",
        "baseSize",
        "limitPrice",
        "requestBodyExact",
    ];
    if fresh.input != approved.input
        || immutable
            .iter()
            .any(|field| fresh.binding[*field] != approved.binding[*field])
    {
        return Err(error(
            "COINBASE_PREVIEW_CHANGED",
            "Exchange product, account, fee bound, or exact order body changed after approval.",
        ));
    }
    gofin_domain::value_bounds::carry(approved, &mut fresh)?;
    Ok(Some(fresh))
}

pub async fn plan_submission(
    profile: &CoinbaseProfile,
    current: &PreparedOperation,
) -> Result<Option<PlannedSubmission<CoinbaseDispatch>>> {
    if let Some(submission) = administration::plan_submission(profile, current)? {
        return Ok(Some(submission));
    }
    if let Some(submission) = operations::plan_submission(profile, current).await? {
        return Ok(Some(submission));
    }
    if current.operation == "coinbaseexchange.create-balance-report"
        || exchange_history_report_type(&current.operation).is_some()
    {
        if let Some(report_type) = exchange_history_report_type(&current.operation) {
            let fresh =
                prepare_exchange_history_report(profile, &current.input, report_type).await?;
            if fresh.input != current.input || fresh.binding != current.binding {
                return Err(error(
                    "COINBASE_PREVIEW_CHANGED",
                    "The enrolled profile, native account/product, or exact report request changed before submission.",
                ));
            }
        }
        let account = current.binding["account"].as_str().ok_or_else(|| {
            error(
                "COINBASE_BINDING_INVALID",
                "Prepared report profile is missing.",
            )
        })?;
        let configured = verified_exchange_account(profile, account)?;
        if current.binding["physicalIdentity"].as_str()
            != Some(configured.physical_identity.as_str())
            || current.binding["resource"] != json!(configured.resource)
        {
            return Err(error(
                "COINBASE_PROFILE_MISMATCH",
                "Exchange profile identity changed after report approval.",
            ));
        }
        let body = current.binding["requestBodyExact"]
            .as_str()
            .or_else(|| current.binding["bodyExact"].as_str())
            .ok_or_else(|| {
                error(
                    "COINBASE_BINDING_INVALID",
                    "Exact report request is missing.",
                )
            })?
            .to_owned();
        let digest = hex::encode(sha2::Sha256::digest(body.as_bytes()));
        let report_type = current.binding["reportType"].as_str().unwrap_or("balance");
        let report_operation = report_operation_name(report_type);
        let submission = Submission {
            kind: "coinbase".into(),
            reference: format!("report-request:{digest}"),
            data: json!({"product":"coinbaseexchange","operation":report_operation,"requestId":current.binding["requestId"],"profileId":current.binding["physicalIdentity"],"bodySha256":digest,"binding":current.binding}),
        };
        return Ok(Some(PlannedSubmission {
            submission: submission.clone(),
            request: CoinbaseDispatch {
                product: CoinbaseProduct::Exchange,
                endpoint: CoinbaseRoute::ExchangeReportCreate,
                account: account.into(),
                exact_body: body,
                submission,
                candidate: current.clone(),
            },
        }));
    }
    if current.operation == "coinbaseinternational.cancel-order" {
        return Ok(Some(plan_international_cancel(profile, current).await?));
    }
    if current.operation == "coinbaseinternational.edit-order" {
        return Ok(Some(plan_international_edit(profile, current).await?));
    }
    if current.operation == "coinbaseinternational.create-order" {
        let client_order_id = current.binding["clientOrderId"].as_str().ok_or_else(|| {
            error(
                "COINBASE_BINDING_INVALID",
                "International client order ID is missing.",
            )
        })?;
        profile.assert_unique_client_order_id(
            CoinbaseProduct::International,
            client_order_id,
            None,
        )?;
        return Ok(Some(plan_international_order(profile, current).await?));
    }
    if current.operation == "coinbaseexchange.cancel-order" {
        return Ok(Some(plan_exchange_cancel(profile, current).await?));
    }
    if current.operation != "coinbaseexchange.create-order" {
        return Ok(None);
    }
    let account = current.binding["account"].as_str().ok_or_else(|| {
        error(
            "COINBASE_BINDING_INVALID",
            "Prepared Exchange account identity is missing.",
        )
    })?;
    let configured = profile
        .accounts()
        .iter()
        .find(|a| a.name == account && a.product == CoinbaseProduct::Exchange)
        .ok_or_else(|| {
            error(
                "COINBASE_ACCOUNT_UNKNOWN",
                "The prepared Exchange account is no longer enrolled.",
            )
        })?;
    if !configured.identity_verified
        || current.binding["physicalIdentity"].as_str()
            != Some(configured.physical_identity.as_str())
    {
        return Err(error(
            "COINBASE_PROFILE_MISMATCH",
            "The Exchange profile identity changed after approval.",
        ));
    }
    let client_oid = current.binding["clientOrderId"].as_str().ok_or_else(|| {
        error(
            "COINBASE_BINDING_INVALID",
            "The client order ID is missing.",
        )
    })?;
    profile.assert_unique_client_order_id(CoinbaseProduct::Exchange, client_oid, None)?;
    let submission = Submission {
        kind: "coinbase".into(),
        reference: client_oid.into(),
        data: json!({"product":"coinbaseexchange","clientOrderId":client_oid,"account":account,"productId":current.binding["productId"]}),
    };
    Ok(Some(PlannedSubmission {
        submission: submission.clone(),
        request: CoinbaseDispatch {
            product: CoinbaseProduct::Exchange,
            endpoint: CoinbaseRoute::ExchangeOrders,
            account: account.into(),
            exact_body: current.binding["requestBodyExact"]
                .as_str()
                .ok_or_else(|| {
                    error(
                        "COINBASE_BINDING_INVALID",
                        "The exact Exchange order body is missing.",
                    )
                })?
                .into(),
            submission,
            candidate: current.clone(),
        },
    }))
}

pub async fn reconcile(
    profile: &CoinbaseProfile,
    record: &OperationRecord,
    submission: &Submission,
) -> Result<Option<Reconciliation>> {
    if let Some(reconciliation) = administration::reconcile(profile, record, submission).await? {
        return Ok(Some(reconciliation));
    }
    if let Some(reconciliation) = operations::reconcile(profile, record, submission).await? {
        return Ok(Some(reconciliation));
    }
    if record.operation == "coinbaseexchange.create-balance-report"
        || exchange_history_report_type(&record.operation).is_some()
    {
        return Ok(Some(
            reconcile_exchange_report(profile, record, submission).await?,
        ));
    }
    if record.operation == "coinbaseinternational.cancel-order" {
        return Ok(Some(
            reconcile_international_cancel(profile, record, submission).await?,
        ));
    }
    if record.operation == "coinbaseinternational.edit-order" {
        return Ok(Some(
            reconcile_international_edit(profile, record, submission).await?,
        ));
    }
    if record.operation == "coinbaseinternational.create-order" {
        return Ok(Some(
            reconcile_international_order(profile, record, submission).await?,
        ));
    }
    if record.operation == "coinbaseexchange.cancel-order" {
        return Ok(Some(
            reconcile_exchange_cancel(profile, record, submission).await?,
        ));
    }
    if record.operation != "coinbaseexchange.create-order" {
        return Ok(None);
    }
    Ok(Some(
        reconcile_exchange_order(profile, record, submission).await?,
    ))
}

fn cancel_exchange_order_schema() -> Value {
    json!({
        "type":"object",
        "required":["account","requestId","orderId","clientOrderId","productId"],
        "properties":{
            "account":{"type":"string","minLength":1,"maxLength":128},
            "requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},
            "orderId":{"type":"string","minLength":1,"maxLength":128},
            "clientOrderId":{"type":"string","minLength":1,"maxLength":128},
            "productId":{"type":"string","pattern":"^[A-Za-z0-9_-]{1,128}$"}
        },
        "additionalProperties":false
    })
}

async fn prepare_exchange_cancel(
    profile: &CoinbaseProfile,
    input: &Value,
) -> Result<PreparedOperation> {
    let validator = jsonschema::validator_for(&cancel_exchange_order_schema()).map_err(|_| {
        error(
            "COINBASE_CONTRACT_INVALID",
            "The Exchange cancellation schema is invalid.",
        )
    })?;
    if !validator.is_valid(input) {
        return Err(error(
            "INVALID_INPUT",
            "Exchange cancellation input does not match its closed contract.",
        ));
    }
    let account_name = input["account"].as_str().expect("schema");
    let account = profile
        .accounts()
        .iter()
        .find(|a| a.name == account_name && a.product == CoinbaseProduct::Exchange)
        .ok_or_else(|| {
            error(
                "COINBASE_ACCOUNT_UNKNOWN",
                "Select an enrolled Coinbase Exchange profile.",
            )
        })?;
    if !account.identity_verified || account.physical_identity.trim().is_empty() {
        return Err(error(
            "COINBASE_IDENTITY_UNVERIFIED",
            "Exchange cancellation requires a verified profile identity.",
        ));
    }
    let (order_id, client_oid, product_id) = (
        input["orderId"].as_str().expect("schema"),
        input["clientOrderId"].as_str().expect("schema"),
        input["productId"].as_str().expect("schema"),
    );
    let response = profile
        .request_for_account(
            account_name,
            CoinbaseProduct::Exchange,
            CoinbaseRoute::ExchangeOrder {
                order_id: order_id.into(),
            },
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    if !(200..300).contains(&response.status) {
        return Err(error(
            "COINBASE_ORDER_UNAVAILABLE",
            "The exact Exchange order cannot be verified for cancellation.",
        ));
    }
    let doc = parse_required_json(response, "Exchange order identity could not be verified.")?;
    let order = doc.get("order").unwrap_or(&doc);
    if order
        .get("id")
        .or_else(|| order.get("order_id"))
        .and_then(Value::as_str)
        != Some(order_id)
        || order.get("client_oid").and_then(Value::as_str) != Some(client_oid)
        || order.get("product_id").and_then(Value::as_str) != Some(product_id)
        || order.get("profile_id").and_then(Value::as_str)
            != Some(account.physical_identity.as_str())
    {
        return Err(error(
            "COINBASE_ORDER_IDENTITY_MISMATCH",
            "The order does not match its bound ID, client ID, product, and profile.",
        ));
    }
    let status = order
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(status.as_str(), "open" | "pending" | "active") {
        return Err(error(
            "COINBASE_ORDER_NOT_OPEN",
            "The Exchange order has no verified open remainder.",
        ));
    }
    let filled_before = required_text(
        order,
        "filled_size",
        "Exchange order omitted its cumulative filled size.",
    )?;
    let _ = Quantity::from_decimal(filled_before, 36)?;
    let original = Submission {
        kind: "coinbase".into(),
        reference: client_oid.into(),
        data: json!({"product":"coinbaseexchange","clientOrderId":client_oid,"account":account_name,"productId":product_id}),
    };
    let originals = profile
        .operations_by_prepared_binding(
            "coinbaseexchange.create-order",
            "/clientOrderId",
            &json!(client_oid),
        )?
        .into_iter()
        .filter(|record| {
            record.submission.as_ref() == Some(&original)
                && record.preview.as_ref().is_some_and(|p| {
                    p.operation == "coinbaseexchange.create-order"
                        && p.binding["account"] == account_name
                        && p.binding["physicalIdentity"] == account.physical_identity
                        && p.binding["productId"] == product_id
                        && p.binding["clientOrderId"] == client_oid
                })
                && matches!(
                    record.status,
                    Status::Submitted | Status::Pending | Status::Unknown
                )
        })
        .collect::<Vec<_>>();
    if originals.len() != 1 {
        return Err(error(
            "COINBASE_ORIGINAL_OPERATION_UNVERIFIED",
            "The protected Fin journal does not identify exactly one unresolved original order.",
        ));
    }
    let parent_id = originals[0].id.clone();
    let binding = json!({"version":1,"provider":"coinbaseexchange","account":account.name,"resource":account.resource,"physicalIdentity":account.physical_identity,"orderId":order_id,"clientOrderId":client_oid,"productId":product_id,"filledSizeBefore":filled_before,"operationId":parent_id,"originalSubmission":original,"requestBodyExact":""});
    Ok(PreparedOperation {
        operation:"coinbaseexchange.cancel-order".into(), input:input.clone(), binding,
        summary:format!("Cancel the open remainder of Exchange order {order_id}; fills remain with the original operation."),
        checks:vec![Check{name:"original_order_identity".into(),status:CheckStatus::Passed,detail:"Exact order, client ID, product, enrolled profile, and unresolved journal parent were verified.".into()}],
        warnings:vec!["Fills may race cancellation; reconcile the cancellation and original order before acting again.".into()],
        observed_at:gofin_domain::now(), expires_at:(chrono::Utc::now()+chrono::Duration::seconds(45)).to_rfc3339(), valuation:None,
        intent:FinancialIntent{debits:vec![],maximum_costs:vec![],expected:vec![EffectConstraint::SubmissionCancellation{operation_id:parent_id,submission:original}],locks:vec![account.resource.clone()]},
        operation_version:1, action_deadline:None,
    })
}

async fn plan_exchange_cancel(
    profile: &CoinbaseProfile,
    current: &PreparedOperation,
) -> Result<PlannedSubmission<CoinbaseDispatch>> {
    let account_name = current.binding["account"].as_str().ok_or_else(|| {
        error(
            "COINBASE_BINDING_INVALID",
            "Cancellation account binding is missing.",
        )
    })?;
    let account = profile
        .accounts()
        .iter()
        .find(|a| a.name == account_name && a.product == CoinbaseProduct::Exchange)
        .ok_or_else(|| {
            error(
                "COINBASE_ACCOUNT_UNKNOWN",
                "The prepared Exchange profile is no longer enrolled.",
            )
        })?;
    if !account.identity_verified
        || current.binding["physicalIdentity"].as_str() != Some(account.physical_identity.as_str())
    {
        return Err(error(
            "COINBASE_PROFILE_MISMATCH",
            "The enrolled Exchange profile changed after approval.",
        ));
    }
    let order_id = current.binding["orderId"].as_str().ok_or_else(|| {
        error(
            "COINBASE_BINDING_INVALID",
            "Cancellation order ID is missing.",
        )
    })?;
    let client_oid = current.binding["clientOrderId"].as_str().ok_or_else(|| {
        error(
            "COINBASE_BINDING_INVALID",
            "Cancellation client ID is missing.",
        )
    })?;
    let product_id = current.binding["productId"].as_str().ok_or_else(|| {
        error(
            "COINBASE_BINDING_INVALID",
            "Cancellation product ID is missing.",
        )
    })?;
    let original: Submission =
        serde_json::from_value(current.binding["originalSubmission"].clone()).map_err(|_| {
            error(
                "COINBASE_BINDING_INVALID",
                "Original order submission binding is invalid.",
            )
        })?;
    let reference = stable_client_oid(
        &account.physical_identity,
        &format!(
            "cancel:{}",
            current.input["requestId"].as_str().unwrap_or_default()
        ),
    );
    let submission = Submission {
        kind: "coinbase".into(),
        reference,
        data: json!({"product":"coinbaseexchange","account":account_name,"orderId":order_id,"clientOrderId":client_oid,"productId":product_id,"binding":current.binding}),
    };
    let _ = original;
    Ok(PlannedSubmission {
        submission: submission.clone(),
        request: CoinbaseDispatch {
            product: CoinbaseProduct::Exchange,
            endpoint: CoinbaseRoute::ExchangeCancelOrderScoped {
                order_id: order_id.into(),
                product_id: product_id.into(),
                profile_id: account.physical_identity.clone(),
            },
            account: account_name.into(),
            exact_body: String::new(),
            submission,
            candidate: current.clone(),
        },
    })
}

async fn reconcile_exchange_cancel(
    profile: &CoinbaseProfile,
    record: &OperationRecord,
    submission: &Submission,
) -> Result<Reconciliation> {
    let unknown = |reason: &str| Reconciliation {
        status: Status::Unknown,
        evidence: json!({"reason":reason,"replay":"forbidden_without_conclusive_reconciliation"}),
        verified: None,
    };
    let binding = &submission.data["binding"];
    if record.submission.as_ref() != Some(submission)
        || record
            .preview
            .as_ref()
            .is_none_or(|p| p.operation != "coinbaseexchange.cancel-order" || p.binding != *binding)
        || submission.kind != "coinbase"
        || submission.data["product"] != "coinbaseexchange"
    {
        return Ok(unknown(
            "durable cancellation record differs from its exact prepared binding",
        ));
    }
    let Some(dispatch_status) = record
        .dispatch_evidence
        .as_ref()
        .and_then(|evidence| evidence.get("httpStatus"))
        .and_then(Value::as_u64)
    else {
        return Ok(unknown(
            "the cancellation dispatch receipt is missing; do not replay",
        ));
    };
    if !(200..300).contains(&(dispatch_status as u16)) {
        return Ok(unknown(
            "the cancellation request was not acknowledged by Exchange",
        ));
    }
    let dispatch = record.dispatch_evidence.as_ref().expect("checked above");
    let (Some(body_hex), Some(body_hash)) = (
        dispatch.get("bodyHex").and_then(Value::as_str),
        dispatch.get("bodySha256").and_then(Value::as_str),
    ) else {
        return Ok(unknown(
            "the exact cancellation response receipt is incomplete",
        ));
    };
    let body_bytes = match hex::decode(body_hex) {
        Ok(bytes) => bytes,
        Err(_) => {
            return Ok(unknown(
                "the exact cancellation response receipt is malformed",
            ));
        }
    };
    if hex::encode(sha2::Sha256::digest(&body_bytes)) != body_hash {
        return Ok(unknown(
            "the cancellation response receipt digest does not match",
        ));
    }
    let receipt_id = serde_json::from_slice::<Value>(&body_bytes)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned));
    let (Some(account_name), Some(order_id), Some(client_oid), Some(product_id), Some(parent_id)) = (
        binding["account"].as_str(),
        binding["orderId"].as_str(),
        binding["clientOrderId"].as_str(),
        binding["productId"].as_str(),
        binding["operationId"].as_str(),
    ) else {
        return Ok(unknown("cancellation binding is incomplete"));
    };
    if !matches!(
        record.status,
        Status::Submitted | Status::Pending | Status::Unknown
    ) {
        return Ok(unknown("cancellation record is not unresolved"));
    }
    let Some(account) = profile
        .accounts()
        .iter()
        .find(|a| a.name == account_name && a.product == CoinbaseProduct::Exchange)
    else {
        return Ok(unknown("Exchange account is no longer enrolled"));
    };
    if !account.identity_verified
        || binding["physicalIdentity"].as_str() != Some(account.physical_identity.as_str())
    {
        return Ok(unknown("Exchange profile identity changed"));
    }
    let parents = profile.operations_by_prepared_binding(
        "coinbaseexchange.create-order",
        "/clientOrderId",
        &json!(client_oid),
    )?;
    let original: Submission = match serde_json::from_value(binding["originalSubmission"].clone()) {
        Ok(s) => s,
        Err(_) => return Ok(unknown("original submission binding is invalid")),
    };
    if !parents.iter().any(|parent| {
        parent.id == parent_id
            && parent.submission.as_ref() == Some(&original)
            && matches!(
                parent.status,
                Status::Submitted | Status::Pending | Status::Unknown
            )
            && parent.preview.as_ref().is_some_and(|p| {
                p.binding["account"] == account_name
                    && p.binding["productId"] == product_id
                    && p.binding["clientOrderId"] == client_oid
            })
    }) {
        return Ok(unknown(
            "exact original order is not unresolved in protected history",
        ));
    }
    let response = profile
        .request_for_account(
            account_name,
            CoinbaseProduct::Exchange,
            CoinbaseRoute::ExchangeOrder {
                order_id: order_id.into(),
            },
            CoinbaseRequest::get(),
            None,
        )
        .await;
    let Ok(response) = response else {
        return Ok(unknown("authoritative Exchange order state is unavailable"));
    };
    if response.status == 404
        && binding["filledSizeBefore"].as_str().is_some_and(|size| {
            Quantity::from_decimal(size, 36).is_ok_and(|amount| amount.is_zero())
        })
        && receipt_id.as_deref() == Some(order_id)
    {
        let effect = FinancialEffect::SubmissionCancellation {
            operation_id: parent_id.into(),
            submission: original,
        };
        let evidence = json!({"reconciledOrderId":order_id,"clientOrderId":client_oid,"cancelReceipt":receipt_id,"venueStatus":"canceled_record_purged","filledSizeBefore":binding["filledSizeBefore"]});
        let mut verified = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects: vec![effect],
            evidence: evidence.clone(),
            observed_at: gofin_domain::now(),
            proof_digest: String::new(),
        };
        verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
        return Ok(Reconciliation {
            status: Status::Completed,
            evidence,
            verified: Some(verified),
        });
    }
    if !(200..300).contains(&response.status) {
        return Ok(unknown("authoritative Exchange order state is unavailable"));
    }
    let value: Value = match serde_json::from_slice(&response.body) {
        Ok(v) => v,
        Err(_) => return Ok(unknown("Exchange order response is invalid")),
    };
    let order = value.get("order").unwrap_or(&value);
    let status = order
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if order
        .get("id")
        .or_else(|| order.get("order_id"))
        .and_then(Value::as_str)
        != Some(order_id)
        || order.get("client_oid").and_then(Value::as_str) != Some(client_oid)
        || order.get("product_id").and_then(Value::as_str) != Some(product_id)
        || order.get("profile_id").and_then(Value::as_str)
            != Some(account.physical_identity.as_str())
    {
        return Ok(unknown(
            "authoritative order identity does not match the cancellation target",
        ));
    }
    if !matches!(status.as_str(), "canceled" | "cancelled") {
        if matches!(status.as_str(), "open" | "pending" | "active") {
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"order":order,"reason":"order has not reached canceled terminal state; original reservation remains"}),
                verified: None,
            });
        }
        if matches!(status.as_str(), "done" | "settled" | "rejected") {
            return Ok(Reconciliation {
                status: Status::Failed,
                evidence: json!({"order":order,"reason":"order became terminal before cancellation could remove its remainder; reconcile the original operation"}),
                verified: None,
            });
        }
        return Ok(unknown(
            "Exchange returned an unrecognized cancellation order status",
        ));
    }
    let effect = FinancialEffect::SubmissionCancellation {
        operation_id: parent_id.into(),
        submission: original,
    };
    let evidence = json!({"order":order,"reconciledOrderId":order_id,"status":status,"profileId":account.physical_identity,"clientOrderId":client_oid});
    let mut verified = VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects: vec![effect],
        evidence: evidence.clone(),
        observed_at: gofin_domain::now(),
        proof_digest: String::new(),
    };
    verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
    Ok(Reconciliation {
        status: Status::Completed,
        evidence,
        verified: Some(verified),
    })
}

async fn reconcile_exchange_order(
    profile: &CoinbaseProfile,
    record: &OperationRecord,
    submission: &Submission,
) -> Result<Reconciliation> {
    let unknown = |reason: &str| Reconciliation {
        status: Status::Unknown,
        evidence: json!({"reason":reason}),
        verified: None,
    };
    let Some(prepared) = record.preview.as_ref().filter(|p| {
        p.operation == "coinbaseexchange.create-order"
            && p.binding["provider"] == "coinbaseexchange"
    }) else {
        return Ok(unknown(
            "the immutable prepared Exchange order is unavailable",
        ));
    };
    let Some(account_name) = prepared.binding["account"].as_str() else {
        return Ok(unknown("the prepared Exchange account identity is missing"));
    };
    let Some(client_oid) = prepared.binding["clientOrderId"].as_str() else {
        return Ok(unknown("the prepared client order ID is missing"));
    };
    if submission.kind != "coinbase"
        || submission.reference != client_oid
        || submission.data["product"] != "coinbaseexchange"
        || submission.data["clientOrderId"] != client_oid
        || submission.data["account"] != account_name
        || submission.data["productId"] != prepared.binding["productId"]
    {
        return Ok(unknown(
            "the durable submission differs from the prepared Exchange order",
        ));
    }
    if profile
        .assert_unique_client_order_id(
            CoinbaseProduct::Exchange,
            client_oid,
            Some(record.id.as_str()),
        )
        .is_err()
    {
        return Ok(unknown(
            "another durable operation claims this Exchange client order ID",
        ));
    }
    let Some(account) = profile
        .accounts()
        .iter()
        .find(|a| a.name == account_name && a.product == CoinbaseProduct::Exchange)
    else {
        return Ok(unknown(
            "the enrolled Exchange profile is no longer available",
        ));
    };
    if !account.identity_verified
        || prepared.binding["physicalIdentity"].as_str() != Some(account.physical_identity.as_str())
    {
        return Ok(unknown(
            "the Exchange profile identity changed after submission",
        ));
    }
    // Exchange accepts `client:<client_oid>` as the native GET order identifier.
    // This permits recovery after a lost POST receipt without creating a new ID.
    let order_response = profile
        .request_for_account(
            account_name,
            CoinbaseProduct::Exchange,
            CoinbaseRoute::ExchangeOrderByClientId {
                client_oid: client_oid.to_owned(),
            },
            CoinbaseRequest::get(),
            None,
        )
        .await;
    let Ok(order_response) = order_response else {
        return Ok(unknown(
            "the Exchange order identity could not be recovered; do not replay",
        ));
    };
    if !(200..300).contains(&order_response.status) {
        return Ok(unknown(
            "the Exchange order lookup is unavailable; do not replay",
        ));
    }
    let order_doc: Value = match serde_json::from_slice(&order_response.body) {
        Ok(value) => value,
        Err(_) => return Ok(unknown("the Exchange order response was invalid")),
    };
    let order = order_doc.get("order").unwrap_or(&order_doc);
    let (Some(order_id), Some(product_id), Some(returned_client_oid), Some(profile_id), Some(side)) = (
        order
            .get("id")
            .or_else(|| order.get("order_id"))
            .and_then(Value::as_str),
        order.get("product_id").and_then(Value::as_str),
        order.get("client_oid").and_then(Value::as_str),
        order.get("profile_id").and_then(Value::as_str),
        order.get("side").and_then(Value::as_str),
    ) else {
        return Ok(unknown(
            "the Exchange order omitted required immutable identity fields",
        ));
    };
    if returned_client_oid != client_oid
        || profile_id != account.physical_identity
        || product_id != prepared.binding["productId"].as_str().unwrap_or_default()
        || side.to_ascii_uppercase() != prepared.input["side"].as_str().unwrap_or_default()
        || order
            .get("time_in_force")
            .and_then(Value::as_str)
            .is_none_or(|tif| {
                !tif.eq_ignore_ascii_case(prepared.binding["timeInForce"].as_str().unwrap_or("GTC"))
            })
    {
        return Ok(unknown(
            "the recovered order does not match its bound client, profile, product, or side",
        ));
    }

    let mut after: Option<String> = None;
    let mut cursors = std::collections::BTreeSet::new();
    let mut fills = std::collections::BTreeMap::<String, (String, Value)>::new();
    let mut exhausted = false;
    for page_number in 0..1000 {
        let response = profile
            .request_for_account(
                account_name,
                CoinbaseProduct::Exchange,
                CoinbaseRoute::ExchangeFillsFiltered {
                    query: CoinbaseQuery {
                        order_id: Some(order_id.to_owned()),
                        product_id: Some(product_id.to_owned()),
                        profile_id: Some(profile_id.to_owned()),
                        after: after.clone(),
                        limit: Some(1000),
                        ..Default::default()
                    },
                },
                CoinbaseRequest::get(),
                None,
            )
            .await;
        let Ok(response) = response else {
            return Ok(unknown("Exchange fill history is unavailable"));
        };
        if !(200..300).contains(&response.status) {
            return Ok(unknown("Exchange fill history is unavailable"));
        }
        let page: Value = match serde_json::from_slice(&response.body) {
            Ok(value) => value,
            Err(_) => return Ok(unknown("Exchange returned invalid fill history")),
        };
        let Some(rows) = page.as_array() else {
            return Ok(unknown("Exchange fill response was not an array"));
        };
        if rows.is_empty() {
            exhausted = true;
            break;
        }
        for fill in rows {
            if fill["order_id"].as_str() != Some(order_id)
                || fill["product_id"].as_str() != Some(product_id)
                || fill["profile_id"].as_str() != Some(profile_id)
                || fill["side"]
                    .as_str()
                    .map(str::to_ascii_uppercase)
                    .as_deref()
                    != Some(side.to_ascii_uppercase().as_str())
            {
                return Ok(unknown(
                    "an Exchange fill did not attest the order, profile, product, and side",
                ));
            }
            let Some(fill_id) = fill
                .get("trade_id")
                .and_then(Value::as_i64)
                .map(|v| v.to_string())
                .or_else(|| {
                    fill.get("trade_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
            else {
                return Ok(unknown(
                    "an Exchange fill omitted its stable trade identity",
                ));
            };
            let digest = hex::encode(sha2::Sha256::digest(
                gofin_domain::canonical_json(fill)?.as_bytes(),
            ));
            if let Some((prior, _)) = fills.get(&fill_id) {
                if prior != &digest {
                    return Ok(unknown(
                        "Exchange returned conflicting evidence for an immutable trade ID",
                    ));
                }
            } else {
                fills.insert(fill_id, (digest, fill.clone()));
            }
        }
        let Some(next) = response
            .pagination
            .after
            .filter(|cursor| !cursor.is_empty())
        else {
            exhausted = true;
            break;
        };
        if page_number == 999 || !cursors.insert(next.clone()) || after.as_deref() == Some(&next) {
            return Ok(unknown("Exchange fill pagination did not advance safely"));
        }
        after = Some(next);
    }
    if !exhausted {
        return Ok(unknown(
            "Exchange fill pagination reached its bounded page limit",
        ));
    }

    let (Some(base), Some(quote)) = (
        prepared.binding["baseCurrencyId"].as_str(),
        prepared.binding["quoteCurrencyId"].as_str(),
    ) else {
        return Ok(unknown(
            "the prepared Exchange currency identities are missing",
        ));
    };
    let mut base_total = Quantity::zero();
    let mut quote_total = Quantity::zero();
    let mut fee_total = Quantity::zero();
    let mut all_fills_settled = true;
    let mut event_digests = std::collections::BTreeMap::new();
    let mut proof_events = Vec::new();
    for (id, (digest, fill)) in &fills {
        let (Some(size), Some(price), Some(fee)) = (
            fill.get("size").and_then(Value::as_str),
            fill.get("price").and_then(Value::as_str),
            fill.get("fee").and_then(Value::as_str),
        ) else {
            return Ok(unknown("an Exchange fill omitted size, price, or fee"));
        };
        let size = Quantity::from_decimal(size, 36)?;
        let Some(settled) = fill.get("settled").and_then(Value::as_bool) else {
            return Ok(unknown("an Exchange fill omitted its settlement state"));
        };
        all_fills_settled &= settled;
        let notional =
            crate::profile::decimal_product_atoms_exact(&size.to_decimal(36), price, 36)?;
        base_total = base_total.checked_add(&size)?;
        quote_total = quote_total.checked_add(&notional)?;
        fee_total = fee_total.checked_add(&Quantity::from_decimal(fee, 36)?)?;
        let event_id = format!("coinbaseexchange:fill:{id}");
        event_digests.insert(event_id.clone(), digest.clone());
        proof_events.push(json!({"eventId":event_id,"fill":fill}));
    }
    let (Some(authoritative_base), Some(authoritative_quote), Some(authoritative_fee)) = (
        order.get("filled_size").and_then(Value::as_str),
        order.get("executed_value").and_then(Value::as_str),
        order.get("fill_fees").and_then(Value::as_str),
    ) else {
        return Ok(unknown(
            "the authoritative Exchange order omitted cumulative fill totals",
        ));
    };
    if Quantity::from_decimal(authoritative_base, 36)? != base_total
        || Quantity::from_decimal(authoritative_quote, 36)? != quote_total
        || Quantity::from_decimal(authoritative_fee, 36)? != fee_total
    {
        return Ok(unknown(
            "paginated Exchange fills do not sum to authoritative order totals",
        ));
    }
    let status_text = order
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_terminal = matches!(
        status_text.as_str(),
        "done" | "settled" | "rejected" | "canceled" | "cancelled"
    );
    let is_open = matches!(status_text.as_str(), "open" | "pending" | "active");
    if !is_terminal && !is_open {
        return Ok(unknown("the Exchange order status is not recognized"));
    }
    if is_open && !fills.is_empty() {
        // The currently supported operation is a single GTC limit order. A partial fill is
        // cumulative and stays reserved until the venue makes it terminal.
    }
    let side = prepared.input["side"].as_str().unwrap_or_default();
    let (spent_asset, received_asset, spent, received) = if side == "BUY" {
        (
            provider_asset(CoinbaseProduct::Exchange, quote),
            provider_asset(CoinbaseProduct::Exchange, base),
            quote_total.clone(),
            base_total.clone(),
        )
    } else {
        (
            provider_asset(CoinbaseProduct::Exchange, base),
            provider_asset(CoinbaseProduct::Exchange, quote),
            base_total.clone(),
            quote_total.clone(),
        )
    };
    let resource = account.resource.clone();
    let mut effects = Vec::new();
    if !fills.is_empty() {
        effects.push(FinancialEffect::Conversion {
            resource: resource.clone(),
            spent: Amount {
                asset: spent_asset,
                atoms: spent.clone(),
            },
            received: Amount {
                asset: received_asset,
                atoms: received.clone(),
            },
        });
        if !fee_total.is_zero() {
            effects.push(FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: Amount {
                    asset: provider_asset(CoinbaseProduct::Exchange, quote),
                    atoms: fee_total.clone(),
                },
                category: "coinbaseexchange-trading-fee".into(),
                treatment: if side == "SELL" && quote_total >= fee_total {
                    CostTreatment::DeductedFromCredit
                } else {
                    CostTreatment::AdditionalDebit
                },
            });
        }
    }
    let max_spend =
        Quantity::from_decimal(prepared.input["maximumSpend"].as_str().unwrap_or("0"), 36)?;
    let max_fee = Quantity::from_decimal(prepared.input["maximumFee"].as_str().unwrap_or("0"), 36)?;
    let min_receive =
        Quantity::from_decimal(prepared.input["minimumReceive"].as_str().unwrap_or("0"), 36)?;
    let net_receive = if side == "SELL" && quote_total >= fee_total {
        quote_total.checked_sub(&fee_total)?
    } else {
        received.clone()
    };
    let within_bounds = spent <= max_spend && fee_total <= max_fee && net_receive >= min_receive;
    let status = if is_open || !all_fills_settled {
        Status::Pending
    } else if within_bounds && !fills.is_empty() {
        Status::Completed
    } else {
        Status::Failed
    };
    if status == Status::Failed && !is_terminal {
        return Ok(unknown("Exchange order is not terminal"));
    }
    let previous = record.effects.as_ref().and_then(|e| {
        SettlementProgress::from_evidence(&e.evidence)
            .ok()
            .flatten()
    });
    if let Some(old) = &previous {
        for (id, digest) in &old.event_digests {
            if event_digests
                .get(id)
                .is_none_or(|current| current != digest)
            {
                return Ok(unknown(
                    "a previously reconciled Exchange fill disappeared or changed",
                ));
            }
        }
    }
    let snapshot_id = gofin_domain::binding_hash(
        &json!({"orderId":order_id,"status":status_text,"events":event_digests,"base":base_total,"quote":quote_total,"fees":fee_total}),
    )?;
    let progress = SettlementProgress {
        version: 1,
        sequence: previous.as_ref().map_or(1, |p| {
            if p.snapshot_id == snapshot_id {
                p.sequence
            } else {
                p.sequence + 1
            }
        }),
        snapshot_id,
        event_digests,
        position_orders: vec![],
    };
    let evidence = json!({"order":order,"reconciledOrderId":order_id,"proofEvents":proof_events,"settlement_progress":progress,"fillCount":fills.len(),"baseTotal":base_total,"quoteTotal":quote_total,"feeTotal":fee_total,"allFillsSettled":all_fills_settled,"withinAuthorizedBounds":within_bounds});
    let mut verified = None;
    if !effects.is_empty() || status.terminal() {
        let mut bundle = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects,
            evidence: evidence.clone(),
            observed_at: gofin_domain::now(),
            proof_digest: String::new(),
        };
        bundle.proof_digest = gofin_domain::verified_effect_digest(&bundle)?;
        verified = Some(bundle);
    }
    Ok(Reconciliation {
        status,
        evidence,
        verified,
    })
}

fn create_order_schema() -> Value {
    let decimal = json!({
        "type":"string",
        "pattern":"^(0|[1-9][0-9]*)(\\.[0-9]+)?$",
        "maxLength":80
    });
    json!({
        "type":"object",
        "required":["account","requestId","productId","side","baseSize","limitPrice","maximumSpend","minimumReceive","maximumFee"],
        "properties":{
            "account":{"type":"string","minLength":1,"maxLength":128},
            "requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},
            "productId":{"type":"string","pattern":"^[A-Za-z0-9_-]{1,128}$"},
            "side":{"type":"string","enum":["BUY","SELL"]},
            "baseSize":decimal,
            "limitPrice":decimal,
            "maximumSpend":decimal,
            "minimumReceive":decimal,
            "maximumFee":decimal,
            "postOnly":{"type":"boolean"},
            "timeInForce":{"type":"string","enum":["GTC","IOC","FOK"]}
        },
        "additionalProperties":false
    })
}

fn international_create_order_schema() -> Value {
    let decimal = json!({"type":"string","pattern":"^(0|[1-9][0-9]*)(\\.[0-9]+)?$","maxLength":80});
    json!({"type":"object","required":["account","requestId","instrument","side","baseSize","limitPrice","maximumSpend","minimumReceive","maximumFee"],"properties":{
        "account":{"type":"string","minLength":1,"maxLength":128},
        "requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},
        "instrument":{"type":"string","pattern":"^[A-Za-z0-9_-]{1,128}$"},
        "side":{"type":"string","enum":["BUY","SELL"]},
        "baseSize":decimal,"limitPrice":decimal,"maximumSpend":decimal,"minimumReceive":decimal,"maximumFee":decimal,
        "postOnly":{"type":"boolean"},
        "timeInForce":{"type":"string","enum":["GTC","IOC","FOK"]}
    },"additionalProperties":false})
}

async fn prepare_international_order(
    profile: &CoinbaseProfile,
    input: &Value,
    allow_current_balance_shortfall: bool,
) -> Result<PreparedOperation> {
    let validator =
        jsonschema::validator_for(&international_create_order_schema()).map_err(|_| {
            error(
                "COINBASE_CONTRACT_INVALID",
                "International order schema is invalid.",
            )
        })?;
    if !validator.is_valid(input) {
        return Err(error(
            "INVALID_INPUT",
            "International order input does not match its closed contract.",
        ));
    }
    let account_name = input["account"].as_str().expect("validated");
    let account = profile
        .accounts()
        .iter()
        .find(|a| a.name == account_name && a.product == CoinbaseProduct::International)
        .ok_or_else(|| {
            error(
                "COINBASE_ACCOUNT_UNKNOWN",
                "Select an enrolled Coinbase International account.",
            )
        })?;
    if !account.identity_verified || account.physical_identity.trim().is_empty() {
        return Err(error(
            "COINBASE_IDENTITY_UNVERIFIED",
            "International order requires a verified enrolled identity.",
        ));
    }
    let portfolio = account
        .portfolio_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            error(
                "COINBASE_PORTFOLIO_UNKNOWN",
                "International order requires the enrolled portfolio ID.",
            )
        })?;
    let instrument_name = input["instrument"].as_str().expect("validated");
    let time_in_force = input
        .get("timeInForce")
        .and_then(Value::as_str)
        .unwrap_or("GTC");
    let post_only = input
        .get("postOnly")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if post_only && time_in_force != "GTC" {
        return Err(error(
            "INVALID_INPUT",
            "International post-only orders require GTC time in force.",
        ));
    }
    let instrument_desc =
        crate::reads::descriptor("coinbaseinternational.read-v1-public-instruments-instrument")
            .ok_or_else(|| {
                error(
                    "COINBASE_ROUTE_UNSUPPORTED",
                    "International instrument metadata route is unavailable.",
                )
            })?;
    let instrument_response = profile
        .request_for_account(
            account_name,
            CoinbaseProduct::International,
            crate::reads::endpoint(
                instrument_desc,
                &json!({"account":account_name,"instrument":instrument_name}),
            ),
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    let instrument_doc = parse_required_json(
        instrument_response,
        "International instrument metadata could not be verified.",
    )?;
    let instrument = instrument_doc.get("instrument").unwrap_or(&instrument_doc);
    if required_text(
        instrument,
        "symbol",
        "International instrument metadata omitted its symbol.",
    )? != instrument_name
        || required_text(
            instrument,
            "type",
            "International instrument metadata omitted its type.",
        )? != "SPOT"
        || required_text(
            instrument,
            "mode",
            "International instrument metadata omitted its mode.",
        )? != "STANDARD"
        || required_text(
            instrument,
            "trading_state",
            "International instrument metadata omitted trading state.",
        )? != "TRADING"
    {
        return Err(error(
            "COINBASE_INSTRUMENT_UNAVAILABLE",
            "Only standard, actively trading International spot instruments are supported.",
        ));
    }
    let base = required_text(
        instrument,
        "base_asset_name",
        "International instrument omitted base asset.",
    )?;
    let quote = required_text(
        instrument,
        "quote_asset_name",
        "International instrument omitted quote asset.",
    )?;
    let base_id = required_text(
        instrument,
        "base_asset_uuid",
        "International instrument omitted base asset identity.",
    )?;
    let quote_id = required_text(
        instrument,
        "quote_asset_uuid",
        "International instrument omitted quote asset identity.",
    )?;
    let base_increment = required_text(
        instrument,
        "base_increment",
        "International instrument omitted base increment.",
    )?;
    let price_increment = required_text(
        instrument,
        "quote_increment",
        "International instrument omitted price increment.",
    )?;
    let minimum_size = required_text(
        instrument,
        "min_quantity",
        "International instrument omitted minimum size.",
    )?;
    let size = input["baseSize"].as_str().expect("validated");
    let price = input["limitPrice"].as_str().expect("validated");
    if !positive(size)?
        || !positive(price)?
        || !multiple(size, base_increment)?
        || !multiple(price, price_increment)?
        || !decimal_at_least(size, minimum_size)?
    {
        return Err(error(
            "COINBASE_INCREMENT_INVALID",
            "International order size or limit price is not positive and aligned to instrument increments.",
        ));
    }
    let notional = crate::profile::decimal_product_atoms_exact(size, price, 36)?;
    let notional_text = notional.to_decimal(36);
    let side = input["side"].as_str().expect("validated");
    let principal = if side == "BUY" { &notional_text } else { size };
    if !decimal_at_least(
        input["maximumSpend"].as_str().expect("validated"),
        principal,
    )? {
        return Err(error(
            "COINBASE_SPEND_CAP_TOO_LOW",
            "Maximum spend is below the exact International limit-order principal.",
        ));
    }
    if side == "BUY"
        && !decimal_at_least(size, input["minimumReceive"].as_str().expect("validated"))?
    {
        return Err(error(
            "COINBASE_RECEIVE_FLOOR_INVALID",
            "Buy minimum receive exceeds its base quantity.",
        ));
    }
    let fee_desc =
        crate::reads::descriptor("coinbaseinternational.read-v1-private-portfolios-fee-rates")
            .ok_or_else(|| {
                error(
                    "COINBASE_ROUTE_UNSUPPORTED",
                    "International fee rate route is unavailable.",
                )
            })?;
    let fee_response = profile
        .request_for_account(
            account_name,
            CoinbaseProduct::International,
            crate::reads::endpoint(fee_desc, &json!({"account":account_name})),
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    let fee_value = parse_required_json(
        fee_response,
        "International fee tiers could not be verified.",
    )?;
    let fee_rows = fee_value.as_array().ok_or_else(|| {
        error(
            "COINBASE_RESPONSE_INVALID",
            "International fee response was not an array.",
        )
    })?;
    let spot_fee = fee_rows
        .iter()
        .find(|row| row.get("instrument_type").and_then(Value::as_str) == Some("SPOT"))
        .ok_or_else(|| {
            error(
                "COINBASE_FEE_UNAVAILABLE",
                "International spot fee tier is missing.",
            )
        })?;
    let maker_rate = required_text(
        spot_fee,
        "maker_fee_rate",
        "International maker fee rate is missing.",
    )?;
    let taker_rate = required_text(
        spot_fee,
        "taker_fee_rate",
        "International taker fee rate is missing.",
    )?;
    let fee_rate =
        if Quantity::from_decimal(maker_rate, 36)? >= Quantity::from_decimal(taker_rate, 36)? {
            maker_rate
        } else {
            taker_rate
        };
    // INTX has no verified native preview endpoint in the pinned client or
    // current official REST reference. Bound the exact request locally using
    // the native instrument metadata and current spot fee tier instead.
    let estimated_fee = crate::profile::decimal_product_atoms_exact(&notional_text, fee_rate, 36)?;
    let maximum_fee = Quantity::from_decimal(input["maximumFee"].as_str().expect("validated"), 36)?;
    if estimated_fee > maximum_fee {
        return Err(error(
            "COINBASE_FEE_CAP_TOO_LOW",
            "Worst-case International fee exceeds the authorized cap.",
        ));
    }
    let spend_asset = if side == "BUY" { quote_id } else { base_id };
    let spend_name = if side == "BUY" { quote } else { base };
    let spend_required = if side == "BUY" {
        notional.checked_add(&estimated_fee)?
    } else {
        Quantity::from_decimal(size, 36)?
    };
    let balance_desc = crate::reads::descriptor(
        "coinbaseinternational.read-v1-private-portfolios-portfolio-balances",
    )
    .ok_or_else(|| {
        error(
            "COINBASE_ROUTE_UNSUPPORTED",
            "International portfolio balances route is unavailable.",
        )
    })?;
    let balance_response = profile
        .request_for_account(
            account_name,
            CoinbaseProduct::International,
            crate::reads::endpoint(
                balance_desc,
                &json!({"account":account_name,"portfolio":portfolio}),
            ),
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    let balances = parse_required_json(
        balance_response,
        "International portfolio balances could not be verified.",
    )?;
    let rows = balances.as_array().ok_or_else(|| {
        error(
            "COINBASE_RESPONSE_INVALID",
            "International balance response was not an array.",
        )
    })?;
    let row = rows
        .iter()
        .find(|row| row.get("asset_uuid").and_then(Value::as_str) == Some(spend_asset))
        .ok_or_else(|| {
            error(
                "COINBASE_BALANCE_INSUFFICIENT",
                "The enrolled portfolio has no exact spend-asset balance row.",
            )
        })?;
    if row.get("asset_name").and_then(Value::as_str) != Some(spend_name)
        || row.get("asset_id").and_then(Value::as_str).is_none()
    {
        return Err(error(
            "COINBASE_ASSET_IDENTITY_MISMATCH",
            "International balance asset did not match instrument metadata.",
        ));
    }
    let total = Quantity::from_decimal(
        required_text(
            row,
            "quantity",
            "International balance omitted total quantity.",
        )?,
        36,
    )?;
    // `loan` is a liability, not an asset hold. Keep it out of this
    // availability calculation; portfolio snapshot coverage records it
    // separately as debt evidence.
    let held = Quantity::from_decimal(
        required_text(row, "hold", "International balance omitted trading hold.")?,
        36,
    )?
    .checked_add(&Quantity::from_decimal(
        required_text(
            row,
            "transfer_hold",
            "International balance omitted transfer hold.",
        )?,
        36,
    )?)?
    .checked_add(&Quantity::from_decimal(
        required_text(
            row,
            "pledged_collateral_quantity",
            "International balance omitted pledged collateral.",
        )?,
        36,
    )?)?;
    let available = total.checked_sub(&held).map_err(|_| {
        error(
            "COINBASE_BALANCE_INSUFFICIENT",
            "Held, pledged or borrowed assets leave no verified spendable balance.",
        )
    })?;
    if available < spend_required && !allow_current_balance_shortfall {
        return Err(error(
            "COINBASE_BALANCE_INSUFFICIENT",
            "International spendable balance does not cover principal and maximum fee.",
        ));
    }
    let client_oid = deterministic_uuid(&format!(
        "coinbaseinternational\0{}\0{}\0{}",
        account.physical_identity,
        portfolio,
        input["requestId"].as_str().expect("validated")
    ));
    let request_body = gofin_domain::canonical_json(
        &json!({"client_order_id":client_oid,"side":side,"size":size,"tif":time_in_force,"instrument":instrument_name,"type":"LIMIT","price":price,"portfolio":portfolio,"post_only":post_only}),
    )?;
    let debit = if side == "BUY" {
        notional.clone()
    } else {
        Quantity::from_decimal(size, 36)?
    };
    let fee_bound = CostBound {
        location: AccountingLocation::Cash {
            resource: account.resource.clone(),
        },
        amount: Amount {
            asset: provider_asset(CoinbaseProduct::International, quote_id),
            atoms: estimated_fee.clone(),
        },
        treatment: if side == "SELL" {
            CostTreatment::DeductedFromCredit
        } else {
            CostTreatment::AdditionalDebit
        },
    };
    let debits = vec![ResourceAmount {
        resource: account.resource.clone(),
        amount: Amount {
            asset: provider_asset(CoinbaseProduct::International, spend_asset),
            atoms: debit.clone(),
        },
    }];
    let costs = if estimated_fee.is_zero() {
        vec![]
    } else {
        vec![fee_bound.clone()]
    };
    let valuation = profile.financial_valuation(&debits, &costs).await?;
    let minimum_receive =
        Quantity::from_decimal(input["minimumReceive"].as_str().expect("validated"), 36)?;
    let output_floor = if side == "SELL" {
        minimum_receive.checked_add(&estimated_fee)?
    } else {
        minimum_receive
    };
    let mut prepared=PreparedOperation{operation:"coinbaseinternational.create-order".into(),input:input.clone(),binding:json!({"version":1,"provider":"coinbaseinternational","account":account.name,"resource":account.resource,"physicalIdentity":account.physical_identity,"portfolioId":portfolio,"clientOrderId":client_oid,"instrument":instrument_name,"instrumentId":instrument["instrument_id"],"instrumentUuid":instrument["instrument_uuid"],"baseAssetId":base_id,"quoteAssetId":quote_id,"baseAssetName":base,"quoteAssetName":quote,"side":side,"baseSize":size,"limitPrice":price,"timeInForce":time_in_force,"postOnly":post_only,"priceIncrement":price_increment,"feeRate":fee_rate,"estimatedFee":estimated_fee.to_decimal(36),"requestBodyExact":request_body}),summary:format!("Place Coinbase International {side} {base}/{quote} spot limit {time_in_force} order for {size} at {price}"),checks:vec![Check{name:"instrument_fees_balance".into(),status:CheckStatus::Passed,detail:"Exact spot instrument increments, current worst-case spot fee tier, and unencumbered enrolled-portfolio balance were checked locally; INTX exposes no verified native preview route.".into()}],warnings:vec!["Order acceptance is not settlement; native order state and complete fills must reconcile before treating balances as final.".into()],observed_at:gofin_domain::now(),expires_at:(chrono::Utc::now()+chrono::Duration::seconds(45)).to_rfc3339(),valuation:Some(valuation),intent:FinancialIntent{debits,maximum_costs:costs,expected:vec![EffectConstraint::Conversion{resource:account.resource.clone(),input:provider_asset(CoinbaseProduct::International, spend_asset),maximum_input:debit,output:provider_asset(CoinbaseProduct::International, if side=="BUY"{base_id}else{quote_id}),minimum_output:output_floor}],locks:vec![account.resource.clone()]},operation_version:1,action_deadline:None};
    gofin_domain::value_bounds::bind(&mut prepared);
    Ok(prepared)
}

fn deterministic_uuid(input: &str) -> String {
    let mut bytes = sha2::Sha256::digest(input.as_bytes())[..16].to_vec();
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

fn international_cancel_order_schema() -> Value {
    json!({"type":"object","required":["account","requestId","orderId","clientOrderId","instrument"],"properties":{"account":{"type":"string","minLength":1,"maxLength":128},"requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},"orderId":{"type":"string","minLength":1,"maxLength":128},"clientOrderId":{"type":"string","format":"uuid"},"instrument":{"type":"string","pattern":"^[A-Za-z0-9_-]{1,128}$"}},"additionalProperties":false})
}

fn international_edit_order_schema() -> Value {
    json!({"type":"object","required":["account","requestId","operationId","orderId","clientOrderId","instrument","newPrice"],"properties":{"account":{"type":"string","minLength":1,"maxLength":128},"requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},"operationId":{"type":"string","minLength":1,"maxLength":256},"orderId":{"type":"string","minLength":1,"maxLength":128},"clientOrderId":{"type":"string","format":"uuid"},"instrument":{"type":"string","pattern":"^[A-Za-z0-9_-]{1,128}$"},"newPrice":{"type":"string","pattern":"^(0|[1-9][0-9]*)(\\.[0-9]+)?$","maxLength":80}},"additionalProperties":false})
}

async fn prepare_international_edit(
    profile: &CoinbaseProfile,
    input: &Value,
    linked: &gofin_domain::amendments::AmendmentContext,
) -> Result<PreparedOperation> {
    let validator =
        jsonschema::validator_for(&international_edit_order_schema()).map_err(|_| {
            error(
                "COINBASE_CONTRACT_INVALID",
                "International edit schema is invalid.",
            )
        })?;
    if !validator.is_valid(input) {
        return Err(error(
            "INVALID_INPUT",
            "International edit input does not match its closed contract.",
        ));
    }
    if !linked.revisions.is_empty() {
        return Err(error(
            "COINBASE_EDIT_CHAIN_UNSUPPORTED",
            "A second International edit is not enabled until native predecessor ordering is proven.",
        ));
    }
    let name = input["account"].as_str().unwrap_or_default();
    let account = profile
        .accounts()
        .iter()
        .find(|a| a.name == name && a.product == CoinbaseProduct::International)
        .ok_or_else(|| {
            error(
                "COINBASE_ACCOUNT_UNKNOWN",
                "Select an enrolled International account.",
            )
        })?;
    if !account.identity_verified {
        return Err(error(
            "COINBASE_IDENTITY_UNVERIFIED",
            "International order editing requires verified enrollment.",
        ));
    }
    let portfolio = account
        .portfolio_id
        .as_deref()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            error(
                "COINBASE_PORTFOLIO_UNKNOWN",
                "International portfolio identity is missing.",
            )
        })?;
    let original = linked.original.as_ref().ok_or_else(|| {
        error(
            "COINBASE_ORIGINAL_OPERATION_UNVERIFIED",
            "Editing requires protected context for the original create-order operation.",
        )
    })?;
    let original_submission = linked
        .original_evidence
        .as_ref()
        .and_then(|e| e.submission.as_ref())
        .ok_or_else(|| {
            error(
                "COINBASE_ORIGINAL_OPERATION_UNVERIFIED",
                "Protected journal has no durable original submission.",
            )
        })?;
    if original.operation != "coinbaseinternational.create-order"
        || original.binding["account"] != name
        || original.binding["portfolioId"] != portfolio
        || original.binding["physicalIdentity"] != account.physical_identity
        || original.binding["instrument"] != input["instrument"]
        || original.binding["clientOrderId"] != input["clientOrderId"]
        || original_submission.kind != "coinbase"
        || original_submission.reference != input["clientOrderId"].as_str().unwrap_or_default()
        || original_submission.data["product"] != "coinbaseinternational"
    {
        return Err(error(
            "COINBASE_ORIGINAL_OPERATION_UNVERIFIED",
            "Protected original order does not match this account, portfolio, instrument and client ID.",
        ));
    }
    let operation_id = input["operationId"].as_str().unwrap_or_default();
    let parents = profile.operations_by_prepared_binding(
        "coinbaseinternational.create-order",
        "/clientOrderId",
        &input["clientOrderId"],
    )?;
    if !parents.iter().any(|record| {
        record.id == operation_id
            && matches!(
                record.status,
                Status::Submitted | Status::Pending | Status::Unknown
            )
            && record.submission.as_ref() == Some(original_submission)
            && record
                .preview
                .as_ref()
                .is_some_and(|p| p.binding == original.binding)
    }) {
        return Err(error(
            "COINBASE_ORIGINAL_OPERATION_UNVERIFIED",
            "The requested original order is not one exact unresolved protected journal entry.",
        ));
    }
    let id = input["orderId"].as_str().unwrap_or_default();
    let client = input["clientOrderId"].as_str().unwrap_or_default();
    let instrument = input["instrument"].as_str().unwrap_or_default();
    let response = profile
        .request_for_account(
            name,
            CoinbaseProduct::International,
            CoinbaseRoute::InternationalOrderScoped {
                order_id: id.into(),
                portfolio_id: portfolio.into(),
            },
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    let order = parse_required_json(
        response,
        "International order could not be revalidated for editing.",
    )?;
    if order.get("order_id").and_then(identifier_text).as_deref() != Some(id)
        || order["client_order_id"] != client
        || order["portfolio_uuid"] != portfolio
        || order["symbol"] != instrument
        || order["instrument_uuid"] != original.binding["instrumentUuid"]
        || order["side"] != original.binding["side"]
        || order["type"] != "LIMIT"
        || order["tif"] != "GTC"
        || order["order_status"] != "WORKING"
        || order["size"] != original.binding["baseSize"]
        || order.get("price").and_then(Value::as_str) != original.binding["limitPrice"].as_str()
        || !positive(
            order
                .get("leaves_qty")
                .and_then(Value::as_str)
                .unwrap_or("0"),
        )?
    {
        return Err(error(
            "COINBASE_EDIT_TARGET_CHANGED",
            "Only the exact open SPOT LIMIT GTC order from the protected create operation can be edited.",
        ));
    }
    let new_price = input["newPrice"].as_str().unwrap_or_default();
    if !positive(new_price)?
        || !multiple(
            new_price,
            original.binding["priceIncrement"].as_str().unwrap_or(""),
        )?
    {
        return Err(error(
            "COINBASE_INCREMENT_INVALID",
            "Edited International limit price must be positive and aligned to the instrument tick.",
        ));
    }
    let side = original.binding["side"].as_str().unwrap_or_default();
    let old_price = original.binding["limitPrice"].as_str().unwrap_or_default();
    let size = original.binding["baseSize"].as_str().unwrap_or_default();
    let new_notional = crate::profile::decimal_product_atoms_exact(size, new_price, 36)?;
    let fee_rate = original.binding["feeRate"].as_str().unwrap_or("0");
    let estimated_fee =
        crate::profile::decimal_product_atoms_exact(&new_notional.to_decimal(36), fee_rate, 36)?;
    let max_fee = Quantity::from_decimal(original.input["maximumFee"].as_str().unwrap_or("0"), 36)?;
    let maximum_spend =
        Quantity::from_decimal(original.input["maximumSpend"].as_str().unwrap_or("0"), 36)?;
    let minimum_receive =
        Quantity::from_decimal(original.input["minimumReceive"].as_str().unwrap_or("0"), 36)?;
    if estimated_fee > max_fee
        || (side == "BUY" && new_notional.checked_add(&estimated_fee)? > maximum_spend)
        || (side == "SELL"
            && new_notional
                .checked_sub(&estimated_fee)
                .map_or(true, |net| net < minimum_receive))
    {
        return Err(error(
            "COINBASE_EDIT_BOUND_INVALID",
            "Edited order economics exceed the original approved spend, fee or net receive limits.",
        ));
    }
    // The native API requires a new client_order_id on PUT. It remains linked
    // to the exact original operation and is never used to create a new order.
    let successor_client_id = deterministic_uuid(&format!(
        "coinbaseinternational-edit\0{operation_id}\0{}\0{}",
        input["requestId"].as_str().unwrap_or_default(),
        new_price
    ));
    let body = gofin_domain::canonical_json(
        &json!({"client_order_id":successor_client_id,"portfolio":portfolio,"price":new_price}),
    )?;
    let parent_json = serde_json::to_value(original_submission).map_err(|_| {
        error(
            "COINBASE_BINDING_INVALID",
            "Original submission could not be bound.",
        )
    })?;
    let mut revision_binding = original.binding.clone();
    revision_binding["effectiveLimitPrice"] = json!(new_price);
    revision_binding["effectiveClientOrderId"] = json!(successor_client_id);
    let mut revision_intent = original.intent.clone();
    revision_intent
        .expected
        .retain(|e| !matches!(e, EffectConstraint::SubmissionAmendment { .. }));
    let revision = gofin_domain::amendments::AmendmentScope {
        intent: revision_intent,
        binding: revision_binding,
        valuation: original.valuation.clone(),
    };
    let mut binding = json!({"version":1,"provider":"coinbaseinternational","account":name,"resource":account.resource,"physicalIdentity":account.physical_identity,"portfolioId":portfolio,"orderId":id,"clientOrderId":client,"successorClientOrderId":successor_client_id,"instrument":instrument,"operationId":operation_id,"originalSubmission":parent_json,"originalBinding":original.binding,"originalInput":original.input,"predecessorPrice":old_price,"newPrice":new_price,"baseSize":size,"requestBodyExact":body,"priceIncrement":original.binding["priceIncrement"]});
    binding["__fin_amendment"] = json!({"version":1,"parent_operation_id":operation_id,"parent_submission":parent_json,"request_id":input["requestId"],"shared_locks":[account.resource.0]});
    let prepared = PreparedOperation { operation:"coinbaseinternational.edit-order".into(), input:input.clone(), binding,
        summary:format!("Edit the price of linked International order {id}"),
        checks:vec![Check{name:"linked_native_order".into(),status:CheckStatus::Passed,detail:"Exact unresolved original journal operation, portfolio-scoped order, instrument and current price were verified.".into()}],
        warnings:vec!["The original operation retains every fill; an edit is not settlement.".into()],
        observed_at:gofin_domain::now(), expires_at:(chrono::Utc::now()+chrono::Duration::seconds(45)).to_rfc3339(), valuation:original.valuation.clone(),
        intent:FinancialIntent{debits:vec![],maximum_costs:vec![],expected:vec![EffectConstraint::SubmissionAmendment{operation_id:operation_id.into(),submission:original_submission.clone(),revision:Box::new(revision)}],locks:original.intent.locks.clone()},operation_version:1,action_deadline:None};
    Ok(prepared)
}

async fn refresh_international_edit(
    profile: &CoinbaseProfile,
    approved: &PreparedOperation,
) -> Result<PreparedOperation> {
    let binding = &approved.binding;
    let (
        Some(name),
        Some(order_id),
        Some(portfolio),
        Some(client),
        Some(instrument),
        Some(old_price),
    ) = (
        binding["account"].as_str(),
        binding["orderId"].as_str(),
        binding["portfolioId"].as_str(),
        binding["clientOrderId"].as_str(),
        binding["instrument"].as_str(),
        binding["predecessorPrice"].as_str(),
    )
    else {
        return Err(error(
            "COINBASE_BINDING_INVALID",
            "International edit binding is incomplete.",
        ));
    };
    let account = profile
        .accounts()
        .iter()
        .find(|a| a.name == name && a.product == CoinbaseProduct::International)
        .ok_or_else(|| {
            error(
                "COINBASE_ACCOUNT_UNKNOWN",
                "International account is no longer enrolled.",
            )
        })?;
    if !account.identity_verified
        || account.portfolio_id.as_deref() != Some(portfolio)
        || account.physical_identity != binding["physicalIdentity"].as_str().unwrap_or_default()
    {
        return Err(error(
            "COINBASE_PROFILE_MISMATCH",
            "Enrolled International identity changed after approval.",
        ));
    }
    let response = profile
        .request_for_account(
            name,
            CoinbaseProduct::International,
            CoinbaseRoute::InternationalOrderScoped {
                order_id: order_id.into(),
                portfolio_id: portfolio.into(),
            },
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    let order = parse_required_json(
        response,
        "International order cannot be revalidated for edit dispatch.",
    )?;
    if order.get("order_id").and_then(identifier_text).as_deref() != Some(order_id)
        || order["client_order_id"] != client
        || order["portfolio_uuid"] != portfolio
        || order["symbol"] != instrument
        || order["order_status"] != "WORKING"
        || order["type"] != "LIMIT"
        || order["tif"] != "GTC"
        || order["price"].as_str() != Some(old_price)
        || order["size"].as_str() != binding["baseSize"].as_str()
        || !positive(order["leaves_qty"].as_str().unwrap_or("0"))?
    {
        return Err(error(
            "COINBASE_PREVIEW_CHANGED",
            "Open International order identity, price, size or status changed after edit approval.",
        ));
    }
    Ok(approved.clone())
}

async fn plan_international_edit(
    profile: &CoinbaseProfile,
    current: &PreparedOperation,
) -> Result<PlannedSubmission<CoinbaseDispatch>> {
    let name = current.binding["account"].as_str().ok_or_else(|| {
        error(
            "COINBASE_BINDING_INVALID",
            "International account is missing.",
        )
    })?;
    let account = profile
        .accounts()
        .iter()
        .find(|a| a.name == name && a.product == CoinbaseProduct::International)
        .ok_or_else(|| {
            error(
                "COINBASE_ACCOUNT_UNKNOWN",
                "International account is no longer enrolled.",
            )
        })?;
    if !account.identity_verified
        || account.physical_identity
            != current.binding["physicalIdentity"]
                .as_str()
                .unwrap_or_default()
        || account.portfolio_id.as_deref() != current.binding["portfolioId"].as_str()
    {
        return Err(error(
            "COINBASE_PROFILE_MISMATCH",
            "International identity or portfolio changed after approval.",
        ));
    }
    let client = current.binding["successorClientOrderId"]
        .as_str()
        .ok_or_else(|| {
            error(
                "COINBASE_BINDING_INVALID",
                "Successor client ID is missing.",
            )
        })?;
    profile.assert_unique_client_order_id(CoinbaseProduct::International, client, None)?;
    let submission = Submission {
        kind: "coinbase".into(),
        reference: client.into(),
        data: json!({"product":"coinbaseinternational","account":name,"portfolioId":current.binding["portfolioId"],"orderId":current.binding["orderId"],"clientOrderId":client,"originalClientOrderId":current.binding["clientOrderId"],"instrument":current.binding["instrument"],"binding":current.binding}),
    };
    Ok(PlannedSubmission {
        submission: submission.clone(),
        request: CoinbaseDispatch {
            product: CoinbaseProduct::International,
            endpoint: CoinbaseRoute::InternationalEditOrder {
                order_id: current.binding["orderId"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
            },
            account: name.into(),
            exact_body: current.binding["requestBodyExact"]
                .as_str()
                .ok_or_else(|| {
                    error(
                        "COINBASE_BINDING_INVALID",
                        "Exact International edit body is missing.",
                    )
                })?
                .into(),
            submission,
            candidate: current.clone(),
        },
    })
}

async fn reconcile_international_edit(
    profile: &CoinbaseProfile,
    record: &OperationRecord,
    submission: &Submission,
) -> Result<Reconciliation> {
    let unknown = |reason: &str| Reconciliation {
        status: Status::Unknown,
        evidence: json!({"reason":reason,"replay":"forbidden_without_conclusive_reconciliation"}),
        verified: None,
    };
    let Some(prepared) = record.preview.as_ref().filter(|p| {
        p.operation == "coinbaseinternational.edit-order" && submission.data["binding"] == p.binding
    }) else {
        return Ok(unknown(
            "persisted edit binding does not match the submission",
        ));
    };
    let binding = &prepared.binding;
    let Some(successor_client_id) = binding["successorClientOrderId"].as_str() else {
        return Ok(unknown(
            "persisted edit omitted its successor client order ID",
        ));
    };
    if profile
        .assert_unique_client_order_id(
            CoinbaseProduct::International,
            successor_client_id,
            Some(record.id.as_str()),
        )
        .is_err()
    {
        return Ok(unknown(
            "another durable operation claims this International edit client order ID",
        ));
    }
    let Some(dispatch) = record.dispatch_evidence.as_ref() else {
        return Ok(unknown(
            "no captured International edit response; replay is forbidden",
        ));
    };
    let status = dispatch["httpStatus"].as_u64().unwrap_or(0) as u16;
    let body = dispatch["bodyHex"]
        .as_str()
        .and_then(|h| hex::decode(h).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let Some(receipt) = body else {
        return Ok(unknown("captured edit response is not valid JSON"));
    };
    if !(200..300).contains(&status) {
        return Ok(Reconciliation {
            status: Status::Failed,
            evidence: json!({"dispatch":dispatch,"receipt":receipt}),
            verified: None,
        });
    }
    let (Some(account_name), Some(order_id), Some(portfolio), Some(successor), Some(instrument)) = (
        binding["account"].as_str(),
        binding["orderId"].as_str(),
        binding["portfolioId"].as_str(),
        binding["successorClientOrderId"].as_str(),
        binding["instrument"].as_str(),
    ) else {
        return Ok(unknown("durable International edit binding is incomplete"));
    };
    // The edit response must itself identify the order mutation. A generic 2xx
    // or an unrelated row cannot establish a successful order revision.
    if receipt.get("order_id").and_then(identifier_text).as_deref() != Some(order_id)
        || receipt["client_order_id"] != successor
        || receipt["portfolio_uuid"] != portfolio
        || receipt["symbol"] != instrument
        || receipt["instrument_uuid"] != binding["originalBinding"]["instrumentUuid"]
        || receipt["side"] != binding["originalBinding"]["side"]
        || receipt["type"] != "LIMIT"
        || receipt["tif"] != "GTC"
        || receipt["price"].as_str() != binding["newPrice"].as_str()
        || receipt["size"].as_str() != binding["baseSize"].as_str()
    {
        return Ok(unknown(
            "captured native edit response does not attest the exact successor identity and price",
        ));
    }
    let account = profile
        .accounts()
        .iter()
        .find(|a| a.name == account_name && a.product == CoinbaseProduct::International)
        .ok_or_else(|| {
            error(
                "COINBASE_ACCOUNT_UNKNOWN",
                "International account is no longer enrolled.",
            )
        })?;
    if !account.identity_verified
        || account.portfolio_id.as_deref() != Some(portfolio)
        || account.physical_identity != binding["physicalIdentity"].as_str().unwrap_or_default()
    {
        return Ok(unknown(
            "enrolled International identity changed before edit verification",
        ));
    }
    let response = profile
        .request_for_account(
            account_name,
            CoinbaseProduct::International,
            CoinbaseRoute::InternationalOrderScoped {
                order_id: order_id.into(),
                portfolio_id: portfolio.into(),
            },
            CoinbaseRequest::get(),
            None,
        )
        .await;
    let Ok(response) = response else {
        return Ok(unknown("authoritative edited order is unavailable"));
    };
    if !(200..300).contains(&response.status) {
        return Ok(unknown("authoritative edited order is unavailable"));
    }
    let Ok(order) = serde_json::from_slice::<Value>(&response.body) else {
        return Ok(unknown("authoritative edited order JSON is invalid"));
    };
    if order.get("order_id").and_then(identifier_text).as_deref() != Some(order_id)
        || order["client_order_id"] != successor
        || order["portfolio_uuid"] != portfolio
        || order["symbol"] != instrument
        || order["instrument_uuid"] != binding["originalBinding"]["instrumentUuid"]
        || order["side"] != binding["originalBinding"]["side"]
        || order["type"] != "LIMIT"
        || order["tif"] != "GTC"
        || order["price"].as_str() != binding["newPrice"].as_str()
        || order["size"].as_str() != binding["baseSize"].as_str()
        || !matches!(
            order["order_status"].as_str(),
            Some("WORKING" | "FILLED" | "CANCELLED" | "EXPIRED")
        )
    {
        return Ok(unknown(
            "authoritative order state does not prove the requested International edit",
        ));
    }
    let original: Submission = serde_json::from_value(binding["originalSubmission"].clone())
        .map_err(|_| {
            error(
                "COINBASE_BINDING_INVALID",
                "Original order submission is invalid.",
            )
        })?;
    let effect = gofin_domain::FinancialEffect::SubmissionAmendment {
        operation_id: binding["operationId"].as_str().unwrap_or_default().into(),
        submission: original,
        successor: None,
    };
    let evidence = json!({"dispatch":dispatch,"receipt":receipt,"order":order,"originalOperationId":binding["operationId"],"successorClientOrderId":successor});
    let mut verified = gofin_domain::VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects: vec![effect],
        evidence: evidence.clone(),
        observed_at: gofin_domain::now(),
        proof_digest: String::new(),
    };
    verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
    Ok(Reconciliation {
        status: Status::Completed,
        evidence,
        verified: Some(verified),
    })
}

async fn prepare_international_cancel(
    profile: &CoinbaseProfile,
    input: &Value,
) -> Result<PreparedOperation> {
    let validator =
        jsonschema::validator_for(&international_cancel_order_schema()).map_err(|_| {
            error(
                "COINBASE_CONTRACT_INVALID",
                "International cancellation schema is invalid.",
            )
        })?;
    if !validator.is_valid(input) {
        return Err(error(
            "INVALID_INPUT",
            "International cancellation input does not match its closed contract.",
        ));
    }
    let name = input["account"].as_str().expect("schema");
    let account = profile
        .accounts()
        .iter()
        .find(|a| a.name == name && a.product == CoinbaseProduct::International)
        .ok_or_else(|| {
            error(
                "COINBASE_ACCOUNT_UNKNOWN",
                "Select an enrolled International account.",
            )
        })?;
    let portfolio = account
        .portfolio_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            error(
                "COINBASE_PORTFOLIO_UNKNOWN",
                "International portfolio identity is missing.",
            )
        })?;
    if !account.identity_verified {
        return Err(error(
            "COINBASE_IDENTITY_UNVERIFIED",
            "International cancellation requires verified enrollment.",
        ));
    }
    let (id, client, instrument) = (
        input["orderId"].as_str().unwrap(),
        input["clientOrderId"].as_str().unwrap(),
        input["instrument"].as_str().unwrap(),
    );
    let response = profile
        .request_for_account(
            name,
            CoinbaseProduct::International,
            CoinbaseRoute::InternationalOrderScoped {
                order_id: id.into(),
                portfolio_id: portfolio.into(),
            },
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    let order = parse_required_json(
        response,
        "International order state could not be verified for cancellation.",
    )?;
    if order.get("order_id").and_then(identifier_text).as_deref() != Some(id)
        || order.get("client_order_id").and_then(Value::as_str) != Some(client)
        || order.get("portfolio_uuid").and_then(Value::as_str) != Some(portfolio)
        || order.get("symbol").and_then(Value::as_str) != Some(instrument)
    {
        return Err(error(
            "COINBASE_ORDER_IDENTITY_MISMATCH",
            "International order did not match its exact enrolled portfolio, client ID, order ID and instrument.",
        ));
    }
    if order.get("order_status").and_then(Value::as_str) != Some("WORKING")
        || !positive(required_text(
            &order,
            "leaves_qty",
            "International order omitted open remainder",
        )?)?
    {
        return Err(error(
            "COINBASE_ORDER_NOT_OPEN",
            "International order has no verified open remainder to cancel.",
        ));
    }
    let original = Submission {
        kind: "coinbase".into(),
        reference: client.into(),
        data: json!({"product":"coinbaseinternational","account":name,"portfolioId":portfolio,"clientOrderId":client,"instrument":instrument}),
    };
    let parents = profile
        .operations_by_prepared_binding(
            "coinbaseinternational.create-order",
            "/clientOrderId",
            &json!(client),
        )?
        .into_iter()
        .filter(|r| {
            r.submission.as_ref() == Some(&original)
                && r.preview.as_ref().is_some_and(|p| {
                    p.operation == "coinbaseinternational.create-order"
                        && p.binding["account"] == name
                        && p.binding["portfolioId"] == portfolio
                        && p.binding["instrument"] == instrument
                        && p.binding["clientOrderId"] == client
                })
                && matches!(
                    r.status,
                    Status::Submitted | Status::Pending | Status::Unknown
                )
        })
        .collect::<Vec<_>>();
    if parents.len() != 1 {
        return Err(error(
            "COINBASE_ORIGINAL_OPERATION_UNVERIFIED",
            "Protected journal does not identify exactly one unresolved original International order.",
        ));
    }
    let parent = parents[0].id.clone();
    let binding = json!({"version":1,"provider":"coinbaseinternational","account":name,"resource":account.resource,"physicalIdentity":account.physical_identity,"portfolioId":portfolio,"orderId":id,"clientOrderId":client,"instrument":instrument,"operationId":parent,"originalSubmission":original,"requestBodyExact":""});
    Ok(PreparedOperation{operation:"coinbaseinternational.cancel-order".into(),input:input.clone(),binding,summary:format!("Cancel remaining size on International order {id}; original fills remain with the linked order operation."),checks:vec![Check{name:"original_order_linkage".into(),status:CheckStatus::Passed,detail:"The exact open order and one unresolved protected parent order operation were verified.".into()}],warnings:vec!["A fill may race cancellation; reconcile cancellation and original order before acting again.".into()],observed_at:gofin_domain::now(),expires_at:(chrono::Utc::now()+chrono::Duration::seconds(45)).to_rfc3339(),valuation:None,intent:FinancialIntent{debits:vec![],maximum_costs:vec![],expected:vec![EffectConstraint::SubmissionCancellation{operation_id:parent,submission:original}],locks:vec![account.resource.clone()]},operation_version:1,action_deadline:None})
}

async fn plan_international_cancel(
    profile: &CoinbaseProfile,
    current: &PreparedOperation,
) -> Result<PlannedSubmission<CoinbaseDispatch>> {
    let name = current.binding["account"].as_str().ok_or_else(|| {
        error(
            "COINBASE_BINDING_INVALID",
            "International account binding is missing.",
        )
    })?;
    let account = profile
        .accounts()
        .iter()
        .find(|a| a.name == name && a.product == CoinbaseProduct::International)
        .ok_or_else(|| {
            error(
                "COINBASE_ACCOUNT_UNKNOWN",
                "International account is no longer enrolled.",
            )
        })?;
    let portfolio = current.binding["portfolioId"].as_str().ok_or_else(|| {
        error(
            "COINBASE_BINDING_INVALID",
            "International portfolio binding is missing.",
        )
    })?;
    if !account.identity_verified
        || account.physical_identity
            != current.binding["physicalIdentity"]
                .as_str()
                .unwrap_or_default()
        || account.portfolio_id.as_deref() != Some(portfolio)
    {
        return Err(error(
            "COINBASE_PROFILE_MISMATCH",
            "International account or portfolio changed after approval.",
        ));
    }
    let reference = deterministic_uuid(&format!(
        "coinbaseinternational-cancel\0{}\0{}",
        account.physical_identity,
        current.input["requestId"].as_str().unwrap_or_default()
    ));
    let submission = Submission {
        kind: "coinbase".into(),
        reference,
        data: json!({"product":"coinbaseinternational","account":name,"portfolioId":portfolio,"orderId":current.binding["orderId"],"clientOrderId":current.binding["clientOrderId"],"instrument":current.binding["instrument"],"binding":current.binding}),
    };
    Ok(PlannedSubmission {
        submission: submission.clone(),
        request: CoinbaseDispatch {
            product: CoinbaseProduct::International,
            endpoint: CoinbaseRoute::InternationalCancelOrderScoped {
                order_id: current.binding["orderId"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
                portfolio_id: portfolio.into(),
            },
            account: name.into(),
            exact_body: String::new(),
            submission,
            candidate: current.clone(),
        },
    })
}

async fn reconcile_international_cancel(
    profile: &CoinbaseProfile,
    record: &OperationRecord,
    submission: &Submission,
) -> Result<Reconciliation> {
    let unknown = |reason: &str| Reconciliation {
        status: Status::Unknown,
        evidence: json!({"reason":reason,"replay":"forbidden_without_conclusive_reconciliation"}),
        verified: None,
    };
    let binding = &submission.data["binding"];
    if submission.kind != "coinbase"
        || submission.data["product"] != "coinbaseinternational"
        || record.submission.as_ref() != Some(submission)
        || record.preview.as_ref().is_none_or(|p| {
            p.operation != "coinbaseinternational.cancel-order" || p.binding != *binding
        })
    {
        return Ok(unknown(
            "durable cancel differs from exact prepared binding",
        ));
    }
    let Some(dispatch) = record.dispatch_evidence.as_ref() else {
        return Ok(unknown(
            "native International cancel receipt is missing; do not replay",
        ));
    };
    let Some(status) = dispatch.get("httpStatus").and_then(Value::as_u64) else {
        return Ok(unknown("native cancel response status is missing"));
    };
    if !(200..300).contains(&(status as u16)) {
        return Ok(unknown("International cancel was not acknowledged"));
    }
    let (Some(hex_body), Some(hash)) = (
        dispatch.get("bodyHex").and_then(Value::as_str),
        dispatch.get("bodySha256").and_then(Value::as_str),
    ) else {
        return Ok(unknown(
            "exact native cancel response evidence is incomplete",
        ));
    };
    let body = match hex::decode(hex_body) {
        Ok(v) => v,
        Err(_) => return Ok(unknown("native cancel response bytes are malformed")),
    };
    if hex::encode(sha2::Sha256::digest(&body)) != hash {
        return Ok(unknown("native cancel response digest mismatch"));
    }
    let receipt: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return Ok(unknown("native cancel response JSON is invalid")),
    };
    let (Some(name), Some(portfolio), Some(order_id), Some(client), Some(instrument), Some(parent)) = (
        binding["account"].as_str(),
        binding["portfolioId"].as_str(),
        binding["orderId"].as_str(),
        binding["clientOrderId"].as_str(),
        binding["instrument"].as_str(),
        binding["operationId"].as_str(),
    ) else {
        return Ok(unknown("cancel binding is incomplete"));
    };
    if receipt.get("order_id").and_then(identifier_text).as_deref() != Some(order_id)
        || receipt.get("client_order_id").and_then(Value::as_str) != Some(client)
        || receipt.get("portfolio_uuid").and_then(Value::as_str) != Some(portfolio)
        || receipt.get("symbol").and_then(Value::as_str) != Some(instrument)
    {
        return Ok(unknown(
            "cancel receipt did not attest the exact target order",
        ));
    }
    let Some(account) = profile
        .accounts()
        .iter()
        .find(|a| a.name == name && a.product == CoinbaseProduct::International)
    else {
        return Ok(unknown("International account is no longer enrolled"));
    };
    if !account.identity_verified
        || account.portfolio_id.as_deref() != Some(portfolio)
        || account.physical_identity != binding["physicalIdentity"].as_str().unwrap_or_default()
    {
        return Ok(unknown("International account identity changed"));
    }
    let original: Submission = match serde_json::from_value(binding["originalSubmission"].clone()) {
        Ok(s) => s,
        Err(_) => return Ok(unknown("original submission binding is invalid")),
    };
    let parents = profile.operations_by_prepared_binding(
        "coinbaseinternational.create-order",
        "/clientOrderId",
        &json!(client),
    )?;
    if !parents.iter().any(|p| {
        p.id == parent
            && p.submission.as_ref() == Some(&original)
            && matches!(
                p.status,
                Status::Submitted | Status::Pending | Status::Unknown
            )
    }) {
        return Ok(unknown(
            "exact original order is not unresolved in protected history",
        ));
    }
    let response = profile
        .request_for_account(
            name,
            CoinbaseProduct::International,
            CoinbaseRoute::InternationalOrderScoped {
                order_id: order_id.into(),
                portfolio_id: portfolio.into(),
            },
            CoinbaseRequest::get(),
            None,
        )
        .await;
    let Ok(response) = response else {
        return Ok(unknown(
            "authoritative International order state is unavailable",
        ));
    };
    if !(200..300).contains(&response.status) {
        return Ok(unknown(
            "authoritative International order state is unavailable",
        ));
    }
    let order: Value = match serde_json::from_slice(&response.body) {
        Ok(v) => v,
        Err(_) => {
            return Ok(unknown(
                "authoritative International order response is invalid",
            ));
        }
    };
    if order.get("order_id").and_then(identifier_text).as_deref() != Some(order_id)
        || order.get("client_order_id").and_then(Value::as_str) != Some(client)
        || order.get("portfolio_uuid").and_then(Value::as_str) != Some(portfolio)
        || order.get("symbol").and_then(Value::as_str) != Some(instrument)
    {
        return Ok(unknown(
            "authoritative order did not attest exact cancel target",
        ));
    }
    let state = order
        .get("order_status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if state != "DONE" {
        return Ok(Reconciliation {
            status: Status::Pending,
            evidence: json!({"order":order,"reason":"cancel acknowledged; order still reports an open state"}),
            verified: None,
        });
    }
    let leaves = Quantity::from_decimal(
        required_text(
            &order,
            "leaves_qty",
            "Terminal International order omitted leaves quantity",
        )?,
        36,
    )?;
    if !leaves.is_zero() {
        return Ok(unknown(
            "terminal International order reports an unexplained open remainder",
        ));
    }
    let effect = FinancialEffect::SubmissionCancellation {
        operation_id: parent.into(),
        submission: original,
    };
    let evidence = json!({"cancelReceipt":receipt,"order":order,"reconciledOrderId":order_id,"portfolioId":portfolio,"clientOrderId":client,"instrument":instrument});
    let mut verified = VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects: vec![effect],
        evidence: evidence.clone(),
        observed_at: gofin_domain::now(),
        proof_digest: String::new(),
    };
    verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
    Ok(Reconciliation {
        status: Status::Completed,
        evidence,
        verified: Some(verified),
    })
}

async fn plan_international_order(
    profile: &CoinbaseProfile,
    current: &PreparedOperation,
) -> Result<PlannedSubmission<CoinbaseDispatch>> {
    let account_name = current.binding["account"].as_str().ok_or_else(|| {
        error(
            "COINBASE_BINDING_INVALID",
            "International account binding is missing.",
        )
    })?;
    let account = profile
        .accounts()
        .iter()
        .find(|a| a.name == account_name && a.product == CoinbaseProduct::International)
        .ok_or_else(|| {
            error(
                "COINBASE_ACCOUNT_UNKNOWN",
                "International account is no longer enrolled.",
            )
        })?;
    if !account.identity_verified
        || current.binding["physicalIdentity"].as_str() != Some(account.physical_identity.as_str())
        || current.binding["portfolioId"].as_str() != account.portfolio_id.as_deref()
    {
        return Err(error(
            "COINBASE_PROFILE_MISMATCH",
            "International identity or portfolio changed after approval.",
        ));
    }
    let client_id = current.binding["clientOrderId"].as_str().ok_or_else(|| {
        error(
            "COINBASE_BINDING_INVALID",
            "International client order ID is missing.",
        )
    })?;
    let submission = Submission {
        kind: "coinbase".into(),
        reference: client_id.into(),
        data: json!({"product":"coinbaseinternational","account":account_name,"portfolioId":current.binding["portfolioId"],"clientOrderId":client_id,"instrument":current.binding["instrument"]}),
    };
    Ok(PlannedSubmission {
        submission: submission.clone(),
        request: CoinbaseDispatch {
            product: CoinbaseProduct::International,
            endpoint: CoinbaseRoute::InternationalOrders,
            account: account_name.into(),
            exact_body: current.binding["requestBodyExact"]
                .as_str()
                .ok_or_else(|| {
                    error(
                        "COINBASE_BINDING_INVALID",
                        "Exact International request body is missing.",
                    )
                })?
                .into(),
            submission,
            candidate: current.clone(),
        },
    })
}

async fn reconcile_international_order(
    profile: &CoinbaseProfile,
    record: &OperationRecord,
    submission: &Submission,
) -> Result<Reconciliation> {
    let unknown = |reason: &str| Reconciliation {
        status: Status::Unknown,
        evidence: json!({"reason":reason,"replay":"forbidden_without_conclusive_reconciliation"}),
        verified: None,
    };
    let Some(prepared) = record.preview.as_ref().filter(|p| {
        p.operation == "coinbaseinternational.create-order"
            && p.binding["provider"] == "coinbaseinternational"
    }) else {
        return Ok(unknown(
            "the immutable International order preparation is unavailable",
        ));
    };
    let (Some(account_name), Some(portfolio), Some(client_id), Some(instrument)) = (
        prepared.binding["account"].as_str(),
        prepared.binding["portfolioId"].as_str(),
        prepared.binding["clientOrderId"].as_str(),
        prepared.binding["instrument"].as_str(),
    ) else {
        return Ok(unknown("International order binding is incomplete"));
    };
    if submission.kind != "coinbase"
        || submission.reference != client_id
        || submission.data["product"] != "coinbaseinternational"
        || submission.data["account"] != account_name
        || submission.data["portfolioId"] != portfolio
        || submission.data["clientOrderId"] != client_id
        || submission.data["instrument"] != instrument
    {
        return Ok(unknown(
            "durable submission differs from exact International order binding",
        ));
    }
    if profile
        .assert_unique_client_order_id(
            CoinbaseProduct::International,
            client_id,
            Some(record.id.as_str()),
        )
        .is_err()
    {
        return Ok(unknown(
            "another durable operation claims this International client order ID",
        ));
    }
    let Some(account) = profile
        .accounts()
        .iter()
        .find(|a| a.name == account_name && a.product == CoinbaseProduct::International)
    else {
        return Ok(unknown("International account is no longer enrolled"));
    };
    if !account.identity_verified
        || account.portfolio_id.as_deref() != Some(portfolio)
        || account.physical_identity
            != prepared.binding["physicalIdentity"]
                .as_str()
                .unwrap_or_default()
    {
        return Ok(unknown(
            "International account identity or portfolio changed",
        ));
    }
    let effective_client_id =
        match international_effective_client_id(profile, record, submission).await? {
            Ok(id) => id,
            Err(reason) => return Ok(unknown(reason)),
        };
    // The native order lookup requires the venue-assigned order ID. Search open orders
    // and fills by our durable client ID; no match is inconclusive and is never replayed.
    let orders = profile
        .request_for_account(
            account_name,
            CoinbaseProduct::International,
            CoinbaseRoute::InternationalOrdersFiltered {
                query: CoinbaseQuery {
                    portfolio_id: Some(portfolio.into()),
                    instrument: Some(instrument.into()),
                    client_order_id: Some(effective_client_id.clone()),
                    result_limit: Some(100),
                    result_offset: Some(0),
                    ..Default::default()
                },
            },
            CoinbaseRequest::get(),
            None,
        )
        .await;
    let Ok(orders) = orders else {
        return Ok(unknown("International order lookup is unavailable"));
    };
    if !(200..300).contains(&orders.status) {
        return Ok(unknown("International order lookup is unavailable"));
    }
    let rows: Value = match serde_json::from_slice(&orders.body) {
        Ok(v) => v,
        Err(_) => return Ok(unknown("International order list JSON is invalid")),
    };
    let rows = rows
        .get("results")
        .and_then(Value::as_array)
        .or_else(|| rows.as_array());
    let Some(rows) = rows else {
        return Ok(unknown("International order list omitted results"));
    };
    let matching = rows
        .iter()
        .filter(|order| {
            order.get("client_order_id").and_then(Value::as_str)
                == Some(effective_client_id.as_str())
                && order.get("symbol").and_then(Value::as_str) == Some(instrument)
                && order.get("portfolio_uuid").and_then(Value::as_str) == Some(portfolio)
        })
        .collect::<Vec<_>>();
    let order_id = if matching.len() == 1 {
        matching[0]
            .get("order_id")
            .and_then(identifier_text)
            .ok_or_else(|| {
                error(
                    "COINBASE_RESPONSE_INVALID",
                    "International order omitted a valid venue order ID.",
                )
            })?
    } else if matching.is_empty() {
        // The official order list contains only active orders. Completed or rejected
        // orders may be absent, so recover a durable venue ID from exact client-bound
        // fill history; no matching fill remains inconclusive and is never replayed.
        match international_order_id_from_fills(
            profile,
            account,
            portfolio,
            instrument,
            &effective_client_id,
        )
        .await?
        {
            Some(id) => id,
            None => {
                return Ok(unknown(
                    "no active order or exact client-bound fill identifies the submitted order; do not replay",
                ));
            }
        }
    } else {
        return Ok(unknown(
            "multiple active International orders matched one durable client ID",
        ));
    };
    reconcile_international_authoritative(
        profile,
        record,
        submission,
        account,
        &order_id,
        &effective_client_id,
    )
    .await
}

async fn international_effective_client_id(
    profile: &CoinbaseProfile,
    parent: &OperationRecord,
    original_submission: &Submission,
) -> Result<std::result::Result<String, &'static str>> {
    let children =
        profile.journal_amendment_children(&parent.id, "coinbaseinternational.edit-order")?;
    if children.is_empty() {
        return Ok(Ok(original_submission.reference.clone()));
    }
    if children.len() != 1 {
        return Ok(Err(
            "multiple International edits prevent exact successor ordering",
        ));
    }
    let child = &children[0];
    let Some(prepared) = child
        .preview
        .as_ref()
        .filter(|p| p.operation == "coinbaseinternational.edit-order")
    else {
        return Ok(Err(
            "linked International edit has no immutable preparation",
        ));
    };
    let b = &prepared.binding;
    let original_json = serde_json::to_value(original_submission).map_err(|_| {
        error(
            "COINBASE_BINDING_INVALID",
            "Original submission could not be checked.",
        )
    })?;
    if b["operationId"] != parent.id
        || b["originalSubmission"] != original_json
        || b["clientOrderId"] != original_submission.reference
        || b["provider"] != "coinbaseinternational"
    {
        return Ok(Err(
            "linked International edit differs from the original order identity",
        ));
    }
    if !child.status.terminal() {
        return Ok(Err(
            "linked International edit has not reached a verified terminal state",
        ));
    }
    if child.status != Status::Completed {
        return Ok(Ok(original_submission.reference.clone()));
    }
    let Some(verified) = child.effects.as_ref() else {
        return Ok(Err("completed International edit has no proof"));
    };
    if child.submission.as_ref().is_none_or(|s| verified.submission != *s)
        || gofin_domain::verified_effect_digest(verified)? != verified.proof_digest
        || !verified.effects.iter().any(|effect| matches!(effect, gofin_domain::FinancialEffect::SubmissionAmendment { operation_id, submission, .. } if operation_id == &parent.id && submission == original_submission)) {
        return Ok(Err("International edit proof is not bound to the original submission"));
    }
    let Some(successor) = b["successorClientOrderId"].as_str() else {
        return Ok(Err("International edit successor ID is missing"));
    };
    if child.submission.as_ref().is_none_or(|s| {
        s.reference != successor || s.data["originalClientOrderId"] != original_submission.reference
    }) {
        return Ok(Err(
            "International edit submission differs from its protected successor ID",
        ));
    }
    Ok(Ok(successor.into()))
}

async fn international_order_id_from_fills(
    profile: &CoinbaseProfile,
    account: &CoinbaseAccount,
    portfolio: &str,
    instrument: &str,
    client_id: &str,
) -> Result<Option<String>> {
    let mut offset = 0u32;
    let mut seen = std::collections::BTreeSet::new();
    let mut ids = std::collections::BTreeSet::new();
    for page in 0..1000 {
        let response = profile
            .request_for_account(
                &account.name,
                CoinbaseProduct::International,
                CoinbaseRoute::InternationalPortfolioFillsFiltered {
                    portfolio_id: portfolio.into(),
                    query: CoinbaseQuery {
                        client_order_id: Some(client_id.into()),
                        result_limit: Some(100),
                        result_offset: Some(offset),
                        ..Default::default()
                    },
                },
                CoinbaseRequest::get(),
                None,
            )
            .await;
        let Ok(response) = response else {
            return Ok(None);
        };
        if !(200..300).contains(&response.status) {
            return Ok(None);
        };
        let body: Value = match serde_json::from_slice(&response.body) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        let Some(rows) = body.get("results").and_then(Value::as_array) else {
            return Ok(None);
        };
        for fill in rows {
            if fill.get("client_order_id").and_then(Value::as_str) != Some(client_id)
                || fill.get("portfolio_uuid").and_then(Value::as_str) != Some(portfolio)
                || fill.get("symbol").and_then(Value::as_str) != Some(instrument)
            {
                return Ok(None);
            };
            let id = fill
                .get("order_id")
                .and_then(identifier_text)
                .ok_or_else(|| {
                    error(
                        "COINBASE_RESPONSE_INVALID",
                        "International fill omitted a valid venue order ID.",
                    )
                })?;
            ids.insert(id);
        }
        if rows.len() < 100 {
            return Ok((ids.len() == 1).then(|| ids.into_iter().next().expect("one ID")));
        }
        if page == 999 || !seen.insert(offset) {
            return Ok(None);
        };
        offset = offset.saturating_add(100);
    }
    Ok(None)
}

async fn reconcile_international_authoritative(
    profile: &CoinbaseProfile,
    record: &OperationRecord,
    submission: &Submission,
    account: &CoinbaseAccount,
    order_id: &str,
    effective_client_id: &str,
) -> Result<Reconciliation> {
    let unknown = |reason: &str| Reconciliation {
        status: Status::Unknown,
        evidence: json!({"reason":reason,"replay":"forbidden_without_conclusive_reconciliation"}),
        verified: None,
    };
    let b = record.preview.as_ref().map(|p| &p.binding).ok_or_else(|| {
        error(
            "COINBASE_BINDING_INVALID",
            "International preparation is missing.",
        )
    })?;
    let (portfolio, client_id, instrument) = (
        b["portfolioId"].as_str().unwrap_or_default(),
        effective_client_id,
        b["instrument"].as_str().unwrap_or_default(),
    );
    let response = profile
        .request_for_account(
            &account.name,
            CoinbaseProduct::International,
            CoinbaseRoute::InternationalOrderScoped {
                order_id: order_id.into(),
                portfolio_id: portfolio.into(),
            },
            CoinbaseRequest::get(),
            None,
        )
        .await;
    let Ok(response) = response else {
        return Ok(unknown(
            "authoritative International order state is unavailable",
        ));
    };
    if !(200..300).contains(&response.status) {
        return Ok(unknown(
            "authoritative International order state is unavailable",
        ));
    }
    let order: Value = match serde_json::from_slice(&response.body) {
        Ok(v) => v,
        Err(_) => return Ok(unknown("International order JSON is invalid")),
    };
    if order.get("order_id").and_then(identifier_text).as_deref() != Some(order_id)
        || order.get("client_order_id").and_then(Value::as_str) != Some(client_id)
        || order.get("portfolio_uuid").and_then(Value::as_str) != Some(portfolio)
        || order.get("symbol").and_then(Value::as_str) != Some(instrument)
        || order.get("instrument_uuid") != b.get("instrumentUuid")
        || order.get("side").and_then(Value::as_str) != b["side"].as_str()
        || order.get("tif").and_then(Value::as_str) != b["timeInForce"].as_str().or(Some("GTC"))
    {
        return Ok(unknown(
            "authoritative International order identity differs from bound order",
        ));
    }
    let status = order
        .get("order_status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let working = status == "WORKING";
    if !working && status != "DONE" {
        return Ok(unknown("International order status is not recognized"));
    }
    let filled = Quantity::from_decimal(
        required_text(
            &order,
            "exec_qty",
            "International order omitted executed quantity",
        )?,
        36,
    )?;
    let fee_order = order
        .get("fee")
        .and_then(Value::as_str)
        .map(|v| Quantity::from_decimal(v, 36))
        .transpose()?;
    let mut fills: Vec<(String, String, Value)> = Vec::new();
    let mut offset = 0u32;
    let mut seen = std::collections::BTreeSet::new();
    let mut exhausted = false;
    for page in 0..1000 {
        let response = profile
            .request_for_account(
                &account.name,
                CoinbaseProduct::International,
                CoinbaseRoute::InternationalPortfolioFillsFiltered {
                    portfolio_id: portfolio.into(),
                    query: CoinbaseQuery {
                        order_id: Some(order_id.into()),
                        client_order_id: Some(client_id.into()),
                        result_limit: Some(100),
                        result_offset: Some(offset),
                        ..Default::default()
                    },
                },
                CoinbaseRequest::get(),
                None,
            )
            .await;
        let Ok(response) = response else {
            return Ok(unknown("International fill history is unavailable"));
        };
        if !(200..300).contains(&response.status) {
            return Ok(unknown("International fill history is unavailable"));
        }
        let body: Value = match serde_json::from_slice(&response.body) {
            Ok(v) => v,
            Err(_) => return Ok(unknown("International fill JSON is invalid")),
        };
        let Some(rows) = body.get("results").and_then(Value::as_array) else {
            return Ok(unknown("International fill response omitted results"));
        };
        for fill in rows {
            if fill.get("order_id").and_then(Value::as_str) != Some(order_id)
                || fill.get("client_order_id").and_then(Value::as_str) != Some(client_id)
                || fill.get("portfolio_uuid").and_then(Value::as_str) != Some(portfolio)
                || fill.get("symbol").and_then(Value::as_str) != Some(instrument)
                || fill.get("instrument_uuid") != b.get("instrumentUuid")
                || fill.get("side").and_then(Value::as_str) != b["side"].as_str()
            {
                return Ok(unknown(
                    "International fill did not attest exact portfolio/order/product/side",
                ));
            }
            let id = required_text(fill, "fill_id", "International fill omitted stable fill ID")?;
            let digest = hex::encode(sha2::Sha256::digest(
                gofin_domain::canonical_json(fill)?.as_bytes(),
            ));
            if let Some(old) = fills.iter().find(|(prior, _, _)| prior == id) {
                if old.1 != digest {
                    return Ok(unknown(
                        "International fill ID has conflicting immutable evidence",
                    ));
                }
            } else {
                fills.push((id.to_owned(), digest, fill.clone()));
            }
        }
        if rows.is_empty() || rows.len() < 100 {
            exhausted = true;
            break;
        }
        if page == 999 || !seen.insert(offset) {
            return Ok(unknown(
                "International fill pagination did not advance safely",
            ));
        }
        offset = offset.saturating_add(100);
    }
    if !exhausted {
        return Ok(unknown(
            "International fill pagination reached its page limit",
        ));
    }
    let mut base_total = Quantity::zero();
    let mut quote_total = Quantity::zero();
    let mut fee_total = Quantity::zero();
    let mut event_digests = std::collections::BTreeMap::new();
    let mut proof_events = Vec::new();
    for (id, digest, fill) in &fills {
        let size = required_text(fill, "fill_qty", "International fill omitted quantity")?;
        let price = required_text(fill, "fill_price", "International fill omitted price")?;
        let fee = Quantity::from_decimal(
            required_text(fill, "fee", "International fill omitted fee")?,
            36,
        )?;
        if required_text(fill, "fee_asset", "International fill omitted fee asset")?
            != b["quoteAssetName"].as_str().unwrap_or_default()
        {
            return Ok(unknown(
                "International spot fill fee denomination is not the bound quote asset",
            ));
        }
        let qty = Quantity::from_decimal(size, 36)?;
        base_total = base_total.checked_add(&qty)?;
        quote_total = quote_total.checked_add(&crate::profile::decimal_product_atoms_exact(
            size, price, 36,
        )?)?;
        fee_total = fee_total.checked_add(&fee)?;
        let event_id = format!("coinbaseinternational:fill:{id}");
        event_digests.insert(event_id.clone(), digest.clone());
        proof_events.push(json!({"eventId":event_id,"fill":fill}));
    }
    let fee_matches = fee_order.map_or(fee_total.is_zero() && filled.is_zero(), |fee| {
        fee == fee_total
    });
    if filled != base_total || !fee_matches {
        return Ok(unknown(
            "Complete International fills do not match authoritative executed quantity or fee total",
        ));
    }
    let side = b["side"].as_str().unwrap_or_default();
    let (spent_asset, received_asset, spent, received) = if side == "BUY" {
        (
            provider_asset(
                CoinbaseProduct::International,
                b["quoteAssetId"].as_str().unwrap_or_default(),
            ),
            provider_asset(
                CoinbaseProduct::International,
                b["baseAssetId"].as_str().unwrap_or_default(),
            ),
            quote_total.clone(),
            base_total.clone(),
        )
    } else {
        (
            provider_asset(
                CoinbaseProduct::International,
                b["baseAssetId"].as_str().unwrap_or_default(),
            ),
            provider_asset(
                CoinbaseProduct::International,
                b["quoteAssetId"].as_str().unwrap_or_default(),
            ),
            base_total.clone(),
            quote_total.clone(),
        )
    };
    let fee_asset = provider_asset(
        CoinbaseProduct::International,
        b["quoteAssetId"].as_str().unwrap_or_default(),
    );
    let fee_treatment = if side == "SELL" && quote_total >= fee_total {
        CostTreatment::DeductedFromCredit
    } else {
        CostTreatment::AdditionalDebit
    };
    let resource = account.resource.clone();
    let mut effects = Vec::new();
    if !fills.is_empty() {
        effects.push(FinancialEffect::Conversion {
            resource: resource.clone(),
            spent: Amount {
                asset: spent_asset,
                atoms: spent.clone(),
            },
            received: Amount {
                asset: received_asset,
                atoms: received.clone(),
            },
        });
        if !fee_total.is_zero() {
            effects.push(FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: Amount {
                    asset: fee_asset,
                    atoms: fee_total.clone(),
                },
                category: "coinbaseinternational-spot-fee".into(),
                treatment: fee_treatment,
            });
        }
    }
    let max_spend = Quantity::from_decimal(
        record.preview.as_ref().unwrap().input["maximumSpend"]
            .as_str()
            .unwrap_or("0"),
        36,
    )?;
    let max_fee = Quantity::from_decimal(
        record.preview.as_ref().unwrap().input["maximumFee"]
            .as_str()
            .unwrap_or("0"),
        36,
    )?;
    let min_receive = Quantity::from_decimal(
        record.preview.as_ref().unwrap().input["minimumReceive"]
            .as_str()
            .unwrap_or("0"),
        36,
    )?;
    let net = if side == "SELL" && quote_total >= fee_total {
        quote_total.checked_sub(&fee_total)?
    } else {
        received.clone()
    };
    let within = spent <= max_spend && fee_total <= max_fee && net >= min_receive;
    let status = if working {
        Status::Pending
    } else if within && !fills.is_empty() {
        Status::Completed
    } else {
        Status::Failed
    };
    let previous = record.effects.as_ref().and_then(|e| {
        SettlementProgress::from_evidence(&e.evidence)
            .ok()
            .flatten()
    });
    if let Some(old) = &previous {
        for (id, digest) in &old.event_digests {
            if event_digests.get(id) != Some(digest) {
                return Ok(unknown(
                    "previous International fill evidence disappeared or changed",
                ));
            }
        }
    }
    let snapshot_id = gofin_domain::binding_hash(
        &json!({"orderId":order_id,"status":status,"events":event_digests,"base":base_total,"quote":quote_total,"fees":fee_total}),
    )?;
    let progress = SettlementProgress {
        version: 1,
        sequence: previous.as_ref().map_or(1, |p| {
            if p.snapshot_id == snapshot_id {
                p.sequence
            } else {
                p.sequence + 1
            }
        }),
        snapshot_id,
        event_digests,
        position_orders: vec![],
    };
    let evidence = json!({"order":order,"reconciledOrderId":order_id,"proofEvents":proof_events,"settlement_progress":progress,"fillCount":fills.len(),"baseTotal":base_total,"quoteTotal":quote_total,"feeTotal":fee_total,"withinAuthorizedBounds":within});
    let mut verified = None;
    if !effects.is_empty() || status.terminal() {
        let mut bundle = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects,
            evidence: evidence.clone(),
            observed_at: gofin_domain::now(),
            proof_digest: String::new(),
        };
        bundle.proof_digest = gofin_domain::verified_effect_digest(&bundle)?;
        verified = Some(bundle);
    }
    Ok(Reconciliation {
        status,
        evidence,
        verified,
    })
}

async fn prepare_exchange_order(
    profile: &CoinbaseProfile,
    input: &Value,
    allow_current_balance_shortfall: bool,
) -> Result<PreparedOperation> {
    let validator = jsonschema::validator_for(&create_order_schema()).map_err(|_| {
        error(
            "COINBASE_CONTRACT_INVALID",
            "The Exchange order schema is invalid.",
        )
    })?;
    if !validator.is_valid(input) {
        return Err(error(
            "INVALID_INPUT",
            "Exchange order input does not match its closed contract.",
        ));
    }
    let time_in_force = input
        .get("timeInForce")
        .and_then(Value::as_str)
        .unwrap_or("GTC");
    let post_only = input
        .get("postOnly")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if post_only && time_in_force != "GTC" {
        return Err(error(
            "INVALID_INPUT",
            "Exchange post-only orders require GTC time in force.",
        ));
    }
    let account_name = input["account"].as_str().expect("schema");
    let account = profile
        .accounts()
        .iter()
        .find(|a| a.name == account_name && a.product == CoinbaseProduct::Exchange)
        .ok_or_else(|| {
            error(
                "COINBASE_ACCOUNT_UNKNOWN",
                "Select an enrolled Coinbase Exchange profile.",
            )
        })?;
    if !account.identity_verified || account.physical_identity.trim().is_empty() {
        return Err(error(
            "COINBASE_IDENTITY_UNVERIFIED",
            "Exchange order preparation requires an enrolled profile identity.",
        ));
    }

    let product_id = input["productId"].as_str().expect("schema");
    let descriptor = crate::reads::descriptor("coinbaseexchange.read-public-products-id")
        .ok_or_else(|| {
            error(
                "COINBASE_ROUTE_UNSUPPORTED",
                "Pinned Exchange product metadata route is unavailable.",
            )
        })?;
    let product_input = json!({"account":account_name,"id":product_id});
    let product_response = profile
        .request_for_account(
            account_name,
            CoinbaseProduct::Exchange,
            crate::reads::endpoint(descriptor, &product_input),
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    let product_value = parse_required_json(
        product_response,
        "Exchange product metadata could not be verified.",
    )?;
    let product = product_value.get("product").unwrap_or(&product_value);
    if product
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| id != product_id)
        || product
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status != "online")
        || product.get("trading_disabled").and_then(Value::as_bool) == Some(true)
    {
        return Err(error(
            "COINBASE_PRODUCT_UNAVAILABLE",
            "The Exchange product is not enabled for trading.",
        ));
    }
    let base = required_text(
        product,
        "base_currency",
        "Exchange product omitted its base currency identity.",
    )?;
    let quote = required_text(
        product,
        "quote_currency",
        "Exchange product omitted its quote currency identity.",
    )?;
    let base_increment = required_text(
        product,
        "base_increment",
        "Exchange product omitted its base increment.",
    )?;
    let price_increment = required_text(
        product,
        "price_increment",
        "Exchange product omitted its price increment.",
    )?;
    let min_size = product
        .get("base_min_size")
        .and_then(Value::as_str)
        .unwrap_or("0");
    let size = input["baseSize"].as_str().expect("schema");
    let price = input["limitPrice"].as_str().expect("schema");
    let maximum_spend = input["maximumSpend"].as_str().expect("schema");
    let minimum_receive = input["minimumReceive"].as_str().expect("schema");
    let maximum_fee = input["maximumFee"].as_str().expect("schema");
    if !multiple(size, base_increment)?
        || !multiple(price, price_increment)?
        || !decimal_at_least(size, min_size)?
        || !positive(size)?
        || !positive(price)?
    {
        return Err(error(
            "COINBASE_INCREMENT_INVALID",
            "Order size or price is below or not aligned to the Exchange product increments.",
        ));
    }

    let notional = crate::profile::decimal_product_atoms_exact(size, price, 36)?;
    let notional_text = notional.to_decimal(36);
    let side = input["side"].as_str().expect("schema");
    let principal = if side == "BUY" { &notional_text } else { size };
    if !decimal_at_least(maximum_spend, principal)? {
        return Err(error(
            "COINBASE_SPEND_CAP_TOO_LOW",
            "Maximum spend is below the exact limit-order principal.",
        ));
    }
    if side == "BUY" && !decimal_at_least(size, minimum_receive)? {
        return Err(error(
            "COINBASE_RECEIVE_FLOOR_INVALID",
            "Buy minimum receive exceeds the requested base quantity.",
        ));
    }

    let fee_response = profile
        .request_for_account(
            account_name,
            CoinbaseProduct::Exchange,
            CoinbaseRoute::ExchangeFees,
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    let fees = parse_required_json(
        fee_response,
        "The Exchange fee schedule could not be verified.",
    )?;
    let maker_rate = required_text(
        &fees,
        "maker_fee_rate",
        "Exchange fee response omitted the maker rate.",
    )?;
    let taker_rate = required_text(
        &fees,
        "taker_fee_rate",
        "Exchange fee response omitted the taker rate.",
    )?;
    let rate = if Quantity::from_decimal(maker_rate, 36)? >= Quantity::from_decimal(taker_rate, 36)?
    {
        maker_rate
    } else {
        taker_rate
    };
    let fee = crate::profile::decimal_product_atoms_exact(&notional_text, rate, 36)?;
    let cap_fee = Quantity::from_decimal(maximum_fee, 36)?;
    if fee > cap_fee {
        return Err(error(
            "COINBASE_FEE_CAP_TOO_LOW",
            "The current worst-case Exchange fee exceeds the authorized fee cap.",
        ));
    }
    let minimum_receive_gross = if side == "SELL" {
        Quantity::from_decimal(minimum_receive, 36)?
            .checked_add(&fee)?
            .to_decimal(36)
    } else {
        minimum_receive.to_owned()
    };
    let debit_asset = provider_asset(
        CoinbaseProduct::Exchange,
        if side == "BUY" { quote } else { base },
    );
    let credit_asset = provider_asset(
        CoinbaseProduct::Exchange,
        if side == "BUY" { base } else { quote },
    );
    let debit_text = if side == "BUY" { &notional_text } else { size };
    let debit = Quantity::from_decimal(debit_text, 36)?;
    let fee_location = AccountingLocation::Cash {
        resource: account.resource.clone(),
    };
    let cost = CostBound {
        location: fee_location,
        amount: Amount {
            asset: provider_asset(CoinbaseProduct::Exchange, quote),
            atoms: fee.clone(),
        },
        treatment: if side == "SELL" {
            CostTreatment::DeductedFromCredit
        } else {
            CostTreatment::AdditionalDebit
        },
    };

    // Verify current spendable funds using Exchange's native available balances.
    // Holds are never treated as available and every row must attest this profile.
    let accounts_response = profile
        .request_for_account(
            account_name,
            CoinbaseProduct::Exchange,
            CoinbaseRoute::ExchangeAccounts,
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    let rows = parse_required_json(
        accounts_response,
        "Exchange balances could not be verified.",
    )?;
    let rows = rows.as_array().ok_or_else(|| {
        error(
            "COINBASE_RESPONSE_INVALID",
            "Exchange account list was not an array.",
        )
    })?;
    let spend_currency = if side == "BUY" { quote } else { base };
    let required_atoms = if side == "BUY" {
        debit.checked_add(&fee)?
    } else {
        debit.clone()
    };
    let mut available = None;
    for row in rows {
        if required_text(
            row,
            "profile_id",
            "Exchange account omitted its profile identity.",
        )? != account.physical_identity
        {
            return Err(error(
                "COINBASE_PROFILE_MISMATCH",
                "Exchange balance row did not belong to the enrolled profile.",
            ));
        }
        if row.get("currency").and_then(Value::as_str) == Some(spend_currency) {
            let amount = required_text(
                row,
                "available",
                "Exchange account omitted its spendable balance.",
            )?;
            available = Some(Quantity::from_decimal(amount, 36)?);
        }
    }
    if available.is_none()
        || available.is_some_and(|value| value < required_atoms) && !allow_current_balance_shortfall
    {
        return Err(error(
            "COINBASE_BALANCE_INSUFFICIENT",
            "Verified Exchange available balance does not cover this order and its maximum fee.",
        ));
    }

    let request_id = input["requestId"].as_str().expect("schema");
    let client_oid = stable_client_oid(&account.physical_identity, request_id);
    let mut body = serde_json::Map::new();
    body.insert("client_oid".into(), json!(client_oid));
    body.insert("product_id".into(), json!(product_id));
    body.insert("side".into(), json!(side.to_ascii_lowercase()));
    body.insert("type".into(), json!("limit"));
    body.insert("size".into(), json!(size));
    body.insert("price".into(), json!(price));
    body.insert("time_in_force".into(), json!(time_in_force));
    if let Some(post_only) = input.get("postOnly") {
        body.insert("post_only".into(), post_only.clone());
    }
    let exact_body = gofin_domain::canonical_json(&Value::Object(body))?;
    let debits = vec![ResourceAmount {
        resource: account.resource.clone(),
        amount: Amount {
            asset: debit_asset.clone(),
            atoms: debit.clone(),
        },
    }];
    let costs = vec![cost];
    let valuation = profile.financial_valuation(&debits, &costs).await?;
    let now = gofin_domain::now();
    let mut prepared = PreparedOperation {
        operation: "coinbaseexchange.create-order".into(),
        input: input.clone(),
        binding: json!({"version":1,"provider":"coinbaseexchange","account":account.name,"resource":account.resource,"physicalIdentity":account.physical_identity,"clientOrderId":client_oid,"productId":product_id,"baseCurrencyId":base,"quoteCurrencyId":quote,"baseSize":size,"limitPrice":price,"timeInForce":time_in_force,"feeRate":rate,"estimatedFee":fee.to_decimal(36),"requestBodyExact":exact_body}),
        summary: format!("Place Coinbase Exchange {side} limit {time_in_force} order for {size} {base} at {price} {quote}"),
        checks: vec![Check { name:"product_and_fee_metadata".into(), status:CheckStatus::Passed, detail:"Enabled Exchange product increments, spendable currency balance, and current worst-case fee rate were verified.".into() }],
        warnings: vec!["Exchange has no native preview route; execution price is capped by the exact limit order, but fill and settlement require reconciliation.".into()],
        observed_at: now,
        expires_at: (chrono::Utc::now()+chrono::Duration::seconds(45)).to_rfc3339(),
        valuation:Some(valuation),
        intent: FinancialIntent {
            debits,
            maximum_costs: costs,
            expected:vec![EffectConstraint::Conversion { resource:account.resource.clone(), input:debit_asset, maximum_input:debit, output:credit_asset, minimum_output:Quantity::from_decimal(&minimum_receive_gross,36)? }],
            locks:vec![account.resource.clone()],
        },
        operation_version:1,
        action_deadline:None,
    };
    gofin_domain::value_bounds::bind(&mut prepared);
    Ok(prepared)
}

/// The OpenAPI schema declares native order IDs as strings, while its examples
/// also show integer JSON tokens. Accept only exact integer number spellings.
fn identifier_text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned).or_else(|| {
        value.as_number().and_then(|number| {
            number
                .as_u64()
                .map(|n| n.to_string())
                .or_else(|| number.as_i64().map(|n| n.to_string()))
        })
    })
}
fn required_text<'a>(value: &'a Value, key: &str, message: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| error("COINBASE_RESPONSE_INVALID", message))
}
fn parse_required_json(response: CoinbaseResponse, message: &str) -> Result<Value> {
    if !(200..300).contains(&response.status) {
        return Err(error("COINBASE_REQUEST_FAILED", message));
    }
    serde_json::from_slice(&response.body).map_err(|_| {
        error(
            "COINBASE_RESPONSE_INVALID",
            "Coinbase returned invalid JSON.",
        )
    })
}
fn stable_client_oid(identity: &str, request_id: &str) -> String {
    let digest =
        sha2::Sha256::digest(format!("coinbaseexchange\0{identity}\0{request_id}").as_bytes());
    format!("fin-{}", hex::encode(&digest[..16]))
}
fn positive(value: &str) -> Result<bool> {
    Ok(!Quantity::from_decimal(value, 36)?.is_zero())
}
fn decimal_at_least(left: &str, right: &str) -> Result<bool> {
    Ok(Quantity::from_decimal(left, 36)? >= Quantity::from_decimal(right, 36)?)
}
fn multiple(value: &str, increment: &str) -> Result<bool> {
    let value = Quantity::from_decimal(value, 36)?.integer();
    let increment = Quantity::from_decimal(increment, 36)?.integer();
    Ok(increment != num_bigint::BigUint::default()
        && value % increment == num_bigint::BigUint::default())
}

fn parse_read_response(
    response: CoinbaseResponse,
    include_pagination: bool,
) -> Result<Option<Value>> {
    if !(200..300).contains(&response.status) {
        let body: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
        let native_code = body
            .get("message")
            .or_else(|| body.get("error"))
            .and_then(Value::as_str)
            .filter(|s| s.len() <= 80 && !s.chars().any(char::is_control));
        return Err(error(
            "COINBASE_REQUEST_FAILED",
            "Coinbase rejected the institutional read.",
        )
        .with_details(json!({"http_status":response.status,"native_code":native_code})));
    }
    let body: Value = serde_json::from_slice(&response.body).map_err(|_| {
        error(
            "COINBASE_RESPONSE_INVALID",
            "Coinbase returned invalid institutional JSON.",
        )
    })?;
    if include_pagination {
        Ok(Some(json!({
            "data":body,
            "pagination":{"after":response.pagination.after,"before":response.pagination.before}
        })))
    } else {
        Ok(Some(body))
    }
}

fn validate_identity(account: &CoinbaseAccount, route: &str, value: Value) -> Result<Value> {
    let matches = |row: &Value, names: &[&str], expected: &str| {
        names
            .iter()
            .find_map(|key| row.get(*key).and_then(Value::as_str))
            == Some(expected)
    };
    match (account.product, route) {
        (CoinbaseProduct::Exchange, "exchange_accounts") => {
            let rows = value.as_array().ok_or_else(|| {
                error(
                    "COINBASE_RESPONSE_INVALID",
                    "Exchange account list was not an array.",
                )
            })?;
            if rows
                .iter()
                .any(|row| !matches(row, &["profile_id"], &account.physical_identity))
            {
                return Err(error(
                    "COINBASE_PROFILE_MISMATCH",
                    "Exchange account rows did not all belong to the enrolled profile.",
                ));
            }
        }
        (CoinbaseProduct::Exchange, "exchange_account" | "exchange_order") => {
            if !matches(&value, &["profile_id"], &account.physical_identity) {
                return Err(error(
                    "COINBASE_PROFILE_MISMATCH",
                    "Exchange response did not attest the enrolled profile.",
                ));
            }
        }
        (CoinbaseProduct::International, "international_accounts") => {
            let rows = value
                .get("portfolios")
                .and_then(Value::as_array)
                .or_else(|| value.as_array())
                .ok_or_else(|| {
                    error(
                        "COINBASE_RESPONSE_INVALID",
                        "International portfolio response was not a portfolio list.",
                    )
                })?;
            let expected = account
                .portfolio_id
                .as_deref()
                .unwrap_or(&account.physical_identity);
            let Some(row) = rows
                .iter()
                .find(|row| matches(row, &["portfolio_id", "portfolio", "id"], expected))
            else {
                return Err(error(
                    "COINBASE_PORTFOLIO_MISMATCH",
                    "International response did not include the enrolled portfolio.",
                ));
            };
            return Ok(row.clone());
        }
        (CoinbaseProduct::International, "international_order") => {
            let expected = account
                .portfolio_id
                .as_deref()
                .unwrap_or(&account.physical_identity);
            if !matches(&value, &["portfolio_id", "portfolio"], expected) {
                return Err(error(
                    "COINBASE_PORTFOLIO_MISMATCH",
                    "International order did not attest the enrolled portfolio.",
                ));
            }
        }
        _ => {}
    }
    Ok(value)
}

fn error(code: &str, message: &str) -> GoFinError {
    GoFinError::new(
        code,
        "coinbase",
        message,
        "Check the enrolled account and retry the read.",
    )
}

#[allow(dead_code)]
fn _account_product(account: &CoinbaseAccount) -> CoinbaseProduct {
    account.product
}

#[allow(dead_code)]
fn _request_type_witness(_: CoinbaseEndpoint, _: CoinbaseRequest) {}
