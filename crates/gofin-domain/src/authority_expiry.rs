//! Generic checks for time-bounded fields declared by trusted operation semantics.
use crate::{
    Definition, GoFinError, PreparedOperation, Result,
    boundaries::{BoundaryViolation, violation_error},
};
use chrono::DateTime;
use serde_json::{Value, json};
use std::collections::BTreeSet;

const MAX_AUTHORITY_EXPIRY_FIELDS: usize = 16;

fn invalid_definition(definition: &Definition, detail: &str) -> GoFinError {
    GoFinError::new(
        "AUTHORITY_EXPIRY_DEFINITION_INVALID",
        "configuration",
        format!(
            "Trusted operation `{}` has invalid authority expiry metadata: {detail}",
            definition.name
        ),
        "Correct the registered operation definition before authorizing this operation.",
    )
}

fn valid_pointer(pointer: &str) -> bool {
    if !pointer.starts_with('/') {
        return false;
    }
    let bytes = pointer.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'~' {
            if index + 1 >= bytes.len() || !matches!(bytes[index + 1], b'0' | b'1') {
                return false;
            }
            index += 2;
        } else {
            index += 1;
        }
    }
    true
}

fn metadata_fields(definition: &Definition) -> Result<Vec<String>> {
    let Some(fields) = definition.semantics.get("authority_expiry_fields") else {
        return Ok(Vec::new());
    };
    let fields = fields.as_array().ok_or_else(|| {
        invalid_definition(definition, "`authority_expiry_fields` must be an array")
    })?;
    if fields.len() > MAX_AUTHORITY_EXPIRY_FIELDS {
        return Err(invalid_definition(
            definition,
            "`authority_expiry_fields` exceeds the 16-field limit",
        ));
    }
    let mut pointers = BTreeSet::new();
    for field in fields {
        let object = field
            .as_object()
            .ok_or_else(|| invalid_definition(definition, "each expiry field must be an object"))?;
        if object.len() != 2 {
            return Err(invalid_definition(
                definition,
                "each expiry field must contain only `pointer` and `format`",
            ));
        }
        let pointer = object
            .get("pointer")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_definition(definition, "expiry pointer must be a string"))?;
        let format = object
            .get("format")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_definition(definition, "expiry format must be a string"))?;
        if format != "unix_seconds" {
            return Err(invalid_definition(
                definition,
                "expiry format must be `unix_seconds`",
            ));
        }
        if !valid_pointer(pointer) {
            return Err(invalid_definition(
                definition,
                "expiry pointer must be a valid non-root JSON pointer",
            ));
        }
        if !pointers.insert(pointer.to_owned()) {
            return Err(invalid_definition(
                definition,
                "expiry pointers must be unique",
            ));
        }
    }
    Ok(pointers.into_iter().collect())
}

fn deadline(value: &Value) -> Option<u64> {
    let value = match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text)
            if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            text.parse::<u64>().ok()
        }
        _ => None,
    }?;
    (value > 0).then_some(value)
}

/// Enforce trusted on-chain deadline fields against a named-plan or boundary
/// authorization expiry. Definitions without this metadata retain prior behavior.
pub fn validate_authority_expiry(
    definition: &Definition,
    prepared: &PreparedOperation,
    expires_at: &str,
) -> Result<()> {
    let fields = metadata_fields(definition)?;
    if fields.is_empty() {
        return Ok(());
    }
    let expires_at_unix = DateTime::parse_from_rfc3339(expires_at)
        .ok()
        .map(|time| time.timestamp())
        .filter(|time| *time > 0)
        .ok_or_else(|| {
            GoFinError::new(
                "AUTHORITY_EXPIRY_INVALID",
                "authorization",
                "Financial authority has an invalid RFC3339 expiry.",
                "Recreate the authorization with a valid expiry before continuing.",
            )
        })?;
    for pointer in fields {
        let raw = prepared.input.pointer(&pointer);
        let candidate = raw.and_then(deadline);
        if candidate.is_none_or(|deadline| deadline > expires_at_unix as u64) {
            return Err(violation_error(
                "On-chain authorization deadline exceeds or does not satisfy the active financial authority.",
                BoundaryViolation {
                    constraint: "deadline".into(),
                    approved_bound: json!({
                        "expires_at": expires_at,
                        "unix_seconds": expires_at_unix,
                    }),
                    candidate_value: raw.cloned().unwrap_or(Value::Null),
                    evidence_time: Some(prepared.observed_at.clone()),
                    reauthorization_required: false,
                    reconciliation_required: false,
                },
            ));
        }
    }
    Ok(())
}

