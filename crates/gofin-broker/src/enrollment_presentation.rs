//! Configuration review rendered from the exact challenged binding in the root broker.
use super::*;

fn safe(value: &str) -> String {
    value.chars().flat_map(|c| {
        if c.is_control() || matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
            c.escape_unicode().collect::<Vec<_>>()
        } else { vec![c] }
    }).collect()
}

pub(super) fn review(binding: &Value) -> Result<String> {
    if binding["financial_movement"] != false || binding["future_financial_authority"] != false {
        return Err(error(
            "INVALID_CHALLENGE",
            "Configuration review cannot approve financial authority.",
        ));
    }
    let action = binding["action"].as_str().unwrap_or("");
    let mut lines = Vec::new();
    if action == "configure" {
        let operation = binding["operation"]
            .as_str()
            .ok_or_else(|| error("INVALID_CHALLENGE", "Configuration operation is missing."))?;
        lines.push(format!(
            "Configure {} | Account: {}",
            safe(operation),
            safe(binding["account"].as_str().unwrap_or(""))
        ));
        lines.push(format!(
            "Product: {} | Physical account: {}",
            safe(binding["product"].as_str().unwrap_or("")),
            safe(binding["physical_identity"].as_str().unwrap_or(""))
        ));
        let fields = binding["parameters"]
            .as_object()
            .ok_or_else(|| error("INVALID_CHALLENGE", "Configuration parameters are missing."))?;
        for (name, value) in fields {
            lines.push(format!("{}: {}", safe(name), safe(&value.to_string())));
        }
        lines.push(safe(binding["warning"].as_str().unwrap_or("")));
        lines.push("Your terminal confirmation approves this exact venue configuration attempt. No financial plan can authorize it.".into());
    } else if action == "policy_set" {
        let previous: BrokerPolicy = serde_json::from_value(binding["previous_policy"].clone())
            .map_err(|_| error("INVALID_CHALLENGE", "Previous policy is invalid."))?;
        let next: BrokerPolicy = serde_json::from_value(binding["policy"].clone())
            .map_err(|_| error("INVALID_CHALLENGE", "Policy is invalid."))?;
        if previous.require_run_for_writes != next.require_run_for_writes
            || previous.preview_ttl_seconds != next.preview_ttl_seconds
        {
            return Err(error(
                "INVALID_CHALLENGE",
                "Only financial limits and the unpriced exchange exception may change.",
            ));
        }
        use gofin_domain::policy::{ALLOCATION_INCLUDES, MOVEMENT_INCLUDES, format_limit};
        lines.push(format!(
            "Movement limit: {} → {}",
            format_limit(&previous.max_movement_usd_micros)?,
            format_limit(&next.max_movement_usd_micros)?
        ));
        lines.push(MOVEMENT_INCLUDES.into());
        lines.push(format!(
            "24-hour allocation limit: {} → {}",
            format_limit(&previous.max_run_allocation_usd_micros)?,
            format_limit(&next.max_run_allocation_usd_micros)?
        ));
        lines.push(ALLOCATION_INCLUDES.into());
        lines.push(format!(
            "Unpriced exchange writes: {} → {}",
            if previous.allow_unpriced_exchange_assets {
                "Allowed"
            } else {
                "Blocked"
            },
            if next.allow_unpriced_exchange_assets {
                "Allowed"
            } else {
                "Blocked"
            }
        ));
        if next.allow_unpriced_exchange_assets {
            lines.push(gofin_domain::unpriced::WARNING.into());
        }
        lines.push("Your terminal confirmation saves this policy. Financial writes require fresh Touch ID, existing bounded authority, or explicitly enabled Yolo.".into());
    } else if action == "initialize" {
        let config: BrokerConfigV2 = serde_json::from_value(binding["configuration"].clone())
            .map_err(|_| {
                error(
                    "INVALID_CHALLENGE",
                    "Setup configuration could not be reviewed.",
                )
            })?;
        config.validate()?;
        let mut expected = setup_configuration(config.approver_uid, BrokerPolicy::beta());
        expected.policy.max_movement_usd_micros = config.policy.max_movement_usd_micros.clone();
        expected.policy.max_run_allocation_usd_micros =
            config.policy.max_run_allocation_usd_micros.clone();
        if serde_json::to_value(&config).ok() != serde_json::to_value(&expected).ok() {
            return Err(error(
                "INVALID_CHALLENGE",
                "This setup policy needs an explicit supported review.",
            ));
        }
        lines.extend([
            "Confirm this Mac account as GoFin's approver.".into(),
            format!(
                "Movement limit: {}",
                gofin_domain::policy::format_limit(&config.policy.max_movement_usd_micros)?
            ),
            gofin_domain::policy::MOVEMENT_INCLUDES.into(),
            format!(
                "24-hour allocation limit: {}",
                gofin_domain::policy::format_limit(&config.policy.max_run_allocation_usd_micros)?
            ),
            gofin_domain::policy::ALLOCATION_INCLUDES.into(),
            "Financial writes require fresh Touch ID, existing bounded authority, or explicitly enabled Yolo.".into(),
            "Your terminal confirmation approves GoFin setup; sudo permitted installation.".into(),
        ]);
    } else {
        let verb = match action {
            "add" => "Save",
            "generate" => "Save generated",
            "update" => "Update",
            "remove" => "Remove",
            _ => {
                return Err(error(
                    "INVALID_CHALLENGE",
                    "Unsupported configuration review.",
                ));
            }
        };
        let connection: ConnectionConfig = serde_json::from_value(binding["connection"].clone())
            .map_err(|_| error("INVALID_CHALLENGE", "The connection could not be reviewed."))?;
        let subject = if matches!(
            &connection,
            ConnectionConfig::Ethereum {
                signing_enabled: false,
                ..
            } | ConnectionConfig::Evm {
                signing_enabled: false,
                ..
            } | ConnectionConfig::Hyperliquid {
                signing_enabled: false,
                ..
            }
        ) {
            "read-only connection"
        } else {
            "credentials for connection"
        };
        lines.push(format!("{verb} {subject}: {}", safe(name(&connection))));
        if let ConnectionConfig::Kraken {
            default_wallet_account_id: Some(id),
            ..
        } = &connection
        {
            lines.push(format!("Default Funding wallet: {} | Relationship: human_verified; ownership: venue verified", safe(id)));
            lines.push("Your selection aliases this wallet to Spot custody; Kraken does not identify the default alias.".into());
        }
        match &connection {
            ConnectionConfig::Coinbase {
                name,
                resource,
                physical_identity,
                physical_identity_verified,
                portfolio_id,
                ..
            }
            | ConnectionConfig::CoinbaseExchange {
                name,
                resource,
                physical_identity,
                physical_identity_verified,
                portfolio_id,
                ..
            }
            | ConnectionConfig::CoinbaseInternational {
                name,
                resource,
                physical_identity,
                physical_identity_verified,
                portfolio_id,
                ..
            } => {
                let provider = match &connection {
                    ConnectionConfig::Coinbase { .. } => "coinbase",
                    ConnectionConfig::CoinbaseExchange { .. } => "coinbaseexchange",
                    _ => "coinbaseinternational",
                };
                if !*physical_identity_verified
                    || physical_identity.is_empty()
                    || portfolio_id
                        .as_deref()
                        .is_some_and(|id| id != physical_identity)
                    || resource.0 != format!("{provider}:portfolio:{physical_identity}")
                {
                    return Err(error(
                        "INVALID_CHALLENGE",
                        "The Coinbase portfolio identity binding is invalid.",
                    ));
                }
                lines.push(format!(
                    "{} portfolio: {}",
                    provider,
                    safe(physical_identity)
                ));
                lines.push(format!(
                    "Connection name: {} | Portfolio identity: venue verified",
                    safe(name)
                ));
                lines.push("The key is bound to this exact portfolio; reported permissions are shown in the verified checks.".into());
            }
            ConnectionConfig::Kraken {
                name,
                physical_identity,
                physical_identity_verified,
                ..
            }
            | ConnectionConfig::KrakenFutures {
                name,
                physical_identity,
                physical_identity_verified,
                ..
            } => {
                lines.push(format!(
                    "Kraken connection: {} | Account: {}",
                    safe(name),
                    safe(physical_identity)
                ));
                lines.push(format!(
                    "Account identity: {}",
                    if *physical_identity_verified {
                        "venue verified"
                    } else {
                        "human confirmed"
                    }
                ));
                lines.push(
                    "Product-specific credentials checked with a read; financial writes untested."
                        .into(),
                );
            }
            ConnectionConfig::CoinGecko {
                api_plan, resource, ..
            } => {
                if !matches!(api_plan.as_str(), "demo" | "pro")
                    || resource.0 != "market-data:coingecko"
                {
                    return Err(error(
                        "INVALID_CHALLENGE",
                        "The pricing connection is invalid.",
                    ));
                }
                lines.push(format!(
                    "CoinGecko {} API | USD pricing reads only",
                    safe(api_plan)
                ));
                lines.push("This key grants no exchange or wallet signing access.".into());
                lines.push("Calls consume API plan quota and may incur billing.".into());
            }
            ConnectionConfig::Jupiter { resource, .. } => {
                if resource.0 != "jupiter:service" {
                    return Err(error(
                        "INVALID_CHALLENGE",
                        "The Jupiter service credential is invalid.",
                    ));
                }
                lines.push(
                    "Jupiter API service key | used for API-backed Jupiter operations; quota or billing may apply."
                        .into(),
                );
                lines.push(
                    "This key is separate from every wallet signer and grants no wallet authority."
                        .into(),
                );
            }
            ConnectionConfig::Relay { resource, .. } => {
                if resource.0 != "relay:service" {
                    return Err(error(
                        "INVALID_CHALLENGE",
                        "The Relay service credential is invalid.",
                    ));
                }
                lines.push(
                    "Relay API service key | used for bridge quotes; quota or billing may apply."
                        .into(),
                );
                lines.push(
                    "This key is separate from every wallet signer and grants no wallet authority."
                        .into(),
                );
            }
            ConnectionConfig::Hyperliquid {
                owner,
                resource,
                execution_scope,
                signing_enabled,
                ..
            } => {
                let execution_address = execution_scope.address().unwrap_or(owner);
                if !gofin_hyperliquid::valid_address(owner)
                    || owner.bytes().any(|byte| byte.is_ascii_uppercase())
                    || !gofin_hyperliquid::valid_address(execution_address)
                    || execution_address
                        .bytes()
                        .any(|byte| byte.is_ascii_uppercase())
                    || resource.0
                        != format!("hyperliquid:mainnet:{}:account:primary", execution_address)
                {
                    return Err(error(
                        "INVALID_CHALLENGE",
                        "The Hyperliquid owner identity is invalid.",
                    ));
                }
                lines.push(format!(
                    "Hyperliquid mainnet | master {} | {} {}",
                    safe(owner),
                    execution_scope.kind(),
                    safe(execution_address)
                ));
                lines.push(if *signing_enabled {
                    "Signing: enabled using the explicitly enrolled protected master key.".into()
                } else {
                    "Signing: disabled; this connection is read-only.".into()
                });
                if execution_scope.address().is_some() {
                    lines.push("The target relationship was read from Hyperliquid during enrollment and is rechecked before each write.".into());
                    lines.push("This delegated scope can trade only; account authority and transfers remain master-owner actions.".into());
                } else {
                    lines.push("Owner enrollment is offline; it does not verify venue account access or permissions.".into());
                    lines.push(
                        "An existing EVM connection does not implicitly enroll this owner.".into(),
                    );
                }
            }
            ConnectionConfig::Binance {
                role,
                parent,
                email,
                wallet_type,
                physical_identity,
                physical_identity_verified,
                resource,
                ..
            } => {
                if !physical_identity_verified
                    || !matches!(role.as_str(), "main" | "sub")
                    || wallet_type != "SPOT"
                    || resource.0 != format!("binance:uid:{physical_identity}:SPOT")
                {
                    return Err(error(
                        "INVALID_CHALLENGE",
                        "The Binance account identity is incomplete.",
                    ));
                }
                lines.push(format!(
                    "Binance {} account | UID {} | SPOT wallet",
                    safe(role),
                    safe(physical_identity)
                ));
                lines.push("Signing: enabled using protected credentials.".into());
                if role == "sub" {
                    match (parent, email) {
                        (None, None) => {
                            lines.push("Standalone subaccount | No main connection linked.".into());
                        }
                        (Some(parent), Some(email)) => lines.push(format!(
                            "Main connection: {} | Child email: {}",
                            safe(parent),
                            safe(email)
                        )),
                        _ => {
                            return Err(error(
                                "INVALID_CHALLENGE",
                                "Subaccount association is incomplete.",
                            ));
                        }
                    }
                }
                if action != "remove" {
                    let permissions = binding["checks"]
                        .as_array()
                        .and_then(|checks| {
                            checks
                                .iter()
                                .find(|c| c["name"] == "reported API restrictions")
                        })
                        .map(|c| &c["permissions"]);
                    let fields = [
                        ("enableReading", "reading"),
                        ("enableWithdrawals", "withdrawals"),
                        ("enableInternalTransfer", "internal transfers"),
                        ("permitsUniversalTransfer", "universal transfers"),
                        ("ipRestrict", "IP restriction"),
                        ("enableSpotAndMarginTrading", "spot/margin trading"),
                        ("enableFutures", "futures trading"),
                    ];
                    let mut enabled = Vec::new();
                    let mut disabled = Vec::new();
                    let mut unknown = Vec::new();
                    for (field, label) in fields {
                        match permissions.and_then(|p| p[field].as_bool()) {
                            Some(true) => enabled.push(label),
                            Some(false) => disabled.push(label),
                            None => unknown.push(label),
                        }
                    }
                    lines.push("Binance reports (financial writes untested):".into());
                    for (label, values) in [
                        ("Enabled", enabled),
                        ("Disabled", disabled),
                        ("Unknown", unknown),
                    ] {
                        if !values.is_empty() {
                            lines.push(format!("{label}: {}", values.join(", ")));
                        }
                    }
                }
            }
            ConnectionConfig::Evm {
                address,
                signing_enabled,
                ..
            } => {
                lines.push(format!(
                    "EVM wallet: {} | Signing: {}",
                    safe(address),
                    if *signing_enabled {
                        "enabled"
                    } else {
                        "disabled (read-only)"
                    }
                ));
                lines.push("Reusable on Ethereum, Arbitrum One and Robinhood Chain.".into());
            }
            ConnectionConfig::Papertrade {
                name,
                owner,
                wallet_connection,
                signing_enabled,
                ..
            } => {
                lines.push(format!(
                    "Papertrade account: {} | Owner: {}",
                    safe(name),
                    safe(owner)
                ));
                lines.push(format!(
                    "Protected EVM wallet connection: {}",
                    safe(wallet_connection)
                ));
                lines.push(if *signing_enabled {
                    "Papertrade writes use the linked protected wallet and protocol-scoped session keys.".into()
                } else {
                    "Signing: disabled (read-only).".into()
                });
                lines.push("PAPER principal has no USD price; USDC and network fees remain subject to normal movement limits.".into());
            }
            ConnectionConfig::Solana {
                address,
                network,
                resource,
                signing_enabled,
                ..
            } => {
                if gofin_solana::parse_address(address).is_err()
                    || network != gofin_solana::NETWORK
                    || resource.0 != format!("{}:{address}", gofin_solana::NETWORK)
                {
                    return Err(error(
                        "INVALID_CHALLENGE",
                        "The Solana wallet mapping is invalid.",
                    ));
                }
                lines.push(format!("Solana mainnet-beta wallet: {}", safe(address)));
                lines.push(format!(
                    "Signing: {}",
                    if *signing_enabled {
                        "enabled using the protected keypair"
                    } else {
                        "disabled (read-only)"
                    }
                ));
                lines.push("Native balance and mainnet-beta identity checked.".into());
            }
            ConnectionConfig::Ethereum {
                address,
                network,
                signing_enabled,
                assets,
                ..
            } => {
                lines.push(format!("Ethereum wallet: {}", safe(address)));
                lines.push(format!(
                    "Network: {} | Signing: {}",
                    safe(network),
                    if *signing_enabled {
                        "enabled"
                    } else {
                        "disabled (read-only)"
                    }
                ));
                lines.push(format!(
                    "Tracked assets: {}",
                    assets
                        .iter()
                        .map(|a| format!("{} ({} decimals)", safe(&a.reference), a.decimals))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                if action != "remove" {
                    lines.push(
                        "RPC network and wallet balance checked; financial writes untested.".into(),
                    );
                }
            }
        }
        if action == "update" {
            lines.push("Replace saved credentials; remove the obsolete local key after saving. Revoke obsolete keys at the provider separately.".into());
        }
        if action == "remove" {
            lines.push("Remove local configuration and credentials. External funds and positions remain; retain independent recovery credentials. Provider keys are not revoked.".into());
        }
    }
    if let Some(value) = binding.get("warnings") {
        let warnings = value.as_array().ok_or_else(|| {
            error(
                "INVALID_CHALLENGE",
                "Configuration warnings could not be reviewed.",
            )
        })?;
        for warning in warnings {
            lines.push(format!(
                "Warning: {}",
                safe(warning.as_str().ok_or_else(|| error(
                    "INVALID_CHALLENGE",
                    "A configuration warning is invalid."
                ))?)
            ));
        }
    }
    if let Some(warning) = binding["warning"].as_str() {
        lines.push(format!("Warning: {}", safe(warning)));
    }
    if action != "initialize" {
        lines.push("Storage: protected local files in /Library/GoFin/private (credentials, configuration and journal).".into());
        lines.push(
            "No funds move now. Later financial writes require fresh Touch ID, existing bounded authority, or explicitly enabled Yolo."
                .into(),
        );
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn binance(role: &str) -> Value {
        json!({"action":"add","financial_movement":false,"future_financial_authority":false,
            "connection":{"provider":"binance","name":"SYNTHETIC-account","resource":"binance:uid:123:SPOT","credential_ref":"SYNTHETIC-ref","role":role,"parent":if role=="sub" {Some("SYNTHETIC-main")} else {None},"email":if role=="sub" {Some("synthetic@example.invalid")} else {None},"wallet_type":"SPOT","physical_identity":"123","physical_identity_verified":true},
            "checks":[{"name":"reported API restrictions","permissions":{"enableReading":true,"enableWithdrawals":false}}],"warnings":["Synthetic warning"]})
    }
    fn coingecko(plan: &str) -> Value {
        let connection = ConnectionConfig::CoinGecko {
            name: "synthetic-pricing".into(),
            resource: ResourceId("market-data:coingecko".into()),
            credential_ref: "11111111-1111-4111-8111-111111111111".into(),
            api_plan: plan.into(),
        };
        json!({"action":"add","financial_movement":false,"future_financial_authority":false,"connection":connection,"checks":[{"name":"CoinGecko API access","status":"passed"},{"name":"fresh USD price read","status":"passed"}],"warnings":[]})
    }

    #[test]
    fn policy_review_shows_exact_changes_and_preserves_write_approval() {
        let previous = BrokerPolicy::beta();
        let mut next = previous.clone();
        next.max_movement_usd_micros = "1000000001".into();
        next.max_run_allocation_usd_micros = "5000000000".into();
        let mut binding = json!({"action":"policy_set","previous_policy":previous,"policy":next,"financial_movement":false,"future_financial_authority":false});
        let text = review(&binding).unwrap();
        for expected in [
            "$100 USD → $1000.000001 USD",
            "$100 USD → $5000 USD",
            "rolling 24-hour",
            "fresh Touch ID",
        ] {
            assert!(text.contains(expected), "{text}");
        }
        binding["policy"]["max_movement_usd_micros"] = json!("0");
        assert!(review(&binding).unwrap().contains("$100 USD → No limit"));
        binding["policy"]["allow_unpriced_exchange_assets"] = json!(true);
        let exception = review(&binding).unwrap();
        assert!(exception.contains("Unpriced exchange writes: Blocked → Allowed"));
        assert!(exception.contains(gofin_domain::unpriced::WARNING));
        binding["policy"]["require_run_for_writes"] = json!(true);
        assert!(review(&binding).is_err());
    }

    #[test]
    fn coingecko_review_names_plan_scope_and_cost_without_private_reference_or_json() {
        for plan in ["demo", "pro"] {
            let binding = coingecko(plan);
            let text = review(&binding).unwrap();
            for expected in [
                "Save credentials for connection: synthetic-pricing",
                &format!("CoinGecko {plan} API"),
                "USD pricing reads only",
                "no exchange or wallet signing access",
                "quota",
                "No funds move now",
                "fresh Touch ID",
            ] {
                assert!(text.contains(expected), "missing {expected}");
            }
            assert!(!text.contains("11111111-1111-4111-8111-111111111111"));
            assert!(!text.contains("credential_ref") && !text.contains("{\""));
            assert!(text.lines().count() <= 9);
            let mut update = binding.clone();
            update["action"] = json!("update");
            let text = review(&update).unwrap();
            assert!(text.contains("Update credentials for connection: synthetic-pricing"));
            assert!(text.contains("Revoke obsolete keys at the provider separately"));
            let mut remove = binding;
            remove["action"] = json!("remove");
            let text = review(&remove).unwrap();
            assert!(text.contains("Remove credentials for connection: synthetic-pricing"));
            assert!(text.contains("Provider keys are not revoked"));
        }
    }

    #[test]
    fn coingecko_review_rejects_materially_invalid_or_secret_bearing_bindings() {
        for (field, value) in [
            ("api_plan", json!("unsupported")),
            ("resource", json!("market-data:other")),
            ("api_key", json!("SYNTHETIC-NOT-A-REAL-KEY")),
            ("signing_enabled", json!(true)),
        ] {
            let mut wrong = coingecko("demo");
            wrong["connection"][field] = value;
            let failure = review(&wrong).unwrap_err();
            assert_eq!(failure.code, "INVALID_CHALLENGE");
            assert!(!failure.to_string().contains("SYNTHETIC-NOT-A-REAL-KEY"));
        }
        for flag in ["financial_movement", "future_financial_authority"] {
            let mut wrong = coingecko("demo");
            wrong[flag] = json!(true);
            assert_eq!(review(&wrong).unwrap_err().code, "INVALID_CHALLENGE");
        }
        let mut binding = coingecko("demo");
        binding["connection"]["name"] = json!("synthetic\u{1b}[2J\u{202e}pricing");
        binding["warnings"] = json!(["synthetic\u{1b}[2Jwarning"]);
        let text = review(&binding).unwrap();
        assert!(!text.contains('\u{1b}') && !text.contains('\u{202e}'));
    }

    #[test]
    fn main_review_is_concise_truthful_and_shows_unknown_permissions() {
        let text = review(&binance("main")).unwrap();
        for expected in [
            "Save credentials for connection",
            "UID 123",
            "Signing: enabled",
            "Enabled: reading",
            "Disabled: withdrawals",
            "Unknown:",
            "Synthetic warning",
            "/Library/GoFin/private",
            "No funds move",
            "fresh Touch ID",
        ] {
            assert!(text.contains(expected), "{expected}");
        }
        assert!(!text.contains("SYNTHETIC-ref"));
        assert!(!text.contains("Keychain"));
        assert!(text.lines().count() <= 12);
    }
    #[test]
    fn child_identity_and_cleanup_remain_visible_without_association_warning() {
        let mut binding = binance("sub");
        let text = review(&binding).unwrap();
        assert!(text.contains("UID 123"));
        assert!(text.contains("Child email: synthetic@example.invalid"));
        assert!(!text.contains("VERIFY:") && !text.contains("UID association"));
        assert!(text.contains("Synthetic warning"));
        assert!(text.contains("Main connection: SYNTHETIC-main"));
        binding["action"] = "update".into();
        let text = review(&binding).unwrap();
        assert!(text.contains("Revoke obsolete keys"));
        assert!(!text.contains("VERIFY:") && !text.contains("UID association"));
        binding["action"] = "remove".into();
        assert!(
            review(&binding)
                .unwrap()
                .contains("External funds and positions remain")
        );
    }
    #[test]
    fn standalone_subaccount_review_supports_add_update_and_remove() {
        for action in ["add", "update", "remove"] {
            let mut binding = binance("sub");
            binding["action"] = json!(action);
            binding["connection"]["parent"] = Value::Null;
            binding["connection"]["email"] = Value::Null;
            let text = review(&binding).unwrap();
            assert!(text.contains("Binance sub account | UID 123 | SPOT wallet"));
            assert!(text.contains("Standalone subaccount | No main connection linked."));
            assert!(!text.contains("SYNTHETIC-ref"));
            assert!(!text.contains("Child email:"));
        }
    }

    #[test]
    fn subaccount_review_rejects_partial_associations() {
        for action in ["add", "update", "remove"] {
            for missing in ["parent", "email"] {
                let mut binding = binance("sub");
                binding["action"] = json!(action);
                binding["connection"][missing] = Value::Null;
                assert_eq!(review(&binding).unwrap_err().code, "INVALID_CHALLENGE");
            }
        }
    }

    #[test]
    fn standalone_subaccount_review_still_requires_verified_matching_identity() {
        for (field, value) in [
            ("physical_identity_verified", json!(false)),
            ("physical_identity", json!("456")),
            ("resource", json!("binance:uid:456:SPOT")),
            ("wallet_type", json!("FUTURES")),
        ] {
            let mut binding = binance("sub");
            binding["connection"]["parent"] = Value::Null;
            binding["connection"]["email"] = Value::Null;
            binding["connection"][field] = value;
            assert_eq!(review(&binding).unwrap_err().code, "INVALID_CHALLENGE");
        }
    }

    #[test]
    fn setup_review_matches_only_the_implemented_policy() {
        let config = setup_configuration(501, BrokerPolicy::beta());
        let mut binding = json!({"action":"initialize","financial_movement":false,"future_financial_authority":false,"configuration":config});
        let text = review(&binding).unwrap();
        assert!(text.contains("Confirm this Mac account as GoFin's approver."));
        assert!(text.contains("sudo permitted installation"));
        assert!(text.contains("terminal confirmation approves GoFin setup"));
        assert!(text.contains("Movement limit: $100 USD"));
        assert!(text.contains("24-hour allocation limit: $100 USD"));
        binding["configuration"]["policy"]["max_movement_usd_micros"] = "200000000".into();
        assert!(review(&binding).unwrap().contains("$200 USD"));
        binding["configuration"]["policy"]["max_movement_usd_micros"] = "0".into();
        assert!(
            review(&binding)
                .unwrap()
                .contains("Movement limit: No limit")
        );
        binding["configuration"]["policy"]["require_run_for_writes"] = true.into();
        assert!(review(&binding).is_err());
    }

    #[test]
    fn setup_review_rejects_changes_outside_the_collected_limits() {
        let config = setup_configuration(501, BrokerPolicy::beta());
        let original = json!({
            "action": "initialize",
            "financial_movement": false,
            "future_financial_authority": false,
            "configuration": config,
        });
        for (path, value) in [
            ("/configuration/policy/preview_ttl_seconds", json!(60)),
            (
                "/configuration/policy/allow_unpriced_exchange_assets",
                json!(true),
            ),
            (
                "/configuration/networks/0/rpc_urls/0",
                json!("https://example.invalid/rpc"),
            ),
            ("/configuration/deployments", json!([])),
            ("/configuration/extensions", json!({"unexpected": true})),
            ("/financial_movement", json!(true)),
            ("/future_financial_authority", json!(true)),
        ] {
            let mut binding = original.clone();
            *binding.pointer_mut(path).expect("setup field exists") = value;
            assert_eq!(
                review(&binding).unwrap_err().code,
                "INVALID_CHALLENGE",
                "{path}"
            );
        }
    }
    #[test]
    fn unsafe_display_characters_and_financial_bindings_cannot_hide_material() {
        let mut binding = binance("main");
        binding["connection"]["name"] = "SYNTHETIC\u{1b}[2J\u{202e}".into();
        let text = review(&binding).unwrap();
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\u{202e}'));
        binding["financial_movement"] = true.into();
        assert!(review(&binding).is_err());
        binding["financial_movement"] = false.into();
        binding["connection"]["provider"] = "unsupported".into();
        assert!(review(&binding).is_err());
    }
    #[test]
    fn default_wallet_review_discloses_exact_human_relationship() {
        let binding = json!({"action":"add","financial_movement":false,"future_financial_authority":false,"connection":{"provider":"kraken","name":"spot","resource":"kraken:account:synthetic","credential_ref":"11111111-1111-4111-8111-111111111111","physical_identity":"synthetic","physical_identity_verified":false,"default_wallet_account_id":"WDEFAULT"}});
        let text = review(&binding).unwrap();
        assert!(text.contains("Default Funding wallet: WDEFAULT"));
        assert!(text.contains("Relationship: human_verified; ownership: venue verified"));
        assert!(text.contains("Kraken does not identify the default alias"));
        let mut changed = binding.clone();
        changed["connection"]["default_wallet_account_id"] = json!("WOTHER");
        assert_ne!(
            gofin_domain::binding_hash(&binding).unwrap(),
            gofin_domain::binding_hash(&changed).unwrap()
        );
    }
}
