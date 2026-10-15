//! Administrative venue changes use fresh protected configuration approval only.
use crate::{ProtectedCredentialProvider, load_protected_config, lock_protected_configuration};
use gofin_domain::{GoFinError, Result, binding_hash, now};
use gofin_presentation::Ui;
use gofin_protocol::ConfigureRequest;
use gofin_store::Store;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
const JOURNAL_PATH: &str = "/Library/GoFin/private/administration.sqlite3";
fn storage() -> GoFinError {
    GoFinError::new(
        "CONFIGURATION_JOURNAL_UNAVAILABLE",
        "configuration",
        "The protected configuration journal could not be read or durably updated.",
        "Keep the original request ID. Inspect protected storage before retrying.",
    )
}
fn identity() -> GoFinError {
    GoFinError::new(
        "REQUEST_ID_CONFLICT",
        "configuration",
        "This configuration request ID already binds different parameters or account identity.",
        "Use the original exact command to inspect its outcome; review a new intent with a new ID.",
    )
}
fn validate_request(request: &ConfigureRequest) -> Result<()> {
    if request.request_id.is_empty()
        || request.request_id.len() > 128
        || !request
            .request_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
    {
        return Err(GoFinError::validation(
            "Configuration --id must contain 1–128 letters, numbers, underscores, dashes, periods or colons.",
        ));
    }
    Ok(())
}
fn create_attempt(
    store: &mut Store,
    request: &ConfigureRequest,
    binding: &Value,
) -> Result<(Value, bool)> {
    validate_request(request)?;
    let fingerprint = binding_hash(&json!({"request":request,"binding":binding}))?;
    let id = format!("configuration:{}", uuid::Uuid::new_v4());
    let record = json!({"version":1,"id":id,"request_id":request.request_id,"fingerprint":fingerprint,"operation":request.operation,"status":"unapproved","input":request.input,"binding":binding,"created_at":now(),"updated_at":now(),"financial_plan_authority":false});
    let row = store
        .create_request(
            &id,
            &request.request_id,
            &fingerprint,
            &request.operation,
            None,
            "unapproved",
            &record,
            &now(),
        )
        .map_err(|e| {
            if matches!(e, gofin_store::StoreError::RequestConflict) {
                identity()
            } else {
                storage()
            }
        })?;
    Ok((row.record, row.created))
}
fn update(store: &mut Store, record: &mut Value, status: &str) -> Result<()> {
    record["status"] = status.into();
    record["updated_at"] = now().into();
    store
        .update_record(record["id"].as_str().ok_or_else(storage)?, record, &now())
        .map_err(|_| storage())
}
fn public_record(record: &Value) -> Value {
    json!({"id":record["id"],"requestId":record["request_id"],"operation":record["operation"],"status":record["status"],"account":record["binding"]["account"],"product":record["binding"]["product"],"verification":record["verification"],"data":record["data"],"observation":record["observation"],"nextAction":if matches!(record["status"].as_str(),Some("dispatch_started"|"unknown")){"Run gofin status with this configuration attempt ID. Status reads never replay the change."}else if record["status"]=="unapproved"{"Repeat the original gofin configure command with the same --id to request fresh approval."}else{"Read the matching venue settings to inspect their current state."}})
}
pub(crate) fn is_kraken_operation(operation: &str) -> bool {
    operation.starts_with("kraken.") || operation.starts_with("krakenfutures.")
}

