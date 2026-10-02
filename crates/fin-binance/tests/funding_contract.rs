use fin_binance::{
    BinanceEndpoint, BinanceError, BinanceRequest, ExactDecimal, FundingCompleteTime,
    FundingHistoryRow, HistoryWindow, StandardWithdrawalIntent, TransferHistoryResponse,
    UniversalTransferIntent, VenueIdentifier, parse_response, signed_preimage,
};
use serde_json::{Value, json};

fn amount(value: &str) -> ExactDecimal {
    ExactDecimal::parse(value).expect("fixture amount is exact")
}

#[test]
fn exact_decimal_keeps_fractional_scale_and_compares_without_rounding() {
    let fine = amount("0.000000000000000000000000000123");
    assert_eq!(fine.scale(), 30);
    assert_eq!(fine.canonical(), "0.000000000000000000000000000123");
    assert_eq!(
        fine.checked_cmp(&amount("0.0000000000000000000000000001230")),
        std::cmp::Ordering::Equal
    );
    assert!(amount("1.2000").is_multiple_of(&amount("0.0001")));
    assert!(!amount("1.2001").is_multiple_of(&amount("0.0002")));
}

#[test]
fn exact_decimal_rejects_exponents_signs_and_overprecision() {
    for value in [
        "1e-8",
        "+1",
        "-1",
        "01.2",
        "1.",
        ".1",
        "1.0000000000000000000000000000000000000",
    ] {
        assert_eq!(
            ExactDecimal::parse(value),
            Err(BinanceError::InvalidDecimal),
            "{value}"
        );
    }
}

#[test]
fn client_ids_are_stable_operation_scoped_and_binance_sized() {
    let transfer = fin_binance::ClientIdentity::transfer("request-000001").unwrap();
    let same = fin_binance::ClientIdentity::transfer("request-000001").unwrap();
    let withdrawal = fin_binance::ClientIdentity::withdrawal("request-000001").unwrap();
    assert_eq!(transfer, same);
    assert_ne!(transfer, withdrawal);
    assert_eq!(transfer.as_str().len(), 32);
    assert!(transfer.as_str().starts_with("fin"));
}

#[test]
fn transfer_request_is_spot_only_and_keeps_exact_amount_as_text() {
    let request = UniversalTransferIntent {
        asset: "ETH".into(),
        amount: amount("0.123456789012345678901234567890"),
        from_email: Some("child@example.invalid".into()),
        to_email: None,
        client_id: fin_binance::ClientIdentity::transfer("request-000001").unwrap(),
    }
    .request()
    .unwrap();
    assert_eq!(
        request.endpoint.method_path(),
        ("POST", "/sapi/v1/sub-account/universalTransfer")
    );
    assert_eq!(
        request.get("amount"),
        Some("0.12345678901234567890123456789")
    );
    assert_eq!(request.get("fromAccountType"), Some("SPOT"));
    assert_eq!(request.get("toAccountType"), Some("SPOT"));
    assert!(request.get("clientTranId").unwrap().starts_with("fin"));

    let both_subaccounts = UniversalTransferIntent {
        asset: "ETH".into(),
        amount: amount("1"),
        from_email: Some("from@example.invalid".into()),
        to_email: Some("to@example.invalid".into()),
        client_id: fin_binance::ClientIdentity::transfer("request-000002").unwrap(),
    };
    assert_eq!(
        both_subaccounts.request(),
        Err(BinanceError::InvalidRequest)
    );
}

#[test]
fn withdrawal_request_freezes_eth_spot_and_exact_quantity() {
    let request = StandardWithdrawalIntent {
        coin: "ETH".into(),
        amount: amount("0.005000000000000001"),
        address: "0x1111111111111111111111111111111111111111".into(),
        client_id: fin_binance::ClientIdentity::withdrawal("request-000001").unwrap(),
    }
    .request()
    .unwrap();
    assert_eq!(
        request.endpoint.method_path(),
        ("POST", "/sapi/v1/capital/withdraw/apply")
    );
    assert_eq!(request.get("amount"), Some("0.005000000000000001"));
    assert_eq!(request.get("network"), Some("ETH"));
    assert_eq!(request.get("walletType"), Some("0"));

    let mut arbitrary = BinanceRequest::new(BinanceEndpoint::StandardWithdrawal)
        .push("coin", "ETH")
        .push("withdrawOrderId", "finfixture")
        .push("network", "ETH")
        .push("address", "0x1111111111111111111111111111111111111111")
        .push("amount", "0.1")
        .push("walletType", "0")
        .push("sideEffectType", "MARGIN_BUY");
    assert_eq!(
        arbitrary.validate(),
        Err(BinanceError::UnsupportedParameter)
    );
    arbitrary.params.pop();
    assert!(arbitrary.validate().is_ok());
}

