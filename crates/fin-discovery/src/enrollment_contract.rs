//! Safe protected-enrollment output shared by discovery and the broker.
use fin_domain::ResourceId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub enum PublicConnection {
    Coinbase {
        name: String,
        resource: ResourceId,
        physical_identity: String,
        identity_verification: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        portfolio_id: Option<String>,
    },
    #[serde(rename = "coinbaseexchange")]
    CoinbaseExchange {
        name: String,
        resource: ResourceId,
        physical_identity: String,
        identity_verification: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        portfolio_id: Option<String>,
    },
    #[serde(rename = "coinbaseinternational")]
    CoinbaseInternational {
        name: String,
        resource: ResourceId,
        physical_identity: String,
        identity_verification: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        portfolio_id: Option<String>,
    },
    Kraken {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_wallet_account_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_wallet_relationship_verification: Option<String>,
        name: String,
        resource: ResourceId,
        physical_identity: String,
        identity_verification: String,
    },
    #[serde(rename = "krakenfutures")]
    KrakenFutures {
        name: String,
        resource: ResourceId,
        physical_identity: String,
        identity_verification: String,
    },
    #[serde(rename = "coingecko")]
    CoinGecko {
        name: String,
        resource: ResourceId,
        api_plan: String,
    },
    Binance {
        name: String,
        resource: ResourceId,
        role: String,
        parent: Option<String>,
        email: Option<String>,
        wallet_type: String,
        physical_identity: String,
    },
    Evm {
        name: String,
        resource: ResourceId,
        address: String,
        signing_enabled: bool,
    },
    /// Preserved for existing chain-scoped connection metadata.
    Ethereum {
        name: String,
        resource: ResourceId,
        network: String,
        address: String,
        signing_enabled: bool,
    },
    Hyperliquid {
        name: String,
        resource: ResourceId,
        owner: String,
        execution_address: String,
        execution_scope: String,
        signing_enabled: bool,
    },
    Solana {
        name: String,
        resource: ResourceId,
        address: String,
        network: String,
        signing_enabled: bool,
    },
    Jupiter {
        name: String,
        resource: ResourceId,
        credential_configured: bool,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentOutcome {
    pub action: String,
    pub connection: Option<PublicConnection>,
    pub checks: Vec<Value>,
    pub warnings: Vec<String>,
    pub cleanup_warning: Option<String>,
    pub configuration_authorized: bool,
    pub financial_movement: bool,
    pub future_financial_authority: bool,
    pub next_action: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn coinbase_public_identity_has_no_credential_or_signing_slot() {
        for provider in ["coinbase", "coinbaseexchange", "coinbaseinternational"] {
            let value = json!({"provider":provider,"name":"coinbase-test",
                "resource":format!("{provider}:portfolio:synthetic-id"),
                "physical_identity":"synthetic-id","identity_verification":"authenticated",
                "portfolio_id":"synthetic-id"});
            let parsed: PublicConnection = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), value);
            for key in [
                "api_key",
                "secret",
                "passphrase",
                "private_key",
                "credential_ref",
                "signature",
            ] {
                let mut wrong = value.clone();
                wrong[key] = json!("SYNTHETIC-SECRET");
                assert!(serde_json::from_value::<PublicConnection>(wrong).is_err());
            }
        }
    }
    #[test]
    fn coingecko_public_metadata_has_no_secret_or_signing_slot() {
        let value = json!({"provider":"coingecko","name":"pricing","resource":"market-data:coingecko","api_plan":"demo"});
        let parsed: PublicConnection = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), value);
        for field in [
            "api_key",
            "credential_ref",
            "secret",
            "network",
            "address",
            "signing_enabled",
        ] {
            let mut wrong = value.clone();
            wrong[field] = json!("SYNTHETIC-NOT-A-REAL-KEY");
            assert!(serde_json::from_value::<PublicConnection>(wrong).is_err());
        }
    }

    #[test]
    fn protected_references_and_secret_fields_have_no_public_output_slot() {
        let connection = json!({"provider":"ethereum","name":"wallet","resource":"ethereum:1:synthetic","network":"ethereum:1","address":"0x1111111111111111111111111111111111111111","signing_enabled":true});
        assert!(serde_json::from_value::<PublicConnection>(connection.clone()).is_ok());
        let evm = json!({"provider":"evm","name":"wallet","resource":"evm:0x1111111111111111111111111111111111111111","address":"0x1111111111111111111111111111111111111111","signing_enabled":true});
        assert!(serde_json::from_value::<PublicConnection>(evm.clone()).is_ok());
        let mut scoped_evm = evm.clone();
        scoped_evm["network"] = json!("ethereum:42161");
        assert!(serde_json::from_value::<PublicConnection>(scoped_evm).is_err());
        for forbidden in ["credential_ref", "private_key", "api_key", "signature"] {
            let mut changed_evm = evm.clone();
            changed_evm[forbidden] = json!("SYNTHETIC-SECRET");
            assert!(serde_json::from_value::<PublicConnection>(changed_evm).is_err());
            let mut changed = connection.clone();
            changed[forbidden] = json!("SYNTHETIC-SECRET");
            assert!(serde_json::from_value::<PublicConnection>(changed).is_err());
        }
    }

    #[test]
    fn hyperliquid_public_metadata_exposes_owner_without_protected_reference() {
        let value = json!({
            "provider":"hyperliquid",
            "name":"hl-main",
            "resource":"hyperliquid:mainnet:0x1111111111111111111111111111111111111111:account:primary",
            "owner":"0x1111111111111111111111111111111111111111",
            "execution_address":"0x1111111111111111111111111111111111111111",
            "execution_scope":"owner",
            "signing_enabled":true
        });
        let parsed: PublicConnection = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), value);
        for forbidden in ["credential_ref", "private_key", "api_key", "signature"] {
            let mut wrong = value.clone();
            wrong[forbidden] = json!("SYNTHETIC-SECRET");
            assert!(serde_json::from_value::<PublicConnection>(wrong).is_err());
        }
    }

    #[test]
    fn solana_and_jupiter_contracts_roundtrip_without_secret_or_reference_slots() {
        let cases = [
            json!({"provider":"solana","name":"observer","resource":"solana:mainnet-beta:observer","address":"SyntheticAddress","network":"solana:mainnet-beta","signing_enabled":false}),
            json!({"provider":"solana","name":"signer","resource":"solana:mainnet-beta:signer","address":"SyntheticSignerAddress","network":"solana:mainnet-beta","signing_enabled":true}),
            json!({"provider":"jupiter","name":"service","resource":"jupiter:service","credential_configured":true}),
        ];
        for value in cases {
            let parsed: PublicConnection = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), value);
            for forbidden in [
                "credential_ref",
                "private_key",
                "api_key",
                "secret",
                "signature",
            ] {
                let mut changed = value.clone();
                changed[forbidden] = json!("SYNTHETIC-SECRET-REFERENCE");
                assert!(serde_json::from_value::<PublicConnection>(changed).is_err());
            }
        }
    }
}
