//! Coinbase Exchange profile configuration.
//!
//! Native profile creation and rename are account-configuration actions. The
//! protected actor is the enrolled Exchange profile; every write is bound to
//! that exact native identity and is never retried after an uncertain result.

use crate::{CoinbaseAccount, CoinbaseDispatch, CoinbaseProduct, CoinbaseProfile};
use fin_domain::{
    Check, CheckStatus, Definition, ExecutionActor, FinError, FinancialIntent, OperationKind,
    OperationRecord, PreparedOperation, Reconciliation, Result, Status, Submission,
};
use fin_exchange::coinbase::{CoinbaseRequest, CoinbaseRoute};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CREATE: &str = "coinbaseexchange.create-profile";
const RENAME: &str = "coinbaseexchange.rename-profile";
const READ_PROFILES: &str = "coinbaseexchange.get-profiles";

pub(super) fn definitions() -> Vec<Definition> {
    vec![
        Definition {
            name: READ_PROFILES.into(),
            description: "Read profiles available to one enrolled Coinbase Exchange credential.".into(),
            kind: OperationKind::Read,
            execution_actor: ExecutionActor::AgentReadable,
            input_schema: account_schema(),
            output_schema: json!({"type":"array"}),
            error_schema: json!({"type":"object"}),
            examples: vec![],
            coverage: "Returns the native authenticated GET /profiles response for the exact enrolled Exchange credential.".into(),
            completion: "Returns the native profile rows only after a successful authenticated read.".into(),
            semantics: json!({"product":"coinbaseexchange","route":"GET /profiles","clientVersion":"ccxt 4.5.85"}),
        },
        profile_definition(CREATE, "Create", "POST /profiles", create_schema()),
        profile_definition(RENAME, "Rename", "PUT /profiles/{profile_id}", rename_schema()),
    ]
}

fn profile_definition(name: &str, verb: &str, route: &str, schema: Value) -> Definition {
    Definition {
        name: name.into(),
        description: format!("{verb} a Coinbase Exchange profile."),
        kind: OperationKind::Configuration,
        execution_actor: ExecutionActor::HumanInteractive,
        input_schema: schema,
        output_schema: json!({"type":"object"}),
        error_schema: json!({"type":"object"}),
        examples: vec![],
        coverage: format!("{verb}s an Exchange profile through its native REST route after binding the initiating enrolled profile and checking current native state."),
        completion: "Completes only after a captured successful native response and exact GET /profiles readback. Lost or invalid responses remain unknown and are never replayed. Newly created profiles require separate credential enrollment before use.".into(),
        semantics: json!({"product":"coinbaseexchange","route":route,"readback":"GET /profiles","clientVersion":"ccxt 4.5.85"}),
    }
}