/// Reject already impossible absolute deadlines before a named-plan adapter is
/// asked to prepare. Relative fields are resolved separately by the broker.
pub fn validate_plan_input_deadlines(
    definition: &Definition,
    input: &Value,
    expires_at: &str,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let expiry = DateTime::parse_from_rfc3339(expires_at)
        .map_err(|_| GoFinError::validation("Invalid plan authorization expiry."))?;
    for pointer in metadata_fields(definition)? {
        let value = input.pointer(&pointer).and_then(deadline).ok_or_else(|| {
            GoFinError::validation("Required concrete action deadline is missing or invalid.")
        })?;
        if value as i128 <= at.timestamp() as i128 || value as i128 > expiry.timestamp() as i128 {
            let expired = value as i128 <= at.timestamp() as i128;
            return Err(GoFinError::new(
                if expired { "PLAN_ACTION_DEADLINE_EXPIRED" } else { "PLAN_ACTION_DEADLINE_AFTER_AUTHORIZATION" },
                "financial_plan", if expired { "The action deadline has expired. No candidate was prepared." } else { "The action deadline exceeds the plan authorization. No candidate was prepared." },
                "For a future action, use an approved relative deadline template; otherwise choose a fresh deadline within active authority or approve a replacement revision.",
            ).with_details(json!({"deadline_pointer":pointer,"deadline_unix_seconds":value,
                "observed_at":at.to_rfc3339(),"expires_at":expires_at,"reauthorization_required":!expired})));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Check, ExecutionActor, FinancialIntent, OperationKind};

    fn definition(fields: Option<Value>) -> Definition {
        let mut semantics = json!({});
        if let Some(fields) = fields {
            semantics["authority_expiry_fields"] = fields;
        }
        Definition {
            name: "test.operation".into(),
            description: String::new(),
            kind: OperationKind::Write,
            execution_actor: ExecutionActor::AgentInvocable,
            input_schema: json!({}),
            output_schema: json!({}),
            error_schema: json!({}),
            examples: vec![],
            coverage: String::new(),
            completion: String::new(),
            semantics,
        }
    }
    fn prepared(input: Value) -> PreparedOperation {
        PreparedOperation {
            operation: "test.operation".into(),
            input,
            binding: json!({}),
            summary: String::new(),
            checks: Vec::<Check>::new(),
            warnings: vec![],
            observed_at: "2027-01-01T00:00:00Z".into(),
            expires_at: "2027-01-01T00:10:00Z".into(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        }
    }
    fn metadata(pointer: &str) -> Value {
        json!([{"pointer":pointer,"format":"unix_seconds"}])
    }
    const AUTH_EXPIRY: &str = "2027-01-01T00:10:00Z";

    #[test]
    fn deadlines_before_and_at_authority_expiry_are_allowed_but_after_is_structured_violation() {
        let definition = definition(Some(metadata("/deadline")));
        for value in ["1798762199", "1798762200"] {
            validate_authority_expiry(
                &definition,
                &prepared(json!({"deadline":value})),
                AUTH_EXPIRY,
            )
            .unwrap();
        }
        let error = validate_authority_expiry(
            &definition,
            &prepared(json!({"deadline":"1798762201"})),
            AUTH_EXPIRY,
        )
        .unwrap_err();
        assert_eq!(error.code, "BOUNDARY_VIOLATION");
        let violation = error.details.unwrap()["boundary_violation"].clone();
        assert_eq!(violation["constraint"], "deadline");
        assert_eq!(violation["candidate_value"], "1798762201");
        assert_eq!(violation["approved_bound"]["unix_seconds"], 1798762200u64);
    }

    #[test]
    fn missing_malformed_zero_and_out_of_range_deadlines_fail_closed() {
        let definition = definition(Some(metadata("/deadline")));
        for input in [
            json!({}),
            json!({"deadline":"nope"}),
            json!({"deadline":0}),
            json!({"deadline":"18446744073709551616"}),
        ] {
            assert_eq!(
                validate_authority_expiry(&definition, &prepared(input), AUTH_EXPIRY)
                    .unwrap_err()
                    .code,
                "BOUNDARY_VIOLATION"
            );
        }
    }

    #[test]
    fn trusted_pointer_must_resolve_in_prepared_input_not_binding_or_other_metadata() {
        let definition = definition(Some(metadata("/deadline")));
        let mut candidate = prepared(json!({}));
        candidate.binding = json!({"deadline":1798762199});
        let error = validate_authority_expiry(&definition, &candidate, AUTH_EXPIRY).unwrap_err();
        assert_eq!(error.code, "BOUNDARY_VIOLATION");
        assert_eq!(
            error.details.unwrap()["boundary_violation"]["candidate_value"],
            Value::Null
        );
    }

    #[test]
    fn absent_metadata_is_compatible_and_bad_definitions_fail_closed() {
        validate_authority_expiry(&definition(None), &prepared(json!({})), "not a time").unwrap();
        assert_eq!(
            validate_authority_expiry(
                &definition(Some(metadata("/deadline"))),
                &prepared(json!({"deadline":1})),
                "not a time",
            )
            .unwrap_err()
            .code,
            "AUTHORITY_EXPIRY_INVALID"
        );
        for fields in [
            json!("bad"),
            json!([{"pointer":"/deadline","format":"unix_millis"}]),
            json!([{"pointer":"deadline","format":"unix_seconds"}]),
            json!([{"pointer":"/bad~2escape","format":"unix_seconds"}]),
            json!([{"pointer":"/deadline","format":"unix_seconds"},{"pointer":"/deadline","format":"unix_seconds"}]),
            Value::Array(vec![
                json!({"pointer":"/x","format":"unix_seconds"});
                MAX_AUTHORITY_EXPIRY_FIELDS + 1
            ]),
        ] {
            assert_eq!(
                validate_authority_expiry(
                    &definition(Some(fields)),
                    &prepared(json!({"deadline":1})),
                    AUTH_EXPIRY
                )
                .unwrap_err()
                .code,
                "AUTHORITY_EXPIRY_DEFINITION_INVALID"
            );
        }
    }
}
