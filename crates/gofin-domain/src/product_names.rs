//! Narrow read compatibility for persisted pre-GoFin financial metadata.
//! New records use GoFin names. Existing signed/hashed JSON is never rewritten.
use crate::{GoFinError, Result};
use serde_json::Value;

pub fn canonical_operation(operation: &str) -> &str {
    match operation {
        "fin.snapshot" => "gofin.snapshot",
        other => other,
    }
}

pub fn legacy_key(key: &str) -> Option<String> {
    key.strip_prefix("__gofin_")
        .map(|suffix| format!("__fin_{suffix}"))
        .or_else(|| {
            key.strip_prefix("_gofin_")
                .map(|suffix| format!("_fin_{suffix}"))
        })
}

/// Read exactly one spelling; even equal duplicate aliases are rejected.
pub fn get<'a>(value: &'a Value, key: &str) -> Result<Option<&'a Value>> {
    let current = value.get(key);
    let legacy = legacy_key(key).and_then(|key| value.get(&key));
    if current.is_some() && legacy.is_some() {
        return Err(GoFinError::new(
            "AMBIGUOUS_PRODUCT_METADATA",
            "validation",
            "The record contains both GoFin and legacy Fin metadata for the same field.",
            "Preserve the original record and reconcile its exact approved binding; do not combine metadata aliases.",
        ));
    }
    Ok(current.or(legacy))
}

pub fn pointer<'a>(value: &'a Value, pointer: &str) -> Result<Option<&'a Value>> {
    let mut current = value;
    for key in pointer.strip_prefix('/').unwrap_or(pointer).split('/') {
        let key = key.replace("~1", "/").replace("~0", "~");
        let Some(next) = get(current, &key)? else {
            return Ok(None);
        };
        current = next;
    }
    Ok(Some(current))
}

/// Remove aliases only from a newly generated working copy, never stored JSON.
pub fn remove(object: &mut serde_json::Map<String, Value>, key: &str) -> Option<Value> {
    let current = object.remove(key);
    let legacy = legacy_key(key).and_then(|key| object.remove(&key));
    current.or(legacy)
}

/// Preserve an already approved opaque venue identity during read-only refresh.
/// Amounts, destinations, permissions and all other fields remain fresh and bound.
pub fn preserve_legacy_client_ids(approved: &Value, refreshed: &mut Value) {
    match (approved, refreshed) {
        (Value::Object(old), Value::Object(new)) => {
            for (key, value) in new {
                let Some(previous) = old.get(key) else {
                    continue;
                };
                if matches!(
                    key.as_str(),
                    "clientTranId"
                        | "withdrawOrderId"
                        | "newClientOrderId"
                        | "cancel_client_id"
                        | "cliOrdId"
                        | "cl_ord_id"
                        | "clientId"
                        | "localSubmissionId"
                ) && previous.as_str().is_some_and(|id| id.starts_with("fin"))
                    && value.as_str().is_some_and(|id| id.starts_with("gofin"))
                {
                    *value = previous.clone();
                } else {
                    preserve_legacy_client_ids(previous, value);
                }
            }
        }
        (Value::Array(old), Value::Array(new)) if old.len() == new.len() => {
            if old.len() == 2
                && old[0] == new[0]
                && matches!(
                    old[0].as_str(),
                    Some("clientTranId" | "withdrawOrderId" | "newClientOrderId")
                )
                && old[1].as_str().is_some_and(|id| id.starts_with("fin"))
                && new[1].as_str().is_some_and(|id| id.starts_with("gofin"))
            {
                new[1] = old[1].clone();
            }
            for (old, new) in old.iter().zip(new) {
                preserve_legacy_client_ids(old, new);
            }
        }
        _ => {}
    }
}

