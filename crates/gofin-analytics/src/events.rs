use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

mod vocabulary {
    include!(concat!(env!("OUT_DIR"), "/vocabulary.rs"));
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Component {
    Cli,
    Broker,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    Failed,
    Canceled,
}
/// Invocation route only; never a grant of financial authority.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Plan,
    BoundedAuthorization,
    Standalone,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Install,
    Update,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleOutcome {
    Attempted,
    Succeeded,
    Failed,
    Canceled,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Rollback {
    NotNeeded,
    Restored,
    Uncertain,
}

const COMMANDS: &[&str] = &[
    "update",
    "capabilities",
    "explain",
    "schema",
    "read",
    "preview",
    "execute",
    "configure",
    "status",
    "operations",
    "doctor",
    "snapshot",
    "setup",
    "help",
    "version",
    "skill.install",
    "skill.inspect",
    "skill.update",
    "skill.remove",
    "plans.preview",
    "plans.authorize",
    "plans.status",
    "plans.cancel",
    "authorizations.authorize",
    "authorizations.status",
    "authorizations.cancel",
    "connections.add",
    "connections.update",
    "connections.remove",
    "connections.list",
    "daemon.start",
    "daemon.stop",
    "daemon.status",
    "policy.show",
    "policy.set",
    "yolo.on",
    "yolo.off",
    "yolo.status",
    "unknown",
];
const PROTOCOLS: &[&str] = &[
    "aave_v3",
    "uniswap_v3",
    "uniswap_v4",
    "sushi_v3",
    "pancakeswap_v3",
    "ramses_v3",
    "jupiter",
    "jupiter_perps",
    "meteora",
    "cctp",
    "cctp_v2",
    "relay",
    "hyperliquid",
    "binance",
    "kraken",
    "coinbase",
    "coinbaseexchange",
    "coinbaseinternational",
    "uniswap",
    "sushi",
    "ramses",
    "pancakeswap",
    "krakenfutures",
    "arbitrum_canonical",
    "base_canonical",
    "arbitrum_eth_deposit_v1",
    "base_standard_bridge_v1",
];
const BRIDGES: &[&str] = &["cctp", "relay", "arbitrum_canonical", "base_canonical"];
const LIFECYCLE_STAGES: &[&str] = &[
    "prepare",
    "verify",
    "configure",
    "service",
    "path",
    "download",
    "compatibility",
    "replace",
    "restart",
    "complete",
    "other",
];
const RESULT_STATUSES: &[&str] = &[
    "pending",
    "submitted",
    "confirmed",
    "settled",
    "succeeded",
    "failed",
    "partial",
    "incomplete",
    "canceled",
    "unknown",
    "awaiting_approval",
    "rejected",
    "expired",
    "cancelled",
    "completed",
];

fn member<'a>(value: &str, allowed: &'a [&'a str], fallback: &'a str) -> &'a str {
    allowed
        .iter()
        .copied()
        .find(|candidate| *candidate == value)
        .unwrap_or(fallback)
}
pub fn safe_operation(operation: &str) -> &str {
    member(operation, vocabulary::OPERATIONS, "unknown")
}
fn error_code(code: &str) -> &str {
    member(code, vocabulary::ERROR_CODES, "UNKNOWN_ERROR")
}
fn error_stage(stage: &str) -> &str {
    if LIFECYCLE_STAGES.contains(&stage) {
        member(stage, LIFECYCLE_STAGES, "other")
    } else {
        member(stage, vocabulary::ERROR_STAGES, "other")
    }
}
fn command(command: &str) -> &str {
    member(command, COMMANDS, "unknown")
}
fn version(value: Option<&str>) -> Option<String> {
    let value = value?.trim_start_matches('v');
    if value.len() > 64
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-+".contains(&c))
    {
        return None;
    }
    let core = value.split(['-', '+']).next()?;
    let numbers: Vec<_> = core.split('.').collect();
    (numbers.len() == 3
        && numbers
            .iter()
            .all(|number| !number.is_empty() && number.bytes().all(|c| c.is_ascii_digit())))
    .then(|| value.to_owned())
}
pub fn release_version() -> &'static str {
    option_env!("GOFIN_RELEASE_VERSION").unwrap_or("development")
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Dimensions {
    networks: BTreeSet<String>,
    protocols: BTreeSet<String>,
    bridges: BTreeSet<String>,
}
impl Dimensions {
    fn sanitize(&mut self) {
        self.networks = self
            .networks
            .iter()
            .filter_map(|value| network(value).map(str::to_owned))
            .take(16)
            .collect();
        self.protocols = self
            .protocols
            .iter()
            .filter(|value| PROTOCOLS.contains(&value.as_str()))
            .take(16)
            .cloned()
            .collect();
        self.bridges = self
            .bridges
            .iter()
            .filter(|value| BRIDGES.contains(&value.as_str()))
            .take(16)
            .cloned()
            .collect();
    }
    fn operation(&mut self, operation: &str) {
        let protocol = if operation.starts_with("aave.") {
            Some("aave_v3")
        } else if operation.starts_with("jupiter.perps.") {
            Some("jupiter_perps")
        } else {
            operation
                .split_once('.')
                .map(|(prefix, _)| prefix)
                .filter(|prefix| PROTOCOLS.contains(prefix))
        };
        if let Some(protocol) = protocol {
            self.protocols.insert(protocol.into());
        }
    }
    fn read(&mut self, value: &Value, depth: usize) {
        if depth > 5 {
            return;
        }
        let Some(object) = value.as_object() else {
            return;
        };
        if object.get("network").and_then(Value::as_str) == Some("solana")
            && object.get("cluster").and_then(Value::as_str) == Some("mainnet-beta")
        {
            self.networks.insert("solana:mainnet-beta".into());
        }
        for key in ["network", "source_network", "destination_network"] {
            if let Some(value) = object.get(key).and_then(Value::as_str).and_then(network) {
                self.networks.insert(value.into());
            }
        }
        for key in [
            "chain_id",
            "source_chain_id",
            "destination_chain_id",
            "chainId",
            "sourceChainId",
            "destinationChainId",
        ] {
            if let Some(id) = object.get(key).and_then(Value::as_u64)
                && let Some(network) = gofin_networks::by_chain_id(id)
            {
                self.networks.insert(network.network_id.into());
            }
        }
        for key in ["protocol", "protocol_version", "amm_protocol"] {
            if let Some(value) = object
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| PROTOCOLS.contains(value))
            {
                self.protocols.insert(value.into());
            }
        }
        // AMM contracts carry a reviewed DEX and a separate short version (v3/v4).
        // Do not infer a DEX from an arbitrary version string alone.
        if let Some(dex) = object.get("dex").and_then(Value::as_str) {
            let version = object
                .get("protocol")
                .or_else(|| object.get("version"))
                .and_then(Value::as_str);
            let protocol = match (dex, version) {
                ("uniswap", Some("v3")) => Some("uniswap_v3"),
                ("uniswap", Some("v4")) => Some("uniswap_v4"),
                ("sushi", Some("v3")) => Some("sushi_v3"),
                ("pancakeswap", Some("v3")) => Some("pancakeswap_v3"),
                ("ramses", Some("v3")) => Some("ramses_v3"),
                _ => None,
            };
            if let Some(protocol) = protocol {
                self.protocols.insert(protocol.into());
            }
        }
        if object
            .get("cctp")
            .and_then(|metadata| metadata.get("protocol_version"))
            .and_then(Value::as_u64)
            == Some(2)
        {
            self.protocols.insert("cctp_v2".into());
        }
        for key in ["provider", "bridge_provider"] {
            if let Some(value) = object
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| BRIDGES.contains(value))
            {
                self.bridges.insert(value.into());
            }
        }
        // Only structural fields owned by financial operation contracts are traversed.
        // Arbitrary metadata, logs, history, error details and stringified JSON are ignored.
        for key in [
            "result",
            "quote",
            "route",
            "intent",
            "source",
            "destination",
            "input",
            "execution",
            "receipt",
            "prepared",
            "preview",
            "binding",
            "resource",
            "call",
            "solana_source",
            "solana_destination",
            "solanaSource",
            "solanaDestination",
            "destination_asset",
            "destinationAsset",
            "destination_locator",
        ] {
            if let Some(value) = object.get(key) {
                self.read(value, depth + 1);
            }
        }
        for key in [
            "routes",
            "hops",
            "protocol_steps",
            "steps",
            "quotes",
            "transfers",
            "destination_assets",
            "destinationAssets",
        ] {
            if let Some(values) = object.get(key).and_then(Value::as_array) {
                for value in values.iter().take(16) {
                    self.read(value, depth + 1);
                }
            }
        }
        self.sanitize();
    }

    fn verified_effects(&mut self, value: &Value) {
        let Some(effects) = value.get("effects").and_then(Value::as_array) else {
            return;
        };
        for effect in effects.iter().take(32) {
            // Network identity only; quantities, locators, fingerprints and raw proof
            // evidence never enter analytics. These are typed FinancialEffect slots.
            for pointer in [
                "/asset/network",
                "/input_asset/network",
                "/output_asset/network",
                "/position_asset/network",
                "/cost_asset/network",
                "/amount/asset/network",
                "/debit/asset/network",
                "/credit/asset/network",
                "/resource/network",
                "/from/network",
                "/to/network",
            ] {
                if let Some(network) = effect
                    .pointer(pointer)
                    .and_then(Value::as_str)
                    .and_then(network)
                {
                    self.networks.insert(network.into());
                }
            }
        }
        self.sanitize();
    }
}
fn network(value: &str) -> Option<&str> {
    if value == "solana:mainnet-beta" {
        Some("solana:mainnet-beta")
    } else {
        gofin_networks::by_network_id(value).map(|network| network.network_id)
    }
}