#[test]
fn withdrawal_allowlist_endpoint_is_fixed_get_only_and_malformed_whitelist_is_unknown() {
    let request = BinanceRequest::new(BinanceEndpoint::WithdrawAddressList);
    assert_eq!(
        request.endpoint.method_path(),
        ("GET", "/sapi/v1/capital/withdraw/address/list")
    );
    assert!(request.validate().is_ok());
    assert_eq!(
        BinanceRequest::new(BinanceEndpoint::WithdrawAddressList)
            .push("address", "0x1111111111111111111111111111111111111111")
            .validate(),
        Err(BinanceError::UnsupportedParameter)
    );
    assert_eq!(
        BinanceRequest::new(BinanceEndpoint::WithdrawAddressList)
            .push("path", "/arbitrary")
            .validate(),
        Err(BinanceError::UnsupportedParameter)
    );

    let rows: Vec<fin_binance::WithdrawAddressListEntry> = fin_binance::parse_response(
        serde_json::from_str(include_str!("fixtures/withdraw-address-list.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0].whitelist_status,
        fin_binance::WithdrawWhitelistStatus::Enabled
    );
    assert_eq!(
        rows[1].whitelist_status,
        fin_binance::WithdrawWhitelistStatus::Unknown
    );
    assert_eq!(
        rows[2].whitelist_status,
        fin_binance::WithdrawWhitelistStatus::Unknown
    );
    assert_eq!(
        rows[0].origin_type,
        Some(fin_binance::WithdrawAddressOriginType::Label(
            "others".into()
        ))
    );
    assert_eq!(
        rows[1].origin_type,
        Some(fin_binance::WithdrawAddressOriginType::LegacyCode(1))
    );
    assert!(
        fin_binance::parse_response::<Vec<fin_binance::WithdrawAddressListEntry>>(json!([{
            "address":"synthetic-address","originType":true,"whiteStatus":false
        }]))
        .is_err(),
        "invalid origin metadata isn't coerced into a supported value"
    );
    // This plumbing doesn't publish a read operation that would expose full addresses.
    assert!(
        fin_binance::operation_definitions()
            .iter()
            .all(|definition| { definition.name != "binance.withdraw-address-list" })
    );
}

#[test]
fn signature_preimage_is_rfc3986_encoded_and_decimal_string_exact() {
    let request = UniversalTransferIntent {
        asset: "ETH".into(),
        amount: amount("0.000000000000000000123456"),
        from_email: None,
        to_email: Some("child+tag@example.invalid".into()),
        client_id: fin_binance::ClientIdentity::transfer("request-000001").unwrap(),
    }
    .request()
    .unwrap();
    let preimage = String::from_utf8(signed_preimage(&request.params)).unwrap();
    assert!(preimage.contains("amount=0.000000000000000000123456"));
    assert!(preimage.contains("toEmail=child%2Btag%40example.invalid"));
}

#[test]
fn history_window_rejects_inverted_range_and_uses_pagination_identity() {
    let id = fin_binance::ClientIdentity::transfer("request-000001").unwrap();
    let request = HistoryWindow {
        start_time_ms: 1,
        end_time_ms: 2,
    }
    .request(BinanceEndpoint::UniversalTransferHistory, None, Some(&id))
    .unwrap();
    assert_eq!(request.get("startTime"), Some("1"));
    assert_eq!(request.get("endTime"), Some("2"));
    assert_eq!(request.get("page"), Some("1"));
    assert_eq!(request.get("limit"), Some("500"));
    assert_eq!(request.get("clientTranId"), Some(id.as_str()));
    assert_eq!(
        HistoryWindow {
            start_time_ms: 2,
            end_time_ms: 1
        }
        .request(BinanceEndpoint::DepositHistory, None, None),
        Err(BinanceError::InvalidRequest)
    );
}

#[test]
fn fixtures_preserve_raw_balances_transfer_history_and_withdrawal_fees() {
    let balances: Value =
        serde_json::from_str(include_str!("fixtures/spot-balances.json")).unwrap();
    let rows = balances["balances"].as_array().unwrap();
    assert_eq!(rows[0]["free"].as_str(), Some("1.000000000000000123456789"));
    assert_eq!(
        rows[0]["locked"].as_str(),
        Some("0.000000000000000000000001")
    );

    let history: TransferHistoryResponse =
        serde_json::from_str(include_str!("fixtures/universal-transfer-history.json")).unwrap();
    assert_eq!(history.total_count, Some(1));
    assert_eq!(history.result.len(), 1);
    assert_eq!(
        history.result[0].amount.canonical(),
        "0.12345678901234567890123456789"
    );
    assert_eq!(history.result[0].tran_id.as_str(), "8439201");
    assert_eq!(history.result[0].status, "SUCCESS");

    let withdrawals: Vec<FundingHistoryRow> =
        serde_json::from_str(include_str!("fixtures/withdrawal-history.json")).unwrap();
    assert_eq!(withdrawals[0].amount.canonical(), "0.005000000000000001");
    assert_eq!(
        withdrawals[0].complete_time,
        Some(FundingCompleteTime::UtcText("2026-09-01 12:30:45".into()))
    );
    assert_eq!(
        withdrawals[0].transaction_fee.as_ref().unwrap().canonical(),
        "0.00040000000000000001"
    );
    assert!(withdrawals[0].completed_external_withdrawal());
}

#[test]
fn numeric_financial_fields_and_inexact_identifiers_are_rejected() {
    let raw = json!({ "asset": "ETH", "free": 0.1, "locked": "0" });
    assert!(parse_response::<fin_binance::SpotBalanceRow>(raw).is_err());
    assert!(VenueIdentifier::parse(&json!(1.1)).is_err());
    assert!(VenueIdentifier::parse(&json!(-1)).is_err());
    assert_eq!(
        VenueIdentifier::parse(&json!(123456789u64))
            .unwrap()
            .as_str(),
        "123456789"
    );
}

#[test]
fn deposit_credit_is_distinct_from_chain_submission_and_locked_balance() {
    let credited: FundingHistoryRow = serde_json::from_value(json!({
        "id": "deposit-credited",
        "coin": "ETH",
        "amount": "0.01",
        "status": 1,
        "travelRuleStatus": 0
    }))
    .unwrap();
    assert!(credited.usable_deposit_credit());
    assert!(!credited.locked_deposit_credit());

    let locked: FundingHistoryRow = serde_json::from_value(json!({
        "id": "deposit-lock",
        "coin": "ETH",
        "amount": "0.01",
        "status": 6
    }))
    .unwrap();
    assert!(!locked.usable_deposit_credit());
    assert!(locked.locked_deposit_credit());
}

#[test]
fn travel_rule_credit_state_accepts_only_missing_or_known_clear_integer_state() {
    let row = |travel_rule_status: Option<Value>| {
        let mut raw = json!({
            "id":"deposit-travel-rule",
            "coin":"ETH",
            "amount":"0.01",
            "status":1
        });
        if let Some(value) = travel_rule_status {
            raw["travelRuleStatus"] = value;
        }
        serde_json::from_value::<FundingHistoryRow>(raw)
    };

    assert!(row(None).unwrap().usable_deposit_credit());
    assert!(row(Some(json!(0))).unwrap().usable_deposit_credit());
    assert!(!row(Some(json!(1))).unwrap().usable_deposit_credit());
    assert!(!row(Some(json!(2))).unwrap().usable_deposit_credit());
    for malformed in [json!("1"), json!(true), json!(1.5), json!(-1)] {
        assert!(row(Some(malformed)).is_err());
    }
}

struct FundingHistoryShapeTransport {
    deposit_rows: Value,
    withdrawal_rows: Value,
}

#[async_trait::async_trait]
impl fin_binance::BinanceTransport for FundingHistoryShapeTransport {
    async fn request(
        &self,
        _credential: &fin_binance::CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, fin_binance::TransportError> {
        match request.endpoint {
            BinanceEndpoint::DepositHistory => Ok(self.deposit_rows.clone()),
            BinanceEndpoint::WithdrawalHistory => Ok(self.withdrawal_rows.clone()),
            endpoint => panic!("unexpected history endpoint {:?}", endpoint),
        }
    }
}

#[tokio::test]
async fn deposit_and_withdrawal_history_enforce_their_documented_complete_time_types() {
    use fin_domain::Adapter;

    let now = chrono::Utc::now().timestamp_millis() as u64;
    let valid_deposit = json!([{
        "id":"deposit-1", "coin":"ETH", "amount":"0.01", "status":1,
        "insertTime":now, "completeTime":now, "network":"ETH", "txId":"0xabc"
    }]);
    let valid_withdrawal = json!([{
        "id":"withdraw-1", "coin":"ETH", "amount":"0.01", "status":6,
        "insertTime":now, "completeTime":"2026-09-01 12:30:45", "network":"ETH"
    }]);
    let adapter = fin_binance::BinanceAdapter::new(
        one_main_config(),
        std::sync::Arc::new(FundingHistoryShapeTransport {
            deposit_rows: valid_deposit,
            withdrawal_rows: valid_withdrawal,
        }),
        None,
        None,
    )
    .unwrap();
    let deposit = adapter
        .read(
            "binance.deposit-history",
            &json!({"account":"main-fixture"}),
        )
        .await
        .unwrap();
    assert_eq!(deposit["records"][0]["completeTime"], json!(now));
    assert_eq!(deposit["records"][0]["transactionFee"], Value::Null);
    let withdrawal = adapter
        .read(
            "binance.withdrawal-history",
            &json!({"account":"main-fixture"}),
        )
        .await
        .unwrap();
    assert_eq!(
        withdrawal["records"][0]["completeTime"],
        "2026-09-01 12:30:45"
    );

    for invalid_deposit_time in [
        json!("2026-09-01 12:30:45"),
        json!(true),
        json!(-1),
        json!(1.5),
    ] {
        let invalid = fin_binance::BinanceAdapter::new(
            one_main_config(),
            std::sync::Arc::new(FundingHistoryShapeTransport {
                deposit_rows: json!([{
                    "id":"deposit-1", "coin":"ETH", "amount":"0.01", "status":1,
                    "insertTime":now, "completeTime":invalid_deposit_time
                }]),
                withdrawal_rows: json!([]),
            }),
            None,
            None,
        )
        .unwrap();
        assert!(
            invalid
                .read(
                    "binance.deposit-history",
                    &json!({"account":"main-fixture"})
                )
                .await
                .is_err()
        );
    }

    for invalid_withdrawal_time in [
        json!(now),
        json!("not-a-time"),
        json!("2026-09-01T12:30:45Z"),
        json!("2026-9-1 2:3:4"),
    ] {
        let invalid = fin_binance::BinanceAdapter::new(
            one_main_config(),
            std::sync::Arc::new(FundingHistoryShapeTransport {
                deposit_rows: json!([]),
                withdrawal_rows: json!([{
                    "id":"withdraw-1", "coin":"ETH", "amount":"0.01", "status":6,
                    "insertTime":now, "completeTime":invalid_withdrawal_time
                }]),
            }),
            None,
            None,
        )
        .unwrap();
        assert!(
            invalid
                .read(
                    "binance.withdrawal-history",
                    &json!({"account":"main-fixture"})
                )
                .await
                .is_err()
        );
    }

    assert!(serde_json::from_str::<FundingHistoryRow>(
        r#"{"id":"bad","coin":"ETH","amount":"0.01","status":1,"completeTime":18446744073709551616}"#
    )
    .is_err());
}

#[derive(Default)]
struct RecordingTransport {
    responses: std::sync::Mutex<Vec<std::result::Result<Value, fin_binance::TransportError>>>,
    requests: std::sync::Mutex<Vec<(fin_binance::CredentialHandle, BinanceRequest)>>,
}

#[async_trait::async_trait]
impl fin_binance::BinanceTransport for RecordingTransport {
    async fn request(
        &self,
        credential: &fin_binance::CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, fin_binance::TransportError> {
        self.requests
            .lock()
            .unwrap()
            .push((credential.clone(), request));
        if self.responses.lock().unwrap().is_empty() {
            return Err(fin_binance::TransportError::InvalidResponse);
        }
        self.responses.lock().unwrap().remove(0)
    }
}

fn one_main_config() -> fin_binance::BinanceConfig {
    fin_binance::BinanceConfig {
        accounts: vec![fin_binance::BinanceAccount {
            name: "main-fixture".into(),
            resource: fin_domain::ResourceId("binance:synthetic-main".into()),
            credential: fin_binance::CredentialHandle::new("test-only:synthetic-main").unwrap(),
            role: fin_binance::AccountRole::Main,
            parent: None,
            email: None,
            wallet_type: "SPOT".into(),
        }],
        destinations: vec![],
    }
}

#[tokio::test]
async fn adapter_balance_read_uses_fixed_account_endpoint_and_preserves_exact_strings() {
    use fin_domain::Adapter;
    let transport = std::sync::Arc::new(RecordingTransport::default());
    transport
        .responses
        .lock()
        .unwrap()
        .push(Ok(serde_json::from_str(include_str!(
            "fixtures/spot-balances.json"
        ))
        .unwrap()));
    let adapter =
        fin_binance::BinanceAdapter::new(one_main_config(), transport.clone(), None, None).unwrap();
    let result = adapter
        .read("binance.balances", &json!({"account":"main-fixture"}))
        .await
        .unwrap();
    assert_eq!(result["walletType"], "SPOT");
    assert_eq!(result["balances"][0]["free"], "1.000000000000000123456789");
    assert_eq!(
        result["balances"][0]["locked"],
        "0.000000000000000000000001"
    );
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0.as_str(), "test-only:synthetic-main");
    assert_eq!(
        requests[0].1.endpoint.method_path(),
        ("GET", "/api/v3/account")
    );
    assert!(requests[0].1.params.is_empty());
}

#[tokio::test]
async fn adapter_fails_closed_on_numeric_quantity_and_sanitizes_timeout() {
    use fin_domain::Adapter;
    let transport = std::sync::Arc::new(RecordingTransport::default());
    transport
        .responses
        .lock()
        .unwrap()
        .push(Ok(json!({"balances":[{
            "asset":"ETH", "free":0.1, "locked":"0"
        }]})));
    transport
        .responses
        .lock()
        .unwrap()
        .push(Err(fin_binance::TransportError::Timeout));
    transport
        .responses
        .lock()
        .unwrap()
        .push(Err(fin_binance::TransportError::InvalidResponse));
    let adapter =
        fin_binance::BinanceAdapter::new(one_main_config(), transport, None, None).unwrap();
    let malformed = adapter
        .read("binance.balances", &json!({"account":"main-fixture"}))
        .await
        .unwrap_err();
    assert_eq!(malformed.code, "VENUE_RESPONSE_INVALID");
    let timeout = adapter
        .read("binance.permissions", &json!({"account":"main-fixture"}))
        .await
        .unwrap_err();
    assert_eq!(timeout.code, "VENUE_TIMEOUT");
    assert!(!timeout.message.contains("test-only"));
    let invalid_response = adapter
        .read("binance.permissions", &json!({"account":"main-fixture"}))
        .await
        .unwrap_err();
    assert_eq!(invalid_response.code, "VENUE_RESPONSE_INVALID");
    assert!(
        invalid_response
            .message
            .contains("reconcile any persisted submission")
    );
}

#[tokio::test]
async fn safe_connection_metadata_excludes_private_credential_handles() {
    use fin_domain::Adapter;
    let adapter = fin_binance::BinanceAdapter::new(
        one_main_config(),
        std::sync::Arc::new(RecordingTransport::default()),
        None,
        None,
    )
    .unwrap();
    let safe = serde_json::to_value(adapter.connections()).unwrap();
    assert_eq!(safe[0]["name"], "main-fixture");
    assert_eq!(safe[0]["role"], "main");
    assert_eq!(safe[0]["provider"], "binance");
    assert_eq!(safe[0]["credential_ref"], Value::Null);
    assert!(safe.to_string().find("test-only").is_none());
    assert!(safe[0].get("email").is_none());
}

#[tokio::test]
async fn conditional_funding_preview_requires_sources_and_is_never_routable_as_a_write() {
    use fin_domain::Adapter;
    let adapter = fin_binance::BinanceAdapter::new(
        one_main_config(),
        std::sync::Arc::new(RecordingTransport::default()),
        None,
        None,
    )
    .unwrap();
    let withdrawal = json!({
        "requestId":"synthetic-conditional",
        "account":"main-fixture",
        "destination":"wallet-fixture",
        "coin":"ETH",
        "amount":"0.01",
        "network":"ETH",
        "walletType":"SPOT",
        "maxFee":"0.001",
        "minReceiveAmount":"0.009"
    });
    let error = adapter
        .preview_withdrawal_after_funding(&withdrawal, vec![])
        .await
        .unwrap_err();
    assert_eq!(error.code, "INVALID_INPUT");
    let public_write = adapter
        .prepare("binance.conditional-withdraw", &withdrawal)
        .await;
    assert!(public_write.is_err());
}

#[test]
fn operation_definitions_are_available_offline_and_writes_remain_agent_plannable() {
    let definitions = fin_binance::operation_definitions();
    assert!(
        definitions
            .iter()
            .any(|definition| definition.name == "binance.withdraw")
    );
    assert!(
        definitions
            .iter()
            .any(|definition| definition.name == "binance.deposit-history")
    );
    assert!(definitions.iter().all(|definition| {
        definition.execution_actor == fin_domain::ExecutionActor::AgentInvocable
    }));
    assert!(
        !definitions
            .iter()
            .any(|definition| definition.name.contains("signed-method"))
    );
}

#[tokio::test]
async fn snapshot_never_claims_completeness_after_observation_failures() {
    use fin_domain::Adapter;
    let adapter = fin_binance::BinanceAdapter::new(
        one_main_config(),
        std::sync::Arc::new(RecordingTransport::default()),
        None,
        None,
    )
    .unwrap();
    let snapshot = adapter.snapshot().await.unwrap();
    assert!(!snapshot.complete);
    assert!(
        snapshot
            .pending
            .iter()
            .any(|item| item.contains("balance observation unavailable"))
    );
    let connections = adapter.connections();
    assert_eq!(connections.len(), 1);
    assert_eq!(connections[0].provider, "binance");
    assert_eq!(connections[0].credential_ref, None);
    assert!(connections[0].details.get("walletType").is_some());
}

struct TransferFixtureTransport {
    events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    timeout_transfer: bool,
    transfer_history_status: &'static str,
}

#[derive(Clone, Copy)]
enum TransferDirection {
    MainToChild,
    ChildToMain,
}

struct RouteAwareTransferTransport {
    direction: TransferDirection,
    main_free: std::sync::Mutex<String>,
    child_free: std::sync::Mutex<String>,
    posts: std::sync::atomic::AtomicUsize,
    history_queries: std::sync::Mutex<Vec<Vec<(String, String)>>>,
    client_id: std::sync::Mutex<Option<String>>,
}

struct DiagnosticTransferTransport {
    inner: RouteAwareTransferTransport,
    posts: std::sync::atomic::AtomicUsize,
    history_reads: std::sync::atomic::AtomicUsize,
    client_id: std::sync::Mutex<Option<String>>,
}

impl RouteAwareTransferTransport {
    fn new(direction: TransferDirection) -> Self {
        let (main_free, child_free) = match direction {
            TransferDirection::MainToChild => ("1", "0"),
            TransferDirection::ChildToMain => ("0", "1"),
        };
        Self {
            direction,
            main_free: std::sync::Mutex::new(main_free.into()),
            child_free: std::sync::Mutex::new(child_free.into()),
            posts: std::sync::atomic::AtomicUsize::new(0),
            history_queries: std::sync::Mutex::new(Vec::new()),
            client_id: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl fin_binance::BinanceTransport for RouteAwareTransferTransport {
    async fn request(
        &self,
        _credential: &fin_binance::CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, fin_binance::TransportError> {
        use fin_binance::BinanceEndpoint as E;
        match request.endpoint {
            E::SubAccountList => Ok(json!({"subAccounts":[{
                "email":"child@example.invalid", "isFreeze":false, "isMaster":false
            }]})),
            E::ApiRestrictions => Ok(json!({
                "enableReading":true, "enableWithdrawals":true,
                "enableInternalTransfer":true, "permitsUniversalTransfer":true
            })),
            E::CapitalConfig => Ok(json!([{
                "coin":"ETH", "withdrawAllEnable":true, "depositAllEnable":true,
                "networkList":[{
                    "network":"ETH", "coin":"ETH", "depositEnable":true,
                    "withdrawEnable":true, "withdrawIntegerMultiple":"0.00000100",
                    "withdrawFee":"0.0004", "withdrawMin":"0.001", "withdrawMax":"1000",
                    "withdrawTag":false, "busy":false
                }]
            }])),
            E::SubAccountSpotAssets => Ok(json!({"balances":[{
                "asset":"ETH", "free":self.child_free.lock().unwrap().clone(), "locked":"0"
            }]})),
            E::Account => Ok(json!({"balances":[{
                "asset":"ETH", "free":self.main_free.lock().unwrap().clone(), "locked":"0"
            }]})),
            E::UniversalTransfer => {
                self.posts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let id = request.get("clientTranId").unwrap().to_owned();
                *self.client_id.lock().unwrap() = Some(id);
                match self.direction {
                    TransferDirection::MainToChild => {
                        *self.main_free.lock().unwrap() = "0.75".into();
                        *self.child_free.lock().unwrap() = "0.25".into();
                    }
                    TransferDirection::ChildToMain => {
                        *self.main_free.lock().unwrap() = "0.25".into();
                        *self.child_free.lock().unwrap() = "0.75".into();
                    }
                }
                // Simulate Binance accepting the write while the HTTP response
                // is lost. Reconciliation must use the original clientTranId.
                Err(fin_binance::TransportError::Timeout)
            }
            E::UniversalTransferHistory => {
                let params = request
                    .params
                    .iter()
                    .map(|p| (p.key.clone(), p.value.clone()))
                    .collect::<Vec<_>>();
                let from = request.get("fromEmail");
                let to = request.get("toEmail");
                match self.direction {
                    TransferDirection::ChildToMain => {
                        assert_eq!(from, Some("child@example.invalid"))
                    }
                    TransferDirection::MainToChild => assert_eq!(to, Some("child@example.invalid")),
                }
                assert_ne!(
                    from.is_some(),
                    to.is_some(),
                    "history must use exactly one route selector"
                );
                self.history_queries.lock().unwrap().push(params);
                let (from_email, to_email) = match self.direction {
                    TransferDirection::ChildToMain => (Some("child@example.invalid"), None),
                    TransferDirection::MainToChild => (None, Some("child@example.invalid")),
                };
                Ok(json!({"totalCount":1,"result":[{
                    "tranId":123456789,
                    "clientTranId":self.client_id.lock().unwrap().clone(),
                    "asset":"ETH", "amount":"0.25", "fromAccountType":"SPOT",
                    "toAccountType":"SPOT", "fromEmail":from_email, "toEmail":to_email,
                    "status":"SUCCESS", "createTimeStamp":1
                }]}))
            }
            endpoint => panic!("unexpected synthetic endpoint {endpoint:?}"),
        }
    }
}

#[async_trait::async_trait]
impl fin_binance::BinanceTransport for DiagnosticTransferTransport {
    async fn request(
        &self,
        credential: &fin_binance::CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, fin_binance::TransportError> {
        if request.endpoint == BinanceEndpoint::UniversalTransfer {
            self.posts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            *self.client_id.lock().unwrap() = request.get("clientTranId").map(str::to_owned);
            return Err(fin_binance::TransportError::HttpFailure {
                status: 400,
                binance_code: Some(-2010),
                kind: fin_binance::HttpFailureKind::Rejected,
            });
        }
        if request.endpoint == BinanceEndpoint::UniversalTransferHistory {
            assert_ne!(
                request.get("fromEmail").is_some(),
                request.get("toEmail").is_some()
            );
            let read = self
                .history_reads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if read == 0 {
                return Ok(json!({"totalCount":1,"result":[{
                    "tranId":123456789,
                    "clientTranId":self.client_id.lock().unwrap().clone(),
                    "asset":"ETH", "amount":"0.25", "fromAccountType":"SPOT",
                    "toAccountType":"SPOT", "fromEmail":"child@example.invalid",
                    "status":"PENDING", "createTimeStamp":1
                }]}));
            }
            return Ok(json!({"totalCount":0,"result":[]}));
        }
        fin_binance::BinanceTransport::request(&self.inner, credential, request).await
    }
}

#[async_trait::async_trait]
impl fin_binance::BinanceTransport for TransferFixtureTransport {
    async fn request(
        &self,
        _credential: &fin_binance::CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, fin_binance::TransportError> {
        use fin_binance::BinanceEndpoint as E;
        self.events
            .lock()
            .unwrap()
            .push(format!("request:{:?}", request.endpoint));
        match request.endpoint {
            E::SubAccountList => Ok(json!({"subAccounts":[{
                "email":"child@example.invalid", "isFreeze":false, "isMaster":false
            }]})),
            E::ApiRestrictions => Ok(json!({
                "enableReading":true, "enableWithdrawals":true,
                "enableInternalTransfer":true, "permitsUniversalTransfer":true
            })),
            E::CapitalConfig => Ok(json!([{
                "coin":"ETH", "withdrawAllEnable":true, "depositAllEnable":true,
                "networkList":[{
                    "network":"ETH", "coin":"ETH", "depositEnable":true,
                    "withdrawEnable":true, "withdrawIntegerMultiple":"0.00000100",
                    "withdrawFee":"0.0004", "withdrawMin":"0.001", "withdrawMax":"1000",
                    "withdrawTag":false, "busy":false
                }]
            }])),
            E::SubAccountSpotAssets => Ok(json!({"balances":[{
                "asset":"ETH", "free":"1.000000000000000123456789", "locked":"0"
            }]})),
            E::Account => Ok(json!({"balances":[{
                "asset":"ETH", "free":"0", "locked":"0"
            }]})),
            E::UniversalTransferHistory => Ok(json!({"totalCount":1,"result":[{
                "tranId":123456789,
                "clientTranId":request.get("clientTranId"),
                "asset":"ETH", "amount":"0.123456", "fromAccountType":"SPOT",
                "toAccountType":"SPOT", "fromEmail":"child@example.invalid",
                "status":self.transfer_history_status, "createTimeStamp":1
            }]})),
            E::UniversalTransfer if self.timeout_transfer => {
                Err(fin_binance::TransportError::Timeout)
            }
            E::UniversalTransfer => {
                Ok(json!({"tranId":123456789, "clientTranId":request.get("clientTranId")}))
            }
            endpoint => panic!("unexpected synthetic endpoint {endpoint:?}"),
        }
    }
}

struct RecordingJournal {
    events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    persisted: std::sync::Mutex<Vec<fin_domain::Submission>>,
}

impl fin_domain::SubmissionJournal for RecordingJournal {
    fn assert_authorized(&self) -> fin_domain::Result<()> {
        self.events.lock().unwrap().push("approval-check".into());
        Ok(())
    }

    fn persist_submission(&self, submission: &fin_domain::Submission) -> fin_domain::Result<()> {
        self.events
            .lock()
            .unwrap()
            .push("persist-submission".into());
        self.persisted.lock().unwrap().push(submission.clone());
        Ok(())
    }
}

fn main_and_child_config() -> fin_binance::BinanceConfig {
    fin_binance::BinanceConfig {
        accounts: vec![
            fin_binance::BinanceAccount {
                name: "main-fixture".into(),
                resource: fin_domain::ResourceId("binance:uid:1001:SPOT".into()),
                credential: fin_binance::CredentialHandle::new("test-only:synthetic-main").unwrap(),
                role: fin_binance::AccountRole::Main,
                parent: None,
                email: None,
                wallet_type: "SPOT".into(),
            },
            fin_binance::BinanceAccount {
                name: "child-fixture".into(),
                resource: fin_domain::ResourceId("binance:uid:1002:SPOT".into()),
                credential: fin_binance::CredentialHandle::new("test-only:synthetic-child")
                    .unwrap(),
                role: fin_binance::AccountRole::Sub,
                parent: Some("main-fixture".into()),
                email: Some("child@example.invalid".into()),
                wallet_type: "SPOT".into(),
            },
        ],
        destinations: vec![],
    }
}

async fn transfer_preview(adapter: &fin_binance::BinanceAdapter) -> fin_domain::PreparedOperation {
    use fin_domain::Adapter;
    adapter
        .prepare(
            "binance.transfer",
            &json!({
                "requestId":"synthetic-transfer-1", "authority":"main-fixture",
                "from":"child-fixture", "to":"main-fixture", "coin":"ETH",
                "amount":"0.123456", "fromWallet":"SPOT", "toWallet":"SPOT"
            }),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn transfer_submission_persists_client_identity_before_fixed_endpoint_dispatch() {
    use fin_domain::Adapter;
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let adapter = fin_binance::BinanceAdapter::new(
        main_and_child_config(),
        std::sync::Arc::new(TransferFixtureTransport {
            events: events.clone(),
            timeout_transfer: false,
            transfer_history_status: "SUCCESS",
        }),
        None,
        None,
    )
    .unwrap();
    let preview = transfer_preview(&adapter).await;
    assert_eq!(preview.binding["asset"]["decimals"], 36);
    let journal = std::sync::Arc::new(RecordingJournal {
        events: events.clone(),
        persisted: std::sync::Mutex::new(Vec::new()),
    });
    let submission = adapter.submit(&preview, journal.clone()).await.unwrap();
    assert_eq!(submission.reference, preview.binding["clientTranId"]);
    assert!(submission.data.get("venueId").is_none());
    let events = events.lock().unwrap();
    let dispatch = events
        .iter()
        .position(|e| e == "request:UniversalTransfer")
        .unwrap();
    let pre_dispatch_persist = events[..dispatch]
        .iter()
        .rposition(|e| e == "persist-submission")
        .unwrap();
    assert!(pre_dispatch_persist < dispatch);
    assert_eq!(journal.persisted.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn transfer_timeout_keeps_original_persisted_identity_for_reconciliation() {
    use fin_domain::Adapter;
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let adapter = fin_binance::BinanceAdapter::new(
        main_and_child_config(),
        std::sync::Arc::new(TransferFixtureTransport {
            events: events.clone(),
            timeout_transfer: true,
            transfer_history_status: "FAILED",
        }),
        None,
        None,
    )
    .unwrap();
    let preview = transfer_preview(&adapter).await;
    let journal = std::sync::Arc::new(RecordingJournal {
        events: events.clone(),
        persisted: std::sync::Mutex::new(Vec::new()),
    });
    let error = adapter.submit(&preview, journal.clone()).await.unwrap_err();
    assert_eq!(error.code, "VENUE_TIMEOUT");
    let persisted = journal.persisted.lock().unwrap();
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].reference, preview.binding["clientTranId"]);
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e == "request:UniversalTransfer")
    );
}

#[tokio::test]
async fn accepted_or_timed_out_write_reconciles_same_identity_without_replay() {
    use fin_domain::Adapter;
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let adapter = fin_binance::BinanceAdapter::new(
        main_and_child_config(),
        std::sync::Arc::new(TransferFixtureTransport {
            events: events.clone(),
            timeout_transfer: true,
            transfer_history_status: "FAILED",
        }),
        None,
        None,
    )
    .unwrap();
    let preview = transfer_preview(&adapter).await;
    let journal = std::sync::Arc::new(RecordingJournal {
        events,
        persisted: std::sync::Mutex::new(Vec::new()),
    });
    assert!(adapter.submit(&preview, journal.clone()).await.is_err());
    let original = journal.persisted.lock().unwrap()[0].clone();
    let record = fin_domain::OperationRecord {
        id: "synthetic-op-1".into(),
        request_id: "synthetic-transfer-1".into(),
        fingerprint: "synthetic-fingerprint".into(),
        fingerprint_version: 1,
        operation: "binance.transfer".into(),
        input: preview.input.clone(),
        status: fin_domain::Status::Submitted,
        created_at: fin_domain::now(),
        updated_at: fin_domain::now(),
        preview: Some(preview),
        approval: None,
        submission: Some(original.clone()),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let reconciled = adapter.reconcile(&record).await.unwrap();
    assert_eq!(reconciled.status, fin_domain::Status::Failed);
    let proof = reconciled.verified.unwrap();
    assert_eq!(proof.submission, original);
    assert_eq!(
        proof.proof_digest,
        fin_domain::verified_effect_digest(&proof).unwrap()
    );
    assert!(proof.effects.is_empty());
    assert_eq!(
        journal
            .persisted
            .lock()
            .unwrap()
            .iter()
            .map(|entry| entry.reference.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1
    );
}

#[tokio::test]
async fn transfer_history_uses_persisted_single_route_selector_after_restart() {
    use fin_domain::Adapter;

    for direction in [
        TransferDirection::MainToChild,
        TransferDirection::ChildToMain,
    ] {
        let transport = std::sync::Arc::new(RouteAwareTransferTransport::new(direction));
        let config = main_and_child_config();
        let adapter =
            fin_binance::BinanceAdapter::new(config.clone(), transport.clone(), None, None)
                .unwrap();
        let (from, to) = match direction {
            TransferDirection::MainToChild => ("main-fixture", "child-fixture"),
            TransferDirection::ChildToMain => ("child-fixture", "main-fixture"),
        };
        let preview = adapter
            .prepare(
                "binance.transfer",
                &json!({
                    "requestId":"synthetic-route-recovery",
                    "authority":"main-fixture", "from":from, "to":to,
                    "coin":"ETH", "amount":"0.25",
                    "fromWallet":"SPOT", "toWallet":"SPOT"
                }),
            )
            .await
            .unwrap();
        let journal = std::sync::Arc::new(RecordingJournal {
            events: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            persisted: std::sync::Mutex::new(Vec::new()),
        });
        assert_eq!(
            adapter
                .submit(&preview, journal.clone())
                .await
                .unwrap_err()
                .code,
            "VENUE_TIMEOUT"
        );
        let original = journal.persisted.lock().unwrap()[0].clone();
        assert_eq!(original.reference, preview.binding["clientTranId"]);
        let persisted_selector = &original.data["historySelector"];
        match direction {
            TransferDirection::MainToChild => {
                assert_eq!(
                    persisted_selector,
                    &json!({"parameter":"toEmail","value":"child@example.invalid"})
                );
            }
            TransferDirection::ChildToMain => {
                assert_eq!(
                    persisted_selector,
                    &json!({"parameter":"fromEmail","value":"child@example.invalid"})
                );
            }
        }
        let record = fin_domain::OperationRecord {
            id: "synthetic-transfer-restart".into(),
            request_id: "synthetic-route-recovery".into(),
            fingerprint: "synthetic-fingerprint".into(),
            fingerprint_version: 1,
            operation: "binance.transfer".into(),
            input: preview.input.clone(),
            status: fin_domain::Status::Submitted,
            created_at: fin_domain::now(),
            updated_at: fin_domain::now(),
            preview: Some(preview),
            approval: None,
            submission: Some(original.clone()),
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };

        // New adapter instance models a process restart; the journaled client
        // identity and route selector are the only write correlation inputs.
        let restarted =
            fin_binance::BinanceAdapter::new(config, transport.clone(), None, None).unwrap();
        let result = restarted.reconcile(&record).await.unwrap();
        assert_eq!(result.status, fin_domain::Status::Completed);
        let verified = result.verified.unwrap();
        assert_eq!(verified.submission, original);
        assert_eq!(verified.effects.len(), 1);
        assert_eq!(transport.posts.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Pre-selector durable records can derive the same filter from their
        // immutable typed write request. The original Submission is retained
        // byte-for-byte; reconciliation does not mint or rewrite an ID.
        let mut legacy = original.clone();
        legacy
            .data
            .as_object_mut()
            .unwrap()
            .remove("historySelector");
        let mut legacy_record = record.clone();
        legacy_record.submission = Some(legacy.clone());
        let legacy_result = restarted.reconcile(&legacy_record).await.unwrap();
        assert_eq!(legacy_result.status, fin_domain::Status::Completed);
        assert_eq!(legacy_result.verified.unwrap().submission, legacy);

        // A missing typed write request, an ambiguous pair of route filters,
        // or a changed client identity must fail before another history read.
        let mut malformed_records = Vec::new();
        let mut missing_params = record.clone();
        missing_params.submission.as_mut().unwrap().data["binding"]
            .as_object_mut()
            .unwrap()
            .remove("params");
        malformed_records.push(missing_params);
        let mut ambiguous_route = record.clone();
        let params =
            &mut ambiguous_route.submission.as_mut().unwrap().data["binding"]["params"]["params"];
        params.as_array_mut().unwrap().push(match direction {
            TransferDirection::MainToChild => {
                json!({"key":"fromEmail","value":"child@example.invalid"})
            }
            TransferDirection::ChildToMain => {
                json!({"key":"toEmail","value":"child@example.invalid"})
            }
        });
        malformed_records.push(ambiguous_route);
        let mut changed_id = record.clone();
        changed_id.submission.as_mut().unwrap().data["binding"]["params"]["params"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|param| param["key"] == "clientTranId")
            .unwrap()["value"] = json!("fin-mutated");
        malformed_records.push(changed_id);
        for malformed in malformed_records {
            let error = restarted.reconcile(&malformed).await.unwrap_err();
            assert_eq!(error.code, "RECONCILIATION_INVALID");
        }
        let queries = transport.history_queries.lock().unwrap();
        assert_eq!(queries.len(), 2);
        let route_keys = queries[0]
            .iter()
            .filter(|(key, _)| key == "fromEmail" || key == "toEmail")
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect::<Vec<_>>();
        match direction {
            TransferDirection::MainToChild => {
                assert_eq!(route_keys, vec![("toEmail", "child@example.invalid")])
            }
            TransferDirection::ChildToMain => {
                assert_eq!(route_keys, vec![("fromEmail", "child@example.invalid")])
            }
        }
        assert!(
            queries[0]
                .iter()
                .any(|(key, value)| key == "clientTranId" && value == &original.reference)
        );
    }
}

struct SyntheticApproval(std::sync::atomic::AtomicUsize);

struct StaticHttpFailure(fin_binance::TransportError);

#[async_trait::async_trait]
impl fin_binance::BinanceTransport for StaticHttpFailure {
    async fn request(
        &self,
        _credential: &fin_binance::CredentialHandle,
        _request: BinanceRequest,
    ) -> std::result::Result<Value, fin_binance::TransportError> {
        Err(self.0.clone())
    }
}

#[async_trait::async_trait]
impl fin_domain::Authorizer for SyntheticApproval {
    async fn authorize(
        &self,
        challenge: &fin_domain::AuthorizationChallenge,
    ) -> fin_domain::Result<fin_domain::ApprovalAudit> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(fin_domain::ApprovalAudit {
            mechanism: "test-only-synthetic-approval".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: fin_domain::now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

#[tokio::test]
async fn http_diagnostics_preserve_public_fin_error_classification() {
    use fin_domain::Adapter;
    let cases = [
        (
            fin_binance::HttpFailureKind::Rejected,
            400,
            Some(-2010),
            "VENUE_REQUEST_FAILED",
        ),
        (
            fin_binance::HttpFailureKind::PermissionDenied,
            403,
            None,
            "VENUE_PERMISSION",
        ),
        (
            fin_binance::HttpFailureKind::RateLimited,
            429,
            None,
            "VENUE_RATE_LIMIT",
        ),
        (
            fin_binance::HttpFailureKind::InvalidResponse,
            200,
            None,
            "VENUE_RESPONSE_INVALID",
        ),
    ];
    for (kind, status, binance_code, expected_code) in cases {
        let adapter = fin_binance::BinanceAdapter::new(
            main_and_child_config(),
            std::sync::Arc::new(StaticHttpFailure(
                fin_binance::TransportError::HttpFailure {
                    status,
                    binance_code,
                    kind,
                },
            )),
            None,
            None,
        )
        .unwrap();
        let error = adapter
            .prepare(
                "binance.transfer",
                &json!({
                    "requestId":"classification-test", "authority":"main-fixture",
                    "from":"child-fixture", "to":"main-fixture", "coin":"ETH",
                    "amount":"0.25", "fromWallet":"SPOT", "toWallet":"SPOT"
                }),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, expected_code);
        assert!(error.message.contains(&format!("HTTP {status}")));
        if let Some(code) = binance_code {
            assert!(error.message.contains(&format!("Binance code {code}")));
        }
    }
}

#[tokio::test]
async fn engine_persists_safe_http_diagnostic_through_unknown_status_without_replay() {
    let transport = std::sync::Arc::new(DiagnosticTransferTransport {
        inner: RouteAwareTransferTransport::new(TransferDirection::ChildToMain),
        posts: std::sync::atomic::AtomicUsize::new(0),
        history_reads: std::sync::atomic::AtomicUsize::new(0),
        client_id: std::sync::Mutex::new(None),
    });
    let adapter = std::sync::Arc::new(
        fin_binance::BinanceAdapter::new(main_and_child_config(), transport.clone(), None, None)
            .unwrap(),
    );
    let authorizer = std::sync::Arc::new(SyntheticApproval(std::sync::atomic::AtomicUsize::new(0)));
    let engine = fin_engine::Engine::new(
        fin_store::Store::in_memory().unwrap(),
        vec![adapter],
        authorizer.clone(),
        fin_engine::EngineOptions::default(),
    )
    .unwrap();
    let request = fin_domain::OperationRequest {
        request_id: "synthetic-http-recovery".into(),
        operation: "binance.transfer".into(),
        input: json!({
            "requestId":"synthetic-http-recovery", "authority":"main-fixture",
            "from":"child-fixture", "to":"main-fixture", "coin":"ETH",
            "amount":"0.25", "fromWallet":"SPOT", "toWallet":"SPOT"
        }),
        run_id: None,
    };
    let initial = engine.execute(request.clone()).await.unwrap();
    assert_eq!(initial.status, fin_domain::Status::Unknown);
    let original_submission = initial.submission.clone().unwrap();
    let original_error = initial.error.clone().unwrap();
    assert_eq!(original_error.code, "VENUE_REQUEST_FAILED");
    assert_eq!(
        original_error.message,
        "Binance rejected the request; reconcile any persisted submission before retrying. [HTTP 400; Binance code -2010]"
    );
    assert_eq!(transport.posts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(authorizer.0.load(std::sync::atomic::Ordering::SeqCst), 1);

    for expected_status in [fin_domain::Status::Pending, fin_domain::Status::Unknown] {
        let status = engine.status(&initial.id, true).await.unwrap();
        assert_eq!(status.status, expected_status);
        assert_eq!(status.error, Some(original_error.clone()));
        assert_eq!(status.submission, Some(original_submission.clone()));
        assert_eq!(transport.posts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    let duplicate = engine.execute(request).await.unwrap();
    assert_eq!(duplicate.id, initial.id);
    assert_eq!(duplicate.status, fin_domain::Status::Unknown);
    assert_eq!(duplicate.error, Some(original_error));
    assert_eq!(duplicate.submission, Some(original_submission));
    assert_eq!(transport.posts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(authorizer.0.load(std::sync::atomic::Ordering::SeqCst), 1);
}

struct EmptyHistorySnapshotTransport {
    accounts: std::sync::atomic::AtomicUsize,
    from_emails: std::sync::Mutex<Vec<Option<String>>>,
}

#[async_trait::async_trait]
impl fin_binance::BinanceTransport for EmptyHistorySnapshotTransport {
    async fn request(
        &self,
        _credential: &fin_binance::CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, fin_binance::TransportError> {
        use fin_binance::BinanceEndpoint as E;
        let response = match request.endpoint {
            E::Account => {
                let index = self
                    .accounts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut response: Value =
                    serde_json::from_str(include_str!("fixtures/spot-balances.json")).unwrap();
                response["accountType"] = json!("SPOT");
                response["uid"] = json!(if index < 2 { "1001" } else { "1002" });
                response
            }
            E::SubAccountSpotAssets => json!({"balances":[{
                "asset":"ETH", "free":"0.25", "locked":"0.000000000000000000000001"
            }]}),
            E::DepositHistory | E::WithdrawalHistory => json!([]),
            E::UniversalTransferHistory => {
                let from_email = request.get("fromEmail").map(str::to_owned);
                self.from_emails.lock().unwrap().push(from_email.clone());
                let rows = if from_email.as_deref() == Some("child@example.invalid") {
                    json!([{"tranId":12345u64,"asset":"ETH","amount":"0.1","fromAccountType":"SPOT","toAccountType":"SPOT","fromEmail":"child@example.invalid","toEmail":null,"status":"SUCCESS","createTimeStamp":1780000000000u64}])
                } else {
                    json!([])
                };
                json!({"totalCount":rows.as_array().unwrap().len(),"result":rows})
            }
            endpoint => panic!("unexpected snapshot endpoint {endpoint:?}"),
        };
        Ok(response)
    }
}

struct PagedTransferSnapshotTransport {
    accounts: std::sync::atomic::AtomicUsize,
    transfer_requests: std::sync::Mutex<Vec<(Option<String>, String)>>,
    total_count: usize,
}

#[async_trait::async_trait]
impl fin_binance::BinanceTransport for PagedTransferSnapshotTransport {
    async fn request(
        &self,
        _credential: &fin_binance::CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, fin_binance::TransportError> {
        use fin_binance::BinanceEndpoint as E;
        match request.endpoint {
            E::Account => {
                let index = self
                    .accounts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let uid = if index < 2 { "1001" } else { "1002" };
                Ok(
                    json!({"uid":uid,"accountType":"SPOT","balances":[{"asset":"ETH","free":"0.25","locked":"0"}]}),
                )
            }
            E::SubAccountSpotAssets => {
                Ok(json!({"balances":[{"asset":"ETH","free":"0.25","locked":"0"}]}))
            }
            E::DepositHistory | E::WithdrawalHistory => Ok(json!([])),
            E::UniversalTransferHistory => {
                let page = request.get("page").unwrap_or("1").parse::<usize>().unwrap();
                let from_email = request.get("fromEmail").map(str::to_owned);
                self.transfer_requests
                    .lock()
                    .unwrap()
                    .push((from_email, page.to_string()));
                let start = (page - 1) * 500;
                let count = self.total_count.saturating_sub(start).min(500);
                let source_id = request.get("fromEmail").unwrap_or("main");
                let result = (0..count)
                    .map(|offset| {
                        // Repeat the first immutable venue ID on a later page
                        // to verify the snapshot doesn't double-count it.
                        let index = if page > 1 && offset == 0 {
                            0
                        } else {
                            start + offset
                        };
                        json!({
                            "tranId":format!("synthetic-transfer-{source_id}-{index}"),
                            "asset":"ETH","amount":"0.001",
                            "fromAccountType":"SPOT","toAccountType":"SPOT",
                            "fromEmail":request.get("fromEmail"),"toEmail":null,
                            "status":if index == 0 { "PENDING" } else { "SUCCESS" },
                            "createTimeStamp":1780000000000u64
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(json!({"totalCount":self.total_count,"result":result}))
            }
            endpoint => panic!("unexpected snapshot endpoint {endpoint:?}"),
        }
    }
}

struct MismatchedUidTransport;

#[async_trait::async_trait]
impl fin_binance::BinanceTransport for MismatchedUidTransport {
    async fn request(
        &self,
        _credential: &fin_binance::CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, fin_binance::TransportError> {
        assert_eq!(request.endpoint, fin_binance::BinanceEndpoint::Account);
        Ok(json!({"uid":"9999","accountType":"SPOT","balances":[]}))
    }
}

#[tokio::test]
async fn account_identity_read_rejects_uid_mismatch_with_enrolled_resource() {
    use fin_domain::Adapter;
    let adapter = fin_binance::BinanceAdapter::new(
        main_and_child_config(),
        std::sync::Arc::new(MismatchedUidTransport),
        None,
        None,
    )
    .unwrap();
    let error = adapter
        .read(
            "binance.account-identity",
            &json!({"account":"main-fixture"}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "ACCOUNT_IDENTITY_MISMATCH");
}

#[tokio::test]
async fn snapshot_covers_exact_spot_amounts_and_bounded_reviewed_histories() {
    use fin_domain::Adapter;
    let transport = std::sync::Arc::new(EmptyHistorySnapshotTransport {
        accounts: std::sync::atomic::AtomicUsize::new(0),
        from_emails: std::sync::Mutex::new(Vec::new()),
    });
    let adapter =
        fin_binance::BinanceAdapter::new(main_and_child_config(), transport.clone(), None, None)
            .unwrap();
    let snapshot = adapter.snapshot().await.unwrap();
    assert!(snapshot.complete, "{:?}", snapshot.pending);
    assert!(snapshot.pending.is_empty());
    assert!(snapshot.coverage.contains(&"binance:uid:1001:SPOT".into()));
    assert!(snapshot.coverage.contains(&"binance:uid:1002:SPOT".into()));
    assert!(
        snapshot
            .coverage
            .iter()
            .all(|entry| entry.starts_with("binance:uid:"))
    );
    assert_eq!(
        *transport.from_emails.lock().unwrap(),
        vec![None, Some("child@example.invalid".into())]
    );
    assert_eq!(snapshot.state.holdings.len(), 3);
    assert!(
        snapshot
            .state
            .holdings
            .iter()
            .all(|holding| holding.amount.asset.decimals == 36)
    );
    assert!(snapshot.state.holdings.iter().any(|holding| {
        holding.resource.0 == "binance:uid:1002:SPOT"
            && holding.amount.atoms.to_decimal(36) == "0.250000000000000000000001"
    }));
}

#[tokio::test]
async fn universal_transfer_snapshot_paginates_each_source_and_marks_page_cap_incomplete() {
    use fin_domain::Adapter;

    let complete_transport = std::sync::Arc::new(PagedTransferSnapshotTransport {
        accounts: std::sync::atomic::AtomicUsize::new(0),
        transfer_requests: std::sync::Mutex::new(Vec::new()),
        total_count: 501,
    });
    let complete_adapter = fin_binance::BinanceAdapter::new(
        main_and_child_config(),
        complete_transport.clone(),
        None,
        None,
    )
    .unwrap();
    let complete = complete_adapter.snapshot().await.unwrap();
    assert!(!complete.complete, "pending venue transfers are visible");
    assert!(
        complete
            .pending
            .iter()
            .all(|item| !item.contains("history is incomplete")),
        "the second page should finish both histories: {:?}",
        complete.pending
    );
    assert_eq!(
        complete.state.in_flight.len(),
        2,
        "one pending row per source, not duplicate page rows"
    );
    {
        let queries = complete_transport.transfer_requests.lock().unwrap();
        assert!(queries.contains(&(None, "1".into())));
        assert!(queries.contains(&(None, "2".into())));
        assert!(queries.contains(&(Some("child@example.invalid".into()), "1".into())));
        assert!(queries.contains(&(Some("child@example.invalid".into()), "2".into())));
    }

    let capped_transport = std::sync::Arc::new(PagedTransferSnapshotTransport {
        accounts: std::sync::atomic::AtomicUsize::new(0),
        transfer_requests: std::sync::Mutex::new(Vec::new()),
        total_count: 10_001,
    });
    let capped_adapter = fin_binance::BinanceAdapter::new(
        main_and_child_config(),
        capped_transport.clone(),
        None,
        None,
    )
    .unwrap();
    let capped = capped_adapter.snapshot().await.unwrap();
    assert!(!capped.complete);
    assert!(
        capped
            .pending
            .iter()
            .any(|item| item.contains("history is incomplete"))
    );
    let queries = capped_transport.transfer_requests.lock().unwrap();
    assert!(
        queries
            .iter()
            .any(|(source, page)| source.is_none() && page == "20")
    );
    assert!(queries.iter().any(|(source, page)| {
        source.as_deref() == Some("child@example.invalid") && page == "20"
    }));
}

#[tokio::test]
async fn child_authority_transfer_is_explicitly_rejected_without_main_credential_substitution() {
    use fin_domain::Adapter;
    let transport = std::sync::Arc::new(RecordingTransport::default());
    let adapter =
        fin_binance::BinanceAdapter::new(main_and_child_config(), transport.clone(), None, None)
            .unwrap();
    let error = adapter
        .prepare(
            "binance.transfer",
            &json!({
                "requestId":"synthetic-child-authorized-transfer",
                "authority":"child-fixture",
                "from":"child-fixture","to":"main-fixture","coin":"ETH",
                "amount":"0.01","fromWallet":"SPOT","toWallet":"SPOT"
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "WRONG_AUTHORITY");
    assert!(transport.requests.lock().unwrap().is_empty());
}

struct WithdrawalFixtureTransport {
    travel_country_code: &'static str,
    withdrawal_permission: bool,
    withdraw_fee: &'static str,
    withdraw_min: &'static str,
    withdraw_increment: &'static str,
    free_balance: &'static str,
}

#[async_trait::async_trait]
impl fin_binance::BinanceTransport for WithdrawalFixtureTransport {
    async fn request(
        &self,
        _credential: &fin_binance::CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, fin_binance::TransportError> {
        use fin_binance::BinanceEndpoint as E;
        match request.endpoint {
            E::ApiRestrictions => Ok(json!({
                "enableReading":true,"enableWithdrawals":self.withdrawal_permission,
                "enableInternalTransfer":true,"permitsUniversalTransfer":true
            })),
            E::TravelRuleRequirements => {
                Ok(json!({"questionnaireCountryCode":self.travel_country_code}))
            }
            E::CapitalConfig => Ok(json!([{
                "coin":"ETH", "withdrawAllEnable":true, "depositAllEnable":true,
                "networkList":[{
                    "network":"ETH", "coin":"ETH", "depositEnable":true,
                    "withdrawEnable":true, "withdrawIntegerMultiple":self.withdraw_increment,
                    "withdrawFee":self.withdraw_fee, "withdrawMin":self.withdraw_min, "withdrawMax":"1000",
                    "withdrawTag":false, "busy":false
                }]
            }])),
            E::Account => Ok(json!({"balances":[{
                "asset":"ETH", "free":self.free_balance, "locked":"0.1"
            }]})),
            endpoint => panic!("unexpected withdrawal preview endpoint {endpoint:?}"),
        }
    }
}

struct SyntheticPriceProvider;

#[async_trait::async_trait]
impl fin_binance::BinancePriceProvider for SyntheticPriceProvider {
    async fn quote_usd(&self, _coin: &str) -> fin_domain::Result<fin_binance::PriceQuote> {
        Ok(fin_binance::PriceQuote {
            usd_per_unit: "2000.25".into(),
            observed_at: fin_domain::now(),
            source: "synthetic-fixture-price".into(),
        })
    }
}

struct SyntheticRecipientVerifier;

#[async_trait::async_trait]
impl fin_binance::BinanceRecipientVerifier for SyntheticRecipientVerifier {
    async fn verify(&self, _request: fin_binance::PayoutRequest) -> fin_binance::PayoutResult {
        fin_binance::PayoutResult {
            status: fin_domain::Status::Pending,
            amount_atoms: None,
            evidence: json!({"fixture":"synthetic recipient verifier"}),
        }
    }
}

#[tokio::test]
async fn withdrawal_preview_keeps_binance_debit_and_ethereum_credit_price_units_distinct() {
    use fin_domain::{AccountingLocation, Adapter, EffectConstraint};
    use std::collections::BTreeMap;
    let mut config = one_main_config();
    config.destinations.push(fin_binance::EthereumDestination {
        name: "wallet-fixture".into(),
        resource: fin_domain::ResourceId("ethereum:synthetic-wallet".into()),
        address: "0x1111111111111111111111111111111111111111".into(),
        chain_id: 1,
        assets: BTreeMap::from([(
            "ETH".into(),
            fin_domain::AssetId {
                network: "ethereum:1".into(),
                reference: "native".into(),
                decimals: 18,
            },
        )]),
        confirmations: 2,
    });
    let adapter = fin_binance::BinanceAdapter::new(
        config,
        std::sync::Arc::new(WithdrawalFixtureTransport {
            travel_country_code: "NIL",
            withdrawal_permission: true,
            withdraw_fee: "0.0004",
            withdraw_min: "0.001",
            withdraw_increment: "0.00000001",
            free_balance: "1.234500000000000001",
        }),
        Some(std::sync::Arc::new(SyntheticPriceProvider)),
        Some(std::sync::Arc::new(SyntheticRecipientVerifier)),
    )
    .unwrap();
    let preview = adapter
        .prepare(
            "binance.withdraw",
            &json!({
                "requestId":"synthetic-withdraw-1", "account":"main-fixture",
                "destination":"wallet-fixture", "coin":"ETH", "amount":"0.01",
                "network":"ETH", "walletType":"SPOT", "maxFee":"0.001",
                "minReceiveAmount":"0.009"
            }),
        )
        .await
        .unwrap();
    let (debit_asset, credit_asset) = match &preview.intent.expected[0] {
        EffectConstraint::Transfer {
            debit_asset,
            credit_asset,
            ..
        } => (debit_asset, credit_asset),
        other => panic!("unexpected withdrawal constraint {other:?}"),
    };
    assert_eq!(debit_asset.network, "binance");
    assert_eq!(debit_asset.decimals, 36);
    assert_eq!(credit_asset.network, "ethereum:1");
    assert_eq!(credit_asset.reference, "native");
    assert_eq!(credit_asset.decimals, 18);
    assert_eq!(preview.valuation.as_ref().unwrap().prices.len(), 2);
    assert_eq!(
        preview.valuation.as_ref().unwrap().prices[0].asset,
        *debit_asset
    );
    assert_eq!(
        preview.valuation.as_ref().unwrap().prices[1].asset,
        *credit_asset
    );
    assert_eq!(
        preview.valuation.as_ref().unwrap().prices[1]
            .unit_atoms
            .as_str(),
        "1000000000000000000"
    );
    assert_eq!(preview.intent.debits[0].amount.atoms.to_decimal(36), "0.01");
    assert_eq!(
        preview.intent.maximum_costs[0].amount.atoms.to_decimal(36),
        "0.001"
    );
    assert_eq!(
        preview.intent.maximum_costs[0].location,
        AccountingLocation::Cash {
            resource: preview.intent.debits[0].resource.clone()
        }
    );
    assert_eq!(
        preview.intent.maximum_costs[0].treatment,
        fin_domain::CostTreatment::AdditionalDebit
    );
}

#[tokio::test]
async fn withdrawal_reserves_principal_and_fee_once_at_exact_balance_boundary() {
    use fin_domain::Adapter;
    use std::collections::BTreeMap;

    async fn preview_with_free(
        free_balance: &'static str,
    ) -> fin_domain::Result<fin_domain::Preview> {
        let mut config = one_main_config();
        config.destinations.push(fin_binance::EthereumDestination {
            name: "wallet-fixture".into(),
            resource: fin_domain::ResourceId("ethereum:synthetic-wallet".into()),
            address: "0x1111111111111111111111111111111111111111".into(),
            chain_id: 1,
            assets: BTreeMap::from([(
                "ETH".into(),
                fin_domain::AssetId {
                    network: "ethereum:1".into(),
                    reference: "native".into(),
                    decimals: 18,
                },
            )]),
            confirmations: 2,
        });
        let adapter = fin_binance::BinanceAdapter::new(
            config,
            std::sync::Arc::new(WithdrawalFixtureTransport {
                travel_country_code: "NIL",
                withdrawal_permission: true,
                withdraw_fee: "0.001",
                withdraw_min: "0.001",
                withdraw_increment: "0.00000001",
                free_balance,
            }),
            Some(std::sync::Arc::new(SyntheticPriceProvider)),
            Some(std::sync::Arc::new(SyntheticRecipientVerifier)),
        )?;
        adapter
            .prepare(
                "binance.withdraw",
                &json!({
                    "requestId":"synthetic-withdraw-boundary", "account":"main-fixture",
                    "destination":"wallet-fixture", "coin":"ETH", "amount":"0.01",
                    "network":"ETH", "walletType":"SPOT", "maxFee":"0.001",
                    "minReceiveAmount":"0.009"
                }),
            )
            .await
    }

    let exact = preview_with_free("0.011").await.unwrap();
    assert_eq!(exact.intent.debits[0].amount.atoms.to_decimal(36), "0.01");
    assert_eq!(
        exact.intent.maximum_costs[0].amount.atoms.to_decimal(36),
        "0.001"
    );
    assert!(
        preview_with_free("0.010999999999999999999999999999999999")
            .await
            .is_err()
    );
}

struct SyntheticSigner {
    signed: std::sync::Mutex<Vec<Vec<u8>>>,
}

#[async_trait::async_trait]
impl fin_binance::BinanceSigner for SyntheticSigner {
    async fn api_key(
        &self,
        _credential: &fin_binance::CredentialHandle,
    ) -> std::result::Result<String, fin_binance::TransportError> {
        Ok("synthetic-test-api-key".into())
    }

    async fn sign(
        &self,
        _credential: &fin_binance::CredentialHandle,
        encoded_parameters: &[u8],
    ) -> std::result::Result<fin_binance::BinanceSignature, fin_binance::TransportError> {
        self.signed
            .lock()
            .unwrap()
            .push(encoded_parameters.to_vec());
        Ok(fin_binance::BinanceSignature::from_provider("fixture+/="))
    }
}

#[tokio::test]
async fn loopback_transport_signs_exact_body_and_uses_only_closed_endpoint_path() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut chunk = [0u8; 2048];
        loop {
            let count = stream.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0, "synthetic request closed before its body arrived");
            request.extend_from_slice(&chunk[..count]);
            if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let body_len = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .expect("POST request has a content length");
                if request.len() >= header_end + 4 + body_len {
                    let body = String::from_utf8_lossy(
                        &request[header_end + 4..header_end + 4 + body_len],
                    );
                    assert!(
                        headers.starts_with("POST /sapi/v1/sub-account/universalTransfer HTTP/1.1")
                    );
                    assert!(
                        headers
                            .to_lowercase()
                            .contains("x-mbx-apikey: synthetic-test-api-key")
                    );
                    assert!(body.contains("amount=0.000000000000000000123456"));
                    assert!(body.contains("toEmail=child%2Btag%40example.invalid"));
                    assert!(body.contains("recvWindow=5000&timestamp=123"));
                    assert!(body.contains("signature=fixture%2B%2F%3D"));
                    break;
                }
            }
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 12\r\nConnection: close\r\n\r\n{\"tranId\":7}")
            .await
            .unwrap();
        request
    });
    let signer = std::sync::Arc::new(SyntheticSigner {
        signed: std::sync::Mutex::new(Vec::new()),
    });
    let transport = fin_binance::BinanceHttpTransport::with_clock(
        format!("http://{address}"),
        signer.clone(),
        || 123,
    )
    .unwrap();
    let request = fin_binance::UniversalTransferIntent {
        asset: "ETH".into(),
        amount: amount("0.000000000000000000123456"),
        from_email: None,
        to_email: Some("child+tag@example.invalid".into()),
        client_id: fin_binance::ClientIdentity::transfer("http-fixture-1").unwrap(),
    }
    .request()
    .unwrap();
    let response = fin_binance::BinanceTransport::request(
        &transport,
        &fin_binance::CredentialHandle::new("synthetic-http-credential").unwrap(),
        request,
    )
    .await
    .unwrap();
    assert_eq!(response["tranId"], 7);
    let preimage = String::from_utf8(signer.signed.lock().unwrap()[0].clone()).unwrap();
    assert!(preimage.contains("amount=0.000000000000000000123456"));
    assert!(preimage.contains("toEmail=child%2Btag%40example.invalid"));
    assert_eq!(
        preimage,
        format!(
            "asset=ETH&amount=0.000000000000000000123456&fromAccountType=SPOT&toAccountType=SPOT&clientTranId={}&toEmail=child%2Btag%40example.invalid&recvWindow=5000&timestamp=123",
            fin_binance::ClientIdentity::transfer("http-fixture-1")
                .unwrap()
                .as_str()
        )
    );
    server.await.unwrap();
}

#[tokio::test]
async fn loopback_http_rate_limits_are_reported_once_without_retry_for_418_or_429() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    for status in [418u16, 429u16] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 2048];
            loop {
                let count = stream.read(&mut chunk).await.unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&chunk[..count]);
                let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let body_len = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + body_len {
                    assert!(
                        headers.starts_with("POST /sapi/v1/sub-account/universalTransfer HTTP/1.1")
                    );
                    break;
                }
            }
            stream.write_all(format!(
                "HTTP/1.1 {status} Rate Limited\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ).as_bytes()).await.unwrap();
            drop(stream);
            let second =
                tokio::time::timeout(std::time::Duration::from_millis(150), listener.accept())
                    .await;
            (request, second.is_ok())
        });
        let transport = fin_binance::BinanceHttpTransport::with_clock(
            format!("http://{address}"),
            std::sync::Arc::new(SyntheticSigner {
                signed: std::sync::Mutex::new(Vec::new()),
            }),
            || 123,
        )
        .unwrap();
        let request = fin_binance::UniversalTransferIntent {
            asset: "ETH".into(),
            amount: amount("0.01"),
            from_email: Some("child@example.invalid".into()),
            to_email: None,
            client_id: fin_binance::ClientIdentity::transfer(&format!("rate-limit-{status}"))
                .unwrap(),
        }
        .request()
        .unwrap();
        let result = fin_binance::BinanceTransport::request(
            &transport,
            &fin_binance::CredentialHandle::new("test-only-rate-limit").unwrap(),
            request,
        )
        .await;
        assert!(matches!(
            result,
            Err(fin_binance::TransportError::HttpFailure {
                status: response_status,
                binance_code: None,
                kind: fin_binance::HttpFailureKind::RateLimited,
            }) if response_status == status
        ));
        let (request, retried) = server.await.unwrap();
        assert!(!retried, "HTTP {status} caused an automatic second request");
        assert!(
            String::from_utf8_lossy(&request)
                .starts_with("POST /sapi/v1/sub-account/universalTransfer")
        );
    }
}

#[tokio::test]
async fn loopback_http_diagnostics_keep_only_validated_status_and_integer_code() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let cases = [
        (
            400u16,
            r#"{"code":-2010,"msg":"body-secret"}"#,
            Some(-2010),
            fin_binance::HttpFailureKind::Rejected,
        ),
        (
            200,
            r#"{"code":-2010,"msg":"body-secret"}"#,
            Some(-2010),
            fin_binance::HttpFailureKind::Rejected,
        ),
        (
            400,
            r#"{"code":"-2010","msg":"body-secret"}"#,
            None,
            fin_binance::HttpFailureKind::Rejected,
        ),
        (
            400,
            r#"{"code":-2010.5,"msg":"body-secret"}"#,
            None,
            fin_binance::HttpFailureKind::Rejected,
        ),
        (
            400,
            r#"{"code":-2010.0,"msg":"body-secret"}"#,
            None,
            fin_binance::HttpFailureKind::Rejected,
        ),
        (
            400,
            r#"{"code":null,"msg":"body-secret"}"#,
            None,
            fin_binance::HttpFailureKind::Rejected,
        ),
        (
            400,
            r#"{"code":{"value":-2010},"msg":"body-secret"}"#,
            None,
            fin_binance::HttpFailureKind::Rejected,
        ),
        (
            400,
            r#"{"code":9223372036854775808,"msg":"body-secret"}"#,
            None,
            fin_binance::HttpFailureKind::Rejected,
        ),
        (
            400,
            r#"{"code":-9223372036854775809,"msg":"body-secret"}"#,
            None,
            fin_binance::HttpFailureKind::Rejected,
        ),
        (
            400,
            "not-json body-secret",
            None,
            fin_binance::HttpFailureKind::Rejected,
        ),
        (
            200,
            "not-json body-secret",
            None,
            fin_binance::HttpFailureKind::InvalidResponse,
        ),
        (
            401,
            "body-secret",
            None,
            fin_binance::HttpFailureKind::PermissionDenied,
        ),
        (
            403,
            "body-secret",
            None,
            fin_binance::HttpFailureKind::PermissionDenied,
        ),
        (
            418,
            "body-secret",
            None,
            fin_binance::HttpFailureKind::RateLimited,
        ),
        (
            429,
            "body-secret",
            None,
            fin_binance::HttpFailureKind::RateLimited,
        ),
    ];
    for (status, body, expected_code, expected_kind) in cases {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let body = body.to_owned();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let count = stream.read(&mut chunk).await.unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let transport = fin_binance::BinanceHttpTransport::with_clock(
            format!("http://{address}"),
            std::sync::Arc::new(SyntheticSigner {
                signed: std::sync::Mutex::new(Vec::new()),
            }),
            || 123,
        )
        .unwrap();
        let result = fin_binance::BinanceTransport::request(
            &transport,
            &fin_binance::CredentialHandle::new("test-only-http-diagnostics").unwrap(),
            BinanceRequest::new(BinanceEndpoint::ApiRestrictions),
        )
        .await;
        let Err(failure) = result else {
            panic!("expected classified HTTP failure, got {result:?}");
        };
        assert!(!format!("{failure:?}").contains("body-secret"));
        let fin_binance::TransportError::HttpFailure {
            status: actual_status,
            binance_code,
            kind,
        } = failure
        else {
            panic!("expected classified HTTP failure, got {failure:?}");
        };
        assert_eq!(actual_status, status);
        assert_eq!(binance_code, expected_code);
        assert_eq!(kind, expected_kind);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn withdrawal_preview_enforces_permission_travel_rule_fee_minimum_and_precision_guards() {
    use fin_domain::Adapter;
    use std::collections::BTreeMap;
    let guard_cases = [
        (
            "permission",
            false,
            "NIL",
            "0.0004",
            "0.001",
            "0.00000001",
            "MISSING_PERMISSION",
        ),
        (
            "travel-rule",
            true,
            "US",
            "0.0004",
            "0.001",
            "0.00000001",
            "TRAVEL_RULE_REQUIRED",
        ),
        (
            "fee",
            true,
            "NIL",
            "0.002",
            "0.001",
            "0.00000001",
            "FEE_BOUND_EXCEEDED",
        ),
        (
            "minimum",
            true,
            "NIL",
            "0.0004",
            "0.02",
            "0.00000001",
            "WITHDRAWAL_LIMIT",
        ),
        (
            "precision",
            true,
            "NIL",
            "0.0004",
            "0.001",
            "0.003",
            "INVALID_PRECISION",
        ),
    ];
    for (case, permission, country, fee, minimum, increment, expected_error) in guard_cases {
        let mut config = one_main_config();
        config.destinations.push(fin_binance::EthereumDestination {
            name: "wallet-fixture".into(),
            resource: fin_domain::ResourceId("ethereum:synthetic-wallet".into()),
            address: "0x1111111111111111111111111111111111111111".into(),
            chain_id: 1,
            assets: BTreeMap::from([(
                "ETH".into(),
                fin_domain::AssetId {
                    network: "ethereum:1".into(),
                    reference: "native".into(),
                    decimals: 18,
                },
            )]),
            confirmations: 2,
        });
        let adapter = fin_binance::BinanceAdapter::new(
            config,
            std::sync::Arc::new(WithdrawalFixtureTransport {
                travel_country_code: country,
                withdrawal_permission: permission,
                withdraw_fee: fee,
                withdraw_min: minimum,
                withdraw_increment: increment,
                free_balance: "1.234500000000000001",
            }),
            Some(std::sync::Arc::new(SyntheticPriceProvider)),
            Some(std::sync::Arc::new(SyntheticRecipientVerifier)),
        )
        .unwrap();
        let result = adapter
            .prepare(
                "binance.withdraw",
                &json!({
                    "requestId":format!("synthetic-guard-{case}"), "account":"main-fixture",
                    "destination":"wallet-fixture", "coin":"ETH", "amount":"0.01",
                    "network":"ETH", "walletType":"SPOT", "maxFee":"0.001",
                    "minReceiveAmount":"0.009"
                }),
            )
            .await;
        assert_eq!(
            result.unwrap_err().code,
            expected_error,
            "guard case {case}"
        );
    }
}

struct WithdrawalLifecycleTransport {
    account_reads: std::sync::atomic::AtomicUsize,
    events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    timeout_withdrawal: bool,
}

#[async_trait::async_trait]
impl fin_binance::BinanceTransport for WithdrawalLifecycleTransport {
    async fn request(
        &self,
        _credential: &fin_binance::CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, fin_binance::TransportError> {
        use fin_binance::BinanceEndpoint as E;
        self.events
            .lock()
            .unwrap()
            .push(format!("request:{:?}", request.endpoint));
        match request.endpoint {
            E::ApiRestrictions => Ok(json!({"enableReading":true,"enableWithdrawals":true})),
            E::TravelRuleRequirements => Ok(json!({"questionnaireCountryCode":"NIL"})),
            E::CapitalConfig => Ok(json!([{
                "coin":"ETH", "withdrawAllEnable":true, "depositAllEnable":true,
                "networkList":[{
                    "network":"ETH", "coin":"ETH", "depositEnable":true,
                    "withdrawEnable":true, "withdrawIntegerMultiple":"0.00000001",
                    "withdrawFee":"0.0004", "withdrawMin":"0.001", "withdrawMax":"1000",
                    "withdrawTag":false, "busy":false
                }]
            }])),
            E::Account => {
                let read = self
                    .account_reads
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let free = if read < 3 { "1.2345" } else { "1.2241" };
                Ok(json!({"balances":[{"asset":"ETH","free":free,"locked":"0"}]}))
            }
            E::StandardWithdrawal => {
                assert_eq!(request.get("network"), Some("ETH"));
                assert_eq!(request.get("walletType"), Some("0"));
                assert_eq!(request.get("amount"), Some("0.01"));
                if self.timeout_withdrawal {
                    Err(fin_binance::TransportError::Timeout)
                } else {
                    Ok(json!({"id":"withdraw-fixture-01"}))
                }
            }
            E::WithdrawalHistory => Ok(json!([{
                "id":"withdraw-fixture-01", "coin":"ETH", "amount":"0.01",
                "transactionFee":"0.0004", "network":"ETH",
                "address":"0x1111111111111111111111111111111111111111",
                "txId":format!("0x{}", "a".repeat(64)), "status":6,
                "completeTime":"2026-09-01 12:30:45",
                "withdrawOrderId":request.get("withdrawOrderId"),
                "transferType":0, "walletType":0
            }])),
            endpoint => panic!("unexpected withdrawal lifecycle endpoint {endpoint:?}"),
        }
    }
}

struct CompletedRecipientVerifier;

#[async_trait::async_trait]
impl fin_binance::BinanceRecipientVerifier for CompletedRecipientVerifier {
    async fn verify(&self, request: fin_binance::PayoutRequest) -> fin_binance::PayoutResult {
        assert_eq!(request.chain_id, 1);
        assert_eq!(
            request.recipient,
            "0x1111111111111111111111111111111111111111"
        );
        assert_eq!(request.asset.network, "ethereum:1");
        assert_eq!(request.asset.decimals, 18);
        fin_binance::PayoutResult {
            status: fin_domain::Status::Completed,
            amount_atoms: Some(fin_domain::Quantity::new("10000000000000000").unwrap()),
            evidence: json!({"fixture":"synthetic Ethereum mainnet receipt"}),
        }
    }
}

#[tokio::test]
async fn withdrawal_lifecycle_correlates_order_fee_and_cross_asset_chain_receipt() {
    use fin_domain::Adapter;
    use std::collections::BTreeMap;
    let mut config = one_main_config();
    config.destinations.push(fin_binance::EthereumDestination {
        name: "wallet-fixture".into(),
        resource: fin_domain::ResourceId("ethereum:synthetic-wallet".into()),
        address: "0x1111111111111111111111111111111111111111".into(),
        chain_id: 1,
        assets: BTreeMap::from([(
            "ETH".into(),
            fin_domain::AssetId {
                network: "ethereum:1".into(),
                reference: "native".into(),
                decimals: 18,
            },
        )]),
        confirmations: 2,
    });
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let adapter = fin_binance::BinanceAdapter::new(
        config,
        std::sync::Arc::new(WithdrawalLifecycleTransport {
            account_reads: std::sync::atomic::AtomicUsize::new(0),
            events: events.clone(),
            timeout_withdrawal: false,
        }),
        None,
        Some(std::sync::Arc::new(CompletedRecipientVerifier)),
    )
    .unwrap();
    let input = json!({
        "requestId":"synthetic-withdraw-lifecycle", "account":"main-fixture",
        "destination":"wallet-fixture", "coin":"ETH", "amount":"0.01",
        "network":"ETH", "walletType":"SPOT", "maxFee":"0.001",
        "minReceiveAmount":"0.009"
    });
    let preview = adapter.prepare("binance.withdraw", &input).await.unwrap();
    let journal = std::sync::Arc::new(RecordingJournal {
        events: events.clone(),
        persisted: std::sync::Mutex::new(Vec::new()),
    });
    let submission = adapter.submit(&preview, journal.clone()).await.unwrap();
    assert_eq!(submission.reference, preview.binding["withdrawOrderId"]);
    assert!(submission.data.get("venueId").is_none());
    let order = fin_domain::OperationRecord {
        id: "synthetic-withdraw-op".into(),
        request_id: "synthetic-withdraw-lifecycle".into(),
        fingerprint: "synthetic-withdraw-fingerprint".into(),
        fingerprint_version: 1,
        operation: "binance.withdraw".into(),
        input,
        status: fin_domain::Status::Submitted,
        created_at: fin_domain::now(),
        updated_at: fin_domain::now(),
        preview: Some(preview),
        approval: None,
        submission: Some(submission.clone()),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let result = adapter.reconcile(&order).await.unwrap();
    assert_eq!(result.status, fin_domain::Status::Completed);
    let proof = result.verified.unwrap();
    assert_eq!(proof.submission, submission);
    assert_eq!(
        proof.proof_digest,
        fin_domain::verified_effect_digest(&proof).unwrap()
    );
    assert_eq!(proof.effects.len(), 2);
    match &proof.effects[0] {
        fin_domain::FinancialEffect::Transfer { debit, credit, .. } => {
            assert_eq!(debit.asset.network, "binance");
            assert_eq!(debit.asset.decimals, 36);
            assert_eq!(debit.atoms.to_decimal(36), "0.01");
            assert_eq!(credit.asset.network, "ethereum:1");
            assert_eq!(credit.asset.decimals, 18);
            assert_eq!(credit.atoms.to_decimal(18), "0.01");
        }
        other => panic!("unexpected withdrawal effect {other:?}"),
    }
    match &proof.effects[1] {
        fin_domain::FinancialEffect::Cost {
            location,
            amount,
            treatment,
            ..
        } => {
            assert_eq!(
                location,
                &fin_domain::AccountingLocation::Cash {
                    resource: fin_domain::ResourceId("binance:synthetic-main".into())
                }
            );
            assert_eq!(amount.asset.network, "binance");
            assert_eq!(amount.atoms.to_decimal(36), "0.0004");
            assert_eq!(treatment, &fin_domain::CostTreatment::AdditionalDebit);
        }
        other => panic!("unexpected withdrawal cost effect {other:?}"),
    }
    let events = events.lock().unwrap();
    let withdraw_dispatch = events
        .iter()
        .position(|event| event == "request:StandardWithdrawal")
        .unwrap();
    let before_dispatch_persist = events[..withdraw_dispatch]
        .iter()
        .rposition(|event| event == "persist-submission")
        .unwrap();
    assert!(before_dispatch_persist < withdraw_dispatch);
    assert_eq!(journal.persisted.lock().unwrap().len(), 1);
}

struct PaginatedWithdrawalHistoryTransport {
    offsets: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl fin_binance::BinanceTransport for PaginatedWithdrawalHistoryTransport {
    async fn request(
        &self,
        _credential: &fin_binance::CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, fin_binance::TransportError> {
        assert_eq!(
            request.endpoint,
            fin_binance::BinanceEndpoint::WithdrawalHistory
        );
        let offset = request.get("offset").unwrap().to_owned();
        self.offsets.lock().unwrap().push(offset.clone());
        let start = offset.parse::<u64>().unwrap();
        let count = if start == 0 { 1000 } else { 1 };
        Ok(Value::Array(
            (0..count)
                .map(|i| {
                    json!({
                        "id":format!("withdraw-{}", start + i),
                        "coin":"ETH", "amount":"0.0001", "status":6
                    })
                })
                .collect(),
        ))
    }
}

#[tokio::test]
async fn withdrawal_history_pagination_uses_offset_and_marks_fully_observed() {
    use fin_domain::Adapter;
    let offsets = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let adapter = fin_binance::BinanceAdapter::new(
        one_main_config(),
        std::sync::Arc::new(PaginatedWithdrawalHistoryTransport {
            offsets: offsets.clone(),
        }),
        None,
        None,
    )
    .unwrap();
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let history = adapter
        .read(
            "binance.withdrawal-history",
            &json!({
                "account":"main-fixture", "startTime":now - 3_600_000,
                "endTime":now
            }),
        )
        .await
        .unwrap();
    assert_eq!(history["complete"], true);
    assert_eq!(history["records"].as_array().unwrap().len(), 1001);
    assert_eq!(*offsets.lock().unwrap(), vec!["0", "1000"]);
}

struct DuplicateWithdrawalHistoryTransport;

#[async_trait::async_trait]
impl fin_binance::BinanceTransport for DuplicateWithdrawalHistoryTransport {
    async fn request(
        &self,
        _credential: &fin_binance::CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, fin_binance::TransportError> {
        assert_eq!(
            request.endpoint,
            fin_binance::BinanceEndpoint::WithdrawalHistory
        );
        let row = json!({
            "id":"withdraw-fixture-01", "coin":"ETH", "amount":"0.01",
            "transactionFee":"0.0004", "network":"ETH",
            "address":"0x1111111111111111111111111111111111111111",
            "txId":format!("0x{}", "a".repeat(64)), "status":6,
            "withdrawOrderId":request.get("withdrawOrderId"), "transferType":0, "walletType":0
        });
        Ok(json!([row.clone(), row]))
    }
}

#[tokio::test]
async fn duplicate_withdrawal_history_records_never_settle_terminally() {
    use fin_domain::Adapter;
    use std::collections::BTreeMap;
    let mut config = one_main_config();
    config.destinations.push(fin_binance::EthereumDestination {
        name: "wallet-fixture".into(),
        resource: fin_domain::ResourceId("ethereum:synthetic-wallet".into()),
        address: "0x1111111111111111111111111111111111111111".into(),
        chain_id: 1,
        assets: BTreeMap::from([(
            "ETH".into(),
            fin_domain::AssetId {
                network: "ethereum:1".into(),
                reference: "native".into(),
                decimals: 18,
            },
        )]),
        confirmations: 2,
    });
    let preview_adapter = fin_binance::BinanceAdapter::new(
        config.clone(),
        std::sync::Arc::new(WithdrawalFixtureTransport {
            travel_country_code: "NIL",
            withdrawal_permission: true,
            withdraw_fee: "0.0004",
            withdraw_min: "0.001",
            withdraw_increment: "0.00000001",
            free_balance: "1.234500000000000001",
        }),
        None,
        Some(std::sync::Arc::new(SyntheticRecipientVerifier)),
    )
    .unwrap();
    let preview = preview_adapter
        .prepare(
            "binance.withdraw",
            &json!({
                "requestId":"synthetic-duplicate-withdraw", "account":"main-fixture",
                "destination":"wallet-fixture", "coin":"ETH", "amount":"0.01",
                "network":"ETH", "walletType":"SPOT", "maxFee":"0.001",
                "minReceiveAmount":"0.009"
            }),
        )
        .await
        .unwrap();
    let reference = preview.binding["withdrawOrderId"]
        .as_str()
        .unwrap()
        .to_owned();
    let submission = fin_domain::Submission {
        kind: "binance".into(),
        reference: reference.clone(),
        data: json!({
            "operation":"binance.withdraw", "binding":preview.binding,
            "submittedAt":fin_domain::now(), "sourceFreeBefore":"1.2345",
            "account":"main-fixture", "destination":"wallet-fixture", "coin":"ETH",
            "amount":"0.01", "address":"0x1111111111111111111111111111111111111111",
            "maximumFee":"0.001", "maximumGrossDebit":"0.011",
            "minimumReceive":"0.009", "maximumReceive":"0.01", "venueId":"withdraw-fixture-01"
        }),
    };
    let reconcile_adapter = fin_binance::BinanceAdapter::new(
        config,
        std::sync::Arc::new(DuplicateWithdrawalHistoryTransport),
        None,
        Some(std::sync::Arc::new(SyntheticRecipientVerifier)),
    )
    .unwrap();
    let record = fin_domain::OperationRecord {
        id: "synthetic-duplicate-withdraw-op".into(),
        request_id: "synthetic-duplicate-withdraw".into(),
        fingerprint: "synthetic-duplicate-fingerprint".into(),
        fingerprint_version: 1,
        operation: "binance.withdraw".into(),
        input: preview.input.clone(),
        status: fin_domain::Status::Submitted,
        created_at: fin_domain::now(),
        updated_at: fin_domain::now(),
        preview: Some(preview),
        approval: None,
        submission: Some(submission.clone()),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let result = reconcile_adapter.reconcile(&record).await.unwrap();
    assert_eq!(result.status, fin_domain::Status::Unknown);
    assert!(result.verified.is_none());
    assert_eq!(
        result.evidence["reason"],
        "Multiple Binance withdrawal records match the persisted identity."
    );
}

#[tokio::test]
async fn withdrawal_timeout_recovers_from_original_withdraw_order_id_without_replay() {
    use fin_domain::Adapter;
    use std::collections::BTreeMap;
    let mut config = one_main_config();
    config.destinations.push(fin_binance::EthereumDestination {
        name: "wallet-fixture".into(),
        resource: fin_domain::ResourceId("ethereum:synthetic-wallet".into()),
        address: "0x1111111111111111111111111111111111111111".into(),
        chain_id: 1,
        assets: BTreeMap::from([(
            "ETH".into(),
            fin_domain::AssetId {
                network: "ethereum:1".into(),
                reference: "native".into(),
                decimals: 18,
            },
        )]),
        confirmations: 2,
    });
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let adapter = fin_binance::BinanceAdapter::new(
        config,
        std::sync::Arc::new(WithdrawalLifecycleTransport {
            account_reads: std::sync::atomic::AtomicUsize::new(0),
            events: events.clone(),
            timeout_withdrawal: true,
        }),
        None,
        Some(std::sync::Arc::new(CompletedRecipientVerifier)),
    )
    .unwrap();
    let input = json!({
        "requestId":"synthetic-withdraw-timeout", "account":"main-fixture",
        "destination":"wallet-fixture", "coin":"ETH", "amount":"0.01",
        "network":"ETH", "walletType":"SPOT", "maxFee":"0.001",
        "minReceiveAmount":"0.009"
    });
    let preview = adapter.prepare("binance.withdraw", &input).await.unwrap();
    let journal = std::sync::Arc::new(RecordingJournal {
        events: events.clone(),
        persisted: std::sync::Mutex::new(Vec::new()),
    });
    let error = adapter.submit(&preview, journal.clone()).await.unwrap_err();
    assert_eq!(error.code, "VENUE_TIMEOUT");
    let original = journal.persisted.lock().unwrap()[0].clone();
    assert_eq!(original.reference, preview.binding["withdrawOrderId"]);
    assert!(original.data.get("venueId").is_none());
    let record = fin_domain::OperationRecord {
        id: "synthetic-withdraw-timeout-op".into(),
        request_id: "synthetic-withdraw-timeout".into(),
        fingerprint: "synthetic-withdraw-timeout-fingerprint".into(),
        fingerprint_version: 1,
        operation: "binance.withdraw".into(),
        input: input.clone(),
        status: fin_domain::Status::Submitted,
        created_at: fin_domain::now(),
        updated_at: fin_domain::now(),
        preview: Some(preview),
        approval: None,
        submission: Some(original.clone()),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let result = adapter.reconcile(&record).await.unwrap();
    assert_eq!(result.status, fin_domain::Status::Completed);
    let proof = result.verified.unwrap();
    assert_eq!(proof.submission, original);
    assert_eq!(
        proof.proof_digest,
        fin_domain::verified_effect_digest(&proof).unwrap()
    );
    assert_eq!(
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.as_str() == "request:StandardWithdrawal")
            .count(),
        1
    );
}