/// Closed administrative preview: no journal, approval, or mutation transport.
pub(crate) async fn preview(
    profile: &gofin_kraken::KrakenProfile,
    operation: &str,
    input: &Value,
) -> Result<gofin_domain::PreparedOperation> {
    let binding = profile.configuration_binding(operation, input)?;
    profile.preflight_configuration(operation, input).await?;
    let observed = chrono::Utc::now();
    Ok(gofin_domain::PreparedOperation {
        operation: operation.into(),
        input: input.clone(),
        summary: format!("Configure {} for {}", operation, binding["account"].as_str().unwrap_or("")),
        checks: vec![gofin_domain::Check {
            name: "configuration permissions".into(),
            status: gofin_domain::CheckStatus::Passed,
            detail: "The typed administrative input and current native key permissions were verified. No change was submitted.".into(),
        }],
        warnings: vec![binding["warning"].as_str().unwrap_or("").into()],
        binding,
        observed_at: observed.to_rfc3339(),
        expires_at: (observed + chrono::Duration::minutes(5)).to_rfc3339(),
        valuation: None,
        intent: gofin_domain::FinancialIntent::default(),
        operation_version: 1,
        action_deadline: None,
    })
}

pub async fn handle_with_ui(request: ConfigureRequest, ui: &Ui) -> Result<Value> {
    // Validate terminal ownership before loading credentials or opening transport.
    gofin_authorization::validate_installation()?;
    let _lock = lock_protected_configuration(true)?;
    let config = load_protected_config()?;
    gofin_human_terminal::HumanTerminal::open(config.approver_uid)?.finish()?;
    validate_request(&request)?;
    let credentials = Arc::new(ProtectedCredentialProvider::open()?);
    let profiles = crate::kraken::profiles(&config, credentials, None, None)?;
    let profile = profiles
        .iter()
        .find(|p| {
            gofin_exchange::ExchangeProfile::definitions(*p)
                .iter()
                .any(|d| d.name == request.operation)
        })
        .ok_or_else(|| {
            GoFinError::validation("Select a supported typed Kraken configuration operation.")
        })?;
    let mut binding = profile.configuration_binding(&request.operation, &request.input)?;
    binding["request_id"] = request.request_id.clone().into();
    binding["configuration_hash"] =
        binding_hash(&serde_json::to_value(&config).map_err(|_| storage())?)?.into();
    let path = Path::new(JOURNAL_PATH);
    if path.exists() {
        gofin_authorization::validate_metadata(path, true)?;
    }
    let mut journal = Store::open(path).map_err(|_| storage())?;
    gofin_authorization::validate_metadata(path, true)?;
    let (mut record, _) = create_attempt(&mut journal, &request, &binding)?;
    if record["status"] != "unapproved" {
        if matches!(
            record["status"].as_str(),
            Some("dispatch_started" | "unknown")
        ) {
            let _progress = ui.progress("Checking the original configuration attempt");
            record["observation"] = profile
                .observe_configuration(&request.operation, &request.input)
                .await
                .unwrap_or_else(|failure| json!({"level":"unresolved","errorCode":failure.code}));
            update(&mut journal, &mut record, "unknown")?;
        }
        return Ok(public_record(&record));
    }
    {
        let _progress = ui.progress("Checking venue configuration permissions");
        profile
            .preflight_configuration(&request.operation, &request.input)
            .await?;
    }
    let challenge = crate::enrollment::approve_and_audit(
        config.approver_uid,
        &binding,
        &format!(
            "Configure {} for {}",
            request.operation,
            binding["account"].as_str().unwrap_or("")
        ),
        ui,
    )
    .await?;
    gofin_authorization::validate_challenge(&challenge, &now())?;
    record["approval_operation_id"] = challenge.operation_id.clone().into();
    record["approval_binding_hash"] = challenge.binding_hash.clone().into();
    // The durable boundary precedes transport dispatch. A crash from this point never replays.
    update(&mut journal, &mut record, "dispatch_started")?;
    let guarded = challenge.clone();
    let guard = Arc::new(move || gofin_authorization::validate_challenge(&guarded, &now()).is_ok());
    let response = {
        let _progress = ui.progress("Applying approved venue configuration");
        profile
            .dispatch_configuration(&request.operation, &request.input, guard)
            .await
    };
    match response {
        Ok(value) => {
            record["data"] = value;
            record["verification"] =
                json!({"level":"venue_acknowledgment","financial_settlement":false});
            update(&mut journal, &mut record, "acknowledged")?;
        }
        Err(failure) => {
            record["verification"] = json!({"level":"unresolved","errorCode":failure.code,"requestMayHaveBeenSent":true});
            update(&mut journal, &mut record, "unknown")?;
        }
    }
    Ok(public_record(&record))
}

