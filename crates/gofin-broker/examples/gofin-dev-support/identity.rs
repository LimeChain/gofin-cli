use async_trait::async_trait;
use gofin_binance::{
    BinanceEndpoint, BinanceRequest, BinanceTransport, CredentialHandle, SubAccountListResponse,
    SubAccountRecord,
};
use serde_json::Value;
use std::sync::Arc;

#[async_trait]
pub trait BinanceIdentityApi: Send + Sync {
    async fn account_uid(&self, credential: &CredentialHandle) -> Result<String, String>;
    async fn sub_accounts(
        &self,
        credential: &CredentialHandle,
    ) -> Result<Vec<SubAccountRecord>, String>;
}

pub struct CcxtBinanceIdentityApi {
    transport: gofin_binance::BinanceCcxtTransport,
}

impl CcxtBinanceIdentityApi {
    pub fn new(
        credentials: Arc<dyn gofin_exchange::ccxt::CredentialProvider>,
    ) -> Result<Self, String> {
        let client = gofin_exchange::ccxt::CcxtClient::new()
            .map_err(|_| "Binance identity transport could not be initialized".to_owned())?;
        Ok(Self {
            transport: gofin_binance::BinanceCcxtTransport::new(Arc::new(client), credentials),
        })
    }
}

#[async_trait]
impl BinanceIdentityApi for CcxtBinanceIdentityApi {
    async fn account_uid(&self, credential: &CredentialHandle) -> Result<String, String> {
        let response = self
            .transport
            .request(credential, BinanceRequest::new(BinanceEndpoint::Account))
            .await
            .map_err(safe_transport_error)?;
        parse_account_uid(&response)
    }