pub(super) async fn read(
    profile: &CoinbaseProfile,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>> {
    if operation != READ_PROFILES {
        return Ok(None);
    }
    validate(&account_schema(), input)?;
    let account = enrolled_exchange(profile, input["account"].as_str().expect("schema"))?;
    let response = profile
        .request_for_account(
            &account.name,
            CoinbaseProduct::Exchange,
            CoinbaseRoute::ExchangeProfiles,
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    if !(200..300).contains(&response.status) {
        return Err(error(
            "COINBASE_REQUEST_FAILED",
            "Coinbase rejected the profile read.",
        ));
    }
    serde_json::from_slice(&response.body)
        .map(Some)
        .map_err(|_| {
            error(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned invalid profile JSON.",
            )
        })
}

pub(super) async fn prepare(
    profile: &CoinbaseProfile,
    operation: &str,
    input: &Value,
) -> Result<Option<PreparedOperation>> {
    if operation != CREATE && operation != RENAME {
        return Ok(None);
    }
    let schema = if operation == CREATE {
        create_schema()
    } else {
        rename_schema()
    };
    validate(&schema, input)?;
    let name = input["name"].as_str().expect("schema");
    validate_profile_name(name)?;
    if matches!(name, "default" | "margin") {
        return Err(error(
            "INVALID_INPUT",
            "Coinbase reserves the names default and margin.",
        ));
    }
    let account = enrolled_exchange(profile, input["account"].as_str().expect("schema"))?;
    let profiles = list_profiles(profile, account).await?;
    let enrolled_row = profile_rows(&profiles)
        .into_iter()
        .find(|row| row["id"].as_str() == Some(account.physical_identity.as_str()))
        .ok_or_else(|| {
            error(
                "COINBASE_PROFILE_MISMATCH",
                "The enrolled Exchange profile was missing from the native profile list.",
            )
        })?;
    if enrolled_row["active"].as_bool() != Some(true) {
        return Err(error(
            "COINBASE_PROFILE_INACTIVE",
            "The enrolled Exchange profile is not active.",
        ));
    }
    let user_id = enrolled_row["user_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            error(
                "COINBASE_RESPONSE_INVALID",
                "Native enrolled profile omitted its user identity.",
            )
        })?;
    let enrolled_name = enrolled_row["name"].as_str().ok_or_else(|| {
        error(
            "COINBASE_RESPONSE_INVALID",
            "Native enrolled profile omitted its name.",
        )
    })?;
    let (profile_id, previous_name) = if operation == CREATE {
        if profile_rows(&profiles)
            .iter()
            .any(|row| row["name"].as_str() == Some(name))
        {
            return Err(error(
                "COINBASE_PROFILE_EXISTS",
                "A profile with that exact name already exists.",
            ));
        }
        (None, None)
    } else {
        let enrolled_id = &account.physical_identity;
        if input["profileId"].as_str() != Some(enrolled_id.as_str()) {
            return Err(error(
                "COINBASE_PROFILE_MISMATCH",
                "Only the exact enrolled Exchange profile can be renamed.",
            ));
        }
        let previous = enrolled_name;
        if previous == name {
            return Err(error(
                "COINBASE_CONFIGURATION_NOOP",
                "The profile already has that name.",
            ));
        }
        if profile_rows(&profiles)
            .iter()
            .any(|row| row["name"].as_str() == Some(name))
        {
            return Err(error(
                "COINBASE_PROFILE_EXISTS",
                "A profile with that exact name already exists.",
            ));
        }
        (Some(enrolled_id.as_str()), Some(previous))
    };
    let body_value = match profile_id {
        Some(id) => json!({"profile_id":id,"name":name}),
        None => json!({"name":name}),
    };
    let body = fin_domain::canonical_json(&body_value)?;
    Ok(Some(PreparedOperation {
        operation: operation.into(),
        input: input.clone(),
        binding: json!({"version":1,"provider":"coinbaseexchange","account":account.name,"resource":account.resource,"physicalIdentity":account.physical_identity,"profileId":profile_id,"userId":user_id,"enrolledName":enrolled_name,"previousName":previous_name,"name":name,"requestId":input["requestId"],"requestBodyExact":body}),
        summary: match profile_id {
            Some(id) => format!("Rename Coinbase Exchange profile {id} to {name}"),
            None => format!("Create Coinbase Exchange profile {name}"),
        },
        checks: vec![Check { name:"enrolled_exchange_profile_state".into(), status:CheckStatus::Passed, detail:"The initiating enrolled Exchange profile and exact current profile-name state were verified.".into() }],
        warnings: vec!["Profile configuration is a human-approved account setting. A created profile needs separately enrolled credentials before financial use. A lost response is never replayed.".into()],
        observed_at: fin_domain::now(),
        expires_at: (chrono::Utc::now() + chrono::Duration::seconds(45)).to_rfc3339(),
        valuation: None,
        intent: FinancialIntent { debits:vec![],maximum_costs:vec![],expected:vec![],locks:vec![] },
        operation_version: 1,
        action_deadline: None,
    }))
}

