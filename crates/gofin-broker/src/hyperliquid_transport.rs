//! Broker-owned Hyperliquid CCXT transport and exact stablecoin price mapping.
use crate::{AssetValuator, BrokerCredentialProvider, error};
use async_trait::async_trait;
use gofin_domain::{AssetId, AssetPrice, GoFinError, SubmissionJournal};
use gofin_exchange::ccxt::HyperliquidExplorerRequest;
use gofin_exchange::ccxt::{CcxtClient, CcxtError, CcxtErrorKind, HyperliquidSigner, RequestGuard};
use gofin_hyperliquid::{HyperliquidAccount, HyperliquidPrices, HyperliquidTransport};
use std::sync::Arc;
use zeroize::Zeroizing;

pub struct HyperliquidCcxtTransport {
    client: Arc<CcxtClient>,
    credentials: Arc<dyn BrokerCredentialProvider>,
}

impl HyperliquidCcxtTransport {
    pub fn new(client: Arc<CcxtClient>, credentials: Arc<dyn BrokerCredentialProvider>) -> Self {
        Self {
            client,
            credentials,
        }
    }
}

#[async_trait]
impl HyperliquidTransport for HyperliquidCcxtTransport {
    async fn explorer(
        &self,
        request: HyperliquidExplorerRequest,
    ) -> Result<serde_json::Value, GoFinError> {
        self.client
            .hyperliquid_explorer(request)
            .await
            .map_err(map_ccxt_read)
    }

    async fn info(
        &self,
        request: gofin_exchange::ccxt::HyperliquidInfoRequest,
    ) -> Result<serde_json::Value, GoFinError> {
        self.client
            .hyperliquid_info(request)
            .await
            .map_err(map_ccxt_read)
    }

