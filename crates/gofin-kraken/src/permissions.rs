//! Permission checks are independent of CLI validation and financial authority.
use crate::{
    KrakenEndpoint,
    observation::result,
    profile::{KrakenAccount, KrakenProfile, error},
};
use gofin_domain::Result;
use serde_json::{Value, json};
impl KrakenProfile {
    pub(crate) async fn derivative_permissions(&self, account: &KrakenAccount) -> Result<Value> {
        let response = self
            .json(
                KrakenEndpoint::KrakenfuturesAuthGetApiKeysV3Check,
                Some(account),
                json!({}),
            )
            .await?;
        if response["permissions"]["general"] != "FULL_ACCESS" {
            return Err(error(
                "KRAKEN_PERMISSION_DENIED",
                "This derivative key needs general FULL_ACCESS for exact financial order operations.",
            ));
        }
        Ok(
            json!({"permissions":response["permissions"],"iiban":response["iiban"],"accountUid":response["accountUid"]}),
        )
    }

    pub(crate) async fn spot_order_mutation_permissions(
        &self,
        account: &KrakenAccount,
        endpoint: KrakenEndpoint,
    ) -> Result<Value> {
        let permission = self
            .spot_permissions(
                account,
                &[
                    "query-funds",
                    "query-open-trades",
                    "query-closed-trades",
                    "query-ledger",
                ],
            )
            .await?;
        let permissions = permission["permissions"].as_array().unwrap();
        let modifies = permissions.iter().any(|p| p == "modify-trades");
        let closes = permissions.iter().any(|p| p == "close-trades");
        let allowed = if endpoint == KrakenEndpoint::KrakenPrivatePostAmendOrder {
            modifies || closes
        } else {
            modifies && closes
        };
        if !allowed {
            return Err(error(
                "KRAKEN_PERMISSION_DENIED",
                if endpoint == KrakenEndpoint::KrakenPrivatePostAmendOrder {
                    "Atomic amendments require either modify-trades or close-trades, plus their settlement-read permissions."
                } else {
                    "Legacy cancel-and-replace edits require both modify-trades and close-trades, plus their settlement-read permissions."
                },
            ));
        }
        Ok(permission)
    }

    pub(crate) async fn spot_permissions(
        &self,
        account: &KrakenAccount,
        required: &[&str],
    ) -> Result<Value> {
        let response = self
            .json(
                KrakenEndpoint::KrakenPrivatePostGetApiKeyInfo,
                Some(account),
                json!({}),
            )
            .await?;
        let data = result(&response);
        let permissions = data["permissions"].as_array().ok_or_else(|| {
            error(
                "KRAKEN_PERMISSION_UNKNOWN",
                "Kraken did not return authoritative API key permissions.",
            )
        })?;
        for permission in required {
            if !permissions.iter().any(|p| p == *permission) {
                return Err(error(
                    "KRAKEN_PERMISSION_DENIED",
                    &format!(
                        "This Kraken key requires the {permission} permission for the selected operation and its settlement reads."
                    ),
                ));
            }
        }
        if let Some(expiry) = data.get("valid_until").or_else(|| data.get("validUntil")) {
            let expiry = expiry.as_str().ok_or_else(|| {
                error(
                    "KRAKEN_KEY_EXPIRY_UNKNOWN",
                    "Kraken returned invalid key expiry evidence.",
                )
            })?;
            let expiry = expiry.parse::<i64>().map_err(|_| {
                error(
                    "KRAKEN_KEY_EXPIRY_UNKNOWN",
                    "Kraken returned invalid key expiry evidence.",
                )
            })?;
            if expiry != 0 && expiry <= chrono::Utc::now().timestamp() {
                return Err(error(
                    "KRAKEN_KEY_EXPIRED",
                    "This Kraken API key has expired.",
                ));
            }
        }
        Ok(
            json!({"permissions":permissions,"iiban":data.get("iban").or_else(||data.get("iiban")).cloned().unwrap_or(Value::Null)}),
        )
    }
}