/// Existing socket status is read-only with respect to venue configuration.
pub(crate) async fn status(config: &crate::BrokerConfigV2, id: &str) -> Result<Value> {
    let path = Path::new(JOURNAL_PATH);
    if !path.exists() {
        return Err(GoFinError::validation(
            "This configuration attempt does not exist.",
        ));
    }
    gofin_authorization::validate_metadata(path, true)?;
    let mut store = Store::open(path).map_err(|_| storage())?;
    let mut record = store
        .get_record(id)
        .map_err(|_| storage())?
        .ok_or_else(|| GoFinError::validation("This configuration attempt does not exist."))?;
    if matches!(
        record["status"].as_str(),
        Some("dispatch_started" | "unknown")
    ) {
        let profiles = ProtectedCredentialProvider::open().and_then(|credentials| {
            crate::kraken::profiles(config, Arc::new(credentials), None, None)
        });
        let profiles = match profiles {
            Ok(profiles) => profiles,
            Err(failure) => {
                record["observation"] = json!({"level":"unresolved","errorCode":failure.code,"reason":"The original attempt remains saved; its observer is unavailable."});
                update(&mut store, &mut record, "unknown")?;
                return Ok(public_record(&record));
            }
        };
        let operation = record["operation"].as_str().ok_or_else(storage)?;
        if let Some(profile) = profiles.iter().find(|p| {
            gofin_exchange::ExchangeProfile::definitions(*p)
                .iter()
                .any(|d| d.name == operation)
        }) {
            let current = profile.configuration_binding(operation, &record["input"]);
            if current.as_ref().is_ok_and(|current| {
                current["resource"] == record["binding"]["resource"]
                    && current["physical_identity"] == record["binding"]["physical_identity"]
            }) {
                record["observation"] = profile
                    .observe_configuration(operation, &record["input"])
                    .await
                    .unwrap_or_else(
                        |failure| json!({"level":"unresolved","errorCode":failure.code}),
                    );
            } else {
                record["observation"] = json!({"level":"unresolved","reason":"The original physical account is no longer enrolled."});
            }
        }
        update(&mut store, &mut record, "unknown")?;
    }
    Ok(public_record(&record))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identities_survive_restarts_and_never_create_second_attempt() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("admin.sqlite");
        let request = ConfigureRequest {
            request_id: "synthetic-config".into(),
            operation: "krakenfutures.set-self-trade-strategy".into(),
            input: json!({"account":"test","strategy":"REJECT_TAKER"}),
        };
        let binding = json!({"account":"test","parameters":{"strategy":"REJECT_TAKER"}});
        let mut store = Store::open(&path).unwrap();
        let (mut record, new) = create_attempt(&mut store, &request, &binding).unwrap();
        assert!(new);
        update(&mut store, &mut record, "dispatch_started").unwrap();
        drop(store);
        let mut store = Store::open(&path).unwrap();
        let (recovered, new) = create_attempt(&mut store, &request, &binding).unwrap();
        assert!(!new);
        assert_eq!(recovered["id"], record["id"]);
        assert_eq!(recovered["status"], "dispatch_started");
        assert_eq!(
            create_attempt(&mut store, &request, &json!({"account":"other"}))
                .unwrap_err()
                .code,
            "REQUEST_ID_CONFLICT"
        );
        assert_eq!(store.list_records().unwrap().len(), 1);
    }
    #[test]
    fn public_output_has_no_approval_receipt_or_private_binding() {
        let result = public_record(
            &json!({"id":"configuration:test","request_id":"test","status":"unknown","binding":{"account":"a","product":"kraken","credential_ref":"private-ref"},"approval_operation_id":"private-approval"}),
        );
        assert_eq!(result["status"], "unknown");
        assert!(!result.to_string().contains("private-ref"));
        assert!(!result.to_string().contains("private-approval"));
    }
    struct PreviewVenue {
        permission: bool,
        reads: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl gofin_kraken::KrakenTransport for PreviewVenue {
        async fn request(
            &self,
            endpoint: gofin_kraken::KrakenEndpoint,
            _account: Option<&gofin_kraken::KrakenAccount>,
            parameters: Value,
            guard: Option<gofin_exchange::kraken::DispatchGuard>,
        ) -> Result<gofin_kraken::KrakenResponse> {
            assert_eq!(
                endpoint,
                gofin_kraken::KrakenEndpoint::KrakenPrivatePostGetApiKeyInfo
            );
            assert!(guard.is_none());
            assert_eq!(parameters, json!({}));
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(gofin_kraken::KrakenResponse::Json(
                json!({"result":{"permissions":if self.permission {vec!["export-data"]}else{vec![]}}}),
            ))
        }
    }
    fn preview_profile(venue: Arc<PreviewVenue>) -> gofin_kraken::KrakenProfile {
        gofin_kraken::KrakenProfile::new(
            gofin_kraken::KrakenProduct::Spot,
            vec![gofin_kraken::KrakenAccount {
                name: "synthetic".into(),
                resource: gofin_domain::ResourceId("kraken:account:synthetic".into()),
                credential_ref: "synthetic-ref".into(),
                physical_identity: "synthetic".into(),
                identity_verified: false,
                wallet_account_id: None,
                default_wallet_account_id: None,
                product: gofin_kraken::KrakenProduct::Spot,
            }],
            venue,
        )
        .unwrap()
    }
    #[tokio::test]
    async fn administrative_dry_run_checks_closed_input_and_permissions_without_dispatch() {
        let venue = Arc::new(PreviewVenue {
            permission: true,
            reads: std::sync::atomic::AtomicUsize::new(0),
        });
        let profile = preview_profile(venue.clone());
        let operation = gofin_kraken::KrakenEndpoint::KrakenPrivatePostAddExport.operation();
        let input =
            json!({"account":"synthetic","report":"trades","description":"synthetic report"});
        let candidate = preview(&profile, operation, &input).await.unwrap();
        assert_eq!(candidate.operation, operation);
        assert_eq!(candidate.input, input);
        assert_eq!(candidate.binding["financial_movement"], false);
        assert_eq!(candidate.binding["future_financial_authority"], false);
        assert_eq!(candidate.intent, gofin_domain::FinancialIntent::default());
        assert!(candidate.valuation.is_none());
        assert_eq!(venue.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
        let mut invalid = input;
        invalid["unreviewedField"] = json!("forbidden");
        assert!(preview(&profile, operation, &invalid).await.is_err());
        assert!(
            preview(
                &profile,
                "kraken.add-order",
                &json!({"account":"synthetic"})
            )
            .await
            .is_err()
        );
        assert_eq!(venue.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
        let denied = preview_profile(Arc::new(PreviewVenue {
            permission: false,
            reads: std::sync::atomic::AtomicUsize::new(0),
        }));
        assert_eq!(
            preview(
                &denied,
                operation,
                &json!({"account":"synthetic","report":"trades","description":"synthetic"})
            )
            .await
            .unwrap_err()
            .code,
            "KRAKEN_PERMISSION_DENIED"
        );
    }
    #[test]
    fn common_configuration_route_matches_only_explicit_kraken_namespaces() {
        assert!(is_kraken_operation("kraken.create-subaccount"));
        assert!(is_kraken_operation("krakenfutures.set-self-trade-strategy"));
        assert!(!is_kraken_operation(
            "hyperliquid.account.set_agent_abstraction"
        ));
        assert!(!is_kraken_operation("krakenfake.create-subaccount"));
    }
}
