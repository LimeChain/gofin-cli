//! Shared safe CLI arguments, validation, and offline onboarding discovery.
//! Enrollment DTOs were drafted by Ogi Spark/GLM and reviewed against the CLI contract.
use clap::{Args, ValueEnum};
use gofin_domain::financial_plans::FinancialPlanSpec;
use gofin_domain::{Definition, ExecutionActor, GoFinError, OperationKind, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub mod companion_skill;
pub mod enrollment_contract;
pub mod policy;
pub use policy::{PolicySetArgs, PolicyView};

/// Shared human-facing explanation for macOS administrator authentication.
pub const MACOS_ADMIN_PASSWORD_PROMPT: &str = "Mac login password for %p: ";
pub const MACOS_ADMIN_PASSWORD_HELP: &str = "macOS may ask for your Mac login password (typing is hidden). Protected terminal confirmation follows. Ctrl-C cancels.";

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
    Hyperliquid,
    Kraken,
    #[serde(rename = "krakenfutures")]
    #[value(name = "krakenfutures")]
    KrakenFutures,
    Solana,
    Jupiter,
    Relay,
    Coinbase,
    #[serde(rename = "coinbaseexchange")]
    #[value(name = "coinbaseexchange")]
    CoinbaseExchange,
    #[serde(rename = "coinbaseinternational")]
    #[value(name = "coinbaseinternational")]
    CoinbaseInternational,
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
    Ecdsa,
    PrivateKey,
    ReadOnly,
    ApiKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum HyperliquidScopeKind {
    Owner,
    Subaccount,
    Vault,
}

/// Wallet key family supported by protected wallet generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum WalletNetwork {
    Evm,
    Solana,
}

#[derive(Args, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GenerateConnectionArgs {
    /// Wallet key family to generate inside the protected broker.
    #[arg(value_enum)]
    pub network: WalletNetwork,
    /// Stable local name matching [a-z][a-z0-9_-]{0,63}.
    #[arg(long)]
    pub name: String,
}

#[derive(Args, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddConnectionArgs {
    /// Public Kraken account identifier, confirmed by the human during enrollment.
    #[arg(long)]
    pub account_id: Option<String>,
    /// Exact Coinbase permissioned portfolio/account identity selected during enrollment.
    #[arg(long)]
    pub portfolio_id: Option<String>,
    /// Exact Kraken Spot default Funding wallet ID selected by the human; ownership is checked.
    #[arg(long)]
    pub default_wallet_account_id: Option<String>,
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
    /// Optional enrollment read-check network from GoFin's reviewed EVM registry.
    /// The EVM wallet remains reusable across all reviewed networks.
    #[arg(long)]
    pub network: Option<String>,
    /// Expected public wallet address; checked against the protected signer.
    #[arg(long)]
    pub address: Option<String>,
    /// Hyperliquid execution account kind. Delegated scopes are attached only
    /// to an already-enrolled protected master signer during human enrollment.
    #[arg(long, value_enum)]
    pub hyperliquid_scope: Option<HyperliquidScopeKind>,
    /// Hyperliquid subaccount or vault address to attach to the protected master.
    #[arg(long)]
    pub execution_address: Option<String>,
    /// CoinGecko API plan: demo (default) or pro. Public metadata only.
    #[arg(long, value_parser = ["demo", "pro"])]
    pub api_plan: Option<String>,
    /// Credential category only: hmac/rsa/ed25519/ecdsa, private_key/read_only or api_key.
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

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigurationCommandInput {
    pub operation: String,
    pub input: Value,
    pub request_id: Option<String>,
    pub dry_run: Option<bool>,
}

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
    Value::Array(
        gofin_networks::all()
            .iter()
            .map(|network| {
                let operations = if network.chain_id == 1 {
                    json!([
                        "wallet",
                        "Uniswap",
                        "Sushi v3",
                        "PancakeSwap v3",
                        "Aave",
                        "Morpho V2"
                    ])
                } else if network.chain_id == 8453 {
                    json!(["wallet", "Uniswap", "Aave", "Morpho V2"])
                } else if network.chain_id == 4663 {
                    json!(["wallet", "Uniswap", "Ramses v3", "PancakeSwap v3"])
                } else if network.chain_id == 42161 {
                    json!(["wallet", "Uniswap", "Sushi v3", "PancakeSwap v3", "Aave"])
                } else {
                    json!(["wallet"])
                };
                json!({
                    "network": network.network_id,
                    "name": network.display_name,
                    "chain_id": network.chain_id,
                    "native_coin": network.native_symbol(),
                    "operations": operations
                })
            })
            .collect(),
    )
}

fn reviewed_evm_network_ids() -> Vec<String> {
    gofin_networks::all()
        .iter()
        .map(|network| network.network_id.to_owned())
        .collect()
}

/// Offline bridge scope. The Relay settlement observer is intentionally kept
/// separate from wallet networks and never grants wallet authority.
pub fn bridge_profile() -> Value {
    json!({
        "providers": [
            {
                "id": "relay",
                "coverage": "Quote-dependent native ETH and verified ERC-20 routes between reviewed EVM networks, plus both directions between reviewed EVM networks and Solana mainnet-beta using native SOL or runtime-verified SPL/Token-2022 mints; an exact recipient may be unenrolled"
            },
            {
                "id": "arbitrum_canonical",
                "coverage": "Native ETH from Ethereum Mainnet to Arbitrum One or Robinhood Chain only; destination transaction identity is derived from finalized source proof"
            },
            {"id":"base_canonical","coverage":"Ethereum ↔ Base ETH and verified StandardBridge ERC-20 pairs; Base withdrawal has separate source, proof and finalization stages"},
            {"id":"cctp","coverage":"Native USDC between reviewed EVM networks and from those EVM networks to an enrolled Solana mainnet-beta destination wallet only"}
        ],
        "provider_credentials": "Relay uses a keyless GoFin proxy with public Relay fallback; an optional protected key uses Relay directly and is never supplied in bridge input or output; canonical providers use configured read-only RPC",
        "wallet_chains": gofin_networks::all().iter().map(|network| json!({
            "chain_id": network.chain_id,
            "network": network.network_id,
            "name": network.display_name
        })).chain(std::iter::once(json!({"chain_id":792703809,"network":"solana:mainnet-beta","name":"Solana Mainnet Beta"}))).collect::<Vec<_>>(),
        "settlement_observers": [
            {"chain_id": 537713, "name": "RelayChain", "role": "read_only_settlement_evidence", "wallet_authority": false}
        ],
        "solver_observers": [
            {"chain_id":8453,"name":"Base","role":"read_only_protocol_order_observer","wallet_authority":false}
        ],
        "assets": "EVM native ETH and exact ERC-20 contracts with RPC-verified precision; Relay also supports native SOL and exact SPL/Token-2022 mints verified by the Solana runtime when an executable quote exists; Base canonical ERC-20s require verified bridge pairs; CCTP Solana accepts native USDC only",
        "writes": "provider-reviewed deposits and finite token approvals; Relay Solana source transactions require complete frozen-transaction validation and currently accept direct transfers only; compound Base canonical withdrawals include reviewed proof and finalization stages; no agent-supplied calldata",
        "relay_solana": {"chain_id":792703809,"network":"solana:mainnet-beta","directions":"to and from reviewed EVM networks","recipient":"exact user-selected base58 address; destination wallet enrollment optional","native_mint_input":null,"token_mint_input":"exact base58 mint","availability":"requires an executable Relay quote, independent asset and transaction verification, and approved native bounds; optional USD estimates may be unavailable"},
        "quotes": "quotes report individual provider coverage, not a market-wide comparison or financial authorization",
        "input_schemas": {
            "bridge.routes": serde_json::to_value(schemars::schema_for!(gofin_bridge::BridgeRoutesInput)).expect("schema serializes"),
            "bridge.recovery": serde_json::to_value(schemars::schema_for!(gofin_bridge::BridgeStatusInput)).expect("schema serializes"),
            "bridge.quotes": gofin_bridge::native_bridge_input_schema(serde_json::to_value(schemars::schema_for!(gofin_bridge::BridgeQuoteInput)).expect("schema serializes")),
            "bridge.transfer": gofin_bridge::native_bridge_input_schema(serde_json::to_value(schemars::schema_for!(gofin_bridge::BridgeTransferInput)).expect("schema serializes")),
            "bridge.status": serde_json::to_value(schemars::schema_for!(gofin_bridge::BridgeStatusInput)).expect("schema serializes")
        }
    })
}

