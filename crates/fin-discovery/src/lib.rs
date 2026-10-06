//! Shared safe CLI arguments, validation, and offline onboarding discovery.
//! Enrollment DTOs were drafted by Ogi Spark/GLM and reviewed against the CLI contract.
use clap::{Args, ValueEnum};
use fin_domain::{
    Definition, ExecutionActor, FinError, OperationKind, PlanStep, ResourceAmount, Result,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub mod enrollment_contract;

/// Shared human-facing explanation for macOS administrator authentication.
pub const MACOS_ADMIN_PASSWORD_PROMPT: &str = "Mac login password for %p: ";
pub const MACOS_ADMIN_PASSWORD_HELP: &str = "macOS will ask for your Mac login password (typing is hidden). Touch ID follows. Ctrl-C cancels.";

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum Provider {
    Binance,
    Ethereum,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum BinanceRole {
    Main,
    Sub,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum CredentialType {
    Hmac,
    Rsa,
    Ed25519,
    PrivateKey,
    ReadOnly,
}

#[derive(Args, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddConnectionArgs {
    /// Provider to enroll. Secret values are never command arguments.
    #[arg(long, value_enum)]
    pub provider: Provider,
    /// Stable local name matching [a-z][a-z0-9_-]{0,63}.
    #[arg(long)]
    pub name: String,
    /// Binance account role; main-authorized universal transfers need a main connection.
    #[arg(long, value_enum)]
    pub role: Option<BinanceRole>,
    /// Existing main connection for a Binance subaccount.
    #[arg(long)]
    pub parent: Option<String>,
    /// Binance subaccount identity; not a credential.
    /// Accepted only for role=sub.
    #[arg(long)]
    pub email: Option<String>,
    /// Explicit reviewed network: ethereum:1 for wallets.
    #[arg(long)]
    pub network: Option<String>,
    /// Expected public wallet address; checked against the protected signer.
    #[arg(long)]
    pub address: Option<String>,
    /// Credential category only: hmac/rsa/ed25519 or private_key/read_only.
    /// Binance prompts privately for both API key and signing secret/key.
    /// PEM input accepts actual line breaks or literal \n separators; include BEGIN/END lines and press Enter.
    #[arg(long, value_enum)]
    pub credential_type: Option<CredentialType>,
}
#[derive(Args, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NamedConnectionArgs {
    #[arg(long)]
    pub name: String,
}
#[derive(Args, Clone, Debug, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct EmptyArgs {}

#[derive(Clone, Copy, Debug, ValueEnum, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompletionInput {
    pub shell: CompletionShell,
}

