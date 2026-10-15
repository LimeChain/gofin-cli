//! Shared planning arithmetic and broker-owned relative action deadlines.
//! Planning allowances never change quote freshness or transaction deadlines.
use crate::{Definition, GoFinError, Result};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const PLANNING_INCREMENT_SECONDS: u64 = 300;
pub const ONCHAIN_ALLOWANCE_SECONDS: u64 = 300;
pub const WITHDRAWAL_ALLOWANCE_SECONDS: u64 = 900;

pub fn sequential_budget(allowances: impl IntoIterator<Item = u64>) -> Result<u64> {
    let invalid = || {
        GoFinError::new(
            "PLAN_DURATION_INVALID",
            "financial_plan",
            "The sequential planning budget cannot be represented.",
            "Choose a representable explicit duration.",
        )
    };
    let total = allowances.into_iter().try_fold(0u64, |total, step| {
        total.checked_add(step).ok_or_else(invalid)
    })?;
    let headroom = total.div_ceil(4).max(PLANNING_INCREMENT_SECONDS);
    total
        .checked_add(headroom)
        .and_then(|value| value.checked_add(PLANNING_INCREMENT_SECONDS - 1))
        .and_then(|value| {
            (value / PLANNING_INCREMENT_SECONDS).checked_mul(PLANNING_INCREMENT_SECONDS)
        })
        .ok_or_else(invalid)
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RelativeDeadlinePolicy {
    /// Trusted top-level operation field; never selected by the caller.
    pub field: String,
    pub maximum_seconds: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResolvedActionDeadline {
    pub field: String,
    pub window_seconds: u64,
    pub resolved_at: String,
    pub deadline_unix_seconds: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlanAuthorizationTiming {
    pub observed_at: String,
    pub expires_at: String,
    pub remaining_seconds: u64,
    pub required_action_window_seconds: Option<u64>,
    pub can_prepare_next_action: bool,
}

pub fn relative_deadline_policy(
    definition: &Definition,
    seconds: Option<u64>,
) -> Result<Option<RelativeDeadlinePolicy>> {
    let Some(seconds) = seconds else {
        return Ok(None);
    };
    let invalid = || {
        GoFinError::new(
            "PLAN_ACTION_DEADLINE_INVALID",
            "financial_plan",
            "Relative action deadline is unsupported or exceeds the reviewed operation horizon.",
            "Use the advertised positive action_deadline_seconds horizon and omit the absolute deadline.",
        )
    };
    // New plan-only operations publish plan execution timing under
    // `plan_permission`, alongside their adapter-scoped permission contract.
    // Keep accepting the top-level location used by existing operations.
    let policy_value = definition
        .semantics
        .get("plan_permission")
        .and_then(|permission| permission.get("plan_relative_deadline"))
        .or_else(|| definition.semantics.get("plan_relative_deadline"))
        .cloned()
        .ok_or_else(invalid)?;
    let policy: RelativeDeadlinePolicy =
        serde_json::from_value(policy_value).map_err(|_| invalid())?;
    if seconds == 0
        || seconds > policy.maximum_seconds
        || policy.field.is_empty()
        || policy.field.contains(['/', '~'])
        || definition
            .input_schema
            .pointer(&format!("/properties/{}/type", policy.field))
            != Some(&json!("integer"))
        || !definition
            .semantics
            .get("authority_expiry_fields")
            .and_then(Value::as_array)
            .is_some_and(|fields| {
                fields.iter().any(|field| {
                    field["pointer"] == format!("/{}", policy.field)
                        && field["format"] == "unix_seconds"
                })
            })
    {
        return Err(invalid());
    }
    Ok(Some(policy))
}

/// Project only a trusted deadline out of the plan-template required fields.
/// Concrete preparation always uses the original operation schema.
pub fn plan_input_schema(
    definition: &Definition,
    input: &Value,
    seconds: Option<u64>,
) -> Result<Value> {
    let mut schema = definition.input_schema.clone();
    if let Some(policy) = relative_deadline_policy(definition, seconds)? {
        if input.get(&policy.field).is_some() {
            return Err(GoFinError::new(
                "PLAN_ACTION_DEADLINE_CONFLICT",
                "financial_plan",
                "A relative plan deadline cannot also specify an absolute deadline.",
                "Omit the absolute deadline from the reviewed template and concrete attempt input.",
            ));
        }
        if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
            required.retain(|field| field != &json!(policy.field));
        }
    }
    Ok(schema)
}

pub fn resolve_action_deadline(
    policy: &RelativeDeadlinePolicy,
    seconds: u64,
    at: DateTime<Utc>,
    expires_at: &str,
) -> Result<ResolvedActionDeadline> {
    let expiry = DateTime::parse_from_rfc3339(expires_at)
        .map_err(|_| GoFinError::validation("Invalid plan expiry."))?;
    let current = u64::try_from(at.timestamp())
        .map_err(|_| GoFinError::validation("Invalid preparation time."))?;
    let deadline = current
        .checked_add(seconds)
        .ok_or_else(|| GoFinError::validation("Action deadline overflow."))?;
    let remaining = expiry.signed_duration_since(at).num_seconds().max(0) as u64;
    if seconds == 0 || seconds > policy.maximum_seconds {
        return Err(GoFinError::validation(
            "Action deadline exceeds its approved horizon.",
        ));
    }
    if remaining < seconds || deadline > expiry.timestamp().max(0) as u64 {
        return Err(GoFinError::new("PLAN_INSUFFICIENT_AUTHORIZATION_TIME", "financial_plan",
            "The remaining plan authorization cannot accommodate the full approved action window. No candidate was prepared.",
            "Reconcile unresolved attempts, preview a replacement with a longer duration, and obtain fresh native approval using --replaces REVISION_ID.")
            .with_details(json!({"observed_at":at.to_rfc3339(),"expires_at":expires_at,
                "remaining_seconds":remaining,"required_window_seconds":seconds,"deadline_field":policy.field,
                "reauthorization_required":true,"reconciliation_required":false})));
    }
    Ok(ResolvedActionDeadline {
        field: policy.field.clone(),
        window_seconds: seconds,
        resolved_at: at.to_rfc3339(),
        deadline_unix_seconds: deadline,
    })
}

impl ResolvedActionDeadline {
    pub fn apply(&self, input: &mut Value) -> Result<()> {
        let object = input
            .as_object_mut()
            .ok_or_else(|| GoFinError::validation("Plan input must be an object."))?;
        if object.contains_key(&self.field) {
            return Err(GoFinError::validation(
                "Relative attempt input must omit the absolute deadline.",
            ));
        }
        object.insert(self.field.clone(), json!(self.deadline_unix_seconds));
        Ok(())
    }
}

/// Classify without contacting the venue. All rules originate in trusted discovery.
pub fn definition_allowance(definition: &Definition, input: &Value) -> Result<Option<u64>> {
    if definition.kind == crate::OperationKind::Read {
        return Ok(Some(0));
    }
    if definition
        .semantics
        .get("plan_immediate_orders")
        .and_then(Value::as_bool)
        == Some(true)
    {
        fn immediate(order: &Value) -> bool {
            order.get("trigger_price").is_none_or(Value::is_null)
                && order.get("order_type").and_then(Value::as_str) != Some("trigger")
                && ((order.get("order_type").and_then(Value::as_str) == Some("market")
                    && order.get("time_in_force").is_none_or(Value::is_null))
                    || order.get("time_in_force").and_then(Value::as_str) == Some("ioc"))
        }
        let orders = input.get("orders").and_then(Value::as_array);
        let is_immediate = orders.map_or_else(
            || immediate(input),
            |orders| {
                !orders.is_empty()
                    && orders
                        .iter()
                        .all(|order| immediate(order.get("order").unwrap_or(order)))
            },
        );
        return Ok(is_immediate.then_some(ONCHAIN_ALLOWANCE_SECONDS));
    }
    Ok(definition
        .semantics
        .get("plan_allowance_seconds")
        .and_then(Value::as_u64))
}

pub fn discovery() -> Value {
    json!({
        "authorization_lifetime":"Parent sequential workflow lifetime from successful native approval; omission computes a planning budget.",
        "onchain_allowance_seconds":ONCHAIN_ALLOWANCE_SECONDS,
        "immediate_exchange_allowance_seconds":ONCHAIN_ALLOWANCE_SECONDS,
        "withdrawal_or_fast_bridge_allowance_seconds":WITHDRAWAL_ALLOWANCE_SECONDS,
        "headroom":"25% of summed sequential allowances, with a minimum of 300 seconds; round upward to a 300-second boundary.",
        "example":{"workflow":"withdrawal → swap → fast bridge","authorization_seconds":2700,"authorization_duration":"45 minutes"},
        "explicit_horizons":"Resting/trigger orders, TWAPs, delayed bridge stages and unclassified writes require explicit --valid-for.",
        "agent_review":"Calculate, explicitly select and state the authorization duration in conversation. Report user/protected-limit conflicts; never truncate.",
        "action_deadline":"Optional step action_deadline_seconds approves a relative full execution window for advertised operations. Omit the absolute input deadline; GoFin resolves it once at eligible preparation within the approved horizon and parent expiry.",
        "insufficient_time":"PLAN_INSUFFICIENT_AUTHORIZATION_TIME blocks before preparation; reconcile unresolved work and approve a replacement revision with --replaces.",
        "quote_expiry":"Independent freshness check at each submission; a longer authorization never extends quote validity.",
        "native_challenge_timeout_seconds":90,
        "client_transport_timeout_seconds":300,
        "transport_timeout":"Recover the same request/attempt ID; never replay an uncertain financial write.",
        "settlement":"Planning allowances are not settlement guarantees. Bridge settlement is counted once; separate allowance/reset writes add their own allowances."
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deadline_definition(semantics: Value) -> Definition {
        Definition {
            name: "swap.execute".into(),
            description: String::new(),
            kind: crate::OperationKind::Write,
            execution_actor: crate::ExecutionActor::AgentInvocable,
            input_schema: json!({
                "type": "object",
                "properties": {"deadline": {"type": "integer"}},
                "required": ["deadline"]
            }),
            output_schema: json!({"type": "object"}),
            error_schema: json!({"type": "object"}),
            examples: Vec::new(),
            coverage: String::new(),
            completion: String::new(),
            semantics,
        }
    }

    #[test]
    fn plan_only_permission_deadline_horizon_is_supported_without_boundary_authority() {
        let definition = deadline_definition(json!({
            "authority_expiry_fields": [{"pointer":"/deadline", "format":"unix_seconds"}],
            "boundary_authorization": {"supported": false},
            "plan_permission": {
                "mode":"agent_nominated",
                "plan_relative_deadline":{"field":"deadline", "maximum_seconds":600}
            }
        }));

        let policy = relative_deadline_policy(&definition, Some(300))
            .unwrap()
            .unwrap();
        assert_eq!(policy.field, "deadline");
        assert_eq!(policy.maximum_seconds, 600);
        assert!(
            !definition.semantics["boundary_authorization"]["supported"]
                .as_bool()
                .unwrap()
        );
        let projected = plan_input_schema(&definition, &json!({}), Some(300)).unwrap();
        assert!(projected["required"].as_array().unwrap().is_empty());
    }

    #[test]
    fn legacy_top_level_plan_deadline_horizon_remains_supported() {
        let definition = deadline_definition(json!({
            "authority_expiry_fields": [{"pointer":"/deadline", "format":"unix_seconds"}],
            "plan_relative_deadline":{"field":"deadline", "maximum_seconds":600}
        }));
        assert_eq!(
            relative_deadline_policy(&definition, Some(300))
                .unwrap()
                .unwrap()
                .maximum_seconds,
            600
        );
    }

    #[test]
    fn budget_headroom_rounding_and_overflow() {
        assert_eq!(sequential_budget([900, 300, 900]).unwrap(), 2700);
        assert_eq!(sequential_budget([300]).unwrap(), 600);
        assert_eq!(sequential_budget([300, 300]).unwrap(), 900);
        assert_eq!(sequential_budget([]).unwrap(), 300);
        assert_eq!(sequential_budget([1200]).unwrap(), 1500);
        assert_eq!(sequential_budget([1201]).unwrap(), 1800);
        assert!(sequential_budget([u64::MAX]).is_err());
        assert!(sequential_budget([u64::MAX, 1]).is_err());
    }
    #[test]
    fn fractional_remaining_time_cannot_be_rounded_up_into_authority() {
        let at = DateTime::parse_from_rfc3339("2027-01-01T00:00:00.500Z")
            .unwrap()
            .with_timezone(&Utc);
        let policy = RelativeDeadlinePolicy {
            field: "deadline".into(),
            maximum_seconds: 600,
        };
        let error = resolve_action_deadline(&policy, 600, at, "2027-01-01T00:10:00Z").unwrap_err();
        assert_eq!(error.details.unwrap()["remaining_seconds"], 599);
    }

    #[test]
    fn full_window_is_required_and_resolution_is_immutable() {
        let at = DateTime::parse_from_rfc3339("2027-01-01T00:11:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let policy = RelativeDeadlinePolicy {
            field: "deadline".into(),
            maximum_seconds: 600,
        };
        let deadline = resolve_action_deadline(&policy, 600, at, "2027-01-01T00:45:00Z").unwrap();
        let mut input = json!({});
        deadline.apply(&mut input).unwrap();
        assert_eq!(input["deadline"], at.timestamp() as u64 + 600);
        assert!(deadline.apply(&mut input).is_err());
        let error = resolve_action_deadline(&policy, 600, at, "2027-01-01T00:20:59Z").unwrap_err();
        assert_eq!(error.code, "PLAN_INSUFFICIENT_AUTHORIZATION_TIME");
        assert_eq!(error.details.unwrap()["remaining_seconds"], 599);
    }
}
