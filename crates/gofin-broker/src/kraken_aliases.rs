//! Protected, nonsecret custody alias history survives connection removal.
use crate::{BrokerConfigV2, ConnectionConfig};
use gofin_domain::{GoFinError, Result};
use std::collections::BTreeMap;

const KEY: &str = "kraken_default_wallet_aliases_v1";
fn invalid() -> GoFinError {
    GoFinError::validation(
        "The protected Kraken default-wallet history is invalid or conflicts with custody identity. Preserve the historical alias; a new selection cannot retarget it.",
    )
}
fn id_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
fn history(config: &BrokerConfigV2) -> Result<BTreeMap<String, String>> {
    let rows = config
        .extensions
        .get(KEY)
        .map(|v| {
            serde_json::from_value::<BTreeMap<String, String>>(v.clone()).map_err(|_| invalid())
        })
        .transpose()?
        .unwrap_or_default();
    if rows.len() > 10_000
        || rows
            .iter()
            .any(|(identity, wallet)| !id_valid(identity) || !id_valid(wallet))
    {
        return Err(invalid());
    }
    Ok(rows)
}
pub(crate) fn validate(config: &BrokerConfigV2) -> Result<()> {
    let rows = history(config)?;
    for connection in &config.connections {
        if let ConnectionConfig::Kraken {
            physical_identity,
            default_wallet_account_id,
            ..
        } = connection
            && let Some(previous) = rows.get(physical_identity)
            && default_wallet_account_id.as_ref() != Some(previous)
        {
            return Err(invalid());
        }
    }
    Ok(())
}
/// Capture existing selections before any connection mutation; history is never deleted.
pub(crate) fn retain(config: &mut BrokerConfigV2) -> Result<()> {
    let mut rows = history(config)?;
    for connection in &config.connections {
        if let ConnectionConfig::Kraken {
            physical_identity,
            default_wallet_account_id: Some(id),
            ..
        } = connection
        {
            if rows
                .get(physical_identity)
                .is_some_and(|previous| previous != id)
            {
                return Err(invalid());
            }
            rows.insert(physical_identity.clone(), id.clone());
        }
    }
    if !rows.is_empty() {
        config.extensions.insert(
            KEY.into(),
            serde_json::to_value(rows).map_err(|_| invalid())?,
        );
    }
    validate(config)
}
pub(crate) fn preserve(
    config: &BrokerConfigV2,
    identity: &str,
    supplied: &mut Option<String>,
) -> Result<()> {
    if let Some(previous) = history(config)?.get(identity) {
        if supplied.as_ref().is_some_and(|id| id != previous) {
            return Err(invalid());
        }
        *supplied = Some(previous.clone());
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn removed_connection_history_survives_reenrollment_and_rejects_retarget() {
        let mut config:BrokerConfigV2=serde_json::from_value(serde_json::json!({"version":2,"approver_uid":501,"networks":[],"deployments":[],"connections":[{"provider":"kraken","name":"spot","resource":"kraken:account:synthetic","credential_ref":"11111111-1111-4111-8111-111111111111","physical_identity":"synthetic","physical_identity_verified":false,"default_wallet_account_id":"WDEFAULT"}],"policy":crate::BrokerPolicy::beta(),"extensions":{}})).unwrap();
        retain(&mut config).unwrap();
        let selected = config.connections.pop().unwrap();
        retain(&mut config).unwrap();
        let config: BrokerConfigV2 =
            serde_json::from_value(serde_json::to_value(config).unwrap()).unwrap();
        let mut selection = None;
        preserve(&config, "synthetic", &mut selection).unwrap();
        assert_eq!(selection.as_deref(), Some("WDEFAULT"));
        selection = Some("OTHER".into());
        assert!(preserve(&config, "synthetic", &mut selection).is_err());
        let mut invalid = config.clone();
        let mut raw = serde_json::to_value(selected).unwrap();
        raw["default_wallet_account_id"] = serde_json::json!("OTHER");
        invalid
            .connections
            .push(serde_json::from_value(raw).unwrap());
        assert!(validate(&invalid).is_err());
        invalid
            .extensions
            .insert(KEY.into(), serde_json::json!({"bad/path":"WDEFAULT"}));
        assert!(validate(&invalid).is_err());
    }
}