/// Machine execution policy shared by offline discovery and callers.
pub fn machine_contract() -> Value {
    json!({"version":1,"flag":"--json","stdout":"one JSON value on success or error","stderr":"no human prose, progress or prompts","color":false,"terminal_prompts":false,"verbose":"ignored","exit_status":{"success":0,"error":1},"error_envelope":{"ok":false,"error":"FinError"},"human_interactive_commands":"Return HUMAN_TERMINAL_REQUIRED before privilege escalation or credential access; human runs the command without --json.","financial_writes":"Agent-invocable; retain out-of-band trusted review and fresh Touch ID for the exact write. No terminal prompts or approval bypass; client wait bounded to 300 seconds, then reconcile the same request ID."})
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationInput {
    pub operation: String,
    /// Validated against the selected operation's bundled input schema.
    pub input: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecuteInput {
    pub operation: String,
    /// Validated against the selected operation's bundled input schema.
    pub input: Value,
    pub request_id: String,
    pub run_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunBeginInput {
    pub allocations: Vec<ResourceAmount>,
    /// Separate borrowed-proceeds reserve, counted once alongside selected physical holdings.
    pub borrowed_proceeds_reserve_usd_micros: String,
    /// Earmark drawn from the selected physical allocation; this value is not added to the cap again.
    pub return_cost_reserve_usd_micros: String,
    pub planned_operations: Vec<PlanStep>,
    pub unwind: Vec<PlanStep>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunReplanInput {
    /// Fresh bounded cleanup steps; the broker supplies observer state and prices.
    pub remaining: Vec<PlanStep>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IdInput {
    pub id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NameInput {
    pub name: String,
}

pub fn validate_name(value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_lowercase()
        || !bytes[1..]
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
    {
        return Err(FinError::validation(
            "Connection names must match [a-z][a-z0-9_-]{0,63}.",
        ));
    }
    Ok(())
}
pub fn validate_add(args: &AddConnectionArgs) -> Result<()> {
    validate_name(&args.name)?;
    if let Some(parent) = &args.parent {
        validate_name(parent)?;
    }
    if args.email.as_ref().is_some_and(|email| {
        email.len() > 254 || !email.contains('@') || email.chars().any(char::is_control)
    }) {
        return Err(FinError::validation(
            "A valid Binance account email is required.",
        ));
    }
    match args.provider {
        Provider::Binance => {
            let role = args.role.ok_or_else(|| {
                FinError::validation("Binance requires --role main or --role sub.")
            })?;
            if args.network.is_some() || args.address.is_some() {
                return Err(FinError::validation(
                    "Binance funding uses SPOT accounts; wallet network/address belong to a separate wallet connection.",
                ));
            }
            if role == BinanceRole::Sub && (args.parent.is_none() || args.email.is_none()) {
                return Err(FinError::validation(
                    "A Binance subaccount requires --parent and --email.",
                ));
            }
            if role == BinanceRole::Main && args.parent.is_some() {
                return Err(FinError::validation(
                    "A Binance main account cannot have a parent connection.",
                ));
            }
            if role == BinanceRole::Main && args.email.is_some() {
                return Err(FinError::validation(
                    "A Binance main account cannot have a subaccount email.",
                ));
            }
            if !matches!(
                args.credential_type,
                None | Some(CredentialType::Hmac | CredentialType::Rsa | CredentialType::Ed25519)
            ) {
                return Err(FinError::validation(
                    "Binance credentials support hmac, rsa, or ed25519.",
                ));
            }
        }
        Provider::Ethereum => {
            if args.role.is_some() || args.parent.is_some() || args.email.is_some() {
                return Err(FinError::validation(
                    "Ethereum wallets do not accept Binance account relationships.",
                ));
            }
            if args.network.as_deref() != Some("ethereum:1") {
                return Err(FinError::validation(
                    "Wallet enrollment currently requires --network ethereum:1.",
                ));
            }
            if !matches!(
                args.credential_type,
                Some(CredentialType::PrivateKey | CredentialType::ReadOnly)
            ) {
                return Err(FinError::validation(
                    "Ethereum requires --credential-type private_key or read_only.",
                ));
            }
            if args.credential_type == Some(CredentialType::ReadOnly) && args.address.is_none() {
                return Err(FinError::validation(
                    "A read-only wallet requires --address.",
                ));
            }
            if args.address.as_ref().is_some_and(|address| {
                address.len() != 42
                    || !address.starts_with("0x")
                    || !address[2..].bytes().all(|b| b.is_ascii_hexdigit())
                    || address[2..].bytes().all(|b| b == b'0')
            }) {
                return Err(FinError::validation(
                    "The wallet address must be a nonzero 20-byte Ethereum address.",
                ));
            }
        }
    }
    Ok(())
}
pub fn schema<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).expect("static schema serialization")
}

fn definition<T: JsonSchema>(
    name: &str,
    description: &str,
    kind: OperationKind,
    actor: ExecutionActor,
    example: Value,
    semantics: Value,
) -> Definition {
    Definition{name:name.into(),description:description.into(),kind,execution_actor:actor,input_schema:schema::<T>(),output_schema:json!({"type":"object","description":"Structured result or typed error with observed checks and a concrete next action. No credential values."}),error_schema:schema::<FinError>(),examples:vec![example],coverage:"Implemented command contract; runtime platform and connection availability are reported when executed.".into(),completion:"Reports only the observed command result. Uncertain financial writes remain unresolved until reconciled.".into(),semantics}
}

pub fn operation_definitions() -> Vec<Definition> {
    let administrator_authentication = json!({
        "provider":"macOS sudo",
        "explanation":MACOS_ADMIN_PASSWORD_HELP,
        "password_prompt":MACOS_ADMIN_PASSWORD_PROMPT,
        "scope":"Setup and connection add/update/remove only. Everyday commands use the protected background service without sudo.",
        "configuration_approval":"A separate Touch ID approval follows administrator authentication.",
        "financial_approval":"Every financial write requires its own fresh Touch ID approval."
    });
    let enrollment = json!({
        "requires_human_terminal":true,"secret_input":"protected-hidden-terminal-prompt",
        "requires_administrator":true,"administrator_authentication":administrator_authentication.clone(),
        "secret_values_accepted_in_arguments":false,"secret_values_accepted_in_json":false,
        "human_action":"Run this command yourself in a new human-owned Terminal session. Never enter keys through an agent tool or chat.",
        "platform":"macOS protected enrollment; unavailable providers fail closed",
        "configuration_authorization":"Independent approval of the resolved connection. Saves configuration only, moves no funds, grants no future financial authority.",
        "financial_authorization":"Each later financial write, including cleanup, requires a separate fresh macOS Touch ID approval. Enrollment and daemon startup grant none.",
        "metadata_input":"Run fin connections add for guided public metadata, or supply complete documented flags. Updates require flags.",
        "pem_input":"In the private human-terminal prompt, paste the complete BEGIN/END block using actual line breaks or literal \\n (also \\r\\n) separators, without surrounding quotes, then press Enter. Never supply it through arguments or chat.",
        "input_progress":"Hidden input acknowledges each received secret without echoing it. Read-only validation and Touch ID stages appear on stderr.",
        "output_format":"Concise human summary on stdout; warnings/progress on stderr. --json returns a structured HUMAN_TERMINAL_REQUIRED handoff before enrollment.",
        "command_line_examples":[
            "fin connections add --provider binance --name main --role main --credential-type hmac",
            "fin connections add --provider binance --name main-rsa --role main --credential-type rsa",
            "fin connections add --provider binance --name main-ed25519 --role main --credential-type ed25519",
            "fin connections add --provider binance --name savings --role sub --parent main --email subaccount@example.invalid --credential-type hmac",
            "fin connections add --provider ethereum --name wallet --network ethereum:1 --credential-type private_key",
            "fin connections add --provider ethereum --name watch --network ethereum:1 --credential-type read_only --address 0x1111111111111111111111111111111111111111"
        ],
        "binance":{"wallet_type":"SPOT","default_credential_type":"hmac","supported_credential_types":["hmac","rsa","ed25519"],"mandatory_fields":["role=main|sub"],"credentials_required":["API key","HMAC secret, RSA PEM signing key, or Ed25519 private signing key"],"roles":["main","sub"],"main_rejects":["parent","email"],"sub_requires":["parent main connection","subaccount email"],"checks":["read-only authentication","reported API restrictions","account identity and parent mapping"],"prerequisites":["machine outbound IP allowed","main universal-transfer permission for internal transfers","withdrawal permission and whitelisted destination for withdrawals"],"unknown_permissions":"Explicitly reported; enrollment performs no trial transfer or withdrawal."},
        "ethereum":{"networks":["ethereum:1"],"credential_types":["private_key","read_only"],"checks":["public RPC chain identity","derived signing address matches resolved wallet","reported native balance"]},
        "failure":"Safe stable error and next action; add cancellation/validation leaves no candidate, while update cancellation/validation preserves the prior active connection. An uncertain commit remains blocked until configuration readback confirms its state.",
        "replacement":"Pending operations are checked by physical resource before changing/removing credentials; unresolved work refuses replacement.",
        "next_check":"fin connections list --json"
    });
    let mut definitions = vec![
        definition::<CompletionInput>(
            "completions",
            "Generate bash, zsh or fish completion for manual/custom use; setup installs completion automatically.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({"shell":"zsh"}),
            json!({"service_required":false,"command_line_example":"fin completions zsh","human_output":"shell script","machine_output":"object with shell and script"}),
        ),
        definition::<EmptyArgs>(
            "capabilities",
            "List offline command and financial operation definitions, schemas, examples, and execution metadata.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({}),
            json!({"service_required":false,"secret_input":"none","financial_movement":false}),
        ),
        definition::<NameInput>(
            "explain",
            "Explain a bundled Fin command or financial operation by its exact name.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({"name":"setup"}),
            json!({"service_required":false,"secret_input":"none","financial_movement":false}),
        ),
        definition::<NameInput>(
            "schema",
            "Show the bundled input, output, error, example, and actor metadata for a command or operation.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({"name":"connections.add"}),
            json!({"service_required":false,"secret_input":"none","financial_movement":false}),
        ),
        definition::<OperationInput>(
            "read",
            "Read one supported financial operation with fin read OPERATION --input JSON through the protected broker.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({"operation":"binance.balances","input":{"account":"main"}}),
            json!({"input_validation":"operation-specific bundled schema","service_required":true,"financial_authorization":false,"command_line_example":"fin read binance.balances --input '{\"account\":\"main\"}'","arguments":["operation","--input JSON"],"account_selection":"The operation-specific input schema defines account selection; there is no generic --account flag."}),
        ),
        definition::<OperationInput>(
            "preview",
            "Validate and preview one financial operation with fin preview OPERATION --input JSON without submitting a write.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({"operation":"binance.transfer","input":{"requestId":"synthetic-preview-1","authority":"main","from":"main","to":"savings","coin":"USDT","amount":"1","fromWallet":"SPOT","toWallet":"SPOT"}}),
            json!({"input_validation":"operation-specific bundled schema","service_required":true,"financial_authorization":false,"command_line_example":"fin preview binance.transfer --input '{\"requestId\":\"synthetic-preview-1\",\"authority\":\"main\",\"from\":\"main\",\"to\":\"savings\",\"coin\":\"USDT\",\"amount\":\"1\",\"fromWallet\":\"SPOT\",\"toWallet\":\"SPOT\"}'","arguments":["operation","--input JSON"]}),
        ),
        definition::<ExecuteInput>(
            "execute",
            "Execute one approved operation under a stable request identity.",
            OperationKind::Write,
            ExecutionActor::AgentInvocable,
            json!({"operation":"binance.transfer","input":{"requestId":"synthetic-venue-request-001","authority":"main","from":"main","to":"savings","coin":"USDT","amount":"1","fromWallet":"SPOT","toWallet":"SPOT"},"request_id":"synthetic-logical-write-001","run_id":"synthetic-bounded-run-001"}),
            json!({
                "input_validation":"operation-specific bundled schema",
                "service_required":true,
                "fresh_approval":"broker enforced per write",
                "retry":"reuse the same --id for this logical request; reconcile uncertain submissions before starting a new action",
                "command_line_template":"fin execute OPERATION --input JSON --id REQUEST_ID --run RUN_ID --json",
                "command_line_example":"fin execute binance.transfer --input '{\"requestId\":\"synthetic-venue-request-001\",\"authority\":\"main\",\"from\":\"main\",\"to\":\"savings\",\"coin\":\"USDT\",\"amount\":\"1\",\"fromWallet\":\"SPOT\",\"toWallet\":\"SPOT\"}' --id synthetic-logical-write-001 --run synthetic-bounded-run-001 --json",
                "arguments":["OPERATION","--input JSON","--id REQUEST_ID","--run RUN_ID","--json"],
                "cli_field_mapping":{"request_id":"--id","run_id":"--run"},
                "run_scope":"--run supplies the bounded run ID. It is required for the bounded verification acceptance flow and whenever protected runtime policy requires a run; it may be omitted only when policy permits an unscoped write.",
                "venue_request_identity":"A venue request identifier inside input JSON is separate from the logical --id value."
            }),
        ),
        definition::<IdInput>(
            "status",
            "Read and reconcile the durable status for one original operation record.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({"id":"operation-record-id"}),
            json!({"secret_input":"none","retry":"status never creates a new financial action"}),
        ),
        definition::<EmptyArgs>(
            "operations",
            "List durable operation records and unresolved submissions.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({}),
            json!({"secret_input":"none","service_required":true}),
        ),
        definition::<EmptyArgs>(
            "doctor",
            "Check protected service readiness and configured provider availability.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({}),
            json!({"secret_input":"none","financial_movement":false,"verification_scope":"Read-only readiness diagnostics, not proof of credential isolation or actual biometric approval. Those require their separate human verification scenarios."}),
        ),
        definition::<EmptyArgs>(
            "snapshot",
            "Capture the broker-observed financial state and completeness metadata.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({}),
            json!({"client_supplied_state":false,"service_required":true}),
        ),
        definition::<RunBeginInput>(
            "run.begin",
            "Capture a fresh baseline, select physical holdings, earmark return costs from those same holdings, and reserve a complete return path.",
            OperationKind::Configuration,
            ExecutionActor::AgentInvocable,
            json!({"allocations":[{"resource":"binance:main:spot","amount":{"asset":{"network":"binance","reference":"USDT","decimals":6},"atoms":"1000000"}}],"borrowed_proceeds_reserve_usd_micros":"0","return_cost_reserve_usd_micros":"1000000","planned_operations":[{"operation":"binance.transfer","input":{"requestId":"synthetic-forward","authority":"main","from":"main","to":"savings","coin":"USDT","amount":"1","fromWallet":"SPOT","toWallet":"SPOT"},"maximum_fee_usd_micros":"500000","description":"Move one selected USDT from the configured main account to its subaccount"}],"unwind":[{"operation":"binance.transfer","input":{"requestId":"synthetic-return","authority":"main","from":"savings","to":"main","coin":"USDT","amount":"1","fromWallet":"SPOT","toWallet":"SPOT"},"maximum_fee_usd_micros":"500000","description":"Return one USDT from the subaccount to the main account"}]}),
            json!({"baseline":"captured by trusted broker observer","client_supplied_state":false,"maximum_allocation_usd_micros":"100000000","cap_counts":"selected physical holdings plus borrowed-proceeds reserve","return_cost_reserve":"earmarked within selected physical holding value; never an additional source","cost_source":"each actual additional fee must be funded by selected-origin cash at its exact resource and asset"}),
        ),
        definition::<IdInput>(
            "run.status",
            "Read the durable run baseline, reservations, and restoration status.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({"id":"run-id"}),
            json!({"financial_movement":false}),
        ),
        definition::<RunReplanInput>(
            "run.replan",
            "Replace only the unexecuted return route after a fresh broker-observed state and bounded restoration proof.",
            OperationKind::Configuration,
            ExecutionActor::AgentInvocable,
            json!({"remaining":[{"operation":"wallet.transfer","input":{"wallet":"main-wallet","network":"ethereum","chain_id":1,"token":"native","amount":"1","recipient":"0x0000000000000000000000000000000000000001","request_id":"synthetic-replan","max_fee_eth":"0.001"},"maximum_fee_usd_micros":"1000","description":"Return the observed excess to its selected origin"}]}),
            json!({"baseline":"original run baseline retained","allocation":"unchanged","cost_budget":"remaining amount of original reserve","writes":"none; each later operation still requires fresh approval","unknown_submission":"must reconcile original identity before replanning"}),
        ),
        definition::<IdInput>(
            "run.finish",
            "Observe the final state and mark the run restored only when all tracked allocation is returned.",
            OperationKind::Configuration,
            ExecutionActor::AgentInvocable,
            json!({"id":"run-id"}),
            json!({"final_state":"captured by trusted broker observer","unresolved_state":"remains cleanup_outstanding"}),
        ),
        definition::<EmptyArgs>(
            "setup",
            "Install the bundled protected broker and native approval helper from a reviewed release.",
            OperationKind::Configuration,
            ExecutionActor::HumanInteractive,
            json!({}),
            json!({"requires_human_terminal":true,"requires_administrator":true,"administrator_authentication":administrator_authentication.clone(),"financial_movement":false,"secret_input":"none","shell_completion":{"automatic":true,"selection":"configured human account login shell and home from account database","supported":["zsh","bash","fish"],"activation":"new shell","unsupported":"brief notice; setup continues","cleanup":"Fin-owned files and exact marked blocks only; preserve user edits"},"installs":["one public fin CLI","protected internal broker","bundled native OS authorization helper","private credential/configuration/journal storage","macOS PATH registration in /etc/paths.d/fin"],"command_availability":"Open a new macOS Terminal after setup to use fin. Existing shells can run: export PATH=\"/Library/Fin/runtime:$PATH\". Custom PATH overrides must retain this directory.","boundary":"Installation authorizes no financial action. Run sudo -K to clear all cached sudo credentials, close elevated shells, and verify the agent lacks root access. This does not change Fin's per-write Touch ID requirement.","next_check":"fin daemon status --json"}),
        ),
        definition::<AddConnectionArgs>(
            "connections.add",
            "Add a named connection with private human terminal enrollment.",
            OperationKind::Configuration,
            ExecutionActor::HumanInteractive,
            json!({"provider":"binance","name":"main","role":"main","credential_type":"rsa"}),
            enrollment.clone(),
        ),
        definition::<AddConnectionArgs>(
            "connections.update",
            "Update a named connection through protected human enrollment while preserving its stable identity and provider relationship.",
            OperationKind::Configuration,
            ExecutionActor::HumanInteractive,
            json!({"provider":"binance","name":"main","role":"main","credential_type":"rsa"}),
            enrollment,
        ),
        definition::<NamedConnectionArgs>(
            "connections.remove",
            "Remove a connection after independent human configuration approval.",
            OperationKind::Configuration,
            ExecutionActor::HumanInteractive,
            json!({"name":"main"}),
            json!({"requires_human_terminal":true,"requires_administrator":true,"administrator_authentication":administrator_authentication,"secret_input":"none","financial_movement":false,"pending_work":"Refuse while an unresolved operation needs this physical resource or credential.","next_check":"fin connections list --json"}),
        ),
        definition::<EmptyArgs>(
            "connections.list",
            "Read safe named connection metadata; no keys are returned.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({}),
            json!({"secret_input":"none","financial_movement":false,"alias":"fin connections --json","next_check":"fin capabilities --json"}),
        ),
        definition::<EmptyArgs>(
            "daemon.start",
            "Start the fixed installed service; grants no financial permission.",
            OperationKind::Lifecycle,
            ExecutionActor::AgentInvocable,
            json!({}),
            json!({"secret_input":"none","financial_authorization":false,"trusted_code":"fixed installed broker; on-demand service activation","behavior":"Explicitly resume stopped broker and report observed running state; never grants financial authority."}),
        ),
        definition::<EmptyArgs>(
            "daemon.status",
            "Report observed running/stopped state without activating the broker.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({}),
            json!({"secret_input":"none","financial_movement":false,"broker_activation":false,"source":"launchd service state; no broker IPC","readiness":"Use fin doctor for configuration and readiness diagnostics."}),
        ),
        definition::<EmptyArgs>(
            "daemon.stop",
            "Stop gracefully and remain stopped until explicit start, preserving submitted identities and reservations.",
            OperationKind::Lifecycle,
            ExecutionActor::AgentInvocable,
            json!({}),
            json!({"secret_input":"none","journal":"Pending outcomes remain unresolved across restart; stop performs no financial cleanup.","restart":"Only explicit fin daemon start clears protected stop intent; ordinary commands cannot reactivate it."}),
        ),
    ];
    let mut connection_schema = schema::<AddConnectionArgs>();
    connection_schema["allOf"] = json!([
        {
            "if": {"properties":{"provider":{"const":"binance"}},"required":["provider"]},
            "then": {
                "required":["role"],
                "properties":{"credential_type":{"enum":["hmac","rsa","ed25519"]}},
                "oneOf":[
                    {"properties":{"role":{"const":"main"}},"not":{"anyOf":[{"required":["parent"]},{"required":["email"]}]}},
                    {"properties":{"role":{"const":"sub"}},"required":["parent","email"]}
                ]
            }
        },
        {
            "if": {"properties":{"provider":{"const":"ethereum"}},"required":["provider"]},
            "then": {
                "required":["network","credential_type"],
                "properties":{"network":{"const":"ethereum:1"},"credential_type":{"enum":["private_key","read_only"]}},
                "not":{"anyOf":[{"required":["role"]},{"required":["parent"]},{"required":["email"]}]},
                "oneOf":[
                    {"properties":{"credential_type":{"const":"private_key"}}},
                    {"properties":{"credential_type":{"const":"read_only"}},"required":["address"]}
                ]
            }
        }
    ]);
    for item in &mut definitions {
        item.semantics["machine_contract"] = machine_contract();
        if matches!(item.name.as_str(), "connections.add" | "connections.update") {
            item.input_schema = connection_schema.clone();
            item.output_schema = schema::<enrollment_contract::EnrollmentOutcome>();
            item.semantics["financial_movement"] = json!(false);
            item.semantics["future_financial_authority"] = json!(false);
        } else if item.name == "connections.remove" {
            item.output_schema = schema::<enrollment_contract::EnrollmentOutcome>();
            item.semantics["financial_movement"] = json!(false);
            item.semantics["future_financial_authority"] = json!(false);
        }
    }
    definitions
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(clap::Parser)]
    struct Cli {
        #[command(flatten)]
        add: AddConnectionArgs,
    }
    fn main_args() -> AddConnectionArgs {
        serde_json::from_value(
            json!({"provider":"binance","name":"main","role":"main","credential_type":"rsa"}),
        )
        .unwrap()
    }
    #[test]
    fn secret_fields_and_flags_are_rejected() {
        for key in ["secret", "api_key", "private_key", "password"] {
            let mut input = serde_json::to_value(main_args()).unwrap();
            input[key] = json!("SYNTHETIC");
            assert!(serde_json::from_value::<AddConnectionArgs>(input).is_err());
        }
        use clap::Parser;
        assert!(
            Cli::try_parse_from([
                "fin",
                "--provider",
                "binance",
                "--name",
                "main",
                "--role",
                "main",
                "--api-key",
                "SYNTHETIC"
            ])
            .is_err()
        );
    }
    #[test]
    fn cli_flags_match_typed_metadata() {
        use clap::Parser;
        let parsed = Cli::try_parse_from([
            "fin",
            "--provider",
            "binance",
            "--name",
            "main",
            "--role",
            "main",
            "--credential-type",
            "rsa",
        ])
        .unwrap();
        validate_add(&parsed.add).unwrap();
        let ed25519 = Cli::try_parse_from([
            "fin",
            "--provider",
            "binance",
            "--name",
            "main-ed25519",
            "--role",
            "main",
            "--credential-type",
            "ed25519",
        ])
        .unwrap();
        validate_add(&ed25519.add).unwrap();
        let definition = operation_definitions()
            .into_iter()
            .find(|d| d.name == "connections.add")
            .unwrap();
        assert_eq!(
            definition.input_schema["allOf"].as_array().unwrap().len(),
            2
        );
        assert_eq!(
            definition.output_schema,
            schema::<enrollment_contract::EnrollmentOutcome>()
        );
        assert_eq!(definition.execution_actor, ExecutionActor::HumanInteractive);
        assert_eq!(
            definition.semantics["secret_values_accepted_in_arguments"],
            false
        );
        assert_eq!(definition.semantics["financial_movement"], false);
        assert_eq!(definition.semantics["future_financial_authority"], false);
        assert_eq!(
            definition.semantics["metadata_input"],
            "Run fin connections add for guided public metadata, or supply complete documented flags. Updates require flags."
        );
        assert_eq!(
            definition.semantics["binance"]["supported_credential_types"],
            json!(["hmac", "rsa", "ed25519"])
        );
    }
    #[test]
    fn binance_enrollment_examples_parse_and_keep_parent_identity_semantics() {
        use clap::Parser;

        let definitions = operation_definitions();
        let add = definitions
            .iter()
            .find(|definition| definition.name == "connections.add")
            .unwrap();
        let examples = add.semantics["command_line_examples"].as_array().unwrap();
        let mut credential_types = Vec::new();
        let mut found_subaccount = false;
        for example in examples
            .iter()
            .filter_map(Value::as_str)
            .filter(|example| example.contains("--provider binance"))
        {
            let args = example
                .strip_prefix("fin connections add ")
                .expect("documented Binance enrollment command has the public CLI prefix")
                .split_whitespace()
                .collect::<Vec<_>>();
            let parsed = Cli::try_parse_from(std::iter::once("fin").chain(args)).unwrap();
            validate_add(&parsed.add).unwrap();
            credential_types.push(parsed.add.credential_type.unwrap());
            if parsed.add.role == Some(BinanceRole::Sub) {
                assert_eq!(parsed.add.parent.as_deref(), Some("main"));
                assert_eq!(
                    parsed.add.email.as_deref(),
                    Some("subaccount@example.invalid")
                );
                found_subaccount = true;
            } else {
                assert_eq!(parsed.add.role, Some(BinanceRole::Main));
                assert!(parsed.add.parent.is_none());
                assert!(parsed.add.email.is_none());
            }
        }
        assert!(found_subaccount);
        for kind in [
            CredentialType::Hmac,
            CredentialType::Rsa,
            CredentialType::Ed25519,
        ] {
            assert!(
                credential_types.contains(&kind),
                "missing example for {kind:?}"
            );
        }

        let update = definitions
            .iter()
            .find(|definition| definition.name == "connections.update")
            .unwrap();
        assert!(
            update
                .description
                .contains("preserving its stable identity and provider relationship")
        );
        assert_eq!(
            update.output_schema,
            schema::<enrollment_contract::EnrollmentOutcome>()
        );
        assert_eq!(update.semantics["financial_movement"], false);
        assert_eq!(update.semantics["future_financial_authority"], false);
    }
    #[test]
    fn relationship_and_network_errors_are_explicit() {
        let mut args = main_args();
        args.email = Some("not-a-main-identity@example.invalid".into());
        assert!(validate_add(&args).is_err());
        args.email = None;
        args.role = Some(BinanceRole::Sub);
        assert!(validate_add(&args).is_err());
        args.parent = Some("main".into());
        args.email = Some("synthetic@example.invalid".into());
        validate_add(&args).unwrap();
        args.provider = Provider::Ethereum;
        assert!(validate_add(&args).is_err());
        args.role = None;
        args.parent = None;
        args.email = None;
        args.network = Some("ethereum:1".into());
        args.credential_type = Some(CredentialType::ReadOnly);
        assert!(validate_add(&args).is_err());
        args.address = Some("0x1111111111111111111111111111111111111111".into());
        validate_add(&args).unwrap();
        args.network = Some("ethereum:2".into());
        assert!(validate_add(&args).is_err());
    }

    #[test]
    fn removal_schema_uses_explicit_name_flag() {
        use clap::Parser;
        #[derive(clap::Parser)]
        struct RemoveCli {
            #[command(flatten)]
            connection: NamedConnectionArgs,
        }
        let parsed = RemoveCli::try_parse_from(["fin", "--name", "main"]).unwrap();
        assert_eq!(parsed.connection.name, "main");
        assert!(RemoveCli::try_parse_from(["fin", "--secret", "SYNTHETIC"]).is_err());
    }
}
