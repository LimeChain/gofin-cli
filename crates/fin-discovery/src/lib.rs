//! Shared safe CLI arguments, validation, and offline onboarding discovery.
//! Enrollment DTOs were drafted by Ogi Spark/GLM and reviewed against the CLI contract.
use clap::{Args, ValueEnum};
use fin_domain::financial_plans::FinancialPlanSpec;
use fin_domain::{Definition, ExecutionActor, FinError, OperationKind, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub mod companion_skill;
pub mod enrollment_contract;
pub mod policy;
pub use policy::{PolicySetArgs, PolicyView};

/// Shared human-facing explanation for macOS administrator authentication.
pub const MACOS_ADMIN_PASSWORD_PROMPT: &str = "Mac login password for %p: ";
pub const MACOS_ADMIN_PASSWORD_HELP: &str = "macOS will ask for your Mac login password (typing is hidden). Touch ID follows. Ctrl-C cancels.";

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum Provider {
    Binance,
    #[serde(alias = "ethereum")]
    #[value(alias = "ethereum")]
    Evm,
    #[serde(rename = "coingecko")]
    #[value(name = "coingecko")]
    CoinGecko,
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
    ApiKey,
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
    /// Optional enrollment read-check network: ethereum:1, ethereum:42161 or ethereum:4663.
    /// The EVM wallet remains reusable across all reviewed networks.
    #[arg(long)]
    pub network: Option<String>,
    /// Expected public wallet address; checked against the protected signer.
    #[arg(long)]
    pub address: Option<String>,
    /// CoinGecko API plan: demo (default) or pro. Public metadata only.
    #[arg(long, value_parser = ["demo", "pro"])]
    pub api_plan: Option<String>,
    /// Credential category only: hmac/rsa/ed25519, private_key/read_only or api_key.
    /// api_key names a category; its secret value is collected privately, never through a flag.
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

/// Reviewed network capabilities available before enrollment or broker setup.
pub fn evm_network_profiles() -> Value {
    json!([{"network":"ethereum:1","name":"Ethereum Mainnet","chain_id":1,"native_coin":"ETH","operations":["wallet","Uniswap","Aave"]},{"network":"ethereum:42161","name":"Arbitrum One","chain_id":42161,"native_coin":"ETH","operations":["wallet","Uniswap"]},{"network":"ethereum:4663","name":"Robinhood Chain","chain_id":4663,"native_coin":"ETH","operations":["wallet","Uniswap"]}])
}

/// Offline bridge scope. The Relay settlement observer is intentionally kept
/// separate from wallet networks and never grants wallet authority.
pub fn bridge_profile() -> Value {
    json!({
        "providers": [
            {
                "id": "relay",
                "coverage": "Quote-dependent native ETH and explicitly reviewed ERC-20 routes between supported networks"
            },
            {
                "id": "arbitrum_canonical",
                "coverage": "Native ETH from Ethereum Mainnet to Arbitrum One or Robinhood Chain only; destination transaction identity is derived from finalized source proof"
            }
        ],
        "provider_credentials": "no provider keys; Relay uses anonymous quotes and the canonical provider uses configured read-only EVM RPC",
        "wallet_chains": [
            {"chain_id": 1, "network": "ethereum:1", "name": "Ethereum Mainnet"},
            {"chain_id": 42161, "network": "ethereum:42161", "name": "Arbitrum One"},
            {"chain_id": 4663, "network": "ethereum:4663", "name": "Robinhood Chain"}
        ],
        "settlement_observers": [
            {"chain_id": 537713, "name": "RelayChain", "role": "read_only_settlement_evidence", "wallet_authority": false}
        ],
        "solver_observers": [
            {"chain_id":8453,"name":"Base","role":"read_only_protocol_order_observer","wallet_authority":false}
        ],
        "assets": "native ETH and explicitly identified reviewed ERC-20 contracts; Relay coverage is quote-dependent, while the canonical provider supports native ETH only",
        "writes": "direct provider-reviewed deposits with finite token approvals only; canonical routes are native ETH only; no origin swaps or agent-supplied calldata",
        "quotes": "quotes report individual provider coverage, not a market-wide comparison or financial authorization",
        "input_schemas": {
            "bridge.quotes": serde_json::to_value(schemars::schema_for!(fin_bridge::BridgeQuoteInput)).expect("schema serializes"),
            "bridge.transfer": serde_json::to_value(schemars::schema_for!(fin_bridge::BridgeTransferInput)).expect("schema serializes"),
            "bridge.status": serde_json::to_value(schemars::schema_for!(fin_bridge::BridgeStatusInput)).expect("schema serializes")
        }
    })
}

/// Machine execution policy shared by offline discovery and callers.
pub fn machine_contract() -> Value {
    json!({"version":1,"flag":"--json","stdout":"one JSON value on success or error","stderr":"no human prose, progress or prompts","color":false,"terminal_prompts":false,"verbose":"ignored","exit_status":{"success":0,"error":1},"error_envelope":{"ok":false,"error":"FinError"},"human_interactive_commands":"Return HUMAN_TERMINAL_REQUIRED before privilege escalation or credential access; human runs the command without --json.","financial_writes":"Agent-invocable through exact per-write approval or an existing bounded authorization. Granting bounded authority still requires fresh macOS Touch ID; the OS approval UI may appear, but --json never prompts in the terminal. Client wait is bounded to 300 seconds, then reconcile the same request ID or authorization ID.","bounded_authorization":{"default_valid_for_seconds":600,"logical_intent_id":"--id on authorizations authorize","attempt_id":"--id on execute","execution_flag":"--authorization","dry_run":"fin execute OP --input JSON --authorization ID --dry-run evaluates fresh evidence without submission or consumption","revocation":"fin authorizations cancel ID prevents future dispatch without Touch ID","status":"fin authorizations status ID reconciles durable attempts"},"named_financial_plans":{"default_valid_for_seconds":600,"preview":"fin plans preview --input PLAN_JSON is read-only and returns a review_hash","authorization":"fin plans authorize --input PLAN_JSON --id REQUEST_ID [--valid-for 10m] [--replaces REVISION_ID] regenerates review and rejects changed review_hash before Touch ID","step_execution":"fin execute OP --input INPUT_JSON --id ATTEMPT_ID --plan PLAN_ID --step STEP_ID explicitly submits one eligible step","dry_run":"fin execute OP --input INPUT_JSON --plan PLAN_ID --step STEP_ID --dry-run refreshes evidence and reserves/submits nothing","replacement":"--replaces-attempt identifies only an unsubmitted plan candidate","status":"fin plans status PLAN_ID reconciles but never executes","revocation":"fin plans cancel PLAN_ID blocks future dispatch without cancelling submitted venue orders","generated_input_fields":"Only exact JSON pointers declared by the trusted operation definition may be assigned by Fin per attempt; the reviewed template remains bound and terminal review labels generated fields.","authority":"one plan revision authority; attempts retain their own IDs; no child grants"}})
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
    /// Required for execution; previews do not consume an identity.
    pub request_id: Option<String>,
    /// Same read-only preparation as `fin preview`, without approval/submission.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BoundaryAuthorizationInput {
    pub operation: String,
    /// Stable logical intent identity, mapped to the protocol request_id.
    pub id: String,
    pub input: Value,
    /// Operation-specific cumulative limits and permitted variations.
    pub bounds: Value,
    /// Human-readable duration such as 10m; defaults to 10m.
    #[serde(default = "default_boundary_duration")]
    pub valid_for: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<String>,
}

fn default_boundary_duration() -> String {
    "10m".into()
}

fn synthetic_financial_plan_example() -> Value {
    json!({
        "version":1,
        "name":"Synthetic wallet transfer",
        "steps":[{
            "step_id":"transfer",
            "operation":"wallet.transfer",
            "input":{"request_id":"SYNTHETIC-plan-step-1","wallet":"main-wallet","network":"evm","chain_id":1,"max_fee_eth":"0.001","token":"native","amount":"0.01","recipient":"0x0000000000000000000000000000000000000001"},
            "permitted_variations":{"mode":"exact","maximum_debits":[{"resource":"ethereum:1:0x0000000000000000000000000000000000000002","amount":{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"atoms":"10000000000000000"}}],"maximum_costs":[{"location":{"kind":"cash","resource":"ethereum:1:0x0000000000000000000000000000000000000002"},"amount":{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"atoms":"1000000000000000"},"treatment":"additional_debit"}],"possible_credits":[]},
            "completion":{"operation_succeeded":true,"principal_spend":{"resource":"ethereum:1:0x0000000000000000000000000000000000000002","amount":{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"atoms":"10000000000000000"}}},
            "funding_dependencies":[]
        }],
        "aggregate_bounds":{"maximum_debits":[{"resource":"ethereum:1:0x0000000000000000000000000000000000000002","amount":{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"atoms":"10000000000000000"}}],"maximum_costs":[{"location":{"kind":"cash","resource":"ethereum:1:0x0000000000000000000000000000000000000002"},"amount":{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"atoms":"1000000000000000"},"treatment":"additional_debit"}]}
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BoundaryExecuteInput {
    pub operation: String,
    pub input: Value,
    pub authorization: String,
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces_attempt: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanAuthorizeInput {
    /// The complete versioned plan JSON passed with `--input`.
    pub plan: FinancialPlanSpec,
    pub id: String,
    #[serde(default = "default_boundary_duration")]
    pub valid_for: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanPreviewInput {
    pub plan: FinancialPlanSpec,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanStepExecuteInput {
    pub operation: String,
    pub input: Value,
    pub plan: String,
    pub step: String,
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces_attempt: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IdInput {
    pub id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DefinitionInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
    if args.provider != Provider::CoinGecko && args.api_plan.is_some() {
        return Err(FinError::validation(
            "--api-plan belongs only to a CoinGecko market-data connection.",
        ));
    }
    match args.provider {
        Provider::CoinGecko => {
            if args.role.is_some()
                || args.parent.is_some()
                || args.email.is_some()
                || args.network.is_some()
                || args.address.is_some()
            {
                return Err(FinError::validation(
                    "CoinGecko market-data connections do not accept account relationships, wallet addresses or networks.",
                ));
            }
            if args
                .api_plan
                .as_deref()
                .is_some_and(|plan| !matches!(plan, "demo" | "pro"))
            {
                return Err(FinError::validation(
                    "Choose --api-plan demo or pro for CoinGecko.",
                ));
            }
            if args.credential_type != Some(CredentialType::ApiKey) {
                return Err(FinError::validation(
                    "CoinGecko requires --credential-type api_key; enter its value only in the protected Terminal prompt.",
                ));
            }
        }
        Provider::Binance => {
            let role = args.role.ok_or_else(|| {
                FinError::validation("Binance requires --role main or --role sub.")
            })?;
            if args.network.is_some() || args.address.is_some() {
                return Err(FinError::validation(
                    "Binance funding uses SPOT accounts; wallet network/address belong to a separate wallet connection.",
                ));
            }
            if role == BinanceRole::Sub && args.parent.is_some() != args.email.is_some() {
                return Err(FinError::validation(
                    "When supplying a Binance --parent, also supply its --email; both are optional for standalone subaccount credentials.",
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
        Provider::Evm => {
            if args.role.is_some() || args.parent.is_some() || args.email.is_some() {
                return Err(FinError::validation(
                    "EVM wallets do not accept Binance account relationships.",
                ));
            }
            if args.network.as_deref().is_some_and(|network| {
                !matches!(network, "ethereum:1" | "ethereum:42161" | "ethereum:4663")
            }) {
                return Err(FinError::validation(
                    "Choose ethereum:1, ethereum:42161 or ethereum:4663 for the optional enrollment check; wallets are reusable across networks.",
                ));
            }
            if !matches!(
                args.credential_type,
                Some(CredentialType::PrivateKey | CredentialType::ReadOnly)
            ) {
                return Err(FinError::validation(
                    "EVM wallets require --credential-type private_key or read_only.",
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
                    "The wallet address must be a nonzero 20-byte EVM address.",
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
        "scope":"Setup, policy set and connection add/update/remove only. Everyday commands use the protected background service without sudo.",
        "configuration_approval":"A separate Touch ID approval follows administrator authentication.",
        "financial_approval":"Standalone financial writes require fresh exact approval. Creating or replacing supported bounded authorizations and named plans requires fresh Touch ID. An explicitly submitted attempt under active authority does not require another prompt, but Fin rechecks current evidence, authority and protected policy before dispatch."
    });
    let enrollment = json!({
        "requires_human_terminal":true,"secret_input":"protected-hidden-terminal-prompt",
        "requires_administrator":true,"administrator_authentication":administrator_authentication.clone(),
        "secret_values_accepted_in_arguments":false,"secret_values_accepted_in_json":false,
        "human_action":"Run this command yourself in a new human-owned Terminal session. Never enter keys through an agent tool or chat.",
        "platform":"macOS protected enrollment; unavailable providers fail closed",
        "configuration_authorization":"Independent approval of the resolved connection. Saves configuration only, moves no funds, grants no future financial authority.",
        "financial_authorization":"Enrollment and daemon startup grant no financial authority. Standalone writes require fresh exact approval; supported bounded authorizations and named plans require fresh Touch ID when created or replaced. Attempts under active authority are explicitly submitted and rechecked before dispatch without another prompt.",
        "metadata_input":"Run fin connections add for guided public metadata, or fin connections add binance/ethereum to preselect the provider. The positional provider and --provider are alternatives; do not combine them. Supply complete metadata flags for explicit enrollment. Updates require flags.",
        "pem_input":"In the private human-terminal prompt, paste the complete BEGIN/END block using actual line breaks or literal \\n (also \\r\\n) separators, without surrounding quotes, then press Enter. Never supply it through arguments or chat.",
        "input_progress":"Hidden input acknowledges each received secret without echoing it. Read-only validation and Touch ID stages appear on stderr.",
        "output_format":"Concise human summary on stdout; warnings/progress on stderr. --json returns a structured HUMAN_TERMINAL_REQUIRED handoff before enrollment.",
        "command_line_examples":[
            "fin connections add binance",
            "fin connections add binance --name master --role main --credential-type ed25519",
            "fin connections add --provider binance --name main --role main --credential-type hmac",
            "fin connections add --provider binance --name main-rsa --role main --credential-type rsa",
            "fin connections add --provider binance --name main-ed25519 --role main --credential-type ed25519",
            "fin connections add --provider binance --name savings --role sub --credential-type hmac",
            "fin connections add --provider binance --name savings --role sub --parent main --email subaccount@example.invalid --credential-type hmac",
            "fin connections add --provider coingecko --name pricing --credential-type api_key --api-plan demo",
            "fin connections add --provider evm --name wallet --credential-type private_key",
            "fin connections add --provider evm --name watch --credential-type read_only --address 0x1111111111111111111111111111111111111111"
        ],
        "binance":{"wallet_type":"SPOT","default_credential_type":"hmac","supported_credential_types":["hmac","rsa","ed25519"],"mandatory_fields":["role=main|sub"],"credentials_required":["API key","HMAC secret, RSA PEM signing key, or Ed25519 private signing key"],"roles":["main","sub"],"main_rejects":["parent","email"],"subaccount_modes":{"standalone":{"parent_required":false,"email_required":false,"identity":"the authenticated subaccount's own verified UID","hierarchy_authority":"none inferred"},"parent_linked":{"parent_required":true,"email_required":true,"check":"optional parent main key verifies the supplied email association"}},"checks":["read-only authentication","reported API restrictions","account identity; optional parent mapping"],"prerequisites":["machine outbound IP allowed","main universal-transfer permission only for main-authorized internal transfers","withdrawal permission and whitelisted destination for withdrawals"],"unknown_permissions":"Explicitly reported; enrollment performs no trial transfer or withdrawal. A main key is needed only for explicitly parent-authorized hierarchy operations, not for standalone subaccount reads or enrollment."},
        "coingecko":{"provider":"coingecko","resource":"market-data:coingecko","api_plans":["demo","pro"],"default_api_plan":"demo","credential_type":"api_key","credentials_required":["CoinGecko API key"],"scope":"Market-data requests only; this key grants no exchange or wallet access. Requests can consume the selected plan quota/billing.","checks":["read-only authenticated CoinGecko ping","fresh ETH/USD quote"],"authentication":"Optional protected human enrollment. Without it, anonymous public pricing remains best effort; missing/stale/rate-limited quotes block writes."},
        "evm":{"provider":"evm","legacy_provider_alias":"ethereum","wallet_scope":"One address and signing key reusable across reviewed EVM networks; choose a network for each operation.","network_argument":"Optional read-only enrollment check; defaults to ethereum:1 and does not scope the wallet key.","networks":["ethereum:1","ethereum:42161","ethereum:4663"],"network_profiles":evm_network_profiles(),"credential_types":["private_key","read_only"],"checks":["public RPC chain identity","derived signing address matches resolved wallet","reported native balance"],"dapp_scope":"Uniswap v3/v4 availability follows reviewed per-network deployment configuration and exact route liquidity. Aave remains Ethereum mainnet only. Tokens are supplied by exact contract identity."},
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
        definition::<DefinitionInput>(
            "explain",
            "Explain a bundled command or financial operation; omit name for usage and examples.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({"name":"setup"}),
            json!({"service_required":false,"secret_input":"none","financial_movement":false,"without_name":"Help and examples; machine output is a help object, as with --help.","catalog_command":"fin capabilities"}),
        ),
        definition::<DefinitionInput>(
            "schema",
            "Show bundled input, output, error and actor metadata; omit name for usage and examples.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({"name":"connections.add"}),
            json!({"service_required":false,"secret_input":"none","financial_movement":false,"without_name":"Help and examples; machine output is a help object, as with --help.","catalog_command":"fin capabilities"}),
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
            "Execute one approved operation under a stable request identity, or preview it with --dry-run.",
            OperationKind::Write,
            ExecutionActor::AgentInvocable,
            json!({"operation":"binance.transfer","input":{"requestId":"synthetic-venue-request-001","authority":"main","from":"main","to":"savings","coin":"USDT","amount":"1","fromWallet":"SPOT","toWallet":"SPOT"},"request_id":"synthetic-logical-write-001"}),
            json!({
                "input_validation":"operation-specific bundled schema",
                "service_required":true,
                "fresh_approval":"broker enforced per write",
                "retry":"reuse the same --id for this logical request; reconcile uncertain submissions before starting a new action",
                "command_line_template":"fin execute OPERATION --input JSON --id REQUEST_ID --json",
                "command_line_example":"fin execute binance.transfer --input '{\"requestId\":\"synthetic-venue-request-001\",\"authority\":\"main\",\"from\":\"main\",\"to\":\"savings\",\"coin\":\"USDT\",\"amount\":\"1\",\"fromWallet\":\"SPOT\",\"toWallet\":\"SPOT\"}' --id synthetic-logical-write-001 --json",
                "arguments":["OPERATION","--input JSON","--id REQUEST_ID","--dry-run","--json"],
                "cli_field_mapping":{"request_id":"--id","dry_run":"--dry-run"},
                "allocation_scope":"Installation-wide rolling 24-hour gross movement, maximum costs and additional borrowed allocation; unresolved submissions continue counting beyond the window. No run, return plan or restoration required.",
                "dry_run":{"flag":"--dry-run","command_line_template":"fin execute OPERATION --input JSON --dry-run --json","equivalent_to":"fin preview OPERATION --input JSON --json","financial_movement":false,"financial_authorization":false,"journal_write":false,"identity":"--id is optional and not consumed; operation-specific input identities remain required where its schema requires them","allocation_scope":"Previews do not reserve or validate the rolling allocation budget","result":"The same preview result/errors as fin preview; checks and simulations depend on the adapter","execution":"Remove --dry-run and supply --id to execute. Standalone exact writes require fresh approval; attempts selected under an active boundary grant or named plan use that authority without another prompt. The broker rechecks current evidence, authority and protected policy before every dispatch."},
                "venue_request_identity":"A venue request identifier inside input JSON is separate from the logical --id value."
            }),
        ),
        definition::<BoundaryAuthorizationInput>(
            "authorizations.authorize",
            "Grant time-limited cumulative authority for a supported financial intent after fresh human approval.",
            OperationKind::Write,
            ExecutionActor::AgentInvocable,
            json!({"operation":"binance.spot-order","id":"synthetic-intent-001","input":{"side":"SELL","symbol":"BTCUSDT","quantity":"0.01"},"bounds":{"target_quantity":"0.01","max_quantity":"0.01","min_price":"60000","max_fees":{"USDT":"1"}},"valid_for":"10m"}),
            json!({
                "command_line_template":"fin authorizations authorize OPERATION --id INTENT_ID --input JSON --bounds JSON [--valid-for 10m] [--replaces AUTHORIZATION_ID]",
                "requires_fresh_human_approval":true,
                "approval_popup":"The protected broker requests fresh macOS Touch ID and displays the operation, permitted variations, cumulative limits and consent duration.",
                "human_terminal_review":true,
                "machine_output":"Structured authorization result; no terminal prompts, animation, color or chatter. Required macOS Touch ID approval still applies.",
                "default_valid_for_seconds":600,
                "valid_for":"Starts after successful approval; separate from biometric challenge timeout and quote freshness.",
                "identity":"--id is the stable logical intent identity; replacing authority retains the intent and achieved effects.",
                "bounds":"Operation-specific fixed identity fields, allowed candidate variations, cumulative debit and fee caps; inspect the operation definition for actual supported coverage.",
                "replacement":"Changing bounds or expiry requires fresh approval and --replaces; remaining authority is preserved, spent effects cannot be reset.",
                "administrative_operations":"Financial authorizations cannot authorize connection management or policy changes."
            }),
        ),
        definition::<BoundaryExecuteInput>(
            "execute.boundary",
            "Evaluate or execute one candidate under existing time-limited cumulative authorization.",
            OperationKind::Write,
            ExecutionActor::AgentInvocable,
            json!({"operation":"binance.spot-order","input":{"side":"SELL","symbol":"BTCUSDT","quantity":"0.01"},"authorization":"opaque-authorization-id","id":"synthetic-attempt-001"}),
            json!({
                "command_line_template":"fin execute OPERATION --input JSON --id ATTEMPT_ID --authorization AUTHORIZATION_ID [--dry-run]",
                "dry_run":"With --dry-run, evaluates fresh evidence and remaining authority without submission or consumption; --id is optional and unused.",
                "execution":"--id identifies one exact attempt; duplicate retries recover that attempt. No automatic resubmission occurs.",
                "approval":"Uses the existing authorization. No second Touch ID challenge is requested for an in-bounds candidate.",
                "fresh_evidence":"Pricing and venue evidence retain their independent freshness rules; refreshing evidence does not renew consent.",
                "recovery":"On response loss, reconcile with fin authorizations status AUTHORIZATION_ID before creating another attempt.",
                "candidate_replacement":"Pass --replaces-attempt OLD_OPERATION_UUID with --authorization to atomically cancel and replace only an unsubmitted active candidate. Submitted or uncertain attempts cannot be replaced. This does not change authorization bounds or expiry; use authorizations authorize --replaces for fresh approval when those change. With --dry-run it only previews and does not replace.",
                "out_of_bounds":"Structured violation identifies the bound, approved value, candidate value and whether replacement approval or reconciliation is needed."
            }),
        ),
        definition::<IdInput>(
            "authorizations.status",
            "Read and reconcile durable progress, remaining authority and attempts for one bounded authorization.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({"id":"opaque-authorization-id"}),
            json!({"command_line_example":"fin authorizations status opaque-authorization-id --json","reconciliation":"Observes attempts and never creates a new financial action.","expiry":"Expiry or revocation blocks future dispatch but allows observation and reconciliation."}),
        ),
        definition::<IdInput>(
            "authorizations.cancel",
            "Revoke future dispatch under one bounded authorization without Touch ID.",
            OperationKind::Configuration,
            ExecutionActor::AgentInvocable,
            json!({"id":"opaque-authorization-id"}),
            json!({"command_line_example":"fin authorizations cancel opaque-authorization-id --json","touch_id":false,"submitted_attempts":"Cancellation does not cancel an already submitted venue order; reconcile it with authorizations status."}),
        ),
        definition::<FinancialPlanSpec>(
            "plans.preview",
            "Regenerate a read-only review of exact-input scopes for registered financial and read operations.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            synthetic_financial_plan_example(),
            json!({"command_line_template":"fin plans preview --input PLAN_JSON [--valid-for 10m] [--replaces REVISION_ID] [--json]","read_only":true,"returns":"Canonical review_hash, ordered exact operation inputs, bound schema and protected connection identities, cumulative debit/fee caps, conditional funding and completion targets. Authorization does not validate live balances, quotes or simulations; every concrete attempt performs fresh adapter checks. No reservation or approval is created.","exact_scope_schema":schema::<fin_domain::financial_plans::FinancialPlanExactScope>(),"scope_selection":"Set step.permitted_variations.mode to exact and declare maximum_debits, maximum_costs, and possible_credits. Inputs are fixed except JSON pointers explicitly declared by the trusted operation definition as broker_generated_attempt_input_fields; Fin assigns each such string per attempt and binds the template. Every step is freshly prepared on submission."}),
        ),
        definition::<FinancialPlanSpec>(
            "plans.authorize",
            "Create or replace one immutable named plan revision after fresh human approval.",
            OperationKind::Write,
            ExecutionActor::AgentInvocable,
            synthetic_financial_plan_example(),
            json!({"command_line_template":"fin plans authorize --input PLAN_JSON --id REQUEST_ID [--valid-for 10m] [--replaces REVISION_ID] [--review-hash HASH] [--json]","requires_fresh_touch_id":true,"review":"Terminal displays the complete named revision before approval. Trusted broker_generated_attempt_input_fields are shown as Fin-assigned per attempt; the original input template remains bound. Generic exact-input scopes bind the operation schema and protected connection identities but defer live balance, market, fee and simulation checks to each concrete attempt. An explicit --review-hash must match freshly generated review material before Touch ID.","exact_scope_schema":schema::<fin_domain::financial_plans::FinancialPlanExactScope>(),"idempotency":"Reuse --id after response loss to recover the original request; review_hash is excluded from request identity.","duration":"Starts after successful Touch ID; default 600 seconds and independent from native challenge expiry.","completion":"Verified operation success is required; optional spend and net-receipt targets are also enforced.","revision":"A replacement preserves started/completed step identity, order, fixed scope, targets and carried-forward usage; only the unstarted suffix can change."}),
        ),
        definition::<PlanStepExecuteInput>(
            "execute.plan-step",
            "Preview or explicitly submit one eligible step under the active named plan revision.",
            OperationKind::Write,
            ExecutionActor::AgentInvocable,
            json!({"operation":"aave.withdraw","input":{"request_id":"SYNTHETIC-plan-attempt-1","wallet":"main-wallet","network":"evm","chain_id":1,"max_fee_eth":"0.001","token":"0x0000000000000000000000000000000000000010","amount":"all","max_amount":"1","recipient":"0x0000000000000000000000000000000000000001"},"plan":"opaque-plan-id","step":"withdraw","id":"synthetic-attempt-001"}),
            json!({"command_line_template":"fin execute OPERATION --input INPUT_JSON --id ATTEMPT_ID --plan PLAN_ID --step STEP_ID [--dry-run] [--replaces-attempt ATTEMPT_ID]","selectors":"--plan and --authorization are mutually exclusive. --step requires --plan.","execution":"Only the first incomplete step can run. Every candidate refreshes balances, prices, fees, market/protocol metadata, protected policy and applicable simulations.","funding":"A dependent step needs verified net proceeds from its declared producing step and current available balance.","retry":"Duplicate IDs recover their original attempt. Uncertain submissions require fin plans status PLAN_ID reconciliation before another attempt.","replacement":"Only explicitly named unsubmitted candidates can be replaced; submitted or uncertain attempts cannot be replaced."}),
        ),
        definition::<IdInput>(
            "plans.status",
            "Read authorization state and sequential execution progress; reconciliation never executes a step.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({"id":"opaque-plan-id"}),
            json!({"command_line_example":"fin plans status opaque-plan-id --json","returns":"Authorization state, revision, attempts, reservations, verified consumption and proceeds, remaining authority, next eligible step and blockers."}),
        ),
        definition::<IdInput>(
            "plans.cancel",
            "Revoke future dispatch under a named plan without Touch ID.",
            OperationKind::Configuration,
            ExecutionActor::AgentInvocable,
            json!({"id":"opaque-plan-id"}),
            json!({"command_line_example":"fin plans cancel opaque-plan-id --json","touch_id":false,"submitted_attempts":"Revocation does not cancel venue orders already submitted; status can continue reconciliation."}),
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
        definition::<EmptyArgs>(
            "setup",
            "Install the bundled protected broker and native approval helper, then start and check the service.",
            OperationKind::Configuration,
            ExecutionActor::HumanInteractive,
            json!({}),
            json!({"requires_human_terminal":true,"requires_administrator":true,"administrator_authentication":administrator_authentication.clone(),"financial_movement":false,"secret_input":"none","shell_completion":{"automatic":true,"selection":"configured human account login shell and home from account database","supported":["zsh","bash","fish"],"activation":"new shell","unsupported":"brief notice; setup continues","cleanup":"Fin-owned files and exact marked blocks only; preserve user edits"},"financial_limits":{"input":"protected human terminal prompts before setup Touch ID","default_usd":"100","zero":"no personal limit for that field","later_change":"fin policy set"},"installs":["one public fin CLI","protected internal broker","bundled native OS authorization helper","private credential/configuration/journal storage","macOS PATH registration in /etc/paths.d/fin"],"command_availability":"Open a new macOS Terminal after setup to use fin. Existing shells can run: export PATH=\"/Library/Fin/runtime:$PATH\". Custom PATH overrides must retain this directory.","boundary":"Installation authorizes no financial action. Run sudo -K to clear all cached sudo credentials, close elevated shells, and verify the agent lacks root access. This does not change Fin's per-write Touch ID requirement.","next_check":"fin daemon status --json"}),
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
            "policy.show",
            "Show human-owned financial limits, the unpriced exchange setting, included costs and durable allocation scope.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({}),
            json!({"financial_movement":false,"command_line_examples":["fin policy show", "fin policy show --json"]}),
        ),
        definition::<PolicySetArgs>(
            "policy.set",
            "Atomically change financial limits and an optional unpriced exchange exception after protected human review and fresh Touch ID.",
            OperationKind::Configuration,
            ExecutionActor::HumanInteractive,
            json!({"max_movement_usd":"1000","max_run_allocation_usd":"5000"}),
            json!({"requires_human_terminal":true,"requires_administrator":true,"requires_fresh_touch_id":true,"financial_movement":false,"future_financial_authority":false,"secret_input":"none","unpriced_exchange_exception":{"field":"allow_unpriced_exchange_assets","default":false,"omission":"Preserves current policy","authorization":"Protected human Terminal review and fresh Touch ID","warning":fin_domain::unpriced::WARNING},"amounts":"Nonnegative exact USD decimals; 0 means no personal limit, at most six fractional digits, bounded by 18446744073709.551615 USD; no floating-point conversion.","pending_work":"Refuse every nonterminal operation; historical unrestored runs do not block changes. No reservation changes or replays.","reload":"Existing configuration-change detection stops the stale broker; launchd reloads on the next request. Never retry a financial write automatically.","command_line_examples":["fin policy set --max-movement-usd 1000 --max-run-allocation-usd 5000", "fin policy set --max-movement-usd 1000 --max-run-allocation-usd 5000 --allow-unpriced-exchange-assets true"],"next_check":"fin policy show --json"}),
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
    definitions.extend(companion_skill::definitions());
    let mut connection_schema = schema::<AddConnectionArgs>();
    connection_schema["$defs"]["Provider"]["enum"] =
        json!(["binance", "evm", "ethereum", "coingecko"]);
    connection_schema["allOf"] = json!([
        {
            "if": {"properties":{"provider":{"const":"binance"}},"required":["provider"]},
            "then": {
                "required":["role"],
                "not":{"required":["api_plan"]},
                "properties":{"credential_type":{"enum":["hmac","rsa","ed25519"]}},
                "oneOf":[
                    {"properties":{"role":{"const":"main"}},"not":{"anyOf":[{"required":["parent"]},{"required":["email"]}]}},
                    {"properties":{"role":{"const":"sub"}},"not":{"anyOf":[{"required":["parent"]},{"required":["email"]}]}},
                    {"properties":{"role":{"const":"sub"}},"required":["parent","email"]}
                ]
            }
        },
        {
            "if": {"properties":{"provider":{"enum":["evm","ethereum"]}},"required":["provider"]},
            "then": {
                "required":["credential_type"],
                "properties":{"network":{"enum":["ethereum:1","ethereum:42161","ethereum:4663",null]},"credential_type":{"enum":["private_key","read_only"]}},
                "not":{"anyOf":[{"required":["role"]},{"required":["parent"]},{"required":["email"]},{"required":["api_plan"]}]},
                "oneOf":[
                    {"properties":{"credential_type":{"const":"private_key"}}},
                    {"properties":{"credential_type":{"const":"read_only"}},"required":["address"]}
                ]
            }
        },
        {
            "if":{"properties":{"provider":{"const":"coingecko"}},"required":["provider"]},
            "then":{
                "required":["credential_type"],
                "properties":{"credential_type":{"const":"api_key"},"api_plan":{"enum":["demo","pro",null],"default":"demo"}},
                "not":{"anyOf":[{"required":["role"]},{"required":["parent"]},{"required":["email"]},{"required":["network"]},{"required":["address"]}]}
            }
        }
    ]);
    for item in &mut definitions {
        item.semantics["machine_contract"] = machine_contract();
        if item.name == "execute" {
            item.input_schema["allOf"] = json!([{
                "if":{"required":["dry_run"],"properties":{"dry_run":{"const":true}}},
                "then":{},
                "else":{"required":["request_id"],"properties":{"request_id":{"type":"string","minLength":1}}}
            }]);
            let mut dry_run = item.examples[0].clone();
            dry_run["dry_run"] = json!(true);
            item.examples.push(dry_run);
            item.semantics["command_line_examples"] = json!([
                item.semantics["command_line_example"],
                format!(
                    "{} --dry-run",
                    item.semantics["command_line_example"]
                        .as_str()
                        .unwrap_or_default()
                )
            ]);
        }
        if item.name == "setup" {
            item.semantics["optional_companion_skill"] = companion_skill::metadata();
            item.semantics["companion_skill_offer"] = json!({
                "prompt":companion_skill::SETUP_PROMPT,
                "default":"no",
                "when":"After successful human setup; never in JSON mode.",
                "installation":"Use arrow keys to move and Space to toggle Codex and Claude Code; Enter installs selected copies as the human account. Nothing is preselected. Enter with none selected, Esc or EOF installs nothing; conflicts are preserved without automatic backup or replacement.",
                "failure":"Optional skill failure or cancellation leaves the completed Fin installation intact."
            });
        }
        if item.name == "capabilities" {
            item.semantics["companion_skill"] = companion_skill::metadata();
        }
        if matches!(item.name.as_str(), "policy.show" | "policy.set") {
            item.output_schema = schema::<PolicyView>();
        } else if matches!(item.name.as_str(), "connections.add" | "connections.update") {
            item.input_schema = connection_schema.clone();
            item.output_schema = schema::<enrollment_contract::EnrollmentOutcome>();
            item.semantics["financial_movement"] = json!(false);
            item.semantics["future_financial_authority"] = json!(false);
        } else if item.name == "connections.remove" {
            item.output_schema = schema::<enrollment_contract::EnrollmentOutcome>();
            item.semantics["financial_movement"] = json!(false);
            item.semantics["future_financial_authority"] = json!(false);
        } else if item.name == "plans.preview" {
            item.output_schema = schema::<fin_domain::financial_plans::FinancialPlanReview>();
        } else if matches!(
            item.name.as_str(),
            "plans.authorize" | "plans.status" | "plans.cancel"
        ) {
            item.output_schema = schema::<fin_domain::financial_plans::FinancialPlanStatus>();
        } else if item.name == "execute.plan-step" {
            item.output_schema = schema::<fin_domain::OperationRecord>();
        }
    }
    definitions
}

/// Library reach is deliberately separate from Fin's reviewed named operations.
pub fn exchange_capabilities() -> serde_json::Value {
    let mut value = serde_json::json!({
        "library": {"name":"ccxt-base", "version":"4.5.85", "source":"https://github.com/ccxt/ccxt", "compiled_exchanges":["binance","kraken","hyperliquid","okx","coinbase","bybit"], "all_library_methods_exposed":false},
        "fin": {"binance": {"scope":"SPOT funding, account discovery, trading reads and bounded SPOT/USDT perpetual orders", "authentication":["hmac","rsa","ed25519"], "market_data":{"operations":["exchange.markets","exchange.ticker","exchange.order-book"],"market_types":["spot","perpetual"],"maximum_depth":100}, "spot":{"execution":"LIMIT IOC/FOK orders; named plans bind exact input, schema and protected connection identity", "ioc_partial_fills":"Each attempt settles through the verified operation journal; plan completion requires its declared cumulative targets"}, "futures":{"execution":"USD-M USDT perpetual LIMIT IOC/FOK", "margin":"isolated, leverage 1, one-way, automatic additions disabled", "configuration_changes":false,"concurrent_positions_per_account":1}, "cancellation":"New approval for the exact original unresolved Fin order; original reservation persists through fill reconciliation", "deposit_address_creation":false, "withdrawal_whitelist_editing":false,
            "address_management":{"reads":["binance.deposit-address","binance.deposit-addresses","binance.whitelist"],"mutations":"No documented public Binance endpoint for new-address creation or whitelist add/remove; human Binance website/app action required.","deposit_creation":{"human_action":"Use Binance Deposit Crypto, select the coin/network and claim a new address when available.","recheck":"fin read binance.deposit-addresses --input JSON --json"},"whitelist_editing":{"human_action":"Use Binance withdrawal Address Management to add/remove or change whitelist entries; complete Binance security checks.","recheck":"fin read binance.whitelist --input JSON --json"},"whitelist_limits":"Per-address flags only; account-wide enforcement and activation/cooldown are not proved."}, "operations":"See the explicit binance.* entries in operations; other CCXT methods are unavailable."}},
        "verification": {"automated":"synthetic transport and lifecycle fixtures", "authenticated_live":"not_run_for_this_refactor", "human_onboarding":"pending", "touch_id":"pending"}
    });
    value["fin"]["binance"]["deposits"] = serde_json::json!({
        "operation": "binance.deposit",
        "accounts": "main or subaccount, using the receiving account's own credential; master enrollment is optional",
        "discovery": "binance.networks and binance.deposit-address resolve current coin/network availability, minimum and exact contract",
        "source": "native ETH and standard ERC20 transfers on configured Fin EVM networks with a reviewed Binance network mapping",
        "unsupported": "other source chains, memo/tag routes and non-unit denominations; Binance metadata availability alone does not establish Fin source-chain execution",
        "approval": "fresh exact approval binds wallet, destination account, chain, asset, address, amount and fee cap",
        "completion": "canonical chain evidence and matching usable Binance deposit credit with balance evidence; delayed credit retains the original submission",
        "broker_generated_attempt_input_fields": ["/requestId"],
        "request_id_assignment": "The required requestId remains in the reviewed plan template, but Fin assigns a stable unique venue requestId for each concrete attempt and shows this field as Fin-assigned in human review."
    });
    for (venue, scope) in [
        ("kraken", "spot"),
        ("hyperliquid", "perpetuals; primary dex"),
        ("okx", "spot"),
        (
            "coinbase",
            "Coinbase Advanced public products; market discovery capped at 1000 spot products",
        ),
        ("bybit", "spot"),
    ] {
        let market_type = if venue == "hyperliquid" {
            "perpetual"
        } else {
            "spot"
        };
        value["fin"][venue] = serde_json::json!({"operations":["exchange.markets","exchange.ticker","exchange.order-book"],"market_types":[market_type],"maximum_order_book_depth":100,"scope":scope,"public_only":true,"account_enrollment":false,"writes":false});
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_settlement_observer_is_disclosed_without_wallet_authority() {
        let wallet_chains = evm_network_profiles()
            .as_array()
            .unwrap()
            .iter()
            .map(|network| network["chain_id"].as_u64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(wallet_chains, [1, 42161, 4663]);
        let bridge = bridge_profile();
        let providers = bridge["providers"].as_array().unwrap();
        assert_eq!(providers[0]["id"], "relay");
        assert_eq!(providers[1]["id"], "arbitrum_canonical");
        assert!(
            providers[1]["coverage"]
                .as_str()
                .unwrap()
                .contains("Native ETH")
        );
        let observers = bridge["settlement_observers"].as_array().unwrap();
        assert_eq!(observers.len(), 1);
        assert_eq!(observers[0]["chain_id"], 537713);
        assert_eq!(observers[0]["wallet_authority"], false);
        assert!(!wallet_chains.contains(&537713));
        let solvers = bridge["solver_observers"].as_array().unwrap();
        assert_eq!(solvers.len(), 1);
        assert_eq!(solvers[0]["chain_id"], 8453);
        assert_eq!(solvers[0]["wallet_authority"], false);
        assert!(!wallet_chains.contains(&8453));
        assert!(bridge["input_schemas"]["bridge.transfer"]["properties"]["bounds"].is_object());
    }
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
            3
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
        let metadata = definition.semantics["metadata_input"].as_str().unwrap();
        assert!(metadata.contains("fin connections add binance/ethereum"));
        assert!(metadata.contains("--provider are alternatives"));
        assert!(metadata.contains("Updates require flags."));
        assert_eq!(
            definition.semantics["binance"]["supported_credential_types"],
            json!(["hmac", "rsa", "ed25519"])
        );
        assert_eq!(
            definition.input_schema["allOf"][0]["then"]["oneOf"]
                .as_array()
                .unwrap()
                .len(),
            3,
            "main, standalone subaccount, and parent-linked subaccount are distinct schema cases"
        );
        assert_eq!(
            definition.semantics["binance"]["subaccount_modes"]["standalone"]["hierarchy_authority"],
            "none inferred"
        );
    }

    #[test]
    fn named_plan_discovery_uses_supported_complete_example_and_typed_outputs() {
        let definitions = operation_definitions();
        let example = synthetic_financial_plan_example();
        let plan: FinancialPlanSpec = serde_json::from_value(example).unwrap();
        plan.validate().unwrap();
        assert_eq!(plan.steps[0].operation, "wallet.transfer");
        assert_eq!(plan.steps[0].permitted_variations["mode"], "exact");
        let preview = definitions
            .iter()
            .find(|definition| definition.name == "plans.preview")
            .unwrap();
        assert_eq!(
            preview.output_schema,
            schema::<fin_domain::financial_plans::FinancialPlanReview>()
        );
        assert!(
            preview.semantics["command_line_template"]
                .as_str()
                .unwrap()
                .contains("--valid-for")
        );
        let authorize = definitions
            .iter()
            .find(|definition| definition.name == "plans.authorize")
            .unwrap();
        assert_eq!(
            authorize.output_schema,
            schema::<fin_domain::financial_plans::FinancialPlanStatus>()
        );
        assert!(
            authorize.semantics["command_line_template"]
                .as_str()
                .unwrap()
                .contains("--review-hash")
        );
        let execute = definitions
            .iter()
            .find(|definition| definition.name == "execute.plan-step")
            .unwrap();
        assert!(
            execute.semantics["command_line_template"]
                .as_str()
                .unwrap()
                .contains("--plan PLAN_ID --step STEP_ID")
        );
        let machine = machine_contract();
        assert_eq!(
            machine["named_financial_plans"]["authority"],
            "one plan revision authority; attempts retain their own IDs; no child grants"
        );
        assert!(
            preview.semantics["returns"]
                .as_str()
                .unwrap()
                .contains("every concrete attempt performs fresh adapter checks")
        );
        for field in [
            "mode",
            "maximum_debits",
            "maximum_costs",
            "possible_credits",
        ] {
            assert!(
                preview.semantics["exact_scope_schema"]["properties"][field].is_object(),
                "exact plan scope schema must expose {field}"
            );
        }
        assert!(
            authorize.semantics["review"]
                .as_str()
                .unwrap()
                .contains("Fin-assigned per attempt")
        );
        let deposit = exchange_capabilities()["fin"]["binance"]["deposits"].clone();
        assert_eq!(
            deposit["broker_generated_attempt_input_fields"],
            json!(["/requestId"])
        );
        assert!(
            deposit["request_id_assignment"]
                .as_str()
                .unwrap()
                .contains("stable unique venue requestId")
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
        let mut found_standalone_subaccount = false;
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
                if parsed.add.parent.is_some() {
                    assert_eq!(parsed.add.parent.as_deref(), Some("main"));
                    assert_eq!(
                        parsed.add.email.as_deref(),
                        Some("subaccount@example.invalid")
                    );
                } else {
                    assert!(parsed.add.email.is_none());
                    found_standalone_subaccount = true;
                }
                found_subaccount = true;
            } else {
                assert_eq!(parsed.add.role, Some(BinanceRole::Main));
                assert!(parsed.add.parent.is_none());
                assert!(parsed.add.email.is_none());
            }
        }
        assert!(found_subaccount);
        assert!(found_standalone_subaccount);
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
        validate_add(&args).unwrap();
        args.email = Some("synthetic@example.invalid".into());
        assert!(validate_add(&args).is_err());
        args.email = None;
        args.parent = Some("main".into());
        assert!(validate_add(&args).is_err());
        args.email = Some("synthetic@example.invalid".into());
        validate_add(&args).unwrap();
        args.provider = Provider::Evm;
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
    fn evm_provider_aliases_and_optional_network_share_one_wallet() {
        use clap::Parser;
        for provider in ["evm", "ethereum"] {
            let parsed = Cli::try_parse_from([
                "fin",
                "--provider",
                provider,
                "--name",
                "wallet",
                "--credential-type",
                "private_key",
            ])
            .unwrap();
            assert_eq!(parsed.add.provider, Provider::Evm);
            assert!(parsed.add.network.is_none());
            validate_add(&parsed.add).unwrap();
            let mut args: AddConnectionArgs = serde_json::from_value(json!({
                "provider": provider, "name":"wallet", "credential_type":"private_key"
            }))
            .unwrap();
            assert_eq!(serde_json::to_value(&args).unwrap()["provider"], "evm");
            for network in ["ethereum:1", "ethereum:42161", "ethereum:4663"] {
                args.network = Some(network.into());
                validate_add(&args).unwrap();
            }
            args.network = Some("ethereum:9999".into());
            assert!(validate_add(&args).is_err());
        }
    }

    #[test]
    fn evm_discovery_schema_and_examples_describe_wallet_reuse_and_network_scope() {
        use clap::Parser;
        let add = operation_definitions()
            .into_iter()
            .find(|definition| definition.name == "connections.add")
            .unwrap();
        assert_eq!(
            add.input_schema["$defs"]["Provider"]["enum"],
            json!(["binance", "evm", "ethereum", "coingecko"])
        );
        assert_eq!(
            add.input_schema["allOf"][1]["then"]["required"],
            json!(["credential_type"])
        );
        let evm = &add.semantics["evm"];
        assert_eq!(
            evm["networks"],
            json!(["ethereum:1", "ethereum:42161", "ethereum:4663"])
        );
        assert_eq!(evm["network_profiles"][1]["name"], "Arbitrum One");
        assert_eq!(evm["network_profiles"][2]["name"], "Robinhood Chain");
        assert_eq!(
            evm["network_profiles"][1]["operations"],
            json!(["wallet", "Uniswap"])
        );
        assert_eq!(
            evm["network_profiles"][2]["operations"],
            json!(["wallet", "Uniswap"])
        );
        assert!(evm["wallet_scope"].as_str().unwrap().contains("reusable"));
        for example in add.semantics["command_line_examples"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .filter(|example| example.contains("--provider evm"))
        {
            let args = example
                .strip_prefix("fin connections add ")
                .unwrap()
                .split_whitespace();
            let parsed = Cli::try_parse_from(std::iter::once("fin").chain(args)).unwrap();
            validate_add(&parsed.add).unwrap();
            assert!(parsed.add.network.is_none());
        }
    }

    #[test]
    fn coingecko_accepts_only_safe_market_data_metadata_and_supported_plan() {
        use clap::Parser;
        for plan in [None, Some("demo"), Some("pro")] {
            let mut input =
                json!({"provider":"coingecko","name":"pricing","credential_type":"api_key"});
            if let Some(plan) = plan {
                input["api_plan"] = json!(plan);
            }
            let args: AddConnectionArgs = serde_json::from_value(input.clone()).unwrap();
            assert_eq!(args.provider, Provider::CoinGecko);
            validate_add(&args).unwrap();
            assert_eq!(
                serde_json::to_value(&args).unwrap()["provider"],
                "coingecko"
            );
            for (field, value) in [
                ("role", json!("main")),
                ("parent", json!("main")),
                ("email", json!("synthetic@example.invalid")),
                ("network", json!("ethereum:1")),
                (
                    "address",
                    json!("0x1111111111111111111111111111111111111111"),
                ),
                ("api_plan", json!("unsupported")),
                ("credential_type", json!("private_key")),
            ] {
                let mut wrong = input.clone();
                wrong[field] = value;
                let wrong: AddConnectionArgs = serde_json::from_value(wrong).unwrap();
                assert!(validate_add(&wrong).is_err(), "accepted {field}");
            }
            for field in ["api_key", "secret", "credential_ref"] {
                let mut wrong = input.clone();
                wrong[field] = json!("SYNTHETIC-NOT-A-REAL-KEY");
                assert!(serde_json::from_value::<AddConnectionArgs>(wrong).is_err());
            }
        }
        assert!(
            Cli::try_parse_from([
                "fin",
                "--provider",
                "coingecko",
                "--name",
                "pricing",
                "--credential-type",
                "api_key",
                "--api-plan",
                "pro"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "fin",
                "--provider",
                "coingecko",
                "--name",
                "pricing",
                "--credential-type",
                "api_key",
                "--api-plan",
                "unsupported"
            ])
            .is_err()
        );
        let mut binance = main_args();
        binance.api_plan = Some("demo".into());
        assert!(validate_add(&binance).is_err());
        let evm: AddConnectionArgs = serde_json::from_value(json!({"provider":"evm","name":"wallet","credential_type":"private_key","api_plan":"demo"})).unwrap();
        assert!(validate_add(&evm).is_err());
    }

    #[test]
    fn coingecko_discovery_and_enrollment_example_expose_plan_not_key() {
        use clap::Parser;
        let add = operation_definitions()
            .into_iter()
            .find(|d| d.name == "connections.add")
            .unwrap();
        let provider = &add.semantics["coingecko"];
        assert_eq!(provider["resource"], "market-data:coingecko");
        assert_eq!(provider["default_api_plan"], "demo");
        assert_eq!(provider["api_plans"], json!(["demo", "pro"]));
        assert_eq!(
            provider["checks"],
            json!([
                "read-only authenticated CoinGecko ping",
                "fresh ETH/USD quote"
            ])
        );
        assert_eq!(
            add.input_schema["allOf"][2]["then"]["properties"]["credential_type"]["const"],
            "api_key"
        );
        for example in add.semantics["command_line_examples"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .filter(|example| example.contains("--provider coingecko"))
        {
            let parsed = Cli::try_parse_from(
                std::iter::once("fin").chain(
                    example
                        .strip_prefix("fin connections add ")
                        .unwrap()
                        .split_whitespace(),
                ),
            )
            .unwrap();
            validate_add(&parsed.add).unwrap();
            assert_eq!(parsed.add.api_plan.as_deref(), Some("demo"));
        }
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