    async fn dispatch(
        &self,
        account: &HyperliquidAccount,
        envelope: &gofin_exchange::ccxt::HyperliquidActionEnvelope,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<serde_json::Value, GoFinError> {
        if envelope.vault_address.as_deref() != account.vault_address() {
            return Err(error(
                "HYPERLIQUID_SCOPE_MISMATCH",
                "signing",
                "The signed execution scope does not match the protected account configuration.",
                "Prepare a new operation against the reviewed Hyperliquid account scope.",
            ));
        }
        match &account.execution_scope {
            gofin_hyperliquid::HyperliquidExecutionScope::Owner => {}
            gofin_hyperliquid::HyperliquidExecutionScope::Subaccount { .. } => {
                let rows = self
                    .client
                    .hyperliquid_info(gofin_exchange::ccxt::HyperliquidInfoRequest::SubAccounts {
                        owner: account.owner.clone(),
                    })
                    .await
                    .map_err(map_ccxt)?;
                gofin_hyperliquid::verify_execution_scope(
                    &account.owner,
                    &account.execution_scope,
                    Some(&rows),
                    None,
                )?;
            }
            gofin_hyperliquid::HyperliquidExecutionScope::Vault { address } => {
                let details = self
                    .client
                    .hyperliquid_info(gofin_exchange::ccxt::HyperliquidInfoRequest::VaultDetails {
                        vault_address: address.clone(),
                        user: Some(account.owner.clone()),
                    })
                    .await
                    .map_err(map_ccxt)?;
                gofin_hyperliquid::verify_execution_scope(
                    &account.owner,
                    &account.execution_scope,
                    None,
                    Some(&details),
                )?;
            }
        }
        if !account.signing_enabled {
            return Err(error(
                "SIGNING_NOT_ENABLED",
                "signing",
                "This Hyperliquid connection is read-only.",
                "Enroll a signing owner connection in your own Terminal before submitting actions.",
            ));
        }
        let reference = account.credential_ref.as_deref().ok_or_else(|| {
            error(
                "SIGNER_UNAVAILABLE",
                "signing",
                "The Hyperliquid signer mapping is unavailable.",
                "Repair the protected connection through human enrollment.",
            )
        })?;
        let signer = self
            .credentials
            .hyperliquid_signer(reference, &account.owner)
            .map_err(|cause| {
                GoFinError::new(
                    "HYPERLIQUID_PREDISPATCH_CREDENTIAL_FAILURE",
                    "pre-dispatch",
                    "The protected Hyperliquid signer was unavailable before action dispatch.",
                    "Repair the protected credential mapping, then reconcile the original operation before creating another write.",
                )
                .with_details(serde_json::json!({"cause_code":cause.code,"cause_stage":cause.stage}))
            })?;
        let guard: RequestGuard = Arc::new(move || journal.assert_authorized().is_ok());
        self.client
            .hyperliquid_action(envelope.clone(), signer, guard)
            .await
            .map_err(map_ccxt)
    }
}

fn map_ccxt_read(cause: CcxtError) -> GoFinError {
    let read_failure = match cause.kind {
        CcxtErrorKind::Timeout => Some((
            "VENUE_READ_TIMEOUT",
            "Hyperliquid did not answer the typed read before its timeout.",
            "Retry the read when the venue is available.",
        )),
        CcxtErrorKind::Rejected => Some((
            "VENUE_READ_REJECTED",
            "Hyperliquid rejected the typed read request.",
            "Check the operation's native endpoint mapping and current venue availability.",
        )),
        _ => None,
    };
    let Some((code, message, action)) = read_failure else {
        return map_ccxt(cause);
    };
    let mut failure = GoFinError::new(code, "read", message, action);
    if let Some(status) = cause.http_status {
        failure = failure.with_details(serde_json::json!({"http_status":status}));
    }
    failure
}

fn map_ccxt(cause: CcxtError) -> GoFinError {
    let (code, stage, message, action) = match cause.kind {
        CcxtErrorKind::PermissionDenied | CcxtErrorKind::Authentication => (
            "VENUE_ACCESS_DENIED",
            "authentication",
            "Hyperliquid rejected protected credentials or the request identity.",
            "Check the explicitly enrolled owner and protected signing key.",
        ),
        CcxtErrorKind::RateLimited => (
            "VENUE_RATE_LIMITED",
            "read",
            "Hyperliquid rate limits prevented the request.",
            "Wait for the venue limit window before retrying.",
        ),
        CcxtErrorKind::Timeout => (
            "VENUE_TIMEOUT",
            "submission",
            "The Hyperliquid request timed out; its outcome may be uncertain.",
            "Reconcile the original operation before considering another action.",
        ),
        CcxtErrorKind::Rejected => (
            "HYPERLIQUID_SUBMISSION_UNCERTAIN",
            "submission",
            "Hyperliquid returned a rejection without an authoritative receipt bound to this submission, so its outcome remains unresolved.",
            "Reconcile the original submission; do not retry or create another request until the outcome is resolved.",
        ),
        CcxtErrorKind::InvalidRequest => (
            "VENUE_REQUEST_INVALID",
            "validation",
            "The typed Hyperliquid request could not be encoded.",
            "Correct the operation input and prepare a new request.",
        ),
        CcxtErrorKind::Unsupported => (
            "VENUE_UNSUPPORTED",
            "validation",
            "This Hyperliquid capability is unavailable in the compiled transport.",
            "Check the installed capability definitions.",
        ),
        _ => (
            "VENUE_UNAVAILABLE",
            "read",
            "Hyperliquid did not return a usable response.",
            "Check venue availability; reconcile an action request before retrying.",
        ),
    };
    let mut details = serde_json::Map::new();
    if let Some(status) = cause.http_status {
        details.insert("http_status".into(), status.into());
    }
    if let Some(exchange_code) = cause.exchange_code {
        details.insert("exchange_code".into(), exchange_code.into());
    }
    let failure = GoFinError::new(code, stage, message, action);
    if details.is_empty() {
        failure
    } else {
        failure.with_details(details.into())
    }
}

/// The upstream USD price is obtained for native Arbitrum USDC, then relabeled
/// with Hyperliquid's own asset identity and decimal unit. No parity is inferred.
pub struct HyperliquidUsdPrices {
    valuator: Arc<dyn AssetValuator>,
}

impl HyperliquidUsdPrices {
    pub fn new(valuator: Arc<dyn AssetValuator>) -> Self {
        Self { valuator }
    }
}

#[async_trait]
impl HyperliquidPrices for HyperliquidUsdPrices {
    async fn usdc_price(&self) -> Result<AssetPrice, GoFinError> {
        const ARBITRUM_USDC: &str = "0xaf88d065e77c8cc2239327c5edb3a432268e5831";
        let source_asset = AssetId {
            network: "ethereum:42161".into(),
            reference: ARBITRUM_USDC.into(),
            decimals: 6,
        };
        let mut quote = self.valuator.asset_price(&source_asset).await?;
        if quote.asset != source_asset {
            return Err(GoFinError::new(
                "PRICE_DATA_INVALID",
                "valuation",
                "The USDC price source returned a different asset identity.",
                "Retry after obtaining an exact Arbitrum USDC quote.",
            ));
        }
        quote.asset = gofin_hyperliquid::usdc();
        quote.unit_atoms = gofin_domain::Quantity::new(format!("1{}", "0".repeat(36)))?;
        Ok(quote)
    }
}

/// Creates the transient CCXT signer value while the credential-store callback
/// is active. The private key remains zeroizing and is never formatted/logged.
pub fn signer_from_key(private_key: &str, owner: &str) -> Result<HyperliquidSigner, GoFinError> {
    let parsed = gofin_evm::LocalEnvelopeSigner::from_private_key(private_key)?;
    let derived = format!("{:#x}", parsed.address()).to_ascii_lowercase();
    if !derived.eq_ignore_ascii_case(owner) {
        return Err(GoFinError::new(
            "SIGNER_ADDRESS_MISMATCH",
            "signing",
            "The protected key does not derive the configured Hyperliquid owner.",
            "Correct the owner or key through protected human enrollment.",
        ));
    }
    Ok(HyperliquidSigner {
        private_key: Zeroizing::new(private_key.to_owned()),
        wallet_address: derived,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_errors_do_not_claim_a_financial_submission() {
        let read = map_ccxt_read(CcxtError::new(CcxtErrorKind::Timeout));
        assert_eq!(read.code, "VENUE_READ_TIMEOUT");
        assert_eq!(read.stage, "read");
        let rejected = map_ccxt_read(CcxtError::new(CcxtErrorKind::Rejected));
        assert_eq!(rejected.code, "VENUE_READ_REJECTED");
        assert_eq!(rejected.stage, "read");
        let write = map_ccxt(CcxtError::new(CcxtErrorKind::Timeout));
        assert_eq!(write.code, "VENUE_TIMEOUT");
        assert_eq!(write.stage, "submission");
    }

    #[test]
    fn rejected_write_without_bound_receipt_is_reported_as_unresolved() {
        let mut cause = CcxtError::new(CcxtErrorKind::Rejected);
        cause.http_status = Some(200);
        cause.exchange_code = Some(1001);
        let error = map_ccxt(cause);
        assert_eq!(error.code, "HYPERLIQUID_SUBMISSION_UNCERTAIN");
        assert_eq!(error.stage, "submission");
        assert!(error.message.contains("outcome remains unresolved"));
        assert!(error.next_action.contains("do not retry"));
        let safe_json = serde_json::to_value(error).unwrap();
        assert_eq!(safe_json["code"], "HYPERLIQUID_SUBMISSION_UNCERTAIN");
        assert!(
            safe_json["next_action"]
                .as_str()
                .unwrap()
                .contains("Reconcile")
        );
        assert_eq!(safe_json["details"]["http_status"], 200);
        assert_eq!(safe_json["details"]["exchange_code"], 1001);
        assert!(safe_json.get("response_body").is_none());
    }
    use gofin_domain::{GoFinError, Submission};
    use gofin_exchange::ccxt::{
        CcxtError, CcxtErrorKind, Credentials, HyperliquidAction, HyperliquidActionEnvelope,
    };
    use gofin_hyperliquid::{HyperliquidAccount, HyperliquidExecutionScope};
    use std::{
        net::SocketAddr,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    const OWNER: &str = "0x1111111111111111111111111111111111111111";
    const VAULT: &str = "0x2222222222222222222222222222222222222222";
    const OTHER_VAULT: &str = "0x3333333333333333333333333333333333333333";
    const SYNTHETIC_KEY_ONE: &str =
        "0x0000000000000000000000000000000000000000000000000000000000000001";

    struct CredentialProbe {
        signer_calls: AtomicUsize,
        key: &'static str,
    }

    struct DefaultCredentialFailure;

    impl gofin_exchange::ccxt::CredentialProvider for DefaultCredentialFailure {
        fn credentials(&self, _reference: &str) -> Result<Credentials, CcxtError> {
            Err(CcxtError::new(CcxtErrorKind::PermissionDenied))
        }
    }

    impl BrokerCredentialProvider for DefaultCredentialFailure {
        fn envelope_signer(
            &self,
            _credential: &gofin_credentials::CredentialRef,
        ) -> gofin_domain::Result<(String, Arc<dyn gofin_evm::EnvelopeSigner>)> {
            Err(GoFinError::validation("unused synthetic EVM signer"))
        }
    }

    impl CredentialProbe {
        fn new(key: &'static str) -> Self {
            Self {
                signer_calls: AtomicUsize::new(0),
                key,
            }
        }
    }

    impl gofin_exchange::ccxt::CredentialProvider for CredentialProbe {
        fn credentials(&self, _reference: &str) -> Result<Credentials, CcxtError> {
            Err(CcxtError::new(CcxtErrorKind::PermissionDenied))
        }
    }

    impl BrokerCredentialProvider for CredentialProbe {
        fn envelope_signer(
            &self,
            _credential: &gofin_credentials::CredentialRef,
        ) -> gofin_domain::Result<(String, Arc<dyn gofin_evm::EnvelopeSigner>)> {
            Err(GoFinError::validation("unused synthetic EVM signer"))
        }

        fn hyperliquid_signer(
            &self,
            _credential: &str,
            owner: &str,
        ) -> gofin_domain::Result<gofin_exchange::ccxt::HyperliquidSigner> {
            self.signer_calls.fetch_add(1, Ordering::SeqCst);
            signer_from_key(self.key, owner)
        }
    }

    #[derive(Default)]
    struct JournalProbe {
        checks: AtomicUsize,
    }

    impl SubmissionJournal for JournalProbe {
        fn assert_authorized(&self) -> gofin_domain::Result<()> {
            self.checks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn persist_submission(&self, _submission: &Submission) -> gofin_domain::Result<()> {
            Ok(())
        }

        fn persist_dispatch_evidence(
            &self,
            _submission: &Submission,
            _evidence: &serde_json::Value,
        ) -> gofin_domain::Result<()> {
            Ok(())
        }
    }

    fn account(scope: HyperliquidExecutionScope) -> HyperliquidAccount {
        HyperliquidAccount {
            name: "synthetic-hl".into(),
            owner: OWNER.into(),
            credential_ref: Some("synthetic-credential-ref".into()),
            signing_enabled: true,
            execution_scope: scope,
        }
    }

    fn envelope(vault_address: Option<&str>) -> HyperliquidActionEnvelope {
        HyperliquidActionEnvelope {
            nonce_ms: 1_800_000_000_000,
            vault_address: vault_address.map(str::to_owned),
            action: HyperliquidAction::UpdateLeverage {
                asset: 0,
                leverage: 2,
                is_cross: true,
            },
        }
    }

    struct LocalHttpFixture {
        endpoint: String,
        requests: Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    async fn local_fixture(response: String) -> LocalHttpFixture {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address: SocketAddr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let task = tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 2048];
            loop {
                let Ok(read) = stream.read(&mut chunk).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                bytes.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&bytes);
                let Some((head, body)) = text.split_once("\r\n\r\n") else {
                    continue;
                };
                let content_length = head
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if body.len() >= content_length {
                    captured.lock().unwrap().push(text.into_owned());
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response.len(),
                        response
                    );
                    let _ = stream.write_all(reply.as_bytes()).await;
                    return;
                }
            }
        });
        LocalHttpFixture {
            endpoint: format!("http://{address}"),
            requests,
            task,
        }
    }

    #[tokio::test]
    async fn dispatch_rejects_arbitrary_owner_and_changed_vault_targets_before_credentials_or_network()
     {
        let fixture = local_fixture(r#"{"status":"ok","response":{}}"#.into()).await;
        let client = Arc::new(CcxtClient::for_test(&fixture.endpoint).unwrap());
        let credentials = Arc::new(CredentialProbe::new(SYNTHETIC_KEY_ONE));
        let transport = HyperliquidCcxtTransport::new(client, credentials.clone());
        let journal = Arc::new(JournalProbe::default());

        for (account, submitted_target) in [
            (account(HyperliquidExecutionScope::Owner), Some(VAULT)),
            (
                account(HyperliquidExecutionScope::Vault {
                    address: VAULT.into(),
                }),
                Some(OTHER_VAULT),
            ),
        ] {
            let error = transport
                .dispatch(&account, &envelope(submitted_target), journal.clone())
                .await
                .unwrap_err();
            assert_eq!(error.code, "HYPERLIQUID_SCOPE_MISMATCH");
        }

        assert_eq!(credentials.signer_calls.load(Ordering::SeqCst), 0);
        assert_eq!(journal.checks.load(Ordering::SeqCst), 0);
        assert!(fixture.requests.lock().unwrap().is_empty());
        fixture.task.abort();
    }

    #[tokio::test]
    async fn fresh_vault_relationship_failure_precedes_signer_and_write_dispatch() {
        let response = format!(
            r#"{{"type":"vaultDetails","data":{{"vaultAddress":"{VAULT}","leader":"{OTHER_VAULT}"}}}}"#
        );
        let fixture = local_fixture(response).await;
        let client = Arc::new(CcxtClient::for_test(&fixture.endpoint).unwrap());
        let credentials = Arc::new(CredentialProbe::new(SYNTHETIC_KEY_ONE));
        let transport = HyperliquidCcxtTransport::new(client, credentials.clone());
        let journal = Arc::new(JournalProbe::default());
        let account = account(HyperliquidExecutionScope::Vault {
            address: VAULT.into(),
        });

        let error = transport
            .dispatch(&account, &envelope(Some(VAULT)), journal.clone())
            .await
            .unwrap_err();
        assert_eq!(error.code, "HYPERLIQUID_SCOPE_UNVERIFIED");
        assert_eq!(credentials.signer_calls.load(Ordering::SeqCst), 0);
        assert_eq!(journal.checks.load(Ordering::SeqCst), 0);
        {
            let requests = fixture.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].starts_with("POST /info "));
        }
        fixture.task.await.unwrap();
    }