pub(super) async fn refresh(
    profile: &CoinbaseProfile,
    approved: &PreparedOperation,
) -> Result<Option<PreparedOperation>> {
    if approved.operation != CREATE && approved.operation != RENAME {
        return Ok(None);
    }
    let fresh = prepare(profile, &approved.operation, &approved.input)
        .await?
        .ok_or_else(|| {
            error(
                "COINBASE_BINDING_INVALID",
                "Profile operation is unsupported.",
            )
        })?;
    for key in [
        "account",
        "resource",
        "physicalIdentity",
        "profileId",
        "userId",
        "enrolledName",
        "previousName",
        "name",
        "requestBodyExact",
    ] {
        if fresh.binding[key] != approved.binding[key] {
            return Err(error(
                "COINBASE_PREVIEW_CHANGED",
                "Exchange profile identity or current name changed after approval.",
            ));
        }
    }
    Ok(Some(fresh))
}

pub(super) fn plan_submission(
    profile: &CoinbaseProfile,
    current: &PreparedOperation,
) -> Result<Option<fin_exchange::PlannedSubmission<CoinbaseDispatch>>> {
    if current.operation != CREATE && current.operation != RENAME {
        return Ok(None);
    }
    let binding = &current.binding;
    let account = enrolled_exchange(profile, binding["account"].as_str().unwrap_or_default())?;
    if !binding_matches(account, binding) {
        return Err(error(
            "COINBASE_PROFILE_MISMATCH",
            "The prepared Exchange profile no longer matches its enrolled identity.",
        ));
    }
    let expected_body = match current.operation.as_str() {
        CREATE if binding["profileId"].is_null() => json!({"name":binding["name"]}),
        RENAME if binding["profileId"].as_str() == Some(account.physical_identity.as_str()) => {
            json!({"profile_id":account.physical_identity,"name":binding["name"]})
        }
        _ => {
            return Err(error(
                "COINBASE_BINDING_INVALID",
                "Profile operation target does not match the enrolled profile binding.",
            ));
        }
    };
    if fin_domain::canonical_json(&expected_body)?
        != binding["requestBodyExact"].as_str().unwrap_or_default()
    {
        return Err(error(
            "COINBASE_BINDING_INVALID",
            "Exact profile request body does not match its typed fields.",
        ));
    }
    let body = binding["requestBodyExact"]
        .as_str()
        .ok_or_else(|| {
            error(
                "COINBASE_BINDING_INVALID",
                "Exact profile request body is missing.",
            )
        })?
        .to_owned();
    let endpoint = if current.operation == CREATE {
        CoinbaseRoute::ExchangeProfileCreate
    } else {
        CoinbaseRoute::ExchangeProfileRename {
            profile_id: account.physical_identity.clone(),
        }
    };
    let digest = hex::encode(Sha256::digest(
        fin_domain::canonical_json(&json!({
            "requestId":binding["requestId"],
            "requestBodySha256":hex::encode(Sha256::digest(body.as_bytes()))
        }))?
        .as_bytes(),
    ));
    let submission = Submission {
        kind: "coinbase".into(),
        reference: format!(
            "exchange-profile:{}:{}:{}",
            current.operation, account.physical_identity, digest
        ),
        data: json!({"product":"coinbaseexchange","operation":current.operation,"binding":binding}),
    };
    Ok(Some(fin_exchange::PlannedSubmission {
        submission: submission.clone(),
        request: CoinbaseDispatch {
            product: CoinbaseProduct::Exchange,
            endpoint,
            account: account.name.clone(),
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
    if record.operation != CREATE && record.operation != RENAME {
        return Ok(None);
    }
    let Some(prepared) = record.preview.as_ref().filter(|preview| {
        preview.operation == record.operation
            && matches!(preview.operation.as_str(), CREATE | RENAME)
    }) else {
        return Ok(Some(unknown(
            "The Exchange profile record does not contain its matching protected preview.",
        )));
    };
    if record.submission.as_ref() != Some(submission) {
        return Ok(Some(unknown(
            "The supplied Exchange profile submission differs from the durable record.",
        )));
    }
    let binding = &prepared.binding;
    let Some(account) = profile.accounts().iter().find(|account| {
        account.name == binding["account"].as_str().unwrap_or_default()
            && account.product == CoinbaseProduct::Exchange
            && account.identity_verified
    }) else {
        return Ok(Some(unknown(
            "The initiating Exchange enrollment is unavailable.",
        )));
    };
    if !binding_matches(account, binding) {
        return Ok(Some(unknown(
            "Current Exchange account identity no longer matches the durable profile binding.",
        )));
    }
    let expected = match plan_submission(profile, prepared) {
        Ok(Some(planned)) => planned.submission,
        _ => {
            return Ok(Some(unknown(
                "The protected Exchange profile binding no longer produces a valid submission.",
            )));
        }
    };
    if expected != *submission {
        return Ok(Some(unknown(
            "The durable Exchange profile submission kind, reference, or data differs from its recomputed plan.",
        )));
    }
    let Some(dispatch) = record.dispatch_evidence.as_ref() else {
        return Ok(Some(unknown(
            "The profile write has no captured response and will not be replayed.",
        )));
    };
    let Some((status, bytes)) = captured_response(dispatch) else {
        return Ok(Some(unknown(
            "Captured profile response bytes or digest are invalid.",
        )));
    };
    let receipt: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if (400..500).contains(&status) {
        return Ok(Some(Reconciliation {
            status: Status::Failed,
            evidence: json!({"httpStatus":status,"receipt":receipt,"reason":"Coinbase rejected profile configuration"}),
            verified: None,
        }));
    }
    if !(200..300).contains(&status) || !receipt.is_object() {
        return Ok(Some(unknown(
            "Captured profile receipt does not prove native success.",
        )));
    }
    let id = if record.operation == CREATE {
        receipt["id"].as_str().filter(|id| !id.is_empty())
    } else {
        binding["profileId"].as_str()
    };
    let Some(id) = id else {
        return Ok(Some(unknown(
            "Profile receipt omitted the exact native profile ID.",
        )));
    };
    if (record.operation == RENAME && receipt["id"].as_str() != Some(id))
        || receipt["user_id"].as_str() != binding["userId"].as_str()
        || (record.operation == CREATE && receipt["name"].as_str() != binding["name"].as_str())
    {
        return Ok(Some(unknown(
            "Captured profile receipt does not contain the exact ID, enrolled native user, and requested create name.",
        )));
    }
    let row = match get_profile(profile, account, id).await {
        Ok(Some(row)) => row,
        _ => {
            return Ok(Some(unknown(
                "Exact native profile readback is unavailable or malformed.",
            )));
        }
    };
    if row["id"].as_str() != Some(id)
        || row["user_id"].as_str() != binding["userId"].as_str()
        || row["name"].as_str() != binding["name"].as_str()
        || row["active"].as_bool() != Some(true)
    {
        return Ok(Some(unknown(
            "Native profile readback does not prove the exact active ID, enrolled native user, and requested name.",
        )));
    }
    Ok(Some(Reconciliation {
        status: Status::Completed,
        evidence: json!({"httpStatus":status,"receipt":receipt,"profile":row,"responseSha256":dispatch["bodySha256"],"nativeProfileId":id}),
        verified: None,
    }))
}

async fn list_profiles(profile: &CoinbaseProfile, account: &CoinbaseAccount) -> Result<Value> {
    let response = profile
        .request_for_account(
            &account.name,
            CoinbaseProduct::Exchange,
            CoinbaseRoute::ExchangeProfiles,
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    if !(200..300).contains(&response.status) {
        return Err(error(
            "COINBASE_REQUEST_FAILED",
            "Coinbase rejected the profile list read.",
        ));
    }
    serde_json::from_slice(&response.body).map_err(|_| {
        error(
            "COINBASE_RESPONSE_INVALID",
            "Coinbase returned invalid profile list JSON.",
        )
    })
}

async fn get_profile(
    profile: &CoinbaseProfile,
    account: &CoinbaseAccount,
    profile_id: &str,
) -> Result<Option<Value>> {
    let response = profile
        .request_for_account(
            &account.name,
            CoinbaseProduct::Exchange,
            CoinbaseRoute::ExchangeProfile {
                profile_id: profile_id.into(),
            },
            CoinbaseRequest::get(),
            None,
        )
        .await?;
    if !(200..300).contains(&response.status) {
        return Ok(None);
    }
    serde_json::from_slice(&response.body)
        .map(Some)
        .map_err(|_| {
            error(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned invalid profile detail JSON.",
            )
        })
}

fn profile_rows(value: &Value) -> Vec<&Value> {
    value
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .collect()
}

fn enrolled_exchange<'a>(profile: &'a CoinbaseProfile, name: &str) -> Result<&'a CoinbaseAccount> {
    profile
        .accounts()
        .iter()
        .find(|account| {
            account.name == name
                && account.product == CoinbaseProduct::Exchange
                && account.identity_verified
                && !account.physical_identity.trim().is_empty()
        })
        .ok_or_else(|| {
            error(
                "COINBASE_ACCOUNT_UNKNOWN",
                "Select an enrolled, verified Exchange account.",
            )
        })
}

fn binding_matches(account: &CoinbaseAccount, binding: &Value) -> bool {
    binding["physicalIdentity"].as_str() == Some(account.physical_identity.as_str())
        && serde_json::to_value(&account.resource).ok().as_ref() == binding.get("resource")
        && binding["account"].as_str() == Some(account.name.as_str())
}

fn validate(schema: &Value, input: &Value) -> Result<()> {
    let validator = jsonschema::validator_for(schema).map_err(|_| {
        error(
            "COINBASE_CONTRACT_INVALID",
            "Profile operation schema is invalid.",
        )
    })?;
    if !validator.is_valid(input) {
        return Err(error(
            "INVALID_INPUT",
            "Profile operation input does not match its closed schema.",
        ));
    }
    Ok(())
}
fn validate_profile_name(name: &str) -> Result<()> {
    if name.trim() != name || name.chars().any(char::is_control) {
        return Err(error(
            "INVALID_INPUT",
            "Profile names cannot have leading or trailing whitespace or control characters.",
        ));
    }
    Ok(())
}
fn account_schema() -> Value {
    json!({"type":"object","required":["account"],"properties":{"account":{"type":"string","minLength":1,"maxLength":128}},"additionalProperties":false})
}
fn create_schema() -> Value {
    json!({"type":"object","required":["account","requestId","name"],"properties":{"account":{"type":"string","minLength":1,"maxLength":128},"requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},"name":{"type":"string","minLength":1,"maxLength":128}},"additionalProperties":false})
}
fn rename_schema() -> Value {
    json!({"type":"object","required":["account","requestId","profileId","name"],"properties":{"account":{"type":"string","minLength":1,"maxLength":128},"requestId":{"type":"string","pattern":"^[A-Za-z0-9_:-]{1,128}$"},"profileId":{"type":"string","minLength":1,"maxLength":128},"name":{"type":"string","minLength":1,"maxLength":128}},"additionalProperties":false})
}
fn error(code: &str, message: &str) -> FinError {
    FinError::new(
        code,
        "coinbaseexchange",
        message,
        "Review the enrolled Exchange profile and retry preparation.",
    )
}
fn unknown(reason: &str) -> Reconciliation {
    Reconciliation {
        status: Status::Unknown,
        evidence: json!({"reason":reason,"replay":"forbidden"}),
        verified: None,
    }
}
fn captured_response(dispatch: &Value) -> Option<(u16, Vec<u8>)> {
    let status = u16::try_from(dispatch.get("httpStatus")?.as_u64()?).ok()?;
    let bytes = hex::decode(dispatch.get("bodyHex")?.as_str()?).ok()?;
    (hex::encode(Sha256::digest(&bytes)) == dispatch.get("bodySha256")?.as_str()?)
        .then_some((status, bytes))
}
