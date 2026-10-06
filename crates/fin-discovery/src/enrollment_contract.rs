//! Safe protected-enrollment output shared by discovery and the broker.
use fin_domain::ResourceId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub enum PublicConnection {
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
}