    #[tokio::test]
    async fn matching_owner_scope_still_derives_signer_address_before_dispatch() {
        let fixture = local_fixture(r#"{"status":"ok","response":{}}"#.into()).await;
        let client = Arc::new(CcxtClient::for_test(&fixture.endpoint).unwrap());
        let credentials = Arc::new(CredentialProbe::new(SYNTHETIC_KEY_ONE));
        let transport = HyperliquidCcxtTransport::new(client, credentials.clone());
        let journal = Arc::new(JournalProbe::default());

        let error = transport
            .dispatch(
                &account(HyperliquidExecutionScope::Owner),
                &envelope(None),
                journal.clone(),
            )
            .await
            .unwrap_err();

        assert_eq!(error.code, "HYPERLIQUID_PREDISPATCH_CREDENTIAL_FAILURE");
        assert_eq!(error.stage, "pre-dispatch");
        assert_eq!(
            error.details.as_deref().unwrap()["cause_code"],
            "SIGNER_ADDRESS_MISMATCH"
        );
        assert_eq!(credentials.signer_calls.load(Ordering::SeqCst), 1);
        assert_eq!(journal.checks.load(Ordering::SeqCst), 0);
        assert!(fixture.requests.lock().unwrap().is_empty());
        tokio::time::timeout(Duration::from_millis(10), fixture.task)
            .await
            .unwrap_err();
    }

    #[tokio::test]
    async fn protected_credential_lookup_failure_is_typed_and_never_posts_action() {
        let fixture = local_fixture(r#"{"status":"ok","response":{}}"#.into()).await;
        let client = Arc::new(CcxtClient::for_test(&fixture.endpoint).unwrap());
        let transport = HyperliquidCcxtTransport::new(client, Arc::new(DefaultCredentialFailure));
        let journal = Arc::new(JournalProbe::default());

        let error = transport
            .dispatch(
                &account(HyperliquidExecutionScope::Owner),
                &envelope(None),
                journal.clone(),
            )
            .await
            .unwrap_err();

        assert_eq!(error.code, "HYPERLIQUID_PREDISPATCH_CREDENTIAL_FAILURE");
        assert_eq!(error.stage, "pre-dispatch");
        assert_eq!(
            error.details.as_deref().unwrap()["cause_code"],
            "CREDENTIAL_TYPE_MISMATCH"
        );
        assert!(fixture.requests.lock().unwrap().is_empty());
        assert_eq!(journal.checks.load(Ordering::SeqCst), 0);
        fixture.task.abort();
    }
}