/// Sanitized state only: never retain the original arguments, request or response.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandContext {
    command: String,
    operation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    execution_mode: Option<ExecutionMode>,
    machine: bool,
    requested: Dimensions,
    resolved: Dimensions,
    observed: Dimensions,
    result_status: Option<String>,
}
impl CommandContext {
    pub fn new(name: &str, operation: Option<&str>, input: Option<&Value>, machine: bool) -> Self {
        let operation = operation.map(safe_operation).map(str::to_owned);
        let mut requested = Dimensions::default();
        if let Some(operation) = &operation {
            requested.operation(operation);
            if operation != "unknown"
                && let Some(input) = input
            {
                requested.read(input, 0);
            }
        } else if name.starts_with("connections")
            && let Some(input) = input
        {
            requested.read(input, 0);
        } else if matches!(name, "plans.preview" | "plans.authorize")
            && let Some(steps) = input
                .and_then(|input| input.get("steps"))
                .and_then(Value::as_array)
        {
            // FinancialPlanSpec contains code-owned operation names and typed inputs.
            // Do not inspect arbitrary nested JSON or financial permission bounds.
            for step in steps.iter().take(32) {
                if let Some(operation) = step.get("operation").and_then(Value::as_str)
                    && safe_operation(operation) != "unknown"
                {
                    requested.operation(operation);
                    if let Some(input) = step.get("input") {
                        requested.read(input, 0);
                    }
                }
            }
        }
        Self {
            command: command(name).into(),
            operation,
            execution_mode: None,
            machine,
            requested,
            resolved: Dimensions::default(),
            observed: Dimensions::default(),
            result_status: None,
        }
    }
    pub fn with_execution_mode(mut self, mode: ExecutionMode) -> Self {
        if self.command == "execute" {
            self.execution_mode = Some(mode);
        }
        self
    }
    pub fn observe_result(&mut self, result: &Value) {
        if self.command == "status"
            && self.operation.is_none()
            && result.get("preview").is_some()
            && result
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| RESULT_STATUSES.contains(&status))
        {
            self.operation = result
                .get("operation")
                .and_then(Value::as_str)
                .map(safe_operation)
                .map(str::to_owned);
        }
        // Lifecycle statuses are finite and are not interpreted as fresh exceptions.
        self.result_status = result
            .get("status")
            .and_then(Value::as_str)
            .filter(|status| RESULT_STATUSES.contains(status))
            .map(str::to_owned);
        if self
            .operation
            .as_deref()
            .is_some_and(|operation| operation != "unknown")
        {
            self.resolved.read(result, 0);
            if let Some(receipt) = result.get("receipt") {
                self.observed.read(receipt, 0);
            }
            if let Some(effects) = result.get("effects") {
                self.observed.verified_effects(effects);
            }
        }
    }
    fn sanitize(&mut self) {
        self.command = command(&self.command).into();
        if self.command != "execute" {
            self.execution_mode = None;
        }
        self.operation = self
            .operation
            .as_deref()
            .map(safe_operation)
            .map(str::to_owned);
        self.requested.sanitize();
        self.resolved.sanitize();
        self.observed.sanitize();
        if !self
            .result_status
            .as_deref()
            .is_none_or(|status| RESULT_STATUSES.contains(&status))
        {
            self.result_status = None;
        }
    }
    pub fn events(&self, outcome: Outcome, error: Option<(&str, &str)>) -> Vec<Event> {
        let mut context = self.clone();
        context.sanitize();
        let id = Uuid::new_v4().to_string();
        let mut events = vec![Event {
            id: id.clone(),
            data: EventData::Command {
                context: Box::new(context),
                outcome,
                error_code: error.map(|(code, _)| error_code(code).into()),
                error_stage: error.map(|(_, stage)| error_stage(stage).into()),
            },
        }];
        if outcome == Outcome::Failed
            && let Some((code, stage)) = error
        {
            events.push(Event {
                id,
                data: EventData::Exception {
                    component: Component::Cli,
                    operation: self
                        .operation
                        .as_deref()
                        .map(safe_operation)
                        .map(str::to_owned),
                    code: error_code(code).into(),
                    stage: error_stage(stage).into(),
                    location: None,
                },
            });
        }
        events
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    id: String,
    data: EventData,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum EventData {
    Command {
        context: Box<CommandContext>,
        outcome: Outcome,
        error_code: Option<String>,
        error_stage: Option<String>,
    },
    Lifecycle {
        lifecycle: Lifecycle,
        outcome: LifecycleOutcome,
        from_version: Option<String>,
        to_version: Option<String>,
        stage: Option<String>,
        rollback: Option<Rollback>,
    },
    Exception {
        component: Component,
        operation: Option<String>,
        code: String,
        stage: String,
        location: Option<SourceLocation>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceLocation {
    file: String,
    line: u32,
}

pub fn lifecycle_events(
    lifecycle: Lifecycle,
    outcome: LifecycleOutcome,
    from: Option<&str>,
    to: Option<&str>,
    stage: Option<&str>,
    rollback: Option<Rollback>,
) -> Vec<Event> {
    let id = Uuid::new_v4().to_string();
    let mut events = vec![Event {
        id: id.clone(),
        data: EventData::Lifecycle {
            lifecycle,
            outcome,
            from_version: version(from),
            to_version: version(to),
            stage: stage.map(|stage| member(stage, LIFECYCLE_STAGES, "other").into()),
            rollback,
        },
    }];
    if outcome == LifecycleOutcome::Failed {
        events.push(Event {
            id,
            data: EventData::Exception {
                component: Component::Cli,
                operation: None,
                code: match lifecycle {
                    Lifecycle::Install => "INSTALL_FAILED",
                    Lifecycle::Update => "UPDATE_FAILED",
                }
                .into(),
                stage: member(stage.unwrap_or("other"), LIFECYCLE_STAGES, "other").into(),
                location: None,
            },
        });
    }
    events
}
pub fn exception_events(
    component: Component,
    operation: Option<&str>,
    code: &str,
    stage: &str,
) -> Vec<Event> {
    if matches!(
        code,
        "ENROLLMENT_CANCELLED"
            | "AUTHORIZATION_CANCELLED"
            | "USER_CANCELLED"
            | "CANCELLED"
            | "CANCELED"
    ) {
        return Vec::new();
    }
    vec![Event {
        id: Uuid::new_v4().to_string(),
        data: EventData::Exception {
            component,
            operation: operation.map(safe_operation).map(str::to_owned),
            code: error_code(code).into(),
            stage: error_stage(stage).into(),
            location: None,
        },
    }]
}
pub(crate) fn panic_events(component: Component, file: &str, line: u32) -> Vec<Event> {
    let file = vocabulary::SOURCES
        .iter()
        .copied()
        .find(|source| {
            file == *source
                || file
                    .strip_suffix(*source)
                    .is_some_and(|prefix| prefix.ends_with('/'))
        })
        .unwrap_or("external");
    vec![Event {
        id: Uuid::new_v4().to_string(),
        data: EventData::Exception {
            component,
            operation: None,
            code: "PANIC".into(),
            stage: "runtime".into(),
            location: Some(SourceLocation {
                file: file.into(),
                line: if file == "external" { 0 } else { line },
            }),
        },
    }]
}

impl Event {
    pub(crate) fn sanitize(&mut self) {
        if Uuid::parse_str(&self.id).is_err() {
            self.id = Uuid::new_v4().to_string();
        }
        match &mut self.data {
            EventData::Command {
                context,
                error_code: code,
                error_stage: stage,
                ..
            } => {
                context.sanitize();
                *code = code.as_deref().map(error_code).map(str::to_owned);
                *stage = stage.as_deref().map(error_stage).map(str::to_owned);
            }
            EventData::Lifecycle {
                from_version,
                to_version,
                stage,
                ..
            } => {
                *from_version = version(from_version.as_deref());
                *to_version = version(to_version.as_deref());
                *stage = stage
                    .as_deref()
                    .map(|stage| member(stage, LIFECYCLE_STAGES, "other").into());
            }
            EventData::Exception {
                operation,
                code,
                stage,
                location,
                ..
            } => {
                *operation = operation.as_deref().map(safe_operation).map(str::to_owned);
                *code = error_code(code).into();
                *stage = if code == "PANIC" {
                    "runtime".into()
                } else {
                    error_stage(stage).into()
                };
                if let Some(location) = location {
                    location.file = member(&location.file, vocabulary::SOURCES, "external").into();
                    if location.file == "external" {
                        location.line = 0;
                    }
                }
            }
        }
    }
    /// Build only allowlisted capture properties, including when received by the hidden helper.
    pub fn capture(&self) -> Value {
        let mut properties = json!({"$process_person_profile":false,"$geoip_disable":true,"$ip":null,"release":release_version(),"os":std::env::consts::OS,"arch":std::env::consts::ARCH});
        let name = match &self.data {
            EventData::Command {
                context,
                outcome,
                error_code: code,
                error_stage: stage,
            } => {
                let mut context = context.clone();
                context.sanitize();
                properties["command"] = json!(context.command);
                properties["operation"] = json!(context.operation);
                if let Some(mode) = context.execution_mode {
                    properties["execution_mode"] = json!(mode);
                }
                properties["machine"] = json!(context.machine);
                properties["outcome"] = json!(outcome);
                properties["result_status"] = json!(context.result_status);
                properties["requested"] = json!(context.requested);
                properties["resolved"] = json!(context.resolved);
                properties["observed"] = json!(context.observed);
                properties["error_code"] = json!(code.as_deref().map(error_code));
                properties["error_stage"] = json!(stage.as_deref().map(error_stage));
                "gofin.command_completed"
            }
            EventData::Lifecycle {
                lifecycle,
                outcome,
                from_version,
                to_version,
                stage,
                rollback,
            } => {
                properties["outcome"] = json!(outcome);
                properties["from_version"] = json!(version(from_version.as_deref()));
                properties["to_version"] = json!(version(to_version.as_deref()));
                properties["stage"] = json!(stage.as_deref().map(|stage| member(
                    stage,
                    LIFECYCLE_STAGES,
                    "other"
                )));
                properties["rollback"] = json!(rollback);
                match lifecycle {
                    Lifecycle::Install => "gofin.install",
                    Lifecycle::Update => "gofin.update",
                }
            }
            EventData::Exception {
                component,
                operation,
                code,
                stage,
                location,
            } => {
                let code = error_code(code);
                let stage = if code == "PANIC" {
                    "runtime"
                } else {
                    error_stage(stage)
                };
                let operation = operation.as_deref().map(safe_operation);
                let location = location.as_ref().map(|location| {
                    let file = member(&location.file, vocabulary::SOURCES, "external");
                    (file, if file == "external" { 0 } else { location.line })
                });
                let fingerprint = format!(
                    "{component:?}:{}:{code}:{stage}:{}:{}",
                    operation.unwrap_or("none"),
                    location.map_or("none", |(file, _)| file),
                    location.map_or(0, |(_, line)| line)
                );
                properties["component"] = json!(component);
                properties["operation"] = json!(operation);
                properties["error_code"] = json!(code);
                properties["error_stage"] = json!(stage);
                properties["$exception_fingerprint"] = json!(fingerprint);
                let mut exception = json!({"type":code,"value":if code == "PANIC" {"GoFin runtime panic"} else {"GoFin command failed"},"mechanism":{"type":"generic","handled":code != "PANIC"}});
                if let Some((file, line)) = location {
                    exception["stacktrace"] = json!({"type":"raw","frames":[{"platform":"custom","lang":"rust","function":"GoFin panic","filename":file,"lineno":line,"in_app":file != "external"}]});
                }
                properties["$exception_list"] = json!([exception]);
                "$exception"
            }
        };
        let id = Uuid::parse_str(&self.id).unwrap_or_else(|_| Uuid::new_v4());
        json!({"event":name,"distinct_id":id,"properties":properties})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_mode_is_typed_optional_and_revalidated_after_handoff() {
        for (mode, expected) in [
            (ExecutionMode::Plan, "plan"),
            (ExecutionMode::BoundedAuthorization, "bounded_authorization"),
            (ExecutionMode::Standalone, "standalone"),
        ] {
            let context = CommandContext::new("execute", Some("bridge.execute"), None, false)
                .with_execution_mode(mode);
            let bytes = serde_json::to_vec(&context.events(Outcome::Succeeded, None)).unwrap();
            let events: Vec<Event> = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                events[0].capture()["properties"]["execution_mode"],
                expected
            );
            let mut value = serde_json::to_value(&context).unwrap();
            value["command"] = json!("preview");
            let preview: CommandContext = serde_json::from_value(value.clone()).unwrap();
            assert!(
                preview.events(Outcome::Succeeded, None)[0].capture()["properties"]
                    .get("execution_mode")
                    .is_none()
            );
            value["execution_mode"] = json!("private-plan-id");
            assert!(serde_json::from_value::<CommandContext>(value.clone()).is_err());
            value.as_object_mut().unwrap().remove("execution_mode");
            value["command"] = json!("execute");
            let legacy: CommandContext = serde_json::from_value(value).unwrap();
            assert!(
                legacy.events(Outcome::Succeeded, None)[0].capture()["properties"]
                    .get("execution_mode")
                    .is_none()
            );
        }
    }

    #[test]
    fn sanitizes_all_inputs_and_tracks_known_route_fields() {
        let secret = "private-wallet-secret";
        let mut context = CommandContext::new(
            "execute",
            Some("bridge.execute"),
            Some(
                &json!({"source_chain_id":1,"destination_chain_id":42161,"provider":"relay","wallet":secret,"amount":"1234","destination": {"network":"solana:mainnet-beta","address":secret}, "metadata":{"network":"ethereum:8453"}}),
            ),
            true,
        );
        context.observe_result(&json!({"status":"pending","receipt":{"chain_id":42161},"quote":{"provider":"cctp","protocol_version":"cctp_v2"},"error":{"code":secret,"message":secret}}));
        let events = context.events(Outcome::Failed, Some((secret, secret)));
        let capture = events.iter().map(Event::capture).collect::<Vec<_>>();
        let serialized = serde_json::to_string(&capture).unwrap();
        assert!(!serialized.contains(secret));
        assert!(!serialized.contains("1234"));
        assert!(!serialized.contains("ethereum:8453"));
        assert_eq!(capture[0]["properties"]["error_code"], "UNKNOWN_ERROR");
        assert_eq!(capture[0]["properties"]["result_status"], "pending");
        assert!(
            capture[0]["properties"]["requested"]["networks"]
                .as_array()
                .unwrap()
                .contains(&json!("ethereum:1"))
        );
        assert_eq!(capture[0]["distinct_id"], capture[1]["distinct_id"]);
    }

    #[test]
    fn invocation_ids_change_and_status_does_not_create_exception() {
        let mut context = CommandContext::new("status", None, None, false);
        context.observe_result(&json!({"status":"failed","error":{"code":"INVALID_INPUT"}}));
        let first = context.events(Outcome::Succeeded, None);
        let second = context.events(Outcome::Succeeded, None);
        assert_eq!(first.len(), 1);
        assert_ne!(
            first[0].capture()["distinct_id"],
            second[0].capture()["distinct_id"]
        );
        assert_eq!(
            first[0].capture()["properties"]["$process_person_profile"],
            false
        );
    }

    #[test]
    fn helper_deserialization_cannot_bypass_allowlists() {
        let events = exception_events(
            Component::Broker,
            Some("unknown.secret"),
            "UNRECOGNIZED_SECRET",
            "secret-stage",
        );
        let mut serialized = serde_json::to_value(&events).unwrap();
        serialized[0]["data"]["location"] = json!({"file":"/Users/private/account.rs","line":123});
        let events: Vec<Event> = serde_json::from_value(serialized).unwrap();
        let capture = events[0].capture();
        assert!(!capture.to_string().contains("private"));
        assert_eq!(capture["properties"]["operation"], "unknown");
        assert_eq!(
            capture["properties"]["$exception_list"][0]["stacktrace"]["frames"][0]["lineno"],
            0
        );
    }

    #[test]
    fn lifecycle_versions_stages_and_rollback_are_bounded() {
        let capture = lifecycle_events(
            Lifecycle::Update,
            LifecycleOutcome::Failed,
            Some("v0.2.0-beta.1"),
            Some("/private/secret"),
            Some("/private/secret"),
            Some(Rollback::Uncertain),
        )[0]
        .capture();
        assert_eq!(capture["properties"]["from_version"], "0.2.0-beta.1");
        assert_eq!(capture["properties"]["to_version"], Value::Null);
        assert_eq!(capture["properties"]["stage"], "other");
        assert_eq!(capture["properties"]["rollback"], "uncertain");
    }

    #[test]
    fn panic_reports_only_code_owned_source_location() {
        let capture = panic_events(
            Component::Cli,
            "/private/build/crates/gofin-cli/src/main.rs",
            42,
        )[0]
        .capture();
        assert!(!capture.to_string().contains("/private/build"));
        assert_eq!(
            capture["properties"]["$exception_list"][0]["stacktrace"]["frames"][0]["filename"],
            "crates/gofin-cli/src/main.rs"
        );
        assert_eq!(
            panic_events(Component::Broker, "/private/secret.rs", 22)[0].capture()["properties"]["$exception_list"]
                [0]["stacktrace"]["frames"][0]["filename"],
            "external"
        );
    }

    #[test]
    fn public_bridge_inputs_and_resolved_amm_protocols_have_exact_dimensions() {
        let mut context = CommandContext::new(
            "preview",
            Some("bridge.quotes"),
            Some(&json!({
                "provider":"base_canonical", "intent":{
                    "source":{"chainId":1,"token":"SECRET"},
                    "destinationChainId":8453,
                    "solanaDestination":{"network":"solana:mainnet-beta","recipient":"SECRET"}
                }
            })),
            false,
        );
        context.observe_result(&json!({"quotes":[{"provider":"arbitrum_canonical","intent":{"source":{"chain_id":1},"destination_chain_id":42161}}]}));
        let capture = context.events(Outcome::Succeeded, None)[0].capture();
        assert_eq!(
            capture["properties"]["requested"]["bridges"],
            json!(["base_canonical"])
        );
        assert_eq!(
            capture["properties"]["requested"]["networks"],
            json!(["ethereum:1", "ethereum:8453", "solana:mainnet-beta"])
        );
        assert_eq!(
            capture["properties"]["resolved"]["bridges"],
            json!(["arbitrum_canonical"])
        );
        assert!(!capture.to_string().contains("SECRET"));

        let mut swap = CommandContext::new(
            "preview",
            Some("swap.execute"),
            Some(&json!({"chain_id":8453})),
            false,
        );
        swap.observe_result(&json!({"quote":{"route":{"dex":"uniswap","version":"v4"}},"metadata":{"protocol":"pancakeswap_v3"}}));
        assert_eq!(
            swap.events(Outcome::Succeeded, None)[0].capture()["properties"]["resolved"]["protocols"],
            json!(["uniswap_v4"])
        );
    }

    #[test]
    fn bridge_transfer_records_and_verified_effects_preserve_phase() {
        let quote = json!({"provider":"relay","intent":{"source":{"chain_id":1},"solana_destination":{"network":"solana:mainnet-beta","recipient":"SECRET"}}});
        let mut quotes = CommandContext::new("read", Some("bridge.quotes"), None, true);
        quotes.observe_result(&json!({"transfers":[{"transfer_id":"SECRET","quote":quote}]}));
        assert_eq!(
            quotes.events(Outcome::Succeeded, None)[0].capture()["properties"]["resolved"]["bridges"],
            json!(["relay"])
        );
        let mut transfer = CommandContext::new(
            "execute",
            Some("bridge.transfer"),
            Some(&json!({"transferId":"SECRET","stepId":"SECRET"})),
            false,
        );
        let record = json!({"operation":"bridge.transfer","status":"submitted","preview":{"binding":{"quote":quote}},"error":null,"effects":null});
        transfer.observe_result(&record);
        let capture = transfer.events(Outcome::Succeeded, None)[0].capture();
        assert_eq!(
            capture["properties"]["resolved"]["bridges"],
            json!(["relay"])
        );
        assert_eq!(capture["properties"]["observed"]["networks"], json!([]));
        assert_eq!(capture["properties"]["result_status"], "submitted");
        let mut status = CommandContext::new("status", None, None, true);
        status.observe_result(&json!({"operation":"bridge.transfer","status":"completed","preview":record["preview"],"effects":{"effects":[{"kind":"transfer","debit":{"asset":{"network":"ethereum:1","reference":"SECRET"},"atoms":"987654321"},"credit":{"asset":{"network":"solana:mainnet-beta","reference":"SECRET"},"atoms":"987654321"}}],"evidence":{"network":"ethereum:8453","secret":"SECRET"}},"error":{"code":"BROKER_FAILED","message":"SECRET"}}));
        let events = status.events(Outcome::Succeeded, None);
        assert_eq!(events.len(), 1);
        let capture = events[0].capture();
        assert_eq!(capture["properties"]["command"], "status");
        assert_eq!(capture["properties"]["result_status"], "completed");
        assert_eq!(
            capture["properties"]["observed"]["networks"],
            json!(["ethereum:1", "solana:mainnet-beta"])
        );
        assert!(!capture.to_string().contains("SECRET"));
        assert!(!capture.to_string().contains("987654321"));
    }

    #[test]
    fn solana_pair_and_lifecycle_failures_are_sanitized() {
        let context = CommandContext::new(
            "read",
            Some("jupiter.quote"),
            Some(&json!({"network":"solana","cluster":"mainnet-beta"})),
            true,
        );
        assert_eq!(
            context.events(Outcome::Succeeded, None)[0].capture()["properties"]["requested"]["networks"],
            json!(["solana:mainnet-beta"])
        );
        let failed = lifecycle_events(
            Lifecycle::Install,
            LifecycleOutcome::Failed,
            None,
            Some("0.2.0"),
            Some("service"),
            Some(Rollback::NotNeeded),
        );
        assert_eq!(failed.len(), 2);
        assert_eq!(failed[1].capture()["event"], "$exception");
        assert_eq!(
            failed[1].capture()["properties"]["error_code"],
            "INSTALL_FAILED"
        );
        assert_eq!(failed[1].capture()["properties"]["error_stage"], "service");
        assert_eq!(
            failed[0].capture()["distinct_id"],
            failed[1].capture()["distinct_id"]
        );
        for outcome in [
            LifecycleOutcome::Succeeded,
            LifecycleOutcome::Attempted,
            LifecycleOutcome::Canceled,
        ] {
            assert_eq!(
                lifecycle_events(Lifecycle::Update, outcome, None, None, None, None).len(),
                1
            );
        }
    }

    #[test]
    fn requested_plan_dimensions_only_use_recognized_steps_and_never_count_them() {
        let input = json!({"name":"SECRET", "steps":[
            {"step_id":"SECRET", "operation":"aave.withdraw", "input":{"chain_id":1,"wallet":"SECRET"}},
            {"step_id":"SECRET", "operation":"bridge.execute", "input":{"provider":"cctp","intent":{"source":{"chainId":1},"destinationChainId":8453}}},
            {"operation":"SECRET", "input":{"chain_id":42161}},
        ],"metadata":{"steps":[{"operation":"meteora.swap","input":{"network":"solana:mainnet-beta"}}]}});
        let events = CommandContext::new("plans.authorize", None, Some(&input), true)
            .events(Outcome::Succeeded, None);
        assert_eq!(events.len(), 1);
        let capture = events[0].capture();
        assert_eq!(
            capture["properties"]["requested"]["networks"],
            json!(["ethereum:1", "ethereum:8453"])
        );
        assert_eq!(
            capture["properties"]["requested"]["protocols"],
            json!(["aave_v3"])
        );
        assert_eq!(
            capture["properties"]["requested"]["bridges"],
            json!(["cctp"])
        );
        assert!(!capture.to_string().contains("SECRET"));
    }
}
