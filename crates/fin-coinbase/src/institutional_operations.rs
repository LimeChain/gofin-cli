//! Prepared configuration operations for Coinbase International.
//!
//! This module only admits requests whose route and wire body are backed by
//! the pinned Coinbase REST map. A persisted request is never replayed after
//! an uncertain dispatch; reconciliation uses the captured receipt and a
//! portfolio-scoped native readback.
use crate::{CoinbaseAccount, CoinbaseDispatch, CoinbaseProduct, CoinbaseProfile};
use fin_domain::{
    Check, CheckStatus, Definition, ExecutionActor, FinError, FinancialIntent, OperationKind,
    OperationRecord, PreparedOperation, Reconciliation, Result, Status, Submission,
};
use fin_exchange::coinbase::{CoinbaseRequest, CoinbaseRoute};
use serde_json::{Value, json};
use sha2::Digest;

const OP_SET_MARGIN: &str = "coinbaseinternational.set-margin-override";
const OP_SET_CROSS_COLLATERAL: &str = "coinbaseinternational.set-cross-collateral";
const OP_SET_AUTO_MARGIN: &str = "coinbaseinternational.set-auto-margin";
const OP_CREATE_PORTFOLIO: &str = "coinbaseinternational.create-portfolio";
const OP_UPDATE_PORTFOLIO: &str = "coinbaseinternational.update-portfolio";
const INTL_READS: [(&str, &str, &str); 6] = [
    (
        "coinbaseinternational.get-portfolio-detail",
        "portfolio detail",
        "GET /api/v1/portfolios/{portfolio}/detail",
    ),
    (
        "coinbaseinternational.get-balances",
        "portfolio balances",
        "GET /api/v1/portfolios/{portfolio}/balances",
    ),
    (
        "coinbaseinternational.get-positions",
        "portfolio positions",
        "GET /api/v1/portfolios/{portfolio}/positions",
    ),
    (
        "coinbaseinternational.get-loans",
        "portfolio loans",
        "GET /api/v1/portfolios/{portfolio}/loans",
    ),
    (
        "coinbaseinternational.get-loan",
        "portfolio loan",
        "GET /api/v1/portfolios/{portfolio}/loans/{asset}",
    ),
    (
        "coinbaseinternational.get-loan-availability",
        "loan availability",
        "GET /api/v1/portfolios/{portfolio}/loans/{asset}/availability",
    ),
];

pub(super) fn definitions() -> Vec<Definition> {
    let mut definitions = vec![Definition {
        name: OP_SET_MARGIN.into(),
        description: "Set the margin override for one enrolled Coinbase International portfolio.".into(),
        kind: OperationKind::Configuration,
        execution_actor: ExecutionActor::HumanInteractive,
        input_schema: set_margin_schema(),
        output_schema: json!({"type":"object"}),
        error_schema: json!({"type":"object"}),
        examples: vec![],
        coverage: "Sets the exact portfolio-wide margin override using Coinbase International's native portfolio margin endpoint. The portfolio identity and prior setting are captured before approval.".into(),
        completion: "Completes only when the captured native response is successful and GET /api/v1/portfolios/{portfolio}/detail reports the exact requested margin override. An uncertain POST is never replayed.".into(),
        semantics: json!({"product":"coinbaseinternational","route":"POST /api/v1/portfolios/margin","readback":"GET /api/v1/portfolios/{portfolio}/detail","clientVersion":"ccxt 4.5.85"}),
    }];
    definitions.push(portfolio_definition(
        OP_CREATE_PORTFOLIO,
        "Create",
        "POST /api/v1/portfolios",
        portfolio_create_schema(),
    ));
    for (name, label, route) in INTL_READS {
        definitions.push(Definition { name:name.into(),description:format!("Read exact Coinbase International {label}."),kind:OperationKind::Read,execution_actor:ExecutionActor::AgentReadable,input_schema:intl_read_schema(name),output_schema:json!({"type":"object"}),error_schema:json!({"type":"object"}),examples:vec![],coverage:format!("Returns the native authenticated {label} response for one exact enrolled International portfolio."),completion:"The response is returned only after a successful native REST read; all amounts remain venue-native observations.".into(),semantics:json!({"product":"coinbaseinternational","route":route,"clientVersion":"ccxt 4.5.85"}) });
    }
    definitions.push(portfolio_definition(
        OP_UPDATE_PORTFOLIO,
        "Update",
        "PUT /api/v1/portfolios/{portfolio}",
        portfolio_update_schema(),
    ));
    for (name, label, route) in [
        (
            OP_SET_CROSS_COLLATERAL,
            "cross collateral",
            "POST /api/v1/portfolios/{portfolio}/cross-collateral-enabled",
        ),
        (
            OP_SET_AUTO_MARGIN,
            "auto margin",
            "POST /api/v1/portfolios/{portfolio}/auto-margin-enabled",
        ),
    ] {
        definitions.push(Definition {
            name: name.into(),
            description: format!("Enable or disable {label} for one enrolled Coinbase International portfolio."),
            kind: OperationKind::Configuration,
            execution_actor: ExecutionActor::HumanInteractive,
            input_schema: set_flag_schema(),
            output_schema: json!({"type":"object"}),
            error_schema: json!({"type":"object"}),
            examples: vec![],
            coverage: format!("Sets the requested {label} state using its native International portfolio endpoint after verifying the current state."),
            completion: format!("Completes only when the captured native response succeeds and the exact portfolio readback confirms the requested {label} state. An uncertain POST is never replayed."),
            semantics: json!({"product":"coinbaseinternational","route":route,"readback":"GET /api/v1/portfolios","clientVersion":"ccxt 4.5.85"}),
        });
    }
    definitions
}