/// Machine execution policy shared by offline discovery and callers.
pub fn machine_contract() -> Value {
    let mut contract = json!({
        "version":1,"flag":"--json","stdout":"one JSON value on success or error",
        "stderr":"no human prose, progress or prompts","color":false,"terminal_prompts":false,
        "verbose":"ignored","exit_status":{"success":0,"error":1},
        "error_envelope":{"ok":false,"error":"GoFinError"},
        "human_interactive_commands":"Return HUMAN_TERMINAL_REQUIRED before privilege escalation or credential access; human runs the command without --json.",
        "financial_writes":"One logical action under native economic approval or protected Yolo. Repeating the execute request observes its original identity and never starts another execution.",
        "approval_terms":{"enforced":"Independently checked before dispatch","estimate":"Context only","after_execution":"Observed breach is a discrepancy; execution remains executed"},
        "outcome":{"version":1,"schema":schema::<gofin_domain::outcome::OperationOutcome>(),
            "additive":true,"top_level_submission":"The immutable submission identity, unchanged",
            "replay_safe":"Proof against duplicate execution; never grants approval",
            "observation":"status ID is bounded and read-only; --wait SECONDS polls until phase changes or time expires",
            "open":"operations --open includes unresolved refunds even when phase is failed",
            "continue":"operations continue ID --token TOKEN resumes only declared, approved stages; use returned next_action arguments",
            "reauthorize":"operations reauthorize ID --token TOKEN --input JSON explicitly reviews replacement terms for unfinished work when returned by next_action; completed stages remain immutable",
            "foreground":"No background runner. Disconnect prevents new stage admission; admitted immutable submissions may finish dispatch."},
        "retired":["personal USD limits","balance reservations","named plans","bounded grants","human outcome overrides"]
    });
    contract["yolo"] = json!({
        "default":"off",
        "status":"gofin yolo status --json; authoritative broker read without sudo",
        "management":"gofin yolo on/off require verified human Terminal and fresh sudo; JSON hands off before sudo or input",
        "approval":"On requires the full danger warning and explicit single-key y/Y confirmation without Enter; neither on nor off requires Touch ID",
        "financial_authority":"Native terms of each chosen action; no personal USD limits. Signed payload validation and independent authority checks remain enforced",
        "disable":"Stops unadmitted Yolo work and invalidates unused Yolo authority permanently; admitted identities may finish dispatch and reconcile"
    });
    contract["streaming"] = json!({
        "flag":"--stream",
        "requires":"--json",
        "stdout":"NDJSON: zero or more progress events, then exactly one result or error event when output is available",
        "stderr":"no prose, animation, color or terminal prompts",
        "progress_schema":{"type":"object","required":["type","stage"],"properties":{"type":{"const":"progress"},"stage":{"enum":["connecting","preparing","refreshing_candidate","awaiting_approval","revalidating","submitting","verifying"]},"context":{"type":"object"}}},
        "result_schema":{"type":"object","required":["type","result"],"properties":{"type":{"const":"result"},"result":{"description":"Unchanged command output schema"},"context":{"type":"object"}}},
        "error_schema":{"type":"object","required":["type","error"],"properties":{"type":{"const":"error"},"error":schema::<GoFinError>(),"context":{"type":"object"}}},
        "context":"Command plus available operation, request_id or operation_id. Parse/startup errors may omit context. Newly assigned operation IDs are in the final result.",
        "stages":"connecting is a local transport observation; other stages are reported by the broker. Progress is best effort and can be dropped for slow readers; it grants no authority and proves no settlement.",
        "completion":"The terminal event ends this invocation. Inspect result.outcome and its next_action for observation or continuation of the original action.",
        "interruption":"Disconnect prevents new stage admissions. Already admitted immutable submissions may finish. Observe the original operation; never repeat successful stages.",
        "example":"gofin execute OPERATION --input INPUT_JSON --id ACTION_ID --json --stream"
    });
    contract
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
pub struct ReadInput {
    pub operation: String,
    pub input: Value,
    pub output: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigureInput {
    pub operation: String,
    pub input: Value,
    pub request_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecuteInput {
    pub operation: String,
    /// Validated against the selected operation's bundled input schema.
    pub input: Value,
    /// Required for execution; previews do not consume an identity.
    pub request_id: Option<String>,
    /// Same read-only preparation as `gofin preview`, without approval/submission.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_for: Option<String>,
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
pub struct StatusInput {
    pub id: String,
    pub wait: Option<u64>,
    pub side: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationsInput {
    #[serde(default)]
    pub open: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContinueInput {
    pub id: String,
    pub token: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReauthorizeInput {
    pub id: String,
    pub token: String,
    pub input: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReservationOperationInput {
    pub operation_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReservationReleaseInput {
    pub operation_id: String,
    pub request_id: String,
    pub review_hash: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationResolutionPreviewInput {
    pub operation_id: String,
    pub input: gofin_domain::operation_recovery::OperationResolutionInput,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationResolveInput {
    pub operation_id: String,
    pub request_id: String,
    pub review_hash: String,
    pub input: gofin_domain::operation_recovery::OperationResolutionInput,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationReceiptRecoveryInput {
    pub operation_id: String,
    pub reference: gofin_domain::operation_recovery::RecoveryReference,
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
        return Err(GoFinError::validation(
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
        return Err(GoFinError::validation(
            "A valid Binance account email is required.",
        ));
    }
    if args.provider != Provider::CoinGecko && args.api_plan.is_some() {
        return Err(GoFinError::validation(
            "--api-plan belongs only to a CoinGecko market-data connection.",
        ));
    }
    if args.portfolio_id.as_deref().is_some_and(|id| {
        id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    }) || (args.provider != Provider::Coinbase
        && args.provider != Provider::CoinbaseExchange
        && args.provider != Provider::CoinbaseInternational
        && args.portfolio_id.is_some())
    {
        return Err(GoFinError::validation(
            "--portfolio-id is an exact Coinbase portfolio selector with a 1-128 character ID.",
        ));
    }
    if args.provider != Provider::Hyperliquid
        && (args.hyperliquid_scope.is_some() || args.execution_address.is_some())
    {
        return Err(GoFinError::validation(
            "Hyperliquid scope options belong only to a Hyperliquid connection.",
        ));
    }
    if !matches!(args.provider, Provider::Kraken | Provider::KrakenFutures)
        && args.account_id.is_some()
    {
        return Err(GoFinError::validation(
            "--account-id is Kraken account metadata.",
        ));
    }
    if let Some(id) = &args.default_wallet_account_id
        && (args.provider != Provider::Kraken
            || id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)))
    {
        return Err(GoFinError::validation(
            "--default-wallet-account-id requires Kraken Spot and an exact 1-128 character wallet ID.",
        ));
    }
    match args.provider {
        Provider::Kraken | Provider::KrakenFutures => {
            if args.role.is_some()
                || args.parent.is_some()
                || args.email.is_some()
                || args.network.is_some()
                || args.address.is_some()
                || !matches!(args.credential_type, None | Some(CredentialType::Hmac))
            {
                return Err(GoFinError::validation(
                    "Kraken uses product-specific HMAC keys; Binance relationships and wallet metadata do not apply.",
                ));
            }
            if !args.account_id.as_deref().is_some_and(|id| {
                !id.is_empty()
                    && id.len() <= 128
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            }) {
                return Err(GoFinError::validation(
                    "Kraken requires its public --account-id, confirmed by the human; GoFin never guesses account relationships.",
                ));
            }
        }

        Provider::Hyperliquid => {
            if args.role.is_some()
                || args.parent.is_some()
                || args.email.is_some()
                || args.network.is_some()
                || args.api_plan.is_some()
            {
                return Err(GoFinError::validation(
                    "Hyperliquid connections require an owner address and do not accept Binance relationships, EVM networks or market-data plans.",
                ));
            }
            if args.address.as_deref().is_none_or(|address| {
                address.len() != 42
                    || !address.starts_with("0x")
                    || !address[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
                    || address[2..].bytes().all(|byte| byte == b'0')
            }) {
                return Err(GoFinError::validation(
                    "Hyperliquid requires --address with the exact nonzero owner address.",
                ));
            }
            let delegated = matches!(
                args.hyperliquid_scope,
                Some(HyperliquidScopeKind::Subaccount | HyperliquidScopeKind::Vault)
            );
            if delegated != args.execution_address.is_some() {
                return Err(GoFinError::validation(
                    "Hyperliquid delegated scope requires both --hyperliquid-scope and --execution-address.",
                ));
            }
            if let Some(execution_address) = &args.execution_address
                && (execution_address.len() != 42
                    || !execution_address.starts_with("0x")
                    || !execution_address[2..]
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit())
                    || execution_address[2..].bytes().all(|byte| byte == b'0'))
            {
                return Err(GoFinError::validation(
                    "Hyperliquid execution address must be a valid nonzero address.",
                ));
            }
            if delegated && args.credential_type.is_some() {
                return Err(GoFinError::validation(
                    "A delegated Hyperliquid scope reuses the protected master signer; do not provide a new credential type.",
                ));
            }
            if !delegated
                && !matches!(
                    args.credential_type,
                    Some(CredentialType::PrivateKey | CredentialType::ReadOnly)
                )
            {
                return Err(GoFinError::validation(
                    "Hyperliquid requires --credential-type private_key or read_only.",
                ));
            }
        }
        Provider::Solana => {
            if args.role.is_some()
                || args.parent.is_some()
                || args.email.is_some()
                || args
                    .network
                    .as_deref()
                    .is_some_and(|network| network != "solana:mainnet-beta")
                || args.api_plan.is_some()
                || args.hyperliquid_scope.is_some()
                || args.execution_address.is_some()
            {
                return Err(GoFinError::validation(
                    "Solana wallets use the reviewed solana:mainnet-beta cluster and do not accept account relationships or EVM options.",
                ));
            }
            if !matches!(
                args.credential_type,
                Some(CredentialType::PrivateKey | CredentialType::ReadOnly)
            ) {
                return Err(GoFinError::validation(
                    "Solana wallets require --credential-type private_key or read_only.",
                ));
            }
            if args.credential_type == Some(CredentialType::ReadOnly) && args.address.is_none() {
                return Err(GoFinError::validation(
                    "A read-only Solana wallet requires --address.",
                ));
            }
            if args.address.as_deref().is_some_and(|address| {
                address.len() > 44 || {
                    bs58::decode(address)
                        .into_vec()
                        .map_or(true, |bytes| bytes.len() != 32)
                }
            }) {
                return Err(GoFinError::validation(
                    "The Solana wallet address must be a valid base58 public key.",
                ));
            }
        }
        Provider::Jupiter | Provider::Relay => {
            if args.role.is_some()
                || args.parent.is_some()
                || args.email.is_some()
                || args.network.is_some()
                || args.address.is_some()
                || args.api_plan.is_some()
                || args.hyperliquid_scope.is_some()
                || args.execution_address.is_some()
                || args.credential_type != Some(CredentialType::ApiKey)
            {
                return Err(GoFinError::validation(
                    "Service credentials require --credential-type api_key and accept no wallet metadata.",
                ));
            }
        }
        Provider::CoinGecko => {
            if args.role.is_some()
                || args.parent.is_some()
                || args.email.is_some()
                || args.network.is_some()
                || args.address.is_some()
            {
                return Err(GoFinError::validation(
                    "CoinGecko market-data connections do not accept account relationships, wallet addresses or networks.",
                ));
            }
            if args
                .api_plan
                .as_deref()
                .is_some_and(|plan| !matches!(plan, "demo" | "pro"))
            {
                return Err(GoFinError::validation(
                    "Choose --api-plan demo or pro for CoinGecko.",
                ));
            }
            if args.credential_type != Some(CredentialType::ApiKey) {
                return Err(GoFinError::validation(
                    "CoinGecko requires --credential-type api_key; enter its value only in the protected Terminal prompt.",
                ));
            }
        }
        Provider::Coinbase => {
            if args.role.is_some()
                || args.parent.is_some()
                || args.email.is_some()
                || args.network.is_some()
                || args.address.is_some()
                || args.account_id.is_some()
                || args.api_plan.is_some()
                || args.default_wallet_account_id.is_some()
                || args.hyperliquid_scope.is_some()
                || args.execution_address.is_some()
            {
                return Err(GoFinError::validation(
                    "Coinbase connections do not accept unrelated account, wallet or network metadata.",
                ));
            }
            if !matches!(
                args.credential_type,
                Some(CredentialType::Ecdsa | CredentialType::Ed25519)
            ) {
                return Err(GoFinError::validation(
                    "Coinbase CDP credentials require --credential-type ecdsa or ed25519.",
                ));
            }
        }
        Provider::CoinbaseExchange | Provider::CoinbaseInternational => {
            if args.role.is_some()
                || args.parent.is_some()
                || args.email.is_some()
                || args.network.is_some()
                || args.address.is_some()
                || args.account_id.is_some()
                || args.api_plan.is_some()
                || args.default_wallet_account_id.is_some()
                || args.hyperliquid_scope.is_some()
                || args.execution_address.is_some()
                || args.credential_type != Some(CredentialType::Hmac)
            {
                return Err(GoFinError::validation(
                    "Coinbase Exchange and International require --credential-type hmac and accept no unrelated account, wallet or network metadata.",
                ));
            }
        }
        Provider::Binance => {
            let role = args.role.ok_or_else(|| {
                GoFinError::validation("Binance requires --role main or --role sub.")
            })?;
            if args.network.is_some() || args.address.is_some() {
                return Err(GoFinError::validation(
                    "Binance funding uses SPOT accounts; wallet network/address belong to a separate wallet connection.",
                ));
            }
            if role == BinanceRole::Sub && args.parent.is_some() != args.email.is_some() {
                return Err(GoFinError::validation(
                    "When supplying a Binance --parent, also supply its --email; both are optional for standalone subaccount credentials.",
                ));
            }
            if role == BinanceRole::Main && args.parent.is_some() {
                return Err(GoFinError::validation(
                    "A Binance main account cannot have a parent connection.",
                ));
            }
            if role == BinanceRole::Main && args.email.is_some() {
                return Err(GoFinError::validation(
                    "A Binance main account cannot have a subaccount email.",
                ));
            }
            if !matches!(
                args.credential_type,
                None | Some(CredentialType::Hmac | CredentialType::Rsa | CredentialType::Ed25519)
            ) {
                return Err(GoFinError::validation(
                    "Binance credentials support hmac, rsa, or ed25519.",
                ));
            }
        }
        Provider::Evm => {
            if args.role.is_some() || args.parent.is_some() || args.email.is_some() {
                return Err(GoFinError::validation(
                    "EVM wallets do not accept Binance account relationships.",
                ));
            }
            if args
                .network
                .as_deref()
                .is_some_and(|network| gofin_networks::by_network_id(network).is_none())
            {
                return Err(GoFinError::validation(
                    "Choose a reviewed EVM network for the optional enrollment check; wallets are reusable across configured networks.",
                ));
            }
            if !matches!(
                args.credential_type,
                Some(CredentialType::PrivateKey | CredentialType::ReadOnly)
            ) {
                return Err(GoFinError::validation(
                    "EVM wallets require --credential-type private_key or read_only.",
                ));
            }
            if args.credential_type == Some(CredentialType::ReadOnly) && args.address.is_none() {
                return Err(GoFinError::validation(
                    "A read-only wallet requires --address.",
                ));
            }
            if args.address.as_ref().is_some_and(|address| {
                address.len() != 42
                    || !address.starts_with("0x")
                    || !address[2..].bytes().all(|b| b.is_ascii_hexdigit())
                    || address[2..].bytes().all(|b| b == b'0')
            }) {
                return Err(GoFinError::validation(
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

/// Public projection is additive; the persisted operation record remains unchanged.
pub fn operation_view_schema() -> Value {
    let mut record = schema::<gofin_domain::OperationRecord>();
    let mut outcome = schema::<gofin_domain::outcome::OperationOutcome>();
    if let Some(Value::Object(definitions)) =
        outcome.as_object_mut().and_then(|o| o.remove("$defs"))
        && let Some(target) = record.get_mut("$defs").and_then(Value::as_object_mut)
    {
        for (name, value) in &definitions {
            if let Some(existing) = target.get(name) {
                assert_eq!(existing, value, "shared outcome schema definition differs");
            } else {
                target.insert(name.clone(), value.clone());
            }
        }
    }
    if let Some(object) = outcome.as_object_mut() {
        object.remove("$schema");
    }
    record["properties"]["outcome"] = outcome;
    record["properties"]["human_dismissed"] = json!({"type":"boolean","description":"Human label only; does not change financial evidence or replay safety."});
    if let Some(required) = record["required"].as_array_mut() {
        required.push(json!("outcome"));
    }
    // Versioned observations and stage details are additive presentation fields.
    record["additionalProperties"] = json!(true);
    record["title"] = json!("OperationView");
    record
}

fn definition<T: JsonSchema>(
    name: &str,
    description: &str,
    kind: OperationKind,
    actor: ExecutionActor,
    example: Value,
    semantics: Value,
) -> Definition {
    Definition{name:name.into(),description:description.into(),kind,execution_actor:actor,input_schema:schema::<T>(),output_schema:json!({"type":"object","description":"Structured result or typed error with observed checks and a concrete next action. No credential values."}),error_schema:schema::<GoFinError>(),examples:vec![example],coverage:"Implemented command contract; runtime platform and connection availability are reported when executed.".into(),completion:"Reports only the observed command result. Uncertain financial writes remain unresolved until reconciled.".into(),semantics}
}

pub fn operation_definitions() -> Vec<Definition> {
    let evm_network_ids = reviewed_evm_network_ids();
    let mut connection_network_enum = evm_network_ids
        .iter()
        .cloned()
        .map(Value::String)
        .collect::<Vec<_>>();
    connection_network_enum.push(Value::Null);
    let default_evm_network = gofin_networks::by_chain_id(1)
        .expect("Ethereum is reviewed")
        .network_id;
    let administrator_authentication = json!({
        "provider":"macOS sudo",
        "requires_fresh_sudo":true,
        "requires_fresh_touch_id":false,
        "authorization_source":"sudo",
        "supported_terminals":["Apple Terminal","macOS system OpenSSH (interactive SSH, including Tailscale transport)"],
        "remote_trust":"The human must control the SSH client. GoFin verifies server-side session and sudo provenance, not the remote operator or whether sudo policy required a password. Agent-accessible root and passwordless elevation are outside the isolation guarantee.",
        "explanation":MACOS_ADMIN_PASSWORD_HELP,
        "password_prompt":MACOS_ADMIN_PASSWORD_PROMPT,
        "scope":"Setup, connection add/generate/update/remove, nonfinancial dismissal and administrative configure. Everyday reads and financial execution use the protected background service without sudo.",
        "configuration_approval":"Fresh system sudo plus exact protected terminal confirmation; no Touch ID is requested. Configuration approval grants no financial authority.",
        "financial_approval":"With protected Yolo off, actions require native intent approval. Compound stages share reviewed terms and an explicit deadline. Enabled Yolo supplies typed financial authority bounded by each action’s terms; no personal USD limits apply. Authority, native bounds and payload identity are checked at every admission."
    });
    let enrollment = json!({
        "requires_human_terminal":true,"secret_input":"protected-hidden-terminal-prompt",
        "requires_administrator":true,"requires_fresh_touch_id":false,"authorization_source":"sudo","administrator_authentication":administrator_authentication.clone(),
        "secret_values_accepted_in_arguments":false,"secret_values_accepted_in_json":false,
        "human_action":"Run this command yourself in Apple Terminal or a human-operated macOS system OpenSSH session. Never enter keys through an agent tool or chat.",
        "platform":"macOS protected enrollment in Apple Terminal or macOS system OpenSSH; no GUI login or Touch ID is required; unavailable providers fail closed",
        "configuration_authorization":"Fresh sudo and protected terminal confirmation approve the resolved connection. Saves configuration only, moves no funds, grants no future financial authority.",
        "financial_authorization":"Enrollment and daemon startup grant no financial authority. Actions use native intent approval or explicitly enabled protected Yolo. Compound stages remain within the original reviewed terms and deadline; every admission independently validates authority and its exact payload.",
        "metadata_input":"Run gofin connections add for guided public metadata, or gofin connections add binance/ethereum/solana/jupiter/relay/coinbase/coinbaseexchange/coinbaseinternational to preselect the provider. The positional provider and --provider are alternatives; do not combine them. Supply complete metadata flags for explicit enrollment. Updates require flags.",
        "pem_input":"In the private human-terminal prompt, paste the complete BEGIN/END block using actual line breaks or literal \\n (also \\r\\n) separators, without surrounding quotes, then press Enter. Never supply it through arguments or chat.",
        "input_progress":"Hidden input acknowledges each received secret without echoing it. Read-only validation and protected review stages appear on stderr.",
        "output_format":"Concise human summary on stdout; warnings/progress on stderr. --json returns a structured HUMAN_TERMINAL_REQUIRED handoff before enrollment.",
        "command_line_examples":[
            "gofin connections add binance",
            "gofin connections add binance --name master --role main --credential-type ed25519",
            "gofin connections add --provider binance --name main --role main --credential-type hmac",
            "gofin connections add --provider binance --name main-rsa --role main --credential-type rsa",
            "gofin connections add --provider binance --name main-ed25519 --role main --credential-type ed25519",
            "gofin connections add --provider binance --name savings --role sub --credential-type hmac",
            "gofin connections add --provider binance --name savings --role sub --parent main --email subaccount@example.invalid --credential-type hmac",
            "gofin connections add --provider coingecko --name pricing --credential-type api_key --api-plan demo",
            "gofin connections add --provider kraken --name spot --credential-type hmac --account-id PUBLIC_ACCOUNT_ID",
            "gofin connections add --provider kraken --name spot --credential-type hmac --account-id PUBLIC_ACCOUNT_ID --default-wallet-account-id OWNED_WALLET_ID",
            "gofin connections add --provider krakenfutures --name derivatives --credential-type hmac --account-id PUBLIC_ACCOUNT_ID",
            "gofin connections add --provider evm --name wallet --credential-type private_key",
            "gofin connections add --provider evm --name watch --credential-type read_only --address 0x1111111111111111111111111111111111111111",
            "gofin connections add --provider hyperliquid --name hl-main --credential-type read_only --address 0x1111111111111111111111111111111111111111",
            "gofin connections add --provider hyperliquid --name hl-sub --address 0x1111111111111111111111111111111111111111 --hyperliquid-scope subaccount --execution-address 0x2222222222222222222222222222222222222222"
            ,"gofin connections add --provider solana --name sol-main --credential-type private_key"
            ,"gofin connections add --provider solana --name sol-watch --credential-type read_only --address 11111111111111111111111111111111"
            ,"gofin connections add --provider jupiter --name jupiter-api --credential-type api_key"
            ,"gofin connections add --provider relay --name relay-api --credential-type api_key"
            ,"gofin connections add --provider coinbase --name retail --credential-type ed25519 --portfolio-id PORTFOLIO_UUID"
            ,"gofin connections add --provider coinbaseexchange --name exchange --credential-type hmac --portfolio-id PROFILE_ID"
            ,"gofin connections add --provider coinbaseinternational --name international --credential-type hmac --portfolio-id PORTFOLIO_ID"
        ],
        "binance":{"wallet_type":"SPOT","default_credential_type":"hmac","supported_credential_types":["hmac","rsa","ed25519"],"mandatory_fields":["role=main|sub"],"credentials_required":["API key","HMAC secret, RSA PEM signing key, or Ed25519 private signing key"],"roles":["main","sub"],"main_rejects":["parent","email"],"subaccount_modes":{"standalone":{"parent_required":false,"email_required":false,"identity":"the authenticated subaccount's own verified UID","hierarchy_authority":"none inferred"},"parent_linked":{"parent_required":true,"email_required":true,"check":"optional parent main key verifies the supplied email association"}},"checks":["read-only authentication","reported API restrictions","account identity; optional parent mapping"],"prerequisites":["machine outbound IP allowed","main universal-transfer permission only for main-authorized internal transfers","withdrawal permission and whitelisted destination for withdrawals"],"unknown_permissions":"Explicitly reported; enrollment performs no trial transfer or withdrawal. A main key is needed only for explicitly parent-authorized hierarchy operations, not for standalone subaccount reads or enrollment."},
        "kraken":{"products":["kraken","krakenfutures"],"credential_type":"hmac","credentials_required":["API key","base64 API secret"],"api_2fa_supported":false,"account_identity":"--account-id is public metadata; signed IIBAN is matched where available, otherwise human confirmation is explicit.","checks":["product-specific authenticated account read","selected default-wallet authenticated ownership"],"default_wallet_relationship":{"argument":"--default-wallet-account-id","product":"kraken Spot only","optional":true,"relationship_verification":"human_verified","ownership":"authenticated ListWalletAccounts","alias":"Only the exact protected selected ID shares legacy Spot custody; Kraken does not identify which wallet is its default.","replacement":"Omitting an existing selection preserves it; retargeting is rejected to preserve custody history."},"financial_writes":"Separate financial authorization is required; enrollment grants no authority"},
        "coingecko":{"provider":"coingecko","resource":"market-data:coingecko","api_plans":["demo","pro"],"default_api_plan":"demo","credential_type":"api_key","credentials_required":["CoinGecko API key"],"scope":"Market-data requests only; this key grants no exchange or wallet access. Requests can consume the selected plan quota/billing.","checks":["read-only authenticated CoinGecko ping","fresh ETH/USD quote"],"authentication":"Optional protected human enrollment. Without it, anonymous public pricing remains best effort; missing or stale optional estimates remain unavailable; protocol-required quotes have separate typed prerequisites."},
        "evm":{"provider":"evm","legacy_provider_alias":"ethereum","wallet_scope":"One address and signing key reusable across reviewed EVM networks; choose a network for each operation. A Hyperliquid owner account is linked automatically with the same signer/read-only status, without granting financial authority.","network_argument":format!("Optional read-only enrollment check; defaults to {default_evm_network} and does not scope the wallet key."),"networks":evm_network_ids,"network_profiles":evm_network_profiles(),"credential_types":["private_key","read_only"],"checks":["public RPC chain identity","derived signing address matches resolved wallet","reported native balance"],"dapp_scope":"Uniswap quotes and swaps use the fixed keyless GoFin Uniswap API provider on Ethereum, Base, Arbitrum and Robinhood Chain, with best-price AMM or UniswapX routing selected from available liquidity. Financial bounds are enforced against quote metadata; transaction payload construction is trusted to the provider. Ramses v3 is available on Robinhood Chain only when its reviewed deployment is configured. Sushi v3 is available on Ethereum and Arbitrum. PancakeSwap v3 is available on Ethereum, Arbitrum and Robinhood Chain; Robinhood uses its reviewed SmartRouter deployment. These routes require configured reviewed deployments and exact pool liquidity. Aave V3 exposes reviewed Ethereum, Base and Arbitrum markets; exact deployed reserve compatibility is checked before writes. Morpho V2 discovery, ERC-20 vault deposits, standard withdrawals and Merkl claims are available on Ethereum and Base; V1 vaults are excluded. Tokens are supplied by exact contract identity."},
        "hyperliquid":{"provider":"hyperliquid","network":"hyperliquid:mainnet","owner_required":true,"credential_types":["private_key","read_only"],"execution_scopes":{"owner":"Automatically linked reusable EVM wallet address, or an explicitly enrolled independent owner.","subaccount":"Human enrollment reuses an existing protected master signer after exact owner-scoped subAccounts verification.","vault":"Human enrollment reuses an existing protected master signer after exact vaultDetails leader verification."},"scope":"Signer identity and nonce namespace remain the master owner; delegated trading reads/resources and signed vaultAddress use the reviewed execution address. Relationship is refreshed before dispatch. Delegated accounts permit trading, isolated margin and TWAP/cancel only; account authority and transfers remain owner-scoped.","checks":["owner address format","protected signer address matches owner when private_key is selected","delegated account relationship verified through fixed official reads"],"enrollment_network_call":true,"automatic_evm_link":{"supported":true,"scope":"reusable_evm_owner_only","account_argument":"existing EVM wallet name","separate_public_connection":false,"credential_reentry":false,"inherits_read_only":true,"financial_authority":false,"existing_owner_aliases":"preserved without permission elevation or duplicate observations","persistence":"runtime effective-config overlay; follows parent wallet rotation/removal"},"note":"Reusable EVM wallets link automatically to their Hyperliquid owner account. Delegated subaccount/vault enrollment remains explicitly human-reviewed and reuses an existing protected master signer."},
        "solana":{"provider":"solana","network":"solana:mainnet-beta","wallet_scope":"One exact base58 address on reviewed Solana mainnet-beta.","credential_types":["private_key","read_only"],"secret_formats":["64-byte base58 keypair","JSON array of 64 bytes"],"checks":["per-provider mainnet-beta genesis identity","signer-derived address matches optional expected address","native balance read"],"rpc_policy":{"reads":"Four ordered reviewed keyless providers (Solana public RPC, PublicNode, OnFinality, Solana Tracker) with failover, per-provider health and a twenty-second transport-read budget.","submission":"One provider per financial dispatch; uncertain outcomes reconcile the original signature without automatic retransmission.","diagnostics":"doctor reports sanitized observed pool health without new RPC probes.","upgrades":"Reviewed defaults apply to existing installations without credential or pending-operation migration."},"credential_boundary":"Signing key is collected only in the protected human terminal and stored as protected credential material."},
        "jupiter":{"provider":"jupiter","credential_type":"api_key","scope":"Optional protected Jupiter service credential, separate from every wallet signer. Keyless use is the default and no shared key is shipped."},
        "relay":{"provider":"relay","credential_type":"api_key","resource":"relay:service","scope":"Optional protected Relay service credential, separate from wallet signing authority. Keyless quotes and request lookup work without it; authenticated request lookup uses it when enrolled."},
        "failure":"Safe stable error and next action; add cancellation/validation leaves no candidate, while update cancellation/validation preserves the prior active connection. An uncertain commit remains blocked until configuration readback confirms its state.",
        "replacement":"Pending operations are checked by physical resource before changing/removing credentials; unresolved work refuses replacement.",
        "next_check":"gofin connections list --json"
    });
    let mut definitions = vec![
        definition::<CompletionInput>(
            "completions",
            "Generate bash, zsh or fish completion for manual/custom use; setup installs completion automatically.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({"shell":"zsh"}),
            json!({"service_required":false,"command_line_example":"gofin completions zsh","human_output":"shell script","machine_output":"object with shell and script"}),
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
            json!({"service_required":false,"secret_input":"none","financial_movement":false,"without_name":"Help and examples; machine output is a help object, as with --help.","catalog_command":"gofin capabilities"}),
        ),
        definition::<DefinitionInput>(
            "schema",
            "Show bundled input, output, error and actor metadata; omit name for usage and examples.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({"name":"connections.add"}),
            json!({"service_required":false,"secret_input":"none","financial_movement":false,"without_name":"Help and examples; machine output is a help object, as with --help.","catalog_command":"gofin capabilities"}),
        ),
        definition::<ReadInput>(
            "read",
            "Read one supported financial operation with gofin read OPERATION --input JSON through the protected broker.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({"operation":"binance.balances","input":{"account":"main"}}),
            json!({"input_validation":"operation-specific bundled schema","service_required":true,"financial_authorization":false,"command_line_example":"gofin read binance.balances --input '{\"account\":\"main\"}'","arguments":["operation","--input JSON","--output PATH"],"artifact_output":{"required_for":["kraken.retrieve-export","krakenfutures.account-log-csv"],"maximum_bytes":33554432,"permissions":"0600","overwrite":false,"completion":"SHA-256 verified before publication"},"account_selection":"The operation-specific input schema defines account selection; there is no generic --account flag."}),
        ),
        definition::<OperationInput>(
            "preview",
            "Validate and preview one financial operation with gofin preview OPERATION --input JSON without submitting a write.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({"operation":"binance.transfer","input":{"requestId":"synthetic-preview-1","authority":"main","from":"main","to":"savings","coin":"USDT","amount":"1","fromWallet":"SPOT","toWallet":"SPOT"}}),
            json!({"input_validation":"operation-specific bundled schema","service_required":true,"financial_authorization":false,"command_line_example":"gofin preview binance.transfer --input '{\"requestId\":\"synthetic-preview-1\",\"authority\":\"main\",\"from\":\"main\",\"to\":\"savings\",\"coin\":\"USDT\",\"amount\":\"1\",\"fromWallet\":\"SPOT\",\"toWallet\":\"SPOT\"}'","arguments":["operation","--input JSON"]}),
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
                "fresh_approval":"Native economic approval per logical action, independently checked at each admission",
                "retry":"Repeating the same --id observes the original action. Use broker-issued continuation or reauthorization arguments for unfinished work.",
                "command_line_template":"gofin execute OPERATION --input JSON --id REQUEST_ID --json",
                "command_line_example":"gofin execute binance.transfer --input '{\"requestId\":\"synthetic-venue-request-001\",\"authority\":\"main\",\"from\":\"main\",\"to\":\"savings\",\"coin\":\"USDT\",\"amount\":\"1\",\"fromWallet\":\"SPOT\",\"toWallet\":\"SPOT\"}' --id synthetic-logical-write-001 --json",
                "arguments":["OPERATION","--input JSON","--id REQUEST_ID","--dry-run","--json","--stream"],
                "cli_field_mapping":{"request_id":"--id","dry_run":"--dry-run"},
                "allocation_scope":"Installation-wide rolling 24-hour gross movement, maximum costs and additional borrowed allocation; unresolved submissions continue counting beyond the window. No run, return plan or restoration required.",
                "dry_run":{"flag":"--dry-run","command_line_template":"gofin execute OPERATION --input JSON --dry-run --json","equivalent_to":"gofin preview OPERATION --input JSON --json","financial_movement":false,"financial_authorization":false,"journal_write":false,"identity":"--id is optional and not consumed; operation-specific input identities remain required where its schema requires them","allocation_scope":"Previews do not reserve or validate the rolling allocation budget","result":"The same preview result/errors as gofin preview; checks and simulations depend on the adapter","execution":"Remove --dry-run and supply --id to execute. Standalone exact writes require fresh approval; attempts selected under an active boundary grant or named plan use that authority without another prompt. The broker rechecks current evidence, authority and protected policy before every dispatch."},
                "venue_request_identity":"A venue request identifier inside input JSON is separate from the logical --id value."
            }),
        ),
        definition::<ReservationOperationInput>(
            "operations.recovery",
            "Explain the original operation's blockers and supported recovery actions without changing it.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({"operation_id":"original-operation-id"}),
            json!({"command_line_template":"gofin operations recovery OPERATION_ID --json","read_only":true,"returns":"Original outcome, human labels and structured observation or continuation guidance. No dispatch or approval occurs."}),
        ),
        definition::<OperationReceiptRecoveryInput>(
            "operations.recover",
            "Independently verify a supplied reference against the original submission, without resubmitting it.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({"operation_id":"original-operation-id","reference":{"kind":"venue_reference","reference":"original-venue-reference"}}),
            json!({"command_line_template":"gofin operations recover OPERATION_ID --input REFERENCE_JSON --json","input_json_schema":schema::<gofin_domain::operation_recovery::RecoveryReference>(),"authority":"Read-only evidence recovery; caller references are selectors, not proof. The adapter must correlate original submission, destination, asset and receipt. Unsupported or inconsistent references return recovery guidance; never submit a replacement financial action."}),
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
        definition::<gofin_domain::SnapshotScope>(
            "snapshot",
            "Capture selected broker-observed resources, networks and assets with explicit coverage and incomplete observations.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({}),
            json!({"client_supplied_state":false,"service_required":true,"scope":"Optional resources, networks and exact assets intersect. Empty selectors request the full portfolio. Related exposure and native fees remain included.","completeness":"Complete is relative to declared coverage, never an exhaustive portfolio claim. Observer failures and timeouts return partial state with complete=false and identify missing coverage; incomplete evidence cannot authorize financial execution.","command_line_examples":["gofin snapshot --input '{\"networks\":[\"ethereum:8453\"]}' --json"]}),
        ),
        definition::<EmptyArgs>(
            "setup",
            "Install the bundled protected broker and native approval helper, then start and check the service.",
            OperationKind::Configuration,
            ExecutionActor::HumanInteractive,
            json!({}),
            json!({"requires_human_terminal":true,"requires_administrator":true,"requires_fresh_touch_id":false,"authorization_source":"sudo","administrator_authentication":administrator_authentication.clone(),"financial_movement":false,"secret_input":"none","shell_completion":{"automatic":true,"selection":"configured human account login shell and home from account database","supported":["zsh","bash","fish"],"activation":"new shell","unsupported":"brief notice; setup continues","cleanup":"GoFin-owned files and exact marked blocks only; preserve user edits"},"financial_limits":{"input":"protected human terminal prompts before exact configuration confirmation","default_usd":"100","zero":"no personal limit for that field","later_change":"gofin policy set"},"installs":["one public gofin CLI","protected internal broker","bundled native OS authorization helper","private credential/configuration/journal storage","macOS PATH registration in /etc/paths.d/gofin"],"command_availability":"Open a new local or SSH login shell after setup to use gofin. Existing shells can run: export PATH=\"/Library/GoFin/runtime:$PATH\". Custom PATH overrides must retain this directory.","boundary":"Installation authorizes no financial action. Run sudo -K to clear all cached sudo credentials, close elevated shells, and verify the agent lacks root access. This does not change financial authorization requirements or enable Yolo.","next_check":"gofin daemon status --json"}),
        ),
        definition::<EmptyArgs>(
            "update",
            "Update an installed macOS GoFin runtime to the latest completed release while preserving protected state.",
            OperationKind::Configuration,
            ExecutionActor::HumanInteractive,
            json!({}),
            json!({
                "command_line_example":"gofin update",
                "requires_human_terminal":true,
                "requires_administrator":true,
                "platform":"macOS protected installation only; unavailable in development environments and Linux discovery bundles",
                "machine_output":"--json returns HUMAN_TERMINAL_REQUIRED before network, privilege escalation or installation",
                "financial_movement":false,
                "future_financial_authority":false,
                "secret_input":"none",
                "release_selection":"Latest completed GoFin release (stable, beta or RC) by publication time from the public Pages latest-beta.json feed; no version override or automatic downgrade",
                "verification":"HTTPS release download, outer checksum manifest, bundle manifest, release identity and existing protected runtime checks",
                "preserves":["connections","credentials","historical settings","financial history","pending operation identities","historical approval bindings"],
                "journal_schema":"Versioned transactional migration preserves original IDs and evidence. Protected update review discloses retired limits and acknowledges preservation of enabled Yolo; unsupported schemas fail closed.",
                "recovery":"Retains a verified forward-recovery bundle and durable update markers. Interrupted replacement resumes without resetting history or replaying actions; old binaries are not restored after the new schema may have been opened.",
                "older_installation":"If gofin update is unavailable, use reviewed local make update from an updated source checkout once; the curl bootstrap refuses existing installations",
                "automatic_installation":false,
                "notification":"The installed CLI may show a cached newer-release notice after successful interactive human commands. Background metadata refresh is limited to once every 24 hours including failures, does not wait on HTTP in the foreground and has no effect on command success; JSON, redirected output, completions and development commands have no notices. GOFIN_NO_UPDATE_CHECK disables automatic checks and notices, not explicit updates."
            }),
        ),
        definition::<AddConnectionArgs>(
            "connections.add",
            "Add a named connection with private human terminal enrollment.",
            OperationKind::Configuration,
            ExecutionActor::HumanInteractive,
            json!({"provider":"binance","name":"main","role":"main","credential_type":"rsa"}),
            enrollment.clone(),
        ),
        definition::<GenerateConnectionArgs>(
            "connections.generate",
            "Generate and store a named wallet inside the protected broker from Apple Terminal or a human-operated macOS OpenSSH session.",
            OperationKind::Configuration,
            ExecutionActor::HumanInteractive,
            json!({"network":"evm","name":"wallet"}),
            json!({
                "command_line_examples":["gofin connections generate evm wallet", "gofin connections generate solana sol-wallet", "gofin connections generate evm --name wallet"],
                "requires_human_terminal":true,
                "requires_administrator":true,
                "requires_fresh_touch_id":false,
                "authorization_source":"sudo",
                "configuration_authorization":"Fresh sudo and exact protected terminal confirmation approve the generated connection; no financial authority is granted.",
                "administrator_authentication":administrator_authentication.clone(),
                "machine_output":"--json returns HUMAN_TERMINAL_REQUIRED before privilege escalation or key generation",
                "secret_input":"none",
                "key_generation":"OS randomness and wallet key generation occur exclusively inside the protected broker; no mnemonic is created or returned",
                "storage":"Existing protected wallet credential storage; private keys never transit the CLI",
                "network":"Wallet key family: evm or solana; EVM wallets are reusable across reviewed EVM chains",
                "checks":"Local key validity and configuration checks only; no public RPC request",
                "failure":"Duplicate names are refused; cancellation and failed saves follow existing protected enrollment rollback and uncertain-save recovery",
                "result":"Human success shows the saved wallet name, network family and signer-derived public address; private keys and credential references are never returned",
                "financial_movement":false,
                "future_financial_authority":false,
                "next_check":"gofin connections list --json"
            }),
        ),
        definition::<ConfigurationCommandInput>(
            "configure",
            "Review or execute a human-interactive venue configuration operation. Actual requests require fresh sudo and exact protected terminal confirmation, locally or over macOS OpenSSH.",
            OperationKind::Configuration,
            ExecutionActor::HumanInteractive,
            json!({"operation":"hyperliquid.account.set_agent_abstraction","input":{"account":"hl-main","enabled":true},"request_id":"SYNTHETIC-config-change-1"}),
            json!({
                "command_line_examples":[
                    "gofin configure hyperliquid.account.set_agent_abstraction --input '{\"account\":\"hl-main\",\"enabled\":true}' --dry-run --json",
                    "gofin configure hyperliquid.account.set_agent_abstraction --input '{\"account\":\"hl-main\",\"enabled\":true}' --id CONFIG_REQUEST_ID",
                    "gofin configure krakenfutures.set-self-trade-strategy --input '{\"account\":\"derivatives\",\"strategy\":\"REJECT_TAKER\"}' --id CONFIG_REQUEST_ID"
                ],
                "human_only":true,
                "dry_run":{"broker_method":"configuration_preview","read_only":true,"json_allowed":true},
                "actual":{"requires_human_terminal":true,"requires_administrator":true,"requires_fresh_touch_id":false,"authorization_source":"sudo","administrator_authentication":administrator_authentication.clone(),"json":"returns HUMAN_TERMINAL_REQUIRED before protected access","request_id":"stable operation identity for duplicate recovery; generated and returned when omitted","run_id":null,"financial_plan":false},
                "allowed_operations":"Only exact definitions marked OperationKind::Configuration and ExecutionActor::HumanInteractive can pass the protected configuration route. Hyperliquid and Kraken product admin operations are listed individually in capabilities."
            }),
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
            json!({"requires_human_terminal":true,"requires_administrator":true,"requires_fresh_touch_id":false,"authorization_source":"sudo","administrator_authentication":administrator_authentication,"secret_input":"none","financial_movement":false,"pending_work":"Refuse while an unresolved operation needs this physical resource or credential.","next_check":"gofin connections list --json"}),
        ),
        definition::<EmptyArgs>(
            "policy.show",
            "Show human-owned financial limits, the unpriced exchange setting, included costs and durable allocation scope.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({}),
            json!({"financial_movement":false,"command_line_examples":["gofin policy show", "gofin policy show --json"]}),
        ),
        definition::<EmptyArgs>(
            "yolo.status",
            "Read the authoritative protected Yolo state; offline discovery reports the state as unknown.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({}),
            json!({"financial_movement":false,"source":"protected broker IPC","offline_state":"unknown","command_line_examples":["gofin yolo status", "gofin yolo status --json"]}),
        ),
        definition::<EmptyArgs>(
            "yolo.on",
            "Enable agent-submitted financial actions under their native terms after fresh sudo and explicit confirmation in Apple Terminal or a human-operated macOS OpenSSH session.",
            OperationKind::Configuration,
            ExecutionActor::HumanInteractive,
            json!({}),
            json!({"requires_human_terminal":true,"requires_administrator":true,"requires_fresh_sudo":true,"requires_explicit_confirmation":true,"requires_fresh_touch_id":false,"financial_movement":false,"future_financial_authority":true,"command_line_examples":["gofin yolo on"],"confirmation":"Prompt `Enable Yolo mode? [y/N]`; y or Y accepts immediately without Enter; other keys decline.","risk":"Agents can submit trades, transfers, withdrawals, bridges, supplies, collateral changes, borrowing and repayments without further approval. No personal USD limits apply; each action is bounded by its submitted native terms. Funds may be lost."}),
        ),
        definition::<EmptyArgs>(
            "yolo.off",
            "Disable new Yolo admissions in Apple Terminal or a human-operated macOS OpenSSH session after fresh sudo; no confirmation or Touch ID is requested.",
            OperationKind::Configuration,
            ExecutionActor::HumanInteractive,
            json!({}),
            json!({"requires_human_terminal":true,"requires_administrator":true,"requires_fresh_sudo":true,"requires_explicit_confirmation":false,"requires_fresh_touch_id":false,"financial_movement":false,"future_financial_authority":false,"command_line_examples":["gofin yolo off"],"effect":"Blocks new Yolo admissions; already admitted work may finish."}),
        ),
        definition::<EmptyArgs>(
            "connections.list",
            "Read safe named connection metadata; no keys are returned.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            json!({}),
            json!({"secret_input":"none","financial_movement":false,"alias":"gofin connections --json","next_check":"gofin capabilities --json"}),
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
            json!({"secret_input":"none","financial_movement":false,"broker_activation":false,"source":"launchd service state; no broker IPC","readiness":"Use gofin doctor for configuration and readiness diagnostics."}),
        ),
        definition::<EmptyArgs>(
            "daemon.stop",
            "Stop gracefully and remain stopped until explicit start, preserving submitted identities and historical evidence.",
            OperationKind::Lifecycle,
            ExecutionActor::AgentInvocable,
            json!({}),
            json!({"secret_input":"none","journal":"Pending outcomes remain unresolved across restart; stop performs no financial cleanup.","restart":"Only explicit gofin daemon start clears protected stop intent; ordinary commands cannot reactivate it."}),
        ),
    ];
    for (name, description) in [
        (
            "analytics.enable",
            "Enable anonymous usage and error reporting for this account.",
        ),
        (
            "analytics.disable",
            "Disable usage, error, install/update and broker panic reporting for this account.",
        ),
        (
            "analytics.status",
            "Show the local analytics preference and this build's reporting eligibility.",
        ),
    ] {
        let mut command = definition::<EmptyArgs>(
            name,
            description,
            OperationKind::Lifecycle,
            ExecutionActor::AgentInvocable,
            json!({}),
            json!({"service_required":false,"financial_authorization":false,"requires_administrator":false,
                "requires_fresh_touch_id":false,"network_access":false,"default_enabled":true,
                "persistent_identifiers":true,"preference":"~/.config/gofin/analytics.json",
                "installation_identity":{"path":"~/.config/gofin/analytics-identity.json","scope":"one OS account and GoFin home","schema_version":1},
                "command_line_example":format!("gofin {} --json", name.replace('.', " "))}),
        );
        command.output_schema = json!({"type":"object","required":["enabled","effective_enabled","reason","release_enabled","message"],
            "properties":{"enabled":{"type":"boolean"},"effective_enabled":{"type":"boolean"},"reason":{"type":"string"},
                "release_enabled":{"type":"boolean"},"message":{"type":"string"}}});
        definitions.push(command);
    }
    definitions.extend(companion_skill::definitions());
    let mut connection_schema = schema::<AddConnectionArgs>();
    connection_schema["$defs"]["Provider"]["enum"] = json!([
        "binance",
        "evm",
        "ethereum",
        "coingecko",
        "hyperliquid",
        "kraken",
        "krakenfutures",
        "solana",
        "jupiter",
        "relay",
        "coinbase",
        "coinbaseexchange",
        "coinbaseinternational"
    ]);
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
            "if":{"properties":{"provider":{"const":"hyperliquid"}},"required":["provider"]},
            "then":{
                "required":["credential_type","address"],
                "properties":{"credential_type":{"enum":["private_key","read_only"]},"address":{"type":"string","pattern":"^0x[0-9a-fA-F]{40}$"}},
                "not":{"anyOf":[{"required":["role"]},{"required":["parent"]},{"required":["email"]},{"required":["network"]},{"required":["api_plan"]}]}
            }
        },
        {
            "if": {"properties":{"provider":{"enum":["evm","ethereum"]}},"required":["provider"]},
            "then": {
                "required":["credential_type"],
                "properties":{"network":{"enum":connection_network_enum},"credential_type":{"enum":["private_key","read_only"]}},
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
        },
        {
            "if":{"properties":{"provider":{"const":"solana"}},"required":["provider"]},
            "then":{
                "required":["credential_type"],
                "properties":{"credential_type":{"enum":["private_key","read_only"]},"network":{"const":"solana:mainnet-beta"},"address":{"type":"string","maxLength":44}},
                "not":{"anyOf":[{"required":["role"]},{"required":["parent"]},{"required":["email"]},{"required":["api_plan"]}]},
                "oneOf":[
                    {"properties":{"credential_type":{"const":"private_key"}}},
                    {"properties":{"credential_type":{"const":"read_only"}},"required":["address"]}
                ]
            }
        },
        {
            "if":{"properties":{"provider":{"enum":["jupiter","relay"]}},"required":["provider"]},
            "then":{
                "required":["credential_type"],
                "properties":{"credential_type":{"const":"api_key"}},
                "not":{"anyOf":[{"required":["role"]},{"required":["parent"]},{"required":["email"]},{"required":["network"]},{"required":["address"]},{"required":["api_plan"]}]}
            }
        }
    ]);
    connection_schema["allOf"].as_array_mut().expect("conditions").push(json!({
        "if":{"properties":{"provider":{"enum":["kraken","krakenfutures"]}},"required":["provider"]},
        "then":{"required":["account_id"],"properties":{"account_id":{"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_-]+$"},"credential_type":{"enum":["hmac",null]}},"not":{"anyOf":[{"required":["role"]},{"required":["parent"]},{"required":["email"]},{"required":["network"]},{"required":["address"]},{"required":["api_plan"]}]}},
        "else":{"properties":{"account_id":{"type":"null"}}}
    }));
    connection_schema["allOf"].as_array_mut().expect("conditions").push(json!({
        "if":{"properties":{"provider":{"const":"kraken"}},"required":["provider"]},
        "then":{"properties":{"default_wallet_account_id":{"type":["string","null"],"minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_-]+$"}}},
        "else":{"properties":{"default_wallet_account_id":{"type":"null"}}}
    }));
    connection_schema["allOf"].as_array_mut().expect("conditions").push(json!({
        "if":{"properties":{"provider":{"const":"coinbase"}},"required":["provider"]},
        "then":{"required":["credential_type"],"properties":{"credential_type":{"enum":["ecdsa","ed25519"]}},"not":{"anyOf":[{"required":["role"]},{"required":["parent"]},{"required":["email"]},{"required":["network"]},{"required":["address"]},{"required":["account_id"]},{"required":["api_plan"]},{"required":["default_wallet_account_id"]},{"required":["hyperliquid_scope"]},{"required":["execution_address"]}]}}
    }));
    connection_schema["allOf"].as_array_mut().expect("conditions").push(json!({
        "if":{"properties":{"provider":{"enum":["coinbaseexchange","coinbaseinternational"]}},"required":["provider"]},
        "then":{"required":["credential_type"],"properties":{"credential_type":{"const":"hmac"}},"not":{"anyOf":[{"required":["role"]},{"required":["parent"]},{"required":["email"]},{"required":["network"]},{"required":["address"]},{"required":["account_id"]},{"required":["api_plan"]},{"required":["default_wallet_account_id"]},{"required":["hyperliquid_scope"]},{"required":["execution_address"]}]}}
    }));
    connection_schema["allOf"].as_array_mut().expect("conditions").push(json!({
        "if":{"properties":{"provider":{"enum":["coinbase","coinbaseexchange","coinbaseinternational"]}},"required":["provider"]},
        "then":{"properties":{"portfolio_id":{"type":["string","null"],"minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_-]+$"}}},
        "else":{"properties":{"portfolio_id":{"type":"null"}}}
    }));
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
                "installation":"Use arrow keys to move and Space to toggle Codex, Claude Code, Hermes, OpenClaw and Grok Bot; Enter prepares selected copies as the human account. Grok Bot prepares a local export for manual import into its private skill library. Nothing is preselected. Enter with none selected, Esc or EOF installs nothing; conflicts are preserved without automatic backup or replacement.",
                "failure":"Optional skill failure or cancellation leaves the completed GoFin installation intact."
            });
        }
        if item.name == "capabilities" {
            item.semantics["companion_skill"] = companion_skill::metadata();
        }
        if matches!(item.name.as_str(), "policy.show" | "policy.set") {
            item.output_schema = schema::<PolicyView>();
        } else if item.name == "yolo.status" {
            item.output_schema = json!({
                "type":"object",
                "required":["enabled","state","authorization_source","expires_at"],
                "properties":{
                    "enabled":{"type":"boolean"},
                    "state":{"type":"string","enum":["enabled","disabled","unknown"]},
                    "authorization_source":{"type":["string","null"]},
                    "expires_at":{"type":["string","null"]}
                }
            });
        } else if matches!(item.name.as_str(), "connections.add" | "connections.update") {
            item.input_schema = connection_schema.clone();
            item.output_schema = schema::<enrollment_contract::EnrollmentOutcome>();
            item.semantics["financial_movement"] = json!(false);
            item.semantics["future_financial_authority"] = json!(false);
        } else if item.name == "connections.generate" {
            item.input_schema["properties"]["name"]["pattern"] = json!("^[a-z][a-z0-9_-]{0,63}$");
            item.output_schema = schema::<enrollment_contract::EnrollmentOutcome>();
        } else if item.name == "connections.remove" {
            item.output_schema = schema::<enrollment_contract::EnrollmentOutcome>();
            item.semantics["financial_movement"] = json!(false);
            item.semantics["future_financial_authority"] = json!(false);
        } else if item.name == "snapshot" {
            item.output_schema = schema::<gofin_domain::StateSnapshot>();
        } else if item.name == "plans.preview" {
            item.output_schema = schema::<gofin_domain::financial_plans::FinancialPlanReview>();
        } else if matches!(
            item.name.as_str(),
            "plans.authorize" | "plans.status" | "plans.cancel"
        ) {
            item.output_schema = schema::<gofin_domain::financial_plans::FinancialPlanStatus>();
        } else if item.name == "execute.plan-step" {
            item.output_schema = schema::<gofin_domain::OperationRecord>();
        }
    }
    definitions.retain(|item| {
        !item.name.starts_with("plans.")
            && !item.name.starts_with("authorizations.")
            && !item.name.starts_with("reservations.")
            && !matches!(
                item.name.as_str(),
                "policy.set"
                    | "execute.boundary"
                    | "execute.plan-step"
                    | "operations.force-resolve"
                    | "operations.resolve-preview"
            )
    });
    for item in &mut definitions {
        match item.name.as_str() {
            "status" => {
                item.input_schema = schema::<StatusInput>();
                item.output_schema = operation_view_schema();
                item.semantics = json!({"read_only":true,"bounded":true,"wait":"Poll until phase changes or wait expires", "side":"source or destination, using persisted transfer identities","outcome":schema::<gofin_domain::outcome::OperationOutcome>()});
            }
            "operations" => {
                item.input_schema = schema::<OperationsInput>();
                let mut operation = operation_view_schema();
                let definitions = operation
                    .as_object_mut()
                    .and_then(|o| o.remove("$defs"))
                    .unwrap_or_else(|| json!({}));
                item.output_schema = json!({"type":"array","items":operation,"$defs":definitions});
                item.semantics = json!({"read_only":true,"open":"Include unfinished outcomes and unresolved transfers, excluding human-dismissed records"});
            }
            "execute" => {
                if let Some(semantics) = item.semantics.as_object_mut() {
                    semantics.remove("allocation_scope");
                }
                item.semantics["dry_run"] = json!({"flag":"--dry-run","financial_movement":false,"financial_authorization":false,"journal_write":false,"execution":"Remove --dry-run and supply --id to request native economic approval. The broker validates every exact candidate independently."});
            }
            "setup" => {
                if let Some(semantics) = item.semantics.as_object_mut() {
                    semantics.remove("financial_limits");
                }
            }
            "policy.show" => {
                item.description = "Inspect retired personal policy settings.".into();
                item.output_schema = json!({"type":"object","required":["retired","personal_limits_enforced","retired_settings"]});
                item.semantics =
                    json!({"read_only":true,"retired":true,"financial_movement":false});
            }
            "yolo.on" => {
                item.description = "Enable agent financial actions under their chosen native terms; no personal USD limits.".into();
            }
            _ => {}
        }
    }
    definitions.push(definition::<IdInput>("operations.dismiss",
        "Human-only dismissal label for an unconfirmed operation; no financial facts change.",
        OperationKind::Configuration, ExecutionActor::HumanInteractive,
        json!({"id":"original-operation-id"}),
        json!({"human_terminal_required":true,"financial_movement":false,"replay_safe_unchanged":true,
            "command_line_template":"gofin operations dismiss ID"})));
    definitions.push(definition::<ContinueInput>("operations.continue",
        "Resume ready approved stages of the original action with a broker-issued token.",
        OperationKind::Write, ExecutionActor::AgentInvocable,
        json!({"id":"original-operation-id","token":"broker-issued-token"}),
        json!({"idempotent":true,"same_identity":true,"command_line_template":"gofin operations continue ID --token TOKEN --json","authority":"Original native terms and explicit compound deadline; submitted failed stages are never automatically retried"})));
    definitions.push(definition::<ReauthorizeInput>("operations.reauthorize",
        "Approve explicit revised native terms for unfinished work on the original logical action; completed stages and spent fees remain recorded.",
        OperationKind::Write, ExecutionActor::AgentInvocable,
        json!({"id":"original-operation-id","token":"broker-issued-token","input":{}}),
        json!({"idempotent":true,"same_identity":true,"fresh_review":true,"command_line_template":"gofin operations reauthorize ID --token TOKEN --input JSON --json","authority":"Explicit replacement native terms and deadline; completed stages cannot be re-granted"})));
    for item in &mut definitions {
        if matches!(
            item.name.as_str(),
            "operations.continue" | "operations.reauthorize"
        ) {
            item.output_schema = operation_view_schema();
        }
    }
    definitions
}

/// Library reach is deliberately separate from GoFin's reviewed named operations.
pub fn exchange_capabilities() -> serde_json::Value {
    let mut value = serde_json::json!({
        "library": {"name":"ccxt-base", "version":"4.5.85", "source":"https://github.com/ccxt/ccxt", "compiled_exchanges":["binance","kraken","krakenfutures","hyperliquid","okx","coinbase","coinbaseexchange","coinbaseinternational","bybit"], "all_library_methods_exposed":false},
        "gofin": {"binance": {"scope":"SPOT funding, account discovery, trading reads and bounded SPOT/USDT perpetual orders", "authentication":["hmac","rsa","ed25519"], "market_data":{"operations":["exchange.markets","exchange.ticker","exchange.order-book"],"market_types":["spot","perpetual"],"maximum_depth":100}, "spot":{"execution":"Native order modes bind input, schema and protected connection identity", "ioc_partial_fills":"Original order and fill evidence preserves executed amounts and the live remainder"}, "futures":{"execution":"USD-M USDT perpetual LIMIT IOC/FOK", "margin":"explicit cross or isolated margin and leverage; one-way positions, automatic additions disabled", "configuration_changes":true,"funding":"explicit same-account USDT spot/futures transfers","concurrent_positions_per_account":1}, "cancellation":"New approval for the exact original unresolved GoFin order; original identity persists through fill observation", "deposit_address_creation":false, "withdrawal_whitelist_editing":false,
            "address_management":{"reads":["binance.deposit-address","binance.deposit-addresses","binance.whitelist"],"mutations":"No documented public Binance endpoint for new-address creation or whitelist add/remove; human Binance website/app action required.","deposit_creation":{"human_action":"Use Binance Deposit Crypto, select the coin/network and claim a new address when available.","recheck":"gofin read binance.deposit-addresses --input JSON --json"},"whitelist_editing":{"human_action":"Use Binance withdrawal Address Management to add/remove or change whitelist entries; complete Binance security checks.","recheck":"gofin read binance.whitelist --input JSON --json"},"whitelist_limits":"Per-address flags only; account-wide enforcement and activation/cooldown are not proved."}, "operations":"See the explicit binance.* entries in operations; other CCXT methods are unavailable."}},
        "verification": {"automated":"synthetic transport and lifecycle fixtures", "authenticated_live":"not_run_for_this_refactor", "human_onboarding":"pending", "touch_id":"pending"}
    });
    value["gofin"]["binance"]["deposits"] = serde_json::json!({
        "operation": "binance.deposit",
        "accounts": "main or subaccount, using the receiving account's own credential; master enrollment is optional",
        "discovery": "binance.networks and binance.deposit-address resolve current coin/network availability, minimum and exact contract",
        "source": "native ETH and standard ERC20 transfers on configured GoFin EVM networks with a reviewed Binance network mapping",
        "unsupported": "other source chains, memo/tag routes and non-unit denominations; Binance metadata availability alone does not establish GoFin source-chain execution",
        "approval": "fresh exact approval binds wallet, destination account, chain, asset, address, amount and fee cap",
        "completion": "canonical chain evidence, matching Binance status 1 or 6 with clear travel-rule status and exact free-balance increase; status 6 may retain a Binance withdrawal hold",
        "broker_generated_attempt_input_fields": ["/requestId"],
        "request_id_assignment": "The required requestId remains in the reviewed plan template, but GoFin assigns a stable unique venue requestId for each concrete attempt and shows this field as GoFin-assigned in human review."
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
        value["gofin"][venue] = serde_json::json!({"operations":["exchange.markets","exchange.ticker","exchange.order-book"],"market_types":[market_type],"maximum_order_book_depth":100,"scope":scope,"public_only":true,"account_enrollment":false,"writes":false});
    }
    value["gofin"]["kraken"] = serde_json::json!({
        "operations":["exchange.markets","exchange.ticker","exchange.order-book"],
        "operation_prefix":"kraken.","operations_catalog":"gofin capabilities --json operations",
        "market_types":["spot","margin"],"maximum_order_book_depth":100,
        "scope":"Typed Spot/Margin REST, current and legacy funding, Earn, administration and exports",
        "withdrawal_discovery":{"preferred_api":"funding-v1","venue_api_status":"Funding Beta in the pinned official schema","methods":"kraken.list-funding-methods-v1 (direction=withdraw)","saved_addresses":"kraken.list-funding-addresses-v1","new_withdrawal":"kraken.create-funding-withdrawal-v1","selection":"Resolve the exact verified saved address and method/network scope; traverse pagination. An absent legacy key does not prove absence from current Funding.","legacy":"Explicit legacy operations remain available; pending submissions retain their original API and identity without automatic translation."},
        "public_only":false,"account_enrollment":true,"writes":true,
        "authentication":["product-specific API key and Base64 HMAC secret"],
        "administration":"gofin configure; separate protected human configuration approval, never financial plans",
        "financial_authorization":"Protected preview/execute/status; native per-action approval or explicitly enabled protected Yolo",
        "exclusions":["WebSockets/token issuance","API-level 2FA","assignment-program enrollment"],
        "financial_limitations":["Tokenized base/rebased ticker denomination unproven","Attached conditional parent amendments unavailable at venue","Issued-child principal expansion beyond original generated-credit ceiling incomplete","VIP credit-line liability coverage incomplete"],
        "verification":"Synthetic native lifecycle and independent request mapping; live private/write and native Touch ID unrun"
    });
    value["gofin"]["krakenfutures"] = serde_json::json!({
        "operation_prefix":"krakenfutures.","operations_catalog":"gofin capabilities --json operations",
        "market_types":["perpetual","future"],"scope":"Typed Derivatives REST, contract/collateral accounting, wallet/subaccount transfers, histories, administration and CSV exports",
        "public_only":false,"account_enrollment":true,"writes":true,
        "authentication":["separate product-specific API key and Base64 secret; optional venue nonce omitted"],
        "administration":"gofin configure; separate protected human configuration approval, never financial plans",
        "financial_authorization":"Protected preview/execute/status; linked authority revisions require fresh exact approval",
        "financial_limitations":["Option markets unsupported by pinned CCXT client","Non-USD converted fee legs incomplete","Deficit repayment/conversion incomplete","Preferred FEE-credit spending incomplete","Other negative currency liabilities incomplete"],
        "verification":"Synthetic native lifecycle and independent request mapping; live private/write and native Touch ID unrun"
    });
    value["gofin"]["coinbase"] = serde_json::json!({
        "operation_prefix":"coinbase.","operations_catalog":"gofin capabilities --json operations",
        "public_only":false,"account_enrollment":true,"writes":true,"authentication":["cdp_ed25519","cdp_ecdsa"],
        "scope":"Typed App/Advanced REST reads and supported Advanced spot orders; native GET routes expose closed query subsets",
        "market_data":{"operations":["exchange.markets","exchange.ticker","exchange.order-book"],"market_types":["spot"]},
        "financial_authorization":"Native per-action approval or protected Yolo; independent dispatch validation",
        "funding":"Base native USDC composite deposit requires verified existing address, original source receipt and correlated usable credit",
        "financial_limitations":["Attached and scaled order accounting incomplete","Full derivatives/loans/staking snapshots incomplete","Additional write families remain under implementation"],
        "verification":"Synthetic signing, financial and CLI/broker workflow fixtures; live-read evidence only; native approval unobserved"
    });
    for provider in ["coinbaseexchange", "coinbaseinternational"] {
        value["gofin"][provider] = serde_json::json!({"operation_prefix":format!("{provider}."),"public_only":false,"account_enrollment":true,"authentication":["hmac"],"scope":"Typed product-specific REST reads with closed query subsets","writes":false,"verification":"Synthetic transport/contracts; live authentication and full write lifecycles remain unverified"});
    }
    value["gofin"]["coinbaseexchange"]["writes"] = serde_json::json!(true);
    value["gofin"]["coinbaseexchange"]["scope"] = serde_json::json!(
        "Typed REST reads, bounded spot limit orders and linked single-order cancellation; other write families remain incomplete"
    );
    value["gofin"]["coinbaseinternational"]["writes"] = serde_json::json!(true);
    value["gofin"]["coinbaseinternational"]["scope"] = serde_json::json!(
        "Typed REST reads, bounded SPOT limit orders, linked price amendment and single-order cancellation; other write families remain incomplete"
    );
    for provider in ["coinbaseexchange", "coinbaseinternational"] {
        value["gofin"][provider]["verification"] = serde_json::json!(
            "Synthetic transport and native order lifecycle fixtures; institutional live writes and native approval remain unverified"
        );
    }
    value
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
    fn wallet_generation_metadata_is_strict_and_discovery_matches_protected_contract() {
        for network in ["evm", "solana"] {
            let input = json!({"network":network,"name":"wallet-1"});
            let args: GenerateConnectionArgs = serde_json::from_value(input.clone()).unwrap();
            validate_name(&args.name).unwrap();
            assert_eq!(serde_json::to_value(args).unwrap(), input);
            for field in [
                "private_key",
                "seed",
                "mnemonic",
                "derivation_path",
                "address",
                "provider",
                "authorization",
                "credential_type",
            ] {
                let mut unknown = input.clone();
                unknown[field] = json!("synthetic");
                assert!(serde_json::from_value::<GenerateConnectionArgs>(unknown).is_err());
            }
        }
        for network in [
            "stellar",
            "hedera",
            "ethereum",
            "EVM",
            "solana:mainnet-beta",
        ] {
            assert!(
                serde_json::from_value::<GenerateConnectionArgs>(
                    json!({"network":network,"name":"wallet"})
                )
                .is_err()
            );
        }
        let definition = operation_definitions()
            .into_iter()
            .find(|d| d.name == "connections.generate")
            .unwrap();
        assert_eq!(definition.kind, OperationKind::Configuration);
        assert_eq!(definition.execution_actor, ExecutionActor::HumanInteractive);
        assert_eq!(definition.input_schema["additionalProperties"], false);
        assert_eq!(
            definition.input_schema["properties"]["name"]["pattern"],
            "^[a-z][a-z0-9_-]{0,63}$"
        );
        assert_eq!(
            definition.input_schema["$defs"]["WalletNetwork"]["enum"],
            json!(["evm", "solana"])
        );
        assert_eq!(
            definition.output_schema,
            schema::<enrollment_contract::EnrollmentOutcome>()
        );
        assert_eq!(definition.semantics["requires_human_terminal"], true);
        assert_eq!(definition.semantics["requires_fresh_touch_id"], false);
        assert_eq!(definition.semantics["authorization_source"], "sudo");
        assert_eq!(
            definition.semantics["administrator_authentication"]["requires_fresh_sudo"],
            true
        );
        assert!(
            definition.semantics["administrator_authentication"]["supported_terminals"]
                .as_array()
                .unwrap()
                .iter()
                .any(|terminal| terminal.as_str().unwrap().contains("OpenSSH"))
        );
        assert_eq!(definition.semantics["financial_movement"], false);
        assert_eq!(definition.semantics["future_financial_authority"], false);
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
                "gofin",
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
    fn hyperliquid_delegated_scope_is_explicit_and_cannot_collect_a_new_key() {
        use clap::Parser;
        let args = Cli::try_parse_from([
            "gofin",
            "--provider",
            "hyperliquid",
            "--name",
            "hl-sub",
            "--address",
            "0x1111111111111111111111111111111111111111",
            "--hyperliquid-scope",
            "subaccount",
            "--execution-address",
            "0x2222222222222222222222222222222222222222",
        ])
        .unwrap();
        validate_add(&args.add).unwrap();

        let mut missing_target = args.add.clone();
        missing_target.execution_address = None;
        assert!(validate_add(&missing_target).is_err());
        let mut new_key = args.add;
        new_key.credential_type = Some(CredentialType::PrivateKey);
        assert!(validate_add(&new_key).is_err());
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
                "gofin",
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
            for network in [
                "ethereum:1",
                "ethereum:8453",
                "ethereum:42161",
                "ethereum:4663",
                "ethereum:10",
                "ethereum:137",
                "ethereum:43114",
                "ethereum:5042",
                "ethereum:999",
            ] {
                args.network = Some(network.into());
                validate_add(&args).unwrap();
            }
            args.network = Some("ethereum:9999".into());
            assert!(validate_add(&args).is_err());
        }
    }

    #[test]
    fn hyperliquid_enrollment_requires_explicit_owner_and_safe_signing_choice() {
        let mut input = json!({
            "provider":"hyperliquid",
            "name":"hl-main",
            "credential_type":"private_key",
            "address":"0x1111111111111111111111111111111111111111"
        });
        let args: AddConnectionArgs = serde_json::from_value(input.clone()).unwrap();
        validate_add(&args).unwrap();
        assert_eq!(
            serde_json::to_value(args).unwrap()["provider"],
            "hyperliquid"
        );
        for forbidden in ["role", "parent", "email", "network", "api_plan"] {
            input[forbidden] = match forbidden {
                "role" => json!("main"),
                "network" => json!("ethereum:1"),
                "api_plan" => json!("demo"),
                _ => json!("synthetic"),
            };
            let args: AddConnectionArgs = serde_json::from_value(input.clone()).unwrap();
            assert!(validate_add(&args).is_err(), "accepted {forbidden}");
            input.as_object_mut().unwrap().remove(forbidden);
        }
        input.as_object_mut().unwrap().remove("address");
        let args: AddConnectionArgs = serde_json::from_value(input.clone()).unwrap();
        assert!(validate_add(&args).is_err());
        input["address"] = json!("0x0000000000000000000000000000000000000000");
        let args: AddConnectionArgs = serde_json::from_value(input.clone()).unwrap();
        assert!(validate_add(&args).is_err());
        input["address"] = json!("0x1111111111111111111111111111111111111111");
        input["credential_type"] = json!("read_only");
        let args: AddConnectionArgs = serde_json::from_value(input).unwrap();
        validate_add(&args).unwrap();
    }

    #[test]
    fn solana_and_jupiter_enrollment_contracts_are_scoped_and_secret_safe() {
        let mut solana = main_args();
        solana.provider = Provider::Solana;
        solana.role = None;
        solana.credential_type = Some(CredentialType::ReadOnly);
        solana.address = Some("11111111111111111111111111111111".into());
        validate_add(&solana).unwrap();
        solana.credential_type = Some(CredentialType::PrivateKey);
        solana.address = None;
        validate_add(&solana).unwrap();
        solana.address = Some("0x1111111111111111111111111111111111111111".into());
        assert!(validate_add(&solana).is_err());
        solana.address = Some("11111111111111111111111111111111".into());
        solana.network = Some("solana:devnet".into());
        assert!(validate_add(&solana).is_err());

        let mut jupiter = main_args();
        jupiter.provider = Provider::Jupiter;
        jupiter.role = None;
        jupiter.credential_type = Some(CredentialType::ApiKey);
        validate_add(&jupiter).unwrap();
        jupiter.address = Some("11111111111111111111111111111111".into());
        assert!(validate_add(&jupiter).is_err());

        let mut relay = main_args();
        relay.provider = Provider::Relay;
        relay.role = None;
        relay.credential_type = Some(CredentialType::ApiKey);
        validate_add(&relay).unwrap();
        relay.network = Some("solana:mainnet-beta".into());
        assert!(validate_add(&relay).is_err());

        let add = operation_definitions()
            .into_iter()
            .find(|definition| definition.name == "connections.add")
            .unwrap();
        for (provider, credential) in [
            ("solana", "private_key"),
            ("jupiter", "api_key"),
            ("relay", "api_key"),
        ] {
            assert!(
                add.input_schema["$defs"]["Provider"]["enum"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(provider))
            );
            let examples = add.semantics["command_line_examples"].as_array().unwrap();
            assert!(examples.iter().any(|example| {
                example.as_str().is_some_and(|text| {
                    text.contains(&format!("--provider {provider}"))
                        && text.contains(&format!("--credential-type {credential}"))
                })
            }));
        }
        let metadata = serde_json::to_string(&add.semantics).unwrap();
        assert!(metadata.contains("mainnet-beta"));
        assert!(metadata.contains("Keyless use is the default"));
        assert!(!metadata.to_ascii_lowercase().contains("api_key_value"));
        assert!(!metadata.contains("\"private_key\":"));
    }

    #[test]
    fn hyperliquid_schema_documents_owner_and_does_not_include_credentials() {
        use clap::Parser;
        let add = operation_definitions()
            .into_iter()
            .find(|definition| definition.name == "connections.add")
            .unwrap();
        assert!(
            add.input_schema["$defs"]["Provider"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("hyperliquid"))
        );
        let metadata = &add.semantics["hyperliquid"];
        assert_eq!(metadata["owner_required"], true);
        assert_eq!(metadata["enrollment_network_call"], true);
        assert_eq!(metadata["automatic_evm_link"]["supported"], true);
        assert_eq!(metadata["automatic_evm_link"]["credential_reentry"], false);
        assert_eq!(
            metadata["automatic_evm_link"]["separate_public_connection"],
            false
        );
        assert_eq!(metadata["automatic_evm_link"]["financial_authority"], false);
        assert!(metadata["execution_scopes"]["subaccount"].is_string());
        assert!(metadata["execution_scopes"]["vault"].is_string());
        let parsed = Cli::try_parse_from([
            "gofin",
            "--provider",
            "hyperliquid",
            "--name",
            "hl-main",
            "--credential-type",
            "read_only",
            "--address",
            "0x1111111111111111111111111111111111111111",
        ])
        .unwrap();
        validate_add(&parsed.add).unwrap();
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
                "gofin",
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
                "gofin",
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
    fn default_wallet_metadata_is_spot_only_exact_and_discoverable() {
        let mut input = json!({"provider":"kraken","name":"spot","account_id":"synthetic","default_wallet_account_id":"WDEFAULT"});
        validate_add(&serde_json::from_value(input.clone()).unwrap()).unwrap();
        for provider in [
            "krakenfutures",
            "binance",
            "evm",
            "coingecko",
            "hyperliquid",
        ] {
            input["provider"] = json!(provider);
            assert!(validate_add(&serde_json::from_value(input.clone()).unwrap()).is_err());
        }
        input["provider"] = json!("kraken");
        for id in ["", "bad/path", "hidden\n", &"a".repeat(129)] {
            input["default_wallet_account_id"] = json!(id);
            assert!(validate_add(&serde_json::from_value(input.clone()).unwrap()).is_err());
        }
        let definitions = operation_definitions();
        let schema = &definitions
            .iter()
            .find(|d| d.name == "connections.add")
            .unwrap()
            .input_schema;
        assert!(schema["properties"]["default_wallet_account_id"].is_object());
        let constraint = schema["allOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|condition| {
                condition["if"]["properties"]["provider"]["const"] == "kraken"
                    && condition["then"]["properties"]["default_wallet_account_id"].is_object()
            })
            .expect("Kraken default wallet constraint");
        assert_eq!(
            constraint["then"]["properties"]["default_wallet_account_id"]["pattern"],
            "^[A-Za-z0-9_-]+$"
        );
    }
}