pub trait MetadataValueExt {
    fn product_get(&self, key: &str) -> Result<Option<&Value>>;
    fn product_value(&self, key: &str) -> Result<&Value>;
    fn product_pointer(&self, pointer: &str) -> Result<Option<&Value>>;
}
impl MetadataValueExt for Value {
    fn product_get(&self, key: &str) -> Result<Option<&Value>> {
        get(self, key)
    }
    fn product_value(&self, key: &str) -> Result<&Value> {
        Ok(get(self, key)?.unwrap_or(&Value::Null))
    }
    fn product_pointer(&self, path: &str) -> Result<Option<&Value>> {
        pointer(self, path)
    }
}

/// Validate alias exclusivity throughout the immutable object before hashing.
pub fn validate(value: &Value) -> Result<()> {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if let Some(legacy) = legacy_key(key)
                    && fields.contains_key(&legacy)
                {
                    return Err(GoFinError::validation(
                        "A record cannot contain both GoFin and legacy Fin metadata aliases.",
                    ));
                }
                validate(value)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                validate(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Keep the spelling of already approved metadata in a fresh in-memory candidate.
/// Values come from revalidation, never from the older record. New candidates
/// remain GoFin-only; no persisted history or hash-bound bytes are rewritten.
pub fn preserve_approved_spelling(approved: &Value, refreshed: &mut Value) -> Result<()> {
    validate(approved)?;
    validate(refreshed)?;
    preserve_spelling(approved, refreshed);
    Ok(())
}
fn preserve_spelling(approved: &Value, refreshed: &mut Value) {
    match (approved, refreshed) {
        (Value::Object(old), Value::Object(new)) => {
            let keys = new.keys().cloned().collect::<Vec<_>>();
            for key in keys {
                let legacy = legacy_key(&key).filter(|key| old.contains_key(key));
                if let Some(legacy) = legacy {
                    if let Some(value) = new.remove(&key) {
                        new.insert(legacy.clone(), value);
                    }
                    if let (Some(previous), Some(value)) = (old.get(&legacy), new.get_mut(&legacy))
                    {
                        preserve_spelling(previous, value);
                    }
                } else if let (Some(previous), Some(value)) = (old.get(&key), new.get_mut(&key)) {
                    preserve_spelling(previous, value);
                }
            }
        }
        (Value::Array(old), Value::Array(new)) if old.len() == new.len() => {
            for (old, new) in old.iter().zip(new) {
                preserve_spelling(old, new);
            }
        }
        _ => {}
    }
}

/// Only these four reviewed deferred-plan placeholders have historical aliases.
pub fn recovery_placeholder(actual: Option<&str>, expected: &str) -> bool {
    let legacy = match expected {
        "__gofin_plan_recovery_request__" => "__fin_plan_recovery_request__",
        "__gofin_plan_recovery_parent__" => "__fin_plan_recovery_parent__",
        "__gofin_plan_recovery_position__" => "__fin_plan_recovery_position__",
        "__gofin_plan_recovery_order_key__" => "__fin_plan_recovery_order_key__",
        _ => return false,
    };
    actual == Some(expected) || actual == Some(legacy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn aliases_are_read_without_rewriting_and_duplicate_aliases_fail_closed() {
        let legacy =
            json!({"preview":{"binding":{"__fin_recovery":{"parent_operation_id":"parent"}}}});
        let original = legacy.clone();
        assert_eq!(
            pointer(
                &legacy,
                "/preview/binding/__gofin_recovery/parent_operation_id"
            )
            .unwrap(),
            Some(&json!("parent"))
        );
        assert_eq!(legacy, original);
        assert!(
            get(
                &json!({"__gofin_recovery":{},"__fin_recovery":{}}),
                "__gofin_recovery"
            )
            .is_err()
        );
    }
    #[test]
    fn legacy_venue_id_is_preserved_without_carrying_financial_fields() {
        let old = json!({"params":{"newClientOrderId":"fin-old","amount":"1"}});
        let mut new = json!({"params":{"newClientOrderId":"gofin-new","amount":"2"}});
        preserve_legacy_client_ids(&old, &mut new);
        assert_eq!(
            new,
            json!({"params":{"newClientOrderId":"fin-old","amount":"2"}})
        );
    }
}