pub(super) async fn prepare(
    profile: &CoinbaseProfile,
    operation: &str,
    input: &Value,
) -> Result<Option<PreparedOperation>> {
    match operation {
        OP_SET_MARGIN => Ok(Some(prepare_margin(profile, input).await?)),
        OP_SET_CROSS_COLLATERAL | OP_SET_AUTO_MARGIN => {
            Ok(Some(prepare_flag(profile, operation, input).await?))
        }
        OP_CREATE_PORTFOLIO | OP_UPDATE_PORTFOLIO => {
            Ok(Some(prepare_portfolio(profile, operation, input).await?))
        }
        _ => Ok(None),
    }
}

pub(super) async fn read(
    profile: &CoinbaseProfile,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>> {
    let Some((name, _, _)) = INTL_READS.iter().find(|(name, _, _)| *name == operation) else {
        return Ok(None);
    };
    let validator = jsonschema::validator_for(&intl_read_schema(name)).map_err(|_| {
        error(
            "COINBASE_CONTRACT_INVALID",
            "International read schema is invalid.",
        )
    })?;
    if !validator.is_valid(input) {
        return Err(error(
            "INVALID_INPUT",
            "International read input does not match its closed schema.",
        ));
    }
    let account = enrolled_account(profile, input["account"].as_str().expect("schema"))?;
    let portfolio = account
        .portfolio_id
        .as_deref()
        .unwrap_or(&account.physical_identity);
    if input["portfolioId"].as_str() != Some(portfolio) {
        return Err(error(
            "COINBASE_PORTFOLIO_MISMATCH",
            "Requested portfolio differs from the enrolled International portfolio.",
        ));
    }
    let endpoint = match operation {
        "coinbaseinternational.get-portfolio-detail" => {
            CoinbaseRoute::InternationalPortfolioDetail {
                portfolio_id: portfolio.into(),
            }
        }
        "coinbaseinternational.get-balances" => CoinbaseRoute::InternationalPortfolioBalances {
            portfolio_id: portfolio.into(),
        },
        "coinbaseinternational.get-positions" => CoinbaseRoute::InternationalPortfolioPositions {
            portfolio_id: portfolio.into(),
        },
        "coinbaseinternational.get-loans" => CoinbaseRoute::InternationalPortfolioLoans {
            portfolio_id: portfolio.into(),
        },
        "coinbaseinternational.get-loan" => CoinbaseRoute::InternationalPortfolioLoan {
            portfolio_id: portfolio.into(),
            asset: input["asset"].as_str().expect("schema").into(),
        },
        "coinbaseinternational.get-loan-availability" => {
            CoinbaseRoute::InternationalPortfolioLoanAvailability {
                portfolio_id: portfolio.into(),
                asset: input["asset"].as_str().expect("schema").into(),
            }
        }
        _ => return Ok(None),
    };
    let response = profile
        .request_for_account(
            &account.name,
            CoinbaseProduct::International,
            endpoint,
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    if !(200..300).contains(&response.status) {
        return Err(error(
            "COINBASE_REQUEST_FAILED",
            "Coinbase rejected the International portfolio read.",
        ));
    }
    serde_json::from_slice(&response.body)
        .map(Some)
        .map_err(|_| {
            error(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned invalid International portfolio JSON.",
            )
        })
}

pub(super) async fn refresh(
    profile: &CoinbaseProfile,
    approved: &PreparedOperation,
) -> Result<Option<PreparedOperation>> {
    if matches!(
        approved.operation.as_str(),
        OP_SET_CROSS_COLLATERAL | OP_SET_AUTO_MARGIN
    ) {
        let fresh = prepare_flag(profile, &approved.operation, &approved.input).await?;
        if [
            "account",
            "physicalIdentity",
            "portfolioId",
            "previousEnabled",
            "requestBodyExact",
        ]
        .iter()
        .any(|key| fresh.binding[*key] != approved.binding[*key])
        {
            return Err(error(
                "COINBASE_PREVIEW_CHANGED",
                "International portfolio flag or current setting changed after approval.",
            ));
        }
        return Ok(Some(fresh));
    }
    if matches!(
        approved.operation.as_str(),
        OP_CREATE_PORTFOLIO | OP_UPDATE_PORTFOLIO
    ) {
        let fresh = prepare_portfolio(profile, &approved.operation, &approved.input).await?;
        let keys = if approved.operation == OP_CREATE_PORTFOLIO {
            vec!["account", "physicalIdentity", "requestBodyExact"]
        } else {
            vec![
                "account",
                "physicalIdentity",
                "portfolioId",
                "previousName",
                "requestBodyExact",
            ]
        };
        if keys
            .iter()
            .any(|key| fresh.binding[*key] != approved.binding[*key])
        {
            return Err(error(
                "COINBASE_PREVIEW_CHANGED",
                "International portfolio identity or current name changed after approval.",
            ));
        }
        return Ok(Some(fresh));
    }
    if approved.operation != OP_SET_MARGIN {
        return Ok(None);
    }
    let fresh = prepare_margin(profile, &approved.input).await?;
    for key in [
        "account",
        "physicalIdentity",
        "portfolioId",
        "previousMargin",
    ] {
        if fresh.binding[key] != approved.binding[key] {
            return Err(error(
                "COINBASE_PREVIEW_CHANGED",
                "International portfolio identity or its current margin changed after approval.",
            ));
        }
    }
    if fresh.binding["requestBodyExact"] != approved.binding["requestBodyExact"] {
        return Err(error(
            "COINBASE_PREVIEW_CHANGED",
            "The exact International margin request changed after approval.",
        ));
    }
    Ok(Some(fresh))
}

pub(super) async fn plan_submission(
    profile: &CoinbaseProfile,
    current: &PreparedOperation,
) -> Result<Option<fin_exchange::PlannedSubmission<CoinbaseDispatch>>> {
    if matches!(
        current.operation.as_str(),
        OP_SET_CROSS_COLLATERAL | OP_SET_AUTO_MARGIN
    ) {
        return Ok(Some(plan_flag(profile, current)?));
    }
    if matches!(
        current.operation.as_str(),
        OP_CREATE_PORTFOLIO | OP_UPDATE_PORTFOLIO
    ) {
        return Ok(Some(plan_portfolio(profile, current)?));
    }
    if current.operation != OP_SET_MARGIN {
        return Ok(None);
    }
    let account_name = current.binding["account"].as_str().ok_or_else(|| {
        error(
            "COINBASE_BINDING_INVALID",
            "International account is missing.",
        )
    })?;
    let account = enrolled_account(profile, account_name)?;
    let portfolio = account
        .portfolio_id
        .as_deref()
        .unwrap_or(&account.physical_identity);
    if current.binding["portfolioId"].as_str() != Some(portfolio)
        || current.binding["physicalIdentity"].as_str() != Some(&account.physical_identity)
    {
        return Err(error(
            "COINBASE_PORTFOLIO_MISMATCH",
            "The prepared International portfolio no longer matches its enrolled identity.",
        ));
    }
    let body = current.binding["requestBodyExact"]
        .as_str()
        .ok_or_else(|| {
            error(
                "COINBASE_BINDING_INVALID",
                "Exact margin request body is missing.",
            )
        })?
        .to_owned();
    let digest = hex::encode(sha2::Sha256::digest(body.as_bytes()));
    let submission = Submission {
        kind: "coinbase".into(),
        reference: format!("intl-margin:{}:{}", portfolio, digest),
        data: json!({"product":"coinbaseinternational","operation":"set-margin","binding":current.binding}),
    };
    Ok(Some(fin_exchange::PlannedSubmission {
        submission: submission.clone(),
        request: CoinbaseDispatch {
            product: CoinbaseProduct::International,
            endpoint: CoinbaseRoute::InternationalPortfolioMargin,
            account: account_name.into(),
            exact_body: body,
            submission,
            candidate: current.clone(),
        },
    }))
}

pub(super) async fn reconcile(
    profile: &CoinbaseProfile,
    record: &OperationRecord,
    submission: &Submission,
) -> Result<Option<Reconciliation>> {
    if matches!(
        record.operation.as_str(),
        OP_SET_CROSS_COLLATERAL | OP_SET_AUTO_MARGIN
    ) {
        return Ok(Some(reconcile_flag(profile, record, submission).await?));
    }
    if matches!(
        record.operation.as_str(),
        OP_CREATE_PORTFOLIO | OP_UPDATE_PORTFOLIO
    ) {
        return Ok(Some(
            reconcile_portfolio(profile, record, submission).await?,
        ));
    }
    if record.operation != OP_SET_MARGIN {
        return Ok(None);
    }
    let binding = &submission.data["binding"];
    if record
        .preview
        .as_ref()
        .is_none_or(|p| p.operation != OP_SET_MARGIN || p.binding != *binding)
    {
        return Ok(Some(unknown_reconciliation(
            "International margin submission differs from its exact protected preview.",
        )));
    }
    if !binding_matches_current_account(profile, binding) {
        return Ok(Some(unknown_reconciliation(
            "Current enrolled International account identity no longer matches the durable margin binding.",
        )));
    }
    let Some(dispatch) = record.dispatch_evidence.as_ref() else {
        return Ok(Some(unknown_reconciliation(
            "International margin change has no captured response and will not be replayed.",
        )));
    };
    let Some((status, bytes)) = captured_response(dispatch) else {
        return Ok(Some(unknown_reconciliation(
            "International margin response bytes or digest are invalid.",
        )));
    };
    let receipt: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if (400..500).contains(&status) {
        return Ok(Some(Reconciliation {
            status: Status::Failed,
            evidence: json!({"httpStatus":status,"receipt":receipt,"reason":"Coinbase rejected the margin request"}),
            verified: None,
        }));
    }
    if !(200..300).contains(&status) || !receipt.is_object() {
        return Ok(Some(unknown_reconciliation(
            "Captured International margin receipt does not prove native success.",
        )));
    }
    let account = binding["account"].as_str().unwrap_or_default();
    let portfolio = binding["portfolioId"].as_str().unwrap_or_default();
    let response = profile
        .request_for_account(
            account,
            CoinbaseProduct::International,
            CoinbaseRoute::InternationalPortfolioDetail {
                portfolio_id: portfolio.into(),
            },
            CoinbaseRequest::get(),
            None,
        )
        .await;
    let readback = match response {
        Ok(response) if (200..300).contains(&response.status) => {
            serde_json::from_slice::<Value>(&response.body).ok()
        }
        _ => None,
    };
    let Some(readback) = readback else {
        return Ok(Some(unknown_reconciliation(
            "Native International margin readback is unavailable.",
        )));
    };
    if !margin_matches(
        &readback,
        binding["marginOverride"].as_str().unwrap_or_default(),
    )? {
        return Ok(Some(unknown_reconciliation(
            "Native International margin readback does not prove the requested setting.",
        )));
    }
    Ok(Some(Reconciliation {
        status: Status::Completed,
        evidence: json!({"httpStatus":status,"receipt":receipt,"readback":readback,"marginOverride":binding["marginOverride"],"exactBytesDigest":dispatch["bodySha256"]}),
        verified: None,
    }))
}

async fn prepare_margin(profile: &CoinbaseProfile, input: &Value) -> Result<PreparedOperation> {
    let validator = jsonschema::validator_for(&set_margin_schema()).map_err(|_| {
        error(
            "COINBASE_CONTRACT_INVALID",
            "The International margin schema is invalid.",
        )
    })?;
    if !validator.is_valid(input) {
        return Err(error(
            "INVALID_INPUT",
            "Margin input does not match its closed schema.",
        ));
    }
    let account = enrolled_account(profile, input["account"].as_str().expect("schema"))?;
    let portfolio = account
        .portfolio_id
        .as_deref()
        .unwrap_or(&account.physical_identity);
    if input["portfolioId"].as_str() != Some(portfolio) {
        return Err(error(
            "COINBASE_PORTFOLIO_MISMATCH",
            "Requested portfolio differs from the enrolled International portfolio.",
        ));
    }
    let margin = input["marginOverride"].as_str().expect("schema");
    let value = profile
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
    if !(200..300).contains(&value.status) {
        return Err(error(
            "COINBASE_READ_UNAVAILABLE",
            "Current International portfolio margin could not be read.",
        ));
    }
    let detail: Value = serde_json::from_slice(&value.body).map_err(|_| {
        error(
            "COINBASE_RESPONSE_INVALID",
            "International portfolio detail was invalid JSON.",
        )
    })?;
    let previous = detail.pointer("/summary/margin_override").ok_or_else(|| {
        error(
            "COINBASE_RESPONSE_INVALID",
            "International portfolio detail omitted its native margin override.",
        )
    })?;
    let previous_text = numeric_text(previous).ok_or_else(|| {
        error(
            "COINBASE_RESPONSE_INVALID",
            "Native margin override was not numeric.",
        )
    })?;
    let _ = fin_domain::Quantity::from_decimal(margin, 18)?;
    let body =
        fin_domain::canonical_json(&json!({"portfolio":portfolio,"margin_override":margin}))?;
    Ok(PreparedOperation {
        operation: OP_SET_MARGIN.into(),
        input: input.clone(),
        binding: json!({"version":1,"provider":"coinbaseinternational","account":account.name,"resource":account.resource,"physicalIdentity":account.physical_identity,"portfolioId":portfolio,"requestId":input["requestId"],"previousMargin":previous_text,"marginOverride":margin,"requestBodyExact":body}),
        summary: format!("Set Coinbase International portfolio {portfolio} margin override to {margin}"),
        checks: vec![Check { name:"enrolled_portfolio_and_current_margin".into(),status:CheckStatus::Passed,detail:"The enrolled International portfolio and its native current margin override were verified.".into() }],
        warnings: vec!["Margin override changes portfolio liquidation and leverage requirements. A lost response is never replayed; completion requires exact native readback.".into()],
        observed_at: fin_domain::now(),
        expires_at: (chrono::Utc::now()+chrono::Duration::seconds(45)).to_rfc3339(),
        valuation: None,
        intent: FinancialIntent { debits:vec![],maximum_costs:vec![],expected:vec![],locks:vec![] },
        operation_version:1,
        action_deadline:None,
    })
}

fn portfolio_definition(name: &str, verb: &str, route: &str, schema: Value) -> Definition {
    Definition {
        name:name.into(),description:format!("{verb} one Coinbase International portfolio."),kind:OperationKind::Configuration,
        execution_actor:ExecutionActor::HumanInteractive,input_schema:schema,output_schema:json!({"type":"object"}),error_schema:json!({"type":"object"}),examples:vec![],
        coverage:format!("{verb}s an International portfolio through its native REST endpoint after validating enrolled identity and current state."),
        completion:"Completes only after a captured native receipt and portfolio-list readback prove the exact requested portfolio state. Uncertain writes are never replayed.".into(),
        semantics:json!({"product":"coinbaseinternational","route":route,"readback":"GET /api/v1/portfolios","clientVersion":"ccxt 4.5.85"}),
    }
}

async fn prepare_portfolio(
    profile: &CoinbaseProfile,
    operation: &str,
    input: &Value,
) -> Result<PreparedOperation> {
    let schema = if operation == OP_CREATE_PORTFOLIO {
        portfolio_create_schema()
    } else {
        portfolio_update_schema()
    };
    let validator = jsonschema::validator_for(&schema).map_err(|_| {
        error(
            "COINBASE_CONTRACT_INVALID",
            "International portfolio configuration schema is invalid.",
        )
    })?;
    if !validator.is_valid(input) {
        return Err(error(
            "INVALID_INPUT",
            "Portfolio configuration input does not match its closed schema.",
        ));
    }
    let account = enrolled_account(profile, input["account"].as_str().expect("schema"))?;
    let rows = list_portfolios(profile, &account.name).await?;
    let name = input["name"].as_str().expect("schema");
    let (portfolio, previous_name) = if operation == OP_CREATE_PORTFOLIO {
        if rows
            .as_array()
            .or_else(|| rows.get("portfolios").and_then(Value::as_array))
            .into_iter()
            .flatten()
            .any(|r| r.get("name").and_then(Value::as_str) == Some(name))
        {
            return Err(error(
                "COINBASE_PORTFOLIO_EXISTS",
                "An International portfolio with that exact name already exists.",
            ));
        }
        (None, None)
    } else {
        let id = account
            .portfolio_id
            .as_deref()
            .unwrap_or(&account.physical_identity);
        if input["portfolioId"].as_str() != Some(id) {
            return Err(error(
                "COINBASE_PORTFOLIO_MISMATCH",
                "Only the exact enrolled International portfolio can be updated.",
            ));
        }
        let row = portfolio_row(&rows, id).ok_or_else(|| {
            error(
                "COINBASE_PORTFOLIO_MISMATCH",
                "The enrolled portfolio was missing from the native list.",
            )
        })?;
        let prev = row.get("name").and_then(Value::as_str).ok_or_else(|| {
            error(
                "COINBASE_RESPONSE_INVALID",
                "Native portfolio omitted its name.",
            )
        })?;
        if prev == name {
            return Err(error(
                "COINBASE_CONFIGURATION_NOOP",
                "The International portfolio already has that name.",
            ));
        }
        (Some(id), Some(prev))
    };
    let body = fin_domain::canonical_json(&json!({"name":name}))?;
    Ok(PreparedOperation {
        operation:operation.into(),input:input.clone(),
        binding:json!({"version":1,"provider":"coinbaseinternational","account":account.name,"resource":account.resource,"physicalIdentity":account.physical_identity,"portfolioId":portfolio,"previousName":previous_name,"name":name,"requestId":input["requestId"],"requestBodyExact":body}),
        summary:if let Some(id)=portfolio {format!("Rename Coinbase International portfolio {id} to {name}")} else {format!("Create Coinbase International portfolio {name}")},
        checks:vec![Check{name:"enrolled_portfolio_state".into(),status:CheckStatus::Passed,detail:"International portfolio identity and current name state were verified.".into()}],
        warnings:vec!["Portfolio configuration is a human-approved account setting. A lost response is never replayed; exact native list readback is required.".into()],
        observed_at:fin_domain::now(),expires_at:(chrono::Utc::now()+chrono::Duration::seconds(45)).to_rfc3339(),valuation:None,
        intent:FinancialIntent{debits:vec![],maximum_costs:vec![],expected:vec![],locks:vec![]},operation_version:1,action_deadline:None,
    })
}

fn plan_portfolio(
    profile: &CoinbaseProfile,
    current: &PreparedOperation,
) -> Result<fin_exchange::PlannedSubmission<CoinbaseDispatch>> {
    let account_name = current.binding["account"].as_str().ok_or_else(|| {
        error(
            "COINBASE_BINDING_INVALID",
            "International account is missing.",
        )
    })?;
    let account = enrolled_account(profile, account_name)?;
    if current.binding["physicalIdentity"].as_str() != Some(&account.physical_identity) {
        return Err(error(
            "COINBASE_PORTFOLIO_MISMATCH",
            "International account identity changed after preparation.",
        ));
    }
    let (endpoint, operation_tag) = if current.operation == OP_CREATE_PORTFOLIO {
        (
            CoinbaseRoute::InternationalPortfolioCreate,
            "create-portfolio",
        )
    } else {
        let id = current.binding["portfolioId"].as_str().ok_or_else(|| {
            error(
                "COINBASE_BINDING_INVALID",
                "International portfolio ID is missing.",
            )
        })?;
        if account
            .portfolio_id
            .as_deref()
            .unwrap_or(&account.physical_identity)
            != id
        {
            return Err(error(
                "COINBASE_PORTFOLIO_MISMATCH",
                "Update target differs from the enrolled portfolio.",
            ));
        }
        (
            CoinbaseRoute::InternationalPortfolioUpdate {
                portfolio_id: id.into(),
            },
            "update-portfolio",
        )
    };
    let body = current.binding["requestBodyExact"]
        .as_str()
        .ok_or_else(|| {
            error(
                "COINBASE_BINDING_INVALID",
                "Exact portfolio request is missing.",
            )
        })?
        .to_owned();
    let ref_id = current.binding["requestId"].as_str().unwrap_or_default();
    let submission = Submission {
        kind: "coinbase".into(),
        reference: format!(
            "intl-portfolio:{operation_tag}:{}:{ref_id}",
            account.physical_identity
        ),
        data: json!({"product":"coinbaseinternational","operation":operation_tag,"binding":current.binding}),
    };
    Ok(fin_exchange::PlannedSubmission {
        submission: submission.clone(),
        request: CoinbaseDispatch {
            product: CoinbaseProduct::International,
            endpoint,
            account: account_name.into(),
            exact_body: body,
            submission,
            candidate: current.clone(),
        },
    })
}

async fn reconcile_portfolio(
    profile: &CoinbaseProfile,
    record: &OperationRecord,
    submission: &Submission,
) -> Result<Reconciliation> {
    let binding = &submission.data["binding"];
    if record
        .preview
        .as_ref()
        .is_none_or(|p| p.operation != record.operation || p.binding != *binding)
    {
        return Ok(unknown_reconciliation(
            "International portfolio submission differs from its exact protected preview.",
        ));
    }
    if !binding_matches_current_account(profile, binding) {
        return Ok(unknown_reconciliation(
            "Current enrolled International account identity no longer matches the durable portfolio binding.",
        ));
    }
    let Some(dispatch) = record.dispatch_evidence.as_ref() else {
        return Ok(unknown_reconciliation(
            "International portfolio write has no captured response and will not be replayed.",
        ));
    };
    let Some((status, bytes)) = captured_response(dispatch) else {
        return Ok(unknown_reconciliation(
            "International portfolio response bytes or digest are invalid.",
        ));
    };
    let receipt: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if (400..500).contains(&status) {
        return Ok(Reconciliation {
            status: Status::Failed,
            evidence: json!({"httpStatus":status,"receipt":receipt,"reason":"Coinbase rejected portfolio configuration"}),
            verified: None,
        });
    }
    if !(200..300).contains(&status) || !receipt.is_object() {
        return Ok(unknown_reconciliation(
            "Captured International portfolio receipt does not prove native success.",
        ));
    }
    let account = binding["account"].as_str().unwrap_or_default();
    let rows = match list_portfolios(profile, account).await {
        Ok(v) => v,
        Err(_) => {
            return Ok(unknown_reconciliation(
                "International portfolio readback is unavailable.",
            ));
        }
    };
    let expected_name = binding["name"].as_str().unwrap_or_default();
    let found = if record.operation == OP_CREATE_PORTFOLIO {
        let native = receipt.get("portfolio").unwrap_or(&receipt);
        let id = ["portfolio_id", "portfolio_uuid", "id"]
            .iter()
            .find_map(|key| native.get(*key).and_then(Value::as_str));
        let Some(id) = id else {
            return Ok(unknown_reconciliation(
                "Portfolio-create receipt omitted its exact native identity.",
            ));
        };
        portfolio_row(&rows, id)
            .filter(|row| row.get("name").and_then(Value::as_str) == Some(expected_name))
    } else {
        let id = binding["portfolioId"].as_str().unwrap_or_default();
        portfolio_row(&rows, id)
            .filter(|row| row.get("name").and_then(Value::as_str) == Some(expected_name))
    };
    let Some(row) = found else {
        return Ok(unknown_reconciliation(
            "International portfolio list does not prove the exact requested portfolio state.",
        ));
    };
    Ok(Reconciliation {
        status: Status::Completed,
        evidence: json!({"httpStatus":status,"receipt":receipt,"portfolio":row,"exactBytesDigest":dispatch["bodySha256"]}),
        verified: None,
    })
}

fn portfolio_create_schema() -> Value {
    json!({"type":"object","required":["account","requestId","name"],"properties":{"account":{"type":"string","minLength":1,"maxLength":128},"requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},"name":{"type":"string","minLength":1,"maxLength":128}},"additionalProperties":false})
}
fn portfolio_update_schema() -> Value {
    json!({"type":"object","required":["account","requestId","portfolioId","name"],"properties":{"account":{"type":"string","minLength":1,"maxLength":128},"requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},"portfolioId":{"type":"string","minLength":1,"maxLength":128},"name":{"type":"string","minLength":1,"maxLength":128}},"additionalProperties":false})
}

async fn prepare_flag(
    profile: &CoinbaseProfile,
    operation: &str,
    input: &Value,
) -> Result<PreparedOperation> {
    let validator = jsonschema::validator_for(&set_flag_schema()).map_err(|_| {
        error(
            "COINBASE_CONTRACT_INVALID",
            "The International portfolio flag schema is invalid.",
        )
    })?;
    if !validator.is_valid(input) {
        return Err(error(
            "INVALID_INPUT",
            "Flag input does not match its closed schema.",
        ));
    }
    let account = enrolled_account(profile, input["account"].as_str().expect("schema"))?;
    let portfolio = account
        .portfolio_id
        .as_deref()
        .unwrap_or(&account.physical_identity);
    if input["portfolioId"].as_str() != Some(portfolio) {
        return Err(error(
            "COINBASE_PORTFOLIO_MISMATCH",
            "Requested portfolio differs from the enrolled International portfolio.",
        ));
    }
    let field = flag_field(operation)?;
    let portfolios = list_portfolios(profile, &account.name).await?;
    let current = portfolio_row(&portfolios, portfolio)
        .and_then(|row| row.get(field))
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            error(
                "COINBASE_RESPONSE_INVALID",
                "Native portfolio list omitted the current Boolean flag.",
            )
        })?;
    let enabled = input["enabled"].as_bool().expect("schema");
    let route = flag_route(operation, portfolio)?;
    // The official INTX Python request models encode `enabled` as a string;
    // preserve that native wire type while keeping Fin's public input Boolean.
    let body = fin_domain::canonical_json(&json!({"enabled":enabled.to_string()}))?;
    Ok(PreparedOperation {
        operation: operation.into(), input: input.clone(),
        binding: json!({"version":1,"provider":"coinbaseinternational","account":account.name,"resource":account.resource,"physicalIdentity":account.physical_identity,"portfolioId":portfolio,"requestId":input["requestId"],"previousEnabled":current,"enabled":enabled,"flagField":field,"route":route.path().map_err(|_| error("COINBASE_ROUTE_INVALID", "Portfolio flag route is invalid."))?,"requestBodyExact":body}),
        summary: format!("Set Coinbase International portfolio {portfolio} {field} to {enabled}"),
        checks: vec![Check {name:"enrolled_portfolio_and_current_flag".into(),status:CheckStatus::Passed,detail:"The enrolled International portfolio and its native current flag were verified.".into()}],
        warnings: vec!["Portfolio margin and collateral changes can alter liquidation and available collateral. A lost response is never replayed; completion requires exact native readback.".into()],
        observed_at:fin_domain::now(),expires_at:(chrono::Utc::now()+chrono::Duration::seconds(45)).to_rfc3339(),valuation:None,
        intent:FinancialIntent {debits:vec![],maximum_costs:vec![],expected:vec![],locks:vec![]},operation_version:1,action_deadline:None,
    })
}

fn plan_flag(
    profile: &CoinbaseProfile,
    current: &PreparedOperation,
) -> Result<fin_exchange::PlannedSubmission<CoinbaseDispatch>> {
    let account_name = current.binding["account"].as_str().ok_or_else(|| {
        error(
            "COINBASE_BINDING_INVALID",
            "International account is missing.",
        )
    })?;
    let account = enrolled_account(profile, account_name)?;
    let portfolio = account
        .portfolio_id
        .as_deref()
        .unwrap_or(&account.physical_identity);
    if current.binding["portfolioId"].as_str() != Some(portfolio)
        || current.binding["physicalIdentity"].as_str() != Some(&account.physical_identity)
    {
        return Err(error(
            "COINBASE_PORTFOLIO_MISMATCH",
            "The prepared International portfolio no longer matches its enrolled identity.",
        ));
    }
    let route = flag_route(&current.operation, portfolio)?;
    if current.binding["route"].as_str() != route.path().ok().as_deref() {
        return Err(error(
            "COINBASE_BINDING_INVALID",
            "Prepared portfolio flag route does not match the operation.",
        ));
    }
    let body = current.binding["requestBodyExact"]
        .as_str()
        .ok_or_else(|| {
            error(
                "COINBASE_BINDING_INVALID",
                "Exact portfolio flag request is missing.",
            )
        })?
        .to_owned();
    let digest = hex::encode(sha2::Sha256::digest(body.as_bytes()));
    let submission = Submission {
        kind: "coinbase".into(),
        reference: format!("intl-config:{}:{}", portfolio, digest),
        data: json!({"product":"coinbaseinternational","operation":current.operation,"binding":current.binding}),
    };
    Ok(fin_exchange::PlannedSubmission {
        submission: submission.clone(),
        request: CoinbaseDispatch {
            product: CoinbaseProduct::International,
            endpoint: route,
            account: account_name.into(),
            exact_body: body,
            submission,
            candidate: current.clone(),
        },
    })
}

async fn reconcile_flag(
    profile: &CoinbaseProfile,
    record: &OperationRecord,
    submission: &Submission,
) -> Result<Reconciliation> {
    let binding = &submission.data["binding"];
    if record
        .preview
        .as_ref()
        .is_none_or(|p| p.operation != record.operation || p.binding != *binding)
    {
        return Ok(unknown_reconciliation(
            "International flag submission differs from its exact protected preview.",
        ));
    }
    if !binding_matches_current_account(profile, binding) {
        return Ok(unknown_reconciliation(
            "Current enrolled International account identity no longer matches the durable flag binding.",
        ));
    }
    let Some(dispatch) = record.dispatch_evidence.as_ref() else {
        return Ok(unknown_reconciliation(
            "International flag change has no captured response and will not be replayed.",
        ));
    };
    let Some((status, bytes)) = captured_response(dispatch) else {
        return Ok(unknown_reconciliation(
            "International flag response bytes or digest are invalid.",
        ));
    };
    let receipt: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if (400..500).contains(&status) {
        return Ok(Reconciliation {
            status: Status::Failed,
            evidence: json!({"httpStatus":status,"receipt":receipt,"reason":"Coinbase rejected the International portfolio flag"}),
            verified: None,
        });
    }
    if !(200..300).contains(&status) || !receipt.is_object() {
        return Ok(unknown_reconciliation(
            "Captured International flag receipt does not prove native success.",
        ));
    }
    let account = binding["account"].as_str().unwrap_or_default();
    let portfolio = binding["portfolioId"].as_str().unwrap_or_default();
    let rows = match list_portfolios(profile, account).await {
        Ok(value) => value,
        Err(_) => {
            return Ok(unknown_reconciliation(
                "Native International portfolio flag readback is unavailable.",
            ));
        }
    };
    let enabled = portfolio_row(&rows, portfolio)
        .and_then(|row| row.get(binding["flagField"].as_str().unwrap_or_default()))
        .and_then(Value::as_bool);
    if enabled != binding["enabled"].as_bool() {
        return Ok(unknown_reconciliation(
            "Native International portfolio readback does not prove the requested flag.",
        ));
    }
    Ok(Reconciliation {
        status: Status::Completed,
        evidence: json!({"httpStatus":status,"receipt":receipt,"portfolio":portfolio_row(&rows,portfolio),"enabled":enabled,"exactBytesDigest":dispatch["bodySha256"]}),
        verified: None,
    })
}

async fn list_portfolios(profile: &CoinbaseProfile, account: &str) -> Result<Value> {
    let response = profile
        .request_for_account(
            account,
            CoinbaseProduct::International,
            CoinbaseRoute::InternationalAccounts,
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    if !(200..300).contains(&response.status) {
        return Err(error(
            "COINBASE_READ_UNAVAILABLE",
            "International portfolio list could not be read.",
        ));
    }
    serde_json::from_slice(&response.body).map_err(|_| {
        error(
            "COINBASE_RESPONSE_INVALID",
            "International portfolio list was invalid JSON.",
        )
    })
}

fn portfolio_row<'a>(value: &'a Value, portfolio: &str) -> Option<&'a Value> {
    value
        .as_array()
        .or_else(|| value.get("portfolios").and_then(Value::as_array))?
        .iter()
        .find(|row| {
            ["portfolio_id", "portfolio_uuid", "portfolio", "id"]
                .iter()
                .filter_map(|key| row.get(*key).and_then(Value::as_str))
                .any(|id| id == portfolio)
        })
}

fn flag_field(operation: &str) -> Result<&'static str> {
    match operation {
        OP_SET_CROSS_COLLATERAL => Ok("cross_collateral_enabled"),
        OP_SET_AUTO_MARGIN => Ok("auto_margin_enabled"),
        _ => Err(error(
            "COINBASE_OPERATION_UNKNOWN",
            "Unknown International portfolio flag operation.",
        )),
    }
}

fn flag_route(operation: &str, portfolio: &str) -> Result<CoinbaseRoute> {
    match operation {
        OP_SET_CROSS_COLLATERAL => Ok(CoinbaseRoute::InternationalCrossCollateral {
            portfolio_id: portfolio.into(),
        }),
        OP_SET_AUTO_MARGIN => Ok(CoinbaseRoute::InternationalAutoMargin {
            portfolio_id: portfolio.into(),
        }),
        _ => Err(error(
            "COINBASE_OPERATION_UNKNOWN",
            "Unknown International portfolio flag operation.",
        )),
    }
}

fn set_flag_schema() -> Value {
    json!({"type":"object","required":["account","requestId","portfolioId","enabled"],"properties":{"account":{"type":"string","minLength":1,"maxLength":128},"requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},"portfolioId":{"type":"string","minLength":1,"maxLength":128},"enabled":{"type":"boolean"}},"additionalProperties":false})
}

fn intl_read_schema(operation: &str) -> Value {
    let needs_asset = matches!(
        operation,
        "coinbaseinternational.get-loan" | "coinbaseinternational.get-loan-availability"
    );
    let mut required = vec!["account", "portfolioId"];
    let mut properties = json!({"account":{"type":"string","minLength":1,"maxLength":128},"portfolioId":{"type":"string","minLength":1,"maxLength":128}});
    if needs_asset {
        required.push("asset");
        properties["asset"] = json!({"type":"string","minLength":1,"maxLength":128});
    }
    json!({"type":"object","required":required,"properties":properties,"additionalProperties":false})
}

fn set_margin_schema() -> Value {
    json!({"type":"object","required":["account","requestId","portfolioId","marginOverride"],"properties":{
        "account":{"type":"string","minLength":1,"maxLength":128},
        "requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},
        "portfolioId":{"type":"string","minLength":1,"maxLength":128},
        "marginOverride":{"type":"string","pattern":"^(0|[1-9][0-9]*)(\\.[0-9]{1,18})?$"}
    },"additionalProperties":false})
}

fn enrolled_account<'a>(profile: &'a CoinbaseProfile, name: &str) -> Result<&'a CoinbaseAccount> {
    profile
        .accounts()
        .iter()
        .find(|a| {
            a.name == name && a.product == CoinbaseProduct::International && a.identity_verified
        })
        .ok_or_else(|| {
            error(
                "COINBASE_ACCOUNT_UNKNOWN",
                "Select an enrolled, verified International account.",
            )
        })
}

fn numeric_text(value: &Value) -> Option<String> {
    match value {
        Value::String(v) => Some(v.clone()),
        Value::Number(v) => Some(v.to_string()),
        _ => None,
    }
}

fn margin_matches(detail: &Value, expected: &str) -> Result<bool> {
    let Some(current) = detail
        .pointer("/summary/margin_override")
        .and_then(numeric_text)
    else {
        return Ok(false);
    };
    Ok(fin_domain::Quantity::from_decimal(&current, 18)?
        == fin_domain::Quantity::from_decimal(expected, 18)?)
}

fn error(code: &str, message: &str) -> FinError {
    FinError::new(
        code,
        "coinbase",
        message,
        "Review the enrolled International portfolio and retry preparation.",
    )
}

fn unknown_reconciliation(reason: &str) -> Reconciliation {
    Reconciliation {
        status: Status::Unknown,
        evidence: json!({"reason":reason,"replay":"forbidden"}),
        verified: None,
    }
}

fn binding_matches_current_account(profile: &CoinbaseProfile, binding: &Value) -> bool {
    let (Some(name), Some(expected_identity), Ok(expected_resource)) = (
        binding["account"].as_str(),
        binding["physicalIdentity"].as_str(),
        serde_json::from_value::<fin_domain::ResourceId>(binding["resource"].clone()),
    ) else {
        return false;
    };
    let Some(account) = profile.accounts().iter().find(|account| {
        account.name == name
            && account.product == CoinbaseProduct::International
            && account.identity_verified
    }) else {
        return false;
    };
    if account.physical_identity != expected_identity || account.resource != expected_resource {
        return false;
    }
    if let Some(expected_portfolio) = binding["portfolioId"].as_str() {
        account
            .portfolio_id
            .as_deref()
            .unwrap_or(&account.physical_identity)
            == expected_portfolio
    } else {
        true
    }
}

fn captured_response(dispatch: &Value) -> Option<(u16, Vec<u8>)> {
    let status = u16::try_from(dispatch.get("httpStatus")?.as_u64()?).ok()?;
    let bytes = hex::decode(dispatch.get("bodyHex")?.as_str()?).ok()?;
    let expected = dispatch.get("bodySha256")?.as_str()?;
    (hex::encode(sha2::Sha256::digest(&bytes)) == expected).then_some((status, bytes))
}