    async fn sub_accounts(
        &self,
        credential: &CredentialHandle,
    ) -> Result<Vec<SubAccountRecord>, String> {
        let mut records = Vec::new();
        for page in 1..=50 {
            let request = BinanceRequest::new(BinanceEndpoint::SubAccountList)
                .push("page", page.to_string())
                .push("limit", "200");
            let response = self
                .transport
                .request(credential, request)
                .await
                .map_err(safe_transport_error)?;
            let page_records: SubAccountListResponse = serde_json::from_value(response)
                .map_err(|_| "Binance subaccount identity response was invalid".to_owned())?;
            let count = page_records.sub_accounts.len();
            records.extend(page_records.sub_accounts);
            if count < 200 {
                return Ok(records);
            }
        }
        Err("Binance subaccount list exceeded its safe page bound".into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedBinanceIdentity {
    pub main_uid: String,
    pub sub_uid: String,
    pub sub_email: String,
    /// True only when the venue returned the matching subAccountId.
    pub sub_email_matched_by_venue_id: bool,
}

pub async fn resolve_main_uid(
    api: &dyn BinanceIdentityApi,
    expected_uid: Option<&str>,
) -> Result<String, String> {
    let handle = CredentialHandle::new("dev-import-main")
        .map_err(|_| "invalid temporary credential handle")?;
    let uid = api.account_uid(&handle).await?;
    validate_uid(&uid)?;
    if expected_uid.is_some_and(|expected| expected != uid) {
        return Err("discovered Binance account identity does not match the configured UID".into());
    }
    Ok(uid)
}

/// Discover a standalone subaccount through its own authenticated account endpoint.
/// No parent relationship or email can be inferred from this observation.
pub async fn resolve_sub_uid(
    api: &dyn BinanceIdentityApi,
    expected_uid: Option<&str>,
) -> Result<String, String> {
    let handle = CredentialHandle::new("dev-import-sub")
        .map_err(|_| "invalid temporary credential handle")?;
    let uid = api.account_uid(&handle).await?;
    validate_uid(&uid)?;
    if expected_uid.is_some_and(|expected| expected != uid) {
        return Err("discovered Binance account identity does not match the configured UID".into());
    }
    Ok(uid)
}

pub async fn resolve_binance_identity(
    api: &dyn BinanceIdentityApi,
    explicit_email: Option<&str>,
    expected_main_uid: Option<&str>,
    expected_sub_uid: Option<&str>,
) -> Result<ResolvedBinanceIdentity, String> {
    let main_handle = CredentialHandle::new("dev-import-main")
        .map_err(|_| "invalid temporary credential handle")?;
    let sub_handle = CredentialHandle::new("dev-import-sub")
        .map_err(|_| "invalid temporary credential handle")?;
    let main_uid = api.account_uid(&main_handle).await?;
    let sub_uid = api.account_uid(&sub_handle).await?;
    validate_uid(&main_uid)?;
    validate_uid(&sub_uid)?;
    if expected_main_uid.is_some_and(|value| value != main_uid)
        || expected_sub_uid.is_some_and(|value| value != sub_uid)
    {
        return Err("discovered Binance account identity does not match the configured UID".into());
    }
    if main_uid == sub_uid {
        return Err("Binance main and subaccount keys resolve to the same physical account".into());
    }
    let rows = api.sub_accounts(&main_handle).await?;
    let (row, matched_by_id) = if let Some(email) = explicit_email {
        if email.trim().is_empty() || email.len() > 254 {
            return Err("configured Binance child email is invalid".into());
        }
        let matches = rows
            .iter()
            .filter(|row| row.email == email)
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(
                "configured Binance child email did not identify exactly one main-account child"
                    .into(),
            );
        }
        let row = matches[0];
        if row.is_freeze != Some(false) {
            return Err("configured Binance child account is frozen".into());
        }
        match &row.sub_account_id {
            Some(id) if id.as_str() != sub_uid => {
                return Err(
                    "configured Binance child email belongs to a different account UID".into(),
                );
            }
            Some(_) => (row, true),
            None => (row, false),
        }
    } else {
        let matches = rows
            .iter()
            .filter(|row| {
                row.sub_account_id
                    .as_ref()
                    .is_some_and(|id| id.as_str() == sub_uid)
            })
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err("Binance did not provide one exact child ID match; set GOFIN_DEV_BINANCE_SUB_EMAIL explicitly".into());
        }
        let row = matches[0];
        if row.is_freeze != Some(false) {
            return Err("discovered Binance child account is frozen".into());
        }
        (row, true)
    };
    if row.email.trim().is_empty() {
        return Err("Binance subaccount email was empty".into());
    }
    Ok(ResolvedBinanceIdentity {
        main_uid,
        sub_uid,
        sub_email: row.email.clone(),
        sub_email_matched_by_venue_id: matched_by_id,
    })
}

pub fn parse_account_uid(identity: &Value) -> Result<String, String> {
    if identity["accountType"] != "SPOT" || !identity["balances"].is_array() {
        return Err("Binance key did not report a SPOT identity and balance array".into());
    }
    let uid = match &identity["uid"] {
        Value::String(value) => value.clone(),
        Value::Number(value) if value.is_u64() => value.to_string(),
        _ => return Err("Binance account UID was missing or inexact".into()),
    };
    validate_uid(&uid)?;
    Ok(uid)
}

fn validate_uid(uid: &str) -> Result<(), String> {
    if uid.is_empty()
        || uid.len() > 32
        || uid.starts_with('0')
        || !uid.bytes().all(|b| b.is_ascii_digit())
        || uid.parse::<u64>().is_err()
        || uid == "0"
    {
        return Err("Binance account UID is not a canonical positive integer".into());
    }
    Ok(())
}

fn safe_transport_error(error: gofin_binance::TransportError) -> String {
    use gofin_binance::{HttpFailureKind, TransportError};
    // Transport errors retain typed numeric evidence only. Never format an
    // upstream response, credential, URL, or signed request into this message.
    let diagnostic = match &error {
        TransportError::HttpFailure {
            status,
            binance_code: Some(code),
            ..
        } => format!(" (HTTP {status}; Binance code {code})"),
        TransportError::HttpFailure {
            status,
            binance_code: None,
            ..
        } => format!(" (HTTP {status})"),
        _ => String::new(),
    };
    let message: &str = match error {
        TransportError::PermissionDenied
        | TransportError::HttpFailure {
            kind: HttpFailureKind::PermissionDenied,
            ..
        } => "Binance credential authentication or endpoint permission was denied",
        TransportError::Timeout => "Binance identity read timed out",
        TransportError::Unavailable => "Binance identity endpoint was unavailable",
        TransportError::RateLimited
        | TransportError::HttpFailure {
            kind: HttpFailureKind::RateLimited,
            ..
        } => "Binance rate limited the identity read",
        TransportError::Rejected
        | TransportError::HttpFailure {
            kind: HttpFailureKind::Rejected,
            ..
        } => "Binance rejected the identity read",
        TransportError::InvalidResponse
        | TransportError::HttpFailure {
            kind: HttpFailureKind::InvalidResponse,
            ..
        } => "Binance returned an invalid identity response",
    };
    format!("{message}{diagnostic}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use gofin_binance::{SubAccountRecord, VenueIdentifier};
    use std::sync::Mutex;

    #[test]
    fn identity_errors_expose_only_numeric_http_evidence() {
        use gofin_binance::{HttpFailureKind, TransportError};
        let message = safe_transport_error(TransportError::HttpFailure {
            status: 401,
            binance_code: Some(-2015),
            kind: HttpFailureKind::PermissionDenied,
        });
        assert_eq!(
            message,
            "Binance credential authentication or endpoint permission was denied (HTTP 401; Binance code -2015)"
        );
        // Exact equality enforces the whitelist: neither raw upstream text nor
        // credential/request fields can be appended to the public diagnostic.
        assert!(!message.contains("signature="));
        assert!(!message.contains("apiKey"));
        assert_eq!(
            safe_transport_error(TransportError::HttpFailure {
                status: 403,
                binance_code: None,
                kind: HttpFailureKind::PermissionDenied,
            }),
            "Binance credential authentication or endpoint permission was denied (HTTP 403)"
        );
        assert_eq!(
            safe_transport_error(TransportError::PermissionDenied),
            "Binance credential authentication or endpoint permission was denied"
        );
        assert_eq!(
            safe_transport_error(TransportError::HttpFailure {
                status: 400,
                binance_code: Some(-1022),
                kind: HttpFailureKind::Rejected,
            }),
            "Binance rejected the identity read (HTTP 400; Binance code -1022)"
        );
    }

    struct FixtureApi {
        uids: Mutex<Vec<String>>,
        rows: Vec<SubAccountRecord>,
    }

    #[async_trait]
    impl BinanceIdentityApi for FixtureApi {
        async fn account_uid(&self, _credential: &CredentialHandle) -> Result<String, String> {
            Ok(self.uids.lock().unwrap().remove(0))
        }
        async fn sub_accounts(
            &self,
            _credential: &CredentialHandle,
        ) -> Result<Vec<SubAccountRecord>, String> {
            Ok(self.rows.clone())
        }
    }

    fn row(email: &str, id: Option<&str>) -> SubAccountRecord {
        SubAccountRecord {
            email: email.into(),
            is_freeze: Some(false),
            is_master: Some(false),
            sub_account_id: id.map(|id| VenueIdentifier::parse(&Value::String(id.into())).unwrap()),
        }
    }

    #[tokio::test]
    async fn maps_sub_email_only_by_exact_id_or_explicit_unique_email() {
        let api = FixtureApi {
            uids: Mutex::new(vec!["100".into(), "200".into()]),
            rows: vec![
                row("child@example.invalid", Some("200")),
                row("other@example.invalid", Some("300")),
            ],
        };
        let resolved = resolve_binance_identity(&api, None, None, None)
            .await
            .unwrap();
        assert_eq!(resolved.sub_email, "child@example.invalid");
        assert!(resolved.sub_email_matched_by_venue_id);

        let no_id_api = FixtureApi {
            uids: Mutex::new(vec!["100".into(), "200".into()]),
            rows: vec![row("child@example.invalid", None)],
        };
        assert!(
            resolve_binance_identity(&no_id_api, None, None, None)
                .await
                .unwrap_err()
                .contains("GOFIN_DEV_BINANCE_SUB_EMAIL")
        );

        let explicit_api = FixtureApi {
            uids: Mutex::new(vec!["100".into(), "200".into()]),
            rows: vec![row("child@example.invalid", None)],
        };
        let explicit =
            resolve_binance_identity(&explicit_api, Some("child@example.invalid"), None, None)
                .await
                .unwrap();
        assert!(!explicit.sub_email_matched_by_venue_id);
    }

    #[tokio::test]
    async fn rejects_duplicate_identity_matches_and_inexact_uids() {
        let rows = vec![
            row("a@example.invalid", Some("200")),
            row("b@example.invalid", Some("200")),
        ];
        let api = FixtureApi {
            uids: Mutex::new(vec!["100".into(), "200".into()]),
            rows,
        };
        assert!(
            resolve_binance_identity(&api, None, None, None)
                .await
                .is_err()
        );
        let same_id = FixtureApi {
            uids: Mutex::new(vec!["100".into(), "100".into()]),
            rows: vec![row("child@example.invalid", Some("100"))],
        };
        assert!(
            resolve_binance_identity(&same_id, None, None, None)
                .await
                .is_err()
        );
        assert!(
            parse_account_uid(&serde_json::json!({"accountType":"SPOT","balances":[],"uid":1.5}))
                .is_err()
        );
    }

    #[tokio::test]
    async fn standalone_sub_discovery_uses_only_its_own_key_and_rejects_identity_swap() {
        struct SubOnlyApi;
        #[async_trait]
        impl BinanceIdentityApi for SubOnlyApi {
            async fn account_uid(&self, credential: &CredentialHandle) -> Result<String, String> {
                assert_eq!(credential.as_str(), "dev-import-sub");
                Ok("200".into())
            }
            async fn sub_accounts(
                &self,
                _: &CredentialHandle,
            ) -> Result<Vec<SubAccountRecord>, String> {
                panic!("standalone discovery must not require a main key");
            }
        }
        assert_eq!(resolve_sub_uid(&SubOnlyApi, None).await.unwrap(), "200");
        assert_eq!(
            resolve_sub_uid(&SubOnlyApi, Some("200")).await.unwrap(),
            "200"
        );
        assert!(resolve_sub_uid(&SubOnlyApi, Some("201")).await.is_err());
    }

    #[tokio::test]
    async fn main_only_discovery_validates_the_expected_uid() {
        let api = FixtureApi {
            uids: Mutex::new(vec!["100".into()]),
            rows: Vec::new(),
        };
        assert_eq!(resolve_main_uid(&api, Some("100")).await.unwrap(), "100");
        let mismatch = FixtureApi {
            uids: Mutex::new(vec!["100".into()]),
            rows: Vec::new(),
        };
        assert!(resolve_main_uid(&mismatch, Some("200")).await.is_err());
    }
}
